//! Parallel (r.md #110, `docs/plan_parallel.md` §4): device ツリーを **フラットな命令列**
//! ([`ChainProgram`]) に落として RT で走らせる。
//!
//! 旧実装は device walk が 3 か所 (leaf / group bus / master) に重複していた。
//! ここが唯一の walker で、`process_track_owned` / `run_group_fx_chain` /
//! master fx はどれも [`run_chain_program`] を呼ぶ。ツリー (Parallel の中の chain の
//! 中の Parallel …) は compile 時 (off-RT、`program_build`) に展開済みなので、RT は
//! 再帰も確保もしない。
//!
//! # 信号規則
//!
//! - `Plugin`: 既存の port 直結規則 (note_in に MIDI、audio_in に audio、note_out で
//!   MIDI 置換、audio_out は audio_in ありなら置換 / 無しなら加算)。
//! - `ParallelBegin`: 現在のバス (audio L/R + MIDI) を parallel 入力に退避、sum を 0 に。
//! - `ChainBegin`: バス := parallel 入力のコピー (各 chain は同じ入力を受ける)。 r.md #112 の
//!   帯域分割 (`Split::Frequency3`) では chain 1/2/3 が Low / Mid / High (`ParallelBegin` で
//!   `band_split` が作った出力) を受け、 4 本目以降は全帯域のまま。
//! - `ChainEnd`: 並列 PDC の delay → post-fx snapshot → chain の gain/pan/mute →
//!   post-fader snapshot → sum に加算。MIDI は「この chain の中で note_out device が
//!   バスを置換した」ときだけ merged に寄与する。
//! - `ParallelEnd`: バス := sum。MIDI := 置換した chain があれば merged (time 順)、無ければ
//!   parallel 入力の素通し。
//!
//! RT 規約: 確保・ロック・I/O なし。scratch は compile 時に Parallel / chain ごとに
//! 1 slot ずつ確保済み。

use std::ops::Range;

use common::model::{AutomationTarget, LoopRegion, Song, SplitBand, SplitEdge, TrackBuiltinParam};
use common::port_config::PortConfig;
use common::process_data::EventKind;

use crate::engine::{PluginRefs, SyncSlot};
use crate::graph::DelayLine;
use crate::graph::band_split::BandSplit;
use crate::launcher::TrackRows;
use crate::mixer::{MAX_EVENTS, MAX_FRAMES};
use crate::sequencer::{NoteTransition, TimedNoteEvent};
use common::mod_plane::ModTickPlaneRef;

/// 命令 1 つ。slot 番号は同じ [`ChainProgram`] の `parallels` / `chains` の index。
#[derive(Debug, Clone, PartialEq)]
pub enum ChainOp {
    /// plugin を現在のバスへ dispatch する (bypass 中の device は compile が落とす)。
    /// `own_prefx_ports`: bit k = aux 入力 port k が **自 track の Pre-FX** (この buffer の
    /// device chain 入力そのもの) を key にする。 他 track / chain の tap は schedule の
    /// `SidechainTap` が事前に staging するが、 自 track の Pre-FX は同じ pass の snapshot を
    /// ここで直接載せる (lag 0、依存辺なし)。
    Plugin {
        device_id: u64,
        ports: PortConfig,
        own_prefx_ports: u8,
    },
    ParallelBegin { parallel_slot: u32 },
    /// `band`: この chain が受ける帯域 (`Split::Frequency3` の 1〜3 本目)。 `None` = Parallel 入力
    /// そのもの。 compile 時に chain の並び順から決める。
    ChainBegin { parallel_slot: u32, chain_slot: u32, band: Option<SplitBand> },
    ChainEnd {
        parallel_slot: u32,
        chain_slot: u32,
        /// mute / solo / gain / pan を Song snapshot から live-read するためのキー。
        parallel_id: u64,
        chain_id: u64,
        /// 並列 PDC: `(delay_lines の index, frames)`。この chain の latency が Parallel 内
        /// 最大より小さいときだけ `Some`。
        delay: Option<(u32, u32)>,
        /// chain の PostFx tap (device 通過後・gain/pan 前) を誰かが読む。
        snapshot_post_fx: bool,
        /// chain の PostFader tap (gain/pan/mute 後) を誰かが読む。
        snapshot_post_fader: bool,
    },
    /// `parallel_id`: 出力 trim / gain match を Song snapshot から live-read するためのキー。
    ParallelEnd { parallel_slot: u32, parallel_id: u64 },
}

/// Parallel 1 つぶんの RT scratch。
pub struct ParallelScratch {
    /// 再 compile 跨ぎの状態移送のキー (= `Parallel::id`)。
    pub parallel_id: u64,
    /// Parallel 入力 (= 全 chain の共通入力、chain の PreFx tap でもある)。
    pub in_l: Vec<f32>,
    pub in_r: Vec<f32>,
    pub in_midi: Vec<TimedNoteEvent>,
    /// chain 出力の合算。
    pub sum_l: Vec<f32>,
    pub sum_r: Vec<f32>,
    /// MIDI を置換した chain の出力を集めたもの。
    pub merged_midi: Vec<TimedNoteEvent>,
    pub any_midi_replaced: bool,
    /// 出力 trim の per-sample ramp (automation + 変調)。
    pub out_gain_ramp: Vec<f32>,
    /// gain match の状態: 入力 / 出力の mean square (一次 IIR)、 現在掛けている match gain。
    pub in_ms: f32,
    pub out_ms: f32,
    pub match_gain: f32,
    /// r.md #112: 帯域分割器 (`Split::Frequency3` のときだけ compile が置く)。 band chain の
    /// 入力と `PreFx` tap はここから読む。
    pub split: Option<Box<BandSplit>>,
}

