//! バス合流と tap の解決 — `execute` から切り出した「信号をどこへ足すか」の層
//! (`docs/plan_arch_refactor.md` §5 の続き)。
//!
//! `execute.rs` は 1,000 実コード行の budget (不変条件 9) に迫っていたので、
//! **足す前に分割**した。ここに集めたのは「schedule の 1 op を実行するのに要る
//! 純粋な合流・走査」だけで、schedule の走り方 (順序 / plugin dispatch) は
//! `execute.rs` に残る。
//!
//! RT 規約: 全関数が audio callback / worker / export freewheel から呼ばれる。
//! ヒープ確保・ロック・I/O を行わない (`has_soloed_contributor` の BFS も
//! スタック上の固定長配列で回す)。

use common::model::{Song, Track};

use crate::engine::MAX_TRACKS;
use crate::graph::BufRef;
use crate::mixer::TrackScratch;

/// Resolve a tap `BufRef` (PostFader / PostFx / PreFx) to the source track's
/// `(L, R)` buffers. Returns `None` for a non-tap `BufRef` or out-of-range
/// track. docs/plan_modulation_followups.md §1. RT-safe (pure slicing).
pub(super) fn resolve_tap_buffers(scratch: &[TrackScratch], src: BufRef) -> Option<(&[f32], &[f32])> {
    Some(match src {
        BufRef::TrackScratch(i) => {
            let s = scratch.get(i as usize)?;
            (s.track_l.as_slice(), s.track_r.as_slice())
        }
        BufRef::PreFaderScratch(i) => {
            let s = scratch.get(i as usize)?;
            (s.pre_fader_l.as_slice(), s.pre_fader_r.as_slice())
        }
        BufRef::PreFxScratch(i) => {
            let s = scratch.get(i as usize)?;
            (s.pre_fx_l.as_slice(), s.pre_fx_r.as_slice())
        }
        _ => return None,
    })
}

/// docs/plan_modulation.md §6 / docs/plan_modulation_followups.md §1: does any
/// aux-input route or mod source tap `track_id` exactly at `want`? Read-only
/// scan, no alloc — RT-safe.
pub(super) fn any_tap_at(song: &Song, track_id: u32, want: common::model::TapPoint) -> bool {
    let hit = |t: &common::model::AudioTap| t.source_track == track_id && t.tap_point == want;
    song.tracks
        .iter()
        .flat_map(|tr| tr.devices.iter())
        .chain(song.master_fx_chain.iter())
        .flat_map(|p| p.aux_inputs.iter().flatten())
        .any(|r| hit(&r.tap))
        // generator (LFO/Random/MSEG/Steps) は tap を持たない。 follower のみ走査。
        || song
            .mod_sources
            .iter()
            .filter_map(|m| m.follower())
            .any(|(tap, _)| hit(tap))
}

/// A `PostFx` tap reads the track's `pre_fader_l/r` snapshot (post-fx,
/// pre-strip), so the per-track render must capture it.
pub(super) fn track_needs_prefader_snapshot(song: &Song, track_id: u32) -> bool {
    any_tap_at(song, track_id, common::model::TapPoint::PostFx)
}

/// A `PreFx` tap reads the track's `pre_fx_l/r` snapshot (the raw signal
/// before the device chain), so the per-track render must capture it.
pub(super) fn track_needs_prefx_snapshot(song: &Song, track_id: u32) -> bool {
    any_tap_at(song, track_id, common::model::TapPoint::PreFx)
}

/// Sum the listed source scratches into `scratch[target_idx]` (used to
/// feed group buses with their children). Clears the target first so
/// stale samples from a previous buffer don't leak.
pub(super) fn mix_into_track_scratch(
    scratch: &mut [TrackScratch],
    target_idx: usize,
    srcs: &[(BufRef, f32)],
    n: usize,
    // `true` clears `dst` first (normal group / return Mix). `false`
    // accumulates on top of whatever is already there (パラアウト
    // group-with-instrument: keep the instrument's own main output written by
    // the pass-1 prefix before summing the children).
    clear: bool,
) {
    if target_idx >= scratch.len() {
        return;
    }
    if clear {
        let target = &mut scratch[target_idx];
        target.track_l[..n].fill(0.0);
        target.track_r[..n].fill(0.0);
    }
    let (left, right) = scratch.split_at_mut(target_idx);
    let (target_slot, after) = right.split_first_mut().expect("split bounds checked above");
    for (src, gain) in srcs {
        let BufRef::TrackScratch(s_idx) = src else {
            continue;
        };
        let s = *s_idx as usize;
        if s == target_idx {
            continue;
        }
        let s_scratch = if s < target_idx {
            &left[s]
        } else if s - target_idx - 1 < after.len() {
            &after[s - target_idx - 1]
        } else {
            continue;
        };
        if s_scratch.effective_mute {
            continue;
        }
        let g = *gain;
        for i in 0..n {
            target_slot.track_l[i] += s_scratch.track_l[i] * g;
            target_slot.track_r[i] += s_scratch.track_r[i] * g;
        }
    }
}

