//! device / chain の id → 置き場 (トラックか master の device 列から辿る位置の道)。

use crate::model::{Device, NativeDevice, Parallel, ParallelChain, ParamStoreAt, Song};

/// master の device 列 (`Song::master_fx_chain`) を指す持ち主。
const MASTER: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeEntry {
    id: u64,
    /// `tracks` の位置、または [`MASTER`]。
    owner: u32,
    /// [`NodeIndex::steps`] の範囲。`[device, chain, device, chain, …]` の位置 (device で終われば device、chain で
    /// 終われば chain)。道の置き場は node ごとに一意なので、`start` はその node 1 つを指す。
    start: u32,
    len: u32,
    /// chain なら親 Parallel の `start`。device は未使用。
    parent: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct NodeIndex {
    /// device を id 順に。同じ id が複数あれば `Song::device_by_id` が先に見つける方 (トラック順 → master、行きがけ順)。
    devices: Vec<NodeEntry>,
    /// 内蔵 device だけを `(owner, id)` 順に。同じ持ち主に同じ id が複数あれば `model::native_in` (その持ち主の device 列を
    /// 行きがけ順に Native だけ見る) が先に見つける方。
    natives: Vec<NodeEntry>,
    /// chain を `(parent, id)` 順に。同じ Parallel に同じ id が複数あれば並びの先頭。
    chains: Vec<NodeEntry>,
    steps: Vec<u32>,
}

/// 辿った先。
enum Found<'a> {
    Device(&'a Device),
    Chain(&'a Parallel, &'a ParallelChain),
}

fn position(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

impl NodeIndex {
    pub(super) fn build(song: &Song) -> Self {
        let mut index = Self::default();
        let mut path = Vec::new();
        for (t, track) in song.tracks.iter().enumerate() {
            index.visit(&track.devices, position(t), &mut path);
        }
        index.visit(&song.master_fx_chain, MASTER, &mut path);
        // 安定ソートなので、同じ鍵の中は行きがけ順のまま → 先頭だけ残す。
        index.devices.sort_by_key(|e| e.id);
        index.devices.dedup_by_key(|e| e.id);
        index.natives.sort_by_key(|e| (e.owner, e.id));
        index.natives.dedup_by_key(|e| (e.owner, e.id));
        index.chains.sort_by_key(|e| (e.parent, e.id));
        index.chains.dedup_by_key(|e| (e.parent, e.id));
        index
    }

    fn visit(&mut self, devices: &[Device], owner: u32, path: &mut Vec<u32>) {
        for (d, device) in devices.iter().enumerate() {
            path.push(position(d));
            let entry = self.record(device.id(), owner, path, u32::MAX);
            self.devices.push(entry);
            match device {
                Device::Native(_) => self.natives.push(entry),
                Device::Parallel(p) => {
                    for (c, chain) in p.chains.iter().enumerate() {
                        path.push(position(c));
                        let chain_entry = self.record(chain.id, owner, path, entry.start);
                        self.chains.push(chain_entry);
                        self.visit(&chain.devices, owner, path);
                        path.pop();
                    }
                }
                Device::Plugin(_) => {}
            }
            path.pop();
        }
    }

    fn record(&mut self, id: u64, owner: u32, path: &[u32], parent: u32) -> NodeEntry {
        let start = position(self.steps.len());
        self.steps.extend_from_slice(path);
        NodeEntry { id, owner, start, len: position(path.len()), parent }
    }

    /// `e` の道を辿る (道の先の id が違えば無いものとする)。
    fn walk<'a>(&self, song: &'a Song, e: NodeEntry) -> Option<Found<'a>> {
        let steps = self.steps.get(e.start as usize..(e.start + e.len) as usize)?;
        let mut devices: &[Device] =
            if e.owner == MASTER { &song.master_fx_chain } else { &song.tracks.get(e.owner as usize)?.devices };
        let mut k = 0;
        loop {
            let device = devices.get(*steps.get(k)? as usize)?;
            if k + 1 == steps.len() {
                return (device.id() == e.id).then_some(Found::Device(device));
            }
            let parallel = device.as_parallel()?;
            let chain = parallel.chains.get(*steps.get(k + 1)? as usize)?;
            if k + 2 == steps.len() {
                return (chain.id == e.id).then_some(Found::Chain(parallel, chain));
            }
            devices = &chain.devices;
            k += 2;
        }
    }

    fn device_entry(&self, id: u64) -> Option<NodeEntry> {
        Some(self.devices[self.devices.binary_search_by_key(&id, |e| e.id).ok()?])
    }

    /// `Song::device_by_id` と同じ device。
    pub(super) fn device<'a>(&self, song: &'a Song, id: u64) -> Option<&'a Device> {
        match self.walk(song, self.device_entry(id)?)? {
            Found::Device(d) => Some(d),
            Found::Chain(..) => None,
        }
    }

    /// `Song::parallel_by_id(parallel_id)` の Parallel の中で `chains.iter().find(|c| c.id == chain_id)` と同じ chain。
    pub(super) fn chain_in<'a>(&self, song: &'a Song, parallel_id: u64, chain_id: u64) -> Option<&'a ParallelChain> {
        let parent = self.device_entry(parallel_id)?.start;
        let i = self.chains.binary_search_by_key(&(parent, chain_id), |e| (e.parent, e.id)).ok()?;
        match self.walk(song, self.chains[i])? {
            Found::Chain(p, c) => (p.id == parallel_id).then_some(c),
            Found::Device(_) => None,
        }
    }

    /// 置き場 `owner` の device 列の中の内蔵 device (`model::native_in(devices, id)` と同じもの)。
    pub(super) fn native_in<'a>(&self, song: &'a Song, owner: ParamStoreAt, id: u64) -> Option<&'a NativeDevice> {
        let owner = match owner {
            ParamStoreAt::Song => MASTER,
            ParamStoreAt::Track(i) => i,
        };
        let i = self.natives.binary_search_by_key(&(owner, id), |e| (e.owner, e.id)).ok()?;
        match self.walk(song, self.natives[i])? {
            Found::Device(d) => d.as_native(),
            Found::Chain(..) => None,
        }
    }
}
