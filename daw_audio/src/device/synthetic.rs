//! 合成のデバイス (実機を開かない検査用。設定画面には出さない)。
//!
//! 専用のスレッドが、決まった周期でコールバックを呼ぶ。入力は id ごとに決まる
//! (`common::audio_device::synthetic`):
//!
//! - `RAMP`: 入力 ch と通し番号から決まる波形。レイテンシの申告は 0。
//! - `LOOPBACK`: 入力 ch 0 / 1 = 出力 L / R (`mute_output` で消す前) を「入力レイテンシ + 出力レイテンシ」
//!   フレーム遅らせたもの。出力レイテンシは実機と同じくバッファ 1 枚ぶんを含む (同じコールバックで出した音が
//!   そのコールバックの入力に戻ることは無い)。
//! - [`NULL`]: 入力なし。出力デバイスが 1 つも開けないときの最後の受け皿 (engine を動かし続ける)。
//!
//! 時計は「呼んだフレーム数」。スレッドが遅れても後からまとめて呼ばない (先読みの描き溜めを一気に空に
//! しない) ので、壁時計より遅れることはあるが、フレームは飛ばない。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::audio_device::{
    AudioChannelInfo, AudioDeviceConfig, AudioDeviceInfo, AudioDriverKind, ChannelRange, synthetic,
};

use super::callback::{self, CallbackSlot, OUT_CHANNELS};
use super::{Change, OpenCtx};

/// 入力の無い合成デバイス (設定からは選べない)。
pub const NULL: &str = "null";

/// ドライバー起点の開き直し (ASIO の reset 要求) を通す検査用。入力は `RAMP` と同じで、動き出した
/// [`REOPEN_AFTER`] 後に「開き直し」を 1 回だけ求める (プロセスにつき 1 回)。開いた回数が名前に入るので、
/// 開き直しが起きたことをデバイス情報から読める。
pub const REOPEN: &str = "reopen";
const REOPEN_AFTER: Duration = Duration::from_secs(1);
/// このプロセスで [`REOPEN`] を開いた回数。
static REOPEN_OPENS: AtomicU32 = AtomicU32::new(0);

const SAMPLE_RATES: [u32; 3] = [44_100, 48_000, 96_000];
/// 2048 は「デバイスの 1 回が `MAX_FRAMES` を超える」場合の検査用。
const BUFFER_SIZES: [u32; 6] = [64, 128, 256, 512, 1024, 2048];
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_BUFFER_FRAMES: u32 = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ramp,
    Loopback,
    Null,
}

struct Shared {
    run: AtomicBool,
    stop: AtomicBool,
}

pub struct SyntheticDevice {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    /// [`REOPEN`]: 動き出したら開き直しを求める (まだ求めていない)。
    asks_reopen: bool,
    /// 開き直しを求める時刻。
    reopen_at: Option<Instant>,
}