/// Sum each non-muted source scratch (with its routing gain) into the
/// master bus. The master buffers are zeroed earlier in the render so
/// this is `+=` style accumulation.
pub(super) fn mix_into_master(
    scratch: &[TrackScratch],
    srcs: &[(BufRef, f32)],
    master_l: &mut [f32],
    master_r: &mut [f32],
    n: usize,
) {
    let n = n.min(master_l.len()).min(master_r.len());
    for (src, gain) in srcs {
        let BufRef::TrackScratch(s_idx) = src else {
            continue;
        };
        let Some(s_scratch) = scratch.get(*s_idx as usize) else {
            continue;
        };
        if s_scratch.effective_mute {
            continue;
        }
        let g = *gain;
        for i in 0..n {
            master_l[i] += s_scratch.track_l[i] * g;
            master_r[i] += s_scratch.track_r[i] * g;
        }
    }
}

/// Accumulate one aux send into a return / bus scratch.
///
/// Reads `scratch[src_idx]`'s post-fader (`track_l/r`) or pre-fader
/// (`pre_fader_l/r`) buffer, scales it by the **live** send gain of
/// the send with stable id `send_id` on `song.tracks[src_track_idx]` —
/// sampled per-sample from a `SendGain` automation lane when present (and
/// not being recorded), otherwise the constant `send.gain` — and adds it
/// into `scratch[dst_idx].track_l/r` (`+=`, no clear). A disabled send or a
/// muted source contributes nothing (Ableton: mute silences sends). The
/// gain is read live, never baked into the schedule, so knob drags and
/// `SendGain` automation apply without recompiling.
#[allow(clippy::too_many_arguments)]
pub(super) fn mix_send_into_track_scratch(
    scratch: &mut [TrackScratch],
    dst_idx: usize,
    src_idx: usize,
    pre_fader: bool,
    song: &Song,
    src_track_idx: u32,
    send_id: u32,
    sample_rate: u32,
    bpm: f32,
    // 積分済み拍位置 (buffer 先頭)。 SendGain lane は beat-domain で読む
    // (M5 の beat-domain 統一)。
    playhead_beats: f64,
    any_solo: bool,
    recording_lanes: &std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    n: usize,
) {
    use common::model::{AutomationTarget, TrackBuiltinParam};

    if src_idx == dst_idx || src_idx >= scratch.len() || dst_idx >= scratch.len() {
        return;
    }
    let Some(track) = song.tracks.get(src_track_idx as usize) else {
        return;
    };
    // v29: stable `Send::id` で live lookup (sends は高々数本 — 線形走査で
    // RT-safe)。 positional index は schedule に焼き込まれない。
    let Some(send) = track.sends.iter().find(|s| s.id == send_id) else {
        return;
    };
    if !send.enabled {
        return;
    }
    // An explicit mute on the source always silences its sends.
    if track.muted {
        return;
    }
    // Solo handling. Soloing a track should let you hear ONLY it and its
    // sends — other tracks' sends must NOT leak into a shared return. So
    // under solo a send flows only if its SOURCE is solo-audible (soloed,
    // or kept alive by a soloed child / send), OR the DESTINATION return is
    // itself explicitly soloed (you soloed the return to audition
    // everything routed to it). The source keeps its signal (see
    // process_track_owned), so the soloed-return audition still works.
    if any_solo {
        let dest_soloed = song.tracks.get(dst_idx).is_some_and(|d| d.solo);
        if !dest_soloed && !track.solo && !has_soloed_contributor(song, track.id) {
            return;
        }
    }

    // Pick this send's `SendGain` automation lane, unless it is currently
    // being recorded (then the live knob value is heard, mirroring the
    // volume / pan recording bypass). v29: lane target は stable send id で
    // 一致させる (`legacy_send_idx` は load 時の remap で常に None)。
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
        send_id,
        legacy_send_idx: None,
    });
    let lane = if recording_lanes.contains(&(track.id, target.clone())) {
        None
    } else {
        track
            .automation_lanes
            .iter()
            .find(|l| l.enabled && l.target == target)
    };
    let beats_per_frame = if bpm > 0.0 && sample_rate > 0 {
        f64::from(bpm) / (60.0 * f64::from(sample_rate))
    } else {
        0.0
    };
    let const_gain = send.gain;

    // Borrow the source immutably and the destination mutably without
    // overlap (`src_idx != dst_idx` checked above).
    let (src_scratch, dst_scratch): (&TrackScratch, &mut TrackScratch) = if src_idx < dst_idx {
        let (left, right) = scratch.split_at_mut(dst_idx);
        (&left[src_idx], &mut right[0])
    } else {
        let (left, right) = scratch.split_at_mut(src_idx);
        (&right[0], &mut left[dst_idx])
    };
    let (src_l, src_r) = if pre_fader {
        (&src_scratch.pre_fader_l, &src_scratch.pre_fader_r)
    } else {
        (&src_scratch.track_l, &src_scratch.track_r)
    };
    let n = n
        .min(src_l.len())
        .min(src_r.len())
        .min(dst_scratch.track_l.len())
        .min(dst_scratch.track_r.len());

    if let (Some(lane), true) = (lane, beats_per_frame > 0.0) {
        for i in 0..n {
            // `fill_track_param_ramps` / `fill_pd_param_events` と同じ積分済み
            // anchor + per-frame 増分 (M5 の beat-domain 統一)。
            let beat = playhead_beats + i as f64 * beats_per_frame;
            let g = common::automation::lane_value_at(lane, &song.clip_contents, beat) as f32;
            dst_scratch.track_l[i] += src_l[i] * g;
            dst_scratch.track_r[i] += src_r[i] * g;
        }
    } else {
        for i in 0..n {
            dst_scratch.track_l[i] += src_l[i] * const_gain;
            dst_scratch.track_r[i] += src_r[i] * const_gain;
        }
    }
}

