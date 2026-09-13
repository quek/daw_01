//! トラック間の配線の依存 (children / サイドチェイン / send) と、その循環の判定
//! (`docs/plan_rack_native_devices.md` §5.9)。
//!
//! 依存辺の定義はここが唯一の実装。engine の実行順 (`graph/compile/deps.rs`) は
//! [`EdgeScope::Active`]、Song 側のガード (SC の配線 / send の追加 / 貼り付けで持ち込んだ配線) は
//! [`EdgeScope::Structural`] で同じ辺を数える。循環した graph は engine が
//! `GraphError::Cycle` で空の schedule にする (= master が無音) ので、編集の口で拒否する。
//!
//! 非 RT (確保する)。wire に載らない。

use std::collections::HashMap;

use crate::model::{
    AudioTap, AuxInputRoute, AutomationLane, ChainRef, Device, ModRouting, Song, TapPoint, TapSource,
    for_each_chain,
};

/// どの配線を辺に数えるか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeScope {
    /// いま処理されうる consumer だけ (plugin は `!bypassed`、native は `can_activate`)。engine の実行順。
    Active,
    /// 配線がある consumer は全部 (bypass 中の配線を後で ON にすると循環しうるので、ガードはこちら)。
    Structural,
}

/// 依存 graph が循環している。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DependencyCycle;

/// トラックの依存 graph。node は `song.tracks` の並び (song-track index)。
#[derive(Debug, Clone)]
pub struct TrackDeps {
    ids: Vec<u32>,
    index: HashMap<u32, usize>,
    /// `deps[i]` = track i が依存する (先に処理される必要がある) track の index。
    /// 並びは children → サイドチェイン source → send source (DFS の訪問順 = 実行順を決める)。
    deps: Vec<Vec<usize>>,
}

impl TrackDeps {
    /// 辺: children → group / SC consumer → source track (`TapSource::Chain` はその chain を持つ
    /// track、同じ track と master の chain は辺にしない) / send source → dest。
    #[must_use]
    pub fn build(song: &Song, scope: EdgeScope) -> Self {
        let ids: Vec<u32> = song.tracks.iter().map(|t| t.id).collect();
        let index: HashMap<u32, usize> =
            ids.iter().enumerate().filter(|(_, id)| **id != 0).map(|(i, id)| (*id, i)).collect();
        let mut chain_owner: HashMap<u64, usize> = HashMap::new();
        for (i, t) in song.tracks.iter().enumerate() {
            for_each_chain(&t.devices, &mut |_, c| {
                chain_owner.insert(c.id, i);
            });
        }
        let mut deps: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
        // children (song 順)。
        for (j, t) in song.tracks.iter().enumerate() {
            if let Some(pid) = t.parent_group_id
                && let Some(&g) = index.get(&pid)
            {
                deps[g].push(j);
            }
        }
        // サイドチェイン。
        for (i, t) in song.tracks.iter().enumerate() {
            for c in aux_consumers(&t.devices, &t.automation_lanes, &t.mod_routings) {
                if scope == EdgeScope::Active && c.inactive {
                    continue;
                }
                for route in c.routes.iter().flatten() {
                    let src = match route.tap.source {
                        TapSource::Track(id) => index.get(&id).copied(),
                        TapSource::Chain(cid) => chain_owner.get(&cid).copied(),
                    };
                    if let Some(s) = src
                        && s != i
                    {
                        deps[i].push(s);
                    }
                }
            }
        }
        // send (source の song 順)。
        for (src, t) in song.tracks.iter().enumerate() {
            for s in &t.sends {
                if let Some(&dest) = index.get(&s.dest_track_id) {
                    deps[dest].push(src);
                }
            }
        }
        Self { ids, index, deps }
    }