/// gain match の窓 (一次 IIR の時定数、秒)。 ポンピングが出ない程度に遅く。
const MATCH_TAU_SECS: f32 = 0.5;
/// gain match が動く下限の mean square (≈ -80 dBFS)。 これ未満 (無音) は直前の gain を保持。
const MATCH_SILENCE_MS: f32 = 1e-8;
/// match gain の範囲 (±12 dB)。
const MATCH_GAIN_MIN: f32 = 0.25;
const MATCH_GAIN_MAX: f32 = 4.0;

impl ParallelScratch {
    pub fn new(parallel_id: u64, split: bool) -> Self {
        Self {
            parallel_id,
            split: split.then(|| Box::new(BandSplit::new())),
            in_l: vec![0.0; MAX_FRAMES],
            in_r: vec![0.0; MAX_FRAMES],
            in_midi: Vec::with_capacity(MAX_EVENTS),
            sum_l: vec![0.0; MAX_FRAMES],
            sum_r: vec![0.0; MAX_FRAMES],
            merged_midi: Vec::with_capacity(MAX_EVENTS),
            any_midi_replaced: false,
            out_gain_ramp: vec![1.0; MAX_FRAMES],
            in_ms: 0.0,
            out_ms: 0.0,
            match_gain: 1.0,
        }
    }

    /// gain match を 1 buffer ぶん進めて、 この buffer の終端で掛ける gain を返す。
    /// 入力 (`in_l/r`) と和 (`sum_l/r`、chain gain 込み) の mean square を時定数
    /// [`MATCH_TAU_SECS`] で追い、 `sqrt(in/out)` を [`MATCH_GAIN_MIN`, `MATCH_GAIN_MAX`] に
    /// 収めたもの。 無音の間は直前の値を保持。 RT 確保なし。
    fn update_gain_match(&mut self, n: usize, sample_rate: u32) -> f32 {
        if n == 0 {
            return self.match_gain;
        }
        let (mut in_acc, mut out_acc) = (0.0f32, 0.0f32);
        for i in 0..n {
            in_acc += self.in_l[i] * self.in_l[i] + self.in_r[i] * self.in_r[i];
            out_acc += self.sum_l[i] * self.sum_l[i] + self.sum_r[i] * self.sum_r[i];
        }
        let inv = 1.0 / (2 * n) as f32;
        let a = (n as f32 / (MATCH_TAU_SECS * sample_rate.max(1) as f32)).clamp(0.0, 1.0);
        self.in_ms += (in_acc * inv - self.in_ms) * a;
        self.out_ms += (out_acc * inv - self.out_ms) * a;
        if self.in_ms > MATCH_SILENCE_MS && self.out_ms > MATCH_SILENCE_MS {
            (self.in_ms / self.out_ms).sqrt().clamp(MATCH_GAIN_MIN, MATCH_GAIN_MAX)
        } else {
            self.match_gain
        }
    }
}

/// chain 1 本ぶんの RT scratch。
pub struct ChainScratch {
    pub chain_id: u64,
    /// per-sample gain / pan ramp (`fill_target_ramp` が毎 buffer 埋める)。
    pub gain_ramp: Vec<f32>,
    pub pan_ramp: Vec<f32>,
    /// `TapPoint::PostFx` (device 通過後・gain/pan 前) の snapshot。
    pub post_fx_l: Vec<f32>,
    pub post_fx_r: Vec<f32>,
    /// `TapPoint::PostFader` (gain/pan/mute 後) の snapshot。
    pub post_fader_l: Vec<f32>,
    pub post_fader_r: Vec<f32>,
}

impl ChainScratch {
    pub fn new(chain_id: u64) -> Self {
        Self {
            chain_id,
            gain_ramp: vec![1.0; MAX_FRAMES],
            pan_ramp: vec![0.0; MAX_FRAMES],
            post_fx_l: vec![0.0; MAX_FRAMES],
            post_fx_r: vec![0.0; MAX_FRAMES],
            post_fader_l: vec![0.0; MAX_FRAMES],
            post_fader_r: vec![0.0; MAX_FRAMES],
        }
    }
}

/// 1 track (または master) の device ツリーを展開した命令列 + その scratch。
/// `Schedule` が track index 順に 1 つずつ持つ (`track_programs`) + master 用 1 つ。
pub struct ChainProgram {
    /// 所有 track (`MASTER_TRACK_ID` = master)。automation / 変調の store を引くキー。
    pub track_id: u32,
    pub ops: Vec<ChainOp>,
    /// パラアウト (`docs/plan_paraout.md`): pass 1 で走らせる op の終端 (exclusive)。
    /// 通常は `ops.len()`。group-with-instrument では instrument prefix の終端で、
    /// `[pass1_end..]` は pass 2 (`ProcessGroupFx`) が走らせる。
    pub pass1_end: usize,
    pub parallels: Vec<ParallelScratch>,
    pub chains: Vec<ChainScratch>,
    /// 並列 PDC の delay line (`ChainOp::ChainEnd::delay` の index)。
    pub delay_lines: Vec<DelayLine>,
    /// `delay_lines` と平行な stable key (= `ParallelChain::id`、再 compile 跨ぎの状態移送)。
    pub delay_keys: Vec<u64>,
}

impl ChainProgram {
    pub fn empty(track_id: u32) -> Self {
        Self {
            track_id,
            ops: Vec::new(),
            pass1_end: 0,
            parallels: Vec::new(),
            chains: Vec::new(),
            delay_lines: Vec::new(),
            delay_keys: Vec::new(),
        }
    }

