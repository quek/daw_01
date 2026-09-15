//! 実行順に並んだ track から node op 列を組む。
//!
//! 各 track で: sidechain tap (その track の device が process する前) → bus なら入力の
//! 合流 (子の `Mix` / `MixAdditive`、パラアウトの `ParallelOutTap`、send の `MixSend`) +
//! `ProcessGroupFx`、leaf なら `ProcessTrack`。最後に master `Mix` と master fx の
//! sidechain tap。envelope follower は PDC の後で末尾に積む ([`emit_followers`])。

use std::collections::HashMap;

use common::model::{SendMode, Song, Track};

use super::deps::Topology;
use super::sidechain::{TapCtx, emit_sidechain_taps};
use super::{ChainMap, tap_bufref_for};
use crate::graph::program_build::BuiltProgram;
use crate::graph::schedule::{BufRef, MASTER_OWNER, NodeOp};

/// Emit ops in dependency post-order. `order` already lists every node after all
/// of its dependencies, so children precede their group, sidechain sources precede
/// their sink, and send sources precede their return.
///
/// `built` / `master_built` は可変: 内蔵 device の SC を staging にするか (`ScMode::Staged`) は
/// tap を解決できた emit の時点でしか決まらないので、ここで program の scratch に書く。
pub(super) fn emit_track_ops(
    song: &Song,
    topo: &Topology,
    order: &[u32],
    built: &mut [BuiltProgram],
    master_built: &mut BuiltProgram,
    taps: &TapCtx<'_>,
) -> Vec<NodeOp> {
    let mut nodes = Vec::with_capacity(song.tracks.len() * 2 + 1);
    let mut master_srcs: Vec<(BufRef, f32)> = Vec::new();

    for &i in order {
        // r.md #131: 無効トラックは手を出さない (sidechain tap / 合流 / ProcessTrack / master への合流)。
        if !topo.enabled[i as usize] {
            continue;
        }
        let track = &song.tracks[i as usize];
        let track_idx = i;
        // PR4 sidechain: the tap must run **before** this track's devices process
        // (i.e. before ProcessTrack / ProcessGroupFx) so the engine can stage the
        // source signal (plugin: `pd.buffer_aux_in[port]`、native: `NativeScratch::sc`)。
        let stores = (track.automation_lanes.as_slice(), track.mod_routings.as_slice());
        emit_sidechain_taps(&track.devices, track.id, track_idx, stores, &mut built[i as usize], taps, &mut nodes);

        // A bus sums its inputs into its own scratch and runs its fx chain +
        // strip via ProcessGroupFx, rather than rendering its own clips /
        // instrument as a leaf (Ableton return-track semantics). A track that
        // is not a bus is a plain leaf. (classification: `Topology::bus_flags`)
        if topo.bus_flags[i as usize] {
            emit_bus_ops(track, track_idx, topo, built[i as usize].program.pass1_end, &mut nodes);
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

    // master bus fx chain の sidechain。 master Mix の **後** に tap を積む (= source track の
    // scratch は dispatch_and_wait で確定済み)。 `execute_schedule_post_dispatch` がこの tap を
    // 処理して staging し、 直後の `process_master_fx_chain` がそれを読む。 track 経路と同じ
    // `emit_sidechain_taps` を使う (critique #1: emit site の単一化)。
    emit_sidechain_taps(
        &song.master_fx_chain,
        common::model::MASTER_TRACK_ID,
        MASTER_OWNER,
        (song.song_lanes.as_slice(), song.song_mod_routings.as_slice()),
        master_built,
        taps,
        &mut nodes,
    );
    nodes
}

/// bus track (group / return / パラアウト先) の入力を自分の scratch へ合流し、
/// `ProcessGroupFx` を積む。`pass1_end` はこの track の展開済み program の pass 1 終端。
fn emit_bus_ops(
    track: &Track,
    track_idx: u32,
    topo: &Topology,
    pass1_end: usize,
    nodes: &mut Vec<NodeOp>,
) {
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
    let group_with_instrument = topo.gwi_split[track_idx as usize].is_some();
    // r.md #110: split は展開後の op 列上の位置 (`ChainProgram::pass1_end`)。
    let start_op = if group_with_instrument {
        pass1_end as u32
    } else {
        0
    };

    let kids = topo.children_of.get(&track.id).cloned().unwrap_or_default();
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
    if let Some(edges) = topo.incoming_paraout.get(&track.id) {
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
    if let Some(edges) = topo.incoming_sends.get(&track.id) {
        for &(src_idx, send_id, mode) in edges {
            let src = match mode {
                SendMode::PostFader => BufRef::TrackScratch(src_idx),
                SendMode::PreFader => BufRef::PreFaderScratch(src_idx),
            };
            nodes.push(NodeOp::MixSend {
                src,
                dst: BufRef::TrackScratch(track_idx),
                src_track_idx: src_idx,
                send_id,
            });
        }
    }
    nodes.push(NodeOp::ProcessGroupFx { track_idx, start_op });
}

/// docs/plan_modulation.md §3/§5: per-`ModSource` envelope follower.
/// Emit `EnvelopeFollow` at the very end of the (post-PDC) schedule — all
/// scratches are settled and the follower only produces a control-rate
/// scalar (no audio feedback, so no ordering / cycle constraint).
/// Coefficients are baked here (recompile-time) so the RT path never
/// derives them (§10)。
///
/// `slot` は **schedule の内部 index** で、`follower_slots` / `follower_keys` /
/// `mod_kinds` の 3 本が同じ並び。外へ出る値 (GUI へ publish する面 / sidecar) は
/// `follower_keys[slot]` = `ModSource::id` を組にして運ぶ
/// (`crate::mod_tick::eval_plane`、アーキ不変条件 1)。envelope follower の slot は
/// EnvelopeFollow node が `env` を駆動するが、generator (LFO/Random/MSEG/Steps) の
/// slot は inert で、engine が `common::modulators::generator_scalar` を刻みの
/// song 位置から評価して載せる (`mod_kinds` を保持)。
pub(super) fn emit_followers(
    song: &Song,
    sample_rate: u32,
    id_to_idx: &HashMap<u32, u32>,
    chain_map: &ChainMap,
    nodes: &mut Vec<NodeOp>,
) -> (Vec<crate::graph::follower::FollowerSlot>, Vec<u32>, Vec<common::model::ModSourceKind>) {
    let mut follower_slots: Vec<crate::graph::follower::FollowerSlot> = Vec::new();
    let mut follower_keys: Vec<u32> = Vec::new();
    let mut mod_kinds: Vec<common::model::ModSourceKind> = Vec::new();
    for (slot, ms) in song.mod_sources.iter().enumerate() {
        mod_kinds.push(ms.kind.clone());
        match &ms.kind {
            common::model::ModSourceKind::EnvelopeFollower { tap, follower } => {
                follower_slots.push(crate::graph::follower::FollowerSlot::from_config(follower, sample_rate));
                // docs/plan_modulation.md §6: tap_point で source buffer を解決。入力なし (`None`) /
                // 読み元が居ない (dangling / r.md #131 無効トラック) / 評価されない source (バイパス中・無効
                // トラックに帰属) は follower node を emit しない。
                let src = tap
                    .as_ref()
                    .filter(|_| song.mod_source_active(ms))
                    .and_then(|tap| tap_bufref_for(tap, id_to_idx, chain_map));
                match src {
                    Some(src) => {
                        nodes.push(NodeOp::EnvelopeFollow { src, slot: slot as u32 });
                        // §5 D: 状態移送キー = ModSource の安定 id (0 = 未採番、移送対象外)。
                        follower_keys.push(ms.id);
                    }
                    // 進まない slot は **鍵を持たない** (0 = 状態を移送せず、plan の slot とも組まない)。
                    // 鍵を残すと前の schedule の envelope を引き継いだまま誰も進めず、`env_at_tick` が
                    // 最後の値を返し続ける = 読み元を失った変調が固まる (「変調なし」にならない)。
                    None => follower_keys.push(0),
                }
            }
            // generator: inert slot (env 未使用、 generator_scalar が値を供給)。
            _ => {
                follower_keys.push(ms.id);
                follower_slots.push(crate::graph::follower::FollowerSlot::from_config(
                    &common::model::FollowerConfig::default(),
                    sample_rate,
                ));
            }
        }
    }
    (follower_slots, follower_keys, mod_kinds)
}
