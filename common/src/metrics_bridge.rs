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

/// per-plugin メトリクススロットのハードキャップ。 `plugin_id` は plugin
/// registry の Vec インデックス (`MAX_TRACKS` = 32 × 妥当な device 数)。 この id
/// を超える plugin も処理はされるが CPU スロットを publish しない
/// (`audio_bridge::track_peaks` と同じ silently-drop)。
pub const MAX_PLUGINS: usize = 512;

/// DSP load を `warn`(黄) 表示に切り替える閾値 (= 期限の 70%)。
pub const LOAD_WARN: f32 = 0.7;
/// DSP load を `danger`(赤) 表示に切り替える閾値 (= 期限の 90%)。
pub const LOAD_DANGER: f32 = 0.9;

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
    /// per-plugin の直近 `process()` 時間 (μs)。 plugin-host worker が
    /// `plugin_id` をインデックスに store。 範囲外 id は silently drop。
    pub plugin_dsp_us: [AtomicU32; MAX_PLUGINS],
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

    /// plugin-host worker (RT): per-plugin の直近 `process()` μs を store。
    /// 範囲外 `plugin_id` は silently drop。
    pub fn set_plugin_dsp_us(&self, plugin_id: u32, us: u32) {
        let Some(cell) = self.bridge().plugin_dsp_us.get(plugin_id as usize) else {
            return;
        };
        cell.store(us, Ordering::Release);
    }

    pub fn plugin_dsp_us(&self, plugin_id: u32) -> u32 {
        let Some(cell) = self.bridge().plugin_dsp_us.get(plugin_id as usize) else {
            return 0;
        };
        cell.load(Ordering::Acquire)
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
