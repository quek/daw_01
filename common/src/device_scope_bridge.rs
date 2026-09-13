//! device 単位のサンプルリング (r.md #129 Q14: EQ Par の背後に出す「その EQ を通った後の音」の
//! スペクトラム、`docs/plan_rack_native_devices.md` §11.2)。
//!
//! `scope_bridge` (master 出力) と同じ流儀で daw_gui (親) が `create`、daw_audio が `open` する。
//! 違いは slot が [`MAX_DEVICE_SCOPES`] 個あり、各 slot に「どの project のどの device か」の
//! **見出し**が付くこと。見出しを書き換えるたびに世代 (`header_generation`) を 2 進め
//! (書き換え中は奇数)、読み手は「見出し → サンプル → 見出し」の順に読んで世代が変わっていたら
//! 破棄する — 前の device のフレームを新しい device のものとして返さない。
//!
//! 書き手は daw_audio の RT (slot ごとに 1 op)、読み手は daw_gui のテレメトリポーラ 1 本。
//! RT 側は事前確保済みリングへの atomic store だけ (確保・ロック・I/O なし)。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use anyhow::Result;

use crate::protocol::ProjectKey;
use crate::scope_bridge::ReadOutcome;
use crate::shmem::NamedShmem;

/// 同時に描画しうる EQ Par の上限。EQ Par 1 枚 ≥ 約 260px なので縦 3840px の画面でも 15 枚まで。
pub const MAX_DEVICE_SCOPES: usize = 16;

/// 1 slot のリングのフレーム数 (2 の冪)。192kHz × ポーラ省電力間隔 250ms = 48000 < 65536。
/// 1 slot 約 512KB、全体 約 8.4MB。
pub const DEVICE_SCOPE_FRAMES: usize = 1 << 16;

const DEVICE_SCOPE_MASK: u64 = (DEVICE_SCOPE_FRAMES as u64) - 1;

/// 1 device ぶんのリング。
#[repr(C)]
pub struct DeviceScopeSlot {
    /// 見出し: `ProjectKey.0`。
    project: AtomicU64,
    /// 見出し: device id。`0` = 空き。
    device_id: AtomicU64,
    /// 見出しを書き換えるたびに +2 (書き換え中は奇数)。
    header_generation: AtomicU64,
    /// 累積書き込みフレーム数 (monotonic)。
    write_frames: AtomicU64,
    /// インターリーブ `[L, R]` を `f32::to_bits` で保持するリング本体。
    samples: [[AtomicU32; 2]; DEVICE_SCOPE_FRAMES],
}

#[repr(C)]
pub struct DeviceScopeBridge {
    /// writer が publish する実サンプルレート (Hz)。0 = 未 publish。
    sample_rate: AtomicU32,
    /// 後続の 8 バイト境界を保つためのパディング。
    _pad: AtomicU32,
    slots: [DeviceScopeSlot; MAX_DEVICE_SCOPES],
}

impl DeviceScopeBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// 共有メモリ領域の所有ハンドル。
pub struct DeviceScopeBridgeHandle {
    shmem: NamedShmem,
}

impl DeviceScopeBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::create(os_id, DeviceScopeBridge::SIZE)?;
        // ゼロ初期化: 全 slot が空き (device_id 0)・未書き込みから始まる。
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, DeviceScopeBridge::SIZE) };
        Ok(Self { shmem })
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::open(os_id, DeviceScopeBridge::SIZE)?;
        Ok(Self { shmem })
    }

    fn bridge(&self) -> &DeviceScopeBridge {
        // SAFETY: マッピングは少なくとも `SIZE` バイト (create/open が検証)、`MapViewOfFile` の
        // ポインタは 64KiB 境界なので `AtomicU64` の 8 バイトアラインを満たす。全フィールドが
        // atomic = どんなビット列も有効なので、プロセスをまたぐ並行アクセスも健全。
        unsafe { &*(self.shmem.as_ptr() as *const DeviceScopeBridge) }
    }

    /// daw_audio が起動時に 1 度だけ publish する実サンプルレート。
    pub fn set_sample_rate(&self, sr: u32) {
        self.bridge().sample_rate.store(sr, Ordering::Release);
    }

    pub fn sample_rate(&self) -> u32 {
        self.bridge().sample_rate.load(Ordering::Acquire)
    }

    /// **RT**。slot `k` の見出しを `(project, device_id)` に書き換える (見出しの書き手は scope project の
    /// render だけ)。同じ見出しなら何もしない。`k` が範囲外なら何もしない。
    pub fn set_slot(&self, k: usize, project: ProjectKey, device_id: u64) {
        let Some(slot) = self.bridge().slots.get(k) else { return };
        if slot.project.load(Ordering::Relaxed) == project.0 && slot.device_id.load(Ordering::Relaxed) == device_id {
            return;
        }
        let g = slot.header_generation.load(Ordering::Relaxed);
        // 奇数 = 書き換え中。以降の store がこれより前に見えないよう fence で仕切る。
        slot.header_generation.store(g.wrapping_add(1) | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        slot.project.store(project.0, Ordering::Relaxed);
        slot.device_id.store(device_id, Ordering::Relaxed);
        slot.header_generation.store((g | 1).wrapping_add(1), Ordering::Release);
    }

    /// **RT**。slot `k` のリングへ 1 ブロック書く (slot ごとの書き手は 1 op)。短い方の長さに合わせる。
    pub fn write_block(&self, k: usize, l: &[f32], r: &[f32]) {
        let Some(slot) = self.bridge().slots.get(k) else { return };
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        let base = slot.write_frames.load(Ordering::Relaxed);
        for i in 0..n {
            let s = &slot.samples[((base + i as u64) & DEVICE_SCOPE_MASK) as usize];
            s[0].store(l[i].to_bits(), Ordering::Relaxed);
            s[1].store(r[i].to_bits(), Ordering::Relaxed);
        }
        slot.write_frames.store(base + n as u64, Ordering::Release);
    }
}

