//! 出力デバイスへ渡す 1 周期分を描く処理と、その描き手の置き場所 (試作: 先読みのパイプライン)。
//!
//! 描き手は 2 通りで、`DAW01_PIPELINE_N` (起動時に 1 回読む。再生中に変えない) で選ぶ:
//! - `N = 0` (既定): 今までどおり CPAL のコールバックの中で [`OutputRenderer::render`] を呼ぶ。
//! - `N ≥ 1`: 専用の描画スレッドが周期 N 個分を先にリングへ描き溜め、コールバックは描き終わった 1 周期分を
//!   リングから取り出して渡し、描画スレッドを起こすだけ。リングが空なら無音を渡して xrun として数える (待たない)。
//!
//! どちらでも [`OutputRenderer`] (= `DeviceRt` を含む描画の状態) を持つのはちょうど 1 本のスレッドで、
//! 描き手が混ざることは型の所有で起きない (`OutputRenderer` は描画スレッドへ move される)。
//!
//! RT: コールバックと描画スレッドは確保・ロック・I/O をしない。リングは rtrb (wait-free SPSC)、
//! 起こしは `Thread::unpark` (Windows は WakeByAddress、確保もロックも無い)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use common::audio_bridge::AudioBridgeHandle;
use common::metrics_bridge::MetricsBridgeHandle;
use common::scope_bridge::ScopeBridgeHandle;

use crate::engine::{self, DeviceRt, SharedState};

