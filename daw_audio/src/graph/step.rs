//! 直列トレースの 1 手 ([`Step`]) の実行と、その資源の引き口 ([`RenderCtx`])
//! (`docs/plan_parallel_graph.md` §3)。
//!
//! 直列実行 (pool 無し / 書き出しで rig 無し) も worker pool の並列実行も、手の実行は [`run_step`] 1 本。
//! 並列実行では複数の runner が同じ `RenderCtx` を共有し、各手は **自分が読み書きする資源だけ** を引く。
//! 同じ資源に触る手は [`RenderGraph`] の辺で直列化されているので、ある手が引いた参照が生きている間に
//! 別の runner が同じ資源へ書くことはない (これが下の `unsafe` 引き口の安全性の根拠)。
//!
//! RT 規約: 確保・ロック・I/O なし。

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::atomic::Ordering;

use common::model::{AutomationTarget, LoopRegion, Song};
use common::mod_plane::ModTickPlaneRef;

use crate::audio_clip_renderer::AudioClipRenderer;
use crate::engine::{PluginRefs, SyncSlot};
use crate::graph::execute::{advance_follower, process_track_owned, run_group_fx_chain};
use crate::graph::mix::{mix_into, mix_into_master, mix_send_into, program_tap_owner, resolve_program_tap, resolve_tap};
use crate::graph::native::{NativeIo, stage_into};
use crate::graph::render_graph::{RenderGraph, Step};
use crate::graph::schedule::{MASTER_OWNER, SoloTables};
use crate::graph::{BufRef, ChainProgram, DelayLine, FollowerSlot, NodeOp, Schedule};
use crate::launcher::RowSourceTable;
use crate::mixer::TrackScratch;
use crate::mod_tick::FollowerDrive;

/// 1 buffer の間不変な描画の条件 (live と書き出しが同じ形で渡す)。
#[derive(Clone, Copy)]
pub struct BufferParams<'a> {
    pub sample_rate: u32,
    pub frames: u32,
    pub playing: bool,
    pub any_solo: bool,
    pub recording_lanes: &'a HashSet<(u32, AutomationTarget)>,
    pub current_bpm: f32,
    pub playhead_beats: f64,
    pub loop_region: LoopRegion,
    pub mod_plane: ModTickPlaneRef<'a>,
    pub follower_drive: FollowerDrive<'a>,
    pub rows: &'a RowSourceTable,
    pub native_io: NativeIo<'a>,
}

/// 1 buffer の描画の文脈。**dispatch 窓の間だけ** 生きる (callback スレッドのスタック上に作り、並列実行では
/// pool の worker へポインタで渡す)。可変な資源は生ポインタで持ち、手ごとに `unsafe` 引き口で借りる。
pub struct RenderCtx<'a> {
    pub song: &'a Song,
    scratch: *mut TrackScratch,
    n_scratch: usize,
    programs: *mut ChainProgram,
    n_programs: usize,
    master_program: *mut ChainProgram,
    master_l: *mut f32,
    master_r: *mut f32,
    n: usize,
    delay_lines: *mut DelayLine,
    n_delay_lines: usize,
    follower_slots: *mut FollowerSlot,
    n_followers: usize,
    nodes: &'a [NodeOp],
    input_delays: &'a [u32],
    solo: &'a SoloTables,
    pub graph: &'a RenderGraph,
    pub slots: &'a [SyncSlot],
    plugin_refs: &'a PluginRefs,
    audio_renderer: Option<&'a AudioClipRenderer>,
    pub params: BufferParams<'a>,
    _borrow: PhantomData<&'a mut Schedule>,
}

// SAFETY: 可変な資源は手ごとに排他 (module doc)。共有参照の中身 (Song / plugin_refs / 変調面 …) は buffer の間不変。
unsafe impl Send for RenderCtx<'_> {}
unsafe impl Sync for RenderCtx<'_> {}

