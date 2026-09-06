//! Parallel (r.md #110, `docs/plan_parallel.md`): ネスト可能な並列 device chain。
//!
//! `Track.devices` / `Song.master_fx_chain` の要素は [`Device`] = plugin か Parallel。
//! Parallel は並列 [`ParallelChain`] の列で、各 chain がまた `Vec<Device>` を持つ (無限ネスト)。
//! plugin / parallel / chain の id は **1 つの id 空間** (`Song.ids.next_device_id`) で採番し、
//! IPC / automation / 選択 / AudioTap はすべてその id でアドレスする (不変条件 1)。
//! 位置 (chain 内 index) は表示順と挿入位置にしか使わない。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::*;

/// device chain の 1 要素。
///
/// serde: `Parallel` は externally tagged (`{"Parallel": {..}}`)、`Plugin` だけが untagged の
/// **fallback** (= 旧 `.daw` の plugin 配列がそのまま読める)。 全 variant untagged の
/// 「field 集合の pairwise 非交差」 には依存しない — 判別は `Parallel` タグの有無 1 点だけで、
/// variant を足すときはタグ付きにすればよい。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum Device {
    Parallel(Parallel),
    #[serde(untagged)] // arch-lint: allow-untagged (fallback variant 1 本、判別は Parallel タグ)
    Plugin(PluginInstance),
}

/// 並列 chain の container (Live: Audio Effect Rack / Bitwig: FX Layer / Reason: Parallel)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Parallel {
    /// 安定 id (`Song::alloc_device_id`、plugin と同じ空間)。`0` = 未採番 sentinel。
    #[serde(default)]
    pub id: u64,
    #[serde(default = "default_parallel_name")]
    pub name: String,
    pub chains: Vec<ParallelChain>,
    /// r.md #105 と同じ意味: `true` の間、Parallel 全体が素通し (中の device は dispatch されない)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bypassed: bool,
    /// 括弧行 (開始 / 終了) の色。 作った時点で周囲と別の色を自動で振る (Bitwig)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// 出力 trim (linear、`1.0` = unity、上限 [`MAX_TRACK_GAIN`])。 全 chain の和に掛ける。
    /// automation / 変調の対象 (`TrackBuiltinParam::ParallelOutGain`)。
    #[serde(default = "default_chain_gain")]
    pub out_gain: f32,
    /// gain match: 並列の和が入力より大きく (小さく) なるぶんを自動で戻す。 入力と出力の
    /// RMS (遅い窓) の比を出力に掛ける (engine `program.rs` の `update_gain_match`)。
    /// 帯域分割や Dry + Wet のように和がそのまま正しい使い方では **off** にする (既定 off)。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gain_match: bool,
}

/// Parallel の中の 1 本の並列 chain。Live の Chain List の 1 行 = Bitwig の layer 1 段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ParallelChain {
    /// 安定 id (plugin / parallel と同じ空間)。`AudioTap` / `ChainGain` automation が指す。
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// linear amp、`1.0` = unity、上限 [`MAX_TRACK_GAIN`]。
    #[serde(default = "default_chain_gain")]
    pub gain: f32,
    /// `-1.0..=1.0` (L..R)。
    #[serde(default)]
    pub pan: f32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub solo: bool,
    #[serde(default)]
    pub devices: Vec<Device>,
}

fn default_parallel_name() -> String {
    "Parallel".to_string()
}

fn default_chain_gain() -> f32 {
    1.0
}

impl Parallel {
    /// 新規 Parallel (chain 1 本 "Chain 1"、id は未採番 = 呼び出し側が `alloc_device_id` で埋める)。
    pub fn new() -> Self {
        Self {
            id: 0,
            name: default_parallel_name(),
            chains: vec![ParallelChain::new("Chain 1")],
            bypassed: false,
            color: None,
            out_gain: 1.0,
            gain_match: false,
        }
    }

    /// Ungroup (Live と同じ): 全 chain の device を chain 順に直列連結した列を返す。
    pub fn flatten(self) -> Vec<Device> {
        self.chains.into_iter().flat_map(|c| c.devices).collect()
    }

