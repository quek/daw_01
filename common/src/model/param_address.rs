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
            | Self::SongTranspose
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
            | Self::SongTranspose
            | Self::ImageBuiltin(_)
            | Self::TextBuiltin(_)
            | Self::GroupTransform(_)
            | Self::ModSourceParam { .. }
            | Self::ModRoutingDepth { .. } => None,
        }
    }
}

/// [`Song::param_stores`] の置き場を **位置で** 持ったもの。off-RT で [`Song::param_store_at`] で解いて RT へ
/// 渡し、RT は [`Song::lanes_at`] で引く (buffer ごとに track を id で探さない)。同じ `Song` の snapshot と組で
/// 使うこと (track の並びが変われば位置も変わる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamStoreAt {
    /// `song_lanes` / `song_mod_routings` (master fx chain と song-wide param)。
    Song,
    /// `tracks[i]` の `automation_lanes` / `mod_routings`。
    Track(u32),
}

impl Song {
    /// `owner` (track id か `MASTER_TRACK_ID`) の lane / routing store。置き場規則の唯一の実装。
    /// 確保なし (RT 可)。`0` は解釈しない (legacy の 0 → master は `mod_source_owner` の責務)。
    #[must_use]
    pub fn param_stores(&self, owner: u32) -> Option<(&[AutomationLane], &[ModRouting])> {
        match self.param_store_at(owner)? {
            ParamStoreAt::Song => Some((&self.song_lanes, &self.song_mod_routings)),
            ParamStoreAt::Track(i) => self
                .tracks
                .get(i as usize)
                .map(|t| (t.automation_lanes.as_slice(), t.mod_routings.as_slice())),
        }
    }

    /// `owner` の置き場の位置 ([`Self::param_stores`] と同じ規則。track を id で探すので off-RT で解く)。
    #[must_use]
    pub fn param_store_at(&self, owner: u32) -> Option<ParamStoreAt> {
        if owner == MASTER_TRACK_ID {
            return Some(ParamStoreAt::Song);
        }
        self.tracks
            .iter()
            .position(|t| t.id == owner && owner != 0)
            .and_then(|i| u32::try_from(i).ok())
            .map(ParamStoreAt::Track)
    }

