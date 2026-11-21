//! ASIO のデバイス (`crate::asio` の層の上で、engine の入口 `super::callback` へつなぐ)。
//!
//! 出力はステレオ 1 組 (選んだ先頭 ch から 2 本。出力が 1 本のドライバーは L だけ)、入力は選んだ範囲を
//! 同じコールバックで受ける (時計は 1 つ)。ch ごとの sample 形式は `super::convert` で f32 と行き来する。
//!
//! **作ったスレッド (制御スレッド、STA) の上でだけ触る** (`Driver` が `!Send`)。ドライバー側の変更
//! (`kAsioResetRequest` / `sampleRateDidChange` / `kAsioLatenciesChanged`) はコールバックが数えるだけで、
//! 制御スレッドが [`AsioDevice::poll`] で拾う。

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::audio_device::{AudioChannelInfo, AudioDeviceConfig, AudioDeviceInfo, AudioDriverKind, ChannelRange};
use common::metrics_bridge::MetricsBridgeHandle;

use super::callback::{self, OUT_CHANNELS};
use super::convert::{self, SampleFormat};
use super::{Change, OpenCtx, enumerate};
use crate::asio::driver::Driver;
use crate::asio::registry;
use crate::asio::stream::{Callback, ChannelSpec, StatsSnapshot, Stream};
use crate::asio::sys;

const SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];
/// 設定画面に並べるバッファサイズの数の上限。
const MAX_BUFFER_CHOICES: usize = 32;
/// ドライバーが開き直しを求めてから、求めが止むのを待つ時間 (設定画面の操作 1 回で何度も来るのを 1 回に畳む)。
const SETTLE: Duration = Duration::from_millis(300);
/// 1 回の poll で数える飢えの上限 (壊れた counter で回り続けない)。
const MAX_STARVES_PER_POLL: u32 = 1000;

