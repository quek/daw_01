//! パラメーターの置き場 (lane / routing の store) の解決と、dangling 参照の掃除
//! (`docs/plan_rack_native_devices.md` §5.8 / §7.2)。
//!
//! 置き場の規則は plugin と同じ: トラックの device → そのトラックの `automation_lanes` /
//! `mod_routings`、master fx chain の device と song-wide param → `song_lanes` /
//! `song_mod_routings`。**置き場の分岐はここ 1 か所** — 呼び出し側で `MASTER_TRACK_ID` を
//! 比べて track / song を選ぶコードを書かない。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use std::collections::{HashMap, HashSet};

use super::*;

/// prune の node 表の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Plugin,
    Native(NativeKind),
    Parallel,
    Chain,
}

impl AutomationTarget {
    /// device / chain / Parallel の **id で束縛する** target の束縛先 id。それ以外は `None`。
    #[must_use]
    pub fn bound_node_id(&self) -> Option<u64> {
        use TrackBuiltinParam as B;
        match self {
            Self::PluginParam { device_id, .. } | Self::NativeParam { device_id, .. } => Some(*device_id),
            Self::TrackBuiltin(b) => match b {
                B::ChainGain { chain_id } | B::ChainPan { chain_id } => Some(*chain_id),
                B::ParallelOutGain { parallel_id }
                | B::ParallelSplitFreq { parallel_id, .. }
                | B::ParallelSelect { parallel_id } => Some(*parallel_id),
                B::Volume | B::Pan | B::Mute | B::SendGain { .. } => None,
            },
            Self::MasterLimiter(_)
            | Self::SongTempo
            | Self::SongTimeSigNumerator
            | Self::ImageBuiltin(_)
            | Self::TextBuiltin(_)
            | Self::GroupTransform(_)
            | Self::ModSourceParam { .. }
            | Self::ModRoutingDepth { .. } => None,
        }
    }

    /// [`Self::bound_node_id`] の可変版 (コピー / 複製で id を貼り替える)。
    pub fn bound_node_id_mut(&mut self) -> Option<&mut u64> {
        use TrackBuiltinParam as B;
        match self {
            Self::PluginParam { device_id, .. } | Self::NativeParam { device_id, .. } => Some(device_id),
            Self::TrackBuiltin(b) => match b {
                B::ChainGain { chain_id } | B::ChainPan { chain_id } => Some(chain_id),
                B::ParallelOutGain { parallel_id }
                | B::ParallelSplitFreq { parallel_id, .. }
                | B::ParallelSelect { parallel_id } => Some(parallel_id),
                B::Volume | B::Pan | B::Mute | B::SendGain { .. } => None,
            },
            Self::MasterLimiter(_)
            | Self::SongTempo
            | Self::SongTimeSigNumerator
            | Self::ImageBuiltin(_)
            | Self::TextBuiltin(_)
            | Self::GroupTransform(_)
            | Self::ModSourceParam { .. }
            | Self::ModRoutingDepth { .. } => None,
        }
    }
}

impl Song {
    /// `owner` (track id か `MASTER_TRACK_ID`) の lane / routing store。置き場規則の唯一の実装。
    /// 確保なし (RT 可)。`0` は解釈しない (legacy の 0 → master は `mod_source_owner` の責務)。
    #[must_use]
    pub fn param_stores(&self, owner: u32) -> Option<(&[AutomationLane], &[ModRouting])> {
        if owner == MASTER_TRACK_ID {
            return Some((&self.song_lanes, &self.song_mod_routings));
        }
        self.tracks
            .iter()
            .find(|t| t.id == owner && owner != 0)
            .map(|t| (t.automation_lanes.as_slice(), t.mod_routings.as_slice()))
    }

    /// [`Self::param_stores`] の可変版。
    pub fn param_stores_mut(&mut self, owner: u32) -> Option<(&mut Vec<AutomationLane>, &mut Vec<ModRouting>)> {
        if owner == MASTER_TRACK_ID {
            return Some((&mut self.song_lanes, &mut self.song_mod_routings));
        }
        self.tracks
            .iter_mut()
            .find(|t| t.id == owner && owner != 0)
            .map(|t| (&mut t.automation_lanes, &mut t.mod_routings))
    }