    /// 依存の post-order (= 実行順、song-track index)。循環していれば `Err`。
    /// 開始 node は index 順、辺は `deps` の並び順で辿る (3 色 DFS)。
    pub fn dependency_order(&self) -> Result<Vec<u32>, DependencyCycle> {
        let n = self.ids.len();
        let mut state = vec![0u8; n]; // 0 = 未訪問、1 = 経路上、2 = 完了
        let mut order: Vec<u32> = Vec::with_capacity(n);
        for start in 0..n {
            if state[start] != 0 {
                continue;
            }
            let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
            state[start] = 1;
            while let Some(top) = stack.last_mut() {
                let (node, edge_i) = *top;
                let Some(&target) = self.deps[node].get(edge_i) else {
                    state[node] = 2;
                    order.push(node as u32);
                    stack.pop();
                    continue;
                };
                top.1 += 1;
                match state[target] {
                    0 => {
                        state[target] = 1;
                        stack.push((target, 0));
                    }
                    1 => return Err(DependencyCycle),
                    _ => {}
                }
            }
        }
        Ok(order)
    }

    /// track `consumer` が track `producer` に依存する辺を足すと循環するか (track id で指定)。
    /// `consumer == producer` か、`producer` が `consumer` に推移的に依存していれば true。
    /// どちらかが track でなければ (master / 不在) false。
    #[must_use]
    pub fn would_cycle(&self, consumer: u32, producer: u32) -> bool {
        if consumer == producer {
            return true;
        }
        let (Some(&c), Some(&p)) = (self.index.get(&consumer), self.index.get(&producer)) else {
            return false;
        };
        let mut seen = vec![false; self.ids.len()];
        let mut stack = vec![p];
        while let Some(node) = stack.pop() {
            if node == c {
                return true;
            }
            if std::mem::replace(&mut seen[node], true) {
                continue;
            }
            stack.extend(self.deps[node].iter().copied());
        }
        false
    }
}

/// サイドチェインを読む consumer 1 個 (plugin の aux 入力 / native の SC)。
#[derive(Debug, Clone, Copy)]
pub struct AuxConsumer<'a> {
    pub device_id: u64,
    /// 処理されない (plugin: bypassed / native: `!can_activate`)。
    pub inactive: bool,
    /// audio in + out を持つ (input delay の対象)。
    pub audio_io: bool,
    /// port ごとの配線 (native は `std::slice::from_ref(&aux_input)`)。
    pub routes: &'a [Option<AuxInputRoute>],
    pub native: bool,
    /// この consumer を含む最上位 device の index (engine の pass 判定)。
    pub top_index: u32,
}

/// `devices` 上の配線済み consumer を信号順 (pre-order) に辿る iterator。非 RT。
pub struct AuxConsumerIter<'a> {
    lanes: &'a [AutomationLane],
    routings: &'a [ModRouting],
    top: std::iter::Enumerate<std::slice::Iter<'a, Device>>,
    nested: Vec<std::slice::Iter<'a, Device>>,
    top_index: u32,
}

/// `devices` (1 トラックの最上位チェーン) の consumer。`lanes` / `routings` はその持ち主の store
/// (native の `can_activate` に使う)。
#[must_use]
pub fn aux_consumers<'a>(
    devices: &'a [Device],
    lanes: &'a [AutomationLane],
    routings: &'a [ModRouting],
) -> AuxConsumerIter<'a> {
    AuxConsumerIter { lanes, routings, top: devices.iter().enumerate(), nested: Vec::new(), top_index: 0 }
}

impl<'a> Iterator for AuxConsumerIter<'a> {
    type Item = AuxConsumer<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let d = if let Some(it) = self.nested.last_mut() {
                let Some(d) = it.next() else {
                    self.nested.pop();
                    continue;
                };
                d
            } else {
                let (i, d) = self.top.next()?;
                self.top_index = i as u32;
                d
            };
            match d {
                Device::Parallel(r) => {
                    // 逆順に積むと pop 順が chain 順になる。
                    for c in r.chains.iter().rev() {
                        self.nested.push(c.devices.iter());
                    }
                }
                Device::Plugin(p) if p.aux_inputs.iter().any(Option::is_some) => {
                    return Some(AuxConsumer {
                        device_id: p.id,
                        inactive: p.bypassed,
                        audio_io: p.ports.has_audio_input && p.ports.has_audio_output,
                        routes: &p.aux_inputs,
                        native: false,
                        top_index: self.top_index,
                    });
                }
                Device::Native(n) if n.sidechain_input().is_some() => {
                    return Some(AuxConsumer {
                        device_id: n.id,
                        inactive: !n.can_activate(self.lanes, self.routings),
                        audio_io: true,
                        routes: std::slice::from_ref(&n.aux_input),
                        native: true,
                        top_index: self.top_index,
                    });
                }
                Device::Plugin(_) | Device::Native(_) => {}
            }
        }
    }
}

