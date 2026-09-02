//! Schedule compiler: `Song` → `Schedule`.
//!
//! Run on the GUI side whenever the routing edits (track add / remove,
//! group reparent, send change, plugin latency reported), then the
//! resulting `Arc<Schedule>` is hot-swapped via `ArcSwap` for the RT
//! thread to pick up on the next buffer.
//!
//! PR2 handles flat tracks **and** the group hierarchy: each group's
//! children are mixed into the group's own scratch via `Mix`, then the
//! group runs its `ProcessGroupFx` op, and the result feeds either the
//! master bus (root group) or the next group up. Cycles in the parent
//! chain return `GraphError::Cycle`; dangling parent ids return
//! `DanglingReference`. PR3 will add the PDC delay-line insertion;
//! PR4 will add sends + sidechain edges + parallel-out routing.

#![allow(dead_code)]

use std::collections::HashMap;
use std::collections::HashSet;

use common::model::Song;

use super::delay_line::DelayLine;
use super::schedule::{BufRef, DelayKey, NodeOp, Schedule};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// Routing graph contains a cycle (`parent_group_id` chain, send loop,
    /// or sidechain feedback).
    Cycle,
    /// A `parent_group_id` / send dest / sidechain source references a
    /// track id that doesn't exist in the song, or names a track of the
    /// wrong kind (e.g. `parent_group_id` pointing at an Audio track).
    DanglingReference(u32),
}

/// 安定 `device_id` → プラグインが報告した processing latency (samples)。
///
/// r.md #9: 報告値は plugin host が持つ **実行時の観測値** であって曲の中身ではない
/// ので `Song` には載せない (載せると保存され、開き直したときに host の報告と
/// 食い違って「開いただけで `*`」 になる)。 engine は
/// `AudioCommand::SetDeviceLatency` でこの表を更新し、 track / master の合計は
/// [`chain_latency`] が chain から導出する (GUI 側では集計しない)。
pub type DeviceLatencies = HashMap<u64, u32>;

/// `chain` 上の device が報告している latency の合計 (samples)。
/// 未報告 (= 表に無い) device は 0 として扱う。
fn chain_latency(chain: &[common::model::PluginInstance], latencies: &DeviceLatencies) -> u32 {
    chain.iter().fold(0u32, |acc, d| {
        acc.saturating_add(latencies.get(&d.id).copied().unwrap_or(0))
    })
}