/// `track_id` に流れ込む (= contribute する) track のいずれかが
/// `solo == true` なら true。 寄与エッジは「子 (`parent_group_id == node`、
/// group の soloed-via-children)」 と「`node` 宛ての aux send を持つ track
/// (= send 元、 return の solo-safe)」 の 2 種。 これで「あるトラックを
/// solo すると、 そのトラックが送っている reverb / delay の **リターン** も
/// 生かす」 Ableton 準拠の挙動になる (リターンを solo-safe にしないと、
/// ソロしたトラックの send 先が solo 規則で無音化され、 ソロ中はセンド
/// エフェクトが聞こえない)。 routing graph は DAG (`compile_schedule` が
/// cycle を弾く) なので BFS は停止する。 `hops` 上限は child + send の
/// fan-in を見込んで広めに取る。
pub(super) fn has_soloed_contributor(song: &Song, track_id: u32) -> bool {
    // RT-safe non-allocating BFS: this runs on the audio dispatch path, so
    // the frontier and the visited set must live on the stack rather than
    // heap-allocated `Vec`s. `MAX_TRACKS` (= 32) caps the number of distinct
    // nodes; the stack is sized to comfortably hold them. Track ids are not
    // dense indices, so the visited set stores ids directly.
    let mut frontier = [0u32; MAX_TRACKS * 2];
    let mut frontier_len = 0usize;
    let mut visited = [0u32; MAX_TRACKS];
    let mut visited_len = 0usize;

    // Seed with the starting node, marked visited so it is never re-pushed.
    frontier[frontier_len] = track_id;
    frontier_len += 1;
    visited[visited_len] = track_id;
    visited_len += 1;

    while frontier_len > 0 {
        frontier_len -= 1;
        let node = frontier[frontier_len];
        for t in &song.tracks {
            let feeds_node = t.parent_group_id == Some(node)
                || t.sends.iter().any(|s| s.dest_track_id == node);
            if feeds_node {
                if t.solo {
                    return true;
                }
                // Skip already-visited nodes so the fixed-length frontier
                // can never overflow (each distinct node is pushed once).
                if visited[..visited_len].contains(&t.id) {
                    continue;
                }
                if visited_len < visited.len() && frontier_len < frontier.len() {
                    visited[visited_len] = t.id;
                    visited_len += 1;
                    frontier[frontier_len] = t.id;
                    frontier_len += 1;
                }
            }
        }
    }
    false
}