/// 設定の合成デバイスを開く (まだ呼ばない)。
pub fn open(config: &AudioDeviceConfig, ctx: &OpenCtx) -> Result<(SyntheticDevice, AudioDeviceInfo), String> {
    let id = config.synthetic_device.as_deref().unwrap_or(synthetic::RAMP);
    let mut name = format!("Synthetic ({id})");
    let mut asks_reopen = false;
    let mode = match id {
        synthetic::RAMP => Mode::Ramp,
        synthetic::LOOPBACK => Mode::Loopback,
        NULL => Mode::Null,
        REOPEN => {
            let opens = REOPEN_OPENS.fetch_add(1, Ordering::Relaxed) + 1;
            name = format!("Synthetic ({id} #{opens})");
            asks_reopen = opens == 1;
            Mode::Ramp
        }
        other => return Err(format!("合成デバイス「{other}」はありません")),
    };
    let sample_rate = config.sample_rate.filter(|r| SAMPLE_RATES.contains(r)).unwrap_or(DEFAULT_SAMPLE_RATE);
    let frames = config.buffer_frames.filter(|f| BUFFER_SIZES.contains(f)).unwrap_or(DEFAULT_BUFFER_FRAMES);
    let inputs = if mode == Mode::Null { 0 } else { synthetic::INPUT_CHANNELS };
    let active = config.input_channels.unwrap_or(ChannelRange { first: 0, count: inputs }).clamped(inputs);
    let (input_latency, output_latency) = match mode {
        Mode::Loopback => (synthetic::LOOPBACK_INPUT_LATENCY, synthetic::LOOPBACK_OUTPUT_LATENCY + frames),
        Mode::Ramp | Mode::Null => (0, 0),
    };
    let info = AudioDeviceInfo {
        driver: AudioDriverKind::Synthetic,
        input_name: (active.count > 0).then(|| name.clone()),
        output_name: name,
        sample_rate,
        buffer_frames: frames,
        sample_rates: SAMPLE_RATES.to_vec(),
        buffer_sizes: BUFFER_SIZES.to_vec(),
        output_channels: named_channels(OUT_CHANNELS as u16, "out"),
        input_channels: named_channels(inputs, "in"),
        output_first_channel: 0,
        active_inputs: active,
        input_latency_frames: input_latency,
        output_latency_frames: output_latency,
        has_control_panel: false,
        fallback_reason: None,
    };
    let shared = Arc::new(Shared { run: AtomicBool::new(false), stop: AtomicBool::new(false) });
    let mut clock = Clock {
        slot: Arc::clone(&ctx.slot),
        shared: Arc::clone(&shared),
        mode,
        frames: frames as usize,
        period: Duration::from_secs_f64(f64::from(frames) / f64::from(sample_rate)),
        active,
        input: vec![0.0; usize::from(active.count) * frames as usize],
        out: vec![0.0; frames as usize * OUT_CHANNELS],
        pre_mute: vec![0.0; frames as usize * OUT_CHANNELS],
        line: vec![0.0; (input_latency + output_latency) as usize * OUT_CHANNELS],
        frame: 0,
    };
    let thread = std::thread::Builder::new()
        .name("synthetic audio device".into())
        .spawn(move || clock.run())
        .map_err(|e| format!("合成デバイスのスレッドを作れません: {e}"))?;
    Ok((SyntheticDevice { shared, thread: Some(thread), asks_reopen, reopen_at: None }, info))
}

fn named_channels(count: u16, prefix: &str) -> Vec<AudioChannelInfo> {
    (0..count).map(|index| AudioChannelInfo { index, name: format!("{prefix} {}", index + 1) }).collect()
}

impl SyntheticDevice {
    /// コールバックを呼ぶスレッドの無いデバイス (スレッドを作れなかったときの置き物)。
    pub fn dead() -> Self {
        Self {
            shared: Arc::new(Shared { run: AtomicBool::new(false), stop: AtomicBool::new(true) }),
            thread: None,
            asks_reopen: false,
            reopen_at: None,
        }
    }

    pub fn set_running(&mut self, run: bool) {
        self.shared.run.store(run, Ordering::Release);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
        if run && std::mem::take(&mut self.asks_reopen) {
            self.reopen_at = Some(Instant::now() + REOPEN_AFTER);
        }
    }

    /// デバイス側の変化 ([`REOPEN`] の開き直しの要求だけ)。
    pub fn poll(&mut self) -> Change {
        match self.reopen_at {
            Some(at) if Instant::now() >= at => {
                self.reopen_at = None;
                Change::Reopen("合成デバイスが開き直しを求めました".to_string())
            }
            _ => Change::None,
        }
    }
}

impl Drop for SyntheticDevice {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            if t.join().is_err() {
                tracing::error!("synthetic audio device thread panicked");
            }
        }
    }
}

/// コールバックを呼ぶスレッドの持ち物 (作業場は開くときに確保)。
struct Clock {
    slot: CallbackSlot,
    shared: Arc<Shared>,
    mode: Mode,
    frames: usize,
    period: Duration,
    active: ChannelRange,
    /// 入力 (planar、使っている ch × `frames`)。
    input: Vec<f32>,
    out: Vec<f32>,
    pre_mute: Vec<f32>,
    /// `LOOPBACK` の遅延線 (stereo interleave、遅らせるフレーム数ぶんの輪)。
    line: Vec<f32>,
    /// engine が受け取ったフレームの通し番号。
    frame: u64,
}