    /// `lane` を `owner` の store に積む。lane id は `owner` の allocator で**必ず**振り直す。
    /// 戻り値 = 新しい lane id (`None` = owner が無い)。
    pub fn push_lane(&mut self, owner: u32, mut lane: AutomationLane) -> Option<u32> {
        if owner == MASTER_TRACK_ID {
            lane.id = self.alloc_song_lane_id();
            let id = lane.id;
            self.song_lanes.push(lane);
            return Some(id);
        }
        let t = self.tracks.iter_mut().find(|t| t.id == owner && owner != 0)?;
        lane.id = t.alloc_lane_id();
        let id = lane.id;
        t.automation_lanes.push(lane);
        Some(id)
    }

    /// id で束縛する target の store の持ち主 (track id か `MASTER_TRACK_ID`)。target だけでは
    /// 決まらない住所 (Volume / Pan / Mute / SendGain / Image / Text / Group) は `None`
    /// (呼び出し側の track が持ち主)。
    #[must_use]
    pub fn bound_owner_track(&self, target: &AutomationTarget) -> Option<u32> {
        use TrackBuiltinParam as B;
        match target {
            AutomationTarget::PluginParam { device_id, .. } | AutomationTarget::NativeParam { device_id, .. } => {
                self.device_owner_track(*device_id)
            }
            AutomationTarget::TrackBuiltin(b) => match b {
                B::ChainGain { chain_id } | B::ChainPan { chain_id } => {
                    self.chain_owner_track(ChainRef::Chain(*chain_id))
                }
                B::ParallelOutGain { parallel_id }
                | B::ParallelSplitFreq { parallel_id, .. }
                | B::ParallelSelect { parallel_id } => self.device_owner_track(*parallel_id),
                B::Volume | B::Pan | B::Mute | B::SendGain { .. } => None,
            },
            AutomationTarget::ModSourceParam { source_id, .. } => self.mod_source_owner(*source_id),
            AutomationTarget::ModRoutingDepth { routing_id } => self.mod_routing_owner(*routing_id),
            AutomationTarget::MasterLimiter(_)
            | AutomationTarget::SongTempo
            | AutomationTarget::SongTimeSigNumerator => Some(MASTER_TRACK_ID),
            AutomationTarget::ImageBuiltin(_)
            | AutomationTarget::TextBuiltin(_)
            | AutomationTarget::GroupTransform(_) => None,
        }
    }

    /// track と master row を統一的に引く lane accessor (`track_id == MASTER_TRACK_ID` は song lane)。
    #[must_use]
    pub fn automation_lane_by_key(&self, track_id: u32, lane_id: u32) -> Option<&AutomationLane> {
        self.param_stores(track_id)?.0.iter().find(|l| l.id == lane_id)
    }

    /// [`Self::automation_lane_by_key`] の可変版。
    pub fn automation_lane_by_key_mut(&mut self, track_id: u32, lane_id: u32) -> Option<&mut AutomationLane> {
        self.param_stores_mut(track_id)?.0.iter_mut().find(|l| l.id == lane_id)
    }

    /// dangling な lane / routing / MIDI binding を**固定点まで**掃除する。**冪等** (2 回目は
    /// `false`)。`enforce_edit_invariants` と `normalize_after_load` の一部。
    ///
    /// 残す条件は `keep_target` の表 (id で束縛する target は「その id の node が実在し、
    /// その store の持ち主に居る」)。変調を 1 本消すとその深さを指す変調 / レーンが dangling に
    /// なるので、変化が無くなるまで回す。
    pub fn prune_dangling_param_targets(&mut self) -> bool {
        let mut changed_any = false;
        loop {
            let nodes = self.node_table();
            let live_sources: HashSet<u32> = self.mod_sources.iter().map(|m| m.id).collect();
            let live_routings: HashSet<u32> = self.all_mod_routings().map(|r| r.id).collect();
            let ctx = PruneCtx { nodes: &nodes, live_sources: &live_sources, live_routings: &live_routings };
            let mut changed = false;
            let mut sweep = |owner: u32, lanes: &mut Vec<AutomationLane>, routings: &mut Vec<ModRouting>| {
                let (nl, nr) = (lanes.len(), routings.len());
                lanes.retain(|l| ctx.keep_target(&l.target, owner));
                routings.retain(|r| ctx.live_sources.contains(&r.source_id) && ctx.keep_target(&r.target, owner));
                changed |= lanes.len() != nl || routings.len() != nr;
            };
            for t in &mut self.tracks {
                sweep(t.id, &mut t.automation_lanes, &mut t.mod_routings);
            }
            sweep(MASTER_TRACK_ID, &mut self.song_lanes, &mut self.song_mod_routings);
            let nb = self.midi_bindings.len();
            self.midi_bindings.retain(|b| ctx.keep_binding(&b.target));
            changed |= self.midi_bindings.len() != nb;
            if !changed {
                return changed_any;
            }
            changed_any = true;
        }
    }

