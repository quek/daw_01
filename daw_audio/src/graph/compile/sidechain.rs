//! Sidechain の会計 — サイドチェインを読む consumer を走査する処理はすべてここに置く
//! (`docs/plan_rack_native_devices.md` §8.3.3)。
//!
//! consumer = plugin の `aux_inputs` と、内蔵 device (Comp / Bus Comp) の SC。列挙は common の
//! [`aux_consumers`] 1 本で、plugin と native を同じ形 (`AuxConsumer`) で信号順に出す。
//! `inactive` なもの (plugin は bypassed、native は `!can_activate`) は dispatch / 処理されないので、
//! その配線は snapshot 要求 / 依存辺 / tap / latency のどれにも数えない (r.md #105)。
//! 走査は次の 5 つ:
//! - [`collect_chain_taps`] — chain の snapshot flag を焼く入力 (`build_all_programs`)
//! - 依存辺 — `common::routing_deps::TrackDeps` (`deps::execution_order`)。辺の定義を 2 本持たない
//! - [`emit_sidechain_taps`] — `NodeOp::SidechainTap` / `NodeOp::NativeSidechainTap` の emit
//! - [`sidechain_input_latency`] — path latency への fan-in (`pdc::compute_path_latency`)
//! - [`compute_sc_delays`] — pass 1 の input delay と、pass 2 の `BusScAlign` の遅延量
//!
//! **lag は consumer が走る pass で決める** ([`TapCtx::lag`])。post-dispatch で staging した音を
//! pass 1 (leaf / group-with-instrument の prefix) は次の buffer で消費する = `buffer_frames` 遅れ、
//! pass 2 (bus の `ProcessGroupFx` / master fx) は同じ buffer で消費する = 0。track 単位で決めて
//! いた頃は、GWI の prefix 宛ての lag を 0 と数え、bus 宛ての SC で main 側に遅延を掛けていなかった
//! (§18-L)。

use std::collections::{HashMap, HashSet};

use common::model::{AudioTap, AutomationLane, Device, ModRouting, Song, TapPoint, TapSource, Track};
use common::routing_deps::{AuxConsumer, aux_consumers};

use super::{ChainMap, tap_bufref_for};
use crate::graph::native::{ScMode, ScStage};
use crate::graph::program_build::BuiltProgram;
use crate::graph::schedule::{MASTER_OWNER, NodeOp};

/// consumer が走る pass。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ScPass {
    /// worker が dispatch する pass (leaf の全 device / group-with-instrument の prefix)。
    Pass1,
    /// post-dispatch の `ProcessGroupFx` (group / return / パラアウト先 / GWI の suffix)。
    Pass2,
}

/// consumer が走る pass。`split` は group-with-instrument のときだけ `Some` (top-level の分割点)。
pub(super) fn consumer_pass(bus: bool, split: Option<u32>, top_index: u32) -> ScPass {
    // leaf は全部 pass 1。group-with-instrument は prefix (top_index < split) が pass 1。
    // それ以外の bus (group / return / パラアウト先 / GWI の suffix) は pass 2。
    if !bus || split.is_some_and(|s| top_index < s) { ScPass::Pass1 } else { ScPass::Pass2 }
}

/// tap の source を解決し、consumer の pass を決めるのに使う表の束。
pub(super) struct TapCtx<'a> {
    /// **有効な** track id → song-track index (`Topology::id_to_idx`。無効トラックの tap は解決しない)。
    pub(super) id_to_idx: &'a HashMap<u32, u32>,
    /// song-track index → 実効的に有効か (`Topology::enabled`。無効トラックの consumer は数えない)。
    pub(super) enabled: &'a [bool],
    pub(super) chains: &'a ChainMap,
    /// song-track index → bus か (`Topology::bus_flags`)。
    pub(super) bus_flags: &'a [bool],
    /// song-track index → group-with-instrument の top-level 分割点 (`Topology::gwi_split`)。
    pub(super) gwi_split: &'a [Option<u32>],
    /// engine が 1 buffer で処理するフレーム数 (pass 1 の staging lag)。
    pub(super) buffer_frames: u32,
}

