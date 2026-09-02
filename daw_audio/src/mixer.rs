//! Per-track scratch buffers used by the audio worker.
//!
//! The audio engine's worker pool reuses one `TrackScratch` per track every
//! buffer — track audio output and the MIDI ping-pong buses live here so
//! the RT loop never allocates. Cache-line aligned to keep concurrent
//! workers from false-sharing each other's scratch.

#![allow(dead_code)]

/// 内蔵チャンネルストリップ (コンプ + EQ) の RT 実行。mixer strip の一部なので
/// `mixer` の下に置く (`docs/plan_channel_strip.md`)。
pub mod channel_strip;

use crate::graph::DelayLine;
use crate::sequencer::{PerTrackState, TimedNoteEvent};

pub const MAX_FRAMES: usize = common::process_data::MAX_FRAMES;
pub const MAX_EVENTS: usize = common::process_data::MAX_EVENTS;

/// Pre-allocated capacity (samples per channel) for each track's input delay
/// line, so PDC / sidechain alignment never reallocates on the audio thread
/// (D1 / PR3). 48000 = 1 s at 48 kHz — comfortably above any real plugin's
/// reported latency. これを超える (病的な) 補償量は publish 側 (off-thread)
/// が replacement line を pre-alloc して bundle で配送する
/// (`RtBundle::input_delay_replacements`)。
pub(crate) const INPUT_DELAY_PREALLOC_SAMPLES: usize = 48_000;

/// E5 (r.md #8): 1 track が同時に持てる tape 位置 accumulator の数 (= track 内
/// audio event の最大 index)。 これを超える index の event は積分無し (= 毎回
/// `event_local × ratio` で再計算) に degrade する。 1 track に数百 clip は実用上
/// 稀なので 256 で足りる。
const MAX_TAPE_EVENTS_PER_TRACK: usize = 256;