impl Song {
    /// 全トラック + master の consumer (`(持ち主の track id か MASTER_TRACK_ID, consumer)`)。非 RT。
    pub fn all_aux_consumers(&self) -> impl Iterator<Item = (u32, AuxConsumer<'_>)> {
        self.tracks
            .iter()
            .flat_map(|t| {
                aux_consumers(&t.devices, &t.automation_lanes, &t.mod_routings).map(move |c| (t.id, c))
            })
            .chain(
                aux_consumers(&self.master_fx_chain, &self.song_lanes, &self.song_mod_routings)
                    .map(|c| (crate::model::MASTER_TRACK_ID, c)),
            )
    }

    /// `src` から `dest` への send を足してよいか (Structural で循環しない)。
    #[must_use]
    pub fn can_add_send(&self, src: u32, dest: u32) -> bool {
        !TrackDeps::build(self, EdgeScope::Structural).would_cycle(dest, src)
    }

    /// `source` を読む tap の持ち主 track (chain source はその chain を持つ track)。
    fn tap_producer(&self, source: TapSource) -> Option<u32> {
        match source {
            TapSource::Track(t) => Some(t),
            TapSource::Chain(c) => self.chain_owner_track(ChainRef::Chain(c)),
        }
    }

    /// device `device_id` の aux 入力 `port` を `source` へ配線する (plugin と native 共通の唯一の口)。
    ///
    /// - 自トラックを source にできるのは Pre-FX (device チェーンの入力) だけ (他は feedback)。
    /// - tap 点は既存の配線から引き継ぐ。
    /// - Structural で循環する配線は拒否して false。
    ///
    /// 戻り値 = 実際に変わったか。
    pub fn set_aux_input(&mut self, device_id: u64, port: u8, source: Option<TapSource>) -> bool {
        let Some(owner) = self.device_owner_track(device_id) else {
            return false;
        };
        if let Some(src) = source
            && let Some(producer) = self.tap_producer(src)
            && producer != owner
            && TrackDeps::build(self, EdgeScope::Structural).would_cycle(owner, producer)
        {
            return false;
        }
        let Some(dev) = self.device_by_id_mut(device_id) else {
            return false;
        };
        let current = dev.aux_input(port).copied();
        let mut tap_point = current.map(|r| r.tap.tap_point).unwrap_or_default();
        if source == Some(TapSource::Track(owner)) {
            tap_point = TapPoint::PreFx;
        }
        let next = source.map(|s| AuxInputRoute { tap: AudioTap::new(s, tap_point) });
        if next == current {
            return false;
        }
        let Some(slot) = dev.aux_input_slot_mut(port) else {
            return false;
        };
        *slot = next;
        true
    }

    /// 編集が持ち込んだ aux 配線のうち、Structural で循環するものを 1 本ずつ判定して落とす
    /// (device の貼り付け / コピー / 運搬、トラックの貼り付けの後に呼ぶ)。
    ///
    /// `brought` はその編集で置いた node の id 全部 (Parallel なら中身と chain も)。判定するのは次の 2 種だけで、
    /// 無関係な既存の配線 (読み込んだファイルに元からある循環を含む) は触らない。
    /// 1. `brought` の device 自身の配線 (持ち込んだもの。先に判定する)
    /// 2. `brought` の chain を source に読む他の device の配線 (運んだ chain の持ち主が変わって辺が変わる)
    ///
    /// 戻り値 = 落とした本数。
    pub fn drop_cyclic_aux_routes(&mut self, brought: &[u64]) -> usize {
        let mut own: Vec<(u32, u64, u8)> = Vec::new();
        let mut reading: Vec<(u32, u64, u8)> = Vec::new();
        for (owner, c) in self.all_aux_consumers() {
            for (port, route) in c.routes.iter().enumerate() {
                let (Some(route), Ok(port)) = (route, u8::try_from(port)) else { continue };
                if brought.contains(&c.device_id) {
                    own.push((owner, c.device_id, port));
                } else if matches!(route.tap.source, TapSource::Chain(chain) if brought.contains(&chain)) {
                    reading.push((owner, c.device_id, port));
                }
            }
        }
        own.into_iter().chain(reading).filter(|&(owner, id, port)| self.drop_aux_route_if_cyclic(owner, id, port)).count()
    }

