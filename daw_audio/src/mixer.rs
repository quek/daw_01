//! Per-track scratch buffers used by the audio worker.
//!
//! The audio engine's worker pool reuses one `TrackScratch` per track every
//! buffer — track audio output and the MIDI ping-pong buses live here so
//! the RT loop never allocates. Cache-line aligned to keep concurrent
//! workers from false-sharing each other's scratch.

#![allow(dead_code)]

use crate::graph::DelayLine;
use crate::sequencer::{PerTrackState, TimedNoteEvent};

pub const MAX_FRAMES: usize = common::process_data::MAX_FRAMES;
pub const MAX_EVENTS: usize = common::process_data::MAX_EVENTS;

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
    /// Post-fx, **pre-fader** snapshot of this track's signal (taken after the
    /// whole device chain in its order — r.md #129: 組み込みの Comp / EQ も含む — and
    /// before the volume / pan strip overwrites `track_l/r` in place).
    /// Written by `process_track_owned` / `run_group_fx_chain` only when
    /// something reads it (pre-fader aux send / `PostFx` tap / mod source:
    /// `ChainProgram::snapshot_post_fx`). `MAX_FRAMES` long, allocated once.
    pub pre_fader_l: Vec<f32>,
    pub pre_fader_r: Vec<f32>,
    /// **Pre-FX** snapshot of this track's signal (the raw audio clip /
    /// input *before* the device chain runs). Written by
    /// `process_track_owned` / `run_group_fx_chain` only when a
    /// `TapPoint::PreFx` tap / mod source reads this track
    /// (`ChainProgram::snapshot_pre_fx`), and read by a `SidechainTap` /
    /// `NativeSidechainTap` / `EnvelopeFollow` resolving `BufRef::PreFxScratch`
    /// (自トラック Pre-FX を読む device は同じ pass の snapshot を直接読む)。 `MAX_FRAMES` long,
    /// allocated once. docs/plan_modulation_followups.md §1.
    pub pre_fx_l: Vec<f32>,
    pub pre_fx_r: Vec<f32>,
    /// Global Sampler (`docs/plan_global_sampler.md` §3.2): 録音源がこの track の
    /// PreFx / PostFx tap のとき engine が buffer ごとに立てる。`Song` に無い tap
    /// なので compile 時に焼く `ChainProgram::snapshot_*` では拾えない。
    pub force_prefx_snapshot: bool,
    pub force_prefader_snapshot: bool,
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
            // 遅延が要る track の線は schedule と同じ便で off-thread 確保して届く
            // (`RtBundle::input_delay_replacements`)。全 track に先回りで 1 秒ぶん持たせない
            // (`docs/plan_unbounded_tracks.md` §2.2)。
            input_delay_line: DelayLine::with_capacity(0),
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
            force_prefx_snapshot: false,
            force_prefader_snapshot: false,
        }
    }
}

impl Default for TrackScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// per-track の入力遅延線 (サイドチェインの揃え) を新しい schedule の遅延に合わせる (bundle の install 時)。
/// RT: 確保・解放なし。
///
/// - `old_delays[i] == 0` (または無い) の track のリングは止まっていたので、中身は古い音 (前 project の音も含む)。
///   読み出す前に 0 にする。
/// - 容量が足りない行は off-thread で確保済みの `replacements[i]` と swap する (旧 line は bundle 側で off-thread
///   drop)。走っていたリングならその過去を写す — 写さないと新しい遅延の長さぶん無音が挟まる。
/// - 遅延の無い track は読まないので触らない (全 track を memset しない)。
pub(crate) fn install_input_delay_lines(
    scratch: &mut [TrackScratch],
    old_delays: &[u32],
    new_delays: &[u32],
    replacements: &mut [Option<DelayLine>],
) {
    for (i, (s, &d)) in scratch.iter_mut().zip(new_delays).enumerate() {
        if d == 0 {
            continue;
        }
        let running = old_delays.get(i).is_some_and(|&o| o > 0);
        match replacements.get_mut(i).and_then(Option::as_mut) {
            Some(line) if s.input_delay_line.capacity() < line.capacity() => {
                if running {
                    line.carry_history_from(&s.input_delay_line);
                }
                std::mem::swap(&mut s.input_delay_line, line);
            }
            _ if !running => s.input_delay_line.reset(),
            _ => {}
        }
    }
}

