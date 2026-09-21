//! Schedule 実行 (per-buffer render 経路)。engine.rs から分離
//! (`docs/plan_arch_refactor.md` §5)。
//!
//! [`render_master_buffer`] が「1 buffer を master へ描く」単一経路:
//! worker dispatch (per-track pass 1) → schedule 実行 (group mix / PDC /
//! send / sidechain / follower) → master fx chain (組み込み Bus Comp / Tone EQ を含む) →
//! master gain → master limiter。
//! **live (CPAL callback) と offline export (freewheel) の両方がこれを呼ぶ**
//! ので、master に挿した limiter が WAV に乗らない類の live/export 乖離が
//! 構造的に起きない。違うのは「聴き方・見方」の [`NativeIo`] (SC Listen / device scope) だけで、
//! export は既定値を渡す。metronome / panic declick 等 monitoring 専用の処理は
//! live 側 (engine.rs / main.rs) にだけ残る。
//!
//! RT 規約: この module の関数はすべて audio callback / audio worker /
//! export freewheel から呼ばれる。ヒープ確保・ロック・I/O・tracing を
//! 行わない。plugin dispatch は **有界** (`DISPATCH_TIMEOUT_MS`) で、
//! timeout は per-device quarantine + per-pair poison を立てて以後 skip する
//! (plan §4、`common::plugin_ref` module doc の poisoning contract)。

use std::sync::atomic::Ordering;

use common::model::{LoopRegion, ParamStoreAt, Song, Track};
use common::plugin_ref::{DISPATCH_TIMEOUT_MS, DispatchOutcome};
use common::process_data::EventKind;
use common::song_index::SongIndex;

use crate::audio_clip_renderer::AudioClipRenderer;
use crate::engine::{PairLease, PluginEntry, PluginRefs, SyncSlot, WorkerRig};
use crate::graph::native::{NativeIo, apply_listen_override};
use crate::graph::program::Pass1Role;
use crate::graph::step::{BufferParams, RenderCtx, run_step};
use crate::graph::{ChainProgram, ProgramCtx, Schedule, run_chain_program};
use crate::launcher::{RowSourceTable, TrackRows};
use common::mod_plane::ModTickPlaneRef;
use crate::mod_tick::FollowerDrive;
use crate::mixer::{TrackScratch, apply_strip, pass_strip};
use crate::native_dsp::MasterLimiterState;
use crate::sequencer::{NoteTransition, TimedNoteEvent};

/// この device / pair が dispatch 可能かどうか (quarantine / poison gate)。
/// gate を通らない device は **shmem (`ProcessData`) にも触らない** — timeout
/// した device の `process()` はまだ plugin_host 側で走っている可能性があり、
/// 入力を書き込むと並行 process と race する (poisoning contract)。
#[inline]
pub(super) fn pair_usable(slot: &SyncSlot, entry: &PluginEntry) -> bool {
    !entry.quarantined.load(Ordering::Acquire) && !slot.poisoned.load(Ordering::Acquire)
}

