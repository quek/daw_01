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
use super::schedule::{BufRef, NodeOp, Schedule};

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

/// Compile a `Schedule` from `song`. PR2 supports the group hierarchy:
/// children → group `Mix` → `ProcessGroupFx` → upstream (parent group or
/// master). Tracks without a `parent_group_id` feed the master bus
/// directly.
pub fn compile_schedule(song: &Song) -> Result<Schedule, GraphError> {
    let n = song.tracks.len();
    if n == 0 {
        return Ok(Schedule {
            nodes: vec![NodeOp::Mix {
                srcs: Vec::new(),
                dst: BufRef::Master,
            }],
            delay_lines: Vec::new(),
            port_buffers: super::PortBufferPool::new(),
            input_delay_per_track: Vec::new(),
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
    let dep_edges_for = |idx: u32| -> Vec<u32> {
        let track = &song.tracks[idx as usize];
        let mut out: Vec<u32> = Vec::new();
        if let Some(kids) = children_of.get(&track.id) {
            out.extend(kids.iter().copied());
        }
        let push_chain_sc = |chain: &[common::model::PluginInstance],
                             out: &mut Vec<u32>| {
            for p in chain {
                for src_id_opt in &p.sidechain_sources {
                    if let Some(src_id) = src_id_opt
                        && let Some(&src_idx) = id_to_idx.get(src_id)
                    {
                        out.push(src_idx);
                    }
                }
            }
        };
        push_chain_sc(&track.midi_fx_chain, &mut out);
        if let Some(inst) = &track.instrument {
            for src_id_opt in &inst.sidechain_sources {
                if let Some(src_id) = src_id_opt
                    && let Some(&src_idx) = id_to_idx.get(src_id)
                {
                    out.push(src_idx);
                }
            }
        }
        push_chain_sc(&track.fx_chain, &mut out);
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

    // ---- compute depth = distance from root (no parent) toward leaves ----
    // Roots have depth 0; their immediate children have depth 1; etc.
    // We need to emit Audio nodes (and inner groups) before their parent
    // group, so we sort the schedule by *descending* depth.
    let mut depth = vec![u32::MAX; n];
    fn fill_depth(
        idx: u32,
        tracks: &[common::model::Track],
        id_to_idx: &HashMap<u32, u32>,
        depth: &mut [u32],
    ) -> u32 {
        if depth[idx as usize] != u32::MAX {
            return depth[idx as usize];
        }
        let d = match tracks[idx as usize].parent_group_id {
            None => 0,
            Some(pid) => {
                let pidx = id_to_idx[&pid];
                fill_depth(pidx, tracks, id_to_idx, depth) + 1
            }
        };
        depth[idx as usize] = d;
        d
    }
    for i in 0..n {
        fill_depth(i as u32, &song.tracks, &id_to_idx, &mut depth);
    }

    // ---- emit ops in descending-depth order ----
    // (children_of was gathered above for cycle detection — reuse it here.)
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(depth[i as usize]));

    let mut nodes = Vec::with_capacity(n * 2 + 1);
    let mut master_srcs: Vec<(BufRef, f32)> = Vec::new();

    for &i in &order {
        let track = &song.tracks[i as usize];
        let track_idx = i;
        // PR4 sidechain: emit `SidechainTap` for every plugin on this
        // track with a `sidechain_sources` entry pointing at a valid
        // source track. The tap must run **before** the plugin's own
        // `process()` (i.e. before ProcessTrack / ProcessGroupFx for
        // this track) so the engine can stage the source signal in the
        // plugin's `pd.buffer_aux_in[port]` shmem region.
        emit_sidechain_taps(track, track_idx, &id_to_idx, &mut nodes);

        if is_group.contains(&track.id) {
            // This track has children → it acts as a group bus. Sum
            // the children's scratches into its own scratch, then run
            // the audio fx chain + strip via ProcessGroupFx. PR2 phase
            // 1 keeps the group's own clips / instrument unused (they
            // were skipped in process_track_owned). Phase 5 will add
            // the Reaper-folder mixing where the group's own audio
            // also feeds the post-fx mix.
            let kids = children_of.get(&track.id).cloned().unwrap_or_default();
            let srcs: Vec<(BufRef, f32)> = kids
                .into_iter()
                .map(|c| (BufRef::TrackScratch(c), 1.0))
                .collect();
            nodes.push(NodeOp::Mix {
                srcs,
                dst: BufRef::TrackScratch(track_idx),
            });
            nodes.push(NodeOp::ProcessGroupFx { track_idx });
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

    // ---- PR3: Plugin Delay Compensation ----
    //
    // 各 track の **path latency** を計算し、 Mix の合流点で path 間の
    // 不一致を `ApplyDelay` で補償する。 Ardour の `Latent` 基底クラス +
    // `route.cc::process_output_buffers` の流儀:
    //
    //   path_latency(leaf)  = leaf.reported_latency_samples
    //   path_latency(group) = max(child.path_latency) + group.reported_latency_samples
    //
    // ※ group では子達が group bus に流れ込むときに既に sibling alignment が
    //   行われている (ここで挿入する `ApplyDelay` で揃う) ので、 group の
    //   入力 bus 時点では全員 `max(child.path_latency)` に揃っている。
    //   従って group 自身の path_latency = max + own。
    //
    // 補償ルール: 各 Mix ノードの srcs の中で `path_latency` が最も小さい側に、
    //   `frames = max_path - this_path` の `ApplyDelay` を **Mix の直前に**
    //   挿入する。 こうすると合流点で全 src の累積 latency が揃う。
    let mut path_latency = vec![u32::MAX; n];
    for i in 0..n {
        compute_path_latency(
            i as u32,
            &song.tracks,
            &is_group,
            &children_of,
            &id_to_idx,
            &mut path_latency,
        );
    }

    // 既存の nodes を線形に走査し、 Mix を見つけたらその直前に
    // `ApplyDelay` を挿入する。 in-place 操作よりも build-from-scratch
    // の方が境界条件が単純なので、 一度別 Vec に組み直す。
    let mut delay_lines: Vec<DelayLine> = Vec::new();
    let mut nodes_with_pdc: Vec<NodeOp> = Vec::with_capacity(nodes.len() + n);
    for op in nodes.into_iter() {
        if let NodeOp::Mix { ref srcs, .. } = op {
            // この Mix の入力中で最大の path latency を求める。
            let max_path = srcs
                .iter()
                .filter_map(|(b, _)| match b {
                    BufRef::TrackScratch(i) => Some(path_latency[*i as usize]),
                    _ => None,
                })
                .max()
                .unwrap_or(0);
            // 各 src を必要なら `ApplyDelay` で max に揃える。
            for (b, _) in srcs.iter() {
                let BufRef::TrackScratch(i) = b else {
                    continue;
                };
                let this = path_latency[*i as usize];
                if this < max_path {
                    let comp = max_path - this;
                    let line_idx = delay_lines.len() as u32;
                    // DelayLine.step は `delay <= capacity - 1` を要求
                    // (`delay_line.rs:55-56` の clamp ロジック)。 補償量
                    // ちょうどを返すために capacity = comp + 1。
                    delay_lines.push(DelayLine::with_capacity((comp as usize) + 1));
                    nodes_with_pdc.push(NodeOp::ApplyDelay {
                        buf: BufRef::TrackScratch(*i),
                        line_idx,
                        frames: comp,
                    });
                }
            }
        }
        nodes_with_pdc.push(op);
    }

    // PR4.5 sidechain plugin-internal alignment: per-track input delay.
    // Only fx_chain plugin sidechain_sources contribute (instrument sidechain
    // is MVP-out-of-scope because aligning it would require also delaying
    // MIDI events into the instrument). Walk each track's fx_chain and
    // pick `max(path_latency(src))` over all sidechain entries.
    let mut input_delay_per_track = vec![0u32; n];
    for (i, track) in song.tracks.iter().enumerate() {
        let mut max_sc: u32 = 0;
        for p in &track.fx_chain {
            for src_id_opt in &p.sidechain_sources {
                if let Some(src_id) = src_id_opt
                    && let Some(&src_idx) = id_to_idx.get(src_id)
                {
                    let l = path_latency[src_idx as usize];
                    if l > max_sc {
                        max_sc = l;
                    }
                }
            }
        }
        input_delay_per_track[i] = max_sc;
    }

    Ok(Schedule {
        nodes: nodes_with_pdc,
        delay_lines,
        port_buffers: super::PortBufferPool::new(),
        input_delay_per_track,
    })
}

/// PR4 sidechain: track の chain (`midi_fx_chain` / `instrument` /
/// `fx_chain`) を順に walk して、 各 plugin の `sidechain_sources` 中で
/// 有効な (= source track が song に存在する) entry について
/// `NodeOp::SidechainTap` を `nodes` に push する。 dangling reference は
/// 寛容に skip (compile error にしない)。 source track の `ProcessTrack`
/// が前に emit されていれば scratch が埋まっているので tap は意味を持つ。
fn emit_sidechain_taps(
    track: &common::model::Track,
    _dst_track_idx: u32,
    id_to_idx: &HashMap<u32, u32>,
    nodes: &mut Vec<NodeOp>,
) {
    let dst_track = track.id;
    let push_taps = |nodes: &mut Vec<NodeOp>,
                     plugins: &[common::model::PluginInstance],
                     slot_kind: fn(u32) -> common::protocol::PluginSlot| {
        for (slot_idx, inst) in plugins.iter().enumerate() {
            for (port_idx, src_track_id_opt) in inst.sidechain_sources.iter().enumerate() {
                let Some(src_track_id) = src_track_id_opt else {
                    continue;
                };
                let Some(src_idx) = id_to_idx.get(src_track_id) else {
                    // dangling reference: silently skip
                    continue;
                };
                nodes.push(NodeOp::SidechainTap {
                    src: BufRef::TrackScratch(*src_idx),
                    dst_track,
                    dst_slot: slot_kind(slot_idx as u32),
                    aux_in_port: port_idx as u8,
                });
            }
        }
    };
    push_taps(nodes, &track.midi_fx_chain, common::protocol::PluginSlot::MidiFx);
    if let Some(inst) = &track.instrument {
        for (port_idx, src_track_id_opt) in inst.sidechain_sources.iter().enumerate() {
            let Some(src_track_id) = src_track_id_opt else {
                continue;
            };
            let Some(src_idx) = id_to_idx.get(src_track_id) else {
                continue;
            };
            nodes.push(NodeOp::SidechainTap {
                src: BufRef::TrackScratch(*src_idx),
                dst_track,
                dst_slot: common::protocol::PluginSlot::Instrument,
                aux_in_port: port_idx as u8,
            });
        }
    }
    push_taps(nodes, &track.fx_chain, common::protocol::PluginSlot::Fx);
}

/// `path_latency[idx]` を計算してキャッシュする。 既に値があれば即返却
/// (memoization)。
///
/// PR3: group (`is_group` メンバ) は子の path_latency の最大値を自身の input
/// bus latency として、 そこに自身の `reported_latency_samples` を足す。
///
/// PR4 sidechain × PDC: track の plugin chain (`midi_fx_chain` / `instrument`
/// / `fx_chain`) の各 plugin の `sidechain_sources` も `input bus latency`
/// に取り込む。 すなわち:
///
///   input_latency(T) = max(
///       max(child.path_latency for child in children_of(T)),
///       max(source.path_latency for (P, source) in sidechain_inputs(T))
///   )
///   path_latency(T) = input_latency(T) + T.reported_latency_samples
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
fn compute_path_latency(
    idx: u32,
    tracks: &[common::model::Track],
    is_group: &HashSet<u32>,
    children_of: &HashMap<u32, Vec<u32>>,
    id_to_idx: &HashMap<u32, u32>,
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
                        compute_path_latency(c, tracks, is_group, children_of, id_to_idx, cache)
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    } else {
        0
    };

    let mut sidechain_input: u32 = 0;
    let mut consider = |src_id_opt: &Option<u32>, cache: &mut [u32]| {
        if let Some(src_id) = src_id_opt
            && let Some(&src_idx) = id_to_idx.get(src_id)
        {
            let l =
                compute_path_latency(src_idx, tracks, is_group, children_of, id_to_idx, cache);
            if l > sidechain_input {
                sidechain_input = l;
            }
        }
    };
    for p in &track.midi_fx_chain {
        for src in &p.sidechain_sources {
            consider(src, cache);
        }
    }
    if let Some(inst) = &track.instrument {
        for src in &inst.sidechain_sources {
            consider(src, cache);
        }
    }
    for p in &track.fx_chain {
        for src in &p.sidechain_sources {
            consider(src, cache);
        }
    }

    let max_input = group_input.max(sidechain_input);
    let total = max_input.saturating_add(track.reported_latency_samples);
    cache[idx as usize] = total;
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Song, Track};

    #[test]
    fn empty_song_compiles_to_master_only_mix() {
        let song = Song::default();
        let sched = compile_schedule(&song).unwrap();
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
    fn flat_audio_tracks_emit_process_then_mix() {
        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();
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
                Track {
                    id: 1,
                    name: "Drums".into(),
                    parent_group_id: None,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Kick".into(),
                    parent_group_id: Some(1),
                    ..Track::default()
                },
                Track {
                    id: 3,
                    name: "Snare".into(),
                    parent_group_id: Some(1),
                    ..Track::default()
                },
                Track {
                    id: 4,
                    name: "Lead".into(),
                    parent_group_id: None,
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();

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
                Track {
                    id: 1,
                    parent_group_id: None,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    parent_group_id: Some(1),
                    ..Track::default()
                },
                Track {
                    id: 3,
                    parent_group_id: Some(2),
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();

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
                Track {
                    id: 1,
                    parent_group_id: Some(2),
                    ..Track::default()
                },
                Track {
                    id: 2,
                    parent_group_id: Some(1),
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        assert_eq!(compile_schedule(&song).err(), Some(GraphError::Cycle));
    }

    #[test]
    fn audio_track_can_become_a_group_implicitly() {
        // With kind removed, any track that has a child IS a group.
        // Track 1 has no flag, but track 2 points at it via
        // parent_group_id, so track 1 is treated as a group bus.
        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    parent_group_id: Some(1),
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();
        // Track 1 should emit Mix → TrackScratch(0) + ProcessGroupFx(0),
        // not ProcessTrack(0).
        let has_group_fx = sched
            .nodes
            .iter()
            .any(|op| matches!(op, NodeOp::ProcessGroupFx { track_idx: 0 }));
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
            tracks: vec![Track {
                id: 1,
                parent_group_id: Some(99),
                ..Track::default()
            }],
            ..Song::default()
        };
        assert_eq!(
            compile_schedule(&song).err(),
            Some(GraphError::DanglingReference(99))
        );
    }

    // ---- PR3: Plugin Delay Compensation ----

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
        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Clean".into(),
                    reported_latency_samples: 0,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Latent".into(),
                    reported_latency_samples: 100,
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();

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

        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "A".into(),
                    reported_latency_samples: 0,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "B".into(),
                    reported_latency_samples: 100,
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let mut sched = compile_schedule(&song).unwrap();

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
                NodeOp::ProcessGroupFx { .. } | NodeOp::SidechainTap { .. } => {
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

    /// Compile-level test: Track 2 (index 1) の Fx(0) plugin に
    /// `sidechain_sources = [Some(track_1.id)]` が設定されているとき、
    /// `compile_schedule` は次の順で nodes を emit する。
    ///
    /// 1. `ProcessTrack(0)` (Track 1、 source)
    /// 2. `SidechainTap { src: TrackScratch(0), dst_track: 2,
    ///    dst_slot: Fx(0), aux_in_port: 0 }`
    /// 3. `ProcessTrack(1)` (Track 2、 receiver)
    /// 4. `Mix` → Master
    ///
    /// 順序が肝: SidechainTap は Track 1 の scratch が埋まった **後** で
    /// Track 2 の plugin が process() を呼ばれる **前** に挿入される。
    /// engine.rs はこの op を見て plugin の `pd.buffer_aux_in[0]` に
    /// Track 1 の signal を copy してから plugin.process() を dispatch する。
    #[test]
    fn sidechain_emits_tap_before_destination_process_track() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;
        use common::protocol::PluginSlot;

        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Source".into(),
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Dest".into(),
                    fx_chain: vec![PluginInstance {
                        plugin_id: "test.compressor".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        // aux input port 0 ← Track 1's output
                        sidechain_sources: vec![Some(1)],
                    }],
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();

        // (a) SidechainTap が emit されている。
        let tap_idx = sched
            .nodes
            .iter()
            .position(|op| {
                matches!(
                    op,
                    NodeOp::SidechainTap {
                        src: BufRef::TrackScratch(0),
                        dst_track: 2,
                        dst_slot: PluginSlot::Fx(0),
                        aux_in_port: 0,
                    }
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "expected SidechainTap (src=TrackScratch(0), dst=2,Fx(0), port=0); \
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

        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Source".into(),
                    reported_latency_samples: 100,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Dest".into(),
                    reported_latency_samples: 50,
                    fx_chain: vec![PluginInstance {
                        plugin_id: "test.compressor".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        sidechain_sources: vec![Some(1)],
                    }],
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();

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

        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Source".into(),
                    reported_latency_samples: 100,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Dest".into(),
                    reported_latency_samples: 50,
                    fx_chain: vec![PluginInstance {
                        plugin_id: "test.compressor".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        sidechain_sources: vec![Some(1)],
                    }],
                    ..Track::default()
                },
                Track {
                    id: 3,
                    name: "Bystander".into(),
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();

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

    /// PR4.5 plugin-internal alignment: midi_fx_chain や instrument に
    /// sidechain wiring があっても `input_delay_per_track` には反映しない。
    /// これは MVP scope 制限 (instrument input は MIDI 経由なので audio
    /// stream に delay を入れるだけでは不十分、 MIDI event も遅延させる
    /// 必要がある — 別 PR)。
    #[test]
    fn pdc_sidechain_instrument_input_delay_skipped_in_mvp() {
        use common::model::PluginInstance;
        use common::plugin_format::PluginFormat;

        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Source".into(),
                    reported_latency_samples: 100,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Dest".into(),
                    instrument: Some(PluginInstance {
                        plugin_id: "test.synth".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        sidechain_sources: vec![Some(1)],
                    }),
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();

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
                Track {
                    id: 1,
                    name: "A".into(),
                    fx_chain: vec![PluginInstance {
                        plugin_id: "test.compressor".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        sidechain_sources: vec![Some(2)],
                    }],
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "B".into(),
                    fx_chain: vec![PluginInstance {
                        plugin_id: "test.compressor".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        sidechain_sources: vec![Some(1)],
                    }],
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        assert_eq!(compile_schedule(&song).err(), Some(GraphError::Cycle));
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
            tracks: vec![Track {
                id: 1,
                name: "Lone".into(),
                fx_chain: vec![PluginInstance {
                    plugin_id: "test.compressor".into(),
                    format: PluginFormat::Vst3,
                    state: None,
                    sidechain_sources: vec![Some(99)], // 存在しない track
                }],
                ..Track::default()
            }],
            ..Song::default()
        };
        let sched = compile_schedule(&song).unwrap();
        assert!(
            !sched
                .nodes
                .iter()
                .any(|op| matches!(op, NodeOp::SidechainTap { .. })),
            "dangling sidechain source must not emit SidechainTap; nodes={:?}",
            sched.nodes
        );
    }
}
