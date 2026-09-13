//! 配線トポロジと実行順。
//!
//! [`Topology`] は track id → index / 親参照の検査 / group の子・send・パラアウトの入力表 /
//! bus 判定を 1 回だけ組み、op の emit と PDC が共有する。[`execution_order`] は
//! path latency の依存辺 (children / sidechain source / send source) を DFS し、循環を
//! `GraphError::Cycle` で弾いて post-order (= 実行順) を返す。

use std::collections::{HashMap, HashSet};

use common::model::{SendMode, Song, plugins};

use super::GraphError;
use super::sidechain::{TapCtx, sidechain_dep_edges};

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

        Ok(Self {
            id_to_idx,
            is_group,
            children_of,
            incoming_sends,
            incoming_paraout,
            bus_flags,
        })
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

/// Detect cycles in the path_latency dependency graph and return its post-order.
///
/// `compute_path_latency` recurses through:
///   1. children of a group track (group depends on every child's path_latency)
///   2. sidechain sources of plugins on the track (track depends on each
///      sidechain source's path_latency)
///   3. send sources of a return / bus
///
/// Cycle in this dep-graph ⇒ infinite recursion in path_latency ⇒ must
/// reject up front. Iterative 3-color DFS over the dep edges ([`dep_edges`]).
///
/// PR4: this subsumes the old parent-chain-only detector — children-of
/// edges cover all parent cycles, sidechain-source edges cover all
/// sidechain feedback (incl. self-feedback A → A and A→B→A).
///
/// Post-order of the dependency DFS = a valid execution order: a node
/// is appended only after all its dependencies (children + sidechain
/// sources + send sources) are done, so producers always precede
/// consumers. Replaces the old parent-only depth sort, which couldn't
/// order send / sidechain edges between same-depth tracks.
pub(super) fn execution_order(
    song: &Song,
    topo: &Topology,
    taps: &TapCtx<'_>,
) -> Result<Vec<u32>, GraphError> {
    let n = song.tracks.len();
    let mut state = vec![0u8; n]; // 0=unvisited, 1=on current path, 2=done
    let mut order: Vec<u32> = Vec::with_capacity(n);
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        // Stack carries (node, next-edge-index-to-explore).
        let mut stack: Vec<(u32, usize)> = vec![(start as u32, 0)];
        state[start] = 1;
        while let Some(&(node, edge_i)) = stack.last() {
            let deps = dep_edges(song, topo, taps, node);
            if edge_i >= deps.len() {
                state[node as usize] = 2;
                order.push(node);
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
    Ok(order)
}

/// track `idx` が path latency で依存する track の index (children → sidechain source →
/// send source の順。DFS の訪問順 = 実行順を決めるので並びも仕様)。
fn dep_edges(song: &Song, topo: &Topology, taps: &TapCtx<'_>, idx: u32) -> Vec<u32> {
    let track = &song.tracks[idx as usize];
    let mut out: Vec<u32> = Vec::new();
    if let Some(kids) = topo.children_of.get(&track.id) {
        out.extend(kids.iter().copied());
    }
    sidechain_dep_edges(idx, &track.devices, taps, &mut out);
    // send edges: this track (the destination / return) depends on
    // every track that sends into it — the source must run before the
    // send is mixed in. Covers send feedback (A→B→A, self-send) for
    // cycle detection.
    if let Some(edges) = topo.incoming_sends.get(&track.id) {
        for &(src_idx, _, _) in edges {
            out.push(src_idx);
        }
    }
    out
}