/// 1 device を worker pair へ **有界** dispatch する (plan §4)。 timeout と待ちの失敗は
/// (a) この pair を poison (host がその依頼を終えるまで dispatch 禁止 — contract。runner は予備の pair に借り替える)、
/// (b) この device を quarantine (以後 skip して bypass) して `false` を返す — host 側で `process()` が
/// まだ走っているかもしれず、別の slot から同じ device を叩くと並行 process になる。
/// 通知は RT からは行わない — flag を notify スレッド (`main.rs`) が poll して
/// `AudioEvent::PluginUnresponsive` を 1 回だけ送る。 RT-safe: atomic store のみ。
/// `frames` / `sample_rate` は完了を回って待つ時間 (buffer 周期に比例) を決める。
///
/// **待った時間は `lease` 経由で内訳へ記録する** (`crate::graph::profile`)。
/// ここが「グラフ自身の仕事」と「他プロセスの完了待ち」の境目そのものなので、
/// 計測点を別の場所に置くと 2 つが混ざって切り分けられなくなる。
#[inline]
pub(super) fn dispatch_bounded(
    lease: PairLease<'_>,
    slot: &SyncSlot,
    entry: &PluginEntry,
    frames: u32,
    sample_rate: u32,
) -> bool {
    let spin = common::worker_bridge::spin_budget(frames, sample_rate);
    let started = std::time::Instant::now();
    let outcome = slot.sync.dispatch(entry.plugin_ref.token, spin, DISPATCH_TIMEOUT_MS);
    lease.record_dispatch_wait(started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
    match outcome {
        DispatchOutcome::Done => true,
        DispatchOutcome::TimedOut | DispatchOutcome::WaitFailed => {
            slot.poisoned.store(true, Ordering::Release);
            entry.quarantined.store(true, Ordering::Release);
            false
        }
    }
}

/// Phase 5 Step 5.3 (`docs/plan_automation.md` §10): populate the
/// transport fields on `ProcessData` from the current `Song` so the
/// plugin host can build a `clap_event_transport` for each
/// `plugin.process()` call. `song = None` (engine init / no song
/// loaded) leaves the default constants set by `ProcessData::empty()`
/// (120 BPM / 4/4 / no loop).
/// Phase 5 Step 5.2: `effective_bpm` is the SongTempo lane evaluated
/// at the buffer-start beat (= what the plugin sees as `clap_event_transport
/// .tempo`)。 引数で受け取るのは song-domain の `song.bpm` (= constant
/// base BPM) と区別するため。
///
/// r.md #87: transport には**曲全体の位置** (`song_pos_beats`) と**行の時間軸**
/// (`pd.row`、[`common::process_data::RowTransport`]) の 2 つが載る。plugin が
/// musical time として見るのは後者 — 行の主導権をランチャーが握っている間、
/// その行の device はセルの拍で動くべきだから。前者は「曲のどこか」を意味する
/// 用途 (録音位置 / ARA の playback region) が読む。
pub fn set_pd_transport(
    pd: &mut common::process_data::ProcessData,
    song: Option<&Song>,
    effective_bpm: f32,
    // 積分済みの真の拍位置 (tempo automation を考慮)。 plugin host が一定
    // テンポ逆算する代わりにこれを直接 song_pos_beats として使う。
    song_pos_beats: f64,
    // 再生ループの状態 (= `shared.loop_region`)。 ループは `Song` ではなく GUI の
    // session state が所有するので、 song からは取れず engine が持ち回った値を渡す。
    loop_region: LoopRegion,
    // r.md #87: この device が載っている行の供給元。行の実効拍 / 鳴っているセル /
    // 無音かを `pd.row` へ載せて plugin host へ渡す (`ProcessData::row` の doc)。
    // アレンジ主導の行では `pd.row.pos_beats == song_pos_beats` になるので、
    // ランチャーを使わない曲の transport は byte 単位で従来と同じ。
    row: crate::launcher::RowTimeSource,
) {
    // song が無い (engine init) 段階でも行の transport は残さない — 前の buffer の
    // セル情報が居座ると、song を読み込む前の 1 buffer が幽霊セルを鳴らす。
    pd.row = crate::launcher::render::row_transport(row, song_pos_beats);
    let Some(song) = song else { return };
    pd.bpm = effective_bpm.max(1.0);
    pd.tsig_num = song.time_sig.0 as u16;
    pd.tsig_denom = song.time_sig.1 as u16;
    pd.loop_start_beats = loop_region.start_beat;
    pd.loop_end_beats = loop_region.end_beat;
    pd.song_pos_beats = song_pos_beats;
    // 実 loop トグル状態を渡す。 plugin host は IS_LOOP_ACTIVE 判定で
    // 別途 `loop_end_beats > loop_start_beats` (= region 定義済) と AND する。
    pd.looping = if loop_region.enabled { 1 } else { 0 };
}

/// Render one track's contribution into its `TrackScratch`. Walks the
/// single device chain (Reaper 流 serial port connection), dispatches every
/// plugin via the assigned worker pair, then applies the mixer strip
/// (equal-power pan + volume + mute/solo). The post-fader audio ends
/// up in `scratch.track_l/r` along with the peak meter info.
///
/// Master accumulation into the bus happens **outside** this function
/// (schedule の `Mix` op) so concurrent workers never race on the same
/// `master_{l,r}[i]`.
///
/// v29: plugin lookup は `song_track.devices[i].id` (安定 device id) で
/// `plugin_refs` を直接引く。 positional slot map は存在しない。
///
/// `worker_sync` may be `None` if `OpenWorkerPool` hasn't arrived yet
/// — in that case plugin chains are skipped entirely (silent track)。
///
/// `input_delay_samples`: PR4.5 sidechain plugin-internal alignment. If
/// non-zero, the track's main signal (vocal / instrument output) is
/// delayed by that many samples **before** the audio FX chain runs.
/// The caller passes `Schedule::input_delay_per_track[track_idx]`.
/// 0 = no delay (the common case).
#[allow(clippy::too_many_arguments)]
pub fn process_track_owned(
    track_idx: u32,
    song_track: &Track,
    scratch: &mut TrackScratch,
    // r.md #110: この track の展開済み device 列 (`Schedule::track_programs[track_idx]`)。
    program: &mut ChainProgram,
    plugin_refs: &PluginRefs,
    audio_renderer: Option<&AudioClipRenderer>,
    worker_sync: Option<PairLease<'_>>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    song: Option<&Song>,
    // `song` と同じ snapshot の索引 (`RtBundle::song_index`)。
    index: &SongIndex,
    any_solo: bool,
    // この track の祖先 group に solo があるか (`SoloTables::of`、folder solo)。
    ancestor_soloed: bool,
    input_delay_samples: u32,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    // Phase 5 Step 5.2: 当該 buffer の effective bpm (= SongTempo lane 評価
    // or song.bpm fallback)。 set_pd_transport / fill_track_param_ramps /
    // fill_pd_param_events の sample-to-beat 変換に使う。
    current_bpm: f32,
    // Phase 5 follow-up (MIDI tempo follow): buffer 開始時の累積 beat-domain
    // playhead。 collect_events_for_buffer に渡して beat-domain で note 配置
    // を判定する。 変動 tempo でも note 位置が正しく追随する。
    playhead_beats: f64,
    // 再生ループの状態 (= `shared.loop_region`)。 set_pd_transport に渡す。
    loop_region: LoopRegion,
    // docs/plan_modulation.md §5 / r.md #89: 変調ソースの値面 (**`ModSource::id`
    // キー**、block-rate snapshot)。fill_track_param_ramps / fill_pd_param_events に
    // 渡して volume/pan/plugin param を変調する。空なら変調なし。
    mod_plane: ModTickPlaneRef<'_>,
    // r.md #87: この track の行 (トラック行 + レーン行) の時間軸の供給元。
    // ループ端での buffer 分割は `crate::launcher::render` が持つ。
    // 空 (`TrackRows::default()`) で全部アレンジ = 従来の挙動。
    rows: TrackRows<'_>,
    // r.md #129: 「聴き方・見方」(SC Listen / device scope)。書き出しは既定値。
    native_io: NativeIo<'_>,
    // r.md #130: この buffer の曲の移調量 (半音)。このトラックが追従しなければここで 0 にする
    // (実効的な追従は祖先グループまで辿った値を索引が持つ)。
    song_transpose: i32,
) {
    let n = frames as usize;
    let transpose = if index.follows_transpose(track_idx as usize) { song_transpose } else { 0 };
    // r.md #129: SC Listen の置換要求は program の実行開始時に消す (GWI は pass 1 の開始 = ここ。
    // pass 2 の `run_group_fx_chain` は prefix で立った要求を PostFx 点で消費する)。
    program.listen_pending = None;

    // Tracks that have children (i.e. behave as a "group" / folder)
    // are handled by the post-dispatch schedule walk: the children's
    // outputs are mixed into this track's scratch by a `Mix` op, then
    // `ProcessGroupFx` applies the audio fx_chain and strip. Skip the
    // sequencer / midi_fx / instrument stages here so the dispatch
    // doesn't smear plugin output into a buffer the schedule is about
    // to overwrite.
    // パラアウト (docs/plan_paraout.md) + pass-1 bus classification.
    // A group / return / parallel-out-dest track is summed + FX'd in pass 2
    // (`run_group_fx_chain`), so it must NOT run its device chain here in pass 1
    // — doing so would double-process stateful FX (a return's delay / reverb
    // would advance at 2× and any aux-dest EQ would see a spurious silent
    // block). This also fixes a latent bug where returns (incoming sends, no
    // children) were not skipped before paraout existed.
    //
    // EXCEPTION — group-with-instrument: a group whose own device chain has a
    // routed aux output (a multi-out instrument feeding child tracks that sum
    // back into it). Its **instrument prefix** `[0..split]` runs here in pass 1
    // to produce the track's own main signal AND fill `buffer_aux_out` for the
    // children; the **suffix FX** `[split..]` + strip run in pass 2 on the
    // summed bus (own main + children). `device_end` bounds the pass-1 device
    // loop; `skip_strip` defers the volume/pan strip + pre-fader/pre-fx
    // snapshots to pass 2.
    // r.md #110: pass 1 で走らせる op 区間。group-with-instrument は instrument prefix
    // (`program.pass1_end`) まで、leaf は全部。
    // r.md #129: 役割は compile 時に焼いた値 (旧実装は毎 buffer Song を歩き、パラアウト先の判定が
    // plugin を列挙する iterator の確保を RT で起こしていた)。
    let (skip_strip, main_to_child) = match program.pass1_role {
        Pass1Role::Leaf => (false, false),
        Pass1Role::GroupWithInstrument { main_to_child } => (true, main_to_child),
        // r.md #131: 無効トラックの手は直列トレースに載らない (`RenderGraph::build`) ので本来ここへは来ない。
        // 来ても何も処理せず無音にする (bus と同じ)。
        Pass1Role::Bus | Pass1Role::Disabled => {
            scratch.track_l[..n].fill(0.0);
            scratch.track_r[..n].fill(0.0);
            scratch.peak_l = 0.0;
            scratch.peak_r = 0.0;
            scratch.effective_mute = false;
            return;
        }
    };
    let op_end = if skip_strip { program.pass1_end } else { program.ops.len() };

    // ---- Sequencer: assemble this buffer's MIDI bus ----
    scratch.midi_bus_a.clear();
    // RT 安全: `midi_bus_a` の容量 (`MAX_EVENTS`) を超える分は捨てる (再確保しない)。 供給元
    // (pending_offs / pending_preview / sequencer) は各々 `MAX_EVENTS` 以下だが、 合算は超えうる。
    let bus_cap = scratch.midi_bus_a.capacity();
    for &(note_id, key) in scratch.state.pending_offs.iter().take(bus_cap) {
        // stuck note flush。 note-on と同じ note_id を載せる (CLAP / VST3 は id 一致で
        // voice を探す。 `0` にすると Surge XT 等で止まらない)。
        scratch.midi_bus_a.push(TimedNoteEvent {
            time: 0,
            event: NoteTransition::Off { note_id, key },
        });
    }
    scratch.state.pending_offs.clear();
    // 鍵盤レーン click のプレビュー note (engine の pump_commands が該当 track の
    // pending_preview に積む)。 transport に関係なく frame 0 で 1 回注入する
    // (instrument dispatch は playing で gate されないので停止中でも発音する)。
    // collect_events_for_buffer より前に push し、 playing 時は同 buffer の
    // sort (CLAP の time 昇順 / 同 time は Off→On) に乗せる。
    let room = bus_cap.saturating_sub(scratch.midi_bus_a.len());
    for &ev in scratch.state.pending_preview.iter().take(room) {
        scratch.midi_bus_a.push(TimedNoteEvent { time: 0, event: ev });
    }
    scratch.state.pending_preview.clear();
    if playing {
        crate::launcher::render::collect_row_midi(
            song,
            index,
            track_idx,
            rows.track(),
            sample_rate,
            playhead_beats,
            current_bpm,
            frames,
            transpose,
            &mut scratch.midi_bus_a,
            &mut scratch.state.active_notes,
        );
    }
    // r.md #117: この track の最新ノート (`Note` 起点のソースを global に落とす起点)。
    {
        let sr = f64::from(sample_rate.max(1));
        scratch.state.observe_latest_note(
            &scratch.midi_bus_a,
            playhead_beats,
            mod_plane.first_sample() as f64 / sr,
            f64::from(current_bpm) / (60.0 * sr),
            sample_rate,
        );
    }

    // ---- Track audio output (cleared every buffer) ----
    // 毎 buffer ゼロから組み立てる。直後に audio clip を加算し、その後 device chain が
    // port 構成に従って audio を上書き / 加算していく。
    scratch.track_l[..n].fill(0.0);
    scratch.track_r[..n].fill(0.0);

    // ---- v23 single-chain: serial port connection (Reaper 流) -----------
    // 役割判定はしない。track の MIDI (notes, midi_bus_a) と audio (clips) を
    // 起点に、各 device を順に処理し、device の port 構成に従って MIDI / audio を
    // 接続する。先に audio source (audio clip + sidechain alignment delay) を
    // track_l/r に入れてからチェーンを通す (clips → エフェクトで処理 / 音源出力に
    // 加算される)。playing == false では audio clip を mix しない (Stop で鳴り
    // 続けるバグ防止)。
    if playing && let Some(renderer) = audio_renderer {
        crate::launcher::render::render_row_audio(
            renderer,
            track_idx as usize,
            rows.track(),
            &mut scratch.track_l[..n],
            &mut scratch.track_r[..n],
            playhead_beats,
            current_bpm,
            sample_rate,
            frames,
            transpose,
            &mut crate::audio_clip_renderer::ClipRenderState {
                repitch_accum: &mut scratch.repitch_accum,
                engines: &mut scratch.stretch_engines,
                event_l: &mut scratch.stretch_out_l,
                event_r: &mut scratch.stretch_out_r,
                render_seq: &mut scratch.clip_render_seq,
            },
        );
    }
    // PR4.5 sidechain plugin-internal alignment: main 信号を遅延させて sidechain
    // source と musical time を揃える。capacity は edit-time 確保済 (RT で再確保なし)。
    if input_delay_samples > 0 {
        scratch.input_delay_line.step_in_place(
            &mut scratch.track_l[..n],
            &mut scratch.track_r[..n],
            input_delay_samples as usize,
        );
    }

    // docs/plan_modulation_followups.md §1: snapshot the **pre-FX** signal (the
    // raw audio clip / input before the device chain) for any PreFx tap / mod
    // source. Guarded so untouched tracks skip the memcpy — RT-safe. For a
    // group-with-instrument prefix (`skip_strip`) the meaningful pre-FX tap is
    // the summed bus before the suffix FX, captured in pass 2
    // (`run_group_fx_chain`), so skip the pass-1 capture here.
    // r.md #129 §18-B: 「誰かが読むか」は compile 時に焼いた値 (RT で Song を歩かない)。
    let captured_prefx = !skip_strip && (scratch.force_prefx_snapshot || program.snapshot_pre_fx);
    if captured_prefx {
        scratch.pre_fx_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fx_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }
    let snapshot_post_fx = program.snapshot_post_fx;

    // ---- device chain (r.md #110: 展開済み program を 1 本の walker で走らせる) ----
    // port 直結規則 / Parallel の fork-join / 内蔵 device は `run_chain_program` (`program.rs`)。
    // group-with-instrument は instrument prefix `[0..pass1_end]` だけ (残りは pass 2)。
    let ctx = ProgramCtx {
        song,
        plugin_refs,
        worker_sync,
        sample_rate,
        frames,
        playing,
        current_bpm,
        playhead_beats,
        loop_region,
        recording_lanes,
        mod_plane,
        rows,
        own_pre_fx: captured_prefx.then_some((&scratch.pre_fx_l[..], &scratch.pre_fx_r[..])),
        native: native_io,
        index,
        owner: ParamStoreAt::Track(track_idx),
    };
    run_chain_program(
        program,
        0..op_end,
        &mut scratch.track_l,
        &mut scratch.track_r,
        &mut scratch.midi_bus_a,
        &mut scratch.midi_bus_b,
        &ctx,
    );

    // パラアウト (docs/plan_paraout.md): a parallel-out source's pass-1 work
    // ends here — its output buses are in `buffer_aux_out` (and, for 楽器兼バス
    // mode, its main signal in `track_l/r`). The children sum + suffix FX +
    // strip all run in pass 2 (`Mix`/`MixAdditive` → `ProcessGroupFx`), so do
    // NOT apply the pre-fader snapshot / strip / mute here.
    if skip_strip {
        // 全部子 (`paraout_main_to_child`): the instrument's MAIN output goes to
        // its OWN child track (port 0 → `buffer_aux_out[0]`), so clear it from
        // the parent's scratch — the parent's clearing `Mix` then sums only the
        // children. 楽器兼バス mode (port 0 unrouted) keeps main for `MixAdditive`.
        if main_to_child {
            scratch.track_l[..n].fill(0.0);
            scratch.track_r[..n].fill(0.0);
            scratch.peak_l = 0.0;
            scratch.peak_r = 0.0;
        }
        return;
    }

    // ---- PostFx 点: SC Listen の置換 (r.md #129 K25d) ----
    // トラックのチェーン出力を、Listen 中の Comp の検出信号で置き換える。後段の device 自体は
    // 普通に走っていて状態も保たれる。pre-fader tap より前なので send / SC にも同じ音が流れる。
    apply_listen_override(program, &mut scratch.track_l, &mut scratch.track_r, n);

    // ---- Pre-fader send tap ----
    // A pre-fader send reads the post-fx, pre-strip signal. Snapshot it
    // before the strip overwrites `track_l/r` in place. docs/plan_modulation.md
    // §6: a PostFx aux-input route or mod source also reads this snapshot, so
    // capture it for those too. Only copied when something actually needs it
    // (`ChainProgram::snapshot_post_fx` は pre-fader send を含めて compile 時に焼いた値)。
    if scratch.force_prefader_snapshot || snapshot_post_fx {
        scratch.pre_fader_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fader_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    // ---- Mixer strip + peak meter ----
    // 焼き込みの program (`RenderScope::Sources` / `PostFx`) はフェーダーの段を通さない。
    if !program.fader {
        pass_strip(scratch, n);
        return;
    }
    let muted = song_track.muted;
    let solo = song_track.solo;
    // Folder solo: グループを solo したらその子も鳴る (Ableton / Reaper 準拠)。
    // 祖先 group のいずれかが solo なら、 この track 自身が非 solo でも透過させる (buffer の頭で解いた `SoloTables`)。
    let effective_mute = muted || (any_solo && !solo && !(song.is_some() && ancestor_soloed));

    // strip は常に適用する — excluded track でも `track_l/r` に post-fader
    // signal を残す (solo された return への send / sidechain tap が読める、
    // Ableton 準拠)。 mute の意味論は `apply_strip` の doc 参照。
    crate::automation::fill_track_param_ramps(
        song,
        index,
        track_idx,
        rows,
        sample_rate,
        f64::from(current_bpm),
        playhead_beats,
        frames,
        &mut scratch.volume_per_sample,
        &mut scratch.pan_per_sample,
        recording_lanes,
        mod_plane,
    );
    apply_strip(scratch, n, muted, effective_mute);
}

/// master bus の audio fx chain を直列 process する。 全 track mix 後に
/// [`render_master_buffer`] から呼ばれる (= metronome guide は master fx を
/// 通さない、 track fx と同じ worker dispatch idiom)。 plugin は
/// `master_fx_chain[i].id` (安定 device id) で `plugin_refs` を直接引き、
/// in-place で `master_l/r` を上書きする。
///
/// master fx param automation / 変調 (r.md #8): master 固有データ (`song_lanes` の
/// PluginParam lane + `song_mod_routings`) を `fill_pd_param_events(MASTER_TRACK_ID,
/// device_id)` で適用する (= track / group fx と同一経路)。
/// RT 規約: ヒープ確保 / lock / I/O なし。 buffer は呼び出し側が事前確保した
/// `master_l/r` と plugin 側 ProcessData shmem のみを使う。
#[allow(clippy::too_many_arguments)]
pub fn process_master_fx_chain(
    program: &mut ChainProgram,
    midi_a: &mut Vec<TimedNoteEvent>,
    midi_b: &mut Vec<TimedNoteEvent>,
    master_l: &mut [f32],
    master_r: &mut [f32],
    plugin_refs: &PluginRefs,
    worker_sync: Option<PairLease<'_>>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    song: Option<&Song>,
    index: &SongIndex,
    current_bpm: f32,
    playhead_beats: f64,
    loop_region: LoopRegion,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    mod_plane: ModTickPlaneRef<'_>,
    // r.md #87: マスター行 (`song_lanes`) の供給元。ランチャーで撃った行は
    // アレンジのカーブではなくセルのカーブを使う。
    master_rows: TrackRows<'_>,
    // r.md #129: 「聴き方・見方」(SC Listen / device scope)。書き出しは既定値。
    native_io: NativeIo<'_>,
) {
    // master は note を持たない = 空の MIDI バスで走らせる。
    midi_a.clear();
    program.listen_pending = None;
    let ctx = ProgramCtx {
        song,
        plugin_refs,
        worker_sync,
        sample_rate,
        frames,
        playing,
        current_bpm,
        playhead_beats,
        loop_region,
        recording_lanes,
        mod_plane,
        rows: master_rows,
        own_pre_fx: None,
        native: native_io,
        index,
        // r.md #129: master fx chain の device (組み込み Bus Comp / Tone EQ を含む) と store は song 側。
        owner: ParamStoreAt::Song,
    };
    let len = program.ops.len();
    run_chain_program(program, 0..len, master_l, master_r, midi_a, midi_b, &ctx);
}

/// 旧 pass 2 (`Schedule::nodes` の op) だけを直列トレースの順に 1 buffer 走らせる (テスト用)。pass 1 の出力は
/// 呼び側が scratch に置く。本番は [`render_master_buffer`] が pass 1 と合わせたグラフで流す。plugin host は居ない
/// (plugin の依頼口を持たない)。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn execute_schedule_post_dispatch(
    schedule: &mut Schedule,
    scratch: &mut [TrackScratch],
    master_l: &mut [f32],
    master_r: &mut [f32],
    n: usize,
    song: &Song,
    plugin_refs: &PluginRefs,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    any_solo: bool,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
    playhead_beats: f64,
    loop_region: LoopRegion,
    mod_plane: ModTickPlaneRef<'_>,
    follower_drive: FollowerDrive<'_>,
    rows: &RowSourceTable,
    native_io: NativeIo<'_>,
) {
    // solo の透過は本番 (`render_master_buffer`) と同じく表を読む手が走る前に解く。
    if any_solo {
        schedule.solo.resolve(song);
    }
    let params = BufferParams {
        sample_rate,
        frames: frames.min(n as u32),
        playing,
        any_solo,
        recording_lanes,
        current_bpm,
        playhead_beats,
        loop_region,
        mod_plane,
        follower_drive,
        rows,
        native_io,
        // pass 2 の op は移調を読まない (移調は pass 1 のノート / クリップだけに効く)。
        transpose: 0,
    };
    let index = SongIndex::build(song);
    let ctx = RenderCtx::new(song, &index, schedule, scratch, master_l, master_r, plugin_refs, None, None, params);
    crate::graph::step::run_nodes_for_test(&ctx, |_| true);
}

/// Run a Group track's audio fx chain on its already-mixed input
/// scratch, then apply the group's mixer strip (volume / pan / mute /
/// solo + peak meter). Mirrors the audio-fx tail of `process_track_owned`,
/// but skips the sequencer / MIDI FX / instrument stages because groups
/// have no clips of their own.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_group_fx_chain(
    track_idx: u32,
    song_track: &Track,
    song: &Song,
    index: &SongIndex,
    scratch: &mut TrackScratch,
    program: &mut ChainProgram,
    plugin_refs: &PluginRefs,
    worker_sync: Option<PairLease<'_>>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    any_solo: bool,
    // この bus の (流れ込む track に solo がある, 祖先 group に solo がある) (`SoloTables::of`)。
    (contributor_soloed, ancestor_soloed): (bool, bool),
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
    // group fx の transport snapshot (= 積分済み拍位置 + 実 loop トグル)。
    playhead_beats: f64,
    loop_region: LoopRegion,
    // B3 (r.md #8): group fx PluginParam の変調の値面。
    mod_plane: ModTickPlaneRef<'_>,
    // パラアウト (docs/plan_paraout.md): first program op to run. `0` for a
    // pure group / return (whole chain is bus FX). For a group-with-instrument
    // it's the prefix split point — the instrument `[0..pass1_end]` ran in
    // pass 1, so here we run only the suffix FX `[start_op..]` on the bus.
    start_op: usize,
    // r.md #87: この group の行の供給元 (レーン行の automation に効く)。
    rows: TrackRows<'_>,
    // r.md #129: 「聴き方・見方」(SC Listen / device scope)。書き出しは既定値。
    native_io: NativeIo<'_>,
) {
    let n = frames as usize;
    // r.md #129: 純粋な bus はここが program の実行開始。GWI (`start_op > 0`) は pass 1 で
    // 立った Listen の要求を PostFx 点で消費するので消さない。
    if start_op == 0 {
        program.listen_pending = None;
    }

    // docs/plan_modulation_followups.md §1: a group's pre-FX signal = the summed
    // children before its own device chain. Capture for any PreFx tap / mod
    // source (guarded — untouched groups skip the memcpy). For a
    // group-with-instrument this is the summed bus *before the suffix FX* (the
    // instrument prefix already ran), which is the right pre-FX tap point.
    // 「誰かが読むか」は compile 時に焼いた値 (r.md #129 §18-B)。`BusScAlign` の遅延はこの
    // snapshot より前に掛かっているので、自トラック Pre-FX を読む SC も揃う。
    let captured_prefx = scratch.force_prefx_snapshot || program.snapshot_pre_fx;
    if captured_prefx {
        scratch.pre_fx_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fx_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }
    let snapshot_post_fx = program.snapshot_post_fx;

    // r.md #110: bus の device 列も同じ walker。summed audio を入力に、MIDI バスは空
    // (bus は note を持たない) で `[start_op..]` を走らせる。
    scratch.midi_bus_a.clear();
    let ctx = ProgramCtx {
        song: Some(song),
        plugin_refs,
        worker_sync,
        sample_rate,
        frames,
        playing,
        current_bpm,
        playhead_beats,
        loop_region,
        recording_lanes,
        mod_plane,
        rows,
        own_pre_fx: captured_prefx.then_some((&scratch.pre_fx_l[..], &scratch.pre_fx_r[..])),
        native: native_io,
        index,
        owner: ParamStoreAt::Track(track_idx),
    };
    let len = program.ops.len();
    run_chain_program(
        program,
        start_op..len,
        &mut scratch.track_l,
        &mut scratch.track_r,
        &mut scratch.midi_bus_a,
        &mut scratch.midi_bus_b,
        &ctx,
    );

    // ---- PostFx 点: SC Listen の置換 (r.md #129 K25d、leaf と同じ位置) ----
    apply_listen_override(program, &mut scratch.track_l, &mut scratch.track_r, n);

    // ---- Pre-fader send tap (bus / return source) ----
    // A pre-fader send from this bus reads its post-fx, pre-strip signal. PostFx の tap /
    // mod source も同じ snapshot を読む (r.md #129 §18-C: 以前は leaf だけがこの条件を持っていた)。
    if scratch.force_prefader_snapshot || snapshot_post_fx {
        scratch.pre_fader_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fader_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    // 焼き込みの program (`RenderScope::Sources` / `PostFx`) はフェーダーの段を通さない (leaf と同じ)。
    if !program.fader {
        pass_strip(scratch, n);
        return;
    }
    let muted = song_track.muted;
    let solo = song_track.solo;
    // Live 互換: 子 / send 元のいずれかが solo されていれば、 この bus 自身は
    // solo フラグが無くても透過させる (`contributor_soloed`)。 さらに folder
    // solo: 祖先 group が solo なら、 このネストした group bus 自身も透過させる (`ancestor_soloed`)。
    let effective_mute = muted || (any_solo && !solo && !ancestor_soloed && !contributor_soloed);

    // strip は常に適用 (mirrors process_track_owned) — mute 意味論は
    // `apply_strip` の doc 参照。
    crate::automation::fill_track_param_ramps(
        Some(song),
        index,
        track_idx,
        rows,
        sample_rate,
        f64::from(current_bpm),
        playhead_beats,
        frames,
        &mut scratch.volume_per_sample,
        &mut scratch.pan_per_sample,
        recording_lanes,
        // r.md #129 §18-A: group / return の volume / pan にも変調を効かせる (leaf と同じ値面)。
        mod_plane,
    );
    apply_strip(scratch, n, muted, effective_mute);
}

/// r.md #89: 1 つの envelope follower を進める。
///
/// A / R / ゲイン / 帯域が**変調されている**フォロワーは係数を刻みごとに引き直し
/// ながら区間で進める。変調されていないフォロワーは buffer 全体を 1 回で舐める
/// 従来経路 — **変調していないソースのコストはゼロのまま**。
pub(super) fn advance_follower(
    fs: &mut super::follower::FollowerSlot,
    src_l: &[f32],
    src_r: &[f32],
    n: usize,
    slot: u32,
    drive: FollowerDrive<'_>,
    sample_rate: u32,
) {
    let driven = !drive.spans.is_empty()
        && drive
            .col_of_slot
            .get(slot as usize)
            .is_some_and(|c| *c != u16::MAX);
    if !driven {
        fs.process_block(src_l, src_r, n, drive.first_sample);
        return;
    }
    for span in drive.spans {
        if let Some(eff) = drive.eff_for(slot, span.row) {
            fs.set_effective(eff, sample_rate);
        }
        let a = span.frame as usize;
        let b = (a + span.frames as usize).min(n);
        if a < b {
            fs.process_block(
                &src_l[a..b],
                &src_r[a..b],
                b - a,
                drive.first_sample + a as u64,
            );
        }
    }
}

/// live (CPAL callback 経由の `ProjectRt::render_buffer`) と offline export
/// (`export::render_loop`) が共有する「1 buffer を master へ描く」単一経路
/// (`docs/plan_arch_refactor.md` §5):
///
/// 1. master バスをゼロ初期化
/// 2. 依存グラフ (track 本体 → group mix / PDC / send / sidechain / bus の chain / follower) を worker pool で
///    並列に、無ければ直列に流す (`docs/plan_parallel_graph.md`)
/// 4. master fx chain (組み込み Bus Comp / Tone EQ を含む) → master の SC Listen
/// 5. master gain
/// 6. master limiter (フェーダーの後、先読み遅延は compile 時に焼いた値で決まる)
///
/// metronome / panic declick 等 **monitoring 専用** の処理はここに入れない
/// (live 側にだけ存在する)。live と export の違いは `native_io` (聴き方・見方) だけで、
/// export は `NativeIo::default()` を渡す。RT-safe: 確保・ロック・I/O なし。
#[allow(clippy::too_many_arguments)]
pub fn render_master_buffer(
    song: &Song,
    // `song` と同じ snapshot の索引 (RT が lane / routing / node を id や target で探さないため)。
    index: &SongIndex,
    schedule: &mut Schedule,
    scratch: &mut [TrackScratch],
    plugin_refs: &PluginRefs,
    worker: Option<&WorkerRig>,
    audio_renderer: &AudioClipRenderer,
    master_l: &mut [f32],
    master_r: &mut [f32],
    sample_rate: u32,
    frames: u32,
    playing: bool,
    loop_region: LoopRegion,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
    playhead_beats: f64,
    mod_plane: ModTickPlaneRef<'_>,
    // r.md #89: フォロワーの刻みごとの係数 (`ModTickRunner::follower_drive`)。
    follower_drive: FollowerDrive<'_>,
    // r.md #87: 行ごとの時間軸の供給元。**live と export は同じ経路** (不変条件 6)
    // なので、両方がここへ同じ形で渡す。空なら全部アレンジ = 従来の挙動。
    rows: &RowSourceTable,
    master_gain: f32,
    // r.md #129: master のフェーダー後 Limiter の状態。live は `ProjectRt`、書き出しは毎回新品。
    master_limiter: &mut MasterLimiterState,
    // r.md #129: 「聴き方・見方」(SC Listen / device scope)。書き出しは既定値。
    native_io: NativeIo<'_>,
) {
    let n = (frames as usize).min(master_l.len()).min(master_r.len());
    let frames = n as u32;
    master_l[..n].fill(0.0);
    master_r[..n].fill(0.0);

    // r.md #131: 無効トラックの solo は数えない (表が compile 時に焼いた有効 / 無効を読む)。
    let any_solo = schedule.solo.any_solo(song);
    // solo の透過はこの buffer の solo から 1 回だけ解く (表を読む手が走る前)。solo が無ければ誰も読まない。
    if any_solo {
        schedule.solo.resolve(song);
    }
    // r.md #130: 曲の移調量は buffer 頭で 1 回だけ解く (track ごとに解くと同じ buffer で別の値を見うる)。
    // live と書き出しが同じここを通るので、WAV / ラウドネス解析も移調込み (不変条件 6)。
    let transpose = crate::automation::resolve_song_transpose(
        song,
        index.song_store(song),
        playhead_beats,
        recording_lanes,
        mod_plane,
    );
    let params = BufferParams {
        sample_rate,
        frames,
        playing,
        any_solo,
        recording_lanes,
        current_bpm,
        playhead_beats,
        loop_region,
        mod_plane,
        follower_drive,
        rows,
        native_io,
        transpose,
    };

    // ---- 依存グラフ: track 本体 / 合流 / PDC / send / sidechain / bus の chain / follower ----
    // (`docs/plan_parallel_graph.md`)。worker pool があればグラフで並列に、無ければ直列トレースの順に流す
    // (どちらも同じ `run_step`、結果は bit 一致)。
    let pool = worker.and_then(|rig| rig.pool.as_ref());
    // timeout した依頼を host が終えた pair を空きに戻す (runner が走る前 = 1 スレッドだけ)。
    if let Some(rig) = worker {
        rig.heal();
    }
    // グラフの壁時計は **並列 / 直列の両経路を覆う 1 箇所**で測る (`crate::graph::profile`)。
    // 経路ごとに測ると、片方だけ計測が抜けたまま「内訳が合わない」ことになる。
    let graph_started = std::time::Instant::now();
    let ran = {
        let ctx = RenderCtx::new(
            song,
            index,
            schedule,
            scratch,
            &mut master_l[..n],
            &mut master_r[..n],
            plugin_refs,
            Some(audio_renderer),
            worker,
            params,
        );
        match pool {
            Some(pool) => pool.run(&ctx),
            None => {
                let profile = ctx.rig.map(|rig| &rig.profile);
                for &step in &ctx.graph.trace {
                    let started = std::time::Instant::now();
                    run_step(&ctx, step, 0);
                    if let Some(pf) = profile {
                        pf.add_step(0, started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
                    }
                }
                true
            }
        }
    };
    if let Some(rig) = worker {
        rig.profile
            .add_buffer(graph_started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
    }
    // stall した pool はこの buffer を描き切っていない (plan §4: stalled = 無音)。途中までの track 出力 / 合流を
    // master の段へ流すと耳障りな stuck tone になるので、scratch と master を明示的に 0 にする
    // (RT-safe: 事前確保済みの buffer への fill のみ)。
    if !ran {
        for ts in scratch.iter_mut() {
            ts.track_l[..n].fill(0.0);
            ts.track_r[..n].fill(0.0);
        }
        master_l[..n].fill(0.0);
        master_r[..n].fill(0.0);
    }

    // bounce (`RenderScope::PostFx` / `Sources`) は master の段を通さない = 合流をそのまま出力する
    // (compile 時に焼いた値。master の fx chain の op も Limiter の遅延も焼いていない)。
    if !schedule.master_stage {
        return;
    }

    // ---- master fx chain ----
    // 全 track mix 後に直列 process。 live/export 両経路で通るので、 master に
    // 挿した limiter / EQ が WAV にも乗る (旧 export は素通りだった)。 r.md #129: 組み込みの
    // Bus Comp / Tone EQ もこの chain の device として走る (既定の位置は先頭)。
    process_master_fx_chain(
        &mut schedule.master_program,
        &mut schedule.master_midi_a,
        &mut schedule.master_midi_b,
        &mut master_l[..n],
        &mut master_r[..n],
        plugin_refs,
        worker.map(|rig| rig.lease(0)),
        sample_rate,
        frames,
        playing,
        Some(song),
        index,
        current_bpm,
        playhead_beats,
        loop_region,
        recording_lanes,
        mod_plane,
        rows.master_rows(),
        native_io,
    );
    // r.md #129: master の PostFx 点 = fx chain の後、master gain の前。
    apply_listen_override(&mut schedule.master_program, &mut master_l[..n], &mut master_r[..n], n);

    // ---- master gain ----
    // session の master volume。 live は従来 CPAL interleave 段で掛けていたが、
    // export が素通りだったため単一経路のここへ移動 (§5 live/export 統一)。
    if (master_gain - 1.0).abs() > f32::EPSILON {
        for i in 0..n {
            master_l[i] *= master_gain;
            master_r[i] *= master_gain;
        }
    }

    // ---- master limiter (最終段) ----
    // **フェーダーの後**が唯一正しい位置 — 前に置くとフェーダーを上げた瞬間に
    // 「出力を超えさせない」保証が破れる。値 (On / Ceiling) はオートメーション / 変調を
    // buffer 頭で解決する。先読み遅延を通すかは compile 時に焼いた値 (PDC の会計と同じ) で決まり、
    // 遅延を焼いてある間は解決値が OFF でも遅延だけを通す。
    let limiter = crate::automation::resolve_master_limiter(
        song,
        index.song_store(song),
        rows.master_rows(),
        playhead_beats,
        recording_lanes,
        mod_plane,
    );
    #[allow(clippy::cast_precision_loss)]
    let sr_f32 = sample_rate as f32;
    master_limiter.process(&limiter, schedule.master_limiter_latency, &mut master_l[..n], &mut master_r[..n], n, sr_f32);
}

/// テスト用 `PluginRefs` helper (shmem を立てずに heap の `ProcessData` を
/// 指す entry を作る)。
#[cfg(test)]
pub(crate) fn test_plugin_refs(
    entries: &[(u64, *mut common::process_data::ProcessData)],
) -> PluginRefs {
    let mut map: PluginRefs = std::collections::HashMap::new();
    for &(device_id, pd) in entries {
        map.insert(device_id, std::sync::Arc::new(PluginEntry::for_test(device_id, pd)));
    }
    map
}

#[cfg(test)]
mod sidechain_tests {
    use super::*;
    use crate::graph::{NodeOp, compile_schedule_for_test};
    use common::model::{Device, PluginInstance, Song, Track};
    use common::plugin_format::PluginFormat;

    /// v23 single-chain: `Track::default()` を mutator で埋める helper。 downstream
    /// crate (daw_audio) の test で `Track { .., ..Track::default() }` を書くと、
    /// `common` 内の `pub(crate)` legacy migration fields が見えず E0451 になる
    /// ため、 private field に触れない default + mutate で回避する。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    #[test]
    fn set_pd_transport_uses_real_beats_and_loop_toggle() {
        // SSoT 回帰防止: pd.song_pos_beats は daw_audio が渡す積分済み拍位置を
        // そのまま使い (= samples × bpm の逆算ではない)、 pd.looping は実 loop
        // トグルを反映する (= region 有無 heuristic ではない)。
        let song = Song { time_sig: (3, 4), ..Song::default() };
        let region = LoopRegion { enabled: true, start_beat: 4.0, end_beat: 8.0 };
        let mut pd = common::process_data::ProcessData::empty();
        // playhead_beats = 12.5 は constant-tempo 逆算とは無関係な「真の拍」。
        let arranger = crate::launcher::RowTimeSource::default();
        set_pd_transport(&mut pd, Some(&song), 90.0, 12.5, region, arranger);
        assert_eq!(pd.song_pos_beats, 12.5);
        // r.md #87: アレンジ主導の行は行の実効拍 == song 拍 (= 従来と同じ音)。
        assert_eq!(pd.row.pos_beats, 12.5);
        assert!(pd.row.is_arrangement() && !pd.row.is_silent());
        assert_eq!(pd.bpm, 90.0);
        assert_eq!(pd.tsig_num, 3);
        assert_eq!(pd.tsig_denom, 4);
        assert_eq!(pd.loop_start_beats, 4.0);
        assert_eq!(pd.loop_end_beats, 8.0);
        assert_eq!(pd.looping, 1);
        // loop region は定義済のまま enabled=false にすると pd.looping=0
        // (= region heuristic を使っていれば 1 のままになる、 という回帰検出)。
        set_pd_transport(
            &mut pd,
            Some(&song),
            90.0,
            12.5,
            LoopRegion { enabled: false, ..region },
            arranger,
        );
        assert_eq!(pd.looping, 0);
    }

    /// PR4 Sidechain engine-handler test: 実 plugin を立てなくても、
    /// `execute_schedule_post_dispatch` の `NodeOp::SidechainTap` ハンドラ
    /// が source TrackScratch の signal を `pd.buffer_aux_in[port]` に正しく
    /// copy することを直接検証する。 ProcessData は heap に Box で置き、
    /// `PluginEntry` を手書きして plugin_refs に登録する (v29: 安定 device id)。
    #[test]
    fn sidechain_tap_copies_source_track_into_plugin_aux_in_buffer() {
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Source".into();
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Dest".into();
                    // v23 single-chain: an audio-FX device (audio_output only,
                    // no note input) → derives as AudioEffect at device 0.
                    t.devices = vec![Device::Plugin(PluginInstance {
                        id: 42,
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.scc".into(),
                            PluginFormat::Vst3,
                            common::port_config::PortConfig {
                                has_note_input: false,
                                has_note_output: false,
                                has_audio_output: true,
                                // audio-FX device: audio を加工する → audio 入力あり。
                                has_audio_input: true,
                                has_video_input: false,
                                has_video_output: false,
                            },
                        )
                    })];
                }),
            ],
            ..Song::default()
        };
        let mut schedule = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        assert!(schedule.nodes.iter().any(|op| matches!(op, NodeOp::SidechainTap { .. })));

        const FRAMES: usize = 64;
        let mut scratch: Vec<TrackScratch> = song.tracks.iter().map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = (i as f32) * 0.1;
            scratch[0].track_r[i] = -(i as f32) * 0.1;
        }
        let mut master_l = vec![0.0f32; FRAMES];
        let mut master_r = vec![0.0f32; FRAMES];

        let mut pd = Box::new(common::process_data::ProcessData::empty());
        let pd_ptr: *mut common::process_data::ProcessData = &mut *pd;
        let plugin_refs = test_plugin_refs(&[(42, pd_ptr)]);

        execute_schedule_post_dispatch(
            &mut schedule,
            &mut scratch,
            &mut master_l,
            &mut master_r,
            FRAMES,
            &song,
            &plugin_refs,
            48_000,
            FRAMES as u32,
            true,
            false,
            &std::collections::HashSet::new(),
            song.bpm,
            0.0,
            LoopRegion::default(),
            ModTickPlaneRef::default(),
            FollowerDrive::default(),
            &RowSourceTable::default(),
            NativeIo::default(),
        );

        for i in 0..FRAMES {
            let want_l = (i as f32) * 0.1;
            let want_r = -(i as f32) * 0.1;
            assert!((pd.buffer_aux_in[0][0][i] - want_l).abs() < 1e-6);
            assert!((pd.buffer_aux_in[0][1][i] - want_r).abs() < 1e-6);
        }
        assert_eq!(pd.aux_in_active[0], 1);
    }

    /// plan §4: quarantined device の shmem には SidechainTap も触らない
    /// (まだ走っている process() と並行に書かない — poisoning contract)。
    #[test]
    fn sidechain_tap_skips_quarantined_device() {
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                }),
                track(|t| {
                    t.id = 2;
                    t.devices = vec![Device::Plugin(PluginInstance {
                        id: 42,
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.scc".into(),
                            PluginFormat::Vst3,
                            common::port_config::PortConfig {
                                has_note_input: false,
                                has_note_output: false,
                                has_audio_output: true,
                                has_audio_input: true,
                                has_video_input: false,
                                has_video_output: false,
                            },
                        )
                    })];
                }),
            ],
            ..Song::default()
        };
        let mut schedule = compile_schedule_for_test(&song, 48_000, 0).unwrap();

        const FRAMES: usize = 16;
        let mut scratch: Vec<TrackScratch> = song.tracks.iter().map(|_| TrackScratch::new()).collect();
        scratch[0].track_l[0] = 1.0;
        let mut master_l = vec![0.0f32; FRAMES];
        let mut master_r = vec![0.0f32; FRAMES];

        let mut pd = Box::new(common::process_data::ProcessData::empty());
        let pd_ptr: *mut common::process_data::ProcessData = &mut *pd;
        let plugin_refs = test_plugin_refs(&[(42, pd_ptr)]);
        plugin_refs[&42].quarantined.store(true, Ordering::Release);

        execute_schedule_post_dispatch(
            &mut schedule,
            &mut scratch,
            &mut master_l,
            &mut master_r,
            FRAMES,
            &song,
            &plugin_refs,
            48_000,
            FRAMES as u32,
            true,
            false,
            &std::collections::HashSet::new(),
            song.bpm,
            0.0,
            LoopRegion::default(),
            ModTickPlaneRef::default(),
            FollowerDrive::default(),
            &RowSourceTable::default(),
            NativeIo::default(),
        );

        assert_eq!(pd.aux_in_active[0], 0, "quarantined device's pd must be untouched");
        assert_eq!(pd.buffer_aux_in[0][0][0], 0.0);
    }
}