/// 先読みする周期の数 (`DAW01_PIPELINE_N`)。読めない / 未設定は 0 (= コールバックの中で描く)。
pub(crate) fn pipeline_n_from_env() -> usize {
    std::env::var("DAW01_PIPELINE_N")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// 描画スレッドが寝る上限。ふつうはコールバックの `unpark` で起きる。上限は停止の取りこぼし防止だけなので、
/// idle park 中 (コールバックが来ない) に空回りしない長さにする。
const RENDER_PARK_TIMEOUT: Duration = Duration::from_millis(100);

/// 1 周期分を描く状態一式 (もとは CPAL クロージャが捕まえていたもの)。
pub(crate) struct OutputRenderer {
    pub(crate) local: DeviceRt,
    shared: Arc<SharedState>,
    bridge: Arc<AudioBridgeHandle>,
    metrics: Arc<MetricsBridgeHandle>,
    scope: Arc<ScopeBridgeHandle>,
    device_scope: Arc<common::device_scope_bridge::DeviceScopeBridgeHandle>,
    session_sample_rate: u32,
    channels: usize,
    // panic-button master declick (詳細は `crate::panic_declick_gain`)。
    // `declick_t` = samples since the envelope started (`None` = inactive).
    // `declick_released_at` = the `declick_t` at which the fade-in began.
    declick_fade_out: u64,
    declick_fade_in: u64,
    declick_max_hold: u64,
    declick_t: Option<u64>,
    declick_released_at: Option<u64>,
    // resource monitor (r.md #3): DSP load average の EMA 状態。
    dsp_load_ema: f32,
    // r.md #49: アイドル park に入るまでの連続無音サンプル数。
    idle_park_samples: u64,
}

impl OutputRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        local: DeviceRt,
        shared: Arc<SharedState>,
        bridge: Arc<AudioBridgeHandle>,
        metrics: Arc<MetricsBridgeHandle>,
        scope: Arc<ScopeBridgeHandle>,
        device_scope: Arc<common::device_scope_bridge::DeviceScopeBridgeHandle>,
        session_sample_rate: u32,
        channels: usize,
    ) -> Self {
        // panic-button master declick. `AudioCommand::Panic` sets
        // `shared.panic_declick`; the renderer consumes that edge and fades the
        // master out, then HOLDS it at zero until `AudioCommand::PanicRelease`
        // (`shared.panic_release`) arrives — which daw_gui sends only once the
        // plugin host has actually finished `ReinitAllPlugins` (reply
        // `PluginsReinitDone`). A `declick_max_hold` safety auto-releases if
        // the reply never comes (plugin-host hang) so the master can't get stuck.
        let sr64 = u64::from(session_sample_rate);
        Self {
            local,
            shared,
            bridge,
            metrics,
            scope,
            device_scope,
            session_sample_rate,
            channels,
            declick_fade_out: (sr64 * 5 / 1000).max(1), // 5 ms
            declick_fade_in: (sr64 * 20 / 1000).max(1), // 20 ms
            declick_max_hold: sr64 * 2,                 // 2 s safety
            declick_t: None,
            declick_released_at: None,
            dsp_load_ema: 0.0,
            idle_park_samples: sr64 * engine::IDLE_PARK_DELAY_SECS,
        }
    }

    /// 1 周期分 (`data.len() / channels` フレーム、`MAX_FRAMES` を超えた分は無音) を `data` (interleave) へ描く。
    ///
    /// `count_load_xrun`: 「処理時間 > 周期」を xrun として数えるか。コールバックの中で描くとき (N = 0) だけ真 —
    /// 先に描くとき (N ≥ 1) の xrun は「リングが空だった回」でコールバックが数える。
    pub(crate) fn render(&mut self, data: &mut [f32], count_load_xrun: bool) {
        // resource monitor (r.md #3): 描く処理全体の時間を測る。 plugin 処理は worker pool でブロッキング同期
        // されるため、 この区間に plugin 負荷が含まれる。 `Instant::now()` は RT 許容。
        let cb_start = std::time::Instant::now();
        let channels = self.channels;
        let sr = self.session_sample_rate;
        let frames = (data.len() / channels).min(common::process_data::MAX_FRAMES);

        self.local
            .process_buffer(&self.shared, &self.bridge, &self.scope, &self.device_scope, sr, frames);

        // consume the panic edge to (re)start the master
        // declick envelope at sample 0 of this buffer.
        if self.shared.panic_declick.swap(false, Ordering::AcqRel) {
            self.declick_t = Some(0);
            self.declick_released_at = None;
        }
        // Once the fade-out is done and we're holding at zero, release
        // (begin the fade-in) when daw_gui signals the reinit finished,
        // or when the safety hold cap is hit (reply never arrived).
        if let Some(t) = self.declick_t
            && self.declick_released_at.is_none()
            && t >= self.declick_fade_out
            && (self.shared.panic_release.swap(false, Ordering::AcqRel)
                || t >= self.declick_fade_out + self.declick_max_hold)
        {
            self.declick_released_at = Some(t);
        }

        // Interleave master_l/r into the device buffer, applying
        // the panic declick envelope when active (master gain は
        // render_master_buffer 内で適用済み — live/export 統一 §5)。
        // Lanes beyond stereo on the device are zeroed.
        for (i, out) in data.chunks_exact_mut(channels).take(frames).enumerate() {
            let dg = match self.declick_t {
                Some(t) => crate::panic_declick_gain(
                    t + i as u64,
                    self.declick_fade_out,
                    self.declick_fade_in,
                    self.declick_released_at,
                ),
                None => 1.0,
            };
            out[0] = self.local.master_l[i] * dg;
            if channels > 1 {
                out[1] = self.local.master_r[i] * dg;
            }
            for s in &mut out[2.min(channels)..] {
                *s = 0.0;
            }
        }
        // Advance the envelope; clear it once the fade-in has finished
        // so the master returns to full gain.
        if let Some(t) = self.declick_t {
            let next = t + frames as u64;
            self.declick_t = match self.declick_released_at {
                Some(r) if next >= r + self.declick_fade_in => None,
                _ => Some(next),
            };
        }
        for s in &mut data[frames * channels..] {
            *s = 0.0;
        }

        // r.md #49 のアイドル判定用「実際にデバイスへ出す音の無音判定」。
        // メーター表示の測定点 (`render_master_buffer` 直後) とは目的が違うので共有しない
        // (park してよいかはメトロノームを含むスピーカーへ出る音で決める)。
        let (peak_l, peak_r) = crate::block_peaks_stereo(data, channels);

        // resource monitor (r.md #3): DSP load を publish。 load = 処理時間 ÷ バッファ周期。
        let metrics = &self.metrics;
        let elapsed = cb_start.elapsed().as_secs_f32();
        let load = common::metrics_bridge::dsp_load(elapsed, frames as u32, sr);
        metrics.observe_dsp_load_peak(load);
        metrics.observe_load_hist(load);
        crate::publish_graph_profile(self.local.worker.as_deref(), metrics);
        self.dsp_load_ema = common::metrics_bridge::ema(self.dsp_load_ema, load, 0.1);
        metrics.set_dsp_load_avg(self.dsp_load_ema);
        metrics.set_buffer_info(frames as u32, sr);
        // xrun は再生中のみカウントする。 停止中 (無音) の処理時間スパイク (起動直後の cold start /
        // OS scheduling jitter) は実際の音切れではないため除外する。
        if count_load_xrun && load > 1.0 && self.local.any_playing() {
            metrics.add_xrun();
        }

        // r.md #49: アイドル park の判定。atomic の読み書きだけなので RT 安全。
        //
        // 「無音」を条件に含めているので、リバーブの残響や自走プラグインが鳴っている間はカウンタが
        // 進まない = 音が途中で切れることは構造的に起きない。
        //
        // **描く処理の最後に置くこと** — publish を 0 に畳む処理が、上の meters / DSP load publish に
        // 上書きされてはならない。
        let idle = engine::buffer_is_idle(
            self.shared.app_active.load(Ordering::Acquire),
            self.local.any_rolling(),
            0,
            self.local.shared.export_running.load(Ordering::Acquire),
            peak_l,
            peak_r,
        );
        let idle_n =
            engine::advance_idle_counter(&self.shared.idle_silent_samples, idle, frames as u64);
        if idle_n >= self.idle_park_samples {
            if !self.shared.park_requested.swap(true, Ordering::AcqRel) {
                // park に入る前に「動くもの」を 0 で publish しておく (止まったメーターが点灯したまま
                // 凍結しないように)。mod scalars はパラメータ値なのでゼロにしない。
                for p in &self.local.projects {
                    p.clear_meters(self.bridge.project(p.telemetry_slot));
                }
                self.dsp_load_ema = 0.0;
                self.metrics.set_dsp_load_avg(0.0);
            }
        } else if self.shared.park_requested.load(Ordering::Acquire) {
            // 条件が崩れた (= resume 済み or 音が鳴り始めた)。要求を取り下げれば notify thread の
            // reconciler が stream を起こす。定常状態で書き込みを出さないよう load してから store。
            self.shared.park_requested.store(false, Ordering::Release);
        }
    }
}