// 全フィールドが lock-free atomic で、読み手はどんな観測値も許容する。
unsafe impl Send for DeviceScopeBridgeHandle {}
unsafe impl Sync for DeviceScopeBridgeHandle {}

/// 単一の読み手が持つ slot ごとのカーソル `(見出しの世代, 読んだ位置)`。
pub struct DeviceScopeReader {
    cursors: [(u64, u64); MAX_DEVICE_SCOPES],
}

impl Default for DeviceScopeReader {
    fn default() -> Self {
        // 世代は偶数しか安定しないので、奇数の初期値は「まだ見出しを見ていない」印になる。
        Self { cursors: [(u64::MAX, 0); MAX_DEVICE_SCOPES] }
    }
}

impl DeviceScopeReader {
    /// slot `k` の前回以降のフレームを `out` へ push する (`out` は clear しない)。
    ///
    /// 見出し → サンプル → 見出しの順に読み、世代が変わっていたら今回積んだぶんを捨てて
    /// カーソルを張り直す (前の device のフレームを返さない)。見出しが変わった初回もカーソルを
    /// 今の書き込み位置に合わせるだけで 0 フレーム。空き slot / 書き換え中は `None`。
    pub fn read(
        &mut self,
        h: &DeviceScopeBridgeHandle,
        k: usize,
        out: &mut Vec<[f32; 2]>,
    ) -> Option<(ProjectKey, u64, ReadOutcome)> {
        let slot = h.bridge().slots.get(k)?;
        let cursor = self.cursors.get_mut(k)?;
        let g0 = slot.header_generation.load(Ordering::Acquire);
        if g0 & 1 != 0 {
            return None;
        }
        let project = ProjectKey(slot.project.load(Ordering::Acquire));
        let device_id = slot.device_id.load(Ordering::Acquire);
        let write = slot.write_frames.load(Ordering::Acquire);
        if device_id == 0 {
            *cursor = (g0, write);
            return None;
        }
        if cursor.0 != g0 || write < cursor.1 {
            *cursor = (g0, write);
            return Some((project, device_id, ReadOutcome { frames: 0, dropped: 0 }));
        }
        let mark = out.len();
        let oldest = write.saturating_sub(DEVICE_SCOPE_FRAMES as u64);
        let cap = crate::scope_bridge::max_read_frames(h.sample_rate()).min(DEVICE_SCOPE_FRAMES) as u64;
        let mut start = cursor.1.max(oldest);
        let mut dropped = start - cursor.1;
        if write - start > cap {
            dropped += (write - start) - cap;
            start = write - cap;
        }
        for i in start..write {
            let s = &slot.samples[(i & DEVICE_SCOPE_MASK) as usize];
            out.push([f32::from_bits(s[0].load(Ordering::Relaxed)), f32::from_bits(s[1].load(Ordering::Relaxed))]);
        }
        fence(Ordering::Acquire);
        if slot.header_generation.load(Ordering::Relaxed) != g0 {
            out.truncate(mark);
            *cursor = (u64::MAX, 0);
            return None;
        }
        *cursor = (g0, write);
        Some((project, device_id, ReadOutcome { frames: (write - start) as usize, dropped }))
    }
}

pub fn device_scope_shmem_id(parent_pid: u32) -> String {
    format!("daw_01_device_scope_{parent_pid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> DeviceScopeBridgeHandle {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let i = N.fetch_add(1, Ordering::Relaxed);
        DeviceScopeBridgeHandle::create(&format!("daw_01_device_scope_test_{}_{i}", std::process::id())).unwrap()
    }

    /// 見出しを別の device に書き換えたら、前の device のフレームは 1 つも返さない。
    /// project が違えば同じ device id でも別物として張り直す。
    #[test]
    fn header_change_never_leaks_frames_of_the_previous_device() {
        let h = handle();
        h.set_sample_rate(48_000);
        let mut r = DeviceScopeReader::default();
        let mut out = Vec::new();
        let (a, b) = (ProjectKey(1), ProjectKey(2));
        h.set_slot(3, a, 10);
        assert_eq!(r.read(&h, 3, &mut out).map(|(p, d, o)| (p, d, o.frames)), Some((a, 10, 0)));
        h.write_block(3, &[0.5; 8], &[0.5; 8]);
        assert_eq!(r.read(&h, 3, &mut out).map(|(_, _, o)| o.frames), Some(8));
        out.clear();

        h.write_block(3, &[0.9; 4], &[0.9; 4]); // device 10 のまま読まれずに残ったフレーム
        h.set_slot(3, a, 11);
        h.write_block(3, &[0.1; 2], &[0.1; 2]);
        let first = r.read(&h, 3, &mut out).expect("見出しあり");
        assert_eq!((first.1, first.2.frames), (11, 0), "張り直しの初回は 0 フレーム");
        assert!(out.is_empty(), "device 10 のフレームが混ざった: {out:?}");
        h.write_block(3, &[0.2; 3], &[0.2; 3]);
        assert_eq!(r.read(&h, 3, &mut out).map(|(_, d, o)| (d, o.frames)), Some((11, 3)));
        assert!(out.iter().all(|s| s[0] == 0.2), "{out:?}");
        out.clear();

        h.set_slot(3, b, 11);
        assert_eq!(r.read(&h, 3, &mut out).map(|(p, _, o)| (p, o.frames)), Some((b, 0)));
        assert!(r.read(&h, 4, &mut out).is_none(), "空き slot");
    }
}
