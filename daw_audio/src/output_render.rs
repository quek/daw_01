//! 出力デバイスへ渡す 1 周期分を描く処理と、その描き手の置き場所 (先読み)。
//!
//! 描き手は 2 通り:
//! - **先読み** (環境設定の既定): 専用の描画スレッドが [`RENDER_AHEAD_PERIODS`] 周期分を先にリングへ描き溜め、
//!   コールバックは描き終わった 1 周期分をリングから取り出して渡し、描画スレッドを起こすだけ。重い 1 回を前の回の
//!   余りで吸収する。リングが空なら無音を渡して xrun として数える (待たない)。
//! - **先読みしない**: 今までどおり CPAL のコールバックの中で [`OutputRenderer::render`] を呼ぶ。
//!
//! [`OutputRenderer`] (= `DeviceRt` を含む描画の状態) を持つのは常にちょうど 1 本のスレッドで、描き手が混ざることは
//! 型の所有で起きない。切り替えは `SharedState::render_ahead_wanted` を見た **今の持ち主が、停止中に** 自分で
//! 1 枠の rtrb へ move して渡す (再生中に替わると聞こえ方と遅れの勘定が途中で変わるので、次の停止まで待つ)。
//! 先読みから戻るときは、描き溜めた分をコールバックが出し切ってから自分で描く (時間は連続)。
//!
//! 遅れの勘定: 先読み中のエンジンの「今」はスピーカーより [`RENDER_AHEAD_PERIODS`] 周期先に居る。GUI が見る
//! transport (再生位置・走行・count-in・録音してよいか) はその回数ぶん遅らせて出す (`engine::TransportOut`)。
//!
//! RT: コールバックと描画スレッドは確保・ロック・I/O をしない。リングは rtrb (wait-free SPSC)、
//! 起こしは `Thread::unpark` (Windows は WakeByAddress、確保もロックも無い)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;

use common::audio_bridge::AudioBridgeHandle;
use common::metrics_bridge::MetricsBridgeHandle;
use common::scope_bridge::ScopeBridgeHandle;

use crate::engine::{self, DeviceRt, SharedState};

