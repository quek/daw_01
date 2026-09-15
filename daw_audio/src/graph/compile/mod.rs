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
//!
//! 段の分担 ([`compile_schedule`] はこの順に呼んで `Schedule` を組み立てるだけ):
//! - [`deps`] — 配線トポロジ (track id → index / group の子 / send・パラアウトの入力 /
//!   bus 判定) と、path latency の依存辺に沿った実行順 (循環は `GraphError::Cycle`)。
//! - [`emit`] — 実行順に node op を積む (bus の合流 + `ProcessGroupFx` / leaf の
//!   `ProcessTrack` / master `Mix`) と envelope follower の emit。
//! - [`pdc`] — path latency、合流点の `ApplyDelay` 挿入、master 出力の遅延量。
//! - [`sidechain`] — sidechain consumer を走査する会計のすべて (chain snapshot の要求 /
//!   依存辺 / tap の emit / path latency への fan-in / plugin 入力の input delay)。
//!
//! mod.rs には組み立て本体と、各段が共有する表を置く: device ツリーの program 展開と
//! chain id → 置き場 ([`ChainMap`])、tap → source buffer の解決 ([`tap_bufref_for`])。

#![allow(dead_code)]

mod deps;
mod emit;
mod pdc;
mod sidechain;
#[cfg(test)]
mod tests;

use std::collections::HashMap;

use common::model::{AudioTap, Song, TapPoint, TapSource};
use common::protocol::RenderScope;

use super::program::Pass1Role;
use super::program_build::{BuiltProgram, ChainLatency, build_program};
use super::render_graph::RenderGraph;
use super::schedule::{BufRef, MASTER_OWNER, NodeOp, Schedule};
use deps::Topology;
use pdc::master_output_latency;
use sidechain::{TapCtx, bake_snapshot_needs, collect_chain_taps, compute_sc_delays};

/// r.md #110: chain id → その chain が居る program と slot (sidechain / follower の
/// `TapSource::Chain` を `BufRef` へ解決する表)。`owner` = song-track index、master 所有
/// は [`MASTER_OWNER`]。
#[derive(Debug, Clone, Copy)]
struct ChainLoc {
    owner: u32,
    chain_slot: u32,
    parallel_slot: u32,
    /// r.md #112: `Split` で受ける出力番号 (`PreFx` tap はその出力)。
    output: Option<u8>,
    lat: ChainLatency,
}

type ChainMap = HashMap<u64, ChainLoc>;

/// tap → source buffer。dangling (track / chain が無い) は `None`。
fn tap_bufref_for(tap: &AudioTap, id_to_idx: &HashMap<u32, u32>, chains: &ChainMap) -> Option<BufRef> {
    match tap.source {
        TapSource::Track(t) => id_to_idx.get(&t).map(|&i| tap_bufref(tap.tap_point, i)),
        TapSource::Chain(c) => chains.get(&c).map(|loc| match tap.tap_point {
            TapPoint::PreFx => match loc.output {
                Some(output) => BufRef::ParallelOutput {
                    owner: loc.owner,
                    slot: loc.parallel_slot,
                    output,
                },
                None => BufRef::ParallelInput {
                    owner: loc.owner,
                    slot: loc.parallel_slot,
                },
            },
            TapPoint::PostFx => BufRef::ChainPostFx {
                owner: loc.owner,
                slot: loc.chain_slot,
            },
            TapPoint::PostFader => BufRef::ChainPostFader {
                owner: loc.owner,
                slot: loc.chain_slot,
            },
        }),
    }
}