    /// `owner` 上の device `id` の aux 入力 `port` を外した graph で、その配線が循環を作るなら外したままにする。
    /// 自トラックを読む配線 (Pre-FX) は辺にならないので判定しない。戻り値 = 外したか。
    fn drop_aux_route_if_cyclic(&mut self, owner: u32, id: u64, port: u8) -> bool {
        let Some(route) = self.device_by_id(id).and_then(|d| d.aux_input(port)).copied() else {
            return false;
        };
        let Some(producer) = self.tap_producer(route.tap.source) else { return false };
        if producer == owner {
            return false;
        }
        let Some(slot) = self.device_by_id_mut(id).and_then(|d| d.aux_input_slot_mut(port)) else {
            return false;
        };
        *slot = None;
        if TrackDeps::build(self, EdgeScope::Structural).would_cycle(owner, producer) {
            return true;
        }
        if let Some(slot) = self.device_by_id_mut(id).and_then(|d| d.aux_input_slot_mut(port)) {
            *slot = Some(route);
        }
        false
    }

    /// トラック群の親を `parent` (None = top-level) にし、`anchor_after` の直後 (None = 先頭、見つからなければ
    /// 末尾) へ並べ替える。**親の付け替え (children 辺) の唯一の口** — アレンジのヘッダ drop と
    /// `SetTrackParent` が通る (SC の [`Self::set_aux_input`]、send の [`Self::can_add_send`] と対)。
    ///
    /// 親が変わるトラック t ごとに、書き換える前の graph で `would_cycle(parent, t)` を判定し、1 本でも
    /// 循環すれば何も書かずに `Err`。足す辺はすべて `parent` から出るので、新しい循環は `parent` を途中に
    /// 含まない t → … → parent の道を持ち、その道は書き換える前の graph にもある (= この判定で過不足ない)。
    /// 自分の子孫を親にするのも同じ判定に入る。読み込んだファイルに元からある循環は拒否の理由にしない
    /// (親が変わらない並べ替えは判定しない)。
    ///
    /// 実在しない id は無視し、`parent` が実在しなければ何もしない。戻り値 = 並びか親が実際に変わったか。
    pub fn move_tracks(
        &mut self,
        track_ids: &[u32],
        parent: Option<u32>,
        anchor_after: Option<u32>,
    ) -> Result<bool, DependencyCycle> {
        if let Some(p) = parent {
            if self.track_by_id(p).is_none() {
                return Ok(false);
            }
            let deps = TrackDeps::build(self, EdgeScope::Structural);
            let cyclic = track_ids
                .iter()
                .copied()
                .filter(|&t| self.track_by_id(t).is_some_and(|track| track.parent_group_id != parent))
                .any(|t| deps.would_cycle(p, t));
            if cyclic {
                return Err(DependencyCycle);
            }
        }
        let before: Vec<(u32, Option<u32>)> = self.tracks.iter().map(|t| (t.id, t.parent_group_id)).collect();
        let mut moved: Vec<crate::model::Track> = Vec::with_capacity(track_ids.len());
        for id in track_ids {
            if let Some(pos) = self.tracks.iter().position(|t| t.id == *id) {
                moved.push(self.tracks.remove(pos));
            }
        }
        if moved.is_empty() {
            return Ok(false);
        }
        for t in &mut moved {
            t.parent_group_id = parent;
        }
        let at = anchor_after.map_or(0, |after| {
            self.tracks.iter().position(|t| t.id == after).map_or(self.tracks.len(), |i| i + 1)
        });
        self.tracks.splice(at..at, moved);
        Ok(!self.tracks.iter().map(|t| (t.id, t.parent_group_id)).eq(before))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NativeDevice, NativeKind, Parallel, PluginInstance, Send, SendMode, Track};
    use crate::plugin_format::PluginFormat;

    fn sc_plugin(id: u64, source: TapSource, bypassed: bool) -> Device {
        let mut p = PluginInstance::new(format!("p{id}"), PluginFormat::Clap);
        p.id = id;
        p.bypassed = bypassed;
        p.aux_inputs = vec![Some(AuxInputRoute { tap: AudioTap::new(source, TapPoint::PostFader) })];
        Device::Plugin(p)
    }

