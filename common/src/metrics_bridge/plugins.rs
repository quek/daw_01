//! per-plugin の `process()` 時間の **伸びる面** (`docs/plan_unbounded_tracks.md` §4)。
//!
//! 固定 shmem の [`super::MetricsBridge`] から外した理由: 同時に計測できる instance 数を起動時に決めると、
//! トラックを増やしてプラグインが増えたぶんだけ黙って計測が落ちる。書き手 daw_plugin_host の plugin-main が
//! instance を registry に入れるとき (off-RT) に枠を割り当て、足りなければ **別名で作り直す**。worker (RT) は
//! 割り当て済みの枠へ store するだけ。読み手 daw_gui は [`super::MetricsBridge::plugin_plane_id`] が
//! 変わったら開き直す。
//!
//! 名前 = [`plugin_plane_shmem_id`] (固定 shmem の id + 作成プロセスの pid + 世代)。世代は作成プロセス内の
//! 単調カウンタなので同じ名前は二度作られない (`crate::shmem` の命名契約)。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;

use crate::protocol::InstanceToken;
use crate::shmem::NamedShmem;

const PLANE_MAGIC: u32 = 0x4441_504D; // "DAPM"

/// 作り直すときの最小容量。
pub const MIN_PLUGIN_SLOTS: u32 = 64;

/// 面の shmem 名。`base` は固定 shmem (`MetricsBridge`) の id、`id` は
/// [`crate::audio_bridge::plane_id`] と同じ `pid << 32 | 世代`。
#[must_use]
pub fn plugin_plane_shmem_id(base: &str, id: u64) -> String {
    format!("{base}_plugins_{}_{}", id >> 32, id & u64::from(u32::MAX))
}

#[repr(C)]
struct Header {
    magic: u32,
    capacity: u32,
}

/// 1 instance ぶん。`token` = どの instance の枠か (`0` = 空き)、`us` = 直近の `process()` 時間 (μs)。
#[repr(C)]
struct Slot {
    token: AtomicU64,
    us: AtomicU32,
    _pad: u32,
}

/// per-plugin 計測の伸びる面 1 枚の handle。
pub struct PluginMetricsPlane {
    shmem: NamedShmem,
    id: u64,
    capacity: u32,
}

// SAFETY: 写像された領域は atomic だけを持ち (容量は作成時に 1 度だけ書く)、読み手は観測したどの値にも耐える。
unsafe impl Send for PluginMetricsPlane {}
unsafe impl Sync for PluginMetricsPlane {}

fn size_for(capacity: u32) -> Option<usize> {
    std::mem::size_of::<Slot>()
        .checked_mul(capacity as usize)?
        .checked_add(std::mem::size_of::<Header>().next_multiple_of(8))
}

impl PluginMetricsPlane {
    /// 書き手 (plugin-main): `capacity` 枠の面を新規に作る (全枠空き)。
    pub fn create(base: &str, id: u64, capacity: u32) -> Result<Self> {
        anyhow::ensure!(id != 0, "plane id 0 は「面なし」の印");
        let size = size_for(capacity).ok_or_else(|| anyhow::anyhow!("plugin metrics の容量が大きすぎる: {capacity}"))?;
        let shmem = NamedShmem::create(&plugin_plane_shmem_id(base, id), size)?;
        // SAFETY: 作ったばかりの `size` バイトの領域で、まだ誰とも共有していない。
        unsafe {
            std::ptr::write_bytes(shmem.as_ptr(), 0, size);
            let h = &mut *shmem.as_ptr().cast::<Header>();
            h.magic = PLANE_MAGIC;
            h.capacity = capacity;
        }
        Ok(Self { shmem, id, capacity })
    }

    /// 読み手: `id` の面を開く。印と容量を検証する。
    pub fn open(base: &str, id: u64) -> Result<Self> {
        anyhow::ensure!(id != 0, "plane id 0 は「面なし」の印");
        let name = plugin_plane_shmem_id(base, id);
        let shmem = NamedShmem::open(&name, std::mem::size_of::<Header>())?;
        // SAFETY: 写像は `Header` 以上の大きさ (open が検証済み)。
        let h = unsafe { &*shmem.as_ptr().cast::<Header>() };
        anyhow::ensure!(h.magic == PLANE_MAGIC, "{name} は plugin metrics plane ではない");
        let capacity = h.capacity;
        let size = size_for(capacity).ok_or_else(|| anyhow::anyhow!("{name} の容量が壊れている: {capacity}"))?;
        anyhow::ensure!(shmem.len() >= size, "{name} が容量より小さい: {} < {size}", shmem.len());
        Ok(Self { shmem, id, capacity })
    }

    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    fn slots(&self) -> &[Slot] {
        // SAFETY: create / open が `size_for(capacity)` 以上の写像を検証済み。要素は atomic だけ。
        unsafe {
            std::slice::from_raw_parts(
                self.shmem.as_ptr().add(std::mem::size_of::<Header>().next_multiple_of(8)).cast::<Slot>(),
                self.capacity as usize,
            )
        }
    }