    /// 全チェーンの node (plugin / native / Parallel / chain) → (持ち主, 種類)。
    fn node_table(&self) -> HashMap<u64, (u32, NodeKind)> {
        let mut nodes = HashMap::new();
        let owners = self
            .tracks
            .iter()
            .map(|t| (t.id, t.devices.as_slice()))
            .chain(std::iter::once((MASTER_TRACK_ID, self.master_fx_chain.as_slice())));
        for (owner, devices) in owners {
            collect_nodes(devices, owner, &mut nodes);
        }
        nodes
    }
}

fn collect_nodes(devices: &[Device], owner: u32, out: &mut HashMap<u64, (u32, NodeKind)>) {
    for d in devices {
        match d {
            Device::Plugin(p) => {
                out.insert(p.id, (owner, NodeKind::Plugin));
            }
            Device::Native(n) => {
                out.insert(n.id, (owner, NodeKind::Native(n.kind())));
            }
            Device::Parallel(r) => {
                out.insert(r.id, (owner, NodeKind::Parallel));
                for c in &r.chains {
                    out.insert(c.id, (owner, NodeKind::Chain));
                    collect_nodes(&c.devices, owner, out);
                }
            }
        }
    }
}

struct PruneCtx<'a> {
    nodes: &'a HashMap<u64, (u32, NodeKind)>,
    live_sources: &'a HashSet<u32>,
    live_routings: &'a HashSet<u32>,
}

impl PruneCtx<'_> {
    fn node_is(&self, id: u64, owner: u32, kind: NodeKind) -> bool {
        self.nodes.get(&id) == Some(&(owner, kind))
    }

    /// store の持ち主 `owner` に置かれた `target` を残すか (網羅、`_` を書かない)。
    fn keep_target(&self, target: &AutomationTarget, owner: u32) -> bool {
        use TrackBuiltinParam as B;
        match target {
            AutomationTarget::PluginParam { device_id, .. } => self.node_is(*device_id, owner, NodeKind::Plugin),
            AutomationTarget::NativeParam { device_id, param } => {
                param.exists() && self.node_is(*device_id, owner, NodeKind::Native(param.kind()))
            }
            AutomationTarget::TrackBuiltin(b) => match b {
                B::ChainGain { chain_id } | B::ChainPan { chain_id } => {
                    self.node_is(*chain_id, owner, NodeKind::Chain)
                }
                B::ParallelOutGain { parallel_id }
                | B::ParallelSplitFreq { parallel_id, .. }
                | B::ParallelSelect { parallel_id } => self.node_is(*parallel_id, owner, NodeKind::Parallel),
                B::Volume | B::Pan | B::Mute | B::SendGain { .. } => true,
            },
            AutomationTarget::MasterLimiter(_)
            | AutomationTarget::SongTempo
            | AutomationTarget::SongTimeSigNumerator => owner == MASTER_TRACK_ID,
            AutomationTarget::ModSourceParam { source_id, .. } => self.live_sources.contains(source_id),
            AutomationTarget::ModRoutingDepth { routing_id } => self.live_routings.contains(routing_id),
            AutomationTarget::ImageBuiltin(_)
            | AutomationTarget::TextBuiltin(_)
            | AutomationTarget::GroupTransform(_) => true,
        }
    }

    /// MIDI binding を残すか (device は持ち主を問わず実在と種類で判定する)。
    fn keep_binding(&self, target: &BindingTarget) -> bool {
        match target {
            BindingTarget::PluginParam { device_id, .. } => {
                matches!(self.nodes.get(device_id), Some((_, NodeKind::Plugin)))
            }
            BindingTarget::NativeParam { device_id, param } => {
                param.exists() && matches!(self.nodes.get(device_id), Some((_, NodeKind::Native(k))) if *k == param.kind())
            }
            BindingTarget::MasterLimiter(_)
            | BindingTarget::TrackVolume(_)
            | BindingTarget::TrackPan(_)
            | BindingTarget::SongTempo
            | BindingTarget::LaunchCell { .. }
            | BindingTarget::LaunchScene { .. }
            | BindingTarget::StopLauncherRow { .. }
            | BindingTarget::StopAllLauncherRows
            | BindingTarget::SwitchRowToArranger { .. }
            | BindingTarget::SwitchAllToArranger => true,
        }
    }
}
