//! Shared-memory layout for resource metrics — DSP load (audio callback の
//! 負荷)、 xrun カウント、 buffer/SR、 そして per-plugin の `process()` 時間。
//!
//! `AudioBridge` / `WorkerBridge` と同じ流儀: daw_gui (親) が `create`、
//! daw_audio と daw_plugin_host が `open`。 全フィールドは lock-free atomic
//! なので RT スレッド (CPAL callback / worker) から store でき、 daw_gui は
//! UI tick で poll する。
//!
//! 計測の定義 (Ableton / Bitwig 公式マニュアルと一致): DSP load = callback
//! 処理時間 ÷ バッファ周期 (`frames / sample_rate`)。 plugin 処理は worker pool
//! でブロッキング同期される (`worker_bridge` の SetEvent/Wait ハンドシェイク)
//! ため、 callback 処理時間に plugin 負荷が自然に含まれる。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;

use crate::shmem::NamedShmem;

/// per-plugin メトリクススロットのハードキャップ (= 同時に CPU 計測できる
/// device 数)。 slot は安定 `device_id` (u64、 単調増加・非再利用) を **値** で
/// 保持し、 空き slot を claim して使う (§7.5 `PluginMetricSlot`)。 これにより
/// device_id を配列 index にする旧実装 (id が 512 を超えると計測が silently
/// drop する) を根治する。 同時 live device 数がこの上限を超えたときだけ drop。
pub const MAX_PLUGINS: usize = 512;

/// DSP load を `warn`(黄) 表示に切り替える閾値 (= 期限の 70%)。
pub const LOAD_WARN: f32 = 0.7;
/// DSP load を `danger`(赤) 表示に切り替える閾値 (= 期限の 90%)。
pub const LOAD_DANGER: f32 = 0.9;

/// per-plugin CPU 計測の 1 slot (§7.5)。 `device_id` は「この slot が誰の計測か」
/// を示す安定 id (`0` = 空き)。 plugin-host worker が load 後に空き slot を claim
/// (`device_id` を CAS 占有)、 RT では小さい slot index へ `us` を store、 daw_gui は
/// `device_id` で線形 scan して読む。 全 atomic なので lock-free。
#[repr(C)]
pub struct PluginMetricSlot {
    /// この slot を占有する device の安定 id。 `0` = 空き。
    pub device_id: AtomicU64,
    /// 直近 `process()` 時間 (μs)。
    pub us: AtomicU32,
}

#[repr(C)]
pub struct MetricsBridge {
    /// DSP load の worst-case (RT)、 `f32::to_bits`。 daw_audio が各 buffer で
    /// `fetch_max`、 daw_gui が poll 時に `swap(0)` でリセット → 「直近 UI 窓の
    /// ピーク」。 非負 f32 はビットパターン順 == 値順なので整数 `fetch_max` で
    /// 正しく最大が取れる。
    pub dsp_load_peak: AtomicU32,
    /// DSP load の指数移動平均、 `f32::to_bits`。 daw_audio が callback ローカルで
    /// 平滑化して store。
    pub dsp_load_avg: AtomicU32,
    /// xrun / dropout の累積カウント (monotonic)。 `load > 1.0` 検出で
    /// `fetch_add(1)`。
    pub xrun_count: AtomicU64,
    /// 現在の buffer 長 (frames)。 daw_audio が起動時に publish (静的、 レイテンシ
    /// 表示と DSP load 検算用)。
    pub buffer_frames: AtomicU32,
    /// 現在の sample rate (Hz)。 daw_audio が起動時に publish (静的)。
    pub sample_rate: AtomicU32,
    /// per-plugin の直近 `process()` 時間 (μs)。 device_id 値を保持する slot
    /// 配列 (§7.5)。 index ではなく `device_id` で claim / read するので、 id が
    /// `MAX_PLUGINS` を超えても計測が drop しない。
    pub plugin_metrics: [PluginMetricSlot; MAX_PLUGINS],
}

impl MetricsBridge {
    pub const SIZE: usize = std::mem::size_of::<Self>();
}

/// Owning handle to the metrics shared memory region.
pub struct MetricsBridgeHandle {
    shmem: NamedShmem,
}

