//! plugin host が `PluginParamList` で送ってくる param 表の、プロジェクトごとのキャッシュ
//! ([`super::ProjectIpc::plugin_params`])。
//!
//! 表が変わるたびに進む世代 ([`PluginParamTable::generation`]) を持つ。レーン名のように param 表から作る派生を
//! 世代キャッシュに載せるため (r.md #129)。書き換えはこの型のメソッドだけ (field は private) なので、世代の
//! 進め忘れは起きない。param id → 位置の索引も一緒に持ち、1 param を引くたびに表を線形探索しない
//! (1 プラグインで数万 param の実例がある)。

use std::collections::HashMap;

use common::protocol::PluginParamInfo;

#[derive(Debug, Default)]
pub struct PluginParamTable {
    by_device: HashMap<u64, DeviceParams>,
    generation: u64,
}

#[derive(Debug)]
struct DeviceParams {
    infos: Vec<PluginParamInfo>,
    /// param id → `infos` の位置。同じ id が複数あれば最初の 1 つ (表を前から探したときと同じ答え)。
    by_id: HashMap<u32, usize>,
}

impl PluginParamTable {
    /// `device_id` の param 表 (host の送った順)。
    pub fn get(&self, device_id: &u64) -> Option<&[PluginParamInfo]> {
        self.by_device.get(device_id).map(|d| d.infos.as_slice())
    }

    /// `device_id` の `param_id` の情報。
    pub fn info(&self, device_id: u64, param_id: u32) -> Option<&PluginParamInfo> {
        let device = self.by_device.get(&device_id)?;
        device.infos.get(*device.by_id.get(&param_id)?)
    }

    /// `device_id` の表を host が送ってきた表で置き換える。
    pub fn insert(&mut self, device_id: u64, infos: Vec<PluginParamInfo>) {
        let mut by_id = HashMap::with_capacity(infos.len());
        for (i, info) in infos.iter().enumerate() {
            by_id.entry(info.id).or_insert(i);
        }
        self.by_device.insert(device_id, DeviceParams { infos, by_id });
        self.generation += 1;
    }

    /// `device_id` の表を捨てる (device が消えた / unload した)。
    pub fn remove(&mut self, device_id: &u64) {
        if self.by_device.remove(device_id).is_some() {
            self.generation += 1;
        }
    }

    /// 全 device の表を捨てる (プロジェクトの差し替え)。
    pub fn clear(&mut self) {
        if !self.by_device.is_empty() {
            self.by_device.clear();
            self.generation += 1;
        }
    }

    /// 表が変わるたびに進む世代 (単調増加)。
    pub fn generation(&self) -> u64 {
        self.generation
    }
}