impl<'a> RenderCtx<'a> {
    /// `schedule` / `scratch` / master バスを 1 buffer の間借りる。`master_l` / `master_r` は `params.frames` 以上。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        song: &'a Song,
        schedule: &'a mut Schedule,
        scratch: &'a mut [TrackScratch],
        master_l: &'a mut [f32],
        master_r: &'a mut [f32],
        plugin_refs: &'a PluginRefs,
        audio_renderer: Option<&'a AudioClipRenderer>,
        slots: &'a [SyncSlot],
        params: BufferParams<'a>,
    ) -> Self {
        let n = (params.frames as usize).min(master_l.len()).min(master_r.len());
        let Schedule {
            nodes, delay_lines, follower_slots, track_programs, master_program, input_delay_per_track, graph, solo, ..
        } = schedule;
        Self {
            song,
            n_scratch: scratch.len(),
            scratch: scratch.as_mut_ptr(),
            n_programs: track_programs.len(),
            programs: track_programs.as_mut_ptr(),
            master_program: std::ptr::from_mut(master_program),
            master_l: master_l.as_mut_ptr(),
            master_r: master_r.as_mut_ptr(),
            n,
            n_delay_lines: delay_lines.len(),
            delay_lines: delay_lines.as_mut_ptr(),
            n_followers: follower_slots.len(),
            follower_slots: follower_slots.as_mut_ptr(),
            nodes,
            input_delays: input_delay_per_track,
            solo,
            graph,
            slots,
            plugin_refs,
            audio_renderer,
            params: BufferParams { frames: n as u32, ..params },
            _borrow: PhantomData,
        }
    }

    // ---- 資源の引き口 ----
    // SAFETY (全部共通): 呼ぶ手が `render_graph::Resources::access` の表でその資源を書く (`_mut`) /
    // 読む手であること。同じ資源に触る手は辺で直列化されるので、返した参照が生きている間に別の runner が
    // 同じ資源へ書くことはない。

    unsafe fn scratch_mut(&self, i: u32) -> Option<&'a mut TrackScratch> {
        ((i as usize) < self.n_scratch).then(|| unsafe { &mut *self.scratch.add(i as usize) })
    }

    unsafe fn scratch(&self, i: u32) -> Option<&'a TrackScratch> {
        ((i as usize) < self.n_scratch).then(|| unsafe { &*self.scratch.add(i as usize) })
    }

    unsafe fn program_mut(&self, owner: u32) -> Option<&'a mut ChainProgram> {
        if owner == MASTER_OWNER {
            return Some(unsafe { &mut *self.master_program });
        }
        ((owner as usize) < self.n_programs).then(|| unsafe { &mut *self.programs.add(owner as usize) })
    }

    unsafe fn program(&self, owner: u32) -> Option<&'a ChainProgram> {
        if owner == MASTER_OWNER {
            return Some(unsafe { &*self.master_program });
        }
        ((owner as usize) < self.n_programs).then(|| unsafe { &*self.programs.add(owner as usize) })
    }

    unsafe fn master_mut(&self) -> (&'a mut [f32], &'a mut [f32]) {
        unsafe { (std::slice::from_raw_parts_mut(self.master_l, self.n), std::slice::from_raw_parts_mut(self.master_r, self.n)) }
    }

    /// delay line / follower は 1 つの手だけが持つ (資源にしない)。
    unsafe fn delay_line_mut(&self, k: u32) -> Option<&'a mut DelayLine> {
        ((k as usize) < self.n_delay_lines).then(|| unsafe { &mut *self.delay_lines.add(k as usize) })
    }

    unsafe fn follower_mut(&self, k: u32) -> Option<&'a mut FollowerSlot> {
        ((k as usize) < self.n_followers).then(|| unsafe { &mut *self.follower_slots.add(k as usize) })
    }

    /// tap の読み元 (scratch 系か program 系)。
    unsafe fn tap(&self, src: BufRef) -> Option<(&'a [f32], &'a [f32])> {
        unsafe { resolve_tap(src, |i| self.scratch(i), |o| self.program(o)) }
    }
}

/// 手 `step` を runner `slot` (callback スレッド = 0、worker i = i + 1) で実行する。
pub fn run_step(ctx: &RenderCtx<'_>, step: Step, slot: usize) {
    let sync = ctx.slots.get(slot);
    let p = ctx.params;
    // SAFETY: 各分岐は `Resources::access` の表どおりの資源だけを引く (module doc)。
    unsafe {
        match step {
            Step::Process(i) => {
                let (Some(track), Some(scratch), Some(program)) =
                    (ctx.song.tracks.get(i as usize), ctx.scratch_mut(i), ctx.program_mut(i))
                else {
                    return;
                };
                let input_delay = ctx.input_delays.get(i as usize).copied().unwrap_or(0);
                process_track_owned(
                    i,
                    track,
                    scratch,
                    program,
                    ctx.plugin_refs,
                    ctx.audio_renderer,
                    sync,
                    p.sample_rate,
                    p.frames,
                    p.playing,
                    Some(ctx.song),
                    p.any_solo,
                    ctx.solo.of(i).1,
                    input_delay,
                    p.recording_lanes,
                    p.current_bpm,
                    p.playhead_beats,
                    p.loop_region,
                    p.mod_plane,
                    p.rows.track_rows(i as usize),
                    p.native_io,
                );
            }
            Step::Node(k) => {
                if let Some(op) = ctx.nodes.get(k as usize) {
                    run_node(ctx, op, sync);
                }
            }
        }
    }
}

