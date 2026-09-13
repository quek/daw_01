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

use super::program_build::{ChainLatency, build_program};
use super::schedule::{BufRef, MASTER_OWNER, NodeOp, Schedule};
use deps::Topology;
use pdc::master_output_latency;
use sidechain::{TapCtx, collect_chain_taps, compute_input_delays};

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
pub fn compile_schedule(
    song: &Song,
    device_latencies: &DeviceLatencies,
    sample_rate: u32,
    buffer_frames: u32,
) -> Result<Schedule, GraphError> {
    let n = song.tracks.len();
    // r.md #110: device ツリーを program に展開する (`docs/plan_parallel.md` §4.1)。
    // 並列 chain の PDC と chain tap の snapshot flag はここで焼き込む。
    let (master_built, built, chain_map) = build_all_programs(song, device_latencies);
    if n == 0 {
        return Ok(Schedule {
            nodes: vec![NodeOp::Mix {
                srcs: Vec::new(),
                dst: BufRef::Master,
            }],
            master_latency_samples: master_output_latency(song, device_latencies, 0, sample_rate),
            master_program: master_built.program,
            ..Schedule::empty()
        });
    }

    // ---- 配線トポロジ: 親参照の検査 → group / send / パラアウトの入力表 → bus 判定 ----
    let topo = Topology::build(song)?;
    let taps = TapCtx {
        id_to_idx: &topo.id_to_idx,
        chains: &chain_map,
    };
    // ---- path latency の依存辺の post-order = 実行順 (循環はここで弾く) ----
    let order = deps::execution_order(song, &topo, &taps)?;
    // ---- 実行順に op を積む (producer は必ず consumer より前に並ぶ) ----
    let nodes = emit::emit_track_ops(song, &topo, &order, &built, &chain_map);

    // ---- PR3: Plugin Delay Compensation ----
    let track_chain_latency: Vec<u32> = built.iter().map(|b| b.latency).collect();
    let path_latency = pdc::path_latencies(song, &topo, &taps, &track_chain_latency, buffer_frames);
    let mut compensated = pdc::insert_delay_compensation(song, nodes, &path_latency);

    // PR4.5 sidechain plugin-internal alignment: per-track input delay
    // (leaf 宛は path latency の sidechain fan-in と同じ staging lag を足す)。
    let input_delay_per_track = compute_input_delays(
        song,
        &topo.bus_flags,
        buffer_frames,
        &topo.id_to_idx,
        &chain_map,
        &path_latency,
        &track_chain_latency,
    );

    // docs/plan_modulation.md §3/§5: per-`ModSource` envelope follower (末尾に emit)。
    let (follower_slots, follower_keys, mod_kinds) =
        emit::emit_followers(song, sample_rate, &topo.id_to_idx, &chain_map, &mut compensated.nodes);

    Ok(Schedule {
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
        master_latency_samples: master_output_latency(
            song,
            device_latencies,
            compensated.master_mix_latency,
            sample_rate,
        ),
    })
}

/// r.md #110: 全 track + master の device ツリーを program に展開し、chain id → 置き場
/// (`ChainMap`) を組む。`compile_schedule` の冒頭から切り出した (関数 budget)。
fn build_all_programs(
    song: &Song,
    device_latencies: &DeviceLatencies,
) -> (super::program_build::BuiltProgram, Vec<super::program_build::BuiltProgram>, ChainMap) {
    let chain_taps = collect_chain_taps(song);
    let master_built = build_program(
        &song.master_fx_chain,
        common::model::MASTER_TRACK_ID,
        None,
        device_latencies,
        &chain_taps,
    );
    let built: Vec<_> = song
        .tracks
        .iter()
        .map(|t| build_program(&t.devices, t.id, t.paraout_split_device(), device_latencies, &chain_taps))
        .collect();
    let mut chain_map: ChainMap = HashMap::new();
    let mut register = |b: &super::program_build::BuiltProgram, owner: u32| {
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
    compile_schedule(song, &DeviceLatencies::new(), sample_rate, buffer_frames)
}