/// per-track scratch の **成長便** (`RtBundle::scratch_growth`、`docs/plan_unbounded_tracks.md` §2.1)。
///
/// `rows` = index `base..base + rows.len()` の行 (off-thread で確保)。容量は `base + rows.len()` 以上
/// (= 伸ばした後の総本数) で、RT はその容量の中で既存の行を並べ直して差し込む。**全本数ぶんを毎回
/// 作り直さない** — 200 本の曲で 1 本足すたびに 200 本ぶん確保しないため。
pub struct ScratchGrowth {
    pub base: usize,
    pub rows: Vec<TrackScratch>,
}

impl ScratchGrowth {
    /// index `base..total` の行を持つ便 (off-thread)。
    #[must_use]
    pub fn new(base: usize, total: usize) -> Self {
        let mut rows = Vec::with_capacity(total);
        rows.extend((base..total).map(|_| TrackScratch::new()));
        Self { base, rows }
    }

    fn end(&self) -> usize {
        self.base + self.rows.len()
    }

    /// audio thread: `scratch` を `end()` 本まで伸ばす。既存の行 (走行状態: 入力遅延のリング / stretch
    /// engine / 鳴っているノート) は要素ごと move で保ち、便の行のうち既存と重なる分は捨てる側へ回す。
    /// 戻り値 = 押し出した Vec (空か、重なって要らなくなった行。recycle で off-thread に落とす)。
    ///
    /// RT 安全: swap / 容量内の extend / rotate だけ (確保・解放なし)。便の base が既存の本数より
    /// 先にある (= 間の便が失われた) ときだけは差し込めないので、便をそのまま返す。
    pub fn install_into(mut self, scratch: &mut Vec<TrackScratch>) -> Vec<TrackScratch> {
        let (s, base, len) = (scratch.len(), self.base, self.rows.len());
        if s >= base + len || s < base || self.rows.capacity() < base + len {
            debug_assert!(s >= base, "成長便の間が抜けている (base {base} > 既存 {s})");
            return self.rows;
        }
        // 既存と重なる行 (`base..s`) は既存の走行状態を便の側へ移し、便の新品を既存の側へ退避する。
        for j in 0..s - base {
            std::mem::swap(&mut scratch[base + j], &mut self.rows[j]);
        }
        let head = base;
        self.rows.extend(scratch.drain(..head));
        self.rows.rotate_left(len);
        std::mem::swap(scratch, &mut self.rows);
        self.rows
    }

    /// `self` (新しい便) が `older` (古い便) を畳み込む (`RtBundle::supersede`、RT 上で呼ばれる)。
    /// publish 側は本数を順に配送するので `older` の行の直後が `self` の行 — 連結して `base` を古い便に
    /// 揃える (容量は伸ばした後の総本数なので確保は起きない)。`older` には空の Vec が残る。
    pub fn absorb_older(&mut self, older: &mut ScratchGrowth) {
        if self.base <= older.base {
            return; // 新しい便が古い便の範囲を含む (テストの全本数便)。古い便は捨てる。
        }
        if older.end() != self.base || self.rows.capacity() < older.base + older.rows.len() + self.rows.len() {
            debug_assert!(false, "成長便の並びが連続していない (古い便 {}..{} / 新しい便 base {})", older.base, older.end(), self.base);
            return;
        }
        let n = self.rows.len();
        self.rows.append(&mut older.rows);
        self.rows.rotate_left(n);
        self.base = older.base;
    }
}

/// mixer strip を scratch に in-place 適用する: per-sample の pan (中央 0 dB の等パワー則、
/// `common::audio_render::pan_gains`) と volume ramp (`volume_per_sample` / `pan_per_sample`、 事前に
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
    // pan 則の SSoT は `common::audio_render::pan_gains` (audio event の pan も同じ式)。
    let (track_l, track_r) = (&mut scratch.track_l[..n], &mut scratch.track_r[..n]);
    let (vols, pans) = (&scratch.volume_per_sample[..n], &scratch.pan_per_sample[..n]);
    (scratch.peak_l, scratch.peak_r) = match common::audio_render::constant_pan_gains(pans) {
        Some(g) => strip_gains(track_l, track_r, vols, |_| g),
        None => strip_gains(track_l, track_r, vols, |i| common::audio_render::pan_gains(pans[i])),
    };
    if muted {
        scratch.track_l[..n].fill(0.0);
        scratch.track_r[..n].fill(0.0);
    }
    if effective_mute {
        scratch.peak_l = 0.0;
        scratch.peak_r = 0.0;
    }
}

