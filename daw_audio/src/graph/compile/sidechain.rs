//! Sidechain の会計 — sidechain を読む consumer を走査する処理はすべてここに置く。
//!
//! consumer = device chain 上 (Parallel の中も `plugins` が辿る) の plugin の
//! `aux_inputs`。走査は次の 5 つ:
//! - [`collect_chain_taps`] — chain の snapshot flag を焼く入力 (`build_all_programs`)
//! - [`sidechain_dep_edges`] — path latency の依存辺 (`deps::execution_order`)
//! - [`emit_aux_input_taps`] — `NodeOp::SidechainTap` の emit (`emit::emit_track_ops`)
//! - [`sidechain_input_latency`] — path latency への fan-in (`pdc::compute_path_latency`)
//! - [`compute_input_delays`] — plugin 入力で main を aux に揃える input delay
//!
//! 5 つとも r.md #105 の規則を共有する: bypass 中の device は dispatch されないので、
//! その配線は snapshot 要求 / 依存辺 / tap / latency のどれにも数えない。leaf 宛の
//! tap の staging lag は [`sidechain_tap_lag`] の 1 か所で決める。

use std::collections::{HashMap, HashSet};

use common::model::{AudioTap, Device, Song, TapPoint, TapSource, plugins};

use super::{ChainMap, tap_bufref_for};
use crate::graph::schedule::{MASTER_OWNER, NodeOp};

/// tap の source を解決するのに使う表の束 (`compute_path_latency` / 依存辺)。
pub(super) struct TapCtx<'a> {
    pub(super) id_to_idx: &'a HashMap<u32, u32>,
    pub(super) chains: &'a ChainMap,
}

/// tap の source が属する **song-track index** (依存辺 / path latency 用)。
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

/// leaf 宛の tap の staging lag: leaf の device は pass 1 で process 済みなので、
/// post-dispatch で staging した source は次 buffer の process で消費される =
/// `buffer_frames` 遅れる。bus (group / return / paraout dest) の device は pass 2 で
/// 同 buffer 内に消費するので 0。path latency の fan-in と input delay は必ずこの値を
/// 使う (食い違うと plugin 入力での main/aux 揃えと master 合流の sibling alignment が
/// ズレる)。
pub(super) fn sidechain_tap_lag(is_bus: bool, buffer_frames: u32) -> u32 {
    if is_bus { 0 } else { buffer_frames }
}

/// `(chain_id, tap_point)` を誰かが読むか (chain の snapshot flag を焼く入力)。
pub(super) fn collect_chain_taps(song: &Song) -> HashSet<(u64, TapPoint)> {
    let mut set = HashSet::new();
    let mut add = |tap: &AudioTap| {
        if let TapSource::Chain(c) = tap.source {
            set.insert((c, tap.tap_point));
        }
    };
    for p in song.all_plugins().filter(|p| !p.bypassed) {
        for route in p.aux_inputs.iter().flatten() {
            add(&route.tap);
        }
    }
    for ms in &song.mod_sources {
        if let Some((tap, _)) = ms.follower() {
            add(tap);
        }
    }
    set
}

/// 依存辺のうち sidechain の分: track `idx` の `devices` 上の consumer が読む tap の
/// source track を `out` へ積む。
///
/// v23 single-chain: sidechain wiring lives on every device's
/// `aux_inputs` regardless of its derived role, so a single walk over
/// `devices` covers what the old per-section walks did.
/// r.md #110: 同 track の chain を source にする tap は自己辺にしない
/// (前 buffer の snapshot を読む = 1 buffer 遅れ、cycle ではない)。
pub(super) fn sidechain_dep_edges(idx: u32, devices: &[Device], taps: &TapCtx<'_>, out: &mut Vec<u32>) {
    for p in plugins(devices).filter(|p| !p.bypassed) {
        for route in p.aux_inputs.iter().flatten() {
            if let Some(src_idx) = tap_owner_idx(&route.tap, taps.id_to_idx, taps.chains)
                && src_idx != idx
            {
                out.push(src_idx);
            }
        }
    }
}