impl TapCtx<'_> {
    /// track `idx` の最上位 `top_index` 番目の device (の中) にある consumer が走る pass。
    pub(super) fn pass(&self, idx: u32, top_index: u32) -> ScPass {
        let i = idx as usize;
        consumer_pass(
            self.bus_flags.get(i).copied().unwrap_or(false),
            self.gwi_split.get(i).copied().flatten(),
            top_index,
        )
    }

    /// その consumer の staging lag (pass 1 → `buffer_frames`、pass 2 → 0)。
    pub(super) fn lag(&self, idx: u32, top_index: u32) -> u32 {
        match self.pass(idx, top_index) {
            ScPass::Pass1 => self.buffer_frames,
            ScPass::Pass2 => 0,
        }
    }
}

/// tap の source が属する **song-track index** (path latency 用)。
/// master 所有の chain と dangling は `None`。
fn tap_owner_idx(tap: &AudioTap, id_to_idx: &HashMap<u32, u32>, chains: &ChainMap) -> Option<u32> {
    match tap.source {
        TapSource::Track(t) => id_to_idx.get(&t).copied(),
        TapSource::Chain(c) => chains
            .get(&c)
            .map(|loc| loc.owner)
            .filter(|&o| o != MASTER_OWNER),
    }
}

/// chain source の「所有 track の chain 起点からの相対 latency」 (track source は 0 =
/// 所有 track の path latency がそのまま)。
fn tap_chain_rel_latency(tap: &AudioTap, chains: &ChainMap) -> u32 {
    match tap.source {
        TapSource::Track(_) => 0,
        TapSource::Chain(c) => chains.get(&c).map_or(0, |loc| match tap.tap_point {
            TapPoint::PreFx => loc.lat.parallel_input,
            TapPoint::PostFx => loc.lat.post_fx,
            TapPoint::PostFader => loc.lat.post_fader,
        }),
    }
}

/// tap の source 側の絶対 latency (path latency 系): track source は所有 track の
/// path latency、chain source は所有 track の入力 latency + chain までの相対 latency。
fn tap_source_latency(
    tap: &AudioTap,
    src_idx: u32,
    path_latency: &[u32],
    track_chain_latency: &[u32],
    chains: &ChainMap,
) -> u32 {
    let src_path = path_latency[src_idx as usize];
    match tap.source {
        TapSource::Track(_) => src_path,
        TapSource::Chain(_) => src_path
            .saturating_sub(track_chain_latency[src_idx as usize])
            .saturating_add(tap_chain_rel_latency(tap, chains)),
    }
}

/// r.md #131: 実効的に有効なトラック (`enabled`) と master の chain 上の consumer (無効トラックの consumer は
/// 処理されないので、読む tap も snapshot の要求も持たない)。
fn live_aux_consumers<'a>(song: &'a Song, enabled: &'a [bool]) -> impl Iterator<Item = AuxConsumer<'a>> {
    song.tracks
        .iter()
        .zip(enabled)
        .filter(|&(_, &on)| on)
        .flat_map(|(t, _)| aux_consumers(&t.devices, &t.automation_lanes, &t.mod_routings))
        .chain(aux_consumers(&song.master_fx_chain, &song.song_lanes, &song.song_mod_routings))
}

/// envelope follower のうち評価されるもの (`Song::mod_source_active`) の tap。
fn live_follower_taps(song: &Song) -> impl Iterator<Item = &AudioTap> {
    song.mod_sources
        .iter()
        .filter(|ms| song.mod_source_active(ms))
        .filter_map(|ms| ms.follower().and_then(|(tap, _)| tap))
}

/// `(chain_id, tap_point)` を誰かが読むか (chain の snapshot flag を焼く入力)。
pub(super) fn collect_chain_taps(song: &Song, enabled: &[bool]) -> HashSet<(u64, TapPoint)> {
    let mut set = HashSet::new();
    let mut add = |tap: &AudioTap| {
        if let TapSource::Chain(c) = tap.source {
            set.insert((c, tap.tap_point));
        }
    };
    for c in live_aux_consumers(song, enabled).filter(|c| !c.inactive) {
        for route in c.routes.iter().flatten() {
            add(&route.tap);
        }
    }
    live_follower_taps(song).for_each(add);
    set
}