/// デバイスが飢えた回を数える (描き方によらない物差し — `MetricsBridge::starve_count`)。
///
/// WASAPI shared + event では 1 回のフレーム数 = バッファ全体 − 未再生分。開始直後の回はバッファが空なので
/// 全体の大きさが来る。以後それ以上が来た回 = デバイスが前の回までの音を出し切っていた。
pub(crate) struct StarveProbe {
    full: usize,
}

impl StarveProbe {
    pub(crate) fn new() -> Self {
        Self { full: 0 }
    }

    pub(crate) fn observe(&mut self, frames: usize, metrics: &MetricsBridgeHandle) {
        if self.full == 0 {
            self.full = frames;
        } else if frames >= self.full {
            metrics.add_starve();
        }
    }
}

/// 描画スレッドと、コールバック側の持ち物 (リングの読み口・起こし・停止)。
///
/// drop (= CPAL の stream とクロージャが捨てられたとき) で描画スレッドを止めて join する。
/// `OutputRenderer` (`DeviceRt`) は描画スレッドの上で drop される。
pub(crate) struct RenderPipeline {
    cons: rtrb::Consumer<f32>,
    /// コールバックが最後に見たフレーム数 (`MAX_FRAMES` 以下)。描画スレッドの 1 回の描画の大きさ。0 = まだ来ていない。
    period: Arc<AtomicUsize>,
    /// 描画スレッドが最後に描いた回が再生中だったか (リングが空の回を xrun として数えるかの判定)。
    playing: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    metrics: Arc<MetricsBridgeHandle>,
}