/// docs/plan_modulation.md §5: walk a device `chain` (a track's `devices` or
/// the master `master_fx_chain`) and emit `NodeOp::SidechainTap` for every
/// plugin `aux_inputs` route whose source track exists. The single helper
/// replaces the former per-track `emit_sidechain_taps` + the inlined
/// master-bus loop (critique #1: there were two emit sites). `owner_track_id` is
/// the destination plugin's owning track id (`MASTER_TRACK_ID` for master
/// fx). dangling references are skipped (no compile error). `ProcessTrack` of
/// the source runs earlier, so its scratch is settled by the time the tap copies it.
pub(super) fn emit_aux_input_taps(
    chain: &[common::model::Device],
    owner_track_id: u32,
    id_to_idx: &HashMap<u32, u32>,
    chains: &ChainMap,
    nodes: &mut Vec<NodeOp>,
) {
    // r.md #105: bypass 中の device は process されないので tap も staging しない。
    // r.md #110: Parallel の中の plugin も `plugins` が辿る。
    for inst in plugins(chain).filter(|p| !p.bypassed) {
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
            // 自 track の Pre-FX は program が同じ pass の snapshot を直接載せる
            // (`ChainOp::Plugin::own_prefx_ports`)。 自 track の他の tap 点は出力の
            // 下流 (= feedback) なので staging しない。
            if route.tap.source == TapSource::Track(owner_track_id) {
                continue;
            }
            let Some(src) = tap_bufref_for(&route.tap, id_to_idx, chains) else {
                // dangling reference: silently skip
                continue;
            };
            // v29: 宛先 plugin は安定 device id で焼き込む。id 未採番 (0) の
            // instance は engine 側 lookup が必ず外れる (= 旧来の「lookup miss
            // で skip」と同じ寛容さ) なのでここでは弾かない。
            nodes.push(NodeOp::SidechainTap {
                src,
                device_id: inst.id,
                aux_in_port: port_idx as u8,
            });
        }
    }
}

/// path latency の sidechain fan-in: track `idx` の `devices` 上の consumer が読む tap
/// について、source 側の latency ([`tap_source_latency`]) + `tap_lag` の最大 (無ければ 0)。
/// `path_of(src, cache)` は source track の path latency を `cache` に確定させる
/// (`compute_path_latency` の memoize + 再帰)。
///
/// v23 single-chain: latency propagation cares about every device's
/// sidechain source regardless of role (a sidechain edge from any device
/// raises this track's input latency), so a single walk over `devices`
/// replaces the old per-section walks. r.md #110: Parallel の中も `plugins` が辿る。
/// chain source は所有 track の入力 latency + chain までの相対 latency。同 track の
/// chain (自己参照) は数えない (main の下流なので揃えようが無い)。
pub(super) fn sidechain_input_latency(
    idx: u32,
    devices: &[Device],
    taps: &TapCtx<'_>,
    track_chain_latency: &[u32],
    tap_lag: u32,
    cache: &mut [u32],
    path_of: impl Fn(u32, &mut [u32]) -> u32,
) -> u32 {
    let mut sidechain_input: u32 = 0;
    for p in plugins(devices).filter(|p| !p.bypassed) {
        for route in p.aux_inputs.iter().flatten() {
            let Some(src_idx) = tap_owner_idx(&route.tap, taps.id_to_idx, taps.chains) else {
                continue;
            };
            if src_idx == idx {
                continue;
            }
            path_of(src_idx, cache);
            let l = tap_source_latency(&route.tap, src_idx, cache, track_chain_latency, taps.chains)
                .saturating_add(tap_lag);
            sidechain_input = sidechain_input.max(l);
        }
    }
    sidechain_input
}

/// PR4.5 sidechain plugin-internal alignment: per-track input delay.
/// The delay is applied to the track's main signal so it lines up with any
/// sidechain a device reads. Only audio-processing devices (= has both an
/// audio input and an audio output) can read a sidechain, so only those
/// contribute (a pure source / MIDI device has no main-in to delay against).
/// v23 single-chain: a direct port predicate, no role derivation. Edit-time.
/// §5 (arch refactor): leaf 宛の tap は staging→消費が 1 buffer ずれるので
/// `buffer_frames` を加算して plugin 入力での main vs aux を位相一致させる
/// (compute_path_latency の sidechain edge と同じ規則 — [`sidechain_tap_lag`])。
/// r.md #110: 同 track の chain source は main と揃えようが無い (chain 出力は main の
/// 下流) ので数えない。
#[allow(clippy::too_many_arguments)]
pub(super) fn compute_input_delays(
    song: &Song,
    bus_flags: &[bool],
    buffer_frames: u32,
    id_to_idx: &HashMap<u32, u32>,
    chain_map: &ChainMap,
    path_latency: &[u32],
    track_chain_latency: &[u32],
) -> Vec<u32> {
    let mut out = vec![0u32; song.tracks.len()];
    for (i, track) in song.tracks.iter().enumerate() {
        let tap_lag = sidechain_tap_lag(bus_flags[i], buffer_frames);
        let mut max_sc: u32 = 0;
        let routes = plugins(&track.devices)
            .filter(|p| !p.bypassed && p.ports.has_audio_input && p.ports.has_audio_output)
            .flat_map(|p| p.aux_inputs.iter().flatten());
        for route in routes {
            let Some(src_idx) = tap_owner_idx(&route.tap, id_to_idx, chain_map) else {
                continue;
            };
            if src_idx as usize == i {
                continue;
            }
            let l = tap_source_latency(&route.tap, src_idx, path_latency, track_chain_latency, chain_map)
                .saturating_add(tap_lag);
            max_sc = max_sc.max(l);
        }
        out[i] = max_sc;
    }
    out
}
