//! 配線トポロジと実行順。
//!
//! [`Topology`] は track id → index / 親参照の検査 / group の子・send・パラアウトの入力表 /
//! bus 判定を 1 回だけ組み、op の emit と PDC が共有する。[`execution_order`] は
//! path latency の依存辺 (children / sidechain source / send source) の post-order (= 実行順) を
//! 返し、循環は `GraphError::Cycle` で弾く。辺の定義は `common::routing_deps::TrackDeps` が唯一の
//! 実装で、GUI の配線ガード (Structural) と engine の実行順 (Active) が同じ辺を数える
//! (`docs/plan_rack_native_devices.md` §5.9)。

use std::collections::{HashMap, HashSet};

use common::model::{SendMode, Song, plugins};
use common::routing_deps::{EdgeScope, TrackDeps};

use super::GraphError;

/// `compile_schedule` の各段が共有する配線の表 (song-track index は `song.tracks` の並び)。
pub(super) struct Topology {
    /// `Track::id` → song-track index。id 0 (未採番) は載せない。
    pub(super) id_to_idx: HashMap<u32, u32>,
    /// 子を 1 つ以上持つ track の id。
    pub(super) is_group: HashSet<u32>,
    /// group track id → 子の song-track index (song 順)。
    pub(super) children_of: HashMap<u32, Vec<u32>>,
    /// dest track id → (source track index, stable `Send::id`, tap mode)。
    pub(super) incoming_sends: HashMap<u32, Vec<(u32, u32, SendMode)>>,
    /// dest track id → (source track id, stable device id, aux out port)。
    pub(super) incoming_paraout: HashMap<u32, Vec<(u32, u64, u8)>>,
    /// song-track index → bus か。
    pub(super) bus_flags: Vec<bool>,
    /// song-track index → group-with-instrument (パラアウトの楽器兼 group) の top-level 分割点。
    /// それ以外の track は `None`。`[..split]` が pass 1、残りが pass 2 (`ProcessGroupFx`)。
    pub(super) gwi_split: Vec<Option<u32>>,
}

impl Topology {
    /// 存在しない track を親に指す track があれば `DanglingReference`。
    pub(super) fn build(song: &Song) -> Result<Self, GraphError> {
        let n = song.tracks.len();
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
        // (required by both cycle detection and the bus ops)
        let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
        for (idx, t) in song.tracks.iter().enumerate() {
            if let Some(pid) = t.parent_group_id {
                children_of.entry(pid).or_default().push(idx as u32);
            }
        }

        let incoming_sends = gather_incoming_sends(song, &id_to_idx);
        let incoming_paraout = gather_incoming_paraout(song, &id_to_idx);

        // ---- bus / leaf classification (shared by op emission + PDC) ----
        // A track is a "bus" if it has children (a group), incoming sends (a
        // return), or incoming paraout (a parallel-out destination). A bus's
        // devices run in pass 2 (`ProcessGroupFx` / master fx) *after* the
        // sidechain taps of the same buffer; a leaf's devices ran in pass 1
        // *before* them — so leaf-destined taps are consumed one buffer late
        // and get `buffer_frames` of extra latency in the PDC math.
        let bus_flags: Vec<bool> = song
            .tracks
            .iter()
            .map(|t| {
                is_group.contains(&t.id)
                    || incoming_sends.contains_key(&t.id)
                    || incoming_paraout.contains_key(&t.id)
            })
            .collect();
        let gwi_split: Vec<Option<u32>> = song
            .tracks
            .iter()
            .map(|t| if is_group.contains(&t.id) { t.paraout_split_device() } else { None })
            .collect();

        Ok(Self {
            id_to_idx,
            is_group,
            children_of,
            incoming_sends,
            incoming_paraout,
            bus_flags,
            gwi_split,
        })
    }

    /// 各 track (song 順) へ **流れ込む** track の推移閉包 (song-track index、昇順)。寄与辺は
    /// 「子 → group」と「send 元 → send 先」(有効 / 無効を問わない)。`ChainProgram::solo_contributors`
    /// に焼き、RT の solo 判定は表の track の `solo` を見るだけにする (`docs/plan_unbounded_tracks.md` §2.3 —
    /// 旧実装は RT で Song の配線を固定長のスタック配列で BFS していて、32 本を超えると判定が壊れた)。
    pub(super) fn solo_contributors(&self, song: &Song) -> Vec<Vec<u32>> {
        let mut seen = vec![false; song.tracks.len()];
        let mut stack: Vec<u32> = Vec::new();
        (0..song.tracks.len()).map(|i| self.contributors_of(song, i, &mut seen, &mut stack)).collect()
    }