    /// chain 追加時の既定名 ("Chain N"、N = 既存最大番号 + 1)。
    pub fn next_chain_name(&self) -> String {
        let max = self
            .chains
            .iter()
            .filter_map(|c| c.name.strip_prefix("Chain ").and_then(|n| n.parse::<u32>().ok()))
            .max()
            .unwrap_or(0);
        format!("Chain {}", max + 1)
    }
}

impl Default for Parallel {
    fn default() -> Self {
        Self::new()
    }
}

impl ParallelChain {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: 0,
            name: name.into(),
            color: None,
            gain: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            devices: Vec::new(),
        }
    }
}

impl Device {
    /// plugin / parallel どちらでも安定 id。
    pub fn id(&self) -> u64 {
        match self {
            Device::Plugin(p) => p.id,
            Device::Parallel(r) => r.id,
        }
    }

    pub fn bypassed(&self) -> bool {
        match self {
            Device::Plugin(p) => p.bypassed,
            Device::Parallel(r) => r.bypassed,
        }
    }

    pub fn set_bypassed(&mut self, bypassed: bool) {
        match self {
            Device::Plugin(p) => p.bypassed = bypassed,
            Device::Parallel(r) => r.bypassed = bypassed,
        }
    }

    pub fn as_plugin(&self) -> Option<&PluginInstance> {
        match self {
            Device::Plugin(p) => Some(p),
            Device::Parallel(_) => None,
        }
    }

    pub fn as_plugin_mut(&mut self) -> Option<&mut PluginInstance> {
        match self {
            Device::Plugin(p) => Some(p),
            Device::Parallel(_) => None,
        }
    }

    pub fn as_parallel(&self) -> Option<&Parallel> {
        match self {
            Device::Parallel(r) => Some(r),
            Device::Plugin(_) => None,
        }
    }

    pub fn as_parallel_mut(&mut self) -> Option<&mut Parallel> {
        match self {
            Device::Parallel(r) => Some(r),
            Device::Plugin(_) => None,
        }
    }

    /// この device (Parallel なら中身全部) に routed aux 出力を持つ plugin が居るか
    /// (パラアウト `docs/plan_paraout.md` の split 判定)。
    pub fn routes_any_aux_output(&self) -> bool {
        plugins(std::slice::from_ref(self)).any(|p| p.aux_outputs.iter().any(Option::is_some))
    }
}

impl From<PluginInstance> for Device {
    fn from(p: PluginInstance) -> Self {
        Device::Plugin(p)
    }
}

impl From<Parallel> for Device {
    fn from(r: Parallel) -> Self {
        Device::Parallel(r)
    }
}

/// device chain 上の「挿入先」。`Track(id)` = その track の top-level chain、
/// `Chain(id)` = Parallel の中の chain。master は `Track(MASTER_TRACK_ID)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainRef {
    Track(u32),
    Chain(u64),
}

/// `devices` 以下の全 plugin を **pre-order (= 信号順)** で辿る iterator。Parallel の中は
/// chain 順・chain 内は device 順。RT では使わない (stack が `Vec`)。
pub fn plugins(devices: &[Device]) -> PluginIter<'_> {
    PluginIter {
        stack: vec![devices.iter()],
    }
}

pub struct PluginIter<'a> {
    stack: Vec<std::slice::Iter<'a, Device>>,
}

impl<'a> Iterator for PluginIter<'a> {
    type Item = &'a PluginInstance;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let top = self.stack.last_mut()?;
            match top.next() {
                None => {
                    self.stack.pop();
                }
                Some(Device::Plugin(p)) => return Some(p),
                Some(Device::Parallel(r)) => {
                    // 逆順に積むと pop 順が chain 順になる。
                    for c in r.chains.iter().rev() {
                        self.stack.push(c.devices.iter());
                    }
                }
            }
        }
    }
}