#[cfg(test)]
mod send_tests {
    use super::*;
    use common::model::{Send, SendMode, Song, Track};

    /// v23 single-chain: `Track::default()` を mutator で埋める helper
    /// (`sidechain_tests::track` と同趣旨、 E0451 回避)。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    const FRAMES: usize = 64;

    /// v29: send は stable `Send::id` (= 7) でアドレスされる。
    const SEND_ID: u32 = 7;

    fn song_with_send(gain: f32, mode: SendMode, enabled: bool) -> Song {
        Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Vocal".into();
                    t.sends = vec![Send {
                        id: SEND_ID,
                        dest_track_id: 2,
                        gain,
                        mode,
                        enabled,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Reverb".into();
                }),
            ],
            ..Song::default()
        }
    }

    fn empty_lanes() -> std::collections::HashSet<(u32, common::model::AutomationTarget)> {
        std::collections::HashSet::new()
    }

    /// track 0 の send (`send_id`) を track 1 の scratch へ足す (`mix_send_into` を 1 回)。
    fn send_0_to_1(scratch: &mut [TrackScratch], pre_fader: bool, song: &Song, send_id: u32, any_solo: bool) {
        let [src, dst] = scratch.get_disjoint_mut([0, 1]).expect("2 本");
        crate::graph::mix::mix_send_into(
            dst,
            1,
            src,
            pre_fader,
            song,
            &SongIndex::build(song),
            0,
            send_id,
            48_000,
            120.0,
            0.0,
            any_solo,
            &empty_lanes(),
            FRAMES,
            crate::launcher::TrackRows::default(),
            false,
        );
    }

    /// A post-fader send accumulates `src * gain` into the return scratch
    /// **on top of** whatever is already there (the prior clearing Mix is
    /// a separate op), reading the source's post-fader `track_l/r`.
    #[test]
    fn post_fader_send_accumulates_src_times_gain() {
        let song = song_with_send(0.5, SendMode::PostFader, true);
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = (i as f32) * 0.1;
            scratch[0].track_r[i] = -(i as f32) * 0.1;
            scratch[1].track_l[i] = 1.0; // pre-existing return content
            scratch[1].track_r[i] = 2.0;
        }
        send_0_to_1(&mut scratch, false, &song, SEND_ID, false);
        for i in 0..FRAMES {
            let want_l = 1.0 + (i as f32) * 0.1 * 0.5;
            let want_r = 2.0 + (-(i as f32) * 0.1) * 0.5;
            assert!((scratch[1].track_l[i] - want_l).abs() < 1e-6, "l[{i}]");
            assert!((scratch[1].track_r[i] - want_r).abs() < 1e-6, "r[{i}]");
        }
    }

    /// A disabled send contributes nothing (per-send mute).
    #[test]
    fn disabled_send_contributes_silence() {
        let song = song_with_send(0.5, SendMode::PostFader, false);
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = 1.0;
            scratch[1].track_l[i] = 3.0;
        }
        send_0_to_1(&mut scratch, false, &song, SEND_ID, false);
        for i in 0..FRAMES {
            assert_eq!(scratch[1].track_l[i], 3.0, "disabled send must not change dst");
        }
    }

    /// An *explicitly* muted source silences its sends.
    #[test]
    fn explicitly_muted_source_send_contributes_silence() {
        let mut song = song_with_send(1.0, SendMode::PostFader, true);
        song.tracks[0].muted = true; // explicit mute kills the send
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = 1.0;
            scratch[1].track_l[i] = 3.0;
        }
        send_0_to_1(&mut scratch, false, &song, SEND_ID, false);
        for i in 0..FRAMES {
            assert_eq!(
                scratch[1].track_l[i], 3.0,
                "explicitly muted source must not feed its send"
            );
        }
    }

    /// Under solo, a send must respect BOTH the source's and the
    /// destination's solo state: soloing one source must not leak other
    /// tracks' sends into a shared return, but soloing the return itself
    /// auditions everything routed to it.
    #[test]
    fn send_under_solo_respects_source_and_return_solo() {
        let render = |solo_src: bool, solo_dest: bool| -> f32 {
            let mut song = song_with_send(1.0, SendMode::PostFader, true);
            song.tracks[0].solo = solo_src; // Vocal (source)
            song.tracks[1].solo = solo_dest; // Reverb return (dest)
            let mut scratch: Vec<TrackScratch> =
                (0..4).map(|_| TrackScratch::new()).collect();
            scratch[0].track_l[0] = 0.5;
            send_0_to_1(&mut scratch, false, &song, SEND_ID, true);
            scratch[1].track_l[0]
        };
        // A soloed source still feeds its own send.
        assert!(
            (render(true, false) - 0.5).abs() < 1e-6,
            "a soloed source still feeds its send"
        );
        // Neither the source audible nor the return soloed → blocked, so
        // soloing one track does not leak other tracks' sends.
        assert_eq!(
            render(false, false),
            0.0,
            "a non-audible source must not leak into the return"
        );
        // Return explicitly soloed → audition: the send flows even from a
        // non-soloed source.
        assert!(
            (render(false, true) - 0.5).abs() < 1e-6,
            "soloing the return auditions the sends feeding it"
        );
    }

    /// A pre-fader send reads the source's `pre_fader_l/r`, not its
    /// post-fader `track_l/r`.
    #[test]
    fn pre_fader_send_reads_pre_fader_buffer() {
        let song = song_with_send(1.0, SendMode::PreFader, true);
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        for i in 0..FRAMES {
            scratch[0].track_l[i] = 9.0; // post-fader — must be ignored
            scratch[0].track_r[i] = 9.0;
            scratch[0].pre_fader_l[i] = 0.25; // pre-fader — must be used
            scratch[0].pre_fader_r[i] = 0.5;
        }
        send_0_to_1(&mut scratch, true, &song, SEND_ID, false);
        for i in 0..FRAMES {
            assert!(
                (scratch[1].track_l[i] - 0.25).abs() < 1e-6,
                "pre-fader send must read pre_fader_l"
            );
            assert!((scratch[1].track_r[i] - 0.5).abs() < 1e-6);
        }
    }

    /// v29 回帰: `MixSend` は stable send id で解決するので、 未知の id は
    /// 何も寄与しない (positional index 解釈に fall back しない)。
    #[test]
    fn unknown_send_id_contributes_silence() {
        let song = song_with_send(1.0, SendMode::PostFader, true);
        let mut scratch: Vec<TrackScratch> = (0..4).map(|_| TrackScratch::new()).collect();
        scratch[0].track_l[0] = 1.0;
        scratch[1].track_l[0] = 3.0;
        send_0_to_1(&mut scratch, false, &song, 999, false);
        assert_eq!(scratch[1].track_l[0], 3.0, "unknown send id must be a no-op");
    }

    /// Solo-safe returns: when a track that aux-sends into a return is
    /// soloed, the return must count as having a soloed contributor so the
    /// solo rule keeps it audible instead of muting it. Regression for the
    /// user-reported "soloed track's send reaches the FX, but the return
    /// fader meter is dead and there is no sound".
    #[test]
    fn soloed_send_source_keeps_return_solo_safe() {
        // song_with_send: Vocal (id 1) post-fader sends to Reverb (id 2).
        let mut song = song_with_send(1.0, SendMode::PostFader, true);
        let mut sched = crate::graph::compile_schedule_for_test(&song, 48_000, 0).expect("compile");
        song.tracks[0].solo = true; // solo the send SOURCE (Vocal)
        sched.solo.resolve(&song);
        assert!(sched.solo.of(1).0, "Reverb return must be solo-safe when its send source is soloed");
        assert!(!sched.solo.of(0).0, "the source itself has no soloed contributor");
        // Nothing soloed → the return has no soloed contributor.
        song.tracks[0].solo = false;
        sched.solo.resolve(&song);
        assert!(!sched.solo.of(1).0, "with nothing soloed, the return has no soloed contributor");
    }

    /// Folder solo: soloing a GROUP must keep its children audible (Ableton /
    /// Reaper folder behavior). The leaf strip rule excludes a non-soloed
    /// track under solo only when no ancestor group is soloed, so a child of
    /// a soloed group is NOT effective-muted. Guards the `solo_ancestors`
    /// condition in the effective-mute formula.
    #[test]
    fn soloed_group_keeps_children_audible() {
        // id 10 = group, id 11 = child of 10, id 12 = unrelated.
        let mut song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 10;
                    t.solo = true;
                }), // solo the group
                track(|t| {
                    t.id = 11;
                    t.parent_group_id = Some(10);
                }),
                track(|t| t.id = 12),
            ],
            ..Default::default()
        };

        let mut sched = crate::graph::compile_schedule_for_test(&song, 48_000, 0).expect("compile");
        sched.solo.resolve(&song);
        // child: not soloed itself, but its ancestor group is → audible.
        assert!(sched.solo.of(1).1, "child sees the soloed ancestor group");
        // unrelated track: no soloed ancestor → excluded (silent) under solo.
        assert!(!sched.solo.of(2).1, "unrelated track is silenced while a group is soloed");
        song.tracks[0].solo = false;
        sched.solo.resolve(&song);
        assert!(!sched.solo.of(1).1, "group の solo を外せば child は透過しない");
    }

    /// 入れ子の group と send が混ざっても、透過の解は「流れ込む track / 祖先を全部舐めて solo を探す」のと同じ。
    /// 孫 → 子 group → 親 group、孫 → return への send、無関係な track。
    #[test]
    fn solo_の透過は入れ子の_group_と_send_を辿った解と同じ() {
        let child = |id: u32, parent: u32| {
            track(|t| {
                t.id = id;
                t.parent_group_id = Some(parent);
            })
        };
        // 0: 親 group / 1: 子 group / 2: 孫 / 3: return / 4: 無関係
        let mut song = Song {
            tracks: vec![track(|t| t.id = 1), child(2, 1), child(3, 2), track(|t| t.id = 4), track(|t| t.id = 5)],
            ..Default::default()
        };
        song.tracks[2].sends.push(common::model::Send {
            id: 1,
            dest_track_id: 4,
            gain: 1.0,
            mode: SendMode::PostFader,
            enabled: false,
        });
        let mut sched = crate::graph::compile_schedule_for_test(&song, 48_000, 0).expect("compile");
        // (solo にする track, 透過の解 [(流れ込む側に solo, 祖先に solo)] × 5)
        let cases: [(usize, [(bool, bool); 5]); 3] = [
            (2, [(true, false), (true, false), (false, false), (true, false), (false, false)]),
            (0, [(false, false), (false, true), (false, true), (false, false), (false, false)]),
            (1, [(true, false), (false, false), (false, true), (false, false), (false, false)]),
        ];
        for (soloed, want) in cases {
            for (i, t) in song.tracks.iter_mut().enumerate() {
                t.solo = i == soloed;
            }
            sched.solo.resolve(&song);
            let got: Vec<(bool, bool)> = (0..5).map(|i| sched.solo.of(i)).collect();
            assert_eq!(got, want, "solo = track {soloed}");
        }
    }
}

