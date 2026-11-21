//! OS 標準のデバイス (cpal。Windows は WASAPI の共有モード)。
//!
//! - 出力は選んだデバイス (未選択 = OS の既定)。形式はデバイスの mix format のまま開き、engine の f32 から
//!   変換する (F32 以外も開ける)。サンプルレートとバッファサイズは OS が決める (共有モードでは選べない)。
//! - 入力は **別の stream** (時計も別)。入力のコールバックが ring に積み、出力のコールバックが自分のフレーム数
//!   ぶんを取り出す。ずれは捨てる / 無音で埋める (`super::monitor::MonitorInput::take_unsynced`)。入力デバイスの
//!   レートが出力と違うときは開かない (共有モードの入力はデバイスの mix format でしか開けず、こちらで
//!   レート変換はしない)。
//!
//! レイテンシは OS が正確な値を出さないので、動かして見えた大きさからの推定値 ([`SystemDevice::poll`])。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use common::audio_device::{
    AudioChannelInfo, AudioDeviceConfig, AudioDeviceInfo, AudioDriverKind, AudioInputDevice, ChannelRange,
};
use common::process_data::MAX_FRAMES;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

use super::callback::{self, CallbackSlot, OUT_CHANNELS};
use super::input::InputBlock;
use super::monitor::{MonitorFeed, MonitorInput, monitor_pair};
use super::{Change, MetricsCell, OpenCtx, enumerate};
use crate::output_render::StarveProbe;

/// 入力の stream から出力のコールバックへ渡す ring の大きさ (frames)。
const CROSS_RING_FRAMES: usize = 8 * MAX_FRAMES;

/// コールバックが見た大きさ (レイテンシの推定に使う)。0 = まだ来ていない。
#[derive(Default)]
struct Observed {
    /// 出力の最初のコールバックのフレーム数 (WASAPI 共有 + event では endpoint の buffer 全体の大きさ)。
    out_buffer: AtomicU32,
    /// 出力の直近のコールバックのフレーム数。
    out_period: AtomicU32,
    /// 入力のコールバックのフレーム数の最大。
    in_period: AtomicU32,
    /// stream が死んだ (デバイスが抜かれた / 形式が変わった)。
    lost: AtomicBool,
}

pub struct SystemDevice {
    output: cpal::Stream,
    input: Option<cpal::Stream>,
    observed: Arc<Observed>,
    lost_reported: bool,
}

/// 設定の OS 標準のデバイスを開く (まだ鳴らさない)。出力が開けなければ `Err` (人が読む理由)。入力だけ
/// 開けなかったときは出力だけで開き、理由を `fallback_reason` に入れる。
pub fn open(config: &AudioDeviceConfig, ctx: &OpenCtx) -> Result<(SystemDevice, AudioDeviceInfo), String> {
    let host = cpal::default_host();
    let device = match &config.system_output {
        Some(wanted) => enumerate::find_system_device(&host, &wanted.id)
            .ok_or_else(|| format!("出力デバイス「{}」が見つかりません", wanted.name))?,
        None => host.default_output_device().ok_or("出力デバイスがありません")?,
    };
    let name = device_name(&device);
    let supported = device
        .default_output_config()
        .map_err(|e| format!("出力デバイス「{name}」の形式を取得できません: {e}"))?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels();
    if channels == 0 {
        return Err(format!("出力デバイス「{name}」に出力 ch がありません"));
    }
    // 出力する組。mono のデバイスは L だけを出す。
    let first = config.output_first_channel.unwrap_or(0).min(channels.saturating_sub(2));
    let observed = Arc::new(Observed::default());

    let (input, input_note) = match open_input(&host, config, sample_rate, &observed) {
        Ok(input) => (input, None),
        Err(note) => (None, Some(note)),
    };
    let (input_stream, input_rx, input_info) = match input {
        Some(i) => (Some(i.stream), Some(i.rx), Some(i.info)),
        None => (None, None, None),
    };

    let side = OutputSide {
        slot: Arc::clone(&ctx.slot),
        metrics: Arc::clone(&ctx.metrics),
        channels: usize::from(channels),
        first: usize::from(first),
        stereo: vec![0.0; MAX_FRAMES * OUT_CHANNELS],
        input: input_rx,
        starve: StarveProbe::new(),
        observed: Arc::clone(&observed),
    };
    let stream_config = supported.config();
    tracing::info!(device = %name, sample_rate, channels, format = ?supported.sample_format(), "opening output stream");
    let output = for_each_format!(supported.sample_format(), build_output(&device, &stream_config, side, &observed))
        .map_err(|e| format!("出力デバイス「{name}」を開けません: {e}"))?;

    let info = AudioDeviceInfo {
        driver: AudioDriverKind::System,
        output_name: name,
        input_name: input_info.as_ref().map(|i| i.name.clone()),
        sample_rate,
        // WASAPI 共有の既定の周期は 10 ms。動き出したら実際の値に直す (`poll`)。
        buffer_frames: sample_rate / 100,
        output_channels: plain_channels(channels),
        input_channels: input_info.as_ref().map(|i| plain_channels(i.channels)).unwrap_or_default(),
        output_first_channel: first,
        active_inputs: input_info.as_ref().map(|i| i.active).unwrap_or_default(),
        fallback_reason: input_note,
        ..Default::default()
    };
    Ok((SystemDevice { output, input: input_stream, observed, lost_reported: false }, info))
}

