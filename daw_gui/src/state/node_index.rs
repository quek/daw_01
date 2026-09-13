//! Song の device node (plugin / native / Parallel と chain) を **id → 木の中の位置** で引く索引 (r.md #129)。
//!
//! 所有者は [`SongDoc`](super::SongDoc)。id 構造 ([`common::model::StructureWatch`] が観測するトラックの並びと
//! node の木の形) が変わった世代 ([`SongDoc::structure_epoch`](super::SongDoc::structure_epoch)) でだけ作り直す。
//! 構造が同じ間は位置が変わらないので、引きは位置をたどるだけで曲全体の木を走査しない。名前や値は位置の先の
//! Song から読むので、値だけの編集 (つまみのドラッグ / 改名) では作り直さない。
//!
//! 同じ id が 2 つあったとき (不変条件違反) に指すのは `Song::device_by_id` / `Song::chain_by_id` と同じ
//! 最初の 1 つ (トラック順 → master、各チェーンは pre-order)。

use std::collections::HashMap;

use common::model::{Device, MASTER_TRACK_ID, Parallel, ParallelChain, Song};

/// node の置き場。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Store {
    /// `Song::tracks` の位置。
    Track(usize),
    Master,
}

/// 木の中の位置。道は `steps[start..start + len]` で、置き場のチェーンから
/// `[device 位置, chain 位置, device 位置, …]` と下る (device は奇数長、chain は chain 位置で終わる偶数長)。
#[derive(Debug, Clone, Copy)]
struct NodeLoc {
    store: Store,
    /// 置き場のトラック id (`MASTER_TRACK_ID` = master)。
    owner: u32,
    start: usize,
    len: usize,
}

#[derive(Debug, Default)]
pub struct NodeIndex {
    devices: HashMap<u64, NodeLoc>,
    chains: HashMap<u64, NodeLoc>,
    /// 全 node の道を並べた置き場 (node ごとに確保しない)。
    steps: Vec<usize>,
}

/// 索引の位置の先が別の node だった (索引が Song より古い = 作り直しの漏れ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleIndex;

/// 引きの結果: 索引に無ければ `Ok(None)`、たどった先が別の node なら `Err(StaleIndex)`。
pub type Lookup<T> = Result<Option<T>, StaleIndex>;

impl NodeIndex {
    /// `song` の木から作り直す。
    pub fn rebuild(&mut self, song: &Song) {
        self.devices.clear();
        self.chains.clear();
        self.steps.clear();
        let mut path = Vec::new();
        for (i, t) in song.tracks.iter().enumerate() {
            self.index_chain(Store::Track(i), t.id, &t.devices, &mut path);
        }
        self.index_chain(Store::Master, MASTER_TRACK_ID, &song.master_fx_chain, &mut path);
    }

    fn index_chain(&mut self, store: Store, owner: u32, devices: &[Device], path: &mut Vec<usize>) {
        for (di, d) in devices.iter().enumerate() {
            path.push(di);
            let loc = self.push_loc(store, owner, path);
            self.devices.entry(d.id()).or_insert(loc);
            if let Device::Parallel(p) = d {
                for (ci, c) in p.chains.iter().enumerate() {
                    path.push(ci);
                    let loc = self.push_loc(store, owner, path);
                    self.chains.entry(c.id).or_insert(loc);
                    self.index_chain(store, owner, &c.devices, path);
                    path.pop();
                }
            }
            path.pop();
        }
    }

    fn push_loc(&mut self, store: Store, owner: u32, path: &[usize]) -> NodeLoc {
        let start = self.steps.len();
        self.steps.extend_from_slice(path);
        NodeLoc { store, owner, start, len: path.len() }
    }

    /// `id` の device と置き場のトラック id を `song` の中にたどる。
    pub fn device<'s>(&self, song: &'s Song, id: u64) -> Lookup<(&'s Device, u32)> {
        let Some(loc) = self.devices.get(&id) else { return Ok(None) };
        let path = &self.steps[loc.start..loc.start + loc.len];
        let (last, chain_steps) = path.split_last().ok_or(StaleIndex)?;
        let mut devices = store_devices(song, loc.store).ok_or(StaleIndex)?;
        for pair in chain_steps.chunks_exact(2) {
            devices = &chain_at(devices, pair[0], pair[1]).ok_or(StaleIndex)?.1.devices;
        }
        match devices.get(*last) {
            Some(d) if d.id() == id => Ok(Some((d, loc.owner))),
            _ => Err(StaleIndex),
        }
    }

    /// `id` の chain を親 Parallel・置き場のトラック id と一緒に `song` の中にたどる。
    pub fn chain<'s>(&self, song: &'s Song, id: u64) -> Lookup<(&'s Parallel, &'s ParallelChain, u32)> {
        let Some(loc) = self.chains.get(&id) else { return Ok(None) };
        let path = &self.steps[loc.start..loc.start + loc.len];
        let mut devices = store_devices(song, loc.store).ok_or(StaleIndex)?;
        let mut found = None;
        for pair in path.chunks_exact(2) {
            let (parallel, chain) = chain_at(devices, pair[0], pair[1]).ok_or(StaleIndex)?;
            devices = &chain.devices;
            found = Some((parallel, chain));
        }
        match found {
            Some((parallel, chain)) if chain.id == id => Ok(Some((parallel, chain, loc.owner))),
            _ => Err(StaleIndex),
        }
    }
}

fn store_devices(song: &Song, store: Store) -> Option<&[Device]> {
    match store {
        Store::Track(i) => song.tracks.get(i).map(|t| t.devices.as_slice()),
        Store::Master => Some(&song.master_fx_chain),
    }
}

fn chain_at(devices: &[Device], device: usize, chain: usize) -> Option<(&Parallel, &ParallelChain)> {
    let parallel = devices.get(device)?.as_parallel()?;
    Some((parallel, parallel.chains.get(chain)?))
}
