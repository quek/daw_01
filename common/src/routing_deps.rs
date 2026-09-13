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

    /// `track_ids` のトラック上の aux 配線のうち、Structural で循環するものを 1 本ずつ判定して落とす
    /// (貼り付け / トラックを跨ぐ運搬で持ち込んだ配線用)。戻り値 = 1 本でも落としたか。
    pub fn drop_cyclic_aux_routes(&mut self, track_ids: &[u32]) -> bool {
        let mut changed = false;
        for &owner in track_ids {
            let Some(devices) = self.fx_chain_by_track_id(owner) else { continue };
            let mut slots: Vec<(u64, u8)> = Vec::new();
            crate::model::for_each_aux_input(devices, &mut |id, port, _| slots.push((id, port)));
            for (id, port) in slots {
                let Some(route) = self.device_by_id(id).and_then(|d| d.aux_input(port)).copied() else {
                    continue;
                };
                let Some(producer) = self.tap_producer(route.tap.source) else { continue };
                if producer == owner {
                    continue;
                }
                let Some(slot) = self.device_by_id_mut(id).and_then(|d| d.aux_input_slot_mut(port)) else {
                    continue;
                };
                *slot = None;
                if TrackDeps::build(self, EdgeScope::Structural).would_cycle(owner, producer) {
                    changed = true;
                } else if let Some(slot) = self.device_by_id_mut(id).and_then(|d| d.aux_input_slot_mut(port)) {
                    *slot = Some(route);
                }
            }
        }
        changed
    }
}