impl MetricsBridgeHandle {
    pub fn create(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::create(os_id, MetricsBridge::SIZE)?;
        // Zero-initialise: every atomic starts at 0 (= 0.0 load, 0 xrun, idle
        // plugins) before any reader polls.
        unsafe { std::ptr::write_bytes(shmem.as_ptr(), 0, MetricsBridge::SIZE) };
        Ok(Self { shmem })
    }

    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::open(os_id, MetricsBridge::SIZE)?;
        Ok(Self { shmem })
    }

    fn bridge(&self) -> &MetricsBridge {
        // SAFETY: the mapping is at least `SIZE` bytes (checked in
        // `create`/`open`) and the `MapViewOfFile` pointer is 64 KiB-aligned,
        // which covers `MetricsBridge`'s 8-byte (AtomicU64) alignment. All
        // fields are atomics → valid for any bit pattern, so the mapped bytes
        // are always a valid `MetricsBridge`, and concurrent cross-process
        // access is sound.
        unsafe { &*(self.shmem.as_ptr() as *const MetricsBridge) }
    }

    /// daw_audio (RT): 1 buffer の DSP load を直近窓のピークに max 合成。
    pub fn observe_dsp_load_peak(&self, load: f32) {
        self.bridge()
            .dsp_load_peak
            .fetch_max(load.to_bits(), Ordering::AcqRel);
    }

    /// daw_gui: 直近窓のピークを読み出してリセット (swap)。
    pub fn take_dsp_load_peak(&self) -> f32 {
        f32::from_bits(self.bridge().dsp_load_peak.swap(0, Ordering::AcqRel))
    }

    /// daw_audio (RT): EMA で平滑化した平均 DSP load を publish。
    pub fn set_dsp_load_avg(&self, v: f32) {
        self.bridge()
            .dsp_load_avg
            .store(v.to_bits(), Ordering::Release);
    }

    pub fn dsp_load_avg(&self) -> f32 {
        f32::from_bits(self.bridge().dsp_load_avg.load(Ordering::Acquire))
    }

    /// daw_audio (RT): dropout を 1 件記録。
    pub fn add_xrun(&self) {
        self.bridge().xrun_count.fetch_add(1, Ordering::AcqRel);
    }

    /// daw_gui: xrun カウンタを 0 にクリアする (詳細パネルの Clear ボタン)。
    /// クリア後に daw_audio が新たな dropout を検出すれば再び増える。
    pub fn reset_xrun(&self) {
        self.bridge().xrun_count.store(0, Ordering::Release);
    }

    pub fn xrun_count(&self) -> u64 {
        self.bridge().xrun_count.load(Ordering::Acquire)
    }

    /// daw_audio: buffer 長 / sample rate を publish (起動時に 1 回)。
    pub fn set_buffer_info(&self, frames: u32, sample_rate: u32) {
        self.bridge()
            .buffer_frames
            .store(frames, Ordering::Release);
        self.bridge()
            .sample_rate
            .store(sample_rate, Ordering::Release);
    }

    pub fn buffer_info(&self) -> (u32, u32) {
        (
            self.bridge().buffer_frames.load(Ordering::Acquire),
            self.bridge().sample_rate.load(Ordering::Acquire),
        )
    }

    /// plugin-host worker (RT): `device_id` の計測 slot を取得する。 既存 slot が
    /// あればその index、 無ければ空き slot を CAS 占有して claim する。 満杯 /
    /// CAS 競合時は `None` (呼び元は次 buffer で再試行)。 worker は返り値を entry に
    /// キャッシュするので、 この線形 scan は plugin ごと事実上 1 回だけ走る。
    pub fn claim_plugin_metric_slot(&self, device_id: u64) -> Option<usize> {
        debug_assert_ne!(device_id, 0, "device_id 0 は sentinel、 claim 不可");
        let slots = &self.bridge().plugin_metrics;
        let mut free: Option<usize> = None;
        for (i, s) in slots.iter().enumerate() {
            let d = s.device_id.load(Ordering::Acquire);
            if d == device_id {
                return Some(i); // 既に自分の slot
            }
            if d == 0 && free.is_none() {
                free = Some(i);
            }
        }
        let i = free?;
        // 空き slot を占有 (他 worker と競合したら諦め = 次回再試行)。
        match slots[i].device_id.compare_exchange(
            0,
            device_id,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Some(i),
            Err(_) => None,
        }
    }

    /// plugin-host worker (RT): claim 済み slot index へ μs を store。
    pub fn set_plugin_dsp_us_slot(&self, slot: usize, us: u32) {
        if let Some(s) = self.bridge().plugin_metrics.get(slot) {
            s.us.store(us, Ordering::Release);
        }
    }

    /// daw_gui: `device_id` の直近 `process()` μs。 未計測 / 未 claim は 0。
    pub fn plugin_dsp_us(&self, device_id: u64) -> u32 {
        for s in self.bridge().plugin_metrics.iter() {
            if s.device_id.load(Ordering::Acquire) == device_id {
                return s.us.load(Ordering::Acquire);
            }
        }
        0
    }

    /// daw_gui: 現在 live でない device が占有する slot を解放する
    /// (unload された plugin の entry は既に消えて worker が store しないので安全)。
    /// per-plugin パネルの read 前に呼び、 slot 枯渇を防ぐ。
    pub fn reclaim_plugin_metric_slots(&self, live: &std::collections::HashSet<u64>) {
        for s in self.bridge().plugin_metrics.iter() {
            let d = s.device_id.load(Ordering::Acquire);
            if d != 0 && !live.contains(&d) {
                s.us.store(0, Ordering::Release);
                s.device_id.store(0, Ordering::Release);
            }
        }
    }
}

// The mapped region holds only atomics; cross-thread / cross-process sharing
// is sound (same reasoning as `AudioBridgeHandle`).
unsafe impl Send for MetricsBridgeHandle {}
unsafe impl Sync for MetricsBridgeHandle {}

