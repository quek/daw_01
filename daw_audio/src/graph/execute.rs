//! Schedule 実行 (per-buffer render 経路)。engine.rs から分離
//! (`docs/plan_arch_refactor.md` §5)。
//!
//! [`render_master_buffer`] が「1 buffer を master へ描く」単一経路:
//! worker dispatch (per-track pass 1) → schedule 実行 (group mix / PDC /
//! send / sidechain / follower) → master fx chain → master gain。
//! **live (CPAL callback) と offline export (freewheel) の両方がこれを呼ぶ**
//! ので、master に挿した limiter が WAV に乗らない類の live/export 乖離が
//! 構造的に起きない。metronome / panic declick 等 monitoring 専用の処理は
//! live 側 (engine.rs / main.rs) にだけ残る。
//!
//! RT 規約: この module の関数はすべて audio callback / audio worker /
//! export freewheel から呼ばれる。ヒープ確保・ロック・I/O・tracing を
//! 行わない。plugin dispatch は **有界** (`DISPATCH_TIMEOUT_MS`) で、
//! timeout は per-device quarantine + per-pair poison を立てて以後 skip する
//! (plan §4、`common::plugin_ref` module doc の poisoning contract)。

use std::sync::atomic::Ordering;

use common::model::{LoopRegion, Song, Track};
use common::plugin_ref::{DISPATCH_TIMEOUT_MS, DispatchOutcome};
use common::process_data::EventKind;

use crate::audio_clip_renderer::AudioClipRenderer;
use crate::engine::{MAX_TRACKS, PluginEntry, PluginRefs, SyncSlot, WorkerRig};
use crate::graph::mix::{
    has_soloed_contributor, mix_into_master, mix_into_track_scratch, mix_send_into_track_scratch,
    resolve_tap_buffers, track_needs_prefader_snapshot, track_needs_prefx_snapshot,
};
use crate::graph::{BufRef, NodeOp, Schedule};
use crate::launcher::{RowSourceTable, TrackRows};
use crate::mixer::{TrackScratch, apply_strip};
use crate::sequencer::{NoteTransition, TimedNoteEvent};

/// この device / pair が dispatch 可能かどうか (quarantine / poison gate)。
/// gate を通らない device は **shmem (`ProcessData`) にも触らない** — timeout
/// した device の `process()` はまだ plugin_host 側で走っている可能性があり、
/// 入力を書き込むと並行 process と race する (poisoning contract)。
#[inline]
fn pair_usable(slot: &SyncSlot, entry: &PluginEntry) -> bool {
    !entry.quarantined.load(Ordering::Acquire) && !slot.poisoned.load(Ordering::Acquire)
}