    /// 再 compile 跨ぎの状態移送 (`Schedule::adopt_state_from` と同じ契約、RT 上で
    /// 呼ばれる = ポインタ swap と f32 コピーのみ)。delay line は chain id で、chain の
    /// tap snapshot も chain id で引き継ぐ (同 track 内 chain sidechain は前 buffer の
    /// snapshot を読むので、捨てると編集のたびに 1 buffer 無音が入る)。
    pub fn adopt_state_from(&mut self, old: &mut ChainProgram) {
        for (i, key) in self.delay_keys.iter().enumerate() {
            if let Some(j) = old.delay_keys.iter().position(|k| k == key) {
                let _ = self.delay_lines[i].try_adopt(&mut old.delay_lines[j]);
            }
        }
        for cs in &mut self.chains {
            if let Some(o) = old.chains.iter().find(|o| o.chain_id == cs.chain_id) {
                cs.post_fx_l.copy_from_slice(&o.post_fx_l);
                cs.post_fx_r.copy_from_slice(&o.post_fx_r);
                cs.post_fader_l.copy_from_slice(&o.post_fader_l);
                cs.post_fader_r.copy_from_slice(&o.post_fader_r);
            }
        }
        // Parallel の走行状態 (gain match の追従値、 帯域分割のフィルタ状態) は Parallel id で。
        for rs in &mut self.parallels {
            if let Some(o) = old.parallels.iter().find(|o| o.parallel_id == rs.parallel_id) {
                rs.in_ms = o.in_ms;
                rs.out_ms = o.out_ms;
                rs.match_gain = o.match_gain;
                if let (Some(s), Some(os)) = (rs.split.as_deref_mut(), o.split.as_deref()) {
                    s.adopt_state_from(os);
                }
            }
        }
    }

}

/// [`run_chain_program`] に渡す、buffer 全体で共通の文脈。
pub struct ProgramCtx<'a> {
    pub song: Option<&'a Song>,
    pub plugin_refs: &'a PluginRefs,
    pub worker_sync: Option<&'a SyncSlot>,
    pub sample_rate: u32,
    pub frames: u32,
    pub playing: bool,
    pub current_bpm: f32,
    pub playhead_beats: f64,
    pub loop_region: LoopRegion,
    pub recording_lanes: &'a std::collections::HashSet<(u32, AutomationTarget)>,
    pub mod_plane: ModTickPlaneRef<'a>,
    pub rows: TrackRows<'a>,
    /// この pass で捕捉した所有 track の Pre-FX snapshot (`ChainOp::Plugin::own_prefx_ports`
    /// の key)。 捕捉していない pass (master / group-with-instrument の pass 1) は `None`。
    pub own_pre_fx: Option<(&'a [f32], &'a [f32])>,
}