#[repr(align(64))]
pub struct TrackScratch {
    /// Per-track audio output (left). Reduced into the master bus after
    /// every worker finishes its dispatch.
    pub track_l: Vec<f32>,
    /// Per-track audio output (right).
    pub track_r: Vec<f32>,
    /// MIDI event ping-pong buffer A. Plugins consume from one and emit
    /// into the other; the worker swaps them between stages.
    pub midi_bus_a: Vec<TimedNoteEvent>,
    pub midi_bus_b: Vec<TimedNoteEvent>,
    /// Stuck-note tracking + queued offs for next buffer.
    pub state: PerTrackState,
    pub peak_l: f32,
    pub peak_r: f32,
    /// Set during dispatch: `muted || (any_solo && !solo)`. The reduce step
    /// reads this to skip the accumulation entirely for muted tracks.
    pub effective_mute: bool,
    /// PR4.5 sidechain plugin-internal alignment: per-track input delay
    /// applied between instrument output and the fx_chain. Capacity grown
    /// only at edit-time (engine schedule swap) so the RT path stays free
    /// of the allocator. Capacity 0 = no delay (most tracks); a track with
    /// fx_chain sidechain gets its line resized to `Schedule::
    /// input_delay_per_track[track_idx] + 1` (DelayLine spec requires
    /// capacity ≥ delay + 1).
    pub input_delay_line: DelayLine,
    /// E5 (r.md #8): tape (Raw / Repitch) mode の **連続 source 位置 accumulator**
    /// (event 単位、 添字 = track 内 schedule 順 index)。
    /// `(last_event_local, accumulated_source_pos)`。 Repitch は `event_local × ratio`
    /// で絶対位置を毎 buffer 再計算していたため tempo automation で ratio が変わると
    /// 位置が跳んで click した。 contiguous 再生では ratio を積分 (= 連続)、
    /// seek/schedule 変化 (event_local 不連続) では再 anchor して click を防ぐ。
    /// `u64::MAX` = 未初期化。 起動時に `MAX_TAPE_EVENTS_PER_TRACK` ぶん pre-alloc し
    /// RT で再確保しない。
    pub repitch_accum: Vec<(u64, f64)>,
    /// r.md #40: この track の stretch engine pool。 引き当ては位置ではなく
    /// **`RenderedEvent::stream_key`** で行う (`acquire_engine`)。
    /// 1 個 ~1 MB なので **確保は off-thread** で行い、
    /// RT は配送された物を `push` するだけ (`Vec` は容量
    /// `MAX_STRETCH_ENGINES_PER_TRACK` ぶん予約済なので push で再確保しない =
    /// 既に走行中のエンジンを触らずに増やせる)。
    pub stretch_engines: Vec<crate::stretch_engine::StretchEngine>,
    /// スペクトル経路の per-event 出力バッファ (fade / gain / pan を掛ける前)。
    pub stretch_out_l: Vec<f32>,
    pub stretch_out_r: Vec<f32>,
    /// `render_audio_events` が buffer ごとに増やす連番。 同じ buffer 内で
    /// 2 つの発音が同じ stretch engine を掴むのを防ぐ (`acquire_engine`)。
    pub clip_render_seq: u64,
    /// Per-sample volume gain ramp for the buffer about to be processed.
    /// `MAX_FRAMES` long, allocated once at construction and overwritten
    /// in place every buffer by `fill_track_param_ramps`. The fx-chain
    /// post-process loop reads this to apply sample-accurate volume
    /// automation. When the track has no `Volume` lane (or the lane is
    /// disabled), the buffer is filled with the constant
    /// `track.volume`.
    pub volume_per_sample: Vec<f32>,
    /// Per-sample pan ramp, same lifecycle as `volume_per_sample`.
    /// Range `-1.0..=1.0` (left..right). Default constant fill is
    /// `track.pan`.
    pub pan_per_sample: Vec<f32>,
    /// Post-fx, **pre-fader** snapshot of this track's signal (taken
    /// before the volume / pan strip overwrites `track_l/r` in place).
    /// Written by `process_track_owned` / `run_group_fx_chain` only when
    /// the track has a pre-fader aux send, and read by a `MixSend` whose
    /// send `mode == PreFader`. `MAX_FRAMES` long, allocated once.
    pub pre_fader_l: Vec<f32>,
    pub pre_fader_r: Vec<f32>,
    /// **Pre-FX** snapshot of this track's signal (the raw audio clip /
    /// input *before* the device chain runs). Written by
    /// `process_track_owned` / `run_group_fx_chain` only when a
    /// `TapPoint::PreFx` tap / mod source reads this track
    /// (`track_needs_prefx_snapshot`), and read by a `SidechainTap` /
    /// `EnvelopeFollow` resolving `BufRef::PreFxScratch`. `MAX_FRAMES` long,
    /// allocated once. docs/plan_modulation_followups.md §1.
    pub pre_fx_l: Vec<f32>,
    pub pre_fx_r: Vec<f32>,
    /// 内蔵チャンネルストリップ (コンプ + EQ) の状態 (`docs/plan_channel_strip.md`)。
    /// バイクワッドの遅延・平滑済みゲイン・係数キャッシュを buffer 間で保つ。
    /// 固定サイズなので `TrackScratch` に埋めても RT で確保は起きない。
    pub strip: channel_strip::StripState,
    /// 直前 buffer の最大ゲインリダクション (dB、0 以下)。
    /// `engine` が peak と一緒に `AudioBridge` へ publish し、mixer strip の
    /// GR メーターになる。
    pub strip_gr_db: f32,
}