/// 先読みで先に描く周期の数。「90% の次に 110%」の重い 1 回を前の回の余りで吸収できる最小の数
/// (ユーザー決定 2026-10-13)。大きくすると全操作の遅れが増えるだけなので、可変にしない。
pub(crate) const RENDER_AHEAD_PERIODS: usize = 2;

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
    /// `ahead`: 描画スレッドが先読みで描いているか。transport の publish を遅らせるか (`engine::TransportOut`) と、
    /// 「処理時間 > 周期」を xrun として数えるか (コールバックの中で描くときだけ — 先読みの xrun は
    /// 「リングが空だった回」でコールバックが数える) を決める。
    pub(crate) fn render(&mut self, data: &mut [f32], ahead: bool) {
        // resource monitor (r.md #3): 描く処理全体の時間を測る。 plugin 処理は worker pool でブロッキング同期
        // されるため、 この区間に plugin 負荷が含まれる。 `Instant::now()` は RT 許容。
        let cb_start = std::time::Instant::now();
        let channels = self.channels;
        let sr = self.session_sample_rate;
        let frames = (data.len() / channels).min(common::process_data::MAX_FRAMES);
        let count_load_xrun = !ahead;
        self.local.render_ahead = ahead;
        self.metrics.set_render_ahead(ahead);

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

/// コールバックと描画スレッドが共有する小さな状態。
struct Link {
    /// コールバックが最後に見たフレーム数 (`MAX_FRAMES` 以下)。描画スレッドの 1 回の描画の大きさ。0 = まだ来ていない。
    period: AtomicUsize,
    /// 描画スレッドが最後に描いた回が再生中だったか (リングが空の回を xrun として数えるかの判定)。
    playing: AtomicBool,
    stop: AtomicBool,
}

/// 停止中か (描き手を替えてよいか)。count-in も走っている扱い ([`DeviceRt::any_rolling`])。
fn may_switch(r: &OutputRenderer) -> bool {
    !r.local.any_rolling()
}

/// CPAL クロージャの持ち物: 描き手 (先読みしない間)・リングの読み口・描き手の受け渡し口・描画スレッド。
///
/// drop (= CPAL の stream とクロージャが捨てられたとき) で描画スレッドを止めて join する。描画スレッドが
/// 持っていた `OutputRenderer` はそのスレッドの上で、こちらが持っていた分はここで drop される (どちらも RT の外)。
pub(crate) struct OutputStage {
    /// コールバックが描き手のとき `Some`。
    inline: Option<Box<OutputRenderer>>,
    cons: rtrb::Consumer<f32>,
    to_thread: rtrb::Producer<Box<OutputRenderer>>,
    from_thread: rtrb::Consumer<Box<OutputRenderer>>,
    link: Arc<Link>,
    shared: Arc<SharedState>,
    metrics: Arc<MetricsBridgeHandle>,
    channels: usize,
    thread: Option<JoinHandle<()>>,
}

impl OutputStage {
    /// 描画スレッドを起こす (描き手は持たずに寝ている)。描き手はまずコールバックが持ち、先読みを望まれていれば
    /// 最初の停止中のコールバックで渡す。リング (`(RENDER_AHEAD_PERIODS + 2) × MAX_FRAMES × channels`) と
    /// 受け渡しの枠はここで 1 回だけ確保する。
    pub(crate) fn spawn(renderer: OutputRenderer) -> std::io::Result<Self> {
        let (mut stage, mut worker) = Self::new(renderer);
        stage.thread =
            Some(std::thread::Builder::new().name("audio render".into()).spawn(move || worker.run())?);
        Ok(stage)
    }

    /// コールバック側と描画スレッド側の持ち物を組む (スレッドはまだ起こさない — 検査は同じスレッドで交互に回す)。
    fn new(renderer: OutputRenderer) -> (Self, RenderThread) {
        let channels = renderer.channels;
        let metrics = Arc::clone(&renderer.metrics);
        let shared = Arc::clone(&renderer.shared);
        let cap = (RENDER_AHEAD_PERIODS + 2) * common::process_data::MAX_FRAMES * channels;
        let (prod, cons) = rtrb::RingBuffer::<f32>::new(cap);
        let (to_thread, thread_rx) = rtrb::RingBuffer::new(1);
        let (thread_tx, from_thread) = rtrb::RingBuffer::new(1);
        let link = Arc::new(Link {
            period: AtomicUsize::new(0),
            playing: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let worker = RenderThread {
            held: None,
            prod,
            rx: thread_rx,
            tx: thread_tx,
            link: Arc::clone(&link),
            shared: Arc::clone(&shared),
            scratch: vec![0.0; common::process_data::MAX_FRAMES * channels],
            channels,
        };
        let stage = Self {
            inline: Some(Box::new(renderer)),
            cons,
            to_thread,
            from_thread,
            link,
            shared,
            metrics,
            channels,
            thread: None,
        };
        (stage, worker)
    }

    /// CPAL コールバックの本体。待たない。
    pub(crate) fn on_callback(&mut self, data: &mut [f32]) {
        let frames = data.len() / self.channels;
        self.link
            .period
            .store(frames.min(common::process_data::MAX_FRAMES), Ordering::Release);
        if self.inline.is_none()
            && let Ok(r) = self.from_thread.pop()
        {
            self.inline = Some(r);
        }
        if self.inline.is_some() {
            self.render_inline(data);
        } else {
            self.take_from_ring(data);
        }
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }

    /// 先読みしない間: 先読みから戻った直後に残っている描き溜めを先に出し切り (時間を連続に保つ)、残りを自分で描く。
    /// 停止中に先読みを望まれていれば、描き溜めが空になった回の後で描き手を描画スレッドへ渡す。
    fn render_inline(&mut self, data: &mut [f32]) {
        let drained = self.cons.slots().min(data.len());
        if drained > 0
            && let Ok(chunk) = self.cons.read_chunk(drained)
        {
            let (a, b) = chunk.as_slices();
            data[..a.len()].copy_from_slice(a);
            data[a.len()..drained].copy_from_slice(b);
            chunk.commit_all();
        }
        let Some(r) = self.inline.as_mut() else { return };
        if drained < data.len() {
            r.render(&mut data[drained..], false);
        }
        if self.shared.render_ahead_wanted.load(Ordering::Acquire) && may_switch(r) && self.cons.is_empty() {
            let Some(r) = self.inline.take() else { return };
            if let Err(rtrb::PushError::Full(r)) = self.to_thread.push(r) {
                // 枠は 1 つで描き手も 1 つなので起きない。起きても手放さない。
                self.inline = Some(r);
            }
        }
    }

    /// 先読みの間: 描き終わった 1 周期分を渡す。足りなければ全部無音にして消費しない (残りは次の回に出す)。
    fn take_from_ring(&mut self, data: &mut [f32]) {
        match self.cons.read_chunk(data.len()) {
            Ok(chunk) => {
                let (a, b) = chunk.as_slices();
                data[..a.len()].copy_from_slice(a);
                data[a.len()..].copy_from_slice(b);
                chunk.commit_all();
            }
            Err(_) => {
                data.fill(0.0);
                if self.link.playing.load(Ordering::Acquire) {
                    self.metrics.add_xrun();
                }
            }
        }
    }
}

impl Drop for OutputStage {
    fn drop(&mut self) {
        self.link.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            if t.join().is_err() {
                tracing::error!("audio render thread panicked");
            }
        }
    }
}

/// 描画スレッドの持ち物。
struct RenderThread {
    /// 描き手 (先読みの間だけ `Some`)。
    held: Option<Box<OutputRenderer>>,
    prod: rtrb::Producer<f32>,
    rx: rtrb::Consumer<Box<OutputRenderer>>,
    tx: rtrb::Producer<Box<OutputRenderer>>,
    link: Arc<Link>,
    shared: Arc<SharedState>,
    /// 1 回分を描く作業場 (起動時に確保、以後使い回す)。
    scratch: Vec<f32>,
    channels: usize,
}

impl RenderThread {
    fn run(&mut self) {
        // 優先度・MMCSS は CPAL のコールバックと同じ (cpal 0.17 WASAPI は TIME_CRITICAL、MMCSS は自前 join)。
        crate::audio_worker::boost_thread_priority("audio render");
        let _mmcss = common::mmcss::join_pro_audio();
        let mut mmcss_reported = _mmcss.is_some();
        while !self.link.stop.load(Ordering::Acquire) {
            self.step();
            if !mmcss_reported && let Some(r) = &self.held {
                r.local.shared.mmcss_join_failed.store(true, Ordering::Release);
                mmcss_reported = true;
            }
            // 起こすのはコールバック (毎回) と停止 (drop)。unpark が先に来ても token が残るので取りこぼさない。
            std::thread::park();
        }
    }

    /// 1 回起きた分の仕事: 描き手を受け取り、停止中に先読みをやめるよう望まれていれば返し、
    /// そうでなければリングに `RENDER_AHEAD_PERIODS × period` フレーム分が溜まるまで描く。
    fn step(&mut self) {
        if self.held.is_none() {
            self.held = self.rx.pop().ok();
        }
        let Some(r) = self.held.as_mut() else { return };
        if !self.shared.render_ahead_wanted.load(Ordering::Acquire) && may_switch(r) {
            if let Some(r) = self.held.take()
                && let Err(rtrb::PushError::Full(r)) = self.tx.push(r)
            {
                // 枠は 1 つで描き手も 1 つなので起きない。起きても手放さない。
                self.held = Some(r);
            }
            return;
        }
        let p = self.link.period.load(Ordering::Acquire);
        if p == 0 {
            return;
        }
        let cap = self.prod.buffer().capacity();
        let want = RENDER_AHEAD_PERIODS * p * self.channels;
        let len = p * self.channels;
        while !self.link.stop.load(Ordering::Acquire)
            && cap - self.prod.slots() < want
            && let Ok(mut chunk) = self.prod.write_chunk(len)
        {
            let buf = &mut self.scratch[..len];
            r.render(buf, true);
            let (a, b) = chunk.as_mut_slices();
            let split = a.len();
            a.copy_from_slice(&buf[..split]);
            b.copy_from_slice(&buf[split..]);
            chunk.commit_all();
            self.link.playing.store(r.local.any_playing(), Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests;