/// `devices` 以下の全 plugin を可変で訪問する (pre-order)。
pub fn for_each_plugin_mut(devices: &mut [Device], f: &mut impl FnMut(&mut PluginInstance)) {
    for d in devices {
        match d {
            Device::Plugin(p) => f(p),
            Device::Parallel(r) => {
                for c in &mut r.chains {
                    for_each_plugin_mut(&mut c.devices, f);
                }
            }
        }
    }
}

/// `devices` 以下の全 chain (`ParallelChain`) を訪問する (pre-order、Parallel ごとに chain 順)。
/// `f(parallel, chain)`。
pub fn for_each_chain<'a>(devices: &'a [Device], f: &mut impl FnMut(&'a Parallel, &'a ParallelChain)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                f(r, c);
                for_each_chain(&c.devices, f);
            }
        }
    }
}

/// [`for_each_parallel`] の可変版。
pub fn for_each_parallel_mut(devices: &mut [Device], f: &mut impl FnMut(&mut Parallel)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            f(r);
            for c in &mut r.chains {
                for_each_parallel_mut(&mut c.devices, f);
            }
        }
    }
}

/// [`for_each_chain`] の可変版 (chain だけ)。
pub fn for_each_chain_mut(devices: &mut [Device], f: &mut impl FnMut(&mut ParallelChain)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                f(c);
                for_each_chain_mut(&mut c.devices, f);
            }
        }
    }
}

/// `devices` 以下の全 Parallel を訪問する (pre-order)。
pub fn for_each_parallel<'a>(devices: &'a [Device], f: &mut impl FnMut(&'a Parallel)) {
    for d in devices {
        if let Device::Parallel(r) = d {
            f(r);
            for c in &r.chains {
                for_each_parallel(&c.devices, f);
            }
        }
    }
}

/// plugin / parallel / chain の **id を全部** 可変で訪問する (`Song::ensure_ids` の採番用)。
pub fn for_each_node_id_mut(devices: &mut [Device], f: &mut impl FnMut(&mut u64)) {
    for d in devices {
        match d {
            Device::Plugin(p) => f(&mut p.id),
            Device::Parallel(r) => {
                f(&mut r.id);
                for c in &mut r.chains {
                    f(&mut c.id);
                    for_each_node_id_mut(&mut c.devices, f);
                }
            }
        }
    }
}

