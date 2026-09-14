//! Audio-side automation playback. Walks the song's automation lanes,
//! evaluates curves at sample resolution, and fills the per-track
//! `volume_per_sample` / `pan_per_sample` ramps for the buffer.
//!
//! Phase 1: track-builtin Volume / Pan only. Plugin parameter
//! automation generates `clap_event_param_value` events through the
//! plugin host and is wired in Phase 2 (`docs/plan_automation.md`).
//!
//! RT-safe: no allocation, no I/O, no locking. Reads `Song` through a
//! shared reference (`ArcSwap<Song>` already loaded by the caller).

#![allow(dead_code)]

use common::automation::apply_modulation_over;
use common::mod_plane::ModTickPlaneRef;
use common::model::{AutomationTarget, MasterLimiterSettings, NativeDevice, Song, TrackBuiltinParam};
use common::process_data::ProcessData;
use common::song_index::{ParamStore, ParamSubject, SongIndex, TargetRoutings};

use crate::engine_shared::RecordingLanes;
use crate::launcher::TrackRows;
use crate::launcher::render::{lane_value, phase_at_frame};

/// `owner` の置き場の `target` を録音中か (録音中はカーブを評価せずノブの値を素通しする)。
fn is_recording(recording_lanes: &RecordingLanes, owner: u32, target: &AutomationTarget) -> bool {
    // `AutomationTarget` の clone は確保しない (中身は数値だけ)。
    !recording_lanes.is_empty() && recording_lanes.contains(&(owner, target.clone()))
}

/// Fill `volume_per_sample` / `pan_per_sample` (each at least `frames`
/// long, but typically `MAX_FRAMES`) for the given track and buffer.
///
/// Default fill: each sample gets `track.volume` / `track.pan` as a
/// constant. Each enabled `Volume` / `Pan` lane then overwrites its
/// target buffer with the curve value sampled at every sample position.
///
/// `bpm == 0` or `sample_rate == 0` short-circuits to the constant
/// fallback (defensive — the engine starts in this state during init).
///
/// Lanes targeting plugin parameters are silently skipped: those are
/// converted into `TimedParamEvent`s elsewhere (Phase 2).
///
/// Phase 4 Step C-2: `recording_lanes` に `(track_id, lane.target)` が含まれて
/// いる lane は curve eval を **skip** し、 track.volume / track.pan の constant
/// fallback がそのまま buffer に残る。 これで GUI 側の knob 操作 (=
/// SetTrackVolume / SetTrackPan IPC) が即時に audio に反映される (Live /
/// Bitwig の Touch / Latch / Write の "you hear what you do" UX)。 set は
/// audio thread が buffer 頭で `load()` する snapshot (ArcSwap、 lock-free)。
#[allow(clippy::too_many_arguments)]
pub fn fill_track_param_ramps(
    song: Option<&Song>,
    // `song` と同じ snapshot の索引 (`RtBundle::song_index`)。
    index: &SongIndex,
    track_idx: u32,
    // r.md #87: この track の行 (トラック行 + レーン行) の供給元。レーン行が
    // ランチャー主導ならそのセルのカーブを、停止していれば `lane.default_value` を
    // 出す (Q11)。空 (`TrackRows::default()`) なら全部アレンジ = 従来の挙動。
    rows: TrackRows<'_>,
    sample_rate: u32,
    // M5 (r.md #8 再監査): automation lookup beat は transport の積分済
    // `playhead_beats` を anchor に、 buffer 内は `current_bpm` で advance する
    // (SongTempo automation を尊重、 A10 と同経路)。 定 tempo では従来の
    // `(playhead + i)/samples_per_beat` と bit 同一。
    current_bpm: f64,
    playhead_beats: f64,
    frames: u32,
    volume_per_sample: &mut [f32],
    pan_per_sample: &mut [f32],
    recording_lanes: &RecordingLanes,
    // docs/plan_modulation.md §5 / r.md #89: **刻みごとの**変調値面
    // (`ModSource::id` キー)。volume/pan lane の `mod_routings` がこれを引く。
    // 刻みの間は線形補間する — 64 サンプルの段 (48kHz で 750Hz) をそのまま
    // 音量に当てると段差が音として出る。空 = 変調なし。
    mod_plane: ModTickPlaneRef<'_>,
) {
    let Some((song, track)) = song.and_then(|s| Some((s, s.tracks.get(track_idx as usize)?))) else {
        let n = (frames as usize).min(volume_per_sample.len()).min(pan_per_sample.len());
        volume_per_sample[..n].fill(1.0);
        pan_per_sample[..n].fill(0.0);
        return;
    };
    let store = index.track_store(song, track_idx as usize);
    fill_target_ramp(
        song,
        track.id,
        store,
        rows,
        sample_rate,
        current_bpm,
        playhead_beats,
        frames,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
        track.volume,
        volume_per_sample,
        recording_lanes,
        mod_plane,
    );
    fill_target_ramp(
        song,
        track.id,
        store,
        rows,
        sample_rate,
        current_bpm,
        playhead_beats,
        frames,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan),
        track.pan,
        pan_per_sample,
        recording_lanes,
        mod_plane,
    );
}