/// `Schedule::nodes` の 1 op (旧 `execute_schedule_post_dispatch` の 1 arm)。
///
/// SAFETY: [`run_step`] と同じ (op ごとの資源は `Resources::access` の表)。
unsafe fn run_node(ctx: &RenderCtx<'_>, op: &NodeOp, sync: Option<&SyncSlot>) {
    let (n, p, song) = (ctx.n, ctx.params, ctx.song);
    unsafe {
        match op {
            // pass 1 の手 (`Step::Process`) が担う。
            NodeOp::ProcessTrack { .. } => {}
            // clearing `Mix` (group / return の入力) と、パラアウト楽器兼バスの `MixAdditive` (pass 1 の
            // prefix が置いた自分の main の上に子を足す)。
            NodeOp::Mix { srcs, dst: BufRef::TrackScratch(t) } | NodeOp::MixAdditive { srcs, dst: BufRef::TrackScratch(t) } => {
                let clear = matches!(op, NodeOp::Mix { .. });
                if let Some(target) = ctx.scratch_mut(*t) {
                    mix_into(target, *t, srcs, n, clear, |s| ctx.scratch(s));
                }
            }
            NodeOp::Mix { srcs, dst: BufRef::Master } => {
                let (l, r) = ctx.master_mut();
                mix_into_master(srcs, l, r, n, |s| ctx.scratch(s));
            }
            // Pooled / Pre* / chain 系の dst は emit されない (match を網羅するための arm)。
            NodeOp::Mix { .. } | NodeOp::MixAdditive { .. } => {}
            NodeOp::ProcessGroupFx { track_idx, start_op } => {
                let (Some(track), Some(target), Some(program)) =
                    (song.tracks.get(*track_idx as usize), ctx.scratch_mut(*track_idx), ctx.program_mut(*track_idx))
                else {
                    return;
                };
                run_group_fx_chain(
                    *track_idx,
                    track,
                    song,
                    target,
                    program,
                    ctx.plugin_refs,
                    sync,
                    p.sample_rate,
                    p.frames,
                    p.playing,
                    p.any_solo,
                    ctx.solo.of(*track_idx),
                    p.recording_lanes,
                    p.current_bpm,
                    p.playhead_beats,
                    p.loop_region,
                    p.mod_plane,
                    *start_op as usize,
                    p.rows.track_rows(*track_idx as usize),
                    p.native_io,
                );
            }
            // PR3: `buf` の scratch を in-place で `delay_frames` だけ遅らせる (compile は小さい path latency の側の
            // `TrackScratch` だけを指す)。
            NodeOp::ApplyDelay { buf: BufRef::TrackScratch(i), line_idx, frames: delay } => {
                let (Some(s), Some(line)) = (ctx.scratch_mut(*i), ctx.delay_line_mut(*line_idx)) else {
                    return;
                };
                let n = n.min(s.track_l.len()).min(s.track_r.len());
                line.step_in_place(&mut s.track_l[..n], &mut s.track_r[..n], *delay as usize);
            }
            NodeOp::ApplyDelay { .. } => {}
            // PR4 sidechain: 読み元 (PostFader / PostFx / PreFx / chain) を plugin `device_id` の
            // `buffer_aux_in[port]` へ写し、次の `process()` で aux bus として渡させる。quarantine 中の device の
            // pd には触らない (process が走ったままの可能性がある — poisoning contract)。
            NodeOp::SidechainTap { src, device_id, aux_in_port } => {
                let port = *aux_in_port as usize;
                if port >= common::process_data::MAX_AUX_IN {
                    return;
                }
                let (Some((src_l, src_r)), Some(entry)) = (ctx.tap(*src), ctx.plugin_refs.get(device_id)) else {
                    return;
                };
                if entry.quarantined.load(Ordering::Acquire) {
                    return;
                }
                let pd = entry.plugin_ref.data_mut();
                let copy_n = n.min(src_l.len()).min(src_r.len());
                pd.buffer_aux_in[port][0][..copy_n].copy_from_slice(&src_l[..copy_n]);
                pd.buffer_aux_in[port][1][..copy_n].copy_from_slice(&src_r[..copy_n]);
                pd.aux_in_active[port] = 1;
            }
            // r.md #129: 内蔵 device (Comp / Bus Comp) の外部サイドチェインの staging。
            NodeOp::NativeSidechainTap { src, owner, native_slot } => {
                stage_native_sidechain(ctx, *src, *owner, *native_slot, n);
            }
            // パラアウト (docs/plan_paraout.md): source plugin の aux 出力 `port` (pass 1 の process が書いた
            // `buffer_aux_out`) を dst track の入力 scratch へ足す。宣言して書いた port だけが active。
            NodeOp::ParallelOutTap { device_id, port, dst_track } => {
                let port = *port as usize;
                if port >= common::process_data::MAX_AUX_OUT {
                    return;
                }
                let (Some(entry), Some(target)) = (ctx.plugin_refs.get(device_id), ctx.scratch_mut(*dst_track)) else {
                    return;
                };
                if entry.quarantined.load(Ordering::Acquire) {
                    return;
                }
                let pd = entry.plugin_ref.data();
                if pd.aux_out_active[port] == 0 {
                    return;
                }
                let copy_n = n.min(target.track_l.len()).min(target.track_r.len());
                for i in 0..copy_n {
                    target.track_l[i] += pd.buffer_aux_out[port][0][i];
                    target.track_r[i] += pd.buffer_aux_out[port][1][i];
                }
            }
            // PR4 aux send: 送り元の post / pre fader を、live (オートメーション込み) の send gain で return /
            // bus の scratch へ足す。
            NodeOp::MixSend { src, dst: BufRef::TrackScratch(dst_idx), src_track_idx, send_id } => {
                let (src_idx, pre_fader) = match *src {
                    BufRef::TrackScratch(i) => (i, false),
                    BufRef::PreFaderScratch(i) => (i, true),
                    _ => return,
                };
                if src_idx == *dst_idx {
                    return;
                }
                let (Some(dst), Some(src_s)) = (ctx.scratch_mut(*dst_idx), ctx.scratch(src_idx)) else {
                    return;
                };
                let contributors = ctx.solo.of(*src_track_idx).0;
                mix_send_into(
                    dst,
                    *dst_idx,
                    src_s,
                    pre_fader,
                    song,
                    *src_track_idx,
                    *send_id,
                    p.sample_rate,
                    p.current_bpm,
                    p.playhead_beats,
                    p.any_solo,
                    p.recording_lanes,
                    n,
                    p.rows.track_rows(*src_track_idx as usize),
                    contributors,
                );
            }
            NodeOp::MixSend { .. } => {}
            // docs/plan_modulation.md §3/§6: この source の envelope follower を確定した信号で進める。
            NodeOp::EnvelopeFollow { src, slot } => {
                let (Some((src_l, src_r)), Some(fs)) = (ctx.tap(*src), ctx.follower_mut(*slot)) else {
                    return;
                };
                advance_follower(fs, src_l, src_r, n, *slot, p.follower_drive, p.sample_rate);
            }
        }
    }
}