/// 1 device を worker pair へ **有界** dispatch する (plan §4)。 timeout は
/// (a) この pair を poison (pool 再構築まで dispatch 禁止 — contract)、
/// (b) この device を quarantine (以後 skip して bypass) して `false` を返す。
/// 通知は RT からは行わない — flag を notify スレッド (`main.rs`) が poll して
/// `AudioEvent::PluginUnresponsive` を 1 回だけ送る。 RT-safe: atomic store のみ。
#[inline]
fn dispatch_bounded(slot: &SyncSlot, entry: &PluginEntry) -> bool {
    match slot.sync.dispatch(entry.plugin_ref.device_id, DISPATCH_TIMEOUT_MS) {
        Ok(DispatchOutcome::Done) => true,
        Ok(DispatchOutcome::TimedOut) => {
            slot.poisoned.store(true, Ordering::Release);
            entry.quarantined.store(true, Ordering::Release);
            false
        }
        // WAIT_FAILED 等 (handle 破棄直後など)。 この buffer は skip する
        // だけで poison しない (次 pool 再構築で自然回復する一時状態)。
        Err(_) => false,
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
    plugin_refs: &PluginRefs,
    audio_renderer: Option<&AudioClipRenderer>,
    worker_sync: Option<&SyncSlot>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    song: Option<&Song>,
    any_solo: bool,
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
    // docs/plan_modulation.md §5: per-`ModSource` follower scalars (block-rate
    // snapshot, slot = `Song::mod_sources` position). fill_track_param_ramps /
    // fill_pd_param_events に渡して volume/pan/plugin param を follower 変調する。
    // 空なら変調なし (= 既存挙動と byte 同一)。
    mod_scalars: &[f32],
    // r.md #87: この track の行 (トラック行 + レーン行) の時間軸の供給元。
    // ループ端での buffer 分割は `crate::launcher::render` が持つ。
    // 空 (`TrackRows::default()`) で全部アレンジ = 従来の挙動。
    rows: TrackRows<'_>,
) {
    let n = frames as usize;

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
    let (device_end, skip_strip) = match song {
        Some(s) => {
            let id = song_track.id;
            let has_children = s.track_has_children(id);
            let split = song_track.paraout_split_device();
            if has_children && split.is_some() {
                (split.unwrap_or(0) as usize, true)
            } else if has_children
                || s.track_receives_send(id)
                || s.track_receives_paraout(id)
            {
                scratch.track_l[..n].fill(0.0);
                scratch.track_r[..n].fill(0.0);
                scratch.peak_l = 0.0;
                scratch.peak_r = 0.0;
                scratch.effective_mute = false;
                return;
            } else {
                (song_track.devices.len(), false)
            }
        }
        None => (song_track.devices.len(), false),
    };

    // ---- Sequencer: assemble this buffer's MIDI bus ----
    scratch.midi_bus_a.clear();
    for &k in &scratch.state.pending_offs {
        // pending_offs は stuck note flush 用なので note_id 不明 → 0
        // (= "未指定" 相当)。 builtin plugin は voice cleanup で key 一致
        // で停止するので、 note_id 0 でも実害なし。
        scratch.midi_bus_a.push(TimedNoteEvent {
            time: 0,
            event: NoteTransition::Off { note_id: 0, key: k },
        });
    }
    scratch.state.pending_offs.clear();
    // 鍵盤レーン click のプレビュー note (engine の pump_commands が該当 track の
    // pending_preview に積む)。 transport に関係なく frame 0 で 1 回注入する
    // (instrument dispatch は playing で gate されないので停止中でも発音する)。
    // collect_events_for_buffer より前に push し、 playing 時は同 buffer の
    // sort (CLAP の time 昇順 / 同 time は Off→On) に乗せる。
    for &ev in &scratch.state.pending_preview {
        scratch.midi_bus_a.push(TimedNoteEvent { time: 0, event: ev });
    }
    scratch.state.pending_preview.clear();
    if playing {
        crate::launcher::render::collect_row_midi(
            song,
            track_idx,
            rows.track(),
            sample_rate,
            playhead_beats,
            current_bpm,
            frames,
            &mut scratch.midi_bus_a,
            &mut scratch.state.active_notes,
        );
    }

    let track_id = song_track.id;

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
    if !skip_strip && song.is_some_and(|s| track_needs_prefx_snapshot(s, track_id)) {
        scratch.pre_fx_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fx_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    // パラアウト: a group-with-instrument runs only its prefix `[0..device_end]`
    // in pass 1; a leaf runs its whole chain (`device_end == devices.len()`).
    for i in 0..device_end {
        // v29: 安定 device id で直接 lookup。 song.tracks の Vec position にも
        // chain 内 index にも依存しないので、 reorder / 削除で lookup が壊れない。
        let device = &song_track.devices[i];
        let ports = device.ports;
        let Some(entry) = plugin_refs.get(&device.id) else {
            continue;
        };
        let Some(ws) = worker_sync else { continue };
        // quarantine / poison gate — 通らない device は pd にも触らない
        // (並行 process との race 回避、 冒頭の contract 参照)。
        if !pair_usable(ws, entry) {
            continue;
        }

        let pd = entry.plugin_ref.data_mut();
        pd.prepare();
        pd.frames = frames;
        pd.playing = if playing { 1 } else { 0 };
        pd.sample_rate = sample_rate;
        set_pd_transport(pd, song, current_bpm, playhead_beats, loop_region, rows.track());
        // ---- inputs: device の port を持つものだけ現在のバスを渡す ----
        // M1 (r.md #8): note を param automation より **先に** push する。 B4 の
        // sub-buffer param automation は events_in (MAX_EVENTS=256) を最大
        // frames/64 event/lane 消費するので、 param を先に積むと大量 automation 時に
        // 後続の NoteOff が溢れて drop → ハングノートになる。 note を先に確保すれば
        // 溢れるのは automation 側だけ (= 音は詰まらず automation が step するのみ)。
        // plugin host は event を time 順に sort するので発音順序は不変。
        if ports.has_note_input {
            for ev in &scratch.midi_bus_a {
                match ev.event {
                    NoteTransition::On { note_id, key, velocity } => {
                        pd.push_note_on(ev.time, key, velocity, 0, note_id)
                    }
                    NoteTransition::Off { note_id, key } => {
                        pd.push_note_off(ev.time, key, 0, note_id)
                    }
                }
            }
        }
        if let Some(song) = song {
            crate::automation::fill_pd_param_events(
                pd,
                song,
                track_id,
                rows,
                device.id,
                sample_rate,
                f64::from(current_bpm),
                playhead_beats,
                frames,
                recording_lanes,
                mod_scalars,
            );
        }
        if ports.has_audio_input {
            pd.buffer_in[0][..n].copy_from_slice(&scratch.track_l[..n]);
            pd.buffer_in[1][..n].copy_from_slice(&scratch.track_r[..n]);
        }
        if !dispatch_bounded(ws, entry) {
            continue;
        }
        // ---- outputs ----
        // note 出力を持つなら出力 MIDI で次段のバスを置き換える (無ければ素通し)。
        if ports.has_note_output {
            scratch.midi_bus_b.clear();
            let n_out = pd.n_events_out as usize;
            for ev in &pd.events_out[..n_out.min(pd.events_out.len())] {
                let timed = match ev.kind {
                    EventKind::NoteOn => TimedNoteEvent {
                        time: ev.time,
                        event: NoteTransition::On {
                            note_id: ev.note_id,
                            key: ev.key,
                            velocity: ev.velocity,
                        },
                    },
                    EventKind::NoteOff => TimedNoteEvent {
                        time: ev.time,
                        event: NoteTransition::Off {
                            note_id: ev.note_id,
                            key: ev.key,
                        },
                    },
                    EventKind::ParamValue | EventKind::ParamMod => continue,
                };
                scratch.midi_bus_b.push(timed);
            }
            scratch.midi_bus_b.sort_unstable_by_key(|e| e.time);
            std::mem::swap(&mut scratch.midi_bus_a, &mut scratch.midi_bus_b);
        }
        // audio 出力を持つなら: audio 入力も持つ機 (= エフェクト) は処理結果で
        // 置き換え、入力を持たない機 (= 音源/生成器) はソースとして加算する。
        if ports.has_audio_output {
            if ports.has_audio_input {
                scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
                scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
            } else {
                for j in 0..n {
                    scratch.track_l[j] += pd.buffer_out[0][j];
                    scratch.track_r[j] += pd.buffer_out[1][j];
                }
            }
        }
    }

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
        if song_track.paraout_main_to_child() {
            scratch.track_l[..n].fill(0.0);
            scratch.track_r[..n].fill(0.0);
            scratch.peak_l = 0.0;
            scratch.peak_r = 0.0;
        }
        return;
    }

    // ---- Pre-fader send tap ----
    // A pre-fader send reads the post-fx, pre-strip signal. Snapshot it
    // before the strip overwrites `track_l/r` in place. docs/plan_modulation.md
    // §6: a PostFx aux-input route or mod source also reads this snapshot, so
    // capture it for those too. Only copied when something actually needs it
    // (cheap check; skips the memcpy otherwise).
    let has_prefader_send = song_track
        .sends
        .iter()
        .any(|s| s.mode == common::model::SendMode::PreFader);
    if has_prefader_send
        || song.is_some_and(|s| track_needs_prefader_snapshot(s, song_track.id))
    {
        scratch.pre_fader_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fader_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    // ---- Mixer strip + peak meter ----
    let muted = song_track.muted;
    let solo = song_track.solo;
    // Folder solo: グループを solo したらその子も鳴る (Ableton / Reaper 準拠)。
    // 祖先 group のいずれかが solo なら、 この track 自身が非 solo でも透過させる。
    let ancestor_soloed = song.is_some_and(|s| s.ancestor_soloed(song_track.id));
    let effective_mute = muted || (any_solo && !solo && !ancestor_soloed);

    // strip は常に適用する — excluded track でも `track_l/r` に post-fader
    // signal を残す (solo された return への send / sidechain tap が読める、
    // Ableton 準拠)。 mute の意味論は `apply_strip` の doc 参照。
    crate::automation::fill_track_param_ramps(
        song,
        track_idx,
        rows,
        sample_rate,
        f64::from(current_bpm),
        playhead_beats,
        frames,
        &mut scratch.volume_per_sample,
        &mut scratch.pan_per_sample,
        recording_lanes,
        mod_scalars,
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
    master_fx_chain: &[common::model::PluginInstance],
    master_l: &mut [f32],
    master_r: &mut [f32],
    plugin_refs: &PluginRefs,
    worker_sync: Option<&SyncSlot>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    song: Option<&Song>,
    current_bpm: f32,
    playhead_beats: f64,
    loop_region: LoopRegion,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    mod_scalars: &[f32],
    // r.md #87: マスター行 (`song_lanes`) の供給元。ランチャーで撃った行は
    // アレンジのカーブではなくセルのカーブを使う。
    master_rows: TrackRows<'_>,
) {
    let n = frames as usize;
    let Some(ws) = worker_sync else { return };
    for device in master_fx_chain {
        let Some(entry) = plugin_refs.get(&device.id) else {
            continue;
        };
        if !pair_usable(ws, entry) {
            continue;
        }
        let pd = entry.plugin_ref.data_mut();
        pd.prepare();
        pd.frames = frames;
        pd.playing = if playing { 1 } else { 0 };
        pd.sample_rate = sample_rate;
        set_pd_transport(pd, song, current_bpm, playhead_beats, loop_region, master_rows.track());
        // master fx param automation (`song_lanes` の PluginParam lane) + 変調
        // (`song_mod_routings`) を MASTER_TRACK_ID 経路で適用 (r.md #8、 track/group
        // fx と同一 idiom)。
        if let Some(song) = song {
            crate::automation::fill_pd_param_events(
                pd,
                song,
                common::model::MASTER_TRACK_ID,
                master_rows,
                device.id,
                sample_rate,
                f64::from(current_bpm),
                playhead_beats,
                frames,
                recording_lanes,
                mod_scalars,
            );
        }
        pd.buffer_in[0][..n].copy_from_slice(&master_l[..n]);
        pd.buffer_in[1][..n].copy_from_slice(&master_r[..n]);
        if !dispatch_bounded(ws, entry) {
            continue;
        }
        master_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
        master_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
    }
}

/// Replay the post-dispatch portion of the routing schedule:
/// `Mix { dst: TrackScratch }` (children → group bus), `ProcessGroupFx`
/// (group's fx_chain + strip), and `Mix { dst: Master }` (top-level
/// scratches → master). `ProcessTrack` ops are no-ops here because
/// the pass-1 dispatch has already filled the per-track scratches.
#[allow(clippy::too_many_arguments)]
pub fn execute_schedule_post_dispatch(
    schedule: &mut Schedule,
    scratch: &mut [TrackScratch],
    master_l: &mut [f32],
    master_r: &mut [f32],
    n: usize,
    song: &Song,
    plugin_refs: &PluginRefs,
    worker_sync: Option<&SyncSlot>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    any_solo: bool,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
    // group fx の transport snapshot 用 (= 積分済み拍位置 + 実 loop トグル)。
    playhead_beats: f64,
    loop_region: LoopRegion,
    // B3 (r.md #8): group fx の PluginParam follower 変調 snapshot (track fx と同じ
    // `mod_scalars_snapshot`)。 post-dispatch 段への plumbing。 空なら変調なし。
    mod_scalars: &[f32],
    // r.md #87: 行ごとの時間軸の供給元 (group の automation レーン行に効く)。
    rows: &RowSourceTable,
) {
    // `nodes` の不変参照と `delay_lines` の可変参照を同時に取りたい
    // (ApplyDelay で line を引きながら nodes を回すため)。 `Schedule`
    // を split borrow で 2 つの参照に分解する。
    let Schedule {
        nodes,
        delay_lines,
        delay_keys: _,
        port_buffers: _,
        input_delay_per_track: _,
        follower_slots,
        follower_keys: _,
        mod_kinds: _,
        master_latency_samples: _,
    } = schedule;
    for op in nodes.iter() {
        match op {
            NodeOp::ProcessTrack { .. } => {
                // Already handled by the pass-1 dispatch.
            }
            NodeOp::Mix {
                srcs,
                dst: BufRef::TrackScratch(target_idx),
            } => {
                mix_into_track_scratch(scratch, *target_idx as usize, srcs, n, true);
            }
            NodeOp::Mix {
                srcs,
                dst: BufRef::Master,
            } => {
                mix_into_master(scratch, srcs, master_l, master_r, n);
            }
            // パラアウト (docs/plan_paraout.md): a group-with-instrument's
            // children are summed **on top of** its own instrument output
            // (already in scratch from the pass-1 prefix), so we accumulate
            // instead of clearing. dst is always its own TrackScratch.
            NodeOp::MixAdditive {
                srcs,
                dst: BufRef::TrackScratch(target_idx),
            } => {
                mix_into_track_scratch(scratch, *target_idx as usize, srcs, n, false);
            }
            NodeOp::MixAdditive { .. } => {
                // Only TrackScratch dsts are ever emitted for MixAdditive.
            }
            NodeOp::Mix {
                dst:
                    BufRef::Pooled(_)
                    | BufRef::PreFaderScratch(_)
                    | BufRef::PreFxScratch(_),
                ..
            } => {
                // Pooled targets land here once pooled-bus routing arrives.
                // A Mix into a Pre*Scratch is never emitted (those are written
                // by ProcessTrack), but the arm keeps the match exhaustive.
            }
            NodeOp::ProcessGroupFx {
                track_idx,
                start_device,
            } => {
                let Some(track) = song.tracks.get(*track_idx as usize) else {
                    continue;
                };
                let Some(target) = scratch.get_mut(*track_idx as usize) else {
                    continue;
                };
                run_group_fx_chain(
                    *track_idx,
                    track,
                    song,
                    target,
                    plugin_refs,
                    worker_sync,
                    sample_rate,
                    frames,
                    playing,
                    any_solo,
                    recording_lanes,
                    current_bpm,
                    playhead_beats,
                    loop_region,
                    mod_scalars,
                    *start_device,
                    rows.track_rows(*track_idx as usize),
                );
            }
            NodeOp::ApplyDelay {
                buf,
                line_idx,
                frames: delay_frames,
            } => {
                // PR3: `buf` の scratch を in-place で `delay_frames` だけ
                // 遅延させる。 `compile_schedule` は path latency が大きい
                // 側に揃えるため、 小さい side の `BufRef::TrackScratch(i)`
                // を絶対指す前提。 想定外 BufRef は無視。
                let BufRef::TrackScratch(track_idx) = *buf else {
                    continue;
                };
                let Some(s) = scratch.get_mut(track_idx as usize) else {
                    continue;
                };
                let Some(line) = delay_lines.get_mut(*line_idx as usize) else {
                    continue;
                };
                let n = (n).min(s.track_l.len()).min(s.track_r.len());
                line.step_in_place(
                    &mut s.track_l[..n],
                    &mut s.track_r[..n],
                    *delay_frames as usize,
                );
            }
            NodeOp::SidechainTap {
                src,
                device_id,
                aux_in_port,
            } => {
                // PR4 sidechain: copy the source track's scratch L/R into the
                // destination plugin's `pd.buffer_aux_in[port]` shmem region,
                // marking the port active so `daw_plugin_host` forwards it as a
                // CLAP `clap_audio_buffer` / VST3 aux bus on the next
                // `process()`. docs/plan_modulation_followups.md §1: the tap
                // point picks the source buffer — PostFader / PostFx
                // (pre-fader) / PreFx. v29: 宛先 plugin は安定 device id。
                // RT path: skip silently on any miss (no per-buffer tracing).
                let Some((src_l, src_r)) = resolve_tap_buffers(scratch, *src) else {
                    continue;
                };
                let port = *aux_in_port as usize;
                if port >= common::process_data::MAX_AUX_IN {
                    continue;
                }
                let Some(entry) = plugin_refs.get(device_id) else {
                    continue;
                };
                // quarantined device の pd には触らない (process が走ったまま
                // の可能性がある — poisoning contract)。
                if entry.quarantined.load(Ordering::Acquire) {
                    continue;
                }
                let pd = entry.plugin_ref.data_mut();
                let copy_n = n.min(src_l.len()).min(src_r.len());
                pd.buffer_aux_in[port][0][..copy_n].copy_from_slice(&src_l[..copy_n]);
                pd.buffer_aux_in[port][1][..copy_n].copy_from_slice(&src_r[..copy_n]);
                pd.aux_in_active[port] = 1;
            }

            NodeOp::ParallelOutTap {
                device_id,
                port,
                dst_track,
            } => {
                // パラアウト (docs/plan_paraout.md): read the source plugin's aux
                // output `port` (`pd.buffer_aux_out[port]`, written by
                // daw_plugin_host during the source plugin's pass-1 process) and
                // **accumulate** it into the destination track's input scratch.
                // The mirror of `SidechainTap` — v29: source plugin は安定
                // device id で直接引く。 RT path: skip silently on any miss.
                let port = *port as usize;
                if port >= common::process_data::MAX_AUX_OUT {
                    continue;
                }
                let Some(entry) = plugin_refs.get(device_id) else {
                    continue;
                };
                if entry.quarantined.load(Ordering::Acquire) {
                    continue;
                }
                let pd = entry.plugin_ref.data();
                // The plugin host marks the port active only when the plugin
                // actually declared (and wrote) this aux output; an unrouted /
                // absent port stays silent (industry-standard behaviour).
                if pd.aux_out_active[port] == 0 {
                    continue;
                }
                let Some(target) = scratch.get_mut(*dst_track as usize) else {
                    continue;
                };
                let copy_n = n.min(target.track_l.len()).min(target.track_r.len());
                for i in 0..copy_n {
                    target.track_l[i] += pd.buffer_aux_out[port][0][i];
                    target.track_r[i] += pd.buffer_aux_out[port][1][i];
                }
            }

            NodeOp::MixSend {
                src,
                dst,
                src_track_idx,
                send_id,
            } => {
                // PR4 aux send: accumulate the source's post- or pre-fader
                // buffer into the destination return / bus scratch, scaled
                // by the live (optionally automated) send gain.
                let BufRef::TrackScratch(dst_idx) = *dst else {
                    continue;
                };
                let (src_idx, pre_fader) = match *src {
                    BufRef::TrackScratch(i) => (i, false),
                    BufRef::PreFaderScratch(i) => (i, true),
                    _ => continue,
                };
                mix_send_into_track_scratch(
                    scratch,
                    dst_idx as usize,
                    src_idx as usize,
                    pre_fader,
                    song,
                    *src_track_idx,
                    *send_id,
                    sample_rate,
                    current_bpm,
                    playhead_beats,
                    any_solo,
                    recording_lanes,
                    n,
                    rows.track_rows(*src_track_idx as usize),
                );
            }

            NodeOp::EnvelopeFollow { src, slot } => {
                // docs/plan_modulation.md §3/§6: advance this source's envelope
                // follower over its (settled) scratch, picking the buffer by
                // tap point. The smoothed envelope lands in
                // `follower_slots[slot].env`; the engine publishes it to
                // `AudioBridge::mod_scalars` after this walk. RT-safe: pure
                // arithmetic, no alloc / lock.
                let Some((src_l, src_r)) = resolve_tap_buffers(scratch, *src) else {
                    continue;
                };
                let Some(fs) = follower_slots.get_mut(*slot as usize) else {
                    continue;
                };
                fs.process_block(src_l, src_r, n);
            }
        }
    }
}

/// Run a Group track's audio fx chain on its already-mixed input
/// scratch, then apply the group's mixer strip (volume / pan / mute /
/// solo + peak meter). Mirrors the audio-fx tail of `process_track_owned`,
/// but skips the sequencer / MIDI FX / instrument stages because groups
/// have no clips of their own.
#[allow(clippy::too_many_arguments)]
fn run_group_fx_chain(
    track_idx: u32,
    song_track: &Track,
    song: &Song,
    scratch: &mut TrackScratch,
    plugin_refs: &PluginRefs,
    worker_sync: Option<&SyncSlot>,
    sample_rate: u32,
    frames: u32,
    playing: bool,
    any_solo: bool,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    current_bpm: f32,
    // group fx の transport snapshot (= 積分済み拍位置 + 実 loop トグル)。
    playhead_beats: f64,
    loop_region: LoopRegion,
    // B3 (r.md #8): group fx PluginParam の follower 変調 snapshot。
    mod_scalars: &[f32],
    // パラアウト (docs/plan_paraout.md): first device index to run. `0` for a
    // pure group / return (whole chain is bus FX). For a group-with-instrument
    // it's the prefix split point — the instrument `[0..start_device]` ran in
    // pass 1, so here we run only the suffix FX `[start_device..]` on the bus.
    start_device: u32,
    // r.md #87: この group の行の供給元 (レーン行の automation に効く)。
    rows: TrackRows<'_>,
) {
    let n = frames as usize;
    let track_id = song_track.id;

    // docs/plan_modulation_followups.md §1: a group's pre-FX signal = the summed
    // children before its own device chain. Capture for any PreFx tap / mod
    // source (guarded — untouched groups skip the memcpy). For a
    // group-with-instrument this is the summed bus *before the suffix FX* (the
    // instrument prefix already ran), which is the right pre-FX tap point.
    if track_needs_prefx_snapshot(song, track_id) {
        scratch.pre_fx_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fx_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    // v23 single-chain: a group / return bus has a summed audio input (no
    // sequencer notes), so the chain runs entirely in the audio domain. Walk
    // `devices` once and connect audio ports serially (Reaper 流) — feed the
    // bus signal into any device that takes audio in, dispatch, then write the
    // audio out back (replace when the device has an audio input = effect, add
    // when it has none = pure source). MIDI ports are irrelevant on a bus.
    // パラアウト: skip the instrument prefix `[0..start_device]` (already run in
    // pass 1) — run only the suffix FX.
    for i in start_device as usize..song_track.devices.len() {
        let device = &song_track.devices[i];
        let ports = device.ports;
        if !ports.has_audio_output {
            // No audio output (e.g. a pure MIDI effect) — nothing to contribute
            // to a bus signal; skip.
            continue;
        }
        // v29: 安定 device id で直接 lookup。
        let Some(entry) = plugin_refs.get(&device.id) else {
            continue;
        };
        let Some(ws) = worker_sync else { continue };
        if !pair_usable(ws, entry) {
            continue;
        }

        let pd = entry.plugin_ref.data_mut();
        pd.prepare();
        pd.frames = frames;
        pd.playing = if playing { 1 } else { 0 };
        pd.sample_rate = sample_rate;
        set_pd_transport(pd, Some(song), current_bpm, playhead_beats, loop_region, rows.track());
        // Phase 2b: group fx 宛 PluginParam automation + B3 (r.md #8) follower 変調。
        crate::automation::fill_pd_param_events(
            pd,
            song,
            track_id,
            rows,
            device.id,
            sample_rate,
            f64::from(current_bpm),
            playhead_beats,
            frames,
            recording_lanes,
            mod_scalars,
        );
        if ports.has_audio_input {
            pd.buffer_in[0][..n].copy_from_slice(&scratch.track_l[..n]);
            pd.buffer_in[1][..n].copy_from_slice(&scratch.track_r[..n]);
        }
        if !dispatch_bounded(ws, entry) {
            continue;
        }
        if ports.has_audio_input {
            // effect: 入力を処理した結果で bus を置換。
            scratch.track_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
            scratch.track_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
        } else {
            // source: 入力を取らず生成する機 → bus に加算。
            for j in 0..n {
                scratch.track_l[j] += pd.buffer_out[0][j];
                scratch.track_r[j] += pd.buffer_out[1][j];
            }
        }
    }

    // ---- Pre-fader send tap (bus / return source) ----
    // A pre-fader send from this bus reads its post-fx, pre-strip signal.
    if song_track
        .sends
        .iter()
        .any(|s| s.mode == common::model::SendMode::PreFader)
    {
        scratch.pre_fader_l[..n].copy_from_slice(&scratch.track_l[..n]);
        scratch.pre_fader_r[..n].copy_from_slice(&scratch.track_r[..n]);
    }

    let muted = song_track.muted;
    let solo = song_track.solo;
    // Live 互換: 子 / send 元のいずれかが solo されていれば、 この bus 自身は
    // solo フラグが無くても透過させる (has_soloed_contributor)。 さらに folder
    // solo: 祖先 group が solo なら、 このネストした group bus 自身も透過させる。
    let effective_mute = muted
        || (any_solo
            && !solo
            && !song.ancestor_soloed(song_track.id)
            && !has_soloed_contributor(song, song_track.id));

    // strip は常に適用 (mirrors process_track_owned) — mute 意味論は
    // `apply_strip` の doc 参照。
    crate::automation::fill_track_param_ramps(
        Some(song),
        track_idx,
        rows,
        sample_rate,
        f64::from(current_bpm),
        playhead_beats,
        frames,
        &mut scratch.volume_per_sample,
        &mut scratch.pan_per_sample,
        recording_lanes,
        // group/master bus の volume/pan follower 変調は follow-up。
        &[],
    );
    apply_strip(scratch, n, muted, effective_mute);
}

/// live (CPAL callback 経由の `LocalState::process_buffer`) と offline export
/// (`export::render_loop`) が共有する「1 buffer を master へ描く」単一経路
/// (`docs/plan_arch_refactor.md` §5):
///
/// 1. master バスをゼロ初期化
/// 2. per-track pass-1 dispatch (worker pool fan-out、無ければ serial)
/// 3. schedule 実行 (group mix / PDC / send / sidechain / follower)
/// 4. master fx chain
/// 5. master gain
///
/// metronome / panic declick 等 **monitoring 専用** の処理はここに入れない
/// (live 側にだけ存在する)。RT-safe: 確保・ロック・I/O なし。
#[allow(clippy::too_many_arguments)]
pub fn render_master_buffer(
    song: &Song,
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
    mod_scalars: &[f32],
    // r.md #87: 行ごとの時間軸の供給元。**live と export は同じ経路** (不変条件 6)
    // なので、両方がここへ同じ形で渡す。空なら全部アレンジ = 従来の挙動。
    rows: &RowSourceTable,
    master_gain: f32,
) {
    let n = (frames as usize).min(master_l.len()).min(master_r.len());
    let frames = n as u32;
    master_l[..n].fill(0.0);
    master_r[..n].fill(0.0);

    let any_solo = song.tracks.iter().any(|t| t.solo);
    let n_tracks = song.tracks.len().min(MAX_TRACKS).min(scratch.len());

    // ---- pass 1: per-track render (worker pool fan-out / serial fallback) --
    let pool = worker.and_then(|rig| rig.pool.as_ref());
    let slots: &[SyncSlot] = worker.map(|rig| rig.slots.as_slice()).unwrap_or(&[]);
    if let Some(pool) = pool {
        pool.dispatch_and_wait(
            Some(song),
            &mut scratch[..n_tracks],
            plugin_refs,
            audio_renderer,
            slots,
            sample_rate,
            frames,
            playing,
            any_solo,
            &schedule.input_delay_per_track,
            recording_lanes,
            current_bpm,
            playhead_beats,
            &loop_region,
            mod_scalars,
            rows,
        );
        // stall した pool は scratch を更新しない (dispatch_and_wait が冒頭で
        // early-return する)。 その残余 (直前 buffer の per-track 出力) を後段の
        // schedule mix が master に積むと、 契約上「無音」であるべき所が耳障りな
        // stuck tone になる (plan §4: stalled = 無音)。 該当 track の scratch を
        // 明示 zero して post-dispatch に無音を流す (RT-safe: pre-alloc buffer への
        // fill のみ、 確保なし)。
        if pool.is_stalled() {
            for ts in scratch[..n_tracks].iter_mut() {
                ts.track_l[..n].fill(0.0);
                ts.track_r[..n].fill(0.0);
            }
        }
    } else {
        let worker_sync = slots.first();
        for (track_idx, track_scratch) in scratch.iter_mut().enumerate().take(n_tracks) {
            let song_track = &song.tracks[track_idx];
            let input_delay = schedule
                .input_delay_per_track
                .get(track_idx)
                .copied()
                .unwrap_or(0);
            process_track_owned(
                track_idx as u32,
                song_track,
                track_scratch,
                plugin_refs,
                Some(audio_renderer),
                worker_sync,
                sample_rate,
                frames,
                playing,
                Some(song),
                any_solo,
                input_delay,
                recording_lanes,
                current_bpm,
                playhead_beats,
                loop_region,
                mod_scalars,
                rows.track_rows(track_idx),
            );
        }
    }

    // ---- pass 2: schedule 実行 (group mix / PDC / send / sidechain) ----
    execute_schedule_post_dispatch(
        schedule,
        scratch,
        &mut master_l[..n],
        &mut master_r[..n],
        n,
        song,
        plugin_refs,
        slots.first(),
        sample_rate,
        frames,
        playing,
        any_solo,
        recording_lanes,
        current_bpm,
        playhead_beats,
        loop_region,
        mod_scalars,
        rows,
    );

    // ---- master fx chain ----
    // 全 track mix 後に直列 process。 live/export 両経路で通るので、 master に
    // 挿した limiter / EQ が WAV にも乗る (旧 export は素通りだった)。
    process_master_fx_chain(
        &song.master_fx_chain,
        &mut master_l[..n],
        &mut master_r[..n],
        plugin_refs,
        slots.first(),
        sample_rate,
        frames,
        playing,
        Some(song),
        current_bpm,
        playhead_beats,
        loop_region,
        recording_lanes,
        mod_scalars,
        rows.master_rows(),
    );

    // ---- master gain ----
    // session の master volume。 live は従来 CPAL interleave 段で掛けていたが、
    // export が素通りだったため単一経路のここへ移動 (§5 live/export 統一)。
    if (master_gain - 1.0).abs() > f32::EPSILON {
        for i in 0..n {
            master_l[i] *= master_gain;
            master_r[i] *= master_gain;
        }
    }
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
    use crate::graph::compile_schedule_for_test;
    use common::model::{PluginInstance, Song, Track};
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
                    t.devices = vec![PluginInstance {
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
                    }];
                }),
            ],
            ..Song::default()
        };
        let mut schedule = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        assert!(schedule.nodes.iter().any(|op| matches!(op, NodeOp::SidechainTap { .. })));

        const FRAMES: usize = 64;
        let mut scratch: Vec<TrackScratch> =
            (0..common::audio_bridge::MAX_TRACKS).map(|_| TrackScratch::new()).collect();
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
            None,
            48_000,
            FRAMES as u32,
            true,
            false,
            &std::collections::HashSet::new(),
            song.bpm,
            0.0,
            LoopRegion::default(),
            &[],
            &RowSourceTable::default(),
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
                    t.devices = vec![PluginInstance {
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
                    }];
                }),
            ],
            ..Song::default()
        };
        let mut schedule = compile_schedule_for_test(&song, 48_000, 0).unwrap();

        const FRAMES: usize = 16;
        let mut scratch: Vec<TrackScratch> =
            (0..common::audio_bridge::MAX_TRACKS).map(|_| TrackScratch::new()).collect();
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
            None,
            48_000,
            FRAMES as u32,
            true,
            false,
            &std::collections::HashSet::new(),
            song.bpm,
            0.0,
            LoopRegion::default(),
            &[],
            &RowSourceTable::default(),
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
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, false, &song, 0, SEND_ID, 48_000, 120.0, 0.0, false, &empty,
            FRAMES,
            crate::launcher::TrackRows::default(),
        );
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
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, false, &song, 0, SEND_ID, 48_000, 120.0, 0.0, false, &empty,
            FRAMES,
            crate::launcher::TrackRows::default(),
        );
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
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, false, &song, 0, SEND_ID, 48_000, 120.0, 0.0, false, &empty,
            FRAMES,
            crate::launcher::TrackRows::default(),
        );
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
            let empty = empty_lanes();
            mix_send_into_track_scratch(
                &mut scratch, 1, 0, false, &song, 0, SEND_ID, 48_000, 120.0, 0.0, true, &empty,
                FRAMES,
                crate::launcher::TrackRows::default(),
            );
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
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, true, &song, 0, SEND_ID, 48_000, 120.0, 0.0, false, &empty,
            FRAMES,
            crate::launcher::TrackRows::default(),
        );
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
        let empty = empty_lanes();
        mix_send_into_track_scratch(
            &mut scratch, 1, 0, false, &song, 0, 999, 48_000, 120.0, 0.0, false, &empty, FRAMES,
            crate::launcher::TrackRows::default(),
        );
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
        song.tracks[0].solo = true; // solo the send SOURCE (Vocal)
        assert!(
            has_soloed_contributor(&song, 2),
            "Reverb return must be solo-safe when its send source is soloed"
        );
        // Nothing soloed → the return has no soloed contributor.
        song.tracks[0].solo = false;
        assert!(
            !has_soloed_contributor(&song, 2),
            "with nothing soloed, the return has no soloed contributor"
        );
    }

    /// Folder solo: soloing a GROUP must keep its children audible (Ableton /
    /// Reaper folder behavior). The leaf strip rule excludes a non-soloed
    /// track under solo only when no ancestor group is soloed, so a child of
    /// a soloed group is NOT effective-muted. Guards the `ancestor_soloed`
    /// condition added to the effective-mute formula.
    #[test]
    fn soloed_group_keeps_children_audible() {
        // id 10 = group, id 11 = child of 10, id 12 = unrelated.
        let song = Song {
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

        let any_solo = song.tracks.iter().any(|t| t.solo);
        assert!(any_solo);
        // child: not soloed itself, but its ancestor group is → audible.
        assert!(song.ancestor_soloed(11), "child sees the soloed ancestor group");
        let child = &song.tracks[1];
        let child_excluded = any_solo && !child.solo && !song.ancestor_soloed(child.id);
        assert!(!child_excluded, "child of a soloed group must not be solo-excluded");
        // unrelated track: no soloed ancestor → excluded (silent) under solo.
        let other = &song.tracks[2];
        let other_excluded = any_solo && !other.solo && !song.ancestor_soloed(other.id);
        assert!(other_excluded, "unrelated track is silenced while a group is soloed");
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
        let mut scratch: Vec<TrackScratch> = (0..MAX_TRACKS).map(|_| TrackScratch::new()).collect();
        let mut master_l = vec![7.0f32; 64]; // 前 buffer の残骸 — clear されるべき
        let mut master_r = vec![7.0f32; 64];
        let plugin_refs: PluginRefs = HashMap::new();
        let renderer = crate::audio_clip_renderer::AudioClipRenderer::empty();
        render_master_buffer(
            &song,
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
            &[],
            &RowSourceTable::default(),
            0.5,
        );
        assert!(master_l.iter().all(|&v| v == 0.0), "master must be cleared+silent");
        assert!(master_r.iter().all(|&v| v == 0.0));
    }
}