impl Clock {
    fn run(&mut self) {
        let mut next = Instant::now();
        while !self.shared.stop.load(Ordering::Acquire) {
            if !self.shared.run.load(Ordering::Acquire) {
                // 止まっている間は寝る (起こすのは `set_running` と drop)。
                std::thread::park_timeout(Duration::from_millis(100));
                next = Instant::now();
                continue;
            }
            self.callback();
            next += self.period;
            match next.checked_duration_since(Instant::now()) {
                Some(wait) => std::thread::sleep(wait),
                // 遅れた分は取り戻さない (module doc)。
                None => next = Instant::now(),
            }
        }
    }

    fn callback(&mut self) {
        let n = self.frames;
        match self.mode {
            Mode::Ramp => {
                for k in 0..self.active.count {
                    let ch = self.active.first + k;
                    let dst = &mut self.input[usize::from(k) * n..][..n];
                    for (i, s) in dst.iter_mut().enumerate() {
                        *s = synthetic::ramp_sample(ch, self.frame + i as u64);
                    }
                }
            }
            Mode::Loopback => self.read_loopback(),
            Mode::Null => {}
        }
        let ran = callback::run(&self.slot, &self.input, n, &mut self.out, Some(&mut self.pre_mute));
        if !ran {
            // engine が受け取っていない回は数えない (通し番号を engine の時計と揃えておく)。
            return;
        }
        if self.mode == Mode::Loopback {
            self.write_loopback();
        }
        self.frame += n as u64;
    }

    /// 入力 ch 0 / 1 に「遅延線の長さ」フレーム前の出力 L / R を置く (それより前は無音)。他の ch は無音。
    fn read_loopback(&mut self) {
        let n = self.frames;
        let delay = self.line.len() / OUT_CHANNELS;
        self.input.fill(0.0);
        for side in 0..OUT_CHANNELS as u16 {
            if !self.active.contains(side) {
                continue;
            }
            let dst = &mut self.input[usize::from(side - self.active.first) * n..][..n];
            for (i, s) in dst.iter_mut().enumerate() {
                let at = self.frame + i as u64;
                // 輪の同じ位置に入っているのは、ちょうど `delay` フレーム前の出力 (`write_loopback`)。
                if at >= delay as u64 {
                    *s = self.line[(at % delay as u64) as usize * OUT_CHANNELS + usize::from(side)];
                }
            }
        }
    }