/// C (plan §5): live/export 統一経路の検証 — `render_master_buffer` が
/// master fx chain を通し、 master gain を適用することを、 plugin を立てずに
/// (= fx は lookup miss で素通り) 検証できる範囲で押さえる。
#[cfg(test)]
mod render_master_tests {
    use super::*;
    use crate::graph::compile_schedule_for_test;
    use common::model::{LoopRegion, Song, Track};
    use std::collections::HashMap;

    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    /// master gain が render 経路内で適用される (= export にも乗る)。
    /// track 出力は無音 (plugin なし / clip なし) なので、 gain 適用の検証は
    /// scratch に事前注入した信号を master Mix が拾う形で行う…はできない
    /// (pass 1 が scratch をクリアする) ため、 gain != 1.0 でも無音が保たれる
    /// こと + 経路が panic しないことの smoke に留める。 実信号での検証は
    /// export 統合テスト (headless script) が担う。
    #[test]
    fn render_master_buffer_smoke_with_gain() {
        let song = Song {
            tracks: vec![track(|t| t.id = 1)],
            ..Song::default()
        };
        let mut schedule = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        let mut scratch: Vec<TrackScratch> = song.tracks.iter().map(|_| TrackScratch::new()).collect();
        let mut master_l = vec![7.0f32; 64]; // 前 buffer の残骸 — clear されるべき
        let mut master_r = vec![7.0f32; 64];
        let plugin_refs: PluginRefs = HashMap::new();
        let renderer = crate::audio_clip_renderer::AudioClipRenderer::empty();
        render_master_buffer(
            &song,
            &SongIndex::build(&song),
            &mut schedule,
            &mut scratch,
            &plugin_refs,
            None,
            &renderer,
            &mut master_l,
            &mut master_r,
            48_000,
            64,
            true,
            LoopRegion::default(),
            &std::collections::HashSet::new(),
            120.0,
            0.0,
            ModTickPlaneRef::default(),
            FollowerDrive::default(),
            &RowSourceTable::default(),
            0.5,
            &mut MasterLimiterState::new(),
            NativeIo::default(),
        );
        assert!(master_l.iter().all(|&v| v == 0.0), "master must be cleared+silent");
        assert!(master_r.iter().all(|&v| v == 0.0));
    }

