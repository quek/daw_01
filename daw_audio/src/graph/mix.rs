//! バス合流と tap の解決 — `execute` から切り出した「信号をどこへ足すか」の層
//! (`docs/plan_arch_refactor.md` §5 の続き)。
//!
//! `execute.rs` は 1,000 実コード行の budget (不変条件 9) に迫っていたので、
//! **足す前に分割**した。ここに集めたのは「schedule の 1 op を実行するのに要る
//! 純粋な合流・走査」だけで、schedule の走り方 (順序 / plugin dispatch) は
//! `execute.rs` に残る。
//!
//! RT 規約: 全関数が audio callback / worker / export freewheel から呼ばれる。
//! ヒープ確保・ロック・I/O を行わない。Song の配線を歩く判定は RT には置かず、compile 時に
//! program へ焼く (`ChainProgram::snapshot_*` r.md #129 §18-B / `ChainProgram::solo_contributors`)。

use common::model::{Song, Track};

use crate::graph::BufRef;
use crate::graph::program::{ChainProgram, ChainScratch, ParallelScratch};
use crate::graph::schedule::MASTER_OWNER;
use crate::mixer::TrackScratch;

/// Resolve a tap `BufRef` (PostFader / PostFx / PreFx / chain / Parallel) to its
/// `(L, R)` buffers. Returns `None` for a non-tap `BufRef` or out-of-range
/// track. docs/plan_modulation_followups.md §1. RT-safe (pure slicing).
///
/// 読み元は `scratch(track index)` / `program(owner、master は [`MASTER_OWNER`])` で引く (並列実行器は手ごとに
/// 読む資源だけを借りる — `graph::step::RenderCtx`)。
pub(super) fn resolve_tap<'a>(
    src: BufRef,
    scratch: impl FnOnce(u32) -> Option<&'a TrackScratch>,
    // r.md #110: chain / Parallel 入力の tap は program の scratch から引く。
    program: impl FnOnce(u32) -> Option<&'a ChainProgram>,
) -> Option<(&'a [f32], &'a [f32])> {
    match program_tap_owner(src) {
        None => resolve_scratch_tap(scratch(scratch_tap_track(src)?)?, src),
        Some(owner) => {
            let p = program(owner)?;
            resolve_program_tap(&p.chains, &p.parallels, src)
        }
    }
}

/// scratch 系の tap (PostFader / PostFx / PreFx) の track index。
pub(super) fn scratch_tap_track(src: BufRef) -> Option<u32> {
    match src {
        BufRef::TrackScratch(i) | BufRef::PreFaderScratch(i) | BufRef::PreFxScratch(i) => Some(i),
        _ => None,
    }
}

/// tap の読み元が program の scratch (chain / Parallel) なら、その program の持ち主
/// (song-track index か [`MASTER_OWNER`])。scratch 系 / tap でない `BufRef` は `None`。
pub(super) fn program_tap_owner(src: BufRef) -> Option<u32> {
    match src {
        BufRef::ChainPostFx { owner, .. }
        | BufRef::ChainPostFader { owner, .. }
        | BufRef::ParallelInput { owner, .. }
        | BufRef::ParallelOutput { owner, .. } => Some(owner),
        BufRef::TrackScratch(_)
        | BufRef::PreFaderScratch(_)
        | BufRef::PreFxScratch(_)
        | BufRef::Master
        | BufRef::Pooled(_) => None,
    }
}

/// track scratch 系の tap (PostFader / PostFx / PreFx) の信号 (`s` = その track の scratch)。
pub(super) fn resolve_scratch_tap(s: &TrackScratch, src: BufRef) -> Option<(&[f32], &[f32])> {
    Some(match src {
        BufRef::TrackScratch(_) => (s.track_l.as_slice(), s.track_r.as_slice()),
        BufRef::PreFaderScratch(_) => (s.pre_fader_l.as_slice(), s.pre_fader_r.as_slice()),
        BufRef::PreFxScratch(_) => (s.pre_fx_l.as_slice(), s.pre_fx_r.as_slice()),
        _ => return None,
    })
}

/// program の scratch 系の tap (chain の PostFx / PostFader、Parallel の入力 / 分割出力)。
/// `owner` は見ない (呼び側がその program の `chains` / `parallels` を渡す — 同じ program の
/// `natives` へ書くときにフィールドで借用を分けるため)。
pub(super) fn resolve_program_tap<'a>(
    chains: &'a [ChainScratch],
    parallels: &'a [ParallelScratch],
    src: BufRef,
) -> Option<(&'a [f32], &'a [f32])> {
    Some(match src {
        BufRef::ChainPostFx { slot, .. } => {
            let c = chains.get(slot as usize)?;
            (c.post_fx_l.as_slice(), c.post_fx_r.as_slice())
        }
        BufRef::ChainPostFader { slot, .. } => {
            let c = chains.get(slot as usize)?;
            (c.post_fader_l.as_slice(), c.post_fader_r.as_slice())
        }
        BufRef::ParallelInput { slot, .. } => {
            let r = parallels.get(slot as usize)?;
            (r.in_l.as_slice(), r.in_r.as_slice())
        }
        BufRef::ParallelOutput { slot, output, .. } => {
            let r = parallels.get(slot as usize)?;
            r.split.as_ref()?.output(output)?
        }
        _ => return None,
    })
}