    /// いま出した出力を遅延線へ (1 回のフレーム数は遅延線より短い: 出力レイテンシがバッファ 1 枚ぶんを含む)。
    fn write_loopback(&mut self) {
        let delay = self.line.len() / OUT_CHANNELS;
        for (i, frame) in self.pre_mute.as_chunks::<OUT_CHANNELS>().0.iter().enumerate() {
            let at = ((self.frame + i as u64) % delay as u64) as usize * OUT_CHANNELS;
            self.line[at..at + OUT_CHANNELS].copy_from_slice(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};

    use super::*;
    use crate::device::callback::DeviceCallback;

    /// engine の代わりに置く検査用のコールバック: 届いた入力を記録し、通し番号から決まる音を出す。
    struct Probe {
        log: Arc<Mutex<Log>>,
        frame: u64,
    }

    #[derive(Default)]
    struct Log {
        /// 使っている入力 ch ごとに、届いた順。
        input: Vec<Vec<f32>>,
        frames: u64,
    }

    /// 出力 L のフレーム `t` の値 (0 にならない。R は符号を反転)。
    fn signal(t: u64) -> f32 {
        ((t % 997) + 1) as f32 / 1000.0
    }

    impl DeviceCallback for Probe {
        fn process(&mut self, input: &[f32], stride: usize, out: &mut [f32], pre_mute: Option<&mut [f32]>) {
            let frames = out.len() / OUT_CHANNELS;
            let mut log = self.log.lock().unwrap();
            for (k, ch) in log.input.iter_mut().enumerate() {
                ch.extend_from_slice(&input[k * stride..][..frames]);
            }
            log.frames += frames as u64;
            for (i, f) in out.as_chunks_mut::<OUT_CHANNELS>().0.iter_mut().enumerate() {
                let s = signal(self.frame + i as u64);
                *f = [s, -s];
            }
            pre_mute.unwrap().copy_from_slice(out);
            // `mute_output` と同じく、デバイスへ渡す音は消す (loopback に戻るのは消す前の音)。
            out.fill(0.0);
            self.frame += frames as u64;
        }
    }

    /// 合成デバイスを開いて `frames` フレームぶん回し、検査用のコールバックに届いた入力と、デバイス情報を返す。
    fn run_device(config: &AudioDeviceConfig, frames: u64) -> (Vec<Vec<f32>>, AudioDeviceInfo) {
        let ctx = OpenCtx {
            slot: Arc::new(Mutex::new(None)),
            metrics: Arc::new(OnceLock::new()),
            sys_window: std::ptr::null_mut(),
        };
        let (mut device, info) = open(config, &ctx).unwrap();
        let log = Arc::new(Mutex::new(Log {
            input: vec![Vec::new(); usize::from(info.active_inputs.count)],
            frames: 0,
        }));
        *ctx.slot.lock().unwrap() = Some(Box::new(Probe { log: Arc::clone(&log), frame: 0 }));
        device.set_running(true);
        let deadline = Instant::now() + Duration::from_secs(10);
        while log.lock().unwrap().frames < frames {
            assert!(Instant::now() < deadline, "合成デバイスがコールバックを呼ばない");
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(device);
        let input = std::mem::take(&mut log.lock().unwrap().input);
        (input, info)
    }

    /// `RAMP`: コールバックに届く入力は、デバイスの ch 番号と通し番号から決まる値 (開いた範囲の ch だけが届く)。
    #[test]
    fn ramp_reaches_the_callback_by_device_channel_and_frame() {
        let config = AudioDeviceConfig {
            driver: AudioDriverKind::Synthetic,
            synthetic_device: Some(synthetic::RAMP.to_string()),
            buffer_frames: Some(64),
            input_channels: Some(ChannelRange { first: 1, count: 2 }),
            ..Default::default()
        };
        let (input, info) = run_device(&config, 640);
        assert_eq!(info.active_inputs, ChannelRange { first: 1, count: 2 });
        assert_eq!((info.input_latency_frames, info.output_latency_frames), (0, 0));
        assert_eq!(input.len(), 2);
        for (k, ch) in input.iter().enumerate() {
            assert!(ch.len() >= 640);
            for (i, &s) in ch.iter().enumerate() {
                assert_eq!(s, synthetic::ramp_sample(1 + k as u16, i as u64), "ch {} frame {i}", 1 + k);
            }
        }
    }

    /// `LOOPBACK`: 出力のフレーム F は、入力のフレーム `F + 入力レイテンシ + 出力レイテンシ` に 1 サンプルも
    /// ずれずに戻る (`mute_output` で消す前の音。それより前の入力は無音)。
    #[test]
    fn loopback_returns_the_output_after_the_reported_latency() {
        let config = AudioDeviceConfig {
            driver: AudioDriverKind::Synthetic,
            synthetic_device: Some(synthetic::LOOPBACK.to_string()),
            ..Default::default()
        };
        let (input, info) = run_device(&config, 2048);
        let delay = u64::from(info.input_latency_frames + info.output_latency_frames);
        assert_eq!(delay, u64::from(synthetic::LOOPBACK_INPUT_LATENCY + synthetic::LOOPBACK_OUTPUT_LATENCY + 128));
        assert_eq!(input.len(), usize::from(synthetic::INPUT_CHANNELS));
        for (i, (&l, &r)) in input[0].iter().zip(&input[1]).enumerate() {
            let want = if (i as u64) < delay { 0.0 } else { signal(i as u64 - delay) };
            assert_eq!((l, r), (want, -want), "frame {i}");
        }
        assert!(input[2..].iter().all(|ch| ch.iter().all(|&s| s == 0.0)), "loopback は ch 0 / 1 だけ");
    }
}