    /// 書き手 (plugin-main、off-RT): 枠 `slot` を `token` の instance に割り当てる。`us` は `last_us`
    /// から始める (作り直しで前の面の値を引き継ぐ。新規は 0)。範囲外は何もしない。
    pub fn assign(&self, slot: u32, token: InstanceToken, last_us: u32) {
        if let Some(s) = self.slots().get(slot as usize) {
            s.us.store(last_us, Ordering::Release);
            s.token.store(token.0, Ordering::Release);
        }
    }

    /// 書き手 (plugin-main、off-RT、quiesce 後): 枠を空ける。
    pub fn release(&self, slot: u32) {
        if let Some(s) = self.slots().get(slot as usize) {
            s.token.store(0, Ordering::Release);
            s.us.store(0, Ordering::Release);
        }
    }

    /// worker (RT): 割り当て済みの枠へ直近の `process()` 時間を store する。範囲外は何もしない。
    pub fn set_us(&self, slot: u32, us: u32) {
        if let Some(s) = self.slots().get(slot as usize) {
            s.us.store(us, Ordering::Release);
        }
    }

    /// 枠 `slot` の直近値 (作り直しで引き継ぐ用)。
    #[must_use]
    pub fn us(&self, slot: u32) -> u32 {
        self.slots().get(slot as usize).map_or(0, |s| s.us.load(Ordering::Acquire))
    }

    /// 読み手 (GUI の poller): 使用中の枠を `(token, μs)` で `out` に積み直す。
    pub fn read(&self, out: &mut Vec<(InstanceToken, u32)>) {
        out.clear();
        for s in self.slots() {
            let token = s.token.load(Ordering::Acquire);
            if token != 0 {
                out.push((InstanceToken(token), s.us.load(Ordering::Acquire)));
            }
        }
    }
}

/// `slots` 枠を載せるのに作る面の容量 (2 冪、最小 [`MIN_PLUGIN_SLOTS`])。作り直しの回数を対数に抑える。
#[must_use]
pub fn plugin_plane_capacity(slots: u32) -> u32 {
    slots.max(MIN_PLUGIN_SLOTS).next_power_of_two()
}

/// 書き手 (plugin-main) の枠の帳簿。空き枠を再利用し、無ければ末尾に足す。
#[derive(Debug, Default)]
pub struct PluginSlotAllocator {
    used: Vec<bool>,
}

impl PluginSlotAllocator {
    /// 空き枠を取る (面の容量を超えるかは呼び側が [`plugin_plane_capacity`] で決める)。
    pub fn allocate(&mut self) -> u32 {
        let slot = match self.used.iter().position(|u| !u) {
            Some(i) => i,
            None => {
                self.used.push(false);
                self.used.len() - 1
            }
        };
        self.used[slot] = true;
        #[allow(clippy::cast_possible_truncation)]
        let slot = slot as u32;
        slot
    }

    /// 枠を空ける。
    pub fn release(&mut self, slot: u32) {
        if let Some(u) = self.used.get_mut(slot as usize) {
            *u = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(tag: &str) -> String {
        format!("daw01_test_pm_{tag}_{}", std::process::id())
    }

    /// 600 instance ぶん枠を割り当てられる (容量は 2 冪で伸びる)。空けた枠は再利用され、読み手は token で引く。
    #[test]
    fn 容量を超える_instance_も全部計測枠を持つ() {
        let base = base("grow");
        let mut alloc = PluginSlotAllocator::default();
        let mut plane = PluginMetricsPlane::create(&base, crate::audio_bridge::plane_id(1, 1), MIN_PLUGIN_SLOTS).unwrap();
        let mut generation = 1;
        let mut assigned: Vec<(u32, InstanceToken)> = Vec::new();
        for t in 1..=600u64 {
            let token = InstanceToken(t);
            let slot = alloc.allocate();
            if slot >= plane.capacity() {
                generation += 1;
                let cap = plugin_plane_capacity(slot + 1);
                let next = PluginMetricsPlane::create(&base, crate::audio_bridge::plane_id(1, generation), cap).unwrap();
                for &(s, tok) in &assigned {
                    next.assign(s, tok, plane.us(s));
                }
                plane = next;
            }
            plane.assign(slot, token, 0);
            plane.set_us(slot, u32::try_from(t).unwrap());
            assigned.push((slot, token));
        }
        assert_eq!(plane.capacity(), 1024);
        let reader = PluginMetricsPlane::open(&base, plane.id()).unwrap();
        let mut out = Vec::new();
        reader.read(&mut out);
        assert_eq!(out.len(), 600, "600 個とも読める");
        assert!(out.contains(&(InstanceToken(600), 600)));
        assert!(out.contains(&(InstanceToken(1), 1)), "作り直し前の値も引き継ぐ");

        let (slot_of_7, _) = assigned[6];
        plane.release(slot_of_7);
        alloc.release(slot_of_7);
        reader.read(&mut out);
        assert!(!out.iter().any(|(t, _)| *t == InstanceToken(7)), "空けた枠は読まれない");
        assert_eq!(alloc.allocate(), slot_of_7, "空いた枠を再利用する");
    }
}