/// 1 本の builtin target (Volume / Pan / ChainGain / ChainPan) の per-sample ramp を
/// `buf` に埋める。
///
/// docs/plan_modulation_routing_redesign.md §3.1: lane の有無に関わらず変調する。
/// base = 「enabled かつ非 recording な lane があればその curve 値、無ければ
/// `constant`」、そこに `mod_routings` の当該 target 変調を正規化領域で乗せる。
/// lane も mod_routing も無い target は constant fill のままで正しいので per-sample
/// ループを丸ごと skip (= 無回帰)。
///
/// `owner_track_id` / `store` は所有者の置き場 (track なら `Track.automation_lanes` / `mod_routings`、
/// master 所有の Parallel chain なら `Song.song_lanes` / `song_mod_routings` + `MASTER_TRACK_ID`)。
/// lane と routing は置き場の索引で引く (置き場の全件を舐めない)。
/// RT 安全: 確保・ロックなし。
#[allow(clippy::too_many_arguments)]
pub fn fill_target_ramp(
    song: &Song,
    owner_track_id: u32,
    store: ParamStore<'_>,
    rows: TrackRows<'_>,
    sample_rate: u32,
    current_bpm: f64,
    playhead_beats: f64,
    frames: u32,
    target: AutomationTarget,
    constant: f32,
    buf: &mut [f32],
    recording_lanes: &RecordingLanes,
    mod_plane: ModTickPlaneRef<'_>,
) {
    let frames = (frames as usize).min(buf.len());
    buf[..frames].fill(constant);
    if frames == 0 || current_bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let beats_per_frame = current_bpm / (60.0 * f64::from(sample_rate));
    if beats_per_frame <= 0.0 {
        return;
    }
    // 当該 target を駆動する lane (enabled + 非 recording)。
    let lane = store.enabled_lane(&target).filter(|_| !is_recording(recording_lanes, owner_track_id, &target));
    let routings = store.routings_for(&target);
    if lane.is_none() && routings.is_empty() {
        return;
    }
    // r.md #87: このレーン行の供給元。`switch_frame` を跨ぐと途中で変わる。
    let src = lane.map(|v| rows.lane(v.pos)).unwrap_or_default();
    for (i, slot) in buf.iter_mut().enumerate().take(frames) {
        let beat = playhead_beats + i as f64 * beats_per_frame;
        let base = match lane {
            #[allow(clippy::cast_possible_truncation)]
            Some(view) => {
                let phase = phase_at_frame(src, i as u32);
                lane_value(view, &song.clip_contents, phase, beat)
            }
            None => f64::from(constant),
        };
        #[allow(clippy::cast_possible_truncation)]
        let f = i as u32;
        // r.md #89 Q9: 深さ自体が動く変調も刻みごとに解決する。
        *slot = apply_modulation_over(
            &target,
            base,
            routings.iter(),
            |id| mod_plane.scalar_at_frame_opt(id, f),
            |r| mod_plane.depth_at_frame(r.id, f).unwrap_or(r.depth),
        ) as f32;
    }
}

/// この buffer で実際に効く **内蔵 device の値**を解決する (r.md #129 §7.3)。
///
/// 出発点は `device` の静的値。そこへ (1) 有効かつ録音中でないオートメーションレーンの
/// カーブ値、(2) 同じ target の変調 を順に重ねる。住所 ↔ フィールドの対応は
/// `NativeDevice::{param, set_param}` が SSoT (`On` は `bypassed`、段階式は `set` が段へ丸める)。
///
/// `owner` はその device を持つトラック (master fx chain なら `MASTER_TRACK_ID`) で、`store` はその置き場、
/// `rows` は owner の行。**block-rate (buffer 先頭で 1 回)**。lane / routing は置き場の索引で device の分だけ引く。
///
/// RT 安全: 確保・ロックなし (`NativeDevice` は `Copy`)。
#[allow(clippy::too_many_arguments)]
pub fn resolve_native_device(
    clip_contents: &std::collections::HashMap<common::model::ContentId, common::model::ClipContent>,
    store: ParamStore<'_>,
    device: &NativeDevice,
    owner: u32,
    rows: TrackRows<'_>,
    playhead_beats: f64,
    recording_lanes: &RecordingLanes,
    mod_plane: ModTickPlaneRef<'_>,
) -> NativeDevice {
    let mut out = *device;
    let subject = ParamSubject::Native(device.id);
    // (1) レーン: 有効かつ録音中でないものだけがカーブ値で上書きする
    // (録音中は GUI のノブ操作を素通しさせる = fill_track_param_ramps と同じ規則)。
    for view in store.lanes_of(subject) {
        let lane = view.lane;
        let AutomationTarget::NativeParam { param, .. } = lane.target else {
            continue;
        };
        if !lane.enabled || out.param(param).is_none() || is_recording(recording_lanes, owner, &lane.target) {
            continue;
        }
        let phase = phase_at_frame(rows.lane(view.pos), 0);
        let v = lane_value(view, clip_contents, phase, playhead_beats);
        #[allow(clippy::cast_possible_truncation)]
        out.set_param(param, v as f32);
    }
    // (2) 変調: base は (1) まで解決済みの現在値。target ごとに、その target を指す routing を畳む。
    for (target, routings) in store.targets_of(subject) {
        let AutomationTarget::NativeParam { param, .. } = *target else {
            continue;
        };
        let Some(base) = out.param(param) else {
            continue;
        };
        #[allow(clippy::cast_possible_truncation)]
        out.set_param(param, modulated_at_block_start(target, f64::from(base), routings, mod_plane) as f32);
    }
    out
}

/// block-rate (buffer 頭の値面) で `base` に変調を乗せる。
fn modulated_at_block_start(
    target: &AutomationTarget,
    base: f64,
    routings: TargetRoutings<'_>,
    mod_plane: ModTickPlaneRef<'_>,
) -> f64 {
    apply_modulation_over(
        target,
        base,
        routings.iter(),
        |id| mod_plane.scalar_at_frame_opt(id, 0),
        |r| mod_plane.depth_at_frame(r.id, 0).unwrap_or(r.depth),
    )
}

/// この buffer で実際に効く **master のフェーダー後 Limiter** を解決する (r.md #129 §7.3)。
/// `store` は song 側の置き場 (`song_lanes` / `song_mod_routings`)、`rows` は master の行。
///
/// RT 安全: 確保・ロックなし (`MasterLimiterSettings` は `Copy`)。
pub fn resolve_master_limiter(
    song: &Song,
    store: ParamStore<'_>,
    rows: TrackRows<'_>,
    playhead_beats: f64,
    recording_lanes: &RecordingLanes,
    mod_plane: ModTickPlaneRef<'_>,
) -> MasterLimiterSettings {
    let mut out = song.master_limiter;
    let master = common::model::MASTER_TRACK_ID;
    for view in store.lanes_of(ParamSubject::MasterLimiter) {
        let lane = view.lane;
        let AutomationTarget::MasterLimiter(param) = lane.target else {
            continue;
        };
        if !lane.enabled || is_recording(recording_lanes, master, &lane.target) {
            continue;
        }
        let phase = phase_at_frame(rows.lane(view.pos), 0);
        let v = lane_value(view, &song.clip_contents, phase, playhead_beats);
        #[allow(clippy::cast_possible_truncation)]
        out.set_param(param, v as f32);
    }
    for (target, routings) in store.targets_of(ParamSubject::MasterLimiter) {
        let AutomationTarget::MasterLimiter(param) = *target else {
            continue;
        };
        let v = modulated_at_block_start(target, f64::from(out.param(param)), routings, mod_plane);
        #[allow(clippy::cast_possible_truncation)]
        out.set_param(param, v as f32);
    }
    out
}