pub struct AsioDevice {
    // drop は宣言の順: stream (stop → disposeBuffers) → driver (Release)。
    stream: Stream,
    driver: Rc<Driver>,
    seen: StatsSnapshot,
    /// 開き直しを求められてから落ち着くのを待っている (最後に求められた時刻と理由)。
    pending: Option<(Instant, &'static str)>,
}

/// 設定の ASIO ドライバーを開いて buffer を作る (まだ鳴らさない)。開けなければ `Err` (人が読む理由)。
pub fn open(config: &AudioDeviceConfig, ctx: &OpenCtx) -> Result<(AsioDevice, AudioDeviceInfo), String> {
    let wanted = config.asio_driver.as_ref().ok_or("ASIO ドライバーが選ばれていません")?;
    let entry = registry::enumerate()
        .into_iter()
        .find(|d| enumerate::clsid_string(&d.clsid).eq_ignore_ascii_case(&wanted.id))
        .ok_or_else(|| format!("ASIO ドライバー「{}」が見つかりません", wanted.name))?;
    let name = entry.name.clone();
    let fail = |e: crate::asio::driver::Error| format!("ASIO ドライバー「{name}」を開けません: {e}");

    let driver = Rc::new(Driver::open(&entry.clsid, ctx.sys_window).map_err(fail)?);
    let (inputs, outputs) = driver.channels().map_err(fail)?;
    if outputs == 0 {
        return Err(format!("ASIO ドライバー「{name}」に出力 ch がありません"));
    }
    let sample_rates: Vec<u32> =
        SAMPLE_RATES.into_iter().filter(|&r| driver.can_sample_rate(f64::from(r))).collect();
    let sample_rate = choose_sample_rate(&driver, config.sample_rate, &sample_rates).map_err(fail)?;

    // buffer 長の範囲は sample rate で変わるので、rate を決めてから取る (`crate::asio::probe` の実測)。
    let sizes = driver.buffer_sizes().map_err(fail)?;
    let mut buffer_sizes = Vec::new();
    sizes.candidates(&mut buffer_sizes, MAX_BUFFER_CHOICES);
    let frames = config.buffer_frames.filter(|f| buffer_sizes.contains(f)).unwrap_or(sizes.preferred);

    // ch 数は `MAX_CHANNELS` (1024) 以下と確かめてあるので u16 に収まる。
    let (inputs, outputs) = (inputs as u16, outputs as u16);
    let out_count = outputs.min(OUT_CHANNELS as u16);
    let first = config.output_first_channel.unwrap_or(0).min(outputs - out_count);
    let active = config.input_channels.unwrap_or(ChannelRange { first: 0, count: inputs }).clamped(inputs);

    let mut specs = Vec::new();
    let mut formats = Vec::new();
    let used = (0..out_count).map(|k| (false, first + k)).chain((0..active.count).map(|k| (true, active.first + k)));
    for (is_input, channel) in used {
        let info = driver.channel_info(is_input, u32::from(channel)).map_err(fail)?;
        let format = SampleFormat::from_asio(info.sample_type).ok_or_else(|| {
            format!(
                "ASIO ドライバー「{name}」の sample 形式 {} は扱えません",
                sys::sample_type_name(info.sample_type)
            )
        })?;
        specs.push(ChannelSpec { is_input, channel: u32::from(channel) });
        formats.push(format);
    }
    let callback = io_callback(ctx, formats, usize::from(out_count), usize::from(active.count), frames as usize);
    let stream = Stream::create(&driver, &specs, frames, callback).map_err(fail)?;
    // latency は buffer を作った後でないと正しくないドライバーがある (`Driver::latencies`)。
    let (input_latency, output_latency) = driver.latencies().map_err(fail)?;

    let channel_list = |is_input: bool, count: u16| -> Vec<AudioChannelInfo> {
        (0..count)
            .map(|index| AudioChannelInfo {
                index,
                name: driver.channel_info(is_input, u32::from(index)).map(|i| i.name).unwrap_or_default(),
            })
            .collect()
    };
    let info = AudioDeviceInfo {
        driver: AudioDriverKind::Asio,
        output_name: name.clone(),
        input_name: (active.count > 0).then(|| name.clone()),
        sample_rate,
        buffer_frames: frames,
        sample_rates,
        buffer_sizes,
        output_channels: channel_list(false, outputs),
        input_channels: channel_list(true, inputs),
        output_first_channel: first,
        active_inputs: active,
        input_latency_frames: input_latency,
        output_latency_frames: output_latency,
        has_control_panel: true,
        fallback_reason: None,
    };
    tracing::info!(
        driver = %name, sample_rate, frames, first, ?active, input_latency, output_latency,
        output_ready = stream.posts_output(), "ASIO stream created"
    );
    let seen = stream.stats();
    Ok((AsioDevice { stream, driver, seen, pending: None }, info))
}

/// 望みのレート (ドライバーが通すものだけ) を入れる。望みが無ければ今のレート。ドライバーがまだレートを
/// 持っていなければ 48 kHz、無ければ最初に通るレートを入れる (`Driver::sample_rate` の doc)。
fn choose_sample_rate(
    driver: &Driver,
    wanted: Option<u32>,
    supported: &[u32],
) -> Result<u32, crate::asio::driver::Error> {
    let current = driver.sample_rate()?.map(|r| r.round() as u32);
    let target = wanted
        .filter(|r| supported.contains(r))
        .or(current)
        .or_else(|| supported.iter().copied().find(|&r| r == 48_000))
        .or_else(|| supported.first().copied())
        .ok_or_else(|| crate::asio::driver::Error::Invalid("no usable sample rate".into()))?;
    if current != Some(target) {
        driver.set_sample_rate(f64::from(target))?;
    }
    // 入れた値をドライバーが丸めることがあるので、読み直した値を使う。
    Ok(driver.sample_rate()?.map_or(target, |r| r.round() as u32))
}

/// buffer 1 枚ごとの処理: 入力を f32 (planar) に直し、engine に描かせ、出力をデバイスの形式で書く。
/// `formats` は有効にした ch の順 (出力 `out_count` 本 → 入力 `in_count` 本)。RT: 確保・ロック・I/O をしない。
fn io_callback(
    ctx: &OpenCtx,
    formats: Vec<SampleFormat>,
    out_count: usize,
    in_count: usize,
    frames: usize,
) -> Callback {
    let slot = Arc::clone(&ctx.slot);
    let mut input = vec![0.0f32; in_count * frames];
    let mut stereo = vec![0.0f32; frames * OUT_CHANNELS];
    Box::new(move |buffers, _time| {
        // buffer 長は `createBuffers` に渡した `frames` で固定 (作業場はその大きさ)。
        let n = buffers.frames().min(frames);
        for k in 0..in_count {
            if let Some(src) = buffers.input(out_count + k) {
                convert::decode(formats[out_count + k], src, &mut input[k * frames..][..n]);
            }
        }
        let stereo = &mut stereo[..n * OUT_CHANNELS];
        callback::run(&slot, &input, frames, stereo, None);
        for side in 0..out_count {
            if let Some(dst) = buffers.output(side) {
                convert::encode(formats[side], &stereo[side..], OUT_CHANNELS, dst);
            }
        }
    })
}

impl AsioDevice {
    pub fn set_running(&mut self, run: bool) -> Result<(), String> {
        if run == self.stream.is_running() {
            return Ok(());
        }
        let result = if run { self.stream.start() } else { self.stream.stop() };
        result.map_err(|e| e.to_string())
    }

    pub fn control_panel(&self) -> Result<(), String> {
        self.driver.control_panel().map_err(|e| e.to_string())
    }

    /// コールバックが数えた「ドライバー側の変更」を拾う。開き直しの求めは [`SETTLE`] の間止むのを待ってから
    /// [`Change::Reopen`] で返す。latency だけが変わったなら `info` を直して [`Change::Info`]。
    pub fn poll(&mut self, info: &mut AudioDeviceInfo, metrics: Option<&MetricsBridgeHandle>) -> Change {
        let now = self.stream.stats();
        let seen = std::mem::replace(&mut self.seen, now);
        if let Some(m) = metrics {
            // `kAsioOverload` = ドライバーが処理の遅れを見た回。
            for _ in 0..now.overloads.wrapping_sub(seen.overloads).min(MAX_STARVES_PER_POLL) {
                m.add_starve();
            }
        }
        if now.sample_rate_changes != seen.sample_rate_changes {
            self.pending = Some((Instant::now(), "ドライバーのサンプルレートが変わりました"));
        } else if now.reset_requests != seen.reset_requests {
            self.pending = Some((Instant::now(), "ドライバーの設定が変わりました"));
        }
        match self.pending {
            Some((since, reason)) if since.elapsed() >= SETTLE => {
                self.pending = None;
                return Change::Reopen(reason.to_string());
            }
            Some(_) => return Change::None,
            None => {}
        }
        if now.latency_changes != seen.latency_changes
            && let Ok((input, output)) = self.driver.latencies()
            && (input, output) != (info.input_latency_frames, info.output_latency_frames)
        {
            info.input_latency_frames = input;
            info.output_latency_frames = output;
            return Change::Info;
        }
        Change::None
    }
}