    /// 解決済みの置き場の lane (位置が外れていれば空)。確保なし (RT 可)。
    #[must_use]
    pub fn lanes_at(&self, at: ParamStoreAt) -> &[AutomationLane] {
        match at {
            ParamStoreAt::Song => &self.song_lanes,
            ParamStoreAt::Track(i) => self.tracks.get(i as usize).map_or(&[], |t| &t.automation_lanes),
        }
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
            | AutomationTarget::SongTimeSigNumerator
            | AutomationTarget::SongTranspose => Some(MASTER_TRACK_ID),
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

    /// dangling な参照を全部掃除する: 信号経路 ([`Self::prune_dangling_routes`]) → 帰属トラックの消えた
    /// モジュレーター ([`Self::prune_orphan_mod_sources`]) → パラメーターの束縛 ([`Self::prune_dangling_param_targets`])
    /// の順 (send / モジュレーターを消すと、その SendGain のレーン / 変調・そのソースの変調が dangling になる。
    /// 逆向きの依存は無い)。**冪等**。`enforce_edit_invariants` と `normalize_after_load` の一部。
    pub fn prune_dangling_refs(&mut self) -> bool {
        self.prune_dangling_routes() | self.prune_orphan_mod_sources() | self.prune_dangling_param_targets()
    }

    /// 帰属トラック (`ModSource::owner_track_id`) が消えたモジュレーターを外す。**冪等**。
    ///
    /// ラックはモジュレーターを帰属トラックの下にしか列挙しないので、残すとどの画面にも出ず削除できないまま、
    /// 生き残ったトラックの param を変調し続ける (LFO / Random / MSEG / Steps は曲位置の純関数、r.md #78)。
    /// `0` (legacy) と `MASTER_TRACK_ID` は master 帰属でトラック不在ではない。そのソースを使う変調と、その
    /// ツマミを指すレーン / 変調は後段の `prune_dangling_param_targets` が落とす。トラックを外す経路
    /// (削除 / グループ解除 / 末尾削除 / undo の差し替え) ごとに掃除を書かない。
    pub fn prune_orphan_mod_sources(&mut self) -> bool {
        let Song { tracks, mod_sources, .. } = self;
        let n = mod_sources.len();
        mod_sources.retain(|m| {
            m.owner_track_id == 0 || m.owner_track_id == MASTER_TRACK_ID || tracks.iter().any(|t| t.id == m.owner_track_id)
        });
        mod_sources.len() != n
    }

    /// 消えたトラック / chain を id で指す**信号経路**を掃除する。**冪等**。
    ///
    /// - aux 入力 (plugin / 内蔵 Comp・Bus Comp の SC) と envelope follower の tap: source が無ければ
    ///   `None` (入力なし。device / モジュレーター本体と設定は残す)
    /// - plugin の aux 出力 (パラアウト): 宛先トラックが無ければ `None`
    /// - send: 宛先トラックが無ければ削除 (その SendGain のレーン / 変調は `prune_dangling_param_targets` が落とす)
    ///
    /// 削除の口 (トラック削除 / chain 削除 / Parallel 解除) ごとに掃除を書かない — どの口で消えても
    /// SongDoc の `enforce_edit_invariants` が同じ undo step でここを通す。
    pub fn prune_dangling_routes(&mut self) -> bool {
        let tracks: HashSet<u32> = self.tracks.iter().map(|t| t.id).collect();
        let mut chains: HashSet<u64> = HashSet::new();
        for devices in self.tracks.iter().map(|t| t.devices.as_slice()).chain(std::iter::once(self.master_fx_chain.as_slice())) {
            for_each_chain(devices, &mut |_, c| {
                chains.insert(c.id);
            });
        }
        let resolves = |source: TapSource| match source {
            TapSource::Track(t) => tracks.contains(&t),
            TapSource::Chain(c) => chains.contains(&c),
        };
        let mut changed = false;
        let Song { tracks: owned, master_fx_chain, mod_sources, .. } = self;
        for devices in owned.iter_mut().map(|t| &mut t.devices).chain(std::iter::once(master_fx_chain)) {
            for_each_aux_slot_mut(devices, &mut |_, _, slot| {
                if slot.is_some_and(|r| !resolves(r.tap.source)) {
                    *slot = None;
                    changed = true;
                }
            });
            for_each_plugin_mut(devices, &mut |p| {
                for slot in &mut p.aux_outputs {
                    if slot.is_some_and(|r| !tracks.contains(&r.dest_track)) {
                        *slot = None;
                        changed = true;
                    }
                }
            });
        }
        for t in owned.iter_mut() {
            let n = t.sends.len();
            t.sends.retain(|s| tracks.contains(&s.dest_track_id));
            changed |= t.sends.len() != n;
        }
        for m in mod_sources.iter_mut() {
            if let Some(tap) = m.follower_tap_mut()
                && tap.is_some_and(|t| !resolves(t.source))
            {
                *tap = None;
                changed = true;
            }
        }
        changed
    }

    /// dangling な lane / routing / MIDI binding を**固定点まで**掃除する。**冪等** (2 回目は
    /// `false`)。[`Self::prune_dangling_refs`] の後段。
    ///
    /// 残す条件は `keep_target` の表 (id で束縛する target は「その id の node が実在し、
    /// その store の持ち主に居る」)。変調を 1 本消すとその深さを指す変調 / レーンが dangling に
    /// なるので、変化が無くなるまで回す。
    pub fn prune_dangling_param_targets(&mut self) -> bool {
        let mut changed_any = false;
        loop {
            let ctx = self.prune_ctx();
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

    /// 残す判定の表 (node / 生きている変調ソース・変調 / トラック / 各トラックの send)。
    fn prune_ctx(&self) -> PruneCtx {
        let mut nodes = HashMap::new();
        let owners = self
            .tracks
            .iter()
            .map(|t| (t.id, t.devices.as_slice()))
            .chain(std::iter::once((MASTER_TRACK_ID, self.master_fx_chain.as_slice())));
        for (owner, devices) in owners {
            collect_nodes(devices, owner, &mut nodes);
        }
        PruneCtx {
            nodes,
            live_sources: self.mod_sources.iter().map(|m| m.id).collect(),
            live_routings: self.all_mod_routings().map(|r| r.id).collect(),
            tracks: self.tracks.iter().map(|t| t.id).collect(),
            sends: self.tracks.iter().map(|t| (t.id, t.sends.iter().map(|s| s.id).collect())).collect(),
        }
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

struct PruneCtx {
    /// 全チェーンの node (plugin / native / Parallel / chain) → (持ち主, 種類)。
    nodes: HashMap<u64, (u32, NodeKind)>,
    live_sources: HashSet<u32>,
    live_routings: HashSet<u32>,
    tracks: HashSet<u32>,
    /// トラック id → そのトラックの send id。
    sends: HashMap<u32, Vec<u32>>,
}

impl PruneCtx {
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
                // その store の持ち主のトラックに、その id の send がある (master の store に send は無い)。
                B::SendGain { send_id, .. } => self.sends.get(&owner).is_some_and(|ids| ids.contains(send_id)),
                B::Volume | B::Pan | B::Mute => true,
            },
            AutomationTarget::MasterLimiter(_)
            | AutomationTarget::SongTempo
            | AutomationTarget::SongTimeSigNumerator
            | AutomationTarget::SongTranspose => owner == MASTER_TRACK_ID,
            AutomationTarget::ModSourceParam { source_id, .. } => self.live_sources.contains(source_id),
            AutomationTarget::ModRoutingDepth { routing_id } => self.live_routings.contains(routing_id),
            AutomationTarget::ImageBuiltin(_)
            | AutomationTarget::TextBuiltin(_)
            | AutomationTarget::GroupTransform(_) => true,
        }
    }

    /// MIDI binding を残すか (device は持ち主を問わず実在と種類で判定する)。トラックを指すものはトラックの実在。
    /// 列 (scene) を指すものは列の削除の口 (`delete_scenes`) が落とす。
    fn keep_binding(&self, target: &BindingTarget) -> bool {
        match target {
            BindingTarget::PluginParam { device_id, .. } => {
                matches!(self.nodes.get(device_id), Some((_, NodeKind::Plugin)))
            }
            BindingTarget::NativeParam { device_id, param } => {
                param.exists() && matches!(self.nodes.get(device_id), Some((_, NodeKind::Native(k))) if *k == param.kind())
            }
            BindingTarget::TrackVolume(track_id)
            | BindingTarget::TrackPan(track_id)
            | BindingTarget::LaunchCell { track_id, .. }
            | BindingTarget::StopLauncherRow { track_id }
            | BindingTarget::SwitchRowToArranger { track_id } => self.tracks.contains(track_id),
            BindingTarget::MasterLimiter(_)
            | BindingTarget::SongTempo
            | BindingTarget::SongTranspose
            | BindingTarget::LaunchScene { .. }
            | BindingTarget::StopAllLauncherRows
            | BindingTarget::SwitchAllToArranger => true,
        }
    }
}

/// 束縛先がいまの Song で解決するかを **編集の前に** 問う口 (A キーでレーンを積む前 / MIDI Learn の適用前 /
/// 消えた対象を指す session 状態の掃除)。規則は `prune_dangling_param_targets` と同じ
/// [`PruneCtx::keep_target`] / [`PruneCtx::keep_binding`] をそのまま使う (ここに複製しない) ので、true の target を
/// 積んだ編集は `enforce_edit_invariants` に同じ編集の中で消されない。**keep_* の意味を変えるときはここも追従する。**
impl Song {
    /// `owner` (track id か `MASTER_TRACK_ID`) の store に置く `target` が、実在する node / 住所を指すか。
    /// `owner` の store 自体が在るかは見ない (呼び出し側が `param_stores` で確かめる)。
    #[must_use]
    pub fn param_target_resolves(&self, target: &AutomationTarget, owner: u32) -> bool {
        self.prune_ctx().keep_target(target, owner)
    }

    /// MIDI binding の `target` が、実在する node / 住所を指すか (device は持ち主を問わず実在と種類)。
    #[must_use]
    pub fn binding_target_resolves(&self, target: &BindingTarget) -> bool {
        self.prune_ctx().keep_binding(target)
    }

    /// `owner` の store に置く「`source_id` のモジュレーターで `target` を変調する」routing が残るか
    /// (prune の routing の retain と同じ式: ソースが実在し、`target` がその store で解決する)。
    #[must_use]
    pub fn mod_routing_resolves(&self, target: &AutomationTarget, source_id: u32, owner: u32) -> bool {
        let ctx = self.prune_ctx();
        ctx.live_sources.contains(&source_id) && ctx.keep_target(target, owner)
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

    /// トラック 1 (chain 11 を持つ Parallel) / 2 / 3 と、それらを読む配線を全部持つ Song。
    /// トラック 3 の plugin 30 の aux 入力 = [トラック 2, chain 11]、aux 出力 = [トラック 2]、send = トラック 2 宛て、
    /// トラック 3 の組み込み Comp 32 の SC = トラック 2、MIDI binding はトラック 2 の音量 / パン / ランチャー。
    fn routed_song() -> Song {
        use T::TrackBuiltin as TB;
        use AutomationTarget as T;
        let mut p = Parallel::new();
        p.id = 10;
        p.chains[0].id = 11;
        let mut plugin = PluginInstance { id: 30, ..PluginInstance::new("p30".into(), PluginFormat::Clap) };
        plugin.aux_inputs = vec![
            Some(AuxInputRoute { tap: AudioTap::new(TapSource::Track(2), TapPoint::PostFader) }),
            Some(AuxInputRoute { tap: AudioTap::new(TapSource::Chain(11), TapPoint::PostFx) }),
        ];
        plugin.aux_outputs = vec![Some(AuxOutputRoute::to_track(2))];
        let mut comp = NativeDevice::new_builtin(NativeKind::Comp, 32);
        comp.aux_input = Some(AuxInputRoute { tap: AudioTap::new(TapSource::Track(2), TapPoint::PostFader) });
        let send = Send { id: 7, dest_track_id: 2, gain: 1.0, mode: SendMode::PostFader, enabled: true };
        let t3 = Track {
            id: 3,
            devices: vec![Device::Plugin(plugin), Device::Native(comp), builtin(NativeKind::Eq, 33)],
            sends: vec![send],
            automation_lanes: vec![lane(TB(TrackBuiltinParam::SendGain { send_id: 7, legacy_send_idx: None }))],
            mod_routings: vec![routing(1, TB(TrackBuiltinParam::SendGain { send_id: 7, legacy_send_idx: None }))],
            ..Track::default()
        };
        let t1 = Track {
            id: 1,
            devices: vec![Device::Parallel(p), builtin(NativeKind::Comp, 12), builtin(NativeKind::Eq, 13)],
            ..Track::default()
        };
        let t2 = Track { id: 2, devices: vec![builtin(NativeKind::Comp, 20), builtin(NativeKind::Eq, 21)], ..Track::default() };
        Song {
            tracks: vec![t1, t2, t3],
            master_fx_chain: vec![builtin(NativeKind::BusComp, 40), builtin(NativeKind::ToneEq, 41)],
            mod_sources: vec![ModSource { id: 1, owner_track_id: 3, color: [1.0; 3], kind: ModSourceKind::default(), enabled: true }],
            midi_bindings: vec![
                binding(BindingTarget::TrackVolume(2)),
                binding(BindingTarget::TrackPan(2)),
                binding(BindingTarget::LaunchCell { track_id: 2, scene_id: 1 }),
                binding(BindingTarget::StopLauncherRow { track_id: 2 }),
                binding(BindingTarget::SwitchRowToArranger { track_id: 2 }),
                binding(BindingTarget::TrackVolume(3)),
            ],
            ..Song::default()
        }
    }

    /// 消えたトラック / chain を指す信号経路 (aux 入力 / aux 出力 / send) と MIDI binding は、編集後の
    /// 不変条件の口が掃除する。aux 入力は「入力なし」、aux 出力は「宛先なし」、send は削除 (その SendGain の
    /// レーン / 変調も落ちる)、binding は削除。実在する参照は触らない。2 回目は変化しない。
    #[test]
    fn enforce_prunes_routes_and_bindings_to_removed_tracks_and_chains() {
        let mut song = routed_song();
        assert!(!song.enforce_edit_invariants(), "前提: 全部実在するので不動点");
        song.tracks.retain(|t| t.id != 2);
        if let Some(Device::Parallel(p)) = song.tracks[0].devices.first_mut() {
            p.chains.clear();
        }
        assert!(song.enforce_edit_invariants());
        let t3 = song.track_by_id(3).expect("t3");
        let Some(Device::Plugin(plugin)) = t3.devices.first() else { panic!("plugin") };
        assert_eq!(plugin.aux_inputs, vec![None, None], "消えたトラック / chain を読む aux 入力は入力なし");
        assert_eq!(plugin.aux_outputs, vec![None], "消えたトラック宛ての aux 出力は宛先なし");
        assert_eq!(song.native_by_id(32).and_then(|n| n.aux_input), None, "内蔵 Comp の SC も同じ");
        assert!(t3.sends.is_empty(), "消えたトラック宛ての send は消える");
        assert!(t3.automation_lanes.is_empty() && t3.mod_routings.is_empty(), "消えた send の SendGain も落ちる");
        let bound: Vec<BindingTarget> = song.midi_bindings.iter().map(|b| b.target).collect();
        assert_eq!(bound, vec![BindingTarget::TrackVolume(3)], "消えたトラックを指す binding だけ落ちる");
        assert!(!song.enforce_edit_invariants(), "2 回目は変化しない");
    }

    /// 帰属トラックが消えたモジュレーターは外れ、それを使う変調も落ちる。帰属が master (`MASTER_TRACK_ID`) /
    /// legacy (`0`) / 実在するトラックのモジュレーターは残る。
    #[test]
    fn enforce_prunes_modulators_whose_owner_track_is_gone() {
        let source = |id, owner_track_id| ModSource { id, owner_track_id, color: [1.0; 3], kind: ModSourceKind::default(), enabled: true };
        let mut song = routed_song();
        song.mod_sources = vec![source(1, 3), source(2, 2), source(3, 0), source(4, MASTER_TRACK_ID)];
        let volume = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
        song.tracks[2].mod_routings = vec![ModRouting { source_id: 2, ..routing(5, volume.clone()) }, routing(6, volume)];
        assert!(!song.enforce_edit_invariants(), "前提: 全部の帰属トラックが居るので不動点");
        song.tracks.retain(|t| t.id != 2);
        assert!(song.enforce_edit_invariants());
        assert_eq!(song.mod_sources.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 3, 4]);
        assert!(song.all_mod_routings().all(|r| r.source_id != 2), "外したモジュレーターの変調も落ちる");
        assert!(!song.enforce_edit_invariants(), "2 回目は変化しない");
    }

    /// 送り先の send が無い SendGain は、その store に send が無ければ残らない (master の store にも send は無い)。
    #[test]
    fn send_gain_without_its_send_is_dangling() {
        let gain = |send_id| AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_id, legacy_send_idx: None });
        let song = routed_song();
        assert!(song.param_target_resolves(&gain(7), 3));
        assert!(!song.param_target_resolves(&gain(8), 3), "無い send id");
        assert!(!song.param_target_resolves(&gain(7), 1), "別トラックの send");
        assert!(!song.param_target_resolves(&gain(7), MASTER_TRACK_ID), "master に send は無い");
    }
}