impl SystemDevice {
    pub fn set_running(&mut self, run: bool) -> Result<(), String> {
        // cpal 0.17 の WASAPI では play / pause は run thread への要求を積むだけ (失敗は error callback へ)。
        if run {
            if let Some(input) = &self.input {
                input.play().map_err(|e| e.to_string())?;
            }
            self.output.play().map_err(|e| e.to_string())
        } else {
            if let Some(input) = &self.input {
                input.pause().map_err(|e| e.to_string())?;
            }
            self.output.pause().map_err(|e| e.to_string())
        }
    }

    /// 動かして見えた大きさを `info` に写す (変わったら [`Change::Info`])。stream が死んでいたら 1 回だけ
    /// [`Change::Lost`]。
    pub fn poll(&mut self, info: &mut AudioDeviceInfo) -> Change {
        if self.observed.lost.load(Ordering::Acquire) && !self.lost_reported {
            self.lost_reported = true;
            return Change::Lost(format!("「{}」が使えなくなりました", info.output_name));
        }
        let out_buffer = self.observed.out_buffer.load(Ordering::Relaxed);
        let out_period = self.observed.out_period.load(Ordering::Relaxed);
        let in_period = self.observed.in_period.load(Ordering::Relaxed);
        if out_buffer == 0 || (self.input.is_some() && in_period == 0) {
            return Change::None;
        }
        // 出力: 書いた音は endpoint の buffer の後ろに並ぶ。入力: 入力の 1 周期ぶん溜めてから届き、出力の
        // コールバックへ合わせ直す ring でさらに待つ (その待ちの目安 = `cross_slack`)。
        let output_latency = out_buffer;
        let input_latency =
            if self.input.is_some() { in_period + cross_slack(in_period, out_period) as u32 } else { 0 };
        if (info.buffer_frames, info.output_latency_frames, info.input_latency_frames)
            == (out_period, output_latency, input_latency)
        {
            return Change::None;
        }
        info.buffer_frames = out_period;
        info.output_latency_frames = output_latency;
        info.input_latency_frames = input_latency;
        Change::Info
    }
}

/// 入力を出力へ合わせ直す ring に残してよい量の目安 (frames): 入力と出力の 1 周期ずつ。
fn cross_slack(in_period: u32, out_frames: u32) -> usize {
    (in_period + out_frames) as usize
}

fn device_name(device: &cpal::Device) -> String {
    enumerate::system_device_ref(device).map_or_else(|| "<unknown>".to_string(), |d| d.name)
}