    /// 各 track (song 順) の祖先 group (`parent_group_id` を辿った song-track index、近い順)。
    /// `ChainProgram::solo_ancestors` に焼く (folder solo)。親参照は `build` で検査済み、循環に備えて
    /// 本数で打ち切る。
    pub(super) fn solo_ancestors(&self, song: &Song) -> Vec<Vec<u32>> {
        let n = song.tracks.len();
        song.tracks
            .iter()
            .map(|t| {
                let mut out = Vec::new();
                let mut cur = t.parent_group_id.and_then(|pid| self.id_to_idx.get(&pid).copied());
                while let Some(idx) = cur.filter(|_| out.len() < n) {
                    out.push(idx);
                    cur = song.tracks[idx as usize].parent_group_id.and_then(|pid| self.id_to_idx.get(&pid).copied());
                }
                out
            })
            .collect()
    }

    /// `solo_contributors` の 1 行ぶん (`seen` / `stack` は呼び側が使い回す作業領域)。
    fn contributors_of(&self, song: &Song, i: usize, seen: &mut [bool], stack: &mut Vec<u32>) -> Vec<u32> {
        seen.fill(false);
        seen[i] = true;
        stack.clear();
        stack.push(song.tracks[i].id);
        let mut out = Vec::new();
        while let Some(node) = stack.pop() {
            let children = self.children_of.get(&node).into_iter().flatten().copied();
            let senders = self.incoming_sends.get(&node).into_iter().flatten().map(|&(src, _, _)| src);
            for idx in children.chain(senders) {
                if seen.get_mut(idx as usize).is_some_and(|flag| !std::mem::replace(flag, true)) {
                    out.push(idx);
                    stack.push(song.tracks[idx as usize].id);
                }
            }
        }
        out.sort_unstable();
        out
    }
}

/// Gather incoming aux sends per destination (return / bus).
/// `incoming_sends[dest_id]` = list of (source track index, stable
/// `Send::id`, tap mode) for every send landing on `dest_id`. Sends whose
/// dest does not exist are dropped (tolerant, like dangling sidechain). A
/// track with ≥1 incoming send acts as a bus (summed like a group) even
/// with no children. v29: positional send index は schedule に焼き込まない
/// — engine は `send_id` で live lookup する。
fn gather_incoming_sends(
    song: &Song,
    id_to_idx: &HashMap<u32, u32>,
) -> HashMap<u32, Vec<(u32, u32, SendMode)>> {
    let mut incoming_sends: HashMap<u32, Vec<(u32, u32, SendMode)>> = HashMap::new();
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
    incoming_sends
}

/// パラアウト (docs/plan_paraout.md): gather incoming aux outputs per
/// destination. `incoming_paraout[dest_id]` = list of (source track id,
/// stable device id, aux out port) for every plugin aux output routed at
/// `dest_id`. v29: the engine resolves the source plugin directly by
/// `device_id` (`PluginInstance::id`); the track id is kept only for the
/// PDC fan-in (`pdc::fan_in_paraout_latency`). Bounded by `MAX_AUX_OUT` (the
/// engine only fills that many aux out ports). A track with ≥1 incoming paraout
/// acts as a bus (summed + FX'd in pass 2), exactly like a group / return; routes
/// to a missing dest are dropped (tolerant, like dangling sidechain / send). No
/// DAG edge is added: the source plugin's aux output is produced in pass 1
/// (`buffer_aux_out`) and consumed in pass 2, so pass 1 always precedes
/// the read — which is also why a group-with-instrument (A sums children
/// B/C while B/C read A's aux) is NOT a cycle.
fn gather_incoming_paraout(
    song: &Song,
    id_to_idx: &HashMap<u32, u32>,
) -> HashMap<u32, Vec<(u32, u64, u8)>> {
    let mut incoming_paraout: HashMap<u32, Vec<(u32, u64, u8)>> = HashMap::new();
    let owners = song
        .tracks
        .iter()
        .map(|t| (t.devices.as_slice(), t.id))
        .chain(std::iter::once((song.master_fx_chain.as_slice(), common::model::MASTER_TRACK_ID)));
    for (chain, src_track_id) in owners {
        for p in plugins(chain) {
            let routes = p
                .aux_outputs
                .iter()
                .take(common::process_data::MAX_AUX_OUT)
                .enumerate();
            for (port, route_opt) in routes {
                let Some(route) = route_opt else { continue };
                if !id_to_idx.contains_key(&route.dest_track) {
                    continue;
                }
                let edges = incoming_paraout.entry(route.dest_track).or_default();
                edges.push((src_track_id, p.id, port as u8));
            }
        }
    }
    incoming_paraout
}

/// path latency の依存 graph の post-order (= 実行順、song-track index) を返す。
///
/// `compute_path_latency` は children / sidechain source / send source を再帰で辿るので、
/// この graph の循環は無限再帰 = compile の前に弾く必要がある (children 辺が親の循環を、
/// sidechain 辺がフィードバック (A → A、A → B → A) を、send 辺が send のループを覆う)。
/// 辺の定義と 3 色 DFS は `TrackDeps` (`EdgeScope::Active` = 処理しうる consumer だけ)。
/// post-order なので producer は必ず consumer より前に並ぶ。
pub(super) fn execution_order(song: &Song) -> Result<Vec<u32>, GraphError> {
    TrackDeps::build(song, EdgeScope::Active)
        .dependency_order()
        .map_err(|_| GraphError::Cycle)
}