/// Phase 2b (`docs/plan_automation.md` §8.3): push automation events for
/// the specified device (v29: 安定 device id `PluginInstance::id` で指定)
/// into `pd.events_in` as `EventKind::ParamValue` entries.
/// plugin_host's `process_server` decodes them into
/// `TimedParamEvent` and forwards to `LoadedPlugin::process(..,
/// param_events, ..)` which converts them to CLAP `clap_event_param_value`
/// / VST3 `IParameterChanges`.
///
/// Phase 2 では「1 buffer = 1 update」 (frame 0 でのみ curve 値を 1 度
/// push) として簡素実装。 frame 単位 sample (= 64 frame 刻みで複数
/// push) は Phase 3+ でカーブの滑らかさが必要になったときに拡張。
///
/// RT 安全性: `push_param` は固定 capacity の `events_in` 配列に書く
/// だけ、 allocation なし。 `lane_value_at` は curve evaluator のみ
/// (allocation なし、 浮動小数演算のみ)。
#[allow(clippy::too_many_arguments)]
pub fn fill_pd_param_events(
    pd: &mut ProcessData,
    song: &Song,
    track_id: u32,
    // `track_id` の置き場 (master fx は song 側)。呼び出し側が program 実行ごとに 1 回解決したもの
    // (plugin ごとに track を id で探さない)。lane / routing は索引で device の分だけ引く。
    store: ParamStore<'_>,
    // r.md #87: この track の行の供給元 (master fx は行を持たないので
    // `TrackRows::default()` = 全部アレンジ)。
    rows: TrackRows<'_>,
    device_id: u64,
    sample_rate: u32,
    // M5 (r.md #8 再監査): transport の積分済 `playhead_beats` を anchor に
    // buffer 内は `current_bpm` で advance (SongTempo automation 尊重、 A10 同経路)。
    // track/group/master 全経路で同じ引数を渡すので、 旧実装の「master=current_bpm /
    // track・group=song.bpm」 の不一致も解消。 定 tempo では従来式と bit 同一。
    current_bpm: f64,
    playhead_beats: f64,
    frames: u32,
    recording_lanes: &RecordingLanes,
    // docs/plan_modulation.md §5 / r.md #89: **刻みごとの**変調値面 (id キー)。
    // PluginParam lane の `mod_routings` がこれを引く。空 = 変調なし。
    mod_plane: ModTickPlaneRef<'_>,
    // r.md #117: この plugin の鳴っているノート (per-note 変調の評価点)。 `None` = ノート無し。
    voices: Option<&crate::graph::voices::VoiceTable>,
) {
    if frames == 0 || current_bpm <= 0.0 || sample_rate == 0 {
        return;
    }
    let beats_per_frame = current_bpm / (60.0 * f64::from(sample_rate));
    if beats_per_frame <= 0.0 {
        return;
    }
    let subject = ParamSubject::Plugin(device_id);
    for view in store.lanes_of(subject) {
        let lane = view.lane;
        let AutomationTarget::PluginParam { param_id, .. } = lane.target else {
            continue;
        };
        // Phase 4 Step C-2 (plugin param 版): recording 中 (Touch/Latch/Write)
        // の lane は curve eval を skip する。 これで plugin が自身の GUI で
        // 持っている値 (= ユーザのノブ操作) を host が curve で毎バッファ
        // 上書きするのを止め、「you hear what you do」 を成立させる。 track
        // builtin Volume/Pan の `fill_track_param_ramps` と同じ仕組みだが、
        // 旧実装は plugin param 側にこの skip が無く、 write が read のまま /
        // touch が半分しか効かないバグだった。
        if !lane.enabled || is_recording(recording_lanes, track_id, &lane.target) {
            continue;
        }
        // automation curve 値 (絶対値)。モジュレーションは下で正規化オフセットを
        // ParamMod として別送する (`docs/plan_modulation_routing_redesign.md` §3.2)
        // ので、CLAP modulatable param では automation を破壊せず非破壊に乗る。
        //
        // B4 (r.md #8): sub-buffer (64 frame 刻み) で curve をサンプルし、 値が変わる
        // たびに frame offset 付きで push_param する (= sample-accurate)。 旧実装は
        // frame 0 の 1 回のみで、 速い automation が階段状 (zipper) になっていた。
        // 静的セグメントは値不変なので 1 event に縮退 (events_in=256 を無駄に食わない)。
        // push_param は満杯時 drop のみ (panic なし) なので RT 安全。
        //
        // r.md #89: 刻み幅の SSoT は `crate::mod_tick::MOD_TICK_FRAMES`。automation の
        // サブバッファ刻みと変調の制御グリッドは **同じ格子でなければならない**
        // (設計正本 §2.2) — 別々の 64 を持つと、片方を変えたときに黙って食い違う。
        const SUB_FRAMES: u32 = crate::mod_tick::MOD_TICK_FRAMES;
        // r.md #87: このレーン行の供給元 (ランチャー主導ならセルのカーブ)。
        let src = rows.lane(view.pos);
        let mut f = 0u32;
        let mut last_v = f64::NAN;
        loop {
            let beat_at_f = playhead_beats + f64::from(f) * beats_per_frame;
            let v = lane_value(view, &song.clip_contents, phase_at_frame(src, f), beat_at_f);
            if last_v.is_nan() || (v - last_v).abs() > 1e-6 {
                pd.push_param(f, param_id, v);
                last_v = v;
            }
            if f + SUB_FRAMES >= frames {
                break;
            }
            f += SUB_FRAMES;
        }
    }

    // docs/plan_modulation_routing_redesign.md §3.2: この device の plugin param を
    // 変調する routing があれば、target ごとに正規化オフセット 1 個を `ParamMod` で
    // 送る。**lane の有無に関わらず** (= lane-free モジュレーション)。plugin_host が
    // per-format に CLAP `param_mod` / 合成へ変換する。follower が 0 に戻った時も
    // offset 0 を送って mod を解除するため、毎バッファ無条件に emit する。
    // r.md #89: **溢れさせない**。`param_mods` はリングなので、満杯だと古い側が
    // 落ちる。積む順は param ごと (外) × 刻み (内) なので、落ちるのは「先頭 param の
    // 全刻み」= その param に 1 件も届かず、前 buffer の offset が解除されないまま
    // 居座る (毎 buffer 同じ順で溢れるので永久に戻らない)。件数が枠を超えるときは
    // **刻みを間引いて解像度を落とす** — どの param にも必ず最後の刻みが届く。
    let n_ticks = mod_plane.starts(frames).count().max(1);
    let n_params = store.targets_of(subject).count().max(1);
    // r.md #117: per-note のストリーム (per-note 対象 param × ボイス) も **同じ枠**を食うので、
    // 予算はストリーム数の合計で割る。 別枠で数えると per-note が global を押し出し、 先頭
    // param の global offset が解除されずに居座る (上の r.md #89 と同じ事故)。
    let n_pn_params = voices.map_or(0, |vt| {
        usize::from(!vt.is_empty()) * store.targets_of(subject).filter(|(_, rs)| has_per_note(mod_plane, *rs)).count()
    });
    let n_streams = n_params + n_pn_params * voices.map_or(0, |vt| vt.len());
    // 1 ストリームあたりの件数。全ストリームが収まるので、積む順 (param の並び) で何かが落ちることは無い
    // (ストリーム数が枠そのものを超える退化だけは、最後の刻み 1 件ずつでも溢れる)。
    let budget = (common::process_data::MAX_PARAM_MODS / n_streams.max(1)).max(1);
    // 同一 target は 1 度だけ (索引が target ごとに束ねてある)。
    for (target, routings) in store.targets_of(subject) {
        let AutomationTarget::PluginParam { param_id, .. } = *target else {
            continue;
        };
        // r.md #89: **刻みごとに** frame offset 付きで送る。1 buffer 1 発だと
        // 変調の解像度が buffer 長 (≒46Hz) に落ち、Hz 指定の LFO が原理的に
        // 鳴らないうえ live (device buffer 長) と書き出し (1024 固定) で段差の
        // 位置が違う。値が変わらない刻みは縮退させる (automation 側の
        // `push_param` と同じ idiom — `param_mods` の枠を無駄に食わない)。
        let mut last = f64::NAN;
        for (t, f) in mod_plane.starts(frames).enumerate() {
            if !sends_tick(t, n_ticks, budget) {
                continue;
            }
            // r.md #89 Q9: 深さ自体が動く変調は面が持つ実効値を使う。
            let offset = f64::from(common::automation::modulation_offset_norm_over(
                routings.iter(),
                |id| mod_plane.scalar_at_frame_opt(id, f),
                |rr| mod_plane.depth_at_frame(rr.id, f).unwrap_or(rr.depth),
            ));
            if last.is_nan() || (offset - last).abs() > 1e-6 {
                pd.push_param_mod(f, param_id, offset);
                last = offset;
            }
        }
    }
    if let Some(voices) = voices {
        push_per_note_param_mods(
            pd, store, device_id, sample_rate, current_bpm, playhead_beats, frames, mod_plane, voices, budget, n_ticks,
        );
    }
}