/// track の Pre-FX / PostFx (pre-fader) snapshot を誰かが読むかを、各 program に焼く
/// (§8.3.2、旧 RT の `track_needs_*_snapshot` → `any_tap_at` は毎 buffer Song を歩いて確保していた
/// = §18-B)。snapshot は bypass と無関係に取るので、処理しえない consumer の配線も数える
/// (無効トラックの consumer は除く — 有効に戻すと compile し直す)。
/// PostFx には pre-fader send を含める (leaf と group で条件を揃える、§18-C)。
pub(super) fn bake_snapshot_needs(song: &Song, enabled: &[bool], built: &mut [BuiltProgram]) {
    let mut wanted: HashSet<(u32, TapPoint)> = HashSet::new();
    {
        let mut add = |tap: &AudioTap| {
            if let Some(t) = tap.source_track() {
                wanted.insert((t, tap.tap_point));
            }
        };
        for c in live_aux_consumers(song, enabled) {
            for route in c.routes.iter().flatten() {
                add(&route.tap);
            }
        }
        live_follower_taps(song).for_each(add);
    }
    for (track, b) in song.tracks.iter().zip(built.iter_mut()) {
        b.program.snapshot_pre_fx = wanted.contains(&(track.id, TapPoint::PreFx));
        b.program.snapshot_post_fx = wanted.contains(&(track.id, TapPoint::PostFx))
            || track.sends.iter().any(|s| s.mode == common::model::SendMode::PreFader);
    }
}

/// device `chain` (track の `devices` か `master_fx_chain`) 上の consumer の tap を emit する
/// (track 経路と master 経路の唯一の emit site)。`owner_track_id` = 持ち主の track id
/// (master は `MASTER_TRACK_ID`)、`owner_idx` = song-track index (master は [`MASTER_OWNER`])、
/// `stores` = 持ち主の lane / routing (native の `can_activate`)、`program` = 持ち主の program。
///
/// - plugin: 配線のある port ごとに `NodeOp::SidechainTap`。
/// - native: 次の 4 条件を満たすときだけ `NodeOp::NativeSidechainTap` を積み、同じ場所で
///   `natives[slot]` を `Staged` にして受け皿を確保する (off-RT)。
///   1. SC を受ける種類 (`aux_consumers` が保証)
///   2. source が自トラックではない
///   3. source を `BufRef` に解決できる
///   4. op が出ている (`native_slots` に id がある — bypass 中の Parallel の中の native には
///      op が無い)
///
/// 自トラックの Pre-FX は program が同じ pass の snapshot を直接載せる (plugin の
/// `own_prefx_ports` / native の `ScMode::OwnPreFx`)。自トラックの他の tap 点は出力の下流
/// (= feedback) なので staging しない。dangling な source は黙って飛ばす。
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_sidechain_taps(
    chain: &[Device],
    owner_track_id: u32,
    owner_idx: u32,
    stores: (&[AutomationLane], &[ModRouting]),
    program: &mut BuiltProgram,
    taps: &TapCtx<'_>,
    nodes: &mut Vec<NodeOp>,
) {
    for c in aux_consumers(chain, stores.0, stores.1).filter(|c| !c.inactive) {
        // aux port は engine が `MAX_AUX_IN` までしか staging しないので `take` で
        // `port < MAX_AUX_IN` を構造的に保証し、`as u8` の wrap を防ぐ。
        for (port, route) in c.routes.iter().take(common::process_data::MAX_AUX_IN).enumerate() {
            let Some(route) = route else { continue };
            if route.tap.source == TapSource::Track(owner_track_id) {
                continue;
            }
            let Some(src) = tap_bufref_for(&route.tap, taps.id_to_idx, taps.chains) else {
                continue;
            };
            if !c.native {
                // v29: 宛先 plugin は安定 device id で焼き込む (未採番 0 は engine 側の lookup が外れる)。
                nodes.push(NodeOp::SidechainTap { src, device_id: c.device_id, aux_in_port: port as u8 });
                continue;
            }
            let Some(&native_slot) = program.native_slots.get(&c.device_id) else { continue };
            let Some(ns) = program.program.natives.get_mut(native_slot as usize) else { continue };
            ns.sc_mode = ScMode::Staged;
            ns.sc.get_or_insert_with(ScStage::new);
            nodes.push(NodeOp::NativeSidechainTap { src, owner: owner_idx, native_slot });
        }
    }
}