/// OS 標準のデバイスは ch に名前を付けない。
fn plain_channels(count: u16) -> Vec<AudioChannelInfo> {
    (0..count).map(|index| AudioChannelInfo { index, name: String::new() }).collect()
}

/// stream の error callback。デバイスが抜かれた / 形式が変わったときは stream が止まるので印を立てる。
fn on_stream_error(observed: &Arc<Observed>, what: &'static str) -> impl FnMut(cpal::StreamError) + Send + 'static {
    let observed = Arc::clone(observed);
    move |err| {
        tracing::error!(?err, "{what} stream error");
        if matches!(err, cpal::StreamError::DeviceNotAvailable | cpal::StreamError::StreamInvalidated) {
            observed.lost.store(true, Ordering::Release);
        }
    }
}

/// sample 形式ごとの型で `$build::<T>(..)` を呼ぶ。
macro_rules! for_each_format {
    ($format:expr, $build:ident ( $($arg:expr),* )) => {
        match $format {
            cpal::SampleFormat::F32 => $build::<f32>($($arg),*),
            cpal::SampleFormat::I16 => $build::<i16>($($arg),*),
            cpal::SampleFormat::I24 => $build::<cpal::I24>($($arg),*),
            cpal::SampleFormat::I32 => $build::<i32>($($arg),*),
            cpal::SampleFormat::U16 => $build::<u16>($($arg),*),
            cpal::SampleFormat::U8 => $build::<u8>($($arg),*),
            cpal::SampleFormat::I8 => $build::<i8>($($arg),*),
            cpal::SampleFormat::F64 => $build::<f64>($($arg),*),
            other => Err(format!("扱えない sample 形式 {other:?}")),
        }
    };
}
use for_each_format;

// ---- 出力 ----

struct OutputSide {
    slot: CallbackSlot,
    metrics: MetricsCell,
    /// デバイスの ch 数 (interleave の刻み)。
    channels: usize,
    /// 出力する組の先頭の ch。
    first: usize,
    /// engine が描く作業場 (stereo interleave、`MAX_FRAMES` フレーム)。
    stereo: Vec<f32>,
    input: Option<MonitorInput>,
    starve: StarveProbe,
    observed: Arc<Observed>,
}

impl OutputSide {
    /// 出力のコールバックの本体。RT: 確保・ロック・I/O をしない。
    fn fill<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T]) {
        let ch = self.channels;
        let frames = data.len() / ch;
        let metrics = self.metrics.get().map(Arc::as_ref);
        if let Some(m) = metrics {
            self.starve.observe(frames, m);
        }
        let _ = self.observed.out_buffer.compare_exchange(0, frames as u32, Ordering::Relaxed, Ordering::Relaxed);
        self.observed.out_period.store(frames as u32, Ordering::Relaxed);
        let in_period = self.observed.in_period.load(Ordering::Relaxed);
        for piece in data.chunks_mut(MAX_FRAMES * ch) {
            let n = piece.len() / ch;
            let stereo = &mut self.stereo[..n * OUT_CHANNELS];
            let (input, stride) = match self.input.as_mut() {
                Some(rx) => {
                    rx.take_unsynced(n, cross_slack(in_period, n as u32), metrics);
                    rx.planar()
                }
                None => (&[][..], 0),
            };
            callback::run(&self.slot, input, stride, stereo, None);
            for (frame, s) in piece.chunks_exact_mut(ch).zip(stereo.as_chunks::<OUT_CHANNELS>().0) {
                frame.fill(T::EQUILIBRIUM);
                frame[self.first] = T::from_sample(s[0]);
                if ch > 1 {
                    frame[self.first + 1] = T::from_sample(s[1]);
                }
            }
        }
    }
}

fn build_output<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut side: OutputSide,
    observed: &Arc<Observed>,
) -> Result<cpal::Stream, String> {
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| side.fill(data),
            on_stream_error(observed, "output"),
            None,
        )
        .map_err(|e| e.to_string())
}