/// 1 ストリームを `budget` 件以内に収めるとき、`n_ticks` 本のうち刻み `t` を送るか。
///
/// 間引くのは中間の刻みだけで、**最後の刻みは必ず送る** (解除されない offset が居座らないように)。その 1 件を
/// 含めて `budget` 件に収める — 最後の刻みを勘定の外に置くと 1 ストリームが `budget + 1` 件になり、全ストリームの
/// 合計がリングの枠を超えて古い側 (先頭 param の刻み) が落ちる。枠に収まる間 (`budget >= n_ticks`) は全部送る。
fn sends_tick(t: usize, n_ticks: usize, budget: usize) -> bool {
    if t + 1 >= n_ticks {
        return true;
    }
    if budget <= 1 {
        return false;
    }
    t.is_multiple_of((n_ticks - 1).div_ceil(budget - 1).max(1))
}

/// r.md #117: この routing が **per-note 経路で評価される** か (source が有効な `Note` 起点)。source は値面の
/// 列 (= 評価計画の slot) から引く — 計画に載るのは有効な source だけ (`Song::mod_sources` を id で線形に探さない)。
fn is_per_note_routing(mod_plane: ModTickPlaneRef<'_>, r: &common::model::ModRouting) -> bool {
    r.enabled && mod_plane.source_node(r.source_id).is_some_and(|n| n.kind.is_per_note())
}

/// r.md #117: この target を指す routing に per-note 経路のものが 1 本でもあるか
/// (= この param は per-note ストリームを持つ)。
fn has_per_note(mod_plane: ModTickPlaneRef<'_>, routings: TargetRoutings<'_>) -> bool {
    routings.iter().any(|r| is_per_note_routing(mod_plane, r))
}

/// r.md #117: 1 ボイス・1 刻みの正規化オフセット = **その param 宛の全 routing の和**。 `Note`
/// 起点の source はそのボイスの時刻 `time` で閉形式評価、 それ以外は面の値 (深さが動く変調は
/// 面の実効値 — global と同じ)。
fn per_note_offset_at(
    routings: TargetRoutings<'_>,
    mod_plane: ModTickPlaneRef<'_>,
    f: u32,
    time: common::modulators::ModTime,
) -> f64 {
    use common::modulators::generator_scalar;
    f64::from(common::automation::modulation_offset_norm_over(
        routings.iter(),
        |sid| match mod_plane.source_node(sid) {
            Some(n) if n.kind.is_per_note() => Some(generator_scalar(&n.kind, time).unwrap_or(0.0).clamp(0.0, 1.0)),
            _ => mod_plane.scalar_at_frame_opt(sid, f),
        },
        |rr| mod_plane.depth_at_frame(rr.id, f).unwrap_or(rr.depth),
    ))
}