    /// r.md #130: live と書き出しが共有する 1 buffer の描画で、曲の移調 +2 はノートを追従するトラックだけ
    /// +2 の鍵盤で鳴らし、追従しないトラック (自分で外した / 追従しないグループの子) は書いた鍵盤のまま。
    /// 移調のレーンを録音中は、カーブではなく基準値で鳴る (ノブの値を素通し)。
    #[test]
    fn transpose_moves_only_following_tracks_in_the_shared_render() {
        use common::model::{AutomationLane, AutomationTarget, Clip, ClipContent, MidiContent, Note};
        let mut song = Song { transpose: 2, ..Song::default() };
        let content_id = song.alloc_content_id();
        let note = Note { id: 1, start_beat: 0.0, duration_beats: 1.0, pitch: 60, velocity: 100, lyric: None, muted: false };
        song.clip_contents.insert(content_id, ClipContent::Midi(MidiContent { notes: vec![note], next_note_id: 2 }));
        let clip = Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id, ..Clip::default() };
        // 1: 追従 / 2: 自分で外す / 3: 追従しないグループ 4 の子。
        song.tracks = vec![
            track(|t| {
                t.id = 1;
                t.clips = vec![clip.clone()];
            }),
            track(|t| {
                t.id = 2;
                t.follow_transpose = false;
                t.clips = vec![clip.clone()];
            }),
            track(|t| {
                t.id = 3;
                t.parent_group_id = Some(4);
                t.clips = vec![clip.clone()];
            }),
            track(|t| {
                t.id = 4;
                t.follow_transpose = false;
            }),
        ];
        let render = |song: &Song, recording: &std::collections::HashSet<(u32, AutomationTarget)>| {
            let mut schedule = compile_schedule_for_test(song, 48_000, 0).unwrap();
            let mut scratch: Vec<TrackScratch> = song.tracks.iter().map(|_| TrackScratch::new()).collect();
            let (mut l, mut r) = (vec![0.0f32; 64], vec![0.0f32; 64]);
            render_master_buffer(
                song,
                &SongIndex::build(song),
                &mut schedule,
                &mut scratch,
                &HashMap::new(),
                None,
                &crate::audio_clip_renderer::AudioClipRenderer::empty(),
                &mut l,
                &mut r,
                48_000,
                64,
                true,
                LoopRegion::default(),
                recording,
                120.0,
                0.0,
                ModTickPlaneRef::default(),
                FollowerDrive::default(),
                &RowSourceTable::default(),
                1.0,
                &mut MasterLimiterState::new(),
                NativeIo::default(),
            );
            scratch.iter().take(3).map(|s| s.state.active_notes.iter().map(|n| n.key).collect()).collect::<Vec<Vec<u8>>>()
        };
        let none = std::collections::HashSet::new();
        assert_eq!(render(&song, &none), vec![vec![62], vec![60], vec![60]]);

        // レーン (全域 -5) があればレーンが勝つ。録音中のレーンは基準値 (+2) に戻る。
        song.song_lanes.push(AutomationLane { id: 1, ..AutomationLane::new(AutomationTarget::SongTranspose, -5.0) });
        assert_eq!(render(&song, &none)[0], vec![55]);
        let recording = std::collections::HashSet::from([(common::model::MASTER_TRACK_ID, AutomationTarget::SongTranspose)]);
        assert_eq!(render(&song, &recording)[0], vec![62]);
    }
}