/// Shared-memory os_id for the metrics bridge, namespaced by the daw_gui PID
/// so two concurrent daw_01 sessions don't clash.
pub fn metrics_shmem_id(pid: u32) -> String {
    format!("daw_01_metrics_{pid}")
}

// ---- 純粋計測ロジック (RT スレッド・UI 双方から使う、 テスト対象) ----

/// DSP load = 処理時間 ÷ バッファ周期。 1.0 で「期限ぴったり」、 >1.0 で dropout。
/// `frames` / `sample_rate` が 0 のときは 0.0 (起動直後の未確定値)。
pub fn dsp_load(elapsed_s: f32, frames: u32, sample_rate: u32) -> f32 {
    if frames == 0 || sample_rate == 0 {
        return 0.0;
    }
    let period_s = frames as f32 / sample_rate as f32;
    if period_s <= 0.0 {
        return 0.0;
    }
    elapsed_s / period_s
}

/// 指数移動平均。 `alpha` は新サンプルの重み (0..=1)。
pub fn ema(prev: f32, sample: f32, alpha: f32) -> f32 {
    prev * (1.0 - alpha) + sample * alpha
}

/// frame 時間 (秒) の移動平均から FPS。 `dt_ema_s <= 0` は 0 (ゼロ除算回避)。
pub fn fps_from_dt(dt_ema_s: f32) -> f32 {
    if dt_ema_s <= 0.0 {
        return 0.0;
    }
    1.0 / dt_ema_s
}

/// daw_gui が UI に表示する集計済みリソース指標のスナップショット。 poller
/// (DSP load / xrun / buffer)、 sysinfo スレッド (system CPU / memory)、 runner
/// (fps) が別々のソースから埋め、 status bar の常駐メーターと詳細パネルが読む。
/// per-plugin CPU はサイズ可変なのでここには含めず、 詳細パネルが
/// `MetricsBridgeHandle::plugin_dsp_us` を track 構成に沿って直接読む。
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceMetrics {
    /// DSP load の直近窓ピーク (RT / worst-case)、 0.0..=1.0+ (1.0 = 期限ぴったり)。
    pub dsp_load_peak: f32,
    /// DSP load の指数移動平均。
    pub dsp_load_avg: f32,
    /// xrun / dropout の累積件数。
    pub xrun_count: u64,
    /// 現在の buffer 長 (frames)。
    pub buffer_frames: u32,
    /// 現在の sample rate (Hz)。
    pub sample_rate: u32,
    /// daw_01 全 3 プロセス合計の system CPU 使用率 (%)。 DSP load とは別物。
    pub system_cpu: f32,
    /// daw_01 全 3 プロセス合計の常駐メモリ (MB)。
    pub memory_mb: f32,
    /// GUI のフレームレート (fps)。
    pub fps: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsp_load_は_処理時間とバッファ周期の比() {
        // 512 frames @ 48000 Hz → 周期 = 512/48000 ≈ 10.667 ms。
        // (elapsed_s, frames, sample_rate, expected)
        let period = 512.0 / 48000.0;
        let cases = [
            (0.0, 512, 48000, 0.0),
            (period, 512, 48000, 1.0),       // 期限ぴったり
            (period * 0.5, 512, 48000, 0.5), // 半分
            (period * 1.5, 512, 48000, 1.5), // overrun
            (0.001, 0, 48000, 0.0),          // frames=0 → 0 (未確定)
            (0.001, 512, 0, 0.0),            // sr=0 → 0 (未確定)
        ];
        for (elapsed, frames, sr, expected) in cases {
            let got = dsp_load(elapsed, frames, sr);
            assert!(
                (got - expected).abs() < 1e-4,
                "elapsed={elapsed} frames={frames} sr={sr} got={got} expected={expected}"
            );
        }
    }

    #[test]
    fn ema_は_前値と新値を_alpha_で混合() {
        // (prev, sample, alpha, expected)
        let cases = [
            (0.0, 1.0, 0.1, 0.1),  // 初回 EMA
            (1.0, 1.0, 0.1, 1.0),  // 定常
            (0.0, 1.0, 1.0, 1.0),  // alpha=1 → 新値そのまま
            (1.0, 0.0, 0.0, 1.0),  // alpha=0 → 前値維持
            (0.5, 1.5, 0.5, 1.0),  // 中点
        ];
        for (prev, sample, alpha, expected) in cases {
            let got = ema(prev, sample, alpha);
            assert!(
                (got - expected).abs() < 1e-6,
                "prev={prev} sample={sample} alpha={alpha} got={got}"
            );
        }
    }

    #[test]
    fn fps_は_dt_の逆数_ゼロは0() {
        let cases = [(1.0 / 60.0, 60.0), (1.0 / 30.0, 30.0), (0.0, 0.0)];
        for (dt, expected) in cases {
            let got = fps_from_dt(dt);
            assert!((got - expected).abs() < 1e-3, "dt={dt} got={got}");
        }
    }
}