/// `devices` 以下で id が `id` の device (plugin / parallel) を探す。
/// 戻り値は `(その device が居る chain, index)`。`root` は `devices` 自身の ChainRef。
pub fn find_device_in(devices: &[Device], root: ChainRef, id: u64) -> Option<(ChainRef, usize)> {
    for (i, d) in devices.iter().enumerate() {
        if d.id() == id {
            return Some((root, i));
        }
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if let Some(found) = find_device_in(&c.devices, ChainRef::Chain(c.id), id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `chain_id` の chain の `devices` を返す。
pub fn chain_devices_in(devices: &[Device], chain_id: u64) -> Option<&Vec<Device>> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if c.id == chain_id {
                    return Some(&c.devices);
                }
                if let Some(found) = chain_devices_in(&c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// [`chain_devices_in`] の可変版。
pub fn chain_devices_in_mut(devices: &mut [Device], chain_id: u64) -> Option<&mut Vec<Device>> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if c.id == chain_id {
                    return Some(&mut c.devices);
                }
                if let Some(found) = chain_devices_in_mut(&mut c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `chain_id` の chain を返す (親 Parallel と一緒に)。
pub fn chain_in(devices: &[Device], chain_id: u64) -> Option<(&Parallel, &ParallelChain)> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if c.id == chain_id {
                    return Some((r, c));
                }
                if let Some(found) = chain_in(&c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// [`chain_in`] の可変版 (chain だけ)。
pub fn chain_in_mut(devices: &mut [Device], chain_id: u64) -> Option<&mut ParallelChain> {
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if c.id == chain_id {
                    return Some(c);
                }
                if let Some(found) = chain_in_mut(&mut c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `id` の device を返す。
pub fn device_in(devices: &[Device], id: u64) -> Option<&Device> {
    for d in devices {
        if d.id() == id {
            return Some(d);
        }
        if let Device::Parallel(r) = d {
            for c in &r.chains {
                if let Some(found) = device_in(&c.devices, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// [`device_in`] の可変版。
pub fn device_in_mut(devices: &mut [Device], id: u64) -> Option<&mut Device> {
    for d in devices {
        if d.id() == id {
            return Some(d);
        }
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if let Some(found) = device_in_mut(&mut c.devices, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下で id が `id` の device を **抜き取る**。
pub fn remove_device_in(devices: &mut Vec<Device>, id: u64) -> Option<Device> {
    if let Some(i) = devices.iter().position(|d| d.id() == id) {
        return Some(devices.remove(i));
    }
    for d in devices {
        if let Device::Parallel(r) = d {
            for c in &mut r.chains {
                if let Some(found) = remove_device_in(&mut c.devices, id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// `devices` 以下の chain id `chain_id` を持つ chain を **抜き取る** (親 Parallel から)。
pub fn remove_chain_in(devices: &mut [Device], chain_id: u64) -> Option<ParallelChain> {
    for d in devices {
        if let Device::Parallel(r) = d {
            if let Some(i) = r.chains.iter().position(|c| c.id == chain_id) {
                return Some(r.chains.remove(i));
            }
            for c in &mut r.chains {
                if let Some(found) = remove_chain_in(&mut c.devices, chain_id) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// 呼び出し側が「この chain は `id` の内側か」を判定するための祖先判定:
/// `devices` 以下で `chain_id` の chain が **`ancestor_id` の device (Parallel) の中** にあるか。
pub fn chain_is_inside_device(devices: &[Device], ancestor_id: u64, chain_id: u64) -> bool {
    device_in(devices, ancestor_id)
        .and_then(|d| d.as_parallel())
        .is_some_and(|r| r.chains.iter().any(|c| c.id == chain_id || chain_devices_in(&c.devices, chain_id).is_some()))
}

/// Song 全体 (全 track + master) の device 走査。
impl Song {
    /// `r` が指す chain の device 列。
    pub fn chain_devices(&self, r: ChainRef) -> Option<&Vec<Device>> {
        match r {
            ChainRef::Track(MASTER_TRACK_ID) => Some(&self.master_fx_chain),
            ChainRef::Track(tid) => self.track_by_id(tid).map(|t| &t.devices),
            ChainRef::Chain(cid) => self
                .tracks
                .iter()
                .find_map(|t| chain_devices_in(&t.devices, cid))
                .or_else(|| chain_devices_in(&self.master_fx_chain, cid)),
        }
    }

    /// [`Self::chain_devices`] の可変版。
    pub fn chain_devices_mut(&mut self, r: ChainRef) -> Option<&mut Vec<Device>> {
        match r {
            ChainRef::Track(MASTER_TRACK_ID) => Some(&mut self.master_fx_chain),
            ChainRef::Track(tid) => self.track_by_id_mut(tid).map(|t| &mut t.devices),
            ChainRef::Chain(cid) => {
                // borrowck: track 側で見つかればそれ、無ければ master。
                let in_track = self
                    .tracks
                    .iter()
                    .position(|t| chain_devices_in(&t.devices, cid).is_some());
                match in_track {
                    Some(i) => chain_devices_in_mut(&mut self.tracks[i].devices, cid),
                    None => chain_devices_in_mut(&mut self.master_fx_chain, cid),
                }
            }
        }
    }

    /// `id` の device (plugin / parallel) が居る `(chain, index)`。
    pub fn find_device(&self, id: u64) -> Option<(ChainRef, usize)> {
        self.tracks
            .iter()
            .find_map(|t| find_device_in(&t.devices, ChainRef::Track(t.id), id))
            .or_else(|| find_device_in(&self.master_fx_chain, ChainRef::Track(MASTER_TRACK_ID), id))
    }

    pub fn device_by_id(&self, id: u64) -> Option<&Device> {
        self.tracks
            .iter()
            .find_map(|t| device_in(&t.devices, id))
            .or_else(|| device_in(&self.master_fx_chain, id))
    }

    pub fn device_by_id_mut(&mut self, id: u64) -> Option<&mut Device> {
        let in_track = self.tracks.iter().position(|t| device_in(&t.devices, id).is_some());
        match in_track {
            Some(i) => device_in_mut(&mut self.tracks[i].devices, id),
            None => device_in_mut(&mut self.master_fx_chain, id),
        }
    }

    pub fn plugin_by_id(&self, id: u64) -> Option<&PluginInstance> {
        self.device_by_id(id).and_then(Device::as_plugin)
    }

    pub fn plugin_by_id_mut(&mut self, id: u64) -> Option<&mut PluginInstance> {
        self.device_by_id_mut(id).and_then(Device::as_plugin_mut)
    }

    pub fn parallel_by_id(&self, id: u64) -> Option<&Parallel> {
        self.device_by_id(id).and_then(Device::as_parallel)
    }

    pub fn parallel_by_id_mut(&mut self, id: u64) -> Option<&mut Parallel> {
        self.device_by_id_mut(id).and_then(Device::as_parallel_mut)
    }

    /// `chain_id` の chain (親 Parallel と一緒に)。
    pub fn chain_by_id(&self, chain_id: u64) -> Option<(&Parallel, &ParallelChain)> {
        self.tracks
            .iter()
            .find_map(|t| chain_in(&t.devices, chain_id))
            .or_else(|| chain_in(&self.master_fx_chain, chain_id))
    }

    pub fn chain_by_id_mut(&mut self, chain_id: u64) -> Option<&mut ParallelChain> {
        let in_track = self.tracks.iter().position(|t| chain_in(&t.devices, chain_id).is_some());
        match in_track {
            Some(i) => chain_in_mut(&mut self.tracks[i].devices, chain_id),
            None => chain_in_mut(&mut self.master_fx_chain, chain_id),
        }
    }

    /// `r` が属する track id (master は `MASTER_TRACK_ID`)。dangling は `None`。
    pub fn chain_owner_track(&self, r: ChainRef) -> Option<u32> {
        match r {
            ChainRef::Track(tid) => {
                (tid == MASTER_TRACK_ID || self.track_by_id(tid).is_some()).then_some(tid)
            }
            ChainRef::Chain(cid) => self
                .tracks
                .iter()
                .find(|t| chain_in(&t.devices, cid).is_some())
                .map(|t| t.id)
                .or_else(|| chain_in(&self.master_fx_chain, cid).map(|_| MASTER_TRACK_ID)),
        }
    }

    /// `id` の device が属する track id (master は `MASTER_TRACK_ID`)。
    pub fn device_owner_track(&self, id: u64) -> Option<u32> {
        self.tracks
            .iter()
            .find(|t| device_in(&t.devices, id).is_some())
            .map(|t| t.id)
            .or_else(|| device_in(&self.master_fx_chain, id).map(|_| MASTER_TRACK_ID))
    }

    /// `id` の device を抜き取る (どの chain に居ても)。
    pub fn remove_device(&mut self, id: u64) -> Option<Device> {
        for t in &mut self.tracks {
            if let Some(d) = remove_device_in(&mut t.devices, id) {
                return Some(d);
            }
        }
        remove_device_in(&mut self.master_fx_chain, id)
    }

    /// `chain_id` の chain を親 Parallel から抜き取る。
    pub fn remove_chain(&mut self, chain_id: u64) -> Option<ParallelChain> {
        for t in &mut self.tracks {
            if let Some(c) = remove_chain_in(&mut t.devices, chain_id) {
                return Some(c);
            }
        }
        remove_chain_in(&mut self.master_fx_chain, chain_id)
    }

    /// `r` の `index` に `device` を挿す (末尾超過は末尾)。chain が無ければ `false`。
    pub fn insert_device(&mut self, r: ChainRef, index: usize, device: Device) -> bool {
        let Some(chain) = self.chain_devices_mut(r) else {
            return false;
        };
        let at = index.min(chain.len());
        chain.insert(at, device);
        true
    }

    /// 全 track + master の全 plugin (pre-order)。
    pub fn all_plugins(&self) -> impl Iterator<Item = &PluginInstance> {
        self.tracks
            .iter()
            .flat_map(|t| plugins(&t.devices))
            .chain(plugins(&self.master_fx_chain))
    }

    /// 全 track + master の全 plugin を可変で訪問。
    pub fn for_each_plugin_mut(&mut self, f: &mut impl FnMut(&mut PluginInstance)) {
        for t in &mut self.tracks {
            for_each_plugin_mut(&mut t.devices, f);
        }
        for_each_plugin_mut(&mut self.master_fx_chain, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_format::PluginFormat;

    fn plug(id: u64) -> Device {
        Device::Plugin(PluginInstance {
            id,
            ..PluginInstance::new(format!("p{id}"), PluginFormat::Clap)
        })
    }

    fn parallel(id: u64, chains: Vec<(u64, Vec<Device>)>) -> Device {
        Device::Parallel(Parallel {
            id,
            name: "Parallel".into(),
            color: None,
            out_gain: 1.0,
            gain_match: false,
            chains: chains
                .into_iter()
                .map(|(cid, devices)| ParallelChain {
                    id: cid,
                    devices,
                    ..ParallelChain::new("c")
                })
                .collect(),
            bypassed: false,
        })
    }

    #[test]
    fn plugins_walks_pre_order_through_nested_parallels() {
        let devices = vec![
            plug(1),
            parallel(10, vec![(11, vec![plug(2), parallel(20, vec![(21, vec![plug(3)])])]), (12, vec![plug(4)])]),
            plug(5),
        ];
        let ids: Vec<u64> = plugins(&devices).map(|p| p.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn find_and_remove_addresses_nested_chain_by_id() {
        let mut devices = vec![
            plug(1),
            parallel(10, vec![(11, vec![plug(2)]), (12, vec![parallel(20, vec![(21, vec![plug(3)])])])]),
        ];
        assert_eq!(find_device_in(&devices, ChainRef::Track(7), 3), Some((ChainRef::Chain(21), 0)));
        assert_eq!(find_device_in(&devices, ChainRef::Track(7), 20), Some((ChainRef::Chain(12), 0)));
        assert_eq!(find_device_in(&devices, ChainRef::Track(7), 10), Some((ChainRef::Track(7), 1)));
        assert!(chain_is_inside_device(&devices, 10, 21));
        assert!(!chain_is_inside_device(&devices, 20, 11));
        let taken = remove_device_in(&mut devices, 3).expect("nested plugin removed");
        assert_eq!(taken.id(), 3);
        assert!(find_device_in(&devices, ChainRef::Track(7), 3).is_none());
        let chain = remove_chain_in(&mut devices, 11).expect("chain removed");
        assert_eq!(chain.devices.len(), 1);
    }

    #[test]
    fn device_json_reads_legacy_plugin_array_and_parallel() {
        // 旧 .daw: plugin object の配列。
        let legacy = r#"[{"plugin_id":"x","format":"Clap"}]"#;
        let v: Vec<Device> = serde_json::from_str(legacy).unwrap();
        assert!(matches!(&v[0], Device::Plugin(p) if p.plugin_id == "x"));
        // Parallel 入りの往復。
        let devices = vec![plug(1), parallel(10, vec![(11, vec![plug(2)])])];
        let json = serde_json::to_string(&devices).unwrap();
        let back: Vec<Device> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, devices);
        // bincode 往復 (wire)。
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&devices, cfg).unwrap();
        let (decoded, _): (Vec<Device>, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(plugins(&decoded).map(|p| p.id).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn flatten_concatenates_chains_in_order() {
        let r = Parallel {
            id: 1,
            name: "R".into(),
            chains: vec![
                ParallelChain { id: 2, devices: vec![plug(5), plug(6)], ..ParallelChain::new("a") },
                ParallelChain { id: 3, devices: vec![plug(7)], ..ParallelChain::new("b") },
            ],
            bypassed: false,
            color: None,
            out_gain: 1.0,
            gain_match: false,
        };
        let flat: Vec<u64> = r.flatten().iter().map(Device::id).collect();
        assert_eq!(flat, vec![5, 6, 7]);
    }
}