impl RenderPipeline {
    /// 描画スレッドを起こす。リングの容量 (`(n + 2) × MAX_FRAMES × channels`) はここで 1 回だけ確保する。
    pub(crate) fn spawn(renderer: OutputRenderer, n: usize) -> std::io::Result<Self> {
        let channels = renderer.channels;
        let metrics = Arc::clone(&renderer.metrics);
        let cap = (n + 2) * common::process_data::MAX_FRAMES * channels;
        let (prod, cons) = rtrb::RingBuffer::<f32>::new(cap);
        let period = Arc::new(AtomicUsize::new(0));
        let playing = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let period = Arc::clone(&period);
            let playing = Arc::clone(&playing);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("audio render".into())
                .spawn(move || render_loop(renderer, prod, n, &period, &playing, &stop))?
        };
        Ok(Self { cons, period, playing, stop, thread: Some(thread), metrics })
    }

    /// CPAL コールバックの本体 (N ≥ 1)。描き終わった 1 周期分を渡して描画スレッドを起こす。待たない。
    pub(crate) fn on_callback(&mut self, data: &mut [f32], channels: usize) {
        let frames = data.len() / channels;
        self.period
            .store(frames.min(common::process_data::MAX_FRAMES), Ordering::Release);
        match self.cons.read_chunk(data.len()) {
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                data[..a.len()].copy_from_slice(a);
                data[a.len()..].copy_from_slice(b);
                chunk.commit_all();
            }
            Err(_) => {
                // 足りない: 全部無音にして消費しない (残りは次の回に出す)。
                data.fill(0.0);
                if self.playing.load(Ordering::Acquire) {
                    self.metrics.add_xrun();
                }
            }
        }
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }
}

impl Drop for RenderPipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            if t.join().is_err() {
                tracing::error!("audio render thread panicked");
            }
        }
    }
}

/// 描画スレッド: リングに `n × period` フレーム分が溜まるまで続けて描き、溜まったら寝る。
fn render_loop(
    mut renderer: OutputRenderer,
    mut prod: rtrb::Producer<f32>,
    n: usize,
    period: &AtomicUsize,
    playing: &AtomicBool,
    stop: &AtomicBool,
) {
    // 優先度・MMCSS は CPAL のコールバックと同じ (cpal 0.17 WASAPI は TIME_CRITICAL、MMCSS は自前 join)。
    crate::audio_worker::boost_thread_priority("audio render");
    let _mmcss = common::mmcss::join_pro_audio();
    if _mmcss.is_none() {
        renderer.local.shared.mmcss_join_failed.store(true, Ordering::Release);
    }
    let channels = renderer.channels;
    let cap = prod.buffer().capacity();
    // 1 回分を描く作業場 (起動時に確保、以後使い回す)。
    let mut scratch = vec![0.0f32; common::process_data::MAX_FRAMES * channels];
    while !stop.load(Ordering::Acquire) {
        let p = period.load(Ordering::Acquire);
        if p > 0 {
            let want = n * p * channels;
            let len = p * channels;
            while !stop.load(Ordering::Acquire)
                && cap - prod.slots() < want
                && let Ok(mut chunk) = prod.write_chunk(len)
            {
                let buf = &mut scratch[..len];
                renderer.render(buf, false);
                let (a, b) = chunk.as_mut_slices();
                let split = a.len();
                a.copy_from_slice(&buf[..split]);
                b.copy_from_slice(&buf[split..]);
                chunk.commit_all();
                playing.store(renderer.local.any_playing(), Ordering::Release);
            }
        }
        std::thread::park_timeout(RENDER_PARK_TIMEOUT);
    }
}