/// docs/plan_modulation.md §6 / docs/plan_modulation_followups.md §1: resolve a
/// tap point to the source scratch buffer. `PostFader` = the track's final
/// output (`TrackScratch`); `PostFx` = after the **whole device chain in its
/// order** (r.md #129: 組み込みの Comp / EQ も device) but before the volume/pan
/// fader (`PreFaderScratch`); `PreFx` = the raw signal before the device chain
/// (`PreFxScratch`). The snapshots are captured only when a tap actually needs
/// them (`ChainProgram::snapshot_*`, baked at compile time).
fn tap_bufref(tap_point: common::model::TapPoint, src_idx: u32) -> BufRef {
    use common::model::TapPoint;
    match tap_point {
        TapPoint::PostFader => BufRef::TrackScratch(src_idx),
        TapPoint::PostFx => BufRef::PreFaderScratch(src_idx),
        TapPoint::PreFx => BufRef::PreFxScratch(src_idx),
    }
}

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
///
/// `scope` = どの処理段を通すか (live / 書き出しは `RenderScope::Mix`)。通さない段は program の形で表す
/// (device の op を出さない / Parallel の混ぜ方 / `ChainProgram::fader` / `Schedule::master_stage`) ので、
/// 描く関数 (`render_master_buffer`) は scope に依らず 1 本。
pub fn compile_schedule(
    song: &Song,
    device_latencies: &DeviceLatencies,
    sample_rate: u32,
    buffer_frames: u32,
    scope: RenderScope,
) -> Result<Schedule, GraphError> {
    let n = song.tracks.len();
    // r.md #129 §8.3.4: Limiter の先読み遅延は compile 時に焼く (PDC の会計と DSP が同じ値を見る)。
    let master_limiter_latency = scope.master() && song.master_limiter_latency_active();
    // r.md #131: 実効的に無効なトラックはグラフに居ない (program は空、手も op も出さない)。
    let enabled = song.effectively_enabled_mask();
    // r.md #110: device ツリーを program に展開する (`docs/plan_parallel.md` §4.1)。
    // 並列 chain の PDC と chain tap の snapshot flag はここで焼き込む。
    let (mut master_built, mut built, chain_map) = build_all_programs(song, &enabled, device_latencies, scope);
    let master_latency = |mix_latency: u32| {
        let chain = master_chain(song, scope);
        master_output_latency(chain, device_latencies, mix_latency, sample_rate, master_limiter_latency, scope)
    };
    if n == 0 {
        let nodes = vec![NodeOp::Mix { srcs: Vec::new(), dst: BufRef::Master }];
        return Ok(Schedule {
            graph: RenderGraph::build(song, &nodes, &enabled),
            nodes,
            master_latency_samples: master_latency(0),
            master_limiter_latency,
            master_stage: scope.master(),
            master_program: master_built.program,
            ..Schedule::empty()
        });
    }

    // ---- 配線トポロジ: 親参照の検査 → group / send / パラアウトの入力表 → bus 判定 ----
    let topo = Topology::build(song, enabled)?;
    // pass 1 の役割を program に焼く (RT の `process_track_owned` が Song を歩かない)。
    let roles = topo.bus_flags.iter().zip(&topo.gwi_split).zip(&topo.enabled);
    for ((b, track), ((&bus, gwi), &on)) in built.iter_mut().zip(&song.tracks).zip(roles) {
        b.program.pass1_role = match (on, gwi, bus) {
            (false, _, _) => Pass1Role::Disabled,
            (true, Some(_), _) => Pass1Role::GroupWithInstrument { main_to_child: track.paraout_main_to_child() },
            (true, None, true) => Pass1Role::Bus,
            (true, None, false) => Pass1Role::Leaf,
        };
    }
    // solo の透過規則の表 (「子 / send 元が solo なら bus も透過」と folder solo。RT で配線を歩かない)。
    let solo = topo.solo_tables(song);
    let taps = TapCtx {
        id_to_idx: &topo.id_to_idx,
        enabled: &topo.enabled,
        chains: &chain_map,
        bus_flags: &topo.bus_flags,
        gwi_split: &topo.gwi_split,
        buffer_frames,
    };
    // ---- path latency の依存辺の post-order = 実行順 (循環はここで弾く) ----
    let order = deps::execution_order(song)?;
    // ---- 実行順に op を積む (producer は必ず consumer より前に並ぶ) ----
    let nodes = emit::emit_track_ops(song, &topo, &order, &mut built, &mut master_built, &taps);

    // ---- PR3: Plugin Delay Compensation ----
    let track_chain_latency: Vec<u32> = built.iter().map(|b| b.latency).collect();
    let path_latency = pdc::path_latencies(song, &topo, &taps, &track_chain_latency);
    // PR4.5 / r.md #129 §8.3.3: consumer の入力で main をサイドチェインに揃える遅延。pass 1 の
    // consumer 宛ては track の input delay、pass 2 の consumer 宛ては `BusScAlign`。
    let non_sc_input = pdc::non_sc_input_latencies(song, &topo, &path_latency);
    let (input_delay_per_track, bus_sc_delay) =
        compute_sc_delays(song, &taps, &path_latency, &track_chain_latency, &non_sc_input);
    let mut compensated = pdc::insert_delay_compensation(song, nodes, &path_latency, &bus_sc_delay);

    // docs/plan_modulation.md §3/§5: per-`ModSource` envelope follower (末尾に emit)。
    let (follower_slots, follower_keys, mod_kinds) =
        emit::emit_followers(song, sample_rate, &topo.id_to_idx, &chain_map, &mut compensated.nodes);

    let mut schedule = Schedule {
        graph: RenderGraph::build(song, &compensated.nodes, &topo.enabled),
        solo,
        nodes: compensated.nodes,
        delay_lines: compensated.delay_lines,
        delay_keys: compensated.delay_keys,
        port_buffers: super::PortBufferPool::new(),
        input_delay_per_track,
        follower_slots,
        follower_keys,
        mod_kinds,
        track_programs: built.into_iter().map(|b| b.program).collect(),
        master_program: master_built.program,
        master_midi_a: Vec::with_capacity(crate::mixer::MAX_EVENTS),
        master_midi_b: Vec::with_capacity(crate::mixer::MAX_EVENTS),
        // r.md #39: click / export が基準にするのは master **出力** の遅延量。
        // master fx chain は Mix の後段で直列 process される (`process_master_fx_chain`)
        // ので、その報告 latency も足さないと master に遅延プラグインを挿したときだけ
        // click が先行し、書き出し WAV もその分ずれる。
        master_latency_samples: master_latency(compensated.master_mix_latency),
        master_limiter_latency,
        master_stage: scope.master(),
        state_keys: Default::default(),
    };
    schedule.index_state_keys();
    Ok(schedule)
}