    fn comp(id: u64) -> Device {
        Device::Native(NativeDevice::new_builtin(NativeKind::Comp, id))
    }

    /// G (group) ← A (child)、B は独立。
    fn song(a_devices: Vec<Device>) -> Song {
        let mut chain_holder = Parallel::new();
        chain_holder.id = 50;
        chain_holder.chains[0].id = 51;
        let mut a_devices = a_devices;
        a_devices.push(Device::Parallel(chain_holder));
        Song {
            tracks: vec![
                Track { id: 1, name: "G".into(), devices: vec![comp(10)], ..Track::default() },
                Track { id: 2, name: "A".into(), parent_group_id: Some(1), devices: a_devices, ..Track::default() },
                Track { id: 3, name: "B".into(), devices: vec![comp(30)], ..Track::default() },
            ],
            ..Song::default()
        }
    }

    /// F-C12: 循環の検出 / bypass 中の配線は Structural だけ / 自分の chain は辺にしない /
    /// `set_aux_input` の拒否と自トラック PreFx 固定 / `can_add_send`。
    #[test]
    fn track_deps_detect_cycles_through_children_sidechain_and_sends() {
        // 子 A の Comp の SC に親 G を選ぶと循環する (拒否、値は変わらない)。
        let mut s = song(vec![comp(20)]);
        let deps = TrackDeps::build(&s, EdgeScope::Structural);
        assert!(deps.would_cycle(2, 1), "A が G を読むと G→A→G");
        assert!(!deps.would_cycle(1, 3) && !deps.would_cycle(2, 3));
        assert!(!s.set_aux_input(20, 0, Some(TapSource::Track(1))));
        assert_eq!(s.native_by_id(20).unwrap().aux_input, None);
        assert!(s.set_aux_input(20, 0, Some(TapSource::Track(3))), "独立トラックは配線できる");
        assert!(s.set_aux_input(20, 0, Some(TapSource::Track(2))), "自トラック");
        assert_eq!(s.native_by_id(20).unwrap().aux_input.unwrap().tap.tap_point, TapPoint::PreFx, "自トラックは PreFx 固定");
        assert!(!s.set_aux_input(20, 0, Some(TapSource::Track(2))), "同じ配線は変化なし");
        assert!(TrackDeps::build(&s, EdgeScope::Active).dependency_order().is_ok());

        // bypass 中の plugin の配線 (A が G を読む) は Structural だけが数える。
        let s = song(vec![sc_plugin(21, TapSource::Track(1), true)]);
        assert!(TrackDeps::build(&s, EdgeScope::Active).dependency_order().is_ok());
        assert_eq!(TrackDeps::build(&s, EdgeScope::Structural).dependency_order(), Err(DependencyCycle));
        let mut s2 = s.clone();
        assert_eq!(s2.drop_cyclic_aux_routes(&[99]), 0, "持ち込んでいない配線は判定しない");
        assert_eq!(s2.drop_cyclic_aux_routes(&[21]), 1, "持ち込んだ配線は判定して落とす");
        assert!(TrackDeps::build(&s2, EdgeScope::Structural).dependency_order().is_ok());

        // 自分の chain を source にする tap は辺にしない (前 buffer の snapshot = 循環ではない)。
        let s = song(vec![sc_plugin(22, TapSource::Chain(51), false)]);
        let order = TrackDeps::build(&s, EdgeScope::Active).dependency_order().expect("自己 chain は循環しない");
        assert_eq!(order.len(), 3);
        let a = order.iter().position(|&i| i == 1).unwrap();
        let g = order.iter().position(|&i| i == 0).unwrap();
        assert!(a < g, "子が親より先: {order:?}");

        // send: G → A は A が G に依存し、G は A (子) に依存するので循環。B → A は可。
        let s = song(vec![]);
        assert!(!s.can_add_send(1, 2));
        assert!(s.can_add_send(3, 2));
        let mut looped = s.clone();
        looped.tracks[0].sends.push(Send { id: 1, dest_track_id: 2, gain: 1.0, mode: SendMode::PostFader, enabled: true });
        assert_eq!(TrackDeps::build(&looped, EdgeScope::Active).dependency_order(), Err(DependencyCycle));
    }