/// path latency の sidechain fan-in: track `idx` の consumer が読む tap について、
/// source 側の latency ([`tap_source_latency`]) + **その consumer の pass の lag** の最大 (無ければ 0)。
/// `path_of(src, cache)` は source track の path latency を `cache` に確定させる
/// (`compute_path_latency` の memoize + 再帰)。同じ track を source にする tap
/// (自己参照 / 自分の chain) は数えない (main の下流なので揃えようが無い)。
pub(super) fn sidechain_input_latency(
    idx: u32,
    track: &Track,
    taps: &TapCtx<'_>,
    track_chain_latency: &[u32],
    cache: &mut [u32],
    path_of: impl Fn(u32, &mut [u32]) -> u32,
) -> u32 {
    let mut sidechain_input: u32 = 0;
    let consumers = aux_consumers(&track.devices, &track.automation_lanes, &track.mod_routings);
    for c in consumers.filter(|c| !c.inactive) {
        let lag = taps.lag(idx, c.top_index);
        for route in c.routes.iter().flatten() {
            let Some(src_idx) = tap_owner_idx(&route.tap, taps.id_to_idx, taps.chains) else {
                continue;
            };
            if src_idx == idx {
                continue;
            }
            path_of(src_idx, cache);
            let l = tap_source_latency(&route.tap, src_idx, cache, track_chain_latency, taps.chains)
                .saturating_add(lag);
            sidechain_input = sidechain_input.max(l);
        }
    }
    sidechain_input
}

/// consumer の入力で main をサイドチェインに揃える遅延 (track ごと、song-track index 順)。
///
/// - `input_delay`: pass 1 の consumer 宛て。`src_latency + buffer_frames` の最大。
///   `process_track_owned` が device チェーンに入る前の main に掛ける (PR4.5)。
/// - `bus_sc_delay`: pass 2 の consumer 宛て。`max(0, src_latency の最大 − non_sc_input)`。
///   `ProcessGroupFx` の直前の `ApplyDelay(BusScAlign)` が bus の入力に掛ける。2 つの遅延を掛けた後の
///   実際の入力 latency は `max(non_sc, sc)` になり、path latency の申告値と揃う。
///
/// main を持つ consumer (audio in + out) だけが対象。自トラックの source は数えない (main の下流)。
pub(super) fn compute_sc_delays(
    song: &Song,
    taps: &TapCtx<'_>,
    path_latency: &[u32],
    track_chain_latency: &[u32],
    non_sc_input: &[u32],
) -> (Vec<u32>, Vec<u32>) {
    let n = song.tracks.len();
    let mut input_delay = vec![0u32; n];
    let mut bus_sc_delay = vec![0u32; n];
    for (i, track) in song.tracks.iter().enumerate() {
        // r.md #131: 無効トラックは処理されないので揃える相手が無い。
        if !taps.enabled.get(i).copied().unwrap_or(false) {
            continue;
        }
        let idx = i as u32;
        let (mut pass1, mut pass2) = (0u32, 0u32);
        let consumers = aux_consumers(&track.devices, &track.automation_lanes, &track.mod_routings);
        for c in consumers.filter(|c| !c.inactive && c.audio_io) {
            let pass = taps.pass(idx, c.top_index);
            for route in c.routes.iter().flatten() {
                let Some(src_idx) = tap_owner_idx(&route.tap, taps.id_to_idx, taps.chains) else {
                    continue;
                };
                if src_idx == idx {
                    continue;
                }
                let l = tap_source_latency(&route.tap, src_idx, path_latency, track_chain_latency, taps.chains);
                match pass {
                    ScPass::Pass1 => pass1 = pass1.max(l.saturating_add(taps.buffer_frames)),
                    ScPass::Pass2 => pass2 = pass2.max(l),
                }
            }
        }
        input_delay[i] = pass1;
        bus_sc_delay[i] = pass2.saturating_sub(non_sc_input.get(i).copied().unwrap_or(0));
    }
    (input_delay, bus_sc_delay)
}