/// r.md #117 (`docs/plan_per_note_modulation.md` §3): per-note routing が刺さっている param に
/// ついて、 **ボイス × 刻み** で `ParamMod { note_id, key }` を積む。
///
/// 値は **その param 宛の全 routing の和** (global と同じ `modulation_offset_norm_with`): `Note`
/// 起点の source はそのボイスの時刻で閉形式評価 (`generator_scalar` に note-on の拍 / 秒と
/// note-off の秒。 config の値で評価 = 位相だけノート起点)、 それ以外の source は面の値。
/// host は「per-note が 1 件でもある param の global を捨てる」 ので、 ここに global 側の寄与
/// (別 routing の LFO 等) も畳んでおかないと、 ボイスが鳴っている間だけその変調が消える。
/// 同じ param の per-note routing が複数あっても 1 event (和) になる (CLAP `param_mod` は
/// 絶対値で後勝ちなので、 別々に送ると加算されない)。
///
/// `budget` / `n_ticks` は global と共通の予算 (ストリーム数で割った 1 ストリームの件数、[`sends_tick`])。
#[allow(clippy::too_many_arguments)]
fn push_per_note_param_mods(
    pd: &mut ProcessData,
    store: ParamStore<'_>,
    device_id: u64,
    sample_rate: u32,
    current_bpm: f64,
    playhead_beats: f64,
    frames: u32,
    mod_plane: ModTickPlaneRef<'_>,
    voices: &crate::graph::voices::VoiceTable,
    budget: usize,
    n_ticks: usize,
) {
    use common::modulators::ModTime;
    let sr = f64::from(sample_rate.max(1));
    let secs0 = mod_plane.first_sample() as f64 / sr;
    let beats_per_frame = current_bpm / (60.0 * sr);
    // 同一 target は 1 度だけ (索引が target ごとに束ねてある)。
    for (target, routings) in store.targets_of(ParamSubject::Plugin(device_id)) {
        let AutomationTarget::PluginParam { param_id, .. } = *target else {
            continue;
        };
        if !has_per_note(mod_plane, routings) {
            continue;
        }
        for v in voices.iter() {
            // host は note id を `i32` で運ぶ (`-1` = 未指定)。 収まらない id (鍵盤プレビューの
            // `PREVIEW_NOTE_ID`) は note event 側も `-1` で届くので、 per-note では当てられない。
            let Ok(note_id) = i32::try_from(v.note_id) else {
                continue;
            };
            let mut last = f64::NAN;
            for (t, f) in mod_plane.starts(frames).enumerate() {
                if !sends_tick(t, n_ticks, budget) {
                    continue;
                }
                let ft = f64::from(f);
                let time = ModTime::at_note(
                    playhead_beats + ft * beats_per_frame,
                    secs0 + ft / sr,
                    v.on_beat,
                    v.on_secs,
                    v.off_secs,
                );
                let offset = per_note_offset_at(routings, mod_plane, f, time);
                if last.is_nan() || (offset - last).abs() > 1e-6 {
                    pd.push_param_mod_note(f, param_id, offset, note_id, v.key, v.channel);
                    last = offset;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の薄いラッパ: 全行アレンジ (= ランチャー無し) で従来の経路を通す。
    /// ランチャー行の評価は `crate::launcher::render` 側でテストする。
    #[allow(clippy::too_many_arguments)]
    fn ramps(
        song: Option<&Song>,
        track_idx: u32,
        sample_rate: u32,
        current_bpm: f64,
        playhead_beats: f64,
        frames: u32,
        volume_per_sample: &mut [f32],
        pan_per_sample: &mut [f32],
        recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
        mod_scalars: &[f32],
    ) {
        let ids: Vec<u32> = song
            .map(|s| s.mod_sources.iter().map(|m| m.id).collect())
            .unwrap_or_default();
        let mod_plane = ModTickPlaneRef::new(&ids, mod_scalars, 64);
        let index = song.map_or_else(SongIndex::default, SongIndex::build);
        fill_track_param_ramps(
            song,
            &index,
            track_idx,
            TrackRows::default(),
            sample_rate,
            current_bpm,
            playhead_beats,
            frames,
            volume_per_sample,
            pan_per_sample,
            recording_lanes,
            mod_plane,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn pd_params(
        pd: &mut ProcessData,
        song: &Song,
        track_id: u32,
        device_id: u64,
        sample_rate: u32,
        current_bpm: f64,
        playhead_beats: f64,
        frames: u32,
        recording_lanes: &std::collections::HashSet<(u32, AutomationTarget)>,
        mod_scalars: &[f32],
    ) {
        let ids: Vec<u32> = song.mod_sources.iter().map(|m| m.id).collect();
        let mod_plane = ModTickPlaneRef::new(&ids, mod_scalars, 64);
        let index = SongIndex::build(song);
        fill_pd_param_events(
            pd,
            song,
            track_id,
            owner_store(song, &index, track_id),
            TrackRows::default(),
            device_id,
            sample_rate,
            current_bpm,
            playhead_beats,
            frames,
            recording_lanes,
            mod_plane,
            None,
        );
    }

    /// `owner` (track id か `MASTER_TRACK_ID`) の置き場 (本番は program 実行ごとに解いたものを渡す)。
    fn owner_store<'a>(song: &'a Song, index: &'a SongIndex, owner: u32) -> ParamStore<'a> {
        index.store(song, song.param_store_at(owner).expect("owner store"))
    }

    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane,
        AutomationPoint, ClipContent, Song, Track,
    };

    /// Helper: build a song with one track owning a single automation
    /// lane that ramps `Volume` from 0.0 → 1.0 across beats 0..4.
    fn one_volume_lane_song() -> Song {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.0,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 4.0,
                        value: 1.0,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 3,
            }),
        );
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "vol".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
                color: None,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.5,
            )
        };
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.volume = 0.5;
            t.automation_lanes = vec![lane];
            t.next_lane_id = 2;
        }));
        song
    }

    /// 120 BPM at 48 kHz → 24000 samples/beat.
    const SR: u32 = 48000;

    /// Phase 4 Step C-2: 既存テストは recording 中ではないので、 共通 helper
    /// で empty set を borrow する。
    fn empty_recording_lanes()
    -> std::collections::HashSet<(u32, common::model::AutomationTarget)> {
        std::collections::HashSet::new()
    }

    /// v23 single-chain: `Track` の `legacy_*` migration fields は `common`
    /// crate に `pub(crate)` で閉じているため、 downstream crate (daw_audio)
    /// の test では `Track { .., ..Track::default() }` の functional-update が
    /// E0451 になる。 `Track::default()` から組んで mutator で埋める helper で
    /// 回避する (private field に触れずに済む)。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    #[test]
    fn no_song_falls_back_to_unity_volume_zero_pan() {
        let mut vol = vec![0.0_f32; 8];
        let mut pan = vec![0.5_f32; 8];
        let empty = empty_recording_lanes();
        ramps(None, 0, SR, 120.0, 0.0, 8, &mut vol, &mut pan, &empty, &[]);
        assert!(vol.iter().all(|&v| (v - 1.0).abs() < 1e-6));
        assert!(pan.iter().all(|&p| p.abs() < 1e-6));
    }

    #[test]
    fn no_lanes_fills_with_track_strip_constants() {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.volume = 0.7;
            t.pan = -0.25;
        }));
        let mut vol = vec![0.0_f32; 16];
        let mut pan = vec![0.0_f32; 16];
        let empty = empty_recording_lanes();
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0.0,
            16,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        assert!(vol.iter().all(|&v| (v - 0.7).abs() < 1e-6));
        assert!(pan.iter().all(|&p| (p - -0.25).abs() < 1e-6));
    }

    #[test]
    fn volume_lane_ramps_across_buffer() {
        let song = one_volume_lane_song();
        let mut vol = vec![0.0_f32; 16];
        let mut pan = vec![0.0_f32; 16];
        let empty = empty_recording_lanes();
        // Buffer of 16 samples starting at playhead 0 → first 16 samples
        // out of 24000 samples-per-beat × 4 = 96000 total. Volume should
        // ramp from 0.0 toward ~16/96000 ≈ 0.000167.
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0.0,
            16,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        assert!(vol[0].abs() < 1e-6);
        assert!(vol[15] > 0.0 && vol[15] < 0.001, "vol[15]={}", vol[15]);
        // Pan untouched (no Pan lane) → falls back to track.pan = 0.
        assert!(pan.iter().all(|&p| p.abs() < 1e-6));
    }

    #[test]
    fn buffer_at_clip_midpoint_returns_half() {
        let song = one_volume_lane_song();
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        // Beat 2.0 = 48000 samples in. Curve linear 0→1 over beats 0..4
        // → value 0.5 at beat 2.
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            2.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 0.001, "expected ~0.5, got {}", v);
        }
    }

    #[test]
    fn disabled_lane_uses_default_value() {
        let mut song = one_volume_lane_song();
        song.tracks[0].automation_lanes[0].enabled = false;
        // default_value is 0.5 from one_volume_lane_song.
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            0.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        // Bypass returns the lane's default — but `fill_track_param_ramps`
        // only writes the lane buffer when `enabled = true`. With the
        // lane disabled the constant fallback (track.volume = 0.5)
        // remains, which happens to equal the default in this test.
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn outside_clip_range_uses_default_value() {
        let song = one_volume_lane_song();
        // Buffer at beat 10 (sample 240_000) — clip ends at beat 4.
        // lane_value_at returns lane.default_value = 0.5.
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            10.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 1e-6, "got {}", v);
        }
    }

    /// Phase 4 Step C-2: `recording_lanes` に含まれる lane は curve eval を
    /// skip し、 track.volume の constant が残るべき (= live knob 値で audio
    /// が鳴る挙動の基盤)。 track.volume を 0.9、 curve は beat 2.0 で 0.5 に
    /// なる構成にし、 bypass で vol[i] が 0.9 になることを確認する。
    #[test]
    fn recording_lane_bypasses_curve_eval() {
        let mut song = one_volume_lane_song();
        song.tracks[0].volume = 0.9; // 区別のため curve mid (0.5) と違う値に
        let track_id = song.tracks[0].id;
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let mut recording = std::collections::HashSet::new();
        recording.insert((
            track_id,
            common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Volume,
            ),
        ));
        // Beat 2.0 の curve eval は 0.5 だが、 recording bypass で track.volume
        // (= 0.9) がそのまま残る。
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            2.0,
            4,
            &mut vol,
            &mut pan,
            &recording,
            &[],
        );
        for &v in vol.iter() {
            assert!(
                (v - 0.9).abs() < 1e-6,
                "expected track.volume bypass (0.9), got {}",
                v
            );
        }
    }

    /// Phase 4 Step C-2: recording set にない lane は通常通り curve eval する。
    /// recording bypass test と pair の sanity check (= bypass が必要なときだけ
    /// 効いて、 不要なときは無回帰)。
    #[test]
    fn non_recording_lane_still_uses_curve() {
        let mut song = one_volume_lane_song();
        song.tracks[0].volume = 0.9;
        let mut vol = vec![0.0_f32; 4];
        let mut pan = vec![0.0_f32; 4];
        let empty = empty_recording_lanes();
        // Beat 2.0 の curve eval は 0.5、 bypass されないので vol[i] = 0.5。
        ramps(
            Some(&song),
            0,
            SR,
            120.0,
            2.0,
            4,
            &mut vol,
            &mut pan,
            &empty,
            &[],
        );
        for &v in vol.iter() {
            assert!((v - 0.5).abs() < 0.001, "expected curve eval (0.5), got {}", v);
        }
    }

    /// One track (id 7) with a single `PluginParam` (device_id 40, param 5)
    /// automation lane ramping 0.25 → 0.75 over beats 0..4.
    const DEVICE_ID: u64 = 40;

    /// レーンとノブ (静的値) の優先順位: 有効なレーンがあればカーブ値が勝ち、
    /// 録音中はノブが素通しになる (= `fill_track_param_ramps` と同じ規則)。
    #[test]
    fn 内蔵デバイスのレーンはカーブ値で上書きし録音中は素通しする() {
        use common::model::{CompParam, Device, EqBand, EqParam, NativeKind, NativeParamId, NativeParams};

        let target = AutomationTarget::NativeParam { device_id: 5, param: NativeParamId::Comp(CompParam::Threshold) };
        let mut song = Song { bpm: 120.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![AutomationPoint {
                    id: 1,
                    time_beat: 0.0,
                    // 正規化 0.5 = -30dB (Threshold は -60..0 の線形)。
                    value: -30.0,
                    curve: AutomationCurve::Linear,
                }],
                next_point_id: 2,
            }),
        );
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "thr".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
                color: None,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(target.clone(), -30.0)
        };
        let mut comp = NativeDevice::new_added(NativeKind::Comp, 5, 1);
        comp.set_param(NativeParamId::Comp(CompParam::Threshold), -6.0);
        let mut eq = NativeDevice::new_added(NativeKind::Eq, 6, 1);
        eq.set_param(NativeParamId::Eq { band: EqBand::Hmf, param: EqParam::Gain }, 4.0);
        song.tracks.push(track(|t| {
            t.id = 1;
            t.devices = vec![Device::Native(comp), Device::Native(eq)];
            t.automation_lanes = vec![lane];
            t.next_lane_id = 2;
        }));

        let empty = empty_recording_lanes();
        let index = SongIndex::build(&song);
        let resolve = |dev: &NativeDevice, rec| {
            let store = owner_store(&song, &index, 1);
            resolve_native_device(&song.clip_contents, store, dev, 1, TrackRows::default(), 0.0, rec, ModTickPlaneRef::default())
        };
        let thr = |d: NativeDevice| d.param(NativeParamId::Comp(CompParam::Threshold)).unwrap();
        assert!((thr(resolve(&comp, &empty)) - -30.0).abs() < 1e-4);
        // レーンは device id で絞る: 別の device (EQ) は静的値のまま残る (巻き添え確認)。
        assert_eq!(resolve(&eq, &empty), eq, "レーンの無い device まで書き換わっている");
        assert!(matches!(resolve(&eq, &empty).params, NativeParams::Eq(s) if s.hmf.gain_db == 4.0));

        // 録音中 (= ノブを掴んでいる) はカーブを評価せず静的値が残る。
        let mut recording = std::collections::HashSet::new();
        recording.insert((1_u32, target));
        assert!((thr(resolve(&comp, &recording)) - -6.0).abs() < 1e-6);
    }

    fn one_plugin_param_lane_song() -> Song {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.25,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 4.0,
                        value: 0.75,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 3,
            }),
        );
        let target = AutomationTarget::PluginParam {
            device_id: DEVICE_ID,
            param_id: 5,
            legacy_device_index: None,
        };
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "p".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
                color: None,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(target, 0.5)
        };
        song.tracks.push(track(|t| {
            t.id = 7;
            t.name = "T".into();
            t.automation_lanes = vec![lane];
            t.next_lane_id = 2;
        }));
        song
    }

    /// B4 (r.md #8): ramping plugin-param lane を 512-frame buffer で fill すると、
    /// frame 0 の 1 event でなく sub-buffer (64 刻み) の複数 event が出る
    /// (= sample-accurate、 速い automation の zipper 解消)。
    #[test]
    fn fill_pd_param_events_sub_samples_changing_curve() {
        let song = one_plugin_param_lane_song();
        let mut pd = ProcessData::empty();
        let empty = empty_recording_lanes();
        pd_params(&mut pd, &song, 7, DEVICE_ID, SR, 120.0, 0.0, 512, &empty, &[]);
        assert!(
            pd.n_events_in > 1,
            "ramp は sub-buffer で複数 event を出すべき, got {}",
            pd.n_events_in
        );
        // 全 event が param_id 5、 frame offset 単調増加、 値は ramp に沿って増加。
        let mut last_time = 0u32;
        for i in 0..pd.n_events_in as usize {
            let e = &pd.events_in[i];
            assert_eq!(e.param_id, 5);
            if i > 0 {
                assert!(e.time > last_time, "frame offset 単調増加");
            }
            last_time = e.time;
        }
        assert!(pd.events_in[0].value >= 0.25 - 1e-6);
        let last = pd.events_in[(pd.n_events_in - 1) as usize].value;
        assert!(last > pd.events_in[0].value, "ramp で値が増加");
    }

    /// 刻みを間引く予算は「最後の刻みは必ず送る」1 件を含めて数える。数えないと 1 ストリームが予算 + 1 件になり、
    /// ストリームが多いと `param_mods` のリングが溢れて先頭 param の刻みが落ちる (20 param × 16 ボイスの per-note)。
    #[test]
    fn many_param_mod_streams_fit_the_ring_and_keep_the_last_tick() {
        use crate::graph::voices::VoiceTable;
        use common::model::{LfoConfig, LfoShape, ModRouting, ModSource, ModSourceKind, Polarity, RetriggerMode};
        let mut song = Song::default();
        song.mod_sources.push(ModSource {
            id: 9,
            owner_track_id: 7,
            color: [0.0; 3],
            kind: ModSourceKind::Lfo(LfoConfig { shape: LfoShape::SawUp, retrigger: RetriggerMode::Note, ..LfoConfig::default() }),
            enabled: true,
        });
        song.tracks.push(track(|t| {
            t.id = 7;
            t.mod_routings = (0..20u32)
                .map(|p| ModRouting {
                    id: p + 1,
                    target: AutomationTarget::PluginParam { device_id: DEVICE_ID, param_id: p, legacy_device_index: None },
                    source_id: 9,
                    depth: 1.0,
                    polarity: Polarity::Unipolar,
                    enabled: true,
                })
                .collect();
        }));
        let plan = common::mod_graph::build_plan(&song, 1, |b| b);
        let plane = ModTickPlaneRef::new(&plan.slot_ids, &[], 64).with_nodes(&plan.nodes);
        let mut voices = VoiceTable::new(DEVICE_ID);
        for v in 0..16u32 {
            voices.note_on(100 + v, 60, 0, -f64::from(v) * 0.01, 0.0, 1.0);
        }
        let index = SongIndex::build(&song);
        let frames = 1024;
        let mut pd = ProcessData::empty();
        let store = owner_store(&song, &index, 7);
        let empty = empty_recording_lanes();
        fill_pd_param_events(&mut pd, &song, 7, store, TrackRows::default(), DEVICE_ID, SR, 120.0, 0.5, frames, &empty, plane, Some(&voices));
        assert_eq!(pd.param_mods_dropped, 0, "リングが溢れた");
        let last = plane.starts(frames).last().expect("刻みがある");
        for p in 0..20u32 {
            for v in 0..16i32 {
                assert!(
                    pd.param_mods_iter().any(|m| m.param_id == p && m.note_id == 100 + v && m.time == last),
                    "param {p} / note {} に最後の刻みが届いていない",
                    100 + v
                );
            }
        }
    }

    /// r.md #117: `Note` 起点のソースを source にする plugin param の routing は、 鳴っている
    /// ボイスごとに `ParamMod { note_id, key }` を積む (値はそのノートの note-on からの位相)。
    /// ノートが無ければ per-note は 1 件も出ない。 routing / source のバイパスは飛ばす。
    #[test]
    fn per_note_sources_emit_one_param_mod_stream_per_voice() {
        use common::model::{LfoConfig, LfoShape, ModRouting, ModSource, ModSourceKind, Polarity, RetriggerMode};
        use crate::graph::voices::VoiceTable;
        let mut song = Song::default();
        song.mod_sources.push(ModSource {
            id: 9,
            owner_track_id: 7,
            color: [0.0; 3],
            kind: ModSourceKind::Lfo(LfoConfig {
                shape: LfoShape::SawUp,
                rate: common::model::ModRate::default(), // 1/4 = 1 拍で 1 周
                retrigger: RetriggerMode::Note,
                ..LfoConfig::default()
            }),
            enabled: true,
        });
        let target = AutomationTarget::PluginParam { device_id: DEVICE_ID, param_id: 5, legacy_device_index: None };
        song.tracks.push(track(|t| {
            t.id = 7;
            t.mod_routings = vec![ModRouting {
                id: 1,
                target: target.clone(),
                source_id: 9,
                depth: 1.0,
                polarity: Polarity::Unipolar,
                enabled: true,
            }];
        }));
        let empty = empty_recording_lanes();
        // 値面の列は評価計画の slot (source の種類もそこから引く)。行は無い = 面の値は引けない。
        let plan = common::mod_graph::build_plan(&song, 1, |b| b);
        let plane = ModTickPlaneRef::new(&plan.slot_ids, &[], 64).with_nodes(&plan.nodes);
        // 2 ボイス: note 100 は beat 0 から、 note 101 は beat 0.25 から。 buffer 頭 = 0.5 拍。
        let mut voices = VoiceTable::new(DEVICE_ID);
        voices.note_on(100, 60, 0, 0.0, 0.0, 1.0);
        voices.note_on(101, 64, 0, 0.25, 0.125, 1.0);
        let mut pd = ProcessData::empty();
        let index = SongIndex::build(&song);
        let store = owner_store(&song, &index, 7);
        fill_pd_param_events(&mut pd, &song, 7, store, TrackRows::default(), DEVICE_ID, SR, 120.0, 0.5, 128, &empty, plane, Some(&voices));
        // global (最新ノート = 面の値、 ここでは面が空なので 0) は従来どおり別に積まれる。
        let mods: Vec<_> = pd.param_mods_iter().copied().filter(|m| !m.is_global()).collect();
        let at0 = |nid: i32| mods.iter().find(|m| m.note_id == nid && m.time == 0).map(|m| m.value).expect("frame 0 の mod");
        assert!((at0(100) - 0.5).abs() < 1e-6, "note 100: 0.5 拍経過 = 位相 0.5");
        assert!((at0(101) - 0.25).abs() < 1e-6, "note 101: 0.25 拍経過");
        assert_eq!(mods.iter().find(|m| m.note_id == 100).unwrap().key, 60);
        assert!(mods.iter().any(|m| m.note_id == 100 && m.time == 64), "刻みごとに出る");

        // ボイス無し → per-note なし。 source バイパス → なし。
        let per_note = |pd: &ProcessData| pd.param_mods_iter().filter(|m| !m.is_global()).count();
        let mut pd = ProcessData::empty();
        fill_pd_param_events(&mut pd, &song, 7, store, TrackRows::default(), DEVICE_ID, SR, 120.0, 0.5, 128, &empty, plane, None);
        assert_eq!(per_note(&pd), 0);
        song.mod_sources[0].enabled = false;
        let plan = common::mod_graph::build_plan(&song, 2, |b| b);
        let plane = ModTickPlaneRef::new(&plan.slot_ids, &[], 64).with_nodes(&plan.nodes);
        let index = SongIndex::build(&song);
        let store = owner_store(&song, &index, 7);
        let mut pd = ProcessData::empty();
        fill_pd_param_events(&mut pd, &song, 7, store, TrackRows::default(), DEVICE_ID, SR, 120.0, 0.5, 128, &empty, plane, Some(&voices));
        assert_eq!(per_note(&pd), 0, "バイパス中の source は per-note も出さない");
    }

    /// Phase 4 Step C-2 (plugin param 版) 回帰: recording 中 (Touch/Latch/Write)
    /// の plugin-param lane は curve eval を skip し、 plugin が GUI で持つ値を
    /// host が上書きしない。 旧実装は skip が無く write が read のままだった。
    #[test]
    fn fill_pd_param_events_skips_recording_lanes() {
        let song = one_plugin_param_lane_song();
        let track_id = 7;
        let target = AutomationTarget::PluginParam {
            device_id: DEVICE_ID,
            param_id: 5,
            legacy_device_index: None,
        };

        // Not recording: the curve value is pushed as a ParamValue event (read).
        let mut pd = ProcessData::empty();
        let empty = empty_recording_lanes();
        pd_params(&mut pd, &song, track_id, DEVICE_ID, SR, 120.0, 0.0, 64, &empty, &[]);
        assert_eq!(pd.n_events_in, 1, "read mode must push the curve value");

        // Recording: the lane is skipped, so no curve event overwrites the
        // plugin's live GUI value.
        let mut pd2 = ProcessData::empty();
        let mut rec = std::collections::HashSet::new();
        rec.insert((track_id, target));
        pd_params(&mut pd2, &song, track_id, DEVICE_ID, SR, 120.0, 0.0, 64, &rec, &[]);
        assert_eq!(pd2.n_events_in, 0, "recording lane curve must be suppressed");
    }

    /// r.md #8 再監査: master fx (`MASTER_TRACK_ID`) の PluginParam automation は
    /// track ではなく `song_lanes` から引く。 track を 1 つも持たない song の
    /// song_lanes に置いた PluginParam lane が `pd_params(MASTER_TRACK_ID,
    /// device_id)` で適用されること (= master fx 自動化) を検証。
    #[test]
    fn fill_pd_param_events_master_fx_reads_song_lanes() {
        let mut song = Song { bpm: 120.0, ..Song::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.25,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 4.0,
                        value: 0.75,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 3,
            }),
        );
        let target = AutomationTarget::PluginParam {
            device_id: DEVICE_ID,
            param_id: 5,
            legacy_device_index: None,
        };
        let lane = AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "m".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                content_offset_beats: 0.0,
                color: None,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(target, 0.5)
        };
        song.song_lanes = vec![lane];
        // MASTER_TRACK_ID の track は存在しない → song_lanes 経由で解決するはず。
        let mut pd = ProcessData::empty();
        let empty = empty_recording_lanes();
        pd_params(
            &mut pd,
            &song,
            common::model::MASTER_TRACK_ID,
            DEVICE_ID,
            SR,
            120.0,
            0.0,
            64,
            &empty,
            &[],
        );
        assert_eq!(pd.n_events_in, 1, "master fx PluginParam lane (song_lanes) must apply");
        assert_eq!(pd.events_in[0].param_id, 5);
        assert!(
            (pd.events_in[0].value - 0.25).abs() < 1e-6,
            "curve value at beat 0 should be 0.25, got {}",
            pd.events_in[0].value
        );
        // 別 device_id (別 master fx) には適用されない。
        let mut pd2 = ProcessData::empty();
        pd_params(
            &mut pd2,
            &song,
            common::model::MASTER_TRACK_ID,
            DEVICE_ID + 1,
            SR,
            120.0,
            0.0,
            64,
            &empty,
            &[],
        );
        assert_eq!(pd2.n_events_in, 0, "別 device_id の master fx には lane が無い");
    }
}