/// compile する master の fx chain: scope が master を通さないなら空 (op も latency も出さない)。
fn master_chain(song: &Song, scope: RenderScope) -> &[common::model::Device] {
    if scope.master() { &song.master_fx_chain } else { &[] }
}

/// r.md #110: 全 track + master の device ツリーを program に展開し、chain id → 置き場
/// (`ChainMap`) を組む。`compile_schedule` の冒頭から切り出した (関数 budget)。
///
/// r.md #129 §8.3.2: 展開した program に track の snapshot 要求を焼く。ここに置くので `n == 0` の
/// 早期 return にも効く (GR メーターは device ごとの `NativeSlot::meter` で、面の容量は曲から数える —
/// `docs/plan_unbounded_tracks.md` §3)。
///
/// r.md #131: 実効的に無効なトラック (`enabled[i] == false`) は device を持たない空の program にする —
/// plugin の依頼も内蔵 DSP も latency も出ず、chain id も登録しない (= その chain を読む tap は dangling と同じ)。
fn build_all_programs(
    song: &Song,
    enabled: &[bool],
    device_latencies: &DeviceLatencies,
    scope: RenderScope,
) -> (BuiltProgram, Vec<BuiltProgram>, ChainMap) {
    let chain_taps = collect_chain_taps(song, enabled);
    let master_built = build_program(
        master_chain(song, scope),
        common::model::MASTER_TRACK_ID,
        None,
        device_latencies,
        &chain_taps,
        scope,
    );
    let mut built: Vec<_> = song
        .tracks
        .iter()
        .zip(enabled)
        .map(|(t, &on)| {
            let (devices, split) = if on { (t.devices.as_slice(), t.paraout_split_device()) } else { (&[][..], None) };
            build_program(devices, t.id, split, device_latencies, &chain_taps, scope)
        })
        .collect();
    let mut chain_map: ChainMap = HashMap::new();
    let mut register = |b: &BuiltProgram, owner: u32| {
        for (&cid, &super::program_build::ChainSlot { chain_slot, parallel_slot, output }) in &b.chain_slots {
            chain_map.insert(
                cid,
                ChainLoc {
                    owner,
                    chain_slot,
                    parallel_slot,
                    output,
                    lat: b.chain_latency.get(&cid).copied().unwrap_or_default(),
                },
            );
        }
    };
    for (idx, b) in built.iter().enumerate() {
        register(b, idx as u32);
    }
    register(&master_built, MASTER_OWNER);
    bake_snapshot_needs(song, enabled, &mut built);
    (master_built, built, chain_map)
}

/// テスト用: 「どの device も latency を報告していない」 前提で compile する短縮形。
/// PDC を検証するテストだけが `compile_schedule` に表を明示的に渡す。
#[cfg(test)]
pub(crate) fn compile_schedule_for_test(
    song: &Song,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<Schedule, GraphError> {
    compile_schedule(song, &DeviceLatencies::new(), sample_rate, buffer_frames, RenderScope::Mix)
}