// ---- 入力 ----

struct OpenedInput {
    stream: cpal::Stream,
    rx: MonitorInput,
    info: InputInfo,
}

struct InputInfo {
    name: String,
    channels: u16,
    active: ChannelRange,
}

/// 設定の入力デバイスを開く。`Ok(None)` = 入力を使わない設定。`Err` = 開けなかった理由 (人が読む 1 行)。
fn open_input(
    host: &cpal::Host,
    config: &AudioDeviceConfig,
    output_rate: u32,
    observed: &Arc<Observed>,
) -> Result<Option<OpenedInput>, String> {
    let device = match &config.system_input {
        AudioInputDevice::None => return Ok(None),
        AudioInputDevice::Default => host.default_input_device().ok_or("入力デバイスがありません")?,
        AudioInputDevice::Device(wanted) => enumerate::find_system_device(host, &wanted.id)
            .ok_or_else(|| format!("入力デバイス「{}」が見つかりません", wanted.name))?,
    };
    let name = device_name(&device);
    let supported = device
        .default_input_config()
        .map_err(|e| format!("入力デバイス「{name}」の形式を取得できません: {e}"))?;
    if supported.sample_rate() != output_rate {
        return Err(format!(
            "入力デバイス「{name}」のサンプルレート ({} Hz) が出力 ({output_rate} Hz) と違うので開けません \
             (Windows のサウンド設定で揃えてください)",
            supported.sample_rate()
        ));
    }
    let channels = supported.channels();
    let active = config.input_channels.unwrap_or(ChannelRange { first: 0, count: channels }).clamped(channels);
    if active.count == 0 {
        return Ok(None);
    }
    let (feed, rx) = monitor_pair(active, CROSS_RING_FRAMES);
    let side = InputSide {
        feed,
        channels: usize::from(channels),
        planar: vec![0.0; usize::from(active.count) * MAX_FRAMES],
        observed: Arc::clone(observed),
        frame: 0,
    };
    tracing::info!(device = %name, channels, ?active, format = ?supported.sample_format(), "opening input stream");
    let stream = for_each_format!(supported.sample_format(), build_input(&device, &supported.config(), side, observed))
        .map_err(|e| format!("入力デバイス「{name}」を開けません: {e}"))?;
    Ok(Some(OpenedInput { stream, rx, info: InputInfo { name, channels, active } }))
}

struct InputSide {
    feed: MonitorFeed,
    /// デバイスの ch 数 (interleave の刻み)。
    channels: usize,
    /// 使う ch だけを planar に直す作業場 (`MAX_FRAMES` フレーム)。
    planar: Vec<f32>,
    observed: Arc<Observed>,
    /// 届いたフレームの通し番号 (入力デバイスの時計。出力の時計とは合わせない)。
    frame: u64,
}

impl InputSide {
    /// 入力のコールバックの本体。RT: 確保・ロック・I/O をしない。
    fn push<T: SizedSample>(&mut self, data: &[T])
    where
        f32: FromSample<T>,
    {
        let ch = self.channels;
        let range = self.feed.channels();
        let (first, count) = (usize::from(range.first), usize::from(range.count));
        self.observed.in_period.fetch_max((data.len() / ch) as u32, Ordering::Relaxed);
        for piece in data.chunks(MAX_FRAMES * ch) {
            let n = piece.len() / ch;
            for (f, frame) in piece.chunks_exact(ch).enumerate() {
                for k in 0..count {
                    self.planar[k * MAX_FRAMES + f] = f32::from_sample(frame[first + k]);
                }
            }
            self.feed.push(&InputBlock::new(&self.planar, MAX_FRAMES, n, range, self.frame));
            self.frame += n as u64;
        }
    }
}

fn build_input<T: SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut side: InputSide,
    observed: &Arc<Observed>,
) -> Result<cpal::Stream, String>
where
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| side.push(data),
            on_stream_error(observed, "input"),
            None,
        )
        .map_err(|e| e.to_string())
}