/// `program.ops[range]` を現在のバス (`bus_l/r` + `midi_a/b`) に対して走らせる。
/// 戻り値 = この区間で MIDI バスが (note_out device か Parallel の merge で) 置換されたか。
///
/// `midi_a` が現在の MIDI バス、`midi_b` は ping-pong 用の空き。
#[allow(clippy::too_many_arguments)]
pub fn run_chain_program(
    program: &mut ChainProgram,
    range: Range<usize>,
    bus_l: &mut [f32],
    bus_r: &mut [f32],
    midi_a: &mut Vec<TimedNoteEvent>,
    midi_b: &mut Vec<TimedNoteEvent>,
    ctx: &ProgramCtx<'_>,
) -> bool {
    let n = (ctx.frames as usize).min(bus_l.len()).min(bus_r.len());
    let ChainProgram {
        track_id,
        ops,
        parallels,
        chains,
        delay_lines,
        ..
    } = program;
    let track_id = *track_id;
    let end = range.end.min(ops.len());
    let start = range.start.min(end);
    let mut midi_replaced = false;
    for op in &ops[start..end] {
        match op {
            ChainOp::Plugin { device_id, ports, own_prefx_ports } => {
                if run_plugin(
                    *device_id, *ports, *own_prefx_ports, track_id, bus_l, bus_r, midi_a, midi_b, n, ctx,
                ) {
                    midi_replaced = true;
                }
            }
            ChainOp::ParallelBegin { parallel_slot } => {
                let Some(rs) = parallels.get_mut(*parallel_slot as usize) else { continue };
                rs.in_l[..n].copy_from_slice(&bus_l[..n]);
                rs.in_r[..n].copy_from_slice(&bus_r[..n]);
                copy_midi(&mut rs.in_midi, midi_a);
                rs.sum_l[..n].fill(0.0);
                rs.sum_r[..n].fill(0.0);
                rs.merged_midi.clear();
                rs.any_midi_replaced = false;
                // r.md #112: 帯域分割。 クロスオーバーは automation / 変調 ramp の終端値で係数を組む。
                let parallel_id = rs.parallel_id;
                if let Some(split) = rs.split.as_deref_mut() {
                    let (low, high) = resolve_split_freqs(ctx, parallel_id);
                    fill_split_ramp(ctx, track_id, parallel_id, SplitEdge::LowMid, low, &mut split.low_ramp);
                    fill_split_ramp(ctx, track_id, parallel_id, SplitEdge::MidHigh, high, &mut split.high_ramp);
                    split.process(ctx.sample_rate, &rs.in_l, &rs.in_r, n);
                }
            }
            ChainOp::ChainBegin { parallel_slot, band, .. } => {
                let Some(rs) = parallels.get(*parallel_slot as usize) else { continue };
                match band.and_then(|b| rs.split.as_deref().map(|s| s.band(b))) {
                    Some((l, r)) => {
                        bus_l[..n].copy_from_slice(&l[..n]);
                        bus_r[..n].copy_from_slice(&r[..n]);
                    }
                    None => {
                        bus_l[..n].copy_from_slice(&rs.in_l[..n]);
                        bus_r[..n].copy_from_slice(&rs.in_r[..n]);
                    }
                }
                copy_midi(midi_a, &rs.in_midi);
                // ここから chain の区間。置換フラグは chain ごとに立て直す。
                midi_replaced = false;
            }
            ChainOp::ChainEnd {
                parallel_slot,
                chain_slot,
                parallel_id,
                chain_id,
                delay,
                snapshot_post_fx,
                snapshot_post_fader,
            } => {
                let (Some(rs), Some(cs)) =
                    (parallels.get_mut(*parallel_slot as usize), chains.get_mut(*chain_slot as usize))
                else {
                    continue;
                };
                // 並列 PDC: 短い chain を Parallel 内最大 latency に揃える。
                if let Some((line_idx, frames)) = delay
                    && let Some(line) = delay_lines.get_mut(*line_idx as usize)
                {
                    line.step_in_place(&mut bus_l[..n], &mut bus_r[..n], *frames as usize);
                }
                if *snapshot_post_fx {
                    cs.post_fx_l[..n].copy_from_slice(&bus_l[..n]);
                    cs.post_fx_r[..n].copy_from_slice(&bus_r[..n]);
                }
                let (gain, pan, effective_mute) =
                    resolve_chain_mixer(ctx, track_id, *parallel_id, *chain_id);
                fill_chain_ramps(ctx, track_id, *chain_id, gain, pan, cs);
                for i in 0..n {
                    let (pl, pr) = chain_pan_gains(cs.pan_ramp[i]);
                    let g = cs.gain_ramp[i];
                    let (l, r) = if effective_mute {
                        (0.0, 0.0)
                    } else {
                        (bus_l[i] * pl * g, bus_r[i] * pr * g)
                    };
                    if *snapshot_post_fader {
                        cs.post_fader_l[i] = l;
                        cs.post_fader_r[i] = r;
                    }
                    rs.sum_l[i] += l;
                    rs.sum_r[i] += r;
                }
                if midi_replaced {
                    append_midi(&mut rs.merged_midi, midi_a);
                    rs.any_midi_replaced = true;
                }
            }
            ChainOp::ParallelEnd { parallel_slot, parallel_id } => {
                let Some(rs) = parallels.get_mut(*parallel_slot as usize) else { continue };
                // 出力 trim (automation / 変調 ramp) × gain match (buffer 内で線形に追従)。
                let (out_gain, gain_match) = resolve_parallel_out(ctx, *parallel_id);
                fill_parallel_out_ramp(ctx, track_id, *parallel_id, out_gain, rs);
                let mg_from = rs.match_gain;
                let mg_to = if gain_match {
                    rs.update_gain_match(n, ctx.sample_rate)
                } else {
                    rs.in_ms = 0.0;
                    rs.out_ms = 0.0;
                    1.0
                };
                rs.match_gain = mg_to;
                let step = if n > 0 { (mg_to - mg_from) / n as f32 } else { 0.0 };
                for i in 0..n {
                    let g = rs.out_gain_ramp[i] * (mg_from + step * (i as f32 + 1.0));
                    bus_l[i] = rs.sum_l[i] * g;
                    bus_r[i] = rs.sum_r[i] * g;
                }
                if rs.any_midi_replaced {
                    rs.merged_midi.sort_unstable_by_key(|e| e.time);
                    copy_midi(midi_a, &rs.merged_midi);
                } else {
                    copy_midi(midi_a, &rs.in_midi);
                }
                // 外側の chain から見ると「この Parallel が MIDI を置換したか」。
                midi_replaced = rs.any_midi_replaced;
            }
        }
    }
    midi_replaced
}

/// chain の pan 則 (SSoT)。**中央 = unity** の balance 則: 空 chain 1 本の Parallel が
/// 素通しと同じ音量になることを保証する (track の等パワー則は中央 -3dB なので使わない)。
/// 片側へ振ると反対側だけ減衰し、boost は無い (並列合算でクリップしない)。
#[inline]
pub fn chain_pan_gains(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    if p > 0.0 { (1.0 - p, 1.0) } else { (1.0, 1.0 + p) }
}

/// `dst := src` (容量の範囲で。RT 確保なし — 両者とも `MAX_EVENTS` 容量で確保済み)。
fn copy_midi(dst: &mut Vec<TimedNoteEvent>, src: &[TimedNoteEvent]) {
    dst.clear();
    let cap = dst.capacity();
    dst.extend_from_slice(&src[..src.len().min(cap)]);
}

/// `dst += src` (容量の範囲で)。
fn append_midi(dst: &mut Vec<TimedNoteEvent>, src: &[TimedNoteEvent]) {
    let room = dst.capacity().saturating_sub(dst.len());
    dst.extend_from_slice(&src[..src.len().min(room)]);
}

/// chain の (gain, pan, effective_mute) を Song snapshot から live-read する
/// (track の M/S と同じく再 compile なしで効く)。snapshot が無ければ unity。
fn resolve_chain_mixer(
    ctx: &ProgramCtx<'_>,
    _track_id: u32,
    parallel_id: u64,
    chain_id: u64,
) -> (f32, f32, bool) {
    let Some(song) = ctx.song else {
        return (1.0, 0.0, false);
    };
    let Some(parallel) = song.parallel_by_id(parallel_id) else {
        return (1.0, 0.0, false);
    };
    let Some(chain) = parallel.chains.iter().find(|c| c.id == chain_id) else {
        return (1.0, 0.0, false);
    };
    let any_solo = parallel.chains.iter().any(|c| c.solo);
    let effective_mute = chain.muted || (any_solo && !chain.solo);
    (chain.gain, chain.pan, effective_mute)
}

/// Parallel の (out_gain, gain_match) を Song snapshot から live-read する。無ければ unity / off。
fn resolve_parallel_out(ctx: &ProgramCtx<'_>, parallel_id: u64) -> (f32, bool) {
    ctx.song
        .and_then(|s| s.parallel_by_id(parallel_id))
        .map_or((1.0, false), |r| (r.out_gain, r.gain_match))
}