impl TrackScratch {
    pub fn new() -> Self {
        Self {
            track_l: vec![0.0; MAX_FRAMES],
            track_r: vec![0.0; MAX_FRAMES],
            midi_bus_a: Vec::with_capacity(MAX_EVENTS),
            midi_bus_b: Vec::with_capacity(MAX_EVENTS),
            // capacity は sequencer の `ACTIVE_NOTES_CAP` (= `MAX_EVENTS`) と
            // 一致させる。 push 前 clamp が `MAX_EVENTS` で効くので、 ここを
            // それ未満にすると clamp が防げない区間で RT realloc が起きる。
            state: PerTrackState::with_capacity(MAX_EVENTS),
            peak_l: 0.0,
            peak_r: 0.0,
            effective_mute: false,
            // D1 / PR3: pre-allocate the sidechain/PDC input delay ring so the
            // `refresh_schedule` alignment step never reallocates on the audio
            // thread. `INPUT_DELAY_PREALLOC_SAMPLES` (1 s @ 48 kHz) is above any
            // real plugin's reported latency; the refresh path still grows it
            // for the pathological >1 s case (which no real plugin hits).
            input_delay_line: DelayLine::with_capacity(INPUT_DELAY_PREALLOC_SAMPLES),
            repitch_accum: vec![(u64::MAX, 0.0); MAX_TAPE_EVENTS_PER_TRACK],
            // 実体 (= 高価なエンジン) は off-thread で作って配送される。 ここでは
            // 容量だけ予約しておき、RT の `push` が再確保しないことを保証する。
            stretch_engines: Vec::with_capacity(
                crate::audio_clip_renderer::MAX_STRETCH_ENGINES_PER_TRACK,
            ),
            stretch_out_l: vec![0.0; MAX_FRAMES],
            stretch_out_r: vec![0.0; MAX_FRAMES],
            clip_render_seq: 0,
            volume_per_sample: vec![1.0; MAX_FRAMES],
            pan_per_sample: vec![0.0; MAX_FRAMES],
            pre_fader_l: vec![0.0; MAX_FRAMES],
            pre_fader_r: vec![0.0; MAX_FRAMES],
            pre_fx_l: vec![0.0; MAX_FRAMES],
            pre_fx_r: vec![0.0; MAX_FRAMES],
            strip: channel_strip::StripState::default(),
            strip_gr_db: 0.0,
        }
    }
}

impl Default for TrackScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// mixer strip を scratch に in-place 適用する: per-sample の equal-power
/// pan と volume ramp (`volume_per_sample` / `pan_per_sample`、 事前に
/// `fill_track_param_ramps` が埋めた値) を `track_l/r` に掛け、 peak meter を
/// 更新する。 leaf (`process_track_owned`) と bus (`run_group_fx_chain`) の
/// 2 箇所にほぼ同文でインライン展開されていた処理の単一実装
/// (`docs/plan_arch_refactor.md` §5)。
///
/// mute の規則 (両呼び出し元共通):
/// - `muted` (明示 mute) は出力を完全にゼロ化 — dry / send / sidechain の
///   どこにも流さない。
/// - `effective_mute` (solo による除外を含む) は **meter だけ** dark にする。
///   信号自体は `track_l/r` に残す — solo された return への send や
///   sidechain tap はミュート対象からも読めるのが Ableton 準拠の挙動。
///
/// RT-safe: in-place 書き込みのみ、確保・ロックなし。
pub fn apply_strip(scratch: &mut TrackScratch, n: usize, muted: bool, effective_mute: bool) {
    let n = n
        .min(scratch.track_l.len())
        .min(scratch.track_r.len())
        .min(scratch.volume_per_sample.len())
        .min(scratch.pan_per_sample.len());
    scratch.effective_mute = effective_mute;
    let mut peak_l = 0.0_f32;
    let mut peak_r = 0.0_f32;
    for i in 0..n {
        // pan 則の SSoT は `common::audio_render::pan_gains` (焼き込み側が
        // これを打ち消すので、式を 2 か所に持たない)。
        let (pan_l, pan_r) = common::audio_render::pan_gains(scratch.pan_per_sample[i]);
        let vol = scratch.volume_per_sample[i];
        let gain_l = pan_l * vol;
        let gain_r = pan_r * vol;
        let l = scratch.track_l[i] * gain_l;
        let r = scratch.track_r[i] * gain_r;
        scratch.track_l[i] = l;
        scratch.track_r[i] = r;
        if l.abs() > peak_l {
            peak_l = l.abs();
        }
        if r.abs() > peak_r {
            peak_r = r.abs();
        }
    }
    scratch.peak_l = peak_l;
    scratch.peak_r = peak_r;
    if muted {
        scratch.track_l[..n].fill(0.0);
        scratch.track_r[..n].fill(0.0);
    }
    if effective_mute {
        scratch.peak_l = 0.0;
        scratch.peak_r = 0.0;
    }
}