    /// 運んだ Parallel の chain を他トラックが読んでいると、chain の持ち主が変わって辺が変わるので判定に入る。
    /// 同じ device の別 port の無関係な配線は触らない。
    #[test]
    fn drop_cyclic_aux_routes_judges_routes_reading_carried_chains() {
        // B (G の子) の plugin が A の Parallel の chain 51 を port 0 で、独立トラック 4 を port 1 で読む。
        let mut s = song(vec![]);
        s.tracks[2].parent_group_id = Some(1);
        s.tracks.push(Track { id: 4, name: "C".into(), ..Track::default() });
        let mut reader = sc_plugin(40, TapSource::Chain(51), false);
        if let Device::Plugin(p) = &mut reader {
            p.aux_inputs.push(Some(AuxInputRoute { tap: AudioTap::new(TapSource::Track(4), TapPoint::PostFader) }));
        }
        s.tracks[2].devices.push(reader);
        // Parallel を G へ運ぶと、B が G を読む辺と G←B (子) で循環する。
        let parallel = s.remove_device(50).expect("parallel");
        s.tracks[0].devices.push(parallel);
        assert_eq!(s.drop_cyclic_aux_routes(&[50, 51]), 1);
        let p = s.plugin_by_id(40).expect("reader");
        assert_eq!(p.aux_inputs[0], None, "運んだ chain を読む配線は落ちる");
        assert!(p.aux_inputs[1].is_some(), "無関係な配線は残る");
    }

    /// `move_tracks`: 付け替えで足す children 辺が SC / send / 親子の鎖と循環するなら何も書かずに拒否する。
    /// 親が変わらない並べ替えと、元からある循環に無関係な付け替えは通る。
    #[test]
    fn move_tracks_rejects_only_new_dependency_cycles() {
        // B の Comp が G を読む (G は B に依存しないので配線できる) → B を G の子にすると G←B←G。
        let mut s = song(vec![]);
        assert!(s.set_aux_input(30, 0, Some(TapSource::Track(1))));
        let before = s.clone();
        assert_eq!(s.move_tracks(&[3], Some(1), Some(2)), Err(DependencyCycle));
        assert_eq!(s, before, "拒否したら何も書かない");
        // G → B の send も同じ。
        let mut s = song(vec![]);
        s.tracks[0].sends.push(Send { id: 1, dest_track_id: 3, gain: 1.0, mode: SendMode::PostFader, enabled: true });
        assert_eq!(s.move_tracks(&[3], Some(1), Some(2)), Err(DependencyCycle));
        // 自分の子孫を親にする。
        let mut s = song(vec![]);
        assert_eq!(s.move_tracks(&[1], Some(2), None), Err(DependencyCycle));
        assert_eq!(s.move_tracks(&[1], Some(1), None), Err(DependencyCycle), "自分自身");

        // 循環しない付け替え + 並べ替え。
        let ids = |s: &Song| s.tracks.iter().map(|t| (t.id, t.parent_group_id)).collect::<Vec<_>>();
        assert_eq!(s.move_tracks(&[3], Some(1), Some(2)), Ok(true));
        assert_eq!(ids(&s), [(1, None), (2, Some(1)), (3, Some(1))]);
        assert_eq!(s.move_tracks(&[3], Some(1), Some(2)), Ok(false), "同じ位置と親は変化なし");
        assert_eq!(s.move_tracks(&[3], None, None), Ok(true));
        assert_eq!(ids(&s), [(3, None), (1, None), (2, Some(1))], "None は先頭");
        assert_eq!(s.move_tracks(&[3], Some(99), None), Ok(false), "実在しない親");
        assert_eq!(s.move_tracks(&[99], None, None), Ok(false), "実在しない id");

        // 元からある循環 (A の bypass 中の plugin が親 G を読む) は、無関係な編集を拒否する理由にしない。
        let mut s = song(vec![sc_plugin(21, TapSource::Track(1), true)]);
        assert_eq!(s.move_tracks(&[2], Some(1), None), Ok(true), "親が変わらない並べ替え");
        assert_eq!(s.move_tracks(&[3], Some(1), Some(2)), Ok(true), "循環に無関係な付け替え");
    }
}
