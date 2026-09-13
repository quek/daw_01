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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_format::PluginFormat;

    fn plug(id: u64) -> Device {
        Device::Plugin(PluginInstance { id, ..PluginInstance::new(format!("p{id}"), PluginFormat::Clap) })
    }

    fn builtin(kind: NativeKind, id: u64) -> Device {
        Device::Native(NativeDevice::new_builtin(kind, id))
    }

    fn lane(target: AutomationTarget) -> AutomationLane {
        AutomationLane::new(target, 0.0)
    }

    fn routing(id: u32, target: AutomationTarget) -> ModRouting {
        ModRouting { id, target, source_id: 1, depth: 0.5, polarity: Polarity::Unipolar, enabled: true }
    }

    fn binding(target: BindingTarget) -> MidiBinding {
        MidiBinding { channel: 0, input: MidiBindInput::ControlChange(1), legacy_controller: None, target }
    }

    fn comp_thr(device_id: u64) -> AutomationTarget {
        AutomationTarget::NativeParam { device_id, param: NativeParamId::Comp(CompParam::Threshold) }
    }

    /// F-C9: dangling な lane / routing / MIDI binding を固定点まで掃除し、実在するものは残す。2 回目は false。
    #[test]
    fn prune_dangling_param_targets_drops_dangling_and_keeps_live_targets() {
        use AutomationTarget as T;
        use TrackBuiltinParam as B;
        let mut p = Parallel::new();
        p.id = 10;
        p.chains[0].id = 11;
        p.chains[0].devices = vec![plug(12)];
        let keep_t1 = vec![
            lane(comp_thr(3)),
            lane(T::TrackBuiltin(B::ChainGain { chain_id: 11 })),
            lane(T::PluginParam { device_id: 12, param_id: 1, legacy_device_index: None }),
            lane(T::TrackBuiltin(B::Volume)),
        ];
        let drop_t1 = vec![
            lane(comp_thr(99)),                                                          // 消えた native
            lane(comp_thr(5)),                                                           // 別トラックの device
            lane(comp_thr(4)),                                                           // 種類違い (4 は EQ)
            lane(T::NativeParam { device_id: 4, param: NativeParamId::Eq { band: EqBand::Hp, param: EqParam::Gain } }),
            lane(T::TrackBuiltin(B::ChainGain { chain_id: 55 })),                        // 消えた chain
            lane(T::PluginParam { device_id: 10, param_id: 1, legacy_device_index: None }), // Parallel を指す PluginParam
            lane(T::MasterLimiter(MasterLimiterParam::Ceiling)),                         // 置き場違い
            lane(T::ModRoutingDepth { routing_id: 2 }),                                  // 連鎖で消える深さの深さ
        ];
        let t1 = Track {
            id: 1,
            devices: vec![builtin(NativeKind::Comp, 3), builtin(NativeKind::Eq, 4), Device::Parallel(p)],
            automation_lanes: keep_t1.iter().chain(&drop_t1).cloned().collect(),
            mod_routings: vec![
                routing(1, comp_thr(99)),                           // dangling
                routing(2, T::ModRoutingDepth { routing_id: 1 }),   // routing 1 の深さ → 連鎖
                routing(3, comp_thr(3)),
            ],
            ..Track::default()
        };
        let t2 = Track { id: 2, devices: vec![builtin(NativeKind::Comp, 5), builtin(NativeKind::Eq, 6)], ..Track::default() };
        let song_keep = vec![
            lane(T::NativeParam { device_id: 7, param: NativeParamId::BusComp(BusCompParam::Ratio) }),
            lane(T::PluginParam { device_id: 9, param_id: 1, legacy_device_index: None }),
            lane(T::MasterLimiter(MasterLimiterParam::Ceiling)),
            lane(T::SongTempo),
        ];
        let mut song = Song {
            tracks: vec![t1, t2],
            master_fx_chain: vec![builtin(NativeKind::BusComp, 7), builtin(NativeKind::ToneEq, 8), plug(9)],
            song_lanes: song_keep.clone(),
            mod_sources: vec![ModSource {
                id: 1,
                owner_track_id: 1,
                color: [1.0; 3],
                kind: ModSourceKind::default(),
                enabled: true,
            }],
            midi_bindings: vec![
                binding(BindingTarget::PluginParam { device_id: 12, param_id: 1, legacy_device_index: None, legacy_track: None }),
                binding(BindingTarget::PluginParam { device_id: 77, param_id: 1, legacy_device_index: None, legacy_track: None }),
                binding(BindingTarget::NativeParam { device_id: 3, param: NativeParamId::On(NativeKind::Comp) }),
                binding(BindingTarget::NativeParam { device_id: 99, param: NativeParamId::On(NativeKind::Comp) }),
                binding(BindingTarget::MasterLimiter(MasterLimiterParam::On)),
            ],
            ..Song::default()
        };
        assert!(song.prune_dangling_param_targets());
        let targets = |lanes: &[AutomationLane]| lanes.iter().map(|l| l.target.clone()).collect::<Vec<_>>();
        assert_eq!(targets(&song.tracks[0].automation_lanes), targets(&keep_t1));
        assert_eq!(song.tracks[0].mod_routings.iter().map(|r| r.id).collect::<Vec<_>>(), vec![3]);
        assert_eq!(targets(&song.song_lanes), targets(&song_keep));
        let bound: Vec<BindingTarget> = song.midi_bindings.iter().map(|b| b.target).collect();
        assert_eq!(bound.len(), 3, "{bound:?}");
        assert!(bound.iter().all(|b| !matches!(b, BindingTarget::PluginParam { device_id: 77, .. } | BindingTarget::NativeParam { device_id: 99, .. })));
        assert!(!song.prune_dangling_param_targets(), "2 回目は変化しない");
    }
}