/// 内蔵チャンネルストリップ (コンプ → EQ) を scratch に in-place 適用し、
/// この buffer の最大ゲインリダクション (dB、0 以下) を `strip_gr_db` に残す。
///
/// 設計正本は `docs/plan_channel_strip.md`。呼び出し位置は leaf / bus とも
/// **pre-fader tap の直前** (= inserts の後、フェーダーの前) で、両経路が
/// この 1 実装を共有する ([`apply_strip`] と同じ理由 — 同文のインライン展開を
/// 作らない)。
///
/// オートメーションと変調の解決は [`crate::automation::resolve_track_strip`]
/// (block-rate)。`song` が無い (= 初期化中) ときは何もしない。
///
/// RT-safe: 確保・ロック・I/O なし。係数の組み直しは `StripState` が
/// 「値が変わった buffer だけ」に絞る。
#[allow(clippy::too_many_arguments)]
pub fn apply_channel_strip(
    scratch: &mut TrackScratch,
    song: Option<&common::model::Song>,
    song_track: &common::model::Track,
    rows: crate::launcher::TrackRows<'_>,
    sample_rate: u32,
    playhead_beats: f64,
    n: usize,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    mod_plane: common::mod_plane::ModTickPlaneRef<'_>,
) {
    let Some(song) = song else {
        scratch.strip_gr_db = 0.0;
        return;
    };
    if song_track.strip.is_bypassed()
        && song_track.automation_lanes.is_empty()
        && song_track.mod_routings.is_empty()
    {
        // 触られていないトラック (= 大多数) はここで抜ける。
        scratch.strip_gr_db = 0.0;
        return;
    }
    let resolved = crate::automation::resolve_track_strip(
        song,
        song_track,
        rows,
        playhead_beats,
        recording_lanes,
        mod_plane,
    );
    // `strip` (状態) と `track_l/r` (信号) を同時に可変で借りるので分解する。
    let TrackScratch { strip, track_l, track_r, strip_gr_db, .. } = scratch;
    #[allow(clippy::cast_precision_loss)]
    let sr = sample_rate as f32;
    *strip_gr_db = strip.process(&resolved, track_l, track_r, n, sr);
}

/// 鳴っている全 note を「次の drain (= 各 track の process 冒頭、frame 0)」で出す
/// NoteOff として予約し、追跡集合を空にする。
///
/// **stuck note を防ぐ全経路が共有する唯一の実装。** live は Stop / loop wrap / seek
/// ([`crate::engine::LocalState::queue_all_notes_off`])、書き出しは走査が
/// `write_end` を越えた瞬間 (= live の Stop に対応する点) で通る。手写しすると
/// どれかが必ず漏れ、跳び越された Off が二度と emit されず note が鳴り続ける。
///
/// RT-safe: `pending_offs` は `process_track_owned` の冒頭で毎 buffer drain + clear
/// されるので push 時点では空。`active_notes` は `PerTrackState::with_capacity` の
/// 確保量でクランプ済みなので push で再確保しない。
pub fn queue_all_notes_off(scratch: &mut [TrackScratch]) {
    for s in scratch.iter_mut() {
        for &k in &s.state.active_notes {
            s.state.pending_offs.push(k);
        }
        s.state.active_notes.clear();
    }
}