/// [`apply_strip`] の本体: `track_l/r[i]` に pan `pan_at(i)` × volume `vols[i]` を掛け、(L, R) の peak を返す。
/// pan が一定の buffer と動く buffer で別々に単態化させる (一定なら三角関数がループから消える)。
#[inline(always)]
fn strip_gains(track_l: &mut [f32], track_r: &mut [f32], vols: &[f32], pan_at: impl Fn(usize) -> (f32, f32)) -> (f32, f32) {
    let mut peak_l = 0.0_f32;
    let mut peak_r = 0.0_f32;
    for (i, ((tl, tr), &vol)) in track_l.iter_mut().zip(track_r.iter_mut()).zip(vols).enumerate() {
        let (pan_l, pan_r) = pan_at(i);
        let gain_l = pan_l * vol;
        let gain_r = pan_r * vol;
        let l = *tl * gain_l;
        let r = *tr * gain_r;
        *tl = l;
        *tr = r;
        if l.abs() > peak_l {
            peak_l = l.abs();
        }
        if r.abs() > peak_r {
            peak_r = r.abs();
        }
    }
    (peak_l, peak_r)
}

/// フェーダーを掛けない strip (`ChainProgram::fader == false` = 焼き込みの `RenderScope::Sources` / `PostFx`): 音はそのまま残し、
/// peak だけを測る。volume / pan / mute / solo はフェーダーの段なので掛けない。
///
/// RT-safe: in-place 読み取りのみ、確保・ロックなし。
pub fn pass_strip(scratch: &mut TrackScratch, n: usize) {
    let n = n.min(scratch.track_l.len()).min(scratch.track_r.len());
    scratch.effective_mute = false;
    scratch.peak_l = scratch.track_l[..n].iter().fold(0.0_f32, |m, s| m.max(s.abs()));
    scratch.peak_r = scratch.track_r[..n].iter().fold(0.0_f32, |m, s| m.max(s.abs()));
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
        for note in s.state.active_notes.iter() {
            s.state.pending_offs.push((note.voice_id, note.key));
        }
        s.state.active_notes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行の同一性の目印に `clip_render_seq` を使う (走行状態を持つ行が入れ替わっていないかを見る)。
    fn tag(rows: &mut [TrackScratch], from: u64) {
        for (i, r) in rows.iter_mut().enumerate() {
            r.clip_render_seq = from + i as u64;
        }
    }

    fn tags(rows: &[TrackScratch]) -> Vec<u64> {
        rows.iter().map(|r| r.clip_render_seq).collect()
    }

    fn existing(n: usize) -> Vec<TrackScratch> {
        let mut rows: Vec<TrackScratch> = (0..n).map(|_| TrackScratch::new()).collect();
        tag(&mut rows, 1);
        rows
    }

    #[test]
    fn 成長便は既存の行を動かさずに後ろへ足す() {
        let mut scratch = existing(2);
        let mut g = ScratchGrowth::new(2, 5);
        tag(&mut g.rows, 30);
        let retired = g.install_into(&mut scratch);
        assert_eq!(tags(&scratch), [1, 2, 30, 31, 32]);
        assert!(retired.is_empty());
    }

    #[test]
    fn 既存と重なる便の行は捨て側へ回して既存の行を残す() {
        let mut scratch = existing(3);
        let mut g = ScratchGrowth::new(0, 4);
        tag(&mut g.rows, 10);
        let retired = g.install_into(&mut scratch);
        assert_eq!(tags(&scratch), [1, 2, 3, 13]);
        assert_eq!(tags(&retired), [10, 11, 12]);
    }

    #[test]
    fn 畳み込んだ便は古い便の行から順に並ぶ() {
        let mut older = ScratchGrowth::new(1, 3);
        tag(&mut older.rows, 11);
        let mut newer = ScratchGrowth::new(3, 4);
        tag(&mut newer.rows, 13);
        newer.absorb_older(&mut older);
        assert!(older.rows.is_empty());

        let mut scratch = existing(1);
        let retired = newer.install_into(&mut scratch);
        assert_eq!(tags(&scratch), [1, 11, 12, 13]);
        assert!(retired.is_empty());
    }
}