/// Compile a `Schedule` from `song`. PR2 supports the group hierarchy:
/// children → group `Mix` → `ProcessGroupFx` → upstream (parent group or
/// master). Tracks without a `parent_group_id` feed the master bus
/// directly.
///
/// `buffer_frames` = engine が 1 buffer で処理するフレーム数 (live = CPAL
/// device period、export = `MAX_FRAMES`)。**leaf** track 宛の sidechain tap は
/// staging (post-dispatch) と消費 (次 buffer の process) が 1 buffer ずれる
/// ため、その補償として leaf 宛 sidechain edge の latency に加算される
/// (`docs/plan_arch_refactor.md` §5)。bus (group / return / paraout dest) 宛は
/// tap → 同 buffer 内の `ProcessGroupFx` / master fx 消費なので加算しない。
///
/// この compile は `Vec` / `HashMap` の heap 確保を伴うが、 **off-RT で実行される**:
/// 呼び出し元は `main.rs` の publish 経路 (receive スレッド) と `export.rs`
/// のみ。 RT パス (audio callback) は wait-free SPSC (`rtrb`) 経由に
/// pre-compiled な `RtBundle` を pop して swap-in するだけで、 この alloc は
/// RT から完全に消えている (`LocalState::refresh_bundle` 参照)。
pub fn compile_schedule(
    song: &Song,
    device_latencies: &DeviceLatencies,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<Schedule, GraphError> {
    let n = song.tracks.len();
    if n == 0 {
        return Ok(Schedule {
            nodes: vec![NodeOp::Mix {
                srcs: Vec::new(),
                dst: BufRef::Master,
            }],
            ..Schedule::empty()
        });
    }

    // ---- track id → index, validate refs, validate kind ----
    let mut id_to_idx: HashMap<u32, u32> = HashMap::with_capacity(n);
    for (idx, t) in song.tracks.iter().enumerate() {
        if t.id != 0 {
            id_to_idx.insert(t.id, idx as u32);
        }
    }
    for t in &song.tracks {
        if let Some(pid) = t.parent_group_id
            && !id_to_idx.contains_key(&pid)
        {
            return Err(GraphError::DanglingReference(pid));
        }
        // Any existing track can act as a parent — the "group" role is
        // implicit (a track that has at least one child).
    }

    // ---- a track is a "group" iff some other track points at it ----
    let is_group: HashSet<u32> = song
        .tracks
        .iter()
        .filter_map(|t| t.parent_group_id)
        .collect();

    // ---- gather children per group track ----
    // (moved up from below: required by both cycle detection and depth fill)
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for (idx, t) in song.tracks.iter().enumerate() {
        if let Some(pid) = t.parent_group_id {
            children_of.entry(pid).or_default().push(idx as u32);
        }
    }

    // ---- gather incoming aux sends per destination (return / bus) ----
    // `incoming_sends[dest_id]` = list of (source track index, stable
    // `Send::id`, tap mode) for every send landing on `dest_id`. Sends whose
    // dest does not exist are dropped (tolerant, like dangling sidechain). A
    // track with ≥1 incoming send acts as a bus (summed like a group) even
    // with no children. v29: positional send index は schedule に焼き込まない
    // — engine は `send_id` で live lookup する。
    let mut incoming_sends: HashMap<u32, Vec<(u32, u32, common::model::SendMode)>> =
        HashMap::new();
    for (src_idx, t) in song.tracks.iter().enumerate() {
        for send in &t.sends {
            if id_to_idx.contains_key(&send.dest_track_id) {
                incoming_sends
                    .entry(send.dest_track_id)
                    .or_default()
                    .push((src_idx as u32, send.id, send.mode));
            }
        }
    }

    // ---- パラアウト (docs/plan_paraout.md): gather incoming aux outputs per
    // destination. `incoming_paraout[dest_id]` = list of (source track id,
    // stable device id, aux out port) for every plugin aux output routed at
    // `dest_id`. v29: the engine resolves the source plugin directly by
    // `device_id` (`PluginInstance::id`); the track id is kept only for the
    // PDC fan-in below. Bounded by `MAX_AUX_OUT` (the engine only fills that
    // many aux out ports). A track with ≥1 incoming paraout acts as a bus
    // (summed + FX'd in pass 2), exactly like a group / return; routes to a
    // missing dest are dropped (tolerant, like dangling sidechain / send). No
    // DAG edge is added: the source plugin's aux output is produced in pass 1
    // (`buffer_aux_out`) and consumed in pass 2, so pass 1 always precedes
    // the read — which is also why a group-with-instrument (A sums children
    // B/C while B/C read A's aux) is NOT a cycle.
    let mut incoming_paraout: HashMap<u32, Vec<(u32, u64, u8)>> = HashMap::new();
    {
        let mut gather = |chain: &[common::model::PluginInstance], src_track_id: u32| {
            for p in chain {
                for (port, route_opt) in p
                    .aux_outputs
                    .iter()
                    .take(common::process_data::MAX_AUX_OUT)
                    .enumerate()
                {
                    let Some(route) = route_opt else { continue };
                    if id_to_idx.contains_key(&route.dest_track) {
                        incoming_paraout
                            .entry(route.dest_track)
                            .or_default()
                            .push((src_track_id, p.id, port as u8));
                    }
                }
            }
        };
        for t in &song.tracks {
            gather(&t.devices, t.id);
        }
        gather(&song.master_fx_chain, common::model::MASTER_TRACK_ID);
    }

    // ---- bus / leaf classification (shared by op emission + PDC) ----
    // A track is a "bus" if it has children (a group), incoming sends (a
    // return), or incoming paraout (a parallel-out destination). A bus's
    // devices run in pass 2 (`ProcessGroupFx` / master fx) *after* the
    // sidechain taps of the same buffer; a leaf's devices ran in pass 1
    // *before* them — so leaf-destined taps are consumed one buffer late
    // and get `buffer_frames` of extra latency in the PDC math below.
    let bus_flags: Vec<bool> = song
        .tracks
        .iter()
        .map(|t| {
            is_group.contains(&t.id)
                || incoming_sends.contains_key(&t.id)
                || incoming_paraout.contains_key(&t.id)
        })
        .collect();

    // ---- detect cycles in path_latency dependency graph ----
    //
    // `compute_path_latency` recurses through:
    //   1. children of a group track (group depends on every child's path_latency)
    //   2. sidechain sources of plugins on the track (track depends on each
    //      sidechain source's path_latency)
    //
    // Cycle in this dep-graph ⇒ infinite recursion in path_latency ⇒ must
    // reject up front. Iterative 3-color DFS over the dep edges.
    //
    // PR4: this subsumes the old parent-chain-only detector — children-of
    // edges cover all parent cycles, sidechain-source edges cover all
    // sidechain feedback (incl. self-feedback A → A and A→B→A).
    let mut state = vec![0u8; n]; // 0=unvisited, 1=on current path, 2=done
    // Post-order of the dependency DFS = a valid execution order: a node
    // is appended only after all its dependencies (children + sidechain
    // sources + send sources) are done, so producers always precede
    // consumers. Replaces the old parent-only depth sort, which couldn't
    // order send / sidechain edges between same-depth tracks.
    let mut order: Vec<u32> = Vec::with_capacity(n);
    let dep_edges_for = |idx: u32| -> Vec<u32> {
        let track = &song.tracks[idx as usize];
        let mut out: Vec<u32> = Vec::new();
        if let Some(kids) = children_of.get(&track.id) {
            out.extend(kids.iter().copied());
        }
        // v23 single-chain: sidechain wiring lives on every device's
        // `aux_inputs` regardless of its derived role, so a single walk over
        // `devices` covers what the old per-section walks did.
        for p in &track.devices {
            for route in p.aux_inputs.iter().flatten() {
                if let Some(&src_idx) = id_to_idx.get(&route.tap.source_track) {
                    out.push(src_idx);
                }
            }
        }
        // send edges: this track (the destination / return) depends on
        // every track that sends into it — the source must run before the
        // send is mixed in. Covers send feedback (A→B→A, self-send) for
        // cycle detection.
        if let Some(edges) = incoming_sends.get(&track.id) {
            for &(src_idx, _, _) in edges {
                out.push(src_idx);
            }
        }
        out
    };
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        // Stack carries (node, next-edge-index-to-explore).
        let mut stack: Vec<(u32, usize)> = vec![(start as u32, 0)];
        state[start] = 1;
        while let Some(&(node, edge_i)) = stack.last() {
            let deps = dep_edges_for(node);
            if edge_i >= deps.len() {
                state[node as usize] = 2;
                order.push(node);
                stack.pop();
                continue;
            }
            // Advance the edge cursor on the current frame before we
            // possibly push a new one.
            if let Some(top) = stack.last_mut() {
                top.1 += 1;
            }
            let target = deps[edge_i];
            match state[target as usize] {
                0 => {
                    state[target as usize] = 1;
                    stack.push((target, 0));
                }
                1 => return Err(GraphError::Cycle),
                _ => {}
            }
        }
    }

    // ---- emit ops in dependency post-order (computed above) ----
    // `order` already lists every node after all of its dependencies, so
    // children precede their group, sidechain sources precede their sink,
    // and send sources precede their return.

    let mut nodes = Vec::with_capacity(n * 2 + 1);
    let mut master_srcs: Vec<(BufRef, f32)> = Vec::new();

    for &i in &order {
        let track = &song.tracks[i as usize];
        let track_idx = i;
        // PR4 sidechain: emit `SidechainTap` for every plugin on this
        // track with an `aux_inputs` route pointing at a valid
        // source track. The tap must run **before** the plugin's own
        // `process()` (i.e. before ProcessTrack / ProcessGroupFx for
        // this track) so the engine can stage the source signal in the
        // plugin's `pd.buffer_aux_in[port]` shmem region.
        emit_aux_input_taps(&track.devices, &id_to_idx, &mut nodes);

        // A bus sums its inputs into its own scratch and runs its fx chain +
        // strip via ProcessGroupFx, rather than rendering its own clips /
        // instrument as a leaf (Ableton return-track semantics). A track that
        // is not a bus is a plain leaf. (classification precomputed above)
        let is_bus = bus_flags[i as usize];
        if is_bus {
            // パラアウト (docs/plan_paraout.md): a group track whose own device
            // chain routes an aux output is a parallel-out **source**. Its
            // instrument prefix `[0..split]` runs in pass 1 (`process_track_owned`)
            // producing every output bus; the suffix FX `[split..]` run in pass 2
            // on the summed bus. Two sub-modes, by where the MAIN output (port 0)
            // goes:
            //  - 全部子 (`paraout_main_to_child`, `aux_outputs[0] = Some`): main
            //    goes to its own child track too, so the parent keeps NO own
            //    signal — a clearing `Mix` sums ALL children (parent = pure bus).
            //  - 楽器兼バス (port 0 unrouted): the parent keeps its own main (e.g.
            //    the kick) in scratch and sums children on top via `MixAdditive`.
            // A pure group / return / paraout-dest bus (no instrument) clears +
            // sums the whole chain (`start_device = 0`).
            let split = track.paraout_split_device();
            let group_with_instrument = is_group.contains(&track.id) && split.is_some();
            let start_device = if group_with_instrument {
                split.unwrap_or(0)
            } else {
                0
            };

            let kids = children_of.get(&track.id).cloned().unwrap_or_default();
            let srcs: Vec<(BufRef, f32)> = kids
                .into_iter()
                .map(|c| (BufRef::TrackScratch(c), 1.0))
                .collect();
            if group_with_instrument && !track.paraout_main_to_child() {
                // 楽器兼バス: keep the parent's own main, add children on top.
                nodes.push(NodeOp::MixAdditive {
                    srcs,
                    dst: BufRef::TrackScratch(track_idx),
                });
            } else {
                // 全部子 / pure group / return / paraout-dest: clear + sum.
                nodes.push(NodeOp::Mix {
                    srcs,
                    dst: BufRef::TrackScratch(track_idx),
                });
            }
            // パラアウト: accumulate each plugin aux output routed INTO this
            // track on top of the children. The source plugin's aux output is
            // produced in pass 1, so this tap (pass 2) always sees settled
            // data — zero latency. `device_id` は安定 id (直接 plugin_refs を
            // 引ける)、`dst_track` is this bus's scratch **index**.
            if let Some(edges) = incoming_paraout.get(&track.id) {
                for &(_, device_id, port) in edges {
                    nodes.push(NodeOp::ParallelOutTap {
                        device_id,
                        port,
                        dst_track: track_idx,
                    });
                }
            }
            // Accumulate each incoming send on top, tapping the source's
            // post- or pre-fader buffer. The gain is applied live by the
            // engine (`MixSend`), not baked here.
            if let Some(edges) = incoming_sends.get(&track.id) {
                for &(src_idx, send_id, mode) in edges {
                    let src = match mode {
                        common::model::SendMode::PostFader => BufRef::TrackScratch(src_idx),
                        common::model::SendMode::PreFader => BufRef::PreFaderScratch(src_idx),
                    };
                    nodes.push(NodeOp::MixSend {
                        src,
                        dst: BufRef::TrackScratch(track_idx),
                        src_track_idx: src_idx,
                        send_id,
                    });
                }
            }
            nodes.push(NodeOp::ProcessGroupFx {
                track_idx,
                start_device,
            });
        } else {
            // Leaf track: full chain handled by ProcessTrack op.
            nodes.push(NodeOp::ProcessTrack { track_idx });
        }

        // Top-level (no parent) tracks/groups feed the master bus.
        if track.parent_group_id.is_none() {
            master_srcs.push((BufRef::TrackScratch(track_idx), 1.0));
        }
    }

    nodes.push(NodeOp::Mix {
        srcs: master_srcs,
        dst: BufRef::Master,
    });

    // master bus fx chain の sidechain。 master Mix の **後** に SidechainTap を
    // 積む (= source track の scratch は dispatch_and_wait で確定済み)。
    // `execute_schedule_post_dispatch` がこの tap を処理して source scratch を
    // master fx plugin の `pd.buffer_aux_in[port]` に staging し、 直後の
    // `process_master_fx_chain` が plugin process でそれを読む。 dst_track は
    // `MASTER_TRACK_ID` (master は audio fx のみ)。 track 経路と同じ
    // `emit_aux_input_taps` を使う (critique #1: emit site の単一化)。
    emit_aux_input_taps(&song.master_fx_chain, &id_to_idx, &mut nodes);

    // ---- PR3: Plugin Delay Compensation ----
    //
    // 各 track の **path latency** を計算し、 Mix の合流点で path 間の
    // 不一致を `ApplyDelay` で補償する。 Ardour の `Latent` 基底クラス +
    // `route.cc::process_output_buffers` の流儀:
    //
    //   path_latency(leaf)  = leaf の chain_latency
    //   path_latency(group) = max(child.path_latency) + group の chain_latency
    //
    // `chain_latency` は device 単位の報告値 (`device_latencies`) を track の
    // device chain 上で合計したもの。 GUI は集計せず device 単位で送ってくる
    // (r.md #9: 集計値を `Song` に持たせると保存されて「開いただけで `*`」)。
    //
    // ※ group では子達が group bus に流れ込むときに既に sibling alignment が
    //   行われている (ここで挿入する `ApplyDelay` で揃う) ので、 group の
    //   入力 bus 時点では全員 `max(child.path_latency)` に揃っている。
    //   従って group 自身の path_latency = max + own。
    //
    // 補償ルール: 各 Mix ノードの srcs の中で `path_latency` が最も小さい側に、
    //   `frames = max_path - this_path` の `ApplyDelay` を **Mix の直前に**
    //   挿入する。 こうすると合流点で全 src の累積 latency が揃う。
    let track_chain_latency: Vec<u32> = song
        .tracks
        .iter()
        .map(|t| chain_latency(&t.devices, device_latencies))
        .collect();
    let mut path_latency = vec![u32::MAX; n];
    for i in 0..n {
        compute_path_latency(
            i as u32,
            &song.tracks,
            &track_chain_latency,
            &is_group,
            &children_of,
            &id_to_idx,
            &incoming_sends,
            &bus_flags,
            buffer_frames,
            &mut path_latency,
        );
    }

    // パラアウト独立 dest の PDC fan-in (docs/plan_paraout.md): plugin の aux
    // 出力を「自分の子でない」 track へ振った独立トポロジでは、 dest の path
    // latency に source の path latency を取り込む (sidechain / send と同じ。
    // dest の入力 = source の aux なので、 source が遅れる分 dest も遅れる)。
    // 子 dest (group-with-instrument の子) は group fan-in 済み + 循環になるので
    // 除外する。 `reported` は既に path_latency[dest] に含まれるので
    // `max(existing, source_latency + dest.reported)` で更新 (= max(a,b)+c の
    // 分配律)。 健全な (非循環) paraout chain は深さ <= n で必ず収束するので
    // bounded fixpoint で回す。 相互 paraout (A.aux→D かつ D.aux→A — ParallelOutTap
    // は dep edge を張らないので既存の cycle 検出を通り抜ける病的ケース) でも n 回で
    // 打ち切り、 path_latency の発散 (= 際限ない DelayLine 確保 / ハング) を防ぐ。
    for _ in 0..n {
        let mut changed = false;
        for (dest_id, edges) in &incoming_paraout {
            let Some(&d_idx) = id_to_idx.get(dest_id) else {
                continue;
            };
            let dest = &song.tracks[d_idx as usize];
            for &(src_id, _, _) in edges {
                if dest.parent_group_id == Some(src_id) {
                    continue; // 子 dest は group fan-in 済み (循環回避)
                }
                let Some(&s_idx) = id_to_idx.get(&src_id) else {
                    continue;
                };
                let cand = path_latency[s_idx as usize]
                    .saturating_add(track_chain_latency[d_idx as usize]);
                if cand > path_latency[d_idx as usize] {
                    path_latency[d_idx as usize] = cand;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // 既存の nodes を線形に走査し、 Mix / MixAdditive を見つけたらその直前に
    // `ApplyDelay` を挿入する。 in-place 操作よりも build-from-scratch
    // の方が境界条件が単純なので、 一度別 Vec に組み直す。
    // §5 D: 各 DelayLine には stable key (`DelayKey`) を平行 Vec で持たせ、
    // 再 compile 時に `Schedule::adopt_state_from` が ring の内容を移送する。
    let mut delay_lines: Vec<DelayLine> = Vec::new();
    let mut delay_keys: Vec<DelayKey> = Vec::new();
    let track_ids: Vec<u32> = song.tracks.iter().map(|t| t.id).collect();
    let mut nodes_with_pdc: Vec<NodeOp> = Vec::with_capacity(nodes.len() + n);
    // master 合流点で全 src が揃う latency (r.md #39)。master fx chain 自身の
    // latency は下でこれに加算する (click / export は master **出力** を基準にする)。
    let mut master_mix_latency: u32 = 0;
    for op in nodes.into_iter() {
        match &op {
            // clearing Mix: 全 src を最大 path latency に揃える。
            NodeOp::Mix { srcs, dst } => {
                let max_path = emit_mix_src_alignment(
                    srcs,
                    &path_latency,
                    &track_ids,
                    &mut delay_lines,
                    &mut delay_keys,
                    &mut nodes_with_pdc,
                );
                if *dst == BufRef::Master {
                    master_mix_latency = max_path;
                }
            }
            // パラアウト MixAdditive (docs/plan_paraout.md): 子 (srcs) を揃える
            // のに加え、 dst (= group-with-instrument 自身の scratch にある prefix
            // main、 相対 latency 0) も子の最大 path latency 分だけ遅らせて揃える。
            // これをしないと、 子に latency 持ちプラグインがあるときキック (main)
            // とスネア等 (子経由) がサンプルずれる。 子の ApplyDelay と dst の
            // ApplyDelay を MixAdditive の直前に積むので、 加算時には両者が揃う。
            NodeOp::MixAdditive {
                srcs,
                dst: BufRef::TrackScratch(a_idx),
            } => {
                let max_path = emit_mix_src_alignment(
                    srcs,
                    &path_latency,
                    &track_ids,
                    &mut delay_lines,
                    &mut delay_keys,
                    &mut nodes_with_pdc,
                );
                if max_path > 0 {
                    let line_idx = delay_lines.len() as u32;
                    delay_lines.push(DelayLine::with_capacity((max_path as usize) + 1));
                    delay_keys.push(DelayKey::MixDst {
                        track_id: track_ids.get(*a_idx as usize).copied().unwrap_or(0),
                    });
                    nodes_with_pdc.push(NodeOp::ApplyDelay {
                        buf: BufRef::TrackScratch(*a_idx),
                        line_idx,
                        frames: max_path,
                    });
                }
            }
            NodeOp::MixAdditive { .. } => {
                // MixAdditive の dst は compile が常に TrackScratch で emit する。
            }
            _ => {}
        }
        nodes_with_pdc.push(op);
    }

    // PR4.5 sidechain plugin-internal alignment: per-track input delay.
    // The delay is applied to the track's main signal so it lines up with any
    // sidechain a device reads. Only audio-processing devices (= has both an
    // audio input and an audio output) can read a sidechain, so only those
    // contribute (a pure source / MIDI device has no main-in to delay against).
    // v23 single-chain: a direct port predicate, no role derivation. Edit-time.
    // §5 (arch refactor): leaf 宛の tap は staging→消費が 1 buffer ずれるので
    // `buffer_frames` を加算して plugin 入力での main vs aux を位相一致させる
    // (compute_path_latency の sidechain edge と同じ規則 — 両者は同値でなければ
    // master 合流の sibling alignment とズレる)。
    let mut input_delay_per_track = vec![0u32; n];
    for (i, track) in song.tracks.iter().enumerate() {
        let tap_lag = if bus_flags[i] { 0 } else { buffer_frames };
        let mut max_sc: u32 = 0;
        for p in &track.devices {
            if !(p.ports.has_audio_input && p.ports.has_audio_output) {
                continue;
            }
            for route in p.aux_inputs.iter().flatten() {
                if let Some(&src_idx) = id_to_idx.get(&route.tap.source_track) {
                    let l = path_latency[src_idx as usize].saturating_add(tap_lag);
                    if l > max_sc {
                        max_sc = l;
                    }
                }
            }
        }
        input_delay_per_track[i] = max_sc;
    }

    // docs/plan_modulation.md §3/§5: per-`ModSource` envelope follower.
    // Emit `EnvelopeFollow` at the very end of the (post-PDC) schedule — all
    // scratches are settled and the follower only produces a control-rate
    // scalar (no audio feedback, so no ordering / cycle constraint).
    // Coefficients are baked here (recompile-time) so the RT path never
    // derives them (§10)。
    //
    // `slot` は **schedule の内部 index** で、`follower_slots` / `follower_keys` /
    // `mod_kinds` の 3 本が同じ並び。外へ出る値 (GUI へ publish する面 / sidecar) は
    // `follower_keys[slot]` = `ModSource::id` を組にして運ぶ
    // (`crate::mod_tick::eval_plane`、アーキ不変条件 1)。envelope follower の slot は
    // EnvelopeFollow node が `env` を駆動するが、generator (LFO/Random/MSEG/Steps) の
    // slot は inert で、engine が `common::modulators::generator_scalar` を刻みの
    // song 位置から評価して載せる (`mod_kinds` を保持)。
    let mut follower_slots: Vec<super::follower::FollowerSlot> = Vec::new();
    let mut follower_keys: Vec<u32> = Vec::new();
    let mut mod_kinds: Vec<common::model::ModSourceKind> = Vec::new();
    for (slot, ms) in song
        .mod_sources
        .iter()
        .take(common::audio_bridge::MAX_MOD_SOURCES)
        .enumerate()
    {
        mod_kinds.push(ms.kind.clone());
        // §5 D: 状態移送キー = ModSource の安定 id (0 = 未採番、移送対象外)。
        follower_keys.push(ms.id);
        match &ms.kind {
            common::model::ModSourceKind::EnvelopeFollower { tap, follower } => {
                follower_slots.push(super::follower::FollowerSlot::from_config(
                    follower,
                    sample_rate,
                ));
                // docs/plan_modulation.md §6: tap_point で source buffer を解決。
                // dangling source は follower node を emit しない (scalar は 0 のまま)。
                if let Some(&src_idx) = id_to_idx.get(&tap.source_track) {
                    nodes_with_pdc.push(NodeOp::EnvelopeFollow {
                        src: tap_bufref(tap.tap_point, src_idx),
                        slot: slot as u32,
                    });
                }
            }
            // generator: inert slot (env 未使用、 generator_scalar が値を供給)。
            _ => {
                follower_slots.push(super::follower::FollowerSlot::from_config(
                    &common::model::FollowerConfig::default(),
                    sample_rate,
                ));
            }
        }
    }

    Ok(Schedule {
        nodes: nodes_with_pdc,
        delay_lines,
        delay_keys,
        port_buffers: super::PortBufferPool::new(),
        input_delay_per_track,
        follower_slots,
        follower_keys,
        mod_kinds,
        // r.md #39: click / export が基準にするのは master **出力** の遅延量。
        // master fx chain は Mix の後段で直列 process される (`process_master_fx_chain`)
        // ので、その報告 latency も足さないと master に遅延プラグインを挿したときだけ
        // click が先行し、書き出し WAV もその分ずれる。
        master_latency_samples:
            master_output_latency(song, device_latencies, master_mix_latency, sample_rate),
    })
}

/// master **出力**の遅延量 (`Schedule::master_latency_samples`)。
///
/// = master 合流点の path latency + master fx chain の報告 latency +
/// マスターリミッターのルックアヘッド (ON のときだけ)。click の参照位置と書き出しの
/// 窓ずらしはどちらもこの 1 値を引くので、遅延源を足すときは必ずここに足す
/// (r.md #39 / docs/plan_master_strip.md §2)。
///
/// リミッターの遅延は OFF なら素通し (0) で、ON/OFF は `edit_song` 経由 = LoadSong で
/// 再 compile されるため、この値は切り替えに即追従する。
fn master_output_latency(
    song: &Song,
    device_latencies: &DeviceLatencies,
    master_mix_latency: u32,
    sample_rate: u32,
) -> u32 {
    let limiter = if song.master_strip.limiter.on {
        common::model::limiter_lookahead_samples(sample_rate)
    } else {
        0
    };
    master_mix_latency
        .saturating_add(chain_latency(&song.master_fx_chain, device_latencies))
        .saturating_add(limiter)
}

/// docs/plan_modulation.md §5: walk a device `chain` (a track's `devices` or
/// the master `master_fx_chain`) and emit `NodeOp::SidechainTap` for every
/// plugin `aux_inputs` route whose source track exists. The single helper
/// replaces the former per-track `emit_sidechain_taps` + the inlined
/// master-bus loop (critique #1: there were two emit sites). `dst_track` is
/// the destination plugin's owning track id (`MASTER_TRACK_ID` for master
/// fx); `dst_index` is the device's position in the chain. dangling
/// references are skipped (no compile error). `ProcessTrack` of the source
/// runs earlier, so its scratch is settled by the time the tap copies it.
/// docs/plan_modulation.md §6 / docs/plan_modulation_followups.md §1: resolve a
/// tap point to the source scratch buffer. `PostFader` = the track's final
/// output (`TrackScratch`); `PostFx` = after the device chain but before the
/// volume/pan strip (`PreFaderScratch`, snapshot guarded in the engine);
/// `PreFx` = the raw signal before the device chain (`PreFxScratch`, snapshot
/// guarded in the engine). All three snapshots are captured only when a tap
/// actually needs them.
fn tap_bufref(tap_point: common::model::TapPoint, src_idx: u32) -> BufRef {
    use common::model::TapPoint;
    match tap_point {
        TapPoint::PostFader => BufRef::TrackScratch(src_idx),
        TapPoint::PostFx => BufRef::PreFaderScratch(src_idx),
        TapPoint::PreFx => BufRef::PreFxScratch(src_idx),
    }
}

fn emit_aux_input_taps(
    chain: &[common::model::PluginInstance],
    id_to_idx: &HashMap<u32, u32>,
    nodes: &mut Vec<NodeOp>,
) {
    for inst in chain {
        // aux port は engine が `MAX_AUX_IN` までしか staging しないので
        // `take(MAX_AUX_IN)` で `port_idx < MAX_AUX_IN` を構造的に保証し、
        // `as u8` の wrap を防ぐ。
        for (port_idx, route_opt) in inst
            .aux_inputs
            .iter()
            .take(common::process_data::MAX_AUX_IN)
            .enumerate()
        {
            let Some(route) = route_opt else {
                continue;
            };
            let Some(&src_idx) = id_to_idx.get(&route.tap.source_track) else {
                // dangling reference: silently skip
                continue;
            };
            // v29: 宛先 plugin は安定 device id で焼き込む。id 未採番 (0) の
            // instance は engine 側 lookup が必ず外れる (= 旧来の「lookup miss
            // で skip」と同じ寛容さ) なのでここでは弾かない。
            nodes.push(NodeOp::SidechainTap {
                src: tap_bufref(route.tap.tap_point, src_idx),
                device_id: inst.id,
                aux_in_port: port_idx as u8,
            });
        }
    }
}

/// `path_latency[idx]` を計算してキャッシュする。 既に値があれば即返却
/// (memoization)。
///
/// PR3: group (`is_group` メンバ) は子の path_latency の最大値を自身の input
/// bus latency として、 そこに自身の chain latency (device 報告値の合計) を足す。
///
/// PR4 sidechain × PDC: track の単一 `devices` チェーン上の各 plugin の
/// `aux_inputs` の tap も `input bus latency` に取り込む。 すなわち:
///
///   input_latency(T) = max(
///       max(child.path_latency for child in children_of(T)),
///       max(source.path_latency for (P, source) in sidechain_inputs(T))
///   )
///   path_latency(T) = input_latency(T) + T の chain latency
///
/// 仕様根拠: Ardour `route.cc::process_output_buffers` の "feed-forward
/// latency reporting" — sidechain edge も `Latent` の input の一種として
/// 扱う。 こうすると master / group bus の sibling alignment compensation が
/// sidechain 経由の遅延を含めた最大 path を基準に補償する (= 「サイドチェイン
/// 受信 track の出力が他の sibling track と musical time で揃う」)。
///
/// **注意 (本実装の限界)**: ここでは plugin の **入力 bus 単位** で latency
/// を揃える。 plugin が chain の途中 (slot K, K>0) にいるときは pre-plugin
/// chain prefix latency があり、 plugin 内部での main vs aux alignment は
/// それを考慮した DelayTrackInput op (= 別 PR) で完成する。 現状でも graph
/// layer の不変量は成立するので master / group の audio mix は崩れない。
///
/// dangling reference (= sidechain source が song に存在しない) は wrap せず
/// 0 として扱う (compile error にしない方針、 編集中の中間状態を許容)。
/// §5 (arch refactor): sidechain edge の実効 latency には、**leaf** 宛のとき
/// `buffer_frames` (tap staging→消費の 1-buffer 遅延) が加算される。bus
/// (group / return / paraout dest) 宛は同 buffer 内消費なので 0。
/// `input_delay_per_track` の計算と同じ規則を使わないと、plugin 入力での
/// main/aux 揃えと master 合流の sibling alignment がズレる。
#[allow(clippy::too_many_arguments)]
fn compute_path_latency(
    idx: u32,
    tracks: &[common::model::Track],
    // `tracks` と同順の「その track の device chain が報告する latency の合計」。
    track_chain_latency: &[u32],
    is_group: &HashSet<u32>,
    children_of: &HashMap<u32, Vec<u32>>,
    id_to_idx: &HashMap<u32, u32>,
    incoming_sends: &HashMap<u32, Vec<(u32, u32, common::model::SendMode)>>,
    bus_flags: &[bool],
    buffer_frames: u32,
    cache: &mut [u32],
) -> u32 {
    if cache[idx as usize] != u32::MAX {
        return cache[idx as usize];
    }
    let track = &tracks[idx as usize];

    let group_input: u32 = if is_group.contains(&track.id) {
        children_of
            .get(&track.id)
            .map(|kids| {
                kids.iter()
                    .map(|&c| {
                        compute_path_latency(
                            c,
                            tracks,
                            track_chain_latency,
                            is_group,
                            children_of,
                            id_to_idx,
                            incoming_sends,
                            bus_flags,
                            buffer_frames,
                            cache,
                        )
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    } else {
        0
    };

    // leaf 宛 tap の staging lag (moduleコメント参照)。
    let tap_lag = if bus_flags[idx as usize] { 0 } else { buffer_frames };
    let mut sidechain_input: u32 = 0;
    let mut consider = |src_id_opt: &Option<u32>, cache: &mut [u32]| {
        if let Some(src_id) = src_id_opt
            && let Some(&src_idx) = id_to_idx.get(src_id)
        {
            let l = compute_path_latency(
                src_idx,
                tracks,
                track_chain_latency,
                is_group,
                children_of,
                id_to_idx,
                incoming_sends,
                bus_flags,
                buffer_frames,
                cache,
            )
            .saturating_add(tap_lag);
            if l > sidechain_input {
                sidechain_input = l;
            }
        }
    };
    // v23 single-chain: latency propagation cares about every device's
    // sidechain source regardless of role (a sidechain edge from any device
    // raises this track's input latency), so a single walk over `devices`
    // replaces the old per-section walks.
    for p in &track.devices {
        for route in p.aux_inputs.iter().flatten() {
            consider(&Some(route.tap.source_track), cache);
        }
    }

    // Aux-send fan-in: a return / bus depends on every track that sends
    // into it, so its input latency must also cover those sources. This
    // keeps the wet return time-aligned with the dry signal at the master
    // mix (the source's post-fader latency is carried by the send copy).
    let mut send_input: u32 = 0;
    if let Some(edges) = incoming_sends.get(&track.id) {
        for &(src_idx, _, _) in edges {
            let l = compute_path_latency(
                src_idx,
                tracks,
                track_chain_latency,
                is_group,
                children_of,
                id_to_idx,
                incoming_sends,
                bus_flags,
                buffer_frames,
                cache,
            );
            if l > send_input {
                send_input = l;
            }
        }
    }

    let max_input = group_input.max(sidechain_input).max(send_input);
    let total = max_input.saturating_add(track_chain_latency[idx as usize]);
    cache[idx as usize] = total;
    total
}

/// PDC helper: emit an `ApplyDelay` for every `TrackScratch` src whose path
/// latency is below the mix's max, so all srcs line up at the mix point.
/// Returns the max path latency over the srcs — `MixAdditive` uses it to also
/// align the dst's own pre-existing signal (the パラアウト instrument main,
/// `docs/plan_paraout.md`). Shared by the `Mix` and `MixAdditive` arms of the
/// PDC pass so the two stay in lock-step. `track_ids` は delay line の
/// stable key (`DelayKey::MixSrc`) 用の song-track-index → `Track::id` 表。
fn emit_mix_src_alignment(
    srcs: &[(BufRef, f32)],
    path_latency: &[u32],
    track_ids: &[u32],
    delay_lines: &mut Vec<DelayLine>,
    delay_keys: &mut Vec<DelayKey>,
    out: &mut Vec<NodeOp>,
) -> u32 {
    let max_path = srcs
        .iter()
        .filter_map(|(b, _)| match b {
            BufRef::TrackScratch(i) => Some(path_latency[*i as usize]),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    for (b, _) in srcs.iter() {
        let BufRef::TrackScratch(i) = b else {
            continue;
        };
        let this = path_latency[*i as usize];
        if this < max_path {
            let comp = max_path - this;
            let line_idx = delay_lines.len() as u32;
            // DelayLine.step は `delay <= capacity - 1` を要求
            // (`delay_line.rs` の clamp ロジック)。 補償量ちょうどを
            // 返すために capacity = comp + 1。
            delay_lines.push(DelayLine::with_capacity((comp as usize) + 1));
            delay_keys.push(DelayKey::MixSrc {
                track_id: track_ids.get(*i as usize).copied().unwrap_or(0),
            });
            out.push(NodeOp::ApplyDelay {
                buf: BufRef::TrackScratch(*i),
                line_idx,
                frames: comp,
            });
        }
    }
    max_path
}

/// テスト用: 「どの device も latency を報告していない」 前提で compile する短縮形。
/// PDC を検証するテストだけが `compile_schedule` に表を明示的に渡す。
#[cfg(test)]
pub(crate) fn compile_schedule_for_test(
    song: &Song,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<Schedule, GraphError> {
    compile_schedule(song, &DeviceLatencies::new(), sample_rate, buffer_frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{PluginInstance, Song, Track};
    use common::plugin_format::PluginFormat;
    use common::port_config::PortConfig;

    /// PDC テスト用: `samples` を報告する device を 1 個持つ chain を作り、
    /// 報告値を `lat` に登録する。 報告 latency は `Song` ではなく
    /// `DeviceLatencies` 側に居る (r.md #9) ので、 テストも同じ形で組む。
    fn latency_chain(
        lat: &mut DeviceLatencies,
        device_id: u64,
        samples: u32,
    ) -> Vec<PluginInstance> {
        lat.insert(device_id, samples);
        vec![PluginInstance {
            id: device_id,
            ..PluginInstance::with_ports(
                format!("test.latent{device_id}"),
                PluginFormat::Clap,
                audio_fx_ports(),
            )
        }]
    }

    /// v23 single-chain: `Track::default()` を mutator で埋める helper。
    /// downstream crate (daw_audio) の test で `Track { .., ..Track::default() }`
    /// を書くと、 `common` 内の `pub(crate)` legacy migration fields が見えず
    /// E0451 になるため、 private field に触れない default + mutate で回避する。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    /// v23 single-chain: a pure audio-FX device (audio output only, no note
    /// I/O) — derives as `AudioEffect` when no device in the chain has note
    /// input. Used by the sidechain / PDC tests where the dest plugin is a
    /// compressor on the track's audio signal.
    fn audio_fx_ports() -> PortConfig {
        PortConfig {
            has_note_input: false,
            has_note_output: false,
            has_audio_output: true,
            // pure audio-FX: audio を加工する → audio 入力あり。
            has_audio_input: true,
            has_video_input: false,
            has_video_output: false,
        }
    }

    /// v23 single-chain: an instrument device (note input + audio output) —
    /// derives as `Instrument` (MIDI→audio). Used by the instrument-sidechain
    /// test.
    fn instrument_ports() -> PortConfig {
        PortConfig {
            has_note_input: true,
            has_note_output: false,
            has_audio_output: true,
            // instrument: note→audio 生成。 audio を加工しない → audio 入力なし。
            has_audio_input: false,
            has_video_input: false,
            has_video_output: false,
        }
    }

    #[test]
    fn empty_song_compiles_to_master_only_mix() {
        let song = Song::default();
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        assert_eq!(sched.nodes.len(), 1);
        match &sched.nodes[0] {
            NodeOp::Mix { dst, srcs } => {
                assert_eq!(*dst, BufRef::Master);
                assert!(srcs.is_empty());
            }
            other => panic!("expected Mix → Master, got {other:?}"),
        }
    }

    #[test]
    fn tap_bufref_resolves_three_tap_points() {
        use common::model::TapPoint;
        // docs/plan_modulation_followups.md §1: PostFader=TrackScratch,
        // PostFx=PreFaderScratch, PreFx=専用 PreFxScratch (旧フォールバック撤廃)。
        assert_eq!(tap_bufref(TapPoint::PostFader, 3), BufRef::TrackScratch(3));
        assert_eq!(tap_bufref(TapPoint::PostFx, 3), BufRef::PreFaderScratch(3));
        assert_eq!(tap_bufref(TapPoint::PreFx, 3), BufRef::PreFxScratch(3));
    }

    #[test]
    fn flat_audio_tracks_emit_process_then_mix() {
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                }),
                track(|t| {
                    t.id = 2;
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        // Depth ordering is stable for a flat song, so ProcessTrack ops
        // appear in reverse-track-order (depth=0 group is just descending
        // index order); both ways the Mix at the end carries both refs.
        assert_eq!(sched.nodes.len(), 3);
        let process_count = sched
            .nodes
            .iter()
            .filter(|op| matches!(op, NodeOp::ProcessTrack { .. }))
            .count();
        assert_eq!(process_count, 2);
        match sched.nodes.last().unwrap() {
            NodeOp::Mix { dst, srcs } => {
                assert_eq!(*dst, BufRef::Master);
                assert_eq!(srcs.len(), 2);
                let track_indices: Vec<u32> = srcs
                    .iter()
                    .map(|(b, _)| match b {
                        BufRef::TrackScratch(i) => *i,
                        other => panic!("unexpected master src {other:?}"),
                    })
                    .collect();
                assert!(track_indices.contains(&0));
                assert!(track_indices.contains(&1));
            }
            other => panic!("expected Mix → Master, got {other:?}"),
        }
    }

    #[test]
    fn group_with_children_compiles_to_two_phase_mix() {
        // Layout:
        //   Group 1 (Drums) → Master
        //     ├─ Audio 2 (Kick)   parent=1
        //     └─ Audio 3 (Snare)  parent=1
        //   Audio 4 (Lead) → Master  (no parent)
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Drums".into();
                    t.parent_group_id = None;
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Kick".into();
                    t.parent_group_id = Some(1);
                }),
                track(|t| {
                    t.id = 3;
                    t.name = "Snare".into();
                    t.parent_group_id = Some(1);
                }),
                track(|t| {
                    t.id = 4;
                    t.name = "Lead".into();
                    t.parent_group_id = None;
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

        // Expect (in some order): two leaf ProcessTrack (kick, snare),
        // group's Mix-into-self + ProcessGroupFx, lead ProcessTrack, then
        // final Mix → Master with two refs (group + lead).
        let process_track_idxs: Vec<u32> = sched
            .nodes
            .iter()
            .filter_map(|op| match op {
                NodeOp::ProcessTrack { track_idx } => Some(*track_idx),
                _ => None,
            })
            .collect();
        assert!(process_track_idxs.contains(&1)); // Kick at index 1
        assert!(process_track_idxs.contains(&2)); // Snare at index 2
        assert!(process_track_idxs.contains(&3)); // Lead at index 3

        // Group must be processed *after* its children → kick and snare
        // ProcessTrack must come before Group's Mix-into-self.
        let kick_pos = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 1 }))
            .unwrap();
        let snare_pos = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 2 }))
            .unwrap();
        let group_mix_pos = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::Mix {
                        dst: BufRef::TrackScratch(0),
                        ..
                    }
                )
            })
            .unwrap();
        assert!(
            kick_pos < group_mix_pos,
            "kick must process before group mix"
        );
        assert!(
            snare_pos < group_mix_pos,
            "snare must process before group mix"
        );

        // Group's Mix should carry both children.
        let group_mix_srcs = sched
            .nodes
            .iter()
            .find_map(|op| match op {
                NodeOp::Mix {
                    dst: BufRef::TrackScratch(0),
                    srcs,
                } => Some(srcs.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(group_mix_srcs.len(), 2);

        // Master mix must reference *the group* (idx 0) and *Lead* (idx 3),
        // not the raw children (which are folded into the group bus).
        let master_srcs = sched
            .nodes
            .iter()
            .find_map(|op| match op {
                NodeOp::Mix {
                    dst: BufRef::Master,
                    srcs,
                } => Some(srcs.clone()),
                _ => None,
            })
            .unwrap();
        let master_idxs: Vec<u32> = master_srcs
            .iter()
            .map(|(b, _)| match b {
                BufRef::TrackScratch(i) => *i,
                other => panic!("unexpected master src {other:?}"),
            })
            .collect();
        assert!(master_idxs.contains(&0)); // Group
        assert!(master_idxs.contains(&3)); // Lead
        assert!(!master_idxs.contains(&1));
        assert!(!master_idxs.contains(&2));
    }

    #[test]
    fn nested_groups_emit_inner_group_before_outer() {
        // Outer track 1 (becomes a group because track 2 points at it)
        //   Inner track 2 (becomes a group because track 3 points at it)
        //     Audio 3 (parent=2)
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.parent_group_id = None;
                }),
                track(|t| {
                    t.id = 2;
                    t.parent_group_id = Some(1);
                }),
                track(|t| {
                    t.id = 3;
                    t.parent_group_id = Some(2);
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

        let audio_pos = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 2 }))
            .unwrap();
        let inner_mix_pos = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::Mix {
                        dst: BufRef::TrackScratch(1),
                        ..
                    }
                )
            })
            .unwrap();
        let outer_mix_pos = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::Mix {
                        dst: BufRef::TrackScratch(0),
                        ..
                    }
                )
            })
            .unwrap();
        assert!(audio_pos < inner_mix_pos);
        assert!(inner_mix_pos < outer_mix_pos);
    }

    #[test]
    fn parent_cycle_is_rejected() {
        // Track 1 ↔ Track 2 cycle.
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.parent_group_id = Some(2);
                }),
                track(|t| {
                    t.id = 2;
                    t.parent_group_id = Some(1);
                }),
            ],
            ..Song::default()
        };
        assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle));
    }

    #[test]
    fn audio_track_can_become_a_group_implicitly() {
        // With kind removed, any track that has a child IS a group.
        // Track 1 has no flag, but track 2 points at it via
        // parent_group_id, so track 1 is treated as a group bus.
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                }),
                track(|t| {
                    t.id = 2;
                    t.parent_group_id = Some(1);
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        // Track 1 should emit Mix → TrackScratch(0) + ProcessGroupFx(0),
        // not ProcessTrack(0).
        let has_group_fx = sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 0, .. }));
        assert!(has_group_fx, "track 1 must be treated as a group");
        let has_process_track_0 = sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 }));
        assert!(
            !has_process_track_0,
            "track 1 has children so its leaf op should be skipped"
        );
    }

    #[test]
    fn parent_pointing_to_unknown_track_is_rejected() {
        let song = Song {
            tracks: vec![track(|t| {
                t.id = 1;
                t.parent_group_id = Some(99);
            })],
            ..Song::default()
        };
        assert_eq!(
            compile_schedule_for_test(&song, 48_000, 0).err(),
            Some(GraphError::DanglingReference(99))
        );
    }

    // ---- PR3: Plugin Delay Compensation ----

    /// r.md #39: `master_latency_samples` = master 合流点で全 src が揃えられる
    /// latency。engine はこの値だけ metronome click の参照位置を戻して、遅延
    /// プラグインを通った track の音と click を一致させる。
    #[test]
    fn master_latency_samples_is_the_max_path_latency_reaching_master() {
        // latency 無しなら 0 (= 補償不要)。
        let plain = Song {
            tracks: vec![track(|t| t.id = 1)],
            ..Song::default()
        };
        assert_eq!(
            compile_schedule_for_test(&plain, 48_000, 0).unwrap().master_latency_samples,
            0
        );

        // 直列でない 2 track のうち片方が 2048 sample 報告 → master は 2048 で揃う。
        let mut lat = DeviceLatencies::new();
        let latent = Song {
            tracks: vec![
                track(|t| t.id = 1),
                track(|t| {
                    t.id = 2;
                    t.devices = latency_chain(&mut lat, 20, 2048);
                }),
            ],
            ..Song::default()
        };
        assert_eq!(
            compile_schedule(&latent, &lat, 48_000, 0).unwrap().master_latency_samples,
            2048
        );

        // group 経由 (= 親を指す子がいる) でも、子の latency が group の path latency
        // として master へ伝わる。master src は group 1 本だけ。
        let mut lat = DeviceLatencies::new();
        let grouped = Song {
            tracks: vec![
                track(|t| t.id = 1),
                track(|t| {
                    t.id = 2;
                    t.parent_group_id = Some(1);
                    t.devices = latency_chain(&mut lat, 20, 512);
                }),
            ],
            ..Song::default()
        };
        assert_eq!(
            compile_schedule(&grouped, &lat, 48_000, 0).unwrap().master_latency_samples,
            512
        );

        // docs/plan_master_strip.md §2: マスターリミッター ON はルックアヘッド (5ms =
        // 48kHz で 240) を出力遅延として足す。OFF (既定 = 上の全ケース) は足さない。
        let mut limited = Song {
            tracks: vec![track(|t| t.id = 1)],
            ..Song::default()
        };
        limited.master_strip.limiter.on = true;
        assert_eq!(
            compile_schedule_for_test(&limited, 48_000, 0).unwrap().master_latency_samples,
            common::model::limiter_lookahead_samples(48_000),
            "リミッター ON のルックアヘッドは master 出力の遅延"
        );
        assert_eq!(common::model::limiter_lookahead_samples(48_000), 240);
    }

    /// r.md #39: `master_latency_samples` は master **出力** の遅延量なので、
    /// send/return・パラアウト・master fx のどの経路で latency が入っても拾う。
    /// (レビュー指摘: 並列 2 track と group しか見ていなかった。)
    #[test]
    fn master_latency_samples_covers_send_paraout_and_master_fx() {
        use common::model::{AuxOutputRoute, PluginInstance, Send, SendMode};
        use common::plugin_format::PluginFormat;

        // (a) send/return: Vocal → Reverb(latency 100)。return が master src なので
        //     master 合流は 100 に揃う。
        let mut lat = DeviceLatencies::new();
        let sends = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.sends = vec![Send {
                        id: 1,
                        dest_track_id: 2,
                        gain: 0.5,
                        mode: SendMode::PostFader,
                        enabled: true,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.devices = latency_chain(&mut lat, 20, 100);
                }),
            ],
            ..Song::default()
        };
        assert_eq!(
            compile_schedule(&sends, &lat, 48_000, 0).unwrap().master_latency_samples,
            100,
            "send 先 (return) の latency も master 合流に効く"
        );

        // (b) パラアウト: Drums.aux → Snare(latency 256)。dest は独立 bus なので
        //     source の path latency を取り込んだ上で自分の chain latency を足す。
        let mut lat = DeviceLatencies::new();
        lat.insert(10, 64);
        let paraout = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.devices = vec![PluginInstance {
                        id: 10,
                        aux_outputs: vec![Some(AuxOutputRoute::to_track(4))],
                        aux_output_count: 1,
                        ..PluginInstance::with_ports(
                            "test.drum_sampler".into(),
                            PluginFormat::Clap,
                            instrument_ports(),
                        )
                    }];
                }),
                track(|t| {
                    t.id = 4;
                    t.devices = latency_chain(&mut lat, 40, 256);
                }),
            ],
            ..Song::default()
        };
        assert_eq!(
            compile_schedule(&paraout, &lat, 48_000, 0).unwrap().master_latency_samples,
            64 + 256,
            "paraout dest は source の path latency を取り込む"
        );

        // (c) master fx: track 側 latency 0 でも master chain の報告 latency を拾う。
        //     旧実装は master Mix の src だけを見ていたので 0 のまま = click が先行した。
        let mut lat = DeviceLatencies::new();
        let master_fx = Song {
            tracks: vec![track(|t| t.id = 1)],
            master_fx_chain: latency_chain(&mut lat, 90, 2048),
            ..Song::default()
        };
        assert_eq!(
            compile_schedule(&master_fx, &lat, 48_000, 0).unwrap().master_latency_samples,
            2048,
            "master fx chain の latency も master 出力の遅延"
        );

        // (d) track 側と master fx は加算 (直列なので両方遅れる)。
        let mut lat = DeviceLatencies::new();
        let both = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.devices = latency_chain(&mut lat, 20, 512);
                }),
                track(|t| t.id = 2),
            ],
            master_fx_chain: latency_chain(&mut lat, 90, 2048),
            ..Song::default()
        };
        assert_eq!(
            compile_schedule(&both, &lat, 48_000, 0).unwrap().master_latency_samples,
            512 + 2048
        );
    }

    /// Compile-level test: 親子無しの 2 track が並行に master へ流れるとき、
    /// 片方のみが latency 100 を report していたら、 もう片方 (latency 0)
    /// に対して `ApplyDelay { frames: 100 }` を Master Mix の **直前** に
    /// 挿入し、 必要な DelayLine を `Schedule::delay_lines` に確保すべき。
    ///
    /// 仕様根拠: Ardour `libs/ardour/route.cc::process_output_buffers` —
    /// 各ルート内の effective_latency を直列加算し、 sink (Master) で全
    /// path を最大値に揃えるため、 latency が小さい path に DelayLine を
    /// 挿入する。
    #[test]
    fn pdc_parallel_tracks_emit_compensating_delay_for_lower_latency_path() {
        let mut lat = DeviceLatencies::new();
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Clean".into();
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Latent".into();
                    t.devices = latency_chain(&mut lat, 20, 100);
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, 0).unwrap();

        // (a) DelayLine が 1 本以上、 capacity ≥ 100 で確保されている。
        assert!(
            sched
                .delay_lines
                .iter()
                .any(|dl| dl.capacity() >= 100),
            "compile_schedule must allocate a DelayLine for the laggard's compensation; \
             got delay_lines.len()={}",
            sched.delay_lines.len()
        );

        // (b) Master Mix の **直前** に latency=0 path (TrackScratch(0)) へ
        //     ApplyDelay { frames: 100 } が刺さっている。
        let master_mix_pos = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::Mix {
                        dst: BufRef::Master,
                        ..
                    }
                )
            })
            .expect("Master Mix must exist");
        let apply_pos = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::ApplyDelay {
                        buf: BufRef::TrackScratch(0),
                        frames: 100,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "ApplyDelay {{ buf: TrackScratch(0), frames: 100 }} must be inserted \
                     before Master Mix; got nodes={:?}",
                    sched.nodes
                )
            });
        assert!(
            apply_pos < master_mix_pos,
            "ApplyDelay must come before Master Mix"
        );

        // (c) latency が大きい側 (TrackScratch(1)) には DelayLine 不要 —
        //     こちらは「全 path の max」と同じ累積 latency を持つので、
        //     compensation を入れると余計な遅延になる。
        let laggard_has_apply = sched.nodes.iter().any(|op| {
            matches!(
                op,
                NodeOp::ApplyDelay {
                    buf: BufRef::TrackScratch(1),
                    ..
                }
            )
        });
        assert!(
            !laggard_has_apply,
            "the highest-latency path should NOT receive an ApplyDelay"
        );
    }

    /// 数値テスト: 各 track に「latency を持つ plugin」 をロードした状態で
    /// 同一の impulse を input すると、 PDC 無しでは plugin の遅延だけ
    /// master の合流点で時間がずれる (= 「トラック間の音ずれ」)。 PDC が
    /// 効いていれば、 低 latency path が補償されて master 上の単一 peak
    /// に収束する。
    ///
    /// 構成:
    ///   Track A (id=1) ← LatencyPlugin(0)   identity
    ///   Track B (id=2) ← LatencyPlugin(100) input を 100 sample 遅延
    ///   両 track に impulse @sample 0 を入力 → master へ合流
    ///
    /// 期待:
    ///   PDC OK → master_l[100] ≈ 2.0、 他は 0
    ///   PDC NG → master_l[0]   ≈ 1.0  (A だけ即時) , master_l[100] ≈ 1.0 (B 遅延)
    ///            → これが「音ずれ」 で、 本テストはこの状況を assertion で検出
    #[test]
    fn pdc_two_track_impulse_aligns_at_master_with_loaded_latency_plugin() {
        const FRAMES: usize = 256;

        let mut lat = DeviceLatencies::new();
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "A".into();
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "B".into();
                    t.devices = latency_chain(&mut lat, 20, 100);
                }),
            ],
            ..Song::default()
        };
        let mut sched = compile_schedule(&song, &lat, 48_000, 0).unwrap();

        // Track ごとに「ロードされた plugin」 を持たせる。 production の
        // CLAP/VST3 と違って format-agnostic な test stub だが、
        //   - state を持つ (history ring buffer)
        //   - process(input -> output) に latency 分の遅延を入れる
        // という意味で「latency を持つ loaded plugin」 そのもの。 Track の
        // 並び (idx) → plugin のマップで保持する。
        let mut plugins: Vec<LatencyPlugin> =
            vec![LatencyPlugin::new(0), LatencyPlugin::new(100)];

        // 各 track の scratch (stereo)。 ProcessTrack ハンドラで
        // plugin.process(input) を呼んだ結果を書き込む。
        let mut scratch_l: Vec<Vec<f32>> = vec![vec![0.0; FRAMES]; 2];
        let mut scratch_r: Vec<Vec<f32>> = vec![vec![0.0; FRAMES]; 2];

        // 共通入力: impulse @sample 0
        let mut input_l = vec![0.0f32; FRAMES];
        let mut input_r = vec![0.0f32; FRAMES];
        input_l[0] = 1.0;
        input_r[0] = 1.0;

        let mut master_l = vec![0.0f32; FRAMES];
        let mut master_r = vec![0.0f32; FRAMES];

        // production の engine.rs:917-968 の dispatch loop を test 用に複製。
        // ProcessTrack で「ロード済 plugin」 の process() を回し、 Mix /
        // ApplyDelay は production と同じロジック。
        for op in &mut sched.nodes {
            match op {
                NodeOp::ProcessTrack { track_idx } => {
                    let i = *track_idx as usize;
                    plugins[i].process(
                        &input_l,
                        &input_r,
                        &mut scratch_l[i],
                        &mut scratch_r[i],
                    );
                }
                NodeOp::ProcessGroupFx { .. }
                | NodeOp::SidechainTap { .. }
                | NodeOp::MixSend { .. }
                | NodeOp::MixAdditive { .. }
                | NodeOp::ParallelOutTap { .. } => {
                    // この test では未使用
                }
                NodeOp::Mix {
                    srcs,
                    dst: BufRef::Master,
                } => {
                    for (b, gain) in srcs.iter() {
                        let BufRef::TrackScratch(i) = b else {
                            continue;
                        };
                        let i = *i as usize;
                        for j in 0..FRAMES {
                            master_l[j] += scratch_l[i][j] * gain;
                            master_r[j] += scratch_r[i][j] * gain;
                        }
                    }
                }
                NodeOp::Mix {
                    srcs,
                    dst: BufRef::TrackScratch(target_idx),
                } => {
                    let target = *target_idx as usize;
                    let mut new_l = vec![0.0f32; FRAMES];
                    let mut new_r = vec![0.0f32; FRAMES];
                    for (b, gain) in srcs.iter() {
                        let BufRef::TrackScratch(i) = b else {
                            continue;
                        };
                        let i = *i as usize;
                        for j in 0..FRAMES {
                            new_l[j] += scratch_l[i][j] * gain;
                            new_r[j] += scratch_r[i][j] * gain;
                        }
                    }
                    scratch_l[target] = new_l;
                    scratch_r[target] = new_r;
                }
                NodeOp::Mix { .. } => {
                    // Pooled / PluginAuxOut: PR4
                }
                NodeOp::ApplyDelay {
                    buf,
                    line_idx,
                    frames,
                } => {
                    let BufRef::TrackScratch(i) = buf else {
                        continue;
                    };
                    let i = *i as usize;
                    let line = &mut sched.delay_lines[*line_idx as usize];
                    let in_l = scratch_l[i].clone();
                    let in_r = scratch_r[i].clone();
                    line.step(
                        &in_l,
                        &in_r,
                        &mut scratch_l[i],
                        &mut scratch_r[i],
                        *frames as usize,
                    );
                }
                NodeOp::EnvelopeFollow { .. } => {
                    // followers produce only control-rate scalars; they do
                    // not affect the audio output exercised by this test.
                }
            }
            // input は 1 buffer 分だけ消費するので、 2 回目以降は input を
            // 0 で埋める必要は無い (ProcessTrack はループ中に 2 回呼ばれない
            // 想定。 ループは 1 buffer 1 イテレーション)。
            let _ = (&input_l, &input_r);
        }

        // (a) sample 0 には peak が立たない (= Track A の出力が PDC で
        //     100 sample 遅延されて、 sample 0 の地点には何も無い)。
        assert!(
            master_l[0].abs() < 1e-6,
            "master_l[0] should be 0 after PDC, got {} (= track misalignment)",
            master_l[0]
        );

        // (b) sample 100 で 2 track の impulse が重なって peak になる。
        assert!(
            (master_l[100] - 2.0).abs() < 1e-6,
            "master_l[100] should be ~2.0 (both tracks' impulses aligned), got {}",
            master_l[100]
        );

        // (c) sample 100 以外は 0 (= 1 つの peak だけ、 「音ずれ」 なし)。
        for (i, &v) in master_l.iter().enumerate() {
            if i == 100 {
                continue;
            }
            assert!(
                v.abs() < 1e-6,
                "master_l[{}] should be 0, got {} (= track misalignment)",
                i,
                v
            );
        }

        // (d) r.md #39: この peak 位置こそが `master_latency_samples` の定義
        //     — `master_buffer[P]` に載っているのは曲位置 `P - master_latency_samples`。
        //     metronome click の参照位置と WAV 書き出し窓は、どちらもこの値を引くことで
        //     曲位置に揃う (`engine.rs` の click_pos / `export.rs` の
        //     `shift_window_for_master_latency`)。ここが崩れると両方が同時に壊れる。
        assert_eq!(
            sched.master_latency_samples, 100,
            "曲位置 0 の impulse は master[master_latency_samples] に現れる"
        );
    }

    /// テスト専用「latency を持つ plugin」 stub。 production の `LoadedPlugin`
    /// trait は format 固有 (CLAP/VST3) の重い API を必要とするため、 PDC
    /// グラフレイヤを単独で検証するためだけの最小 stub を test mod 内に置く。
    /// `process(input -> output)` で `latency` サンプルだけ遅延した出力を返す。
    struct LatencyPlugin {
        latency: usize,
        hist_l: Vec<f32>,
        hist_r: Vec<f32>,
        write: usize,
        cap: usize,
    }

    impl LatencyPlugin {
        fn new(latency: usize) -> Self {
            // capacity = latency + 1 で「latency 分の遅延」 を厳密に再現
            // (DelayLine の clamp 仕様と整合)。
            let cap = latency + 1;
            Self {
                latency,
                hist_l: vec![0.0; cap],
                hist_r: vec![0.0; cap],
                write: 0,
                cap,
            }
        }

        fn process(
            &mut self,
            in_l: &[f32],
            in_r: &[f32],
            out_l: &mut [f32],
            out_r: &mut [f32],
        ) {
            let n = in_l.len().min(in_r.len()).min(out_l.len()).min(out_r.len());
            if self.latency == 0 {
                out_l[..n].copy_from_slice(&in_l[..n]);
                out_r[..n].copy_from_slice(&in_r[..n]);
                return;
            }
            for i in 0..n {
                self.hist_l[self.write] = in_l[i];
                self.hist_r[self.write] = in_r[i];
                let read = (self.write + self.cap - self.latency) % self.cap;
                out_l[i] = self.hist_l[read];
                out_r[i] = self.hist_r[read];
                self.write = (self.write + 1) % self.cap;
            }
        }
    }

    // ---- PR4 Sidechain ----

    /// Compile-level test: Track 2 (index 1) の device 0 (device_id 77) に
    /// `aux_inputs = [Some(track_1)]` が設定されているとき、
    /// `compile_schedule` は次の順で nodes を emit する。
    ///
    /// 1. `ProcessTrack(0)` (Track 1、 source)
    /// 2. `SidechainTap { src: TrackScratch(0), device_id: 77, aux_in_port: 0 }`
    /// 3. `ProcessTrack(1)` (Track 2、 receiver)
    /// 4. `Mix` → Master
    ///
    /// 順序が肝: SidechainTap は Track 1 の scratch が埋まった **後** で
    /// Track 2 の plugin が process() を呼ばれる **前** に挿入される。
    /// engine 側はこの op を見て plugin の `pd.buffer_aux_in[0]` に
    /// Track 1 の signal を copy してから plugin.process() を dispatch する。
    /// v29: 宛先 plugin は安定 device id で焼き込まれる。
    #[test]
    fn sidechain_emits_tap_before_destination_process_track() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Source".into();
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Dest".into();
                    t.devices = vec![PluginInstance {
                        id: 77,
                        // aux input port 0 ← Track 1's output
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.compressor".into(),
                            PluginFormat::Vst3,
                            audio_fx_ports(),
                        )
                    }];
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

        // (a) SidechainTap が emit されている (安定 device id で addressing)。
        let tap_idx = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::SidechainTap {
                        src: BufRef::TrackScratch(0),
                        device_id: 77,
                        aux_in_port: 0,
                    }
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected SidechainTap (src=TrackScratch(0), device_id=77, port=0); \
                     nodes={:?}",
                    sched.nodes
                )
            });

        // (b) source track の ProcessTrack が tap より前 (= source scratch
        //     が埋まってから tap で copy される)。
        let src_proc_idx = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 }))
            .expect("ProcessTrack(0) missing");
        assert!(
            src_proc_idx < tap_idx,
            "source ProcessTrack must run before SidechainTap: src={src_proc_idx} tap={tap_idx}"
        );

        // (c) destination track の ProcessTrack が tap より後 (= plugin
        //     process() が呼ばれる前に sidechain buffer が埋まる)。
        let dst_proc_idx = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 1 }))
            .expect("ProcessTrack(1) missing");
        assert!(
            tap_idx < dst_proc_idx,
            "SidechainTap must run before destination ProcessTrack: tap={tap_idx} dst={dst_proc_idx}"
        );
    }

    #[test]
    fn master_fx_sidechain_emits_tap_after_master_mix() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        // Track 1 → master bus fx[0] の aux input。 master fx の SidechainTap は
        // master Mix の **後** (source scratch 確定後) に emit される。
        let song = Song {
            tracks: vec![track(|t| {
                t.id = 1;
                t.name = "Source".into();
            })],
            master_fx_chain: vec![PluginInstance {
                id: 900,
                aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                ..PluginInstance::with_ports(
                    "test.bus_comp".into(),
                    PluginFormat::Vst3,
                    audio_fx_ports(),
                )
            }],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

        let tap_idx = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::SidechainTap {
                        src: BufRef::TrackScratch(0),
                        device_id: 900,
                        aux_in_port: 0,
                    }
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected master SidechainTap (src=TrackScratch(0), device_id=900); \
                     nodes={:?}",
                    sched.nodes
                )
            });

        // master Mix が tap より前 (= 全 track mix 後に source scratch から copy)。
        let master_mix_idx = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::Mix { dst: BufRef::Master, .. }))
            .expect("master Mix missing");
        assert!(
            master_mix_idx < tap_idx,
            "master SidechainTap must run after master Mix: mix={master_mix_idx} tap={tap_idx}"
        );
    }

    /// PR4 sidechain × PDC integration: dest track の plugin が source track
    /// から sidechain 入力を受けている場合、 dest 自身の `path_latency` は
    /// 「sidechain source の path_latency」 を **input bus latency として
    /// 取り込んだ上で** dest 自身の chain latency を加算する。 こうすると
    /// master mix の sibling alignment が source の遅延分も補償する。
    ///
    /// 仕様根拠: Ardour `route.cc` の `Latent` 基底クラス + sidechain
    /// (`Send`/`PluginInsert::sidechain_input`) の implicit synchronization。
    /// 実装上は sidechain edge を `compute_path_latency` の input fan-in に
    /// 加算するだけで graph layer の不変量が成立する。
    ///
    /// セットアップ:
    ///   Track A (id=1, latency 100) → master, source for sidechain
    ///   Track B (id=2, latency 50, fx slot 0 sidechain ← A) → master
    ///
    /// 修正前 (PDC が sidechain edge を見ていない):
    ///   path_latency(A) = 100, path_latency(B) = 50
    ///   master mix max = 100, B が 50 サンプル遅延される (B が low-latency)。
    ///   → B の plugin は main を「即時」、 aux を A の遅延済み信号で受ける。
    ///   master 上では B が遅延されて A と「揃う」 が、 plugin 内部の sidechain
    ///   検出は時間軸ずれの状態。 さらに B の output に master mix delay が
    ///   掛かるので musical alignment が壊れる方向に動く。
    ///
    /// 修正後 (sidechain edge を path_latency に取り込む):
    ///   path_latency(B) = max(0, path_latency(A)=100) + 50 = 150
    ///   master mix max = 150, A が 50 サンプル遅延される (A が low-latency)。
    ///   → master 上の musical alignment が一致する。 sibling drift が消える。
    ///
    /// 残課題 (本テスト範囲外): plugin の main vs aux 内部 alignment は per-slot
    /// chain prefix latency が必要なので、 別 PR で `DelayTrackInput` op を
    /// 入れて対応。
    #[test]
    fn pdc_sidechain_source_path_latency_propagates_to_dest() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        let mut lat = DeviceLatencies::new();
        lat.insert(20, 50);
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Source".into();
                    t.devices = latency_chain(&mut lat, 10, 100);
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Dest".into();
                    t.devices = vec![PluginInstance {
                        id: 20,
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.compressor".into(),
                            PluginFormat::Vst3,
                            audio_fx_ports(),
                        )
                    }];
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, 0).unwrap();

        // Master Mix の input は (TrackScratch(0), TrackScratch(1)) の 2 本。
        // path_latency(A=0) = 100, path_latency(B=1) = 100 + 50 = 150 になっている
        // はずなので、 max=150 に対し A (=100) を 50 サンプル遅延する `ApplyDelay`
        // が master mix の直前に出る。
        let master_mix_pos = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::Mix {
                        dst: BufRef::Master,
                        ..
                    }
                )
            })
            .expect("Master Mix must exist");

        // Source (TrackScratch(0)) が低 latency 側、 50 サンプル補償される。
        let apply_pos = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::ApplyDelay {
                        buf: BufRef::TrackScratch(0),
                        frames: 50,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected ApplyDelay {{ TrackScratch(0), frames: 50 }} \
                     to align Source(latency 100) with Dest(input 100 + chain 50 = 150) \
                     before Master Mix; got nodes={:?}",
                    sched.nodes
                )
            });
        assert!(
            apply_pos < master_mix_pos,
            "compensating ApplyDelay must come before Master Mix"
        );

        // Dest (TrackScratch(1)) には ApplyDelay が刺さらない (= max-latency 側)。
        let dest_has_delay = sched.nodes.iter().any(|op| {
            matches!(
                op,
                NodeOp::ApplyDelay {
                    buf: BufRef::TrackScratch(1),
                    ..
                }
            )
        });
        assert!(
            !dest_has_delay,
            "the highest-latency path (Dest including sidechain input) must NOT receive ApplyDelay"
        );
    }

    /// PR4.5 plugin-internal alignment: dest track の fx_chain plugin が
    /// sidechain 入力を持つとき、 `Schedule::input_delay_per_track` に dest
    /// track の input_delay_samples として「sidechain source の max
    /// path_latency」 が記録される。 これは engine 側で `process_track_owned`
    /// が instrument 出力 → fx_chain の境目で delay を入れて plugin の
    /// main vs aux を時刻揃えするための spec。
    ///
    /// 本テストは graph layer のみを検証 (engine の delay 適用は別レイヤ)。
    /// fx_chain の sidechain は `input_delay` に反映、 midi_fx_chain /
    /// instrument の sidechain は反映しない (= MVP scope。 instrument の
    /// sidechain alignment は MIDI event 側も遅延させる必要があり、 別 PR)。
    #[test]
    fn pdc_sidechain_input_delay_recorded_for_dest_fx_chain_track() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        let mut lat = DeviceLatencies::new();
        lat.insert(20, 50);
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Source".into();
                    t.devices = latency_chain(&mut lat, 10, 100);
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Dest".into();
                    t.devices = vec![PluginInstance {
                        id: 20,
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.compressor".into(),
                            PluginFormat::Vst3,
                            audio_fx_ports(),
                        )
                    }];
                }),
                track(|t| {
                    t.id = 3;
                    t.name = "Bystander".into();
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, 0).unwrap();

        assert_eq!(
            sched.input_delay_per_track.len(),
            3,
            "input_delay_per_track must have one entry per track"
        );
        assert_eq!(
            sched.input_delay_per_track[0], 0,
            "Source has no sidechain inputs, so input_delay = 0"
        );
        assert_eq!(
            sched.input_delay_per_track[1], 100,
            "Dest receives sidechain from Source(path_latency=100), \
             so input_delay must equal Source.path_latency"
        );
        assert_eq!(
            sched.input_delay_per_track[2], 0,
            "Bystander has no sidechain wiring, input_delay = 0"
        );
    }

    /// §5 (arch refactor): **leaf** 宛の sidechain tap は staging (post-
    /// dispatch) と消費 (次 buffer の pass-1 process) が 1 buffer ずれるので、
    /// `buffer_frames` が入力遅延と path latency の両方に加算される。bus 宛
    /// (return の ProcessGroupFx) は同 buffer 消費なので加算されない。
    #[test]
    fn pdc_leaf_sidechain_tap_adds_one_buffer_of_lag() {
        use common::model::{PluginInstance, Send, SendMode};
        use common::plugin_format::PluginFormat;

        const BUF: u32 = 512;
        let sc_device = |id: u64| PluginInstance {
            id,
            aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
            ..PluginInstance::with_ports(
                "test.compressor".into(),
                PluginFormat::Vst3,
                audio_fx_ports(),
            )
        };
        let mut lat = DeviceLatencies::new();
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Source".into();
                    t.devices = latency_chain(&mut lat, 10, 100);
                    // Bus (id 3) へ send → id 3 は return bus になる。
                    t.sends = vec![Send {
                        id: 1,
                        dest_track_id: 3,
                        gain: 1.0,
                        mode: SendMode::PostFader,
                        enabled: true,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "LeafDest".into();
                    t.devices = vec![sc_device(20)];
                }),
                track(|t| {
                    t.id = 3;
                    t.name = "BusDest".into();
                    t.devices = vec![sc_device(30)];
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, BUF).unwrap();

        // leaf 宛: source path latency (100) + 1 buffer (512)。
        assert_eq!(
            sched.input_delay_per_track[1],
            100 + BUF,
            "leaf-destined tap must include the 1-buffer staging lag"
        );
        // bus 宛 (return): 同 buffer 消費なので lag 加算なし。入力遅延自体
        // bus では未使用 (ProcessGroupFx は input_delay を適用しない) だが、
        // 規則の対称性を検証する。
        assert_eq!(
            sched.input_delay_per_track[2],
            100,
            "bus-destined tap is consumed in the same buffer (no extra lag)"
        );
    }

    /// PR4.5 plugin-internal alignment: midi_fx_chain や instrument に
    /// sidechain wiring があっても `input_delay_per_track` には反映しない。
    /// これは MVP scope 制限 (instrument input は MIDI 経由なので audio
    /// stream に delay を入れるだけでは不十分、 MIDI event も遅延させる
    /// 必要がある — 別 PR)。
    #[test]
    fn pdc_sidechain_instrument_input_delay_skipped_in_mvp() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        let mut lat = DeviceLatencies::new();
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Source".into();
                    t.devices = latency_chain(&mut lat, 10, 100);
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Dest".into();
                    // v23 single-chain: an instrument device (note in + audio
                    // out) → derives as Instrument, NOT AudioEffect, so its
                    // sidechain does not contribute to input_delay_per_track.
                    t.devices = vec![PluginInstance {
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.synth".into(),
                            PluginFormat::Vst3,
                            instrument_ports(),
                        )
                    }];
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, 0).unwrap();

        // path_latency は instrument の sidechain も拾う (= 100 + 0 = 100)
        // ので master mix の sibling alignment は機能する。
        // ただし input_delay_per_track には乗らない (MVP scope)。
        assert_eq!(
            sched.input_delay_per_track[1], 0,
            "instrument sidechain は input_delay に反映しない (MVP scope)"
        );
    }

    /// PR4 sidechain × PDC: sidechain edge が cycle を作る (A→B→A) 場合、
    /// `compile_schedule` は `GraphError::Cycle` を返す。 graph layer で
    /// 検出しないと `compute_path_latency` が無限再帰する。
    #[test]
    fn sidechain_cycle_between_two_tracks_is_rejected() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        // A(id=1) の plugin が B(id=2) からの sidechain を、
        // B(id=2) の plugin が A(id=1) からの sidechain を要求 → cycle。
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "A".into();
                    t.devices = vec![PluginInstance {
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(2))],
                        ..PluginInstance::with_ports(
                            "test.compressor".into(),
                            PluginFormat::Vst3,
                            audio_fx_ports(),
                        )
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "B".into();
                    t.devices = vec![PluginInstance {
                        aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(1))],
                        ..PluginInstance::with_ports(
                            "test.compressor".into(),
                            PluginFormat::Vst3,
                            audio_fx_ports(),
                        )
                    }];
                }),
            ],
            ..Song::default()
        };
        assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle));
    }

    /// 同じく compile-level test: `sidechain_sources` の対象 track が
    /// 存在しない (DanglingReference) 場合は無視する (Tap を emit しない、
    /// schedule 全体は壊さない)。 schedule 自体の compile error にすると
    /// 編集中に他の compile error が track 間で連鎖して厄介なので、
    /// 寛容に扱う。
    #[test]
    fn sidechain_with_dangling_source_track_is_skipped() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        let song = Song {
            tracks: vec![track(|t| {
                t.id = 1;
                t.name = "Lone".into();
                t.devices = vec![PluginInstance {
                    // 存在しない track を指す → dangling、 Tap は emit されない
                    aux_inputs: vec![Some(common::model::AuxInputRoute::post_fader(99))],
                    ..PluginInstance::with_ports(
                        "test.compressor".into(),
                        PluginFormat::Vst3,
                        audio_fx_ports(),
                    )
                }];
            })],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        assert!(
            !sched
                .nodes
                .iter()
                .any(|op| matches!(op, NodeOp::SidechainTap { .. })),
            "dangling sidechain source must not emit SidechainTap; nodes={:?}",
            sched.nodes
        );
    }

    // ---- PR4 aux send / return ----

    #[test]
    fn send_emits_mixsend_into_return_bus_after_clear_before_group_fx() {
        use common::model::{Send, SendMode};

        // Vocal (id 1, idx 0) post-fader sends to Reverb (id 2, idx 1).
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Vocal".into();
                    t.sends = vec![Send {
                        id: 11,
                        dest_track_id: 2,
                        gain: 0.5,
                        mode: SendMode::PostFader,
                        enabled: true,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Reverb".into();
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();

        // Vocal is a leaf → ProcessTrack(0). Reverb has an incoming send,
        // so it is a bus → Mix(clear) + MixSend + ProcessGroupFx(1).
        let vocal_proc = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 }))
            .expect("Vocal ProcessTrack");
        let reverb_clear = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::Mix {
                        dst: BufRef::TrackScratch(1),
                        ..
                    }
                )
            })
            .expect("Reverb clearing Mix");
        let mixsend = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::MixSend {
                        src: BufRef::TrackScratch(0),
                        dst: BufRef::TrackScratch(1),
                        src_track_idx: 0,
                        send_id: 11,
                    }
                )
            })
            .unwrap_or_else(|| panic!("expected MixSend Vocal→Reverb; nodes={:?}", sched.nodes));
        let reverb_fx = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 1, .. }))
            .expect("Reverb ProcessGroupFx");

        assert!(
            vocal_proc < mixsend,
            "source must process before its send is mixed in"
        );
        assert!(
            reverb_clear < mixsend,
            "return scratch must be cleared before sends accumulate"
        );
        assert!(
            mixsend < reverb_fx,
            "sends must accumulate before the return's fx chain runs"
        );

        // Vocal still feeds master dry; Reverb feeds master wet.
        let master_idxs: Vec<u32> = sched
            .nodes
            .iter()
            .find_map(|op| match op {
                NodeOp::Mix {
                    dst: BufRef::Master,
                    srcs,
                } => Some(srcs.clone()),
                _ => None,
            })
            .unwrap()
            .iter()
            .map(|(b, _)| match b {
                BufRef::TrackScratch(i) => *i,
                other => panic!("unexpected master src {other:?}"),
            })
            .collect();
        assert!(master_idxs.contains(&0), "Vocal dry must still reach master");
        assert!(master_idxs.contains(&1), "Reverb return must reach master");
    }

    #[test]
    fn pre_fader_send_taps_pre_fader_scratch() {
        use common::model::{Send, SendMode};

        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Vocal".into();
                    t.sends = vec![Send {
                        id: 1,
                        dest_track_id: 2,
                        gain: 1.0,
                        mode: SendMode::PreFader,
                        enabled: true,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Cue".into();
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        assert!(
            sched.nodes.iter().any(|op| matches!(
                op,
                NodeOp::MixSend {
                    src: BufRef::PreFaderScratch(0),
                    dst: BufRef::TrackScratch(1),
                    ..
                }
            )),
            "pre-fader send must tap the source's PreFaderScratch; nodes={:?}",
            sched.nodes
        );
    }

    #[test]
    fn self_send_is_rejected_as_cycle() {
        use common::model::{Send, SendMode};

        let song = Song {
            tracks: vec![track(|t| {
                t.id = 1;
                t.name = "A".into();
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 1, // sends to itself
                    gain: 1.0,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            })],
            ..Song::default()
        };
        assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle));
    }

    #[test]
    fn send_loop_between_two_returns_is_rejected() {
        use common::model::{Send, SendMode};

        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "A".into();
                    t.sends = vec![Send {
                        id: 1,
                        dest_track_id: 2,
                        gain: 1.0,
                        mode: SendMode::PostFader,
                        enabled: true,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "B".into();
                    t.sends = vec![Send {
                        id: 1,
                        dest_track_id: 1,
                        gain: 1.0,
                        mode: SendMode::PostFader,
                        enabled: true,
                    }];
                }),
            ],
            ..Song::default()
        };
        assert_eq!(compile_schedule_for_test(&song, 48_000, 0).err(), Some(GraphError::Cycle));
    }

    #[test]
    fn send_to_dangling_dest_is_skipped() {
        use common::model::{Send, SendMode};

        let song = Song {
            tracks: vec![track(|t| {
                t.id = 1;
                t.name = "Lone".into();
                t.sends = vec![Send {
                    id: 1,
                    dest_track_id: 99, // no such track
                    gain: 1.0,
                    mode: SendMode::PostFader,
                    enabled: true,
                }];
            })],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).unwrap();
        assert!(
            !sched
                .nodes
                .iter()
                .any(|op| matches!(op, NodeOp::MixSend { .. })),
            "dangling send dest must not emit MixSend; nodes={:?}",
            sched.nodes
        );
        // With no valid routing the track stays a plain leaf.
        assert!(
            sched
                .nodes
                .iter()
                .any(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 })),
            "Lone with only a dangling send must remain a ProcessTrack leaf"
        );
    }

    #[test]
    fn send_source_latency_aligns_return_with_dry_at_master() {
        use common::model::{Send, SendMode};

        // Vocal (idx 0, latency 0) sends to Reverb (idx 1, reported
        // latency 100) and also feeds master dry. Reverb's path latency =
        // max(send src Vocal = 0) + 100 = 100, so at the master mix the
        // dry Vocal (latency 0) must be delayed 100 to align with the wet.
        let mut lat = DeviceLatencies::new();
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Vocal".into();
                    t.sends = vec![Send {
                        id: 1,
                        dest_track_id: 2,
                        gain: 0.5,
                        mode: SendMode::PostFader,
                        enabled: true,
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Reverb".into();
                    t.devices = latency_chain(&mut lat, 20, 100);
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, 0).unwrap();

        let master_mix = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::Mix {
                        dst: BufRef::Master,
                        ..
                    }
                )
            })
            .expect("master Mix");
        let apply = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::ApplyDelay {
                        buf: BufRef::TrackScratch(0),
                        frames: 100,
                        ..
                    }
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected ApplyDelay {{ TrackScratch(0), frames: 100 }} before master; \
                     nodes={:?}",
                    sched.nodes
                )
            });
        assert!(apply < master_mix, "dry-path delay must precede the master mix");

        // The MixSend taps Vocal's undelayed scratch before that delay is
        // applied (the send was already consumed by the Reverb bus).
        let mixsend = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::MixSend { .. }))
            .expect("MixSend");
        assert!(
            mixsend < apply,
            "the send must read the source before the master dry-delay mutates it"
        );
    }

    // ---- パラアウト (docs/plan_paraout.md) ----

    /// パラアウト 全部子 (docs/plan_paraout.md): a multi-out instrument on track
    /// A (id 1) routes EVERY output to a child — port 0 (MAIN) → 子2 (id 2),
    /// aux bus 0 (port 1) → 子3 (id 3) — both parenting back to A. A becomes a
    /// **pure bus** (keeps no own main); 子2/子3 are **paraout-dest buses**, no cycle:
    ///  - per port: a `ParallelOutTap` (port 0 = main, port 1.. = aux buses)
    ///  - A: a **clearing** `Mix` into its own scratch (no own main to keep) +
    ///    `ProcessGroupFx { start_device: split }` (suffix FX only)
    ///  - A must NOT emit `MixAdditive` (nothing of its own to preserve)
    ///  - children processed before A sums them
    #[test]
    fn paraout_all_children_clears_main_and_taps_every_port() {
        use common::model::{AuxOutputRoute, PluginInstance};
        use common::plugin_format::PluginFormat;

        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Drums".into();
                    t.devices = vec![PluginInstance {
                        id: 10,
                        // port 0 (main) → 子2, port 1 (aux bus 0) → 子3 = 全部子
                        aux_outputs: vec![
                            Some(AuxOutputRoute::to_track(2)),
                            Some(AuxOutputRoute::to_track(3)),
                        ],
                        aux_output_count: 2,
                        ..PluginInstance::with_ports(
                            "test.drum_sampler".into(),
                            PluginFormat::Clap,
                            instrument_ports(),
                        )
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Kick".into();
                    t.parent_group_id = Some(1);
                }),
                track(|t| {
                    t.id = 3;
                    t.name = "Snare".into();
                    t.parent_group_id = Some(1);
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).expect("全部子 must not cycle");

        // (a) ParallelOutTap per port: main(port0) → 子2(idx1), aux0(port1) → 子3(idx2).
        //     v29: source plugin は安定 device id (10) で焼き込まれる。
        assert!(
            sched.nodes.iter().any(|op| matches!(
                op,
                NodeOp::ParallelOutTap { device_id: 10, port: 0, dst_track: 1 }
            )),
            "expected ParallelOutTap main(port0) → 子2(idx1); nodes={:?}",
            sched.nodes
        );
        assert!(
            sched.nodes.iter().any(|op| matches!(
                op,
                NodeOp::ParallelOutTap { device_id: 10, port: 1, dst_track: 2 }
            )),
            "expected ParallelOutTap aux0(port1) → 子3(idx2)"
        );

        // (b) 全部子: A clears + sums ALL children via a clearing Mix (main went
        //     to 子2 via port 0), running suffix FX from the split (device 1).
        let sum_mix = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::Mix { dst: BufRef::TrackScratch(0), .. }))
            .expect("全部子 A must use a clearing Mix into its own scratch");
        assert!(
            sched
                .nodes
                .iter()
                .any(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 0, start_device: 1 })),
            "A's suffix FX must start at the split (device 1); nodes={:?}",
            sched.nodes
        );
        // (c) A keeps no own main, so it must NOT use MixAdditive.
        assert!(
            !sched.nodes.iter().any(|op| matches!(op, NodeOp::MixAdditive { .. })),
            "全部子 A must not emit MixAdditive (main went to 子2 via port 0)"
        );

        // (d) children's bus FX run before A sums them.
        let b_fx = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 1, .. }))
            .expect("子2 ProcessGroupFx");
        let c_fx = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 2, .. }))
            .expect("子3 ProcessGroupFx");
        assert!(
            b_fx < sum_mix && c_fx < sum_mix,
            "children must be processed before A's clearing Mix sums them"
        );

        // (e) A's clearing Mix carries both children.
        let srcs = sched
            .nodes
            .iter()
            .find_map(|op| match op {
                NodeOp::Mix { dst: BufRef::TrackScratch(0), srcs } => Some(srcs.clone()),
                _ => None,
            })
            .unwrap();
        let idxs: Vec<u32> = srcs
            .iter()
            .map(|(b, _)| match b {
                BufRef::TrackScratch(i) => *i,
                o => panic!("unexpected Mix src {o:?}"),
            })
            .collect();
        assert!(
            idxs.contains(&1) && idxs.contains(&2),
            "A must sum children 子2(1) and 子3(2); got {idxs:?}"
        );
    }

    /// Independent-topology paraout: A (id 1) routes an aux output to D (id 4)
    /// which is NOT a child of A (D → master directly). A stays a plain leaf
    /// (`ProcessTrack`, full chain), D becomes a paraout-dest bus, and the tap
    /// flows A.aux → D. No cycle, no MixAdditive (A has no children).
    #[test]
    fn paraout_independent_dest_keeps_source_a_leaf() {
        use common::model::{AuxOutputRoute, PluginInstance};
        use common::plugin_format::PluginFormat;

        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Drums".into();
                    t.devices = vec![PluginInstance {
                        id: 10,
                        aux_outputs: vec![Some(AuxOutputRoute::to_track(4))],
                        aux_output_count: 1,
                        ..PluginInstance::with_ports(
                            "test.drum_sampler".into(),
                            PluginFormat::Clap,
                            instrument_ports(),
                        )
                    }];
                }),
                track(|t| {
                    t.id = 4;
                    t.name = "Snare".into();
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule_for_test(&song, 48_000, 0).expect("independent paraout must not cycle");

        // A (idx 0) is a plain leaf: ProcessTrack, no MixAdditive.
        assert!(
            sched
                .nodes
                .iter()
                .any(|op| matches!(op, NodeOp::ProcessTrack { track_idx: 0 })),
            "source A must remain a leaf (ProcessTrack); nodes={:?}",
            sched.nodes
        );
        assert!(
            !sched.nodes.iter().any(|op| matches!(op, NodeOp::MixAdditive { .. })),
            "no MixAdditive when the source has no children"
        );
        // D (idx 1) is a paraout-dest bus receiving A's aux (device_id 10).
        assert!(
            sched.nodes.iter().any(|op| matches!(
                op,
                NodeOp::ParallelOutTap { device_id: 10, port: 0, dst_track: 1 }
            )),
            "expected ParallelOutTap A.dev(10).port0 → D(idx1); nodes={:?}",
            sched.nodes
        );
        assert!(
            sched
                .nodes
                .iter()
                .any(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 1, start_device: 0 })),
            "D must run as a bus (ProcessGroupFx start_device 0)"
        );
    }

    /// パラアウト PDC 楽器兼バス (docs/plan_paraout.md): main を親に残す
    /// (port 0 unrouted) group-with-instrument で子に latency 持ちプラグインが
    /// あると、 MixAdditive の直前に「A の main (dst scratch、 prefix 後で相対 0)」
    /// と latency の小さい子を最大 path latency に揃える `ApplyDelay` が入る。
    /// これが無いとキック (main) とスネア (子経由) がサンプルずれる。 emit を検証
    /// (ApplyDelay handler の数値正しさは既存の
    /// `pdc_two_track_impulse_aligns_at_master_with_loaded_latency_plugin` が担保)。
    #[test]
    fn paraout_instrument_bus_pdc_aligns_main_and_children() {
        use common::model::{AuxOutputRoute, PluginInstance};
        use common::plugin_format::PluginFormat;

        let mut lat = DeviceLatencies::new();
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Drums".into();
                    t.devices = vec![PluginInstance {
                        // port 0 (main) unrouted = 楽器兼バス (the kick stays on A);
                        // aux bus 0/1 (port 1/2) → 子2/子3.
                        aux_outputs: vec![
                            None,
                            Some(AuxOutputRoute::to_track(2)),
                            Some(AuxOutputRoute::to_track(3)),
                        ],
                        aux_output_count: 3,
                        ..PluginInstance::with_ports(
                            "test.drum_sampler".into(),
                            PluginFormat::Clap,
                            instrument_ports(),
                        )
                    }];
                }),
                track(|t| {
                    t.id = 2;
                    t.name = "Snare".into();
                    t.parent_group_id = Some(1);
                    t.devices = latency_chain(&mut lat, 20, 100); // 子に latency 持ち FX
                }),
                track(|t| {
                    t.id = 3;
                    t.name = "HiHat".into();
                    t.parent_group_id = Some(1);
                    // latency なし
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, 0).expect("must compile");

        let add_mix = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::MixAdditive { dst: BufRef::TrackScratch(0), .. }))
            .expect("A MixAdditive");

        // A の main (idx0) を子の max latency (100) に揃える ApplyDelay が
        // MixAdditive の直前に在る。
        assert!(
            sched.nodes[..add_mix].iter().any(|op| matches!(
                op,
                NodeOp::ApplyDelay { buf: BufRef::TrackScratch(0), frames: 100, .. }
            )),
            "A's instrument main must be delayed 100 to align with the latent child; nodes={:?}",
            sched.nodes
        );
        // latency の無い子 HiHat (idx2) も 100 揃え。
        assert!(
            sched.nodes[..add_mix].iter().any(|op| matches!(
                op,
                NodeOp::ApplyDelay { buf: BufRef::TrackScratch(2), frames: 100, .. }
            )),
            "HiHat (no latency) must be delayed 100 to align with Snare; nodes={:?}",
            sched.nodes
        );
        // latency 持ちの Snare (idx1) は max なので揃え不要 (ApplyDelay 無し)。
        assert!(
            !sched.nodes[..add_mix].iter().any(|op| matches!(
                op,
                NodeOp::ApplyDelay { buf: BufRef::TrackScratch(1), .. }
            )),
            "Snare (the max-latency child) needs no compensating delay"
        );
    }

    /// パラアウト独立 dest の PDC fan-in (docs/plan_paraout.md): source A の aux を
    /// 子でない D へ振ると、 D の path latency に A の latency が乗り、 master で
    /// 他トラックと揃う。 fan-in が無いと D が二重遅延 (= A.aux で既に遅れている
    /// のに更に PDC で遅らされる) になる。
    #[test]
    fn paraout_independent_dest_pdc_fans_in_source_latency() {
        use common::model::{AuxOutputRoute, PluginInstance};
        use common::plugin_format::PluginFormat;

        let mut lat = DeviceLatencies::new();
        lat.insert(10, 100); // source に latency
        let song = Song {
            tracks: vec![
                track(|t| {
                    t.id = 1;
                    t.name = "Drums".into();
                    t.devices = vec![PluginInstance {
                        id: 10,
                        aux_outputs: vec![Some(AuxOutputRoute::to_track(4))],
                        aux_output_count: 1,
                        ..PluginInstance::with_ports(
                            "test.drum_sampler".into(),
                            PluginFormat::Clap,
                            instrument_ports(),
                        )
                    }];
                }),
                track(|t| {
                    t.id = 4;
                    t.name = "Snare".into(); // 独立 dest (A の子でない)、 latency なし
                }),
                track(|t| {
                    t.id = 5;
                    t.name = "Dry".into(); // 整合相手、 latency なし
                }),
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song, &lat, 48_000, 0).expect("must compile");

        let master_mix = sched
            .nodes
            .iter()
            .position(|op| matches!(op, NodeOp::Mix { dst: BufRef::Master, .. }))
            .expect("master Mix");

        // D (idx1) は A.aux (100 遅れ) を受けるので path latency 100。 master で
        // 余計な ApplyDelay は入らない (既に max)。
        assert!(
            !sched.nodes[..master_mix].iter().any(|op| matches!(
                op,
                NodeOp::ApplyDelay { buf: BufRef::TrackScratch(1), .. }
            )),
            "independent dest D already carries source latency; must not be delayed again; nodes={:?}",
            sched.nodes
        );
        // Dry (idx2, latency 0) は master で 100 揃え (A/D が 100 で max)。
        assert!(
            sched.nodes[..master_mix].iter().any(|op| matches!(
                op,
                NodeOp::ApplyDelay { buf: BufRef::TrackScratch(2), frames: 100, .. }
            )),
            "the dry track must be delayed 100 to align with the paraout chain (A+D); nodes={:?}",
            sched.nodes
        );
    }
}