/// r.md #112: Parallel のクロスオーバー (low, high) を Song snapshot から live-read する。
/// snapshot に無い / `Frequency3` でなければ既定値 (compile 時に split が付いた Parallel だけが
/// ここへ来る)。
fn resolve_split_freqs(ctx: &ProgramCtx<'_>, parallel_id: u64) -> (f32, f32) {
    ctx.song
        .and_then(|s| s.parallel_by_id(parallel_id))
        .map_or(common::model::Split::DEFAULT_FREQS, |r| r.split.freqs_or_default())
}

/// クロスオーバー周波数の ramp を埋める (`fill_parallel_out_ramp` と同じ経路)。
fn fill_split_ramp(
    ctx: &ProgramCtx<'_>,
    track_id: u32,
    parallel_id: u64,
    edge: SplitEdge,
    hz: f32,
    buf: &mut [f32],
) {
    let n = (ctx.frames as usize).min(buf.len());
    let Some(song) = ctx.song else {
        buf[..n].fill(hz);
        return;
    };
    let (lanes, routings) = track_stores(song, track_id);
    crate::automation::fill_target_ramp(
        song,
        track_id,
        lanes,
        routings,
        ctx.rows,
        ctx.sample_rate,
        f64::from(ctx.current_bpm),
        ctx.playhead_beats,
        ctx.frames,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::ParallelSplitFreq { parallel_id, edge }),
        hz,
        buf,
        ctx.recording_lanes,
        ctx.mod_plane,
    );
}

/// 所有 track の lane / routing store (master は song 側)。
fn track_stores(
    song: &Song,
    track_id: u32,
) -> (&[common::model::AutomationLane], &[common::model::ModRouting]) {
    if track_id == common::model::MASTER_TRACK_ID {
        (&song.song_lanes, &song.song_mod_routings)
    } else {
        match song.track_by_id(track_id) {
            Some(t) => (&t.automation_lanes, &t.mod_routings),
            None => (&[], &[]),
        }
    }
}

/// Parallel の出力 trim ramp を埋める (`fill_chain_ramps` と同じ経路)。
fn fill_parallel_out_ramp(
    ctx: &ProgramCtx<'_>,
    track_id: u32,
    parallel_id: u64,
    out_gain: f32,
    rs: &mut ParallelScratch,
) {
    let n = (ctx.frames as usize).min(MAX_FRAMES);
    let Some(song) = ctx.song else {
        rs.out_gain_ramp[..n].fill(out_gain);
        return;
    };
    let (lanes, routings) = track_stores(song, track_id);
    crate::automation::fill_target_ramp(
        song,
        track_id,
        lanes,
        routings,
        ctx.rows,
        ctx.sample_rate,
        f64::from(ctx.current_bpm),
        ctx.playhead_beats,
        ctx.frames,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::ParallelOutGain { parallel_id }),
        out_gain,
        &mut rs.out_gain_ramp,
        ctx.recording_lanes,
        ctx.mod_plane,
    );
}

/// chain の gain / pan ramp を埋める (automation lane + 変調、`SendGain` と同じ
/// per-sample 経路)。lane / routing の store は所有 track (master は song 側)。
fn fill_chain_ramps(
    ctx: &ProgramCtx<'_>,
    track_id: u32,
    chain_id: u64,
    gain: f32,
    pan: f32,
    cs: &mut ChainScratch,
) {
    let n = (ctx.frames as usize).min(MAX_FRAMES);
    let Some(song) = ctx.song else {
        cs.gain_ramp[..n].fill(gain);
        cs.pan_ramp[..n].fill(pan);
        return;
    };
    let (lanes, routings) = track_stores(song, track_id);
    crate::automation::fill_target_ramp(
        song,
        track_id,
        lanes,
        routings,
        ctx.rows,
        ctx.sample_rate,
        f64::from(ctx.current_bpm),
        ctx.playhead_beats,
        ctx.frames,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id }),
        gain,
        &mut cs.gain_ramp,
        ctx.recording_lanes,
        ctx.mod_plane,
    );
    crate::automation::fill_target_ramp(
        song,
        track_id,
        lanes,
        routings,
        ctx.rows,
        ctx.sample_rate,
        f64::from(ctx.current_bpm),
        ctx.playhead_beats,
        ctx.frames,
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainPan { chain_id }),
        pan,
        &mut cs.pan_ramp,
        ctx.recording_lanes,
        ctx.mod_plane,
    );
}