/// Sum the listed source scratches into `target` (= scratch `target_idx`, used to
/// feed group buses with their children). Clears the target first so
/// stale samples from a previous buffer don't leak. `scratch(i)` は src の scratch を引く
/// (`target_idx` 自身は引かない)。
pub(super) fn mix_into<'a>(
    target: &mut TrackScratch,
    target_idx: u32,
    srcs: &[(BufRef, f32)],
    n: usize,
    // `true` clears `dst` first (normal group / return Mix). `false`
    // accumulates on top of whatever is already there (パラアウト
    // group-with-instrument: keep the instrument's own main output written by
    // the pass-1 prefix before summing the children).
    clear: bool,
    scratch: impl Fn(u32) -> Option<&'a TrackScratch>,
) {
    let n = n.min(target.track_l.len()).min(target.track_r.len());
    if clear {
        target.track_l[..n].fill(0.0);
        target.track_r[..n].fill(0.0);
    }
    for (src, gain) in srcs {
        let BufRef::TrackScratch(s_idx) = *src else {
            continue;
        };
        if s_idx == target_idx {
            continue;
        }
        let Some(s_scratch) = scratch(s_idx) else {
            continue;
        };
        if s_scratch.effective_mute {
            continue;
        }
        let g = *gain;
        for i in 0..n {
            target.track_l[i] += s_scratch.track_l[i] * g;
            target.track_r[i] += s_scratch.track_r[i] * g;
        }
    }
}

/// Sum each non-muted source scratch (with its routing gain) into the
/// master bus. The master buffers are zeroed earlier in the render so
/// this is `+=` style accumulation.
pub(super) fn mix_into_master<'a>(
    srcs: &[(BufRef, f32)],
    master_l: &mut [f32],
    master_r: &mut [f32],
    n: usize,
    scratch: impl Fn(u32) -> Option<&'a TrackScratch>,
) {
    let n = n.min(master_l.len()).min(master_r.len());
    for (src, gain) in srcs {
        let BufRef::TrackScratch(s_idx) = src else {
            continue;
        };
        let Some(s_scratch) = scratch(*s_idx) else {
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
/// Reads `src_scratch`'s post-fader (`track_l/r`) or pre-fader
/// (`pre_fader_l/r`) buffer, scales it by the **live** send gain of
/// the send with stable id `send_id` on `song.tracks[src_track_idx]` —
/// sampled per-sample from a `SendGain` automation lane when present (and
/// not being recorded), otherwise the constant `send.gain` — and adds it
/// into `dst_scratch.track_l/r` (`+=`, no clear; `dst_idx` = その track index). A disabled send or a
/// muted source contributes nothing (Ableton: mute silences sends). The
/// gain is read live, never baked into the schedule, so knob drags and
/// `SendGain` automation apply without recompiling.
#[allow(clippy::too_many_arguments)]
pub(super) fn mix_send_into(
    dst_scratch: &mut TrackScratch,
    dst_idx: u32,
    src_scratch: &TrackScratch,
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
    // r.md #87: 送り元トラックの行の供給元。SendGain レーン行をランチャーで
    // 撃っていたら、アレンジのカーブではなく **セルのカーブ**を使う
    // (Volume / Pan / PluginParam は既にそうなっていて、ここだけ抜けていた)。
    rows: crate::launcher::TrackRows<'_>,
    // 送り元トラックへ流れ込む track の表 (`ChainProgram::solo_contributors`)。
    src_contributors: &[u32],
) {
    use common::model::{AutomationTarget, TrackBuiltinParam};

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
        let dest_soloed = song.tracks.get(dst_idx as usize).is_some_and(|d| d.solo);
        if !dest_soloed && !track.solo && !any_soloed(song, src_contributors) {
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
        // この行の供給元 (アレンジ / セル / 停止) を 1 度だけ解く。
        let lane_row = song
            .tracks
            .get(src_track_idx as usize)
            .and_then(|t| t.automation_lanes.iter().position(|l| l.id == lane.id))
            .map_or(crate::launcher::RowTimeSource::default(), |i| rows.lane(i));
        for i in 0..n {
            // `fill_track_param_ramps` / `fill_pd_param_events` と同じ積分済み
            // anchor + per-frame 増分 (M5 の beat-domain 統一)。
            let beat = playhead_beats + i as f64 * beats_per_frame;
            #[allow(clippy::cast_possible_truncation)]
            let phase = crate::launcher::render::phase_at_frame(lane_row, i as u32);
            let g = crate::launcher::render::lane_value(
                lane,
                &song.clip_contents,
                phase,
                beat,
            ) as f32;
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

/// `tracks` (song-track index) のいずれかが `solo == true` なら true。表は compile 時に焼いた配線の閉包で、
/// solo の透過規則の 2 つがこれを引く:
///
/// - [`ChainProgram::solo_contributors`] — その track に流れ込む track (子 → group、send 元 → return)。
///   「あるトラックを solo すると、そのトラックが送っている reverb / delay の **リターン** も生かす」
///   Ableton 準拠の挙動 (リターンを solo-safe にしないと、ソロ中はセンドエフェクトが聞こえない)。
/// - [`ChainProgram::solo_ancestors`] — 祖先 group (folder solo: group を solo したら子も鳴る)。
///
/// 配線は topology なので再 compile と同じ便で変わり、solo は値のみ更新なので song から毎 buffer 読む。
/// RT-safe: 表の走査のみ (確保なし、トラック数に上限なし、RT で Song の配線を歩かない)。
pub(super) fn any_soloed(song: &Song, tracks: &[u32]) -> bool {
    tracks.iter().any(|&i| song.tracks.get(i as usize).is_some_and(|t| t.solo))
}