/// r.md #129: 内蔵 device の外部サイドチェインを `owner` の program の `natives[native_slot]` の受け皿へ写す。
/// 読み元が同じ program の chain / Parallel ならフィールドで借用を分ける。
///
/// SAFETY: [`run_step`] と同じ (読むのは `src` の資源、書くのは `Program(owner)`)。
unsafe fn stage_native_sidechain(ctx: &RenderCtx<'_>, src: BufRef, owner: u32, native_slot: u32, n: usize) {
    let slot = native_slot as usize;
    unsafe {
        match program_tap_owner(src) {
            Some(o) if o == owner => {
                let Some(ChainProgram { chains, parallels, natives, .. }) = ctx.program_mut(owner) else { return };
                let Some((l, r)) = resolve_program_tap(chains, parallels, src) else { return };
                stage_into(natives.get_mut(slot), l, r, n);
            }
            _ => {
                let (Some((l, r)), Some(dst)) = (ctx.tap(src), ctx.program_mut(owner)) else { return };
                stage_into(dst.natives.get_mut(slot), l, r, n);
            }
        }
    }
}

/// 直列トレース `T` の `nodes` の手のうち、`keep` に合う op を順に実行する (テスト用の直列実行)。
#[cfg(test)]
pub(crate) fn run_nodes_for_test(ctx: &RenderCtx<'_>, keep: impl Fn(&NodeOp) -> bool) {
    for &step in &ctx.graph.trace {
        if let Step::Node(k) = step
            && ctx.nodes.get(k as usize).is_some_and(&keep)
        {
            run_step(ctx, step, 0);
        }
    }
}
