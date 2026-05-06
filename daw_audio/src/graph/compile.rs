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

    // ---- detect parent-chain cycles via iterative DFS ----
    // depth: 0 = unvisited, 1 = on current path, 2 = fully explored.
    let mut state = vec![0u8; n];
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        let mut stack: Vec<u32> = vec![start as u32];
        while let Some(&top) = stack.last() {
            let s = state[top as usize];
            if s == 0 {
                state[top as usize] = 1;
                let track = &song.tracks[top as usize];
                if let Some(pid) = track.parent_group_id {
                    let pidx = id_to_idx[&pid];
                    match state[pidx as usize] {
                        0 => stack.push(pidx),
                        1 => return Err(GraphError::Cycle),
                        _ => {
                            state[top as usize] = 2;
                            stack.pop();
                        }
                    }
                } else {
                    state[top as usize] = 2;
                    stack.pop();
                }
            } else if s == 1 {
                // Children done — mark fully explored and pop.
                state[top as usize] = 2;
                stack.pop();
            } else {
                stack.pop();
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

    // ---- gather children per group track ----
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for (idx, t) in song.tracks.iter().enumerate() {
        if let Some(pid) = t.parent_group_id {
            children_of.entry(pid).or_default().push(idx as u32);
        }
    }

    // ---- emit ops in descending-depth order ----
    let mut order: Vec<u32> = (0..n as u32).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(depth[i as usize]));

    let mut nodes = Vec::with_capacity(n * 2 + 1);
    let mut master_srcs: Vec<(BufRef, f32)> = Vec::new();

    for &i in &order {
        let track = &song.tracks[i as usize];
        let track_idx = i;
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

    Ok(Schedule {
        nodes,
        delay_lines: Vec::new(),
        port_buffers: super::PortBufferPool::new(),
    })
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
}