/// 1 plugin を現在のバスへ dispatch する (v23 の port 直結規則)。戻り値 = note_out で
/// MIDI バスを置換したか。
#[allow(clippy::too_many_arguments)]
fn run_plugin(
    device_id: u64,
    ports: PortConfig,
    own_prefx_ports: u8,
    track_id: u32,
    bus_l: &mut [f32],
    bus_r: &mut [f32],
    midi_a: &mut Vec<TimedNoteEvent>,
    midi_b: &mut Vec<TimedNoteEvent>,
    n: usize,
    ctx: &ProgramCtx<'_>,
) -> bool {
    let Some(entry) = ctx.plugin_refs.get(&device_id) else {
        return false;
    };
    let Some(ws) = ctx.worker_sync else {
        return false;
    };
    // quarantine / poison gate — 通らない device は pd にも触らない
    // (並行 process との race 回避、`execute.rs` 冒頭の contract 参照)。
    if !super::execute::pair_usable(ws, entry) {
        return false;
    }
    let pd = entry.plugin_ref.data_mut();
    pd.prepare();
    pd.frames = ctx.frames;
    pd.playing = if ctx.playing { 1 } else { 0 };
    pd.sample_rate = ctx.sample_rate;
    super::execute::set_pd_transport(
        pd,
        ctx.song,
        ctx.current_bpm,
        ctx.playhead_beats,
        ctx.loop_region,
        ctx.rows.track(),
    );
    // ---- inputs: device の port を持つものだけ現在のバスを渡す ----
    // M1 (r.md #8): note を param automation より **先に** push する (events_in の
    // 溢れで NoteOff を落とさない)。
    if ports.has_note_input {
        for ev in midi_a.iter() {
            match ev.event {
                NoteTransition::On { note_id, key, velocity } => {
                    pd.push_note_on(ev.time, key, velocity, 0, note_id)
                }
                NoteTransition::Off { note_id, key } => pd.push_note_off(ev.time, key, 0, note_id),
            }
        }
    }
    if let Some(song) = ctx.song {
        crate::automation::fill_pd_param_events(
            pd,
            song,
            track_id,
            ctx.rows,
            device_id,
            ctx.sample_rate,
            f64::from(ctx.current_bpm),
            ctx.playhead_beats,
            ctx.frames,
            ctx.recording_lanes,
            ctx.mod_plane,
        );
    }
    if ports.has_audio_input {
        pd.buffer_in[0][..n].copy_from_slice(&bus_l[..n]);
        pd.buffer_in[1][..n].copy_from_slice(&bus_r[..n]);
    }
    // 自 track の Pre-FX を key にする aux port: この pass の snapshot を直接載せる。
    if own_prefx_ports != 0 && let Some((pl, pr)) = ctx.own_pre_fx {
        let copy_n = n.min(pl.len()).min(pr.len());
        for port in 0..common::process_data::MAX_AUX_IN.min(8) {
            if own_prefx_ports & (1 << port) == 0 {
                continue;
            }
            pd.buffer_aux_in[port][0][..copy_n].copy_from_slice(&pl[..copy_n]);
            pd.buffer_aux_in[port][1][..copy_n].copy_from_slice(&pr[..copy_n]);
            pd.aux_in_active[port] = 1;
        }
    }
    if !super::execute::dispatch_bounded(ws, entry) {
        return false;
    }
    // ---- outputs ----
    let mut replaced = false;
    if ports.has_note_output {
        midi_b.clear();
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
                EventKind::ParamValue => continue,
            };
            if midi_b.len() < midi_b.capacity() {
                midi_b.push(timed);
            }
        }
        midi_b.sort_unstable_by_key(|e| e.time);
        std::mem::swap(midi_a, midi_b);
        replaced = true;
    }
    if ports.has_audio_output {
        if ports.has_audio_input {
            bus_l[..n].copy_from_slice(&pd.buffer_out[0][..n]);
            bus_r[..n].copy_from_slice(&pd.buffer_out[1][..n]);
        } else {
            for j in 0..n {
                bus_l[j] += pd.buffer_out[0][j];
                bus_r[j] += pd.buffer_out[1][j];
            }
        }
    }
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build_program;
    use crate::graph::compile::DeviceLatencies;
    use common::model::{Device, Parallel, ParallelChain, Track};
    use std::collections::HashSet;

    fn parallel(id: u64, chains: Vec<ParallelChain>) -> Device {
        Device::Parallel(Parallel {
            id,
            name: "Parallel".into(),
            chains,
            bypassed: false,
            color: None,
            out_gain: 1.0,
            gain_match: false,
            split: common::model::Split::None,
        })
    }

    fn chain(id: u64, devices: Vec<Device>) -> ParallelChain {
        ParallelChain {
            id,
            devices,
            ..ParallelChain::new("c")
        }
    }

    fn song_with(devices: Vec<Device>) -> Song {
        let t = Track { id: 1, devices, ..Track::default() };
        Song {
            tracks: vec![t],
            ..Song::default()
        }
    }

    /// plugin 無しで program を走らせる (dispatch 先が無いので `Plugin` op は素通し)。
    fn run(song: &Song, frames: u32, bus: &mut (Vec<f32>, Vec<f32>), midi: &mut Vec<TimedNoteEvent>) -> ChainProgram {
        let built = build_program(
            &song.tracks[0].devices,
            1,
            None,
            &DeviceLatencies::new(),
            &HashSet::new(),
        );
        let mut program = built.program;
        let refs: PluginRefs = std::collections::HashMap::new();
        let lanes = HashSet::new();
        let ctx = ProgramCtx {
            song: Some(song),
            plugin_refs: &refs,
            worker_sync: None,
            sample_rate: 48_000,
            frames,
            playing: true,
            current_bpm: 120.0,
            playhead_beats: 0.0,
            loop_region: LoopRegion::default(),
            recording_lanes: &lanes,
            mod_plane: ModTickPlaneRef::default(),
            own_pre_fx: None,
            rows: TrackRows::default(),
        };
        let mut midi_b = Vec::with_capacity(MAX_EVENTS);
        let len = program.ops.len();
        run_chain_program(&mut program, 0..len, &mut bus.0, &mut bus.1, midi, &mut midi_b, &ctx);
        program
    }

    fn ramp(n: usize) -> (Vec<f32>, Vec<f32>) {
        ((0..n).map(|i| i as f32).collect(), (0..n).map(|i| -(i as f32)).collect())
    }

    #[test]
    fn two_empty_chains_sum_to_twice_the_input() {
        let song = song_with(vec![parallel(10, vec![chain(11, vec![]), chain(12, vec![])])]);
        let mut bus = ramp(8);
        let mut midi = Vec::with_capacity(MAX_EVENTS);
        run(&song, 8, &mut bus, &mut midi);
        assert_eq!(bus.0, (0..8).map(|i| 2.0 * i as f32).collect::<Vec<_>>());
        assert_eq!(bus.1, (0..8).map(|i| -2.0 * i as f32).collect::<Vec<_>>());
    }

    /// r.md #112: `Frequency3` の空 chain 3 本は帯域の和 = 入力 (振幅平坦)。 4 本目の空 chain を
    /// 足すと全帯域が 1 回余分に足されて 2x になる (= 4 本目は素通し入力を受けている)。
    #[test]
    fn frequency_split_with_three_empty_chains_is_transparent_and_a_fourth_chain_is_full_band() {
        fn steady_rms(song: &Song, f: f32) -> f32 {
            let sr = 48_000u32;
            let n = 960usize; // 20 ms: 50 / 700 / 8000 Hz の周期が整数個乗る
            let built = build_program(&song.tracks[0].devices, 1, None, &DeviceLatencies::new(), &HashSet::new());
            let mut program = built.program;
            let refs: PluginRefs = std::collections::HashMap::new();
            let lanes = HashSet::new();
            let ctx = ProgramCtx {
                song: Some(song),
                plugin_refs: &refs,
                worker_sync: None,
                sample_rate: sr,
                frames: n as u32,
                playing: true,
                current_bpm: 120.0,
                playhead_beats: 0.0,
                loop_region: LoopRegion::default(),
                recording_lanes: &lanes,
                mod_plane: ModTickPlaneRef::default(),
                own_pre_fx: None,
                rows: TrackRows::default(),
            };
            let mut midi = Vec::with_capacity(MAX_EVENTS);
            let mut midi_b = Vec::with_capacity(MAX_EVENTS);
            let mut last = 0.0f32;
            for b in 0..(sr as usize / n) {
                let mut l: Vec<f32> = (0..n)
                    .map(|i| (std::f32::consts::TAU * f * ((b * n + i) as f32) / sr as f32).sin())
                    .collect();
                let mut r = l.clone();
                let len = program.ops.len();
                run_chain_program(&mut program, 0..len, &mut l, &mut r, &mut midi, &mut midi_b, &ctx);
                last = (l.iter().map(|x| x * x).sum::<f32>() / n as f32).sqrt();
            }
            last
        }
        let in_rms = std::f32::consts::FRAC_1_SQRT_2;
        let mut song = song_with(vec![parallel(10, vec![chain(11, vec![]), chain(12, vec![]), chain(13, vec![])])]);
        song.tracks[0].devices[0].as_parallel_mut().unwrap().split = common::model::Split::DEFAULT_FREQUENCY3;
        for f in [50.0f32, 700.0, 8_000.0] {
            let db = 20.0 * (steady_rms(&song, f) / in_rms).log10();
            assert!(db.abs() < 0.05, "f={f}: {db:.3} dB");
        }
        song.tracks[0].devices[0].as_parallel_mut().unwrap().chains.push(chain(14, vec![]));
        let db = 20.0 * (steady_rms(&song, 700.0) / in_rms).log10();
        assert!((db - 6.02).abs() < 0.1, "4 本目は全帯域 (+6 dB): {db:.3} dB");
    }

    /// gain match: 空 chain 2 本 (和 = 2x) でも、 一定振幅を数秒流せば出力は入力と同じ
    /// レベルに戻る。 off なら 2x のまま。 out_gain は match の後に掛かる。
    #[test]
    fn gain_match_brings_a_two_chain_sum_back_to_the_input_level() {
        let mut song = song_with(vec![parallel(10, vec![chain(11, vec![]), chain(12, vec![])])]);
        song.tracks[0].devices[0].as_parallel_mut().unwrap().gain_match = true;
        // off 側の snapshot (後半で使う): out_gain 0.5、match off。
        let mut song_off = song.clone();
        {
            let r = song_off.tracks[0].devices[0].as_parallel_mut().unwrap();
            r.gain_match = false;
            r.out_gain = 0.5;
        }
        let frames = 256usize;
        let mut midi = Vec::with_capacity(MAX_EVENTS);
        // 同じ program を buffer 跨ぎで回すため `run` ではなく手で組む (`run` は毎回 build する)。
        let built = build_program(&song.tracks[0].devices, 1, None, &DeviceLatencies::new(), &HashSet::new());
        let mut program = built.program;
        let refs: PluginRefs = std::collections::HashMap::new();
        let lanes = HashSet::new();
        let ctx = ProgramCtx {
            song: Some(&song),
            plugin_refs: &refs,
            worker_sync: None,
            sample_rate: 48_000,
            frames: frames as u32,
            playing: true,
            current_bpm: 120.0,
            playhead_beats: 0.0,
            loop_region: LoopRegion::default(),
            recording_lanes: &lanes,
            mod_plane: ModTickPlaneRef::default(),
            rows: TrackRows::default(),
            own_pre_fx: None,
        };
        let mut last = 0.0f32;
        // 3 秒ぶん (時定数 0.5 s の 6 倍) 流す。
        for _ in 0..(48_000 * 3 / frames) {
            let mut l = vec![0.5f32; frames];
            let mut r = vec![0.5f32; frames];
            let len = program.ops.len();
            run_chain_program(&mut program, 0..len, &mut l, &mut r, &mut midi, &mut Vec::with_capacity(MAX_EVENTS), &ctx);
            last = l[frames - 1];
        }
        assert!((last - 0.5).abs() < 0.01, "match 後の出力 = 入力 (0.5): {last}");

        // off に戻すと 2x へ戻る (数 buffer で追従)。 out_gain 0.5 は match の後に掛かる。
        let ctx = ProgramCtx { song: Some(&song_off), ..ctx };
        let mut last = 0.0f32;
        for _ in 0..8 {
            let mut l = vec![0.5f32; frames];
            let mut r = vec![0.5f32; frames];
            let len = program.ops.len();
            run_chain_program(&mut program, 0..len, &mut l, &mut r, &mut midi, &mut Vec::with_capacity(MAX_EVENTS), &ctx);
            last = l[frames - 1];
        }
        assert!((last - 0.5).abs() < 1e-4, "off: 2x × out_gain 0.5 = 0.5: {last}");
    }

    #[test]
    fn a_parallel_with_no_chains_passes_the_input_through() {
        let song = song_with(vec![parallel(10, vec![])]);
        let mut bus = ramp(8);
        let want = bus.clone();
        let mut midi = Vec::with_capacity(MAX_EVENTS);
        let program = run(&song, 8, &mut bus, &mut midi);
        assert!(program.ops.is_empty(), "op を出さない: {:?}", program.ops.len());
        assert_eq!(bus, want);
    }

    #[test]
    fn chain_gain_pan_mute_and_solo_apply_per_chain() {
        let mut c1 = chain(11, vec![]);
        c1.gain = 0.5;
        let mut c2 = chain(12, vec![]);
        c2.muted = true;
        let mut c3 = chain(13, vec![]);
        c3.pan = -1.0; // 全部 L
        let song = song_with(vec![parallel(10, vec![c1, c2, c3])]);
        let mut bus = (vec![1.0; 4], vec![1.0; 4]);
        let mut midi = Vec::with_capacity(MAX_EVENTS);
        run(&song, 4, &mut bus, &mut midi);
        let (pl, pr) = chain_pan_gains(-1.0);
        // c1: 0.5 (中央 = unity) / c2: 0 / c3: pan hard L (L=1, R=0)。
        let want_l = 0.5 + pl;
        let want_r = 0.5 + pr;
        for i in 0..4 {
            assert!((bus.0[i] - want_l).abs() < 1e-6, "L[{i}] = {}", bus.0[i]);
            assert!((bus.1[i] - want_r).abs() < 1e-6, "R[{i}] = {}", bus.1[i]);
        }

        // solo: c3 だけ鳴る。
        let mut song2 = song;
        let r = song2.tracks[0].devices[0].as_parallel_mut().unwrap();
        r.chains[2].solo = true;
        r.chains[1].muted = false;
        let mut bus = (vec![1.0; 4], vec![1.0; 4]);
        run(&song2, 4, &mut bus, &mut midi);
        assert!((bus.0[0] - pl).abs() < 1e-6);
        assert!((bus.1[0] - pr).abs() < 1e-6);
    }

    #[test]
    fn nested_parallel_sums_inner_chains_into_outer_chain() {
        // outer: [inner parallel (2 空 chain) , 空 chain] → 入力 x は inner で 2x、外側で 2x + x = 3x。
        let inner = parallel(20, vec![chain(21, vec![]), chain(22, vec![])]);
        let song = song_with(vec![parallel(10, vec![chain(11, vec![inner]), chain(12, vec![])])]);
        let mut bus = (vec![1.0; 4], vec![2.0; 4]);
        let mut midi = Vec::with_capacity(MAX_EVENTS);
        run(&song, 4, &mut bus, &mut midi);
        assert_eq!(bus.0, vec![3.0; 4]);
        assert_eq!(bus.1, vec![6.0; 4]);
    }

    #[test]
    fn midi_passes_through_a_parallel_without_note_out_devices() {
        let song = song_with(vec![parallel(10, vec![chain(11, vec![]), chain(12, vec![])])]);
        let mut bus = (vec![0.0; 4], vec![0.0; 4]);
        let mut midi = Vec::with_capacity(MAX_EVENTS);
        midi.push(TimedNoteEvent {
            time: 3,
            event: NoteTransition::On { note_id: 1, key: 60, velocity: 1.0 },
        });
        run(&song, 4, &mut bus, &mut midi);
        // 置換した chain が無いので入力 MIDI が **1 回だけ** 素通し (2 倍にならない)。
        assert_eq!(midi.len(), 1);
        assert_eq!(midi[0].time, 3);
    }

    #[test]
    fn parallel_pdc_aligns_the_shorter_chain() {
        // chain 11 が 4 sample 遅い device を持つ (報告値のみ、実 device は無い) →
        // chain 12 (空) に 4 sample の delay が入る。impulse を入れて sum を見る。
        let mut lat = DeviceLatencies::new();
        lat.insert(5, 4);
        let latent = Device::Plugin(common::model::PluginInstance {
            id: 5,
            ..common::model::PluginInstance::new("latent".into(), common::plugin_format::PluginFormat::Clap)
        });
        let song = song_with(vec![parallel(10, vec![chain(11, vec![latent]), chain(12, vec![])])]);
        let built = build_program(&song.tracks[0].devices, 1, None, &lat, &HashSet::new());
        let mut program = built.program;
        let refs: PluginRefs = std::collections::HashMap::new();
        let lanes = HashSet::new();
        let ctx = ProgramCtx {
            song: Some(&song),
            plugin_refs: &refs,
            worker_sync: None,
            sample_rate: 48_000,
            frames: 8,
            playing: true,
            current_bpm: 120.0,
            playhead_beats: 0.0,
            loop_region: LoopRegion::default(),
            recording_lanes: &lanes,
            mod_plane: ModTickPlaneRef::default(),
            own_pre_fx: None,
            rows: TrackRows::default(),
        };
        let mut l = vec![0.0; 8];
        l[0] = 1.0;
        let mut r = l.clone();
        let mut a = Vec::with_capacity(MAX_EVENTS);
        let mut b = Vec::with_capacity(MAX_EVENTS);
        let len = program.ops.len();
        run_chain_program(&mut program, 0..len, &mut l, &mut r, &mut a, &mut b, &ctx);
        // chain 11: plugin は dispatch されない (素通し) → impulse @0。
        // chain 12: 4 sample 遅延 → impulse @4。合算 = [1,0,0,0,1,0,0,0]。
        assert_eq!(l, vec![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
        assert_eq!(r, l);
    }
}
