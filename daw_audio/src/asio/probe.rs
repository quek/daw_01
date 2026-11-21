//! `daw_audio --asio-probe [ドライバー名]`: ASIO 層がこの PC で動くことの実測。
//!
//! - 引数なし: registry の列挙だけ (ドライバーは load しない)。
//! - 名前あり: そのドライバーを開いて申告値を全部出し、出力 2 ch の buffer を作って約 1 秒、
//!   **無音だけ**を書きながら callback を数える。 音は出さない。
//!
//! IPC を張る前に分岐して終わる (親プロセスは要らない)。 出力は stdout なので、console の無い
//! release ビルドでは redirect して読む。
//!
//! ASIO ドライバーの多くはデバイスを排他で握る。 **他のアプリが使っている最中のドライバーを
//! 開くと、そのアプリの音が止まることがある**。

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetDesktopWindow, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};

use super::driver::{BufferSizes, ComApartment, Driver, Error};
use super::registry::{self, DriverEntry};
use super::stream::{Callback, ChannelSpec, Stream};
use super::sys;

/// probe が buffer を回す長さ。
const RUN_TIME: Duration = Duration::from_secs(1);
/// ch の一覧を出す上限 (片方向あたり)。
const MAX_LISTED_CHANNELS: u32 = 32;
const SAMPLE_RATES: [f64; 6] = [44_100.0, 48_000.0, 88_200.0, 96_000.0, 176_400.0, 192_000.0];

/// 引数が `--asio-probe` なら probe を走らせて終了コードを返す。 違えば `None`。
pub fn run_from_args() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--asio-probe") {
        return None;
    }
    let target = args.next();
    // COM の STA を持つ専用スレッドで動かす (tokio の worker を STA にしない)。
    let worker = std::thread::Builder::new().name("asio-probe".into()).spawn(move || run(target.as_deref()));
    Some(match worker.map(std::thread::JoinHandle::join) {
        Ok(Ok(code)) => code,
        _ => 1,
    })
}

fn run(target: Option<&str>) -> i32 {
    let drivers = registry::enumerate();
    println!("ASIO drivers: {}", drivers.len());
    for (i, d) in drivers.iter().enumerate() {
        println!("  [{i}] name={:?} key={:?} clsid={:?} dll={:?}", d.name, d.key, d.clsid, d.dll_path);
    }
    let Some(target) = target else { return 0 };
    let Some(entry) = drivers
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(target) || d.key.eq_ignore_ascii_case(target))
    else {
        println!("driver {target:?} not found");
        return 2;
    };
    match probe_driver(entry) {
        Ok(()) => 0,
        Err(err) => {
            println!("probe failed: {err}");
            1
        }
    }
}

fn probe_driver(entry: &DriverEntry) -> Result<(), Error> {
    // drop は逆順: stream → driver (Release) → apartment。
    let _apartment = ComApartment::enter()?;
    // SAFETY: 引数なしの問い合わせ。
    let control_thread = unsafe { GetCurrentThreadId() };
    // `daw_audio` は窓を持たない。 `sysRef` には desktop 窓を渡す (PortAudio の既定と同じ)。
    // SAFETY: 引数なしの問い合わせ。
    let hwnd = unsafe { GetDesktopWindow() };
    let driver = Rc::new(Driver::open(&entry.clsid, hwnd.0)?);
    println!("opened {:?} on thread {control_thread}", entry.name);
    println!("  driver name: {:?}  version: {}", driver.name(), driver.version());
    let (inputs, outputs) = driver.channels()?;
    println!("  channels: {inputs} in / {outputs} out");
    print_buffer_sizes("as opened", &driver.buffer_sizes()?);
    let supported: Vec<f64> = SAMPLE_RATES.into_iter().filter(|&r| driver.can_sample_rate(r)).collect();
    let rate = match driver.sample_rate()? {
        Some(rate) => {
            println!("  sample rate: {rate}  supported: {supported:?}");
            rate
        }
        None => {
            // まだ決まっていない (`Driver::sample_rate` の doc)。 48 kHz、無ければ最初に通る rate を入れる。
            let pick = supported.iter().copied().find(|&r| (r - 48_000.0).abs() < 0.5).or(supported.first().copied());
            println!("  sample rate: not set  supported: {supported:?}");
            let Some(pick) = pick else {
                println!("  no usable sample rate; stopping here");
                return Ok(());
            };
            driver.set_sample_rate(pick)?;
            let rate = driver.sample_rate()?.unwrap_or(pick);
            println!("  setSampleRate({pick}) -> {rate}");
            rate
        }
    };
    // **buffer 長の範囲は sample rate で変わる** (この PC の "Realtek ASIO" の初回: rate を入れる前は
    // 1024、48 kHz を入れた後は 144。 ドライバーは設定を覚えていて、次に開くと最初から 144)。
    // rate を決めてから取り直した値で buffer を作る。
    let sizes = driver.buffer_sizes()?;
    print_buffer_sizes("at this sample rate", &sizes);
    print_latencies("before createBuffers", &driver);
    for (is_input, count) in [(true, inputs), (false, outputs)] {
        for channel in 0..count.min(MAX_LISTED_CHANNELS) {
            let info = driver.channel_info(is_input, channel)?;
            println!(
                "  {} {channel}: {:?} group {} type {} ({}) active {}",
                if is_input { "in " } else { "out" },
                info.name,
                info.group,
                sys::sample_type_name(info.sample_type),
                info.sample_type,
                info.active
            );
        }
    }
    if outputs == 0 {
        println!("  no outputs; skipping the stream run");
        return Ok(());
    }
    run_stream(&driver, outputs.min(2), sizes.preferred, rate, control_thread)
}

fn print_buffer_sizes(when: &str, sizes: &BufferSizes) {
    let mut candidates = Vec::new();
    sizes.candidates(&mut candidates, 64);
    println!(
        "  buffer size ({when}): min {} / max {} / preferred {} / granularity {}  -> {candidates:?}",
        sizes.min, sizes.max, sizes.preferred, sizes.granularity
    );
}

fn print_latencies(when: &str, driver: &Driver) {
    match driver.latencies() {
        Ok((input, output)) => println!("  latency ({when}): {input} in / {output} out frames"),
        Err(err) => println!("  latency ({when}): {err}"),
    }
}

fn run_stream(
    driver: &Rc<Driver>,
    outputs: u32,
    frames: u32,
    rate: f64,
    control_thread: u32,
) -> Result<(), Error> {
    let channels: Vec<ChannelSpec> =
        (0..outputs).map(|channel| ChannelSpec { is_input: false, channel }).collect();
    let written = Arc::new(AtomicU64::new(0));
    let first_time = Arc::new(AtomicU64::new(0));
    let last_time = Arc::new(AtomicU64::new(0));
    let callback: Callback = {
        let (written, first_time, last_time) =
            (Arc::clone(&written), Arc::clone(&first_time), Arc::clone(&last_time));
        // RT: 無音で埋めて数えるだけ (確保・ロック・I/O なし)。
        Box::new(move |buffers, time| {
            for index in 0..buffers.channels().len() {
                if let Some(out) = buffers.output(index) {
                    out.fill(0);
                }
            }
            written.fetch_add(buffers.frames() as u64, Ordering::Relaxed);
            if let Some(time) = time {
                let _ = first_time.compare_exchange(0, time.system_time_ns, Ordering::Relaxed, Ordering::Relaxed);
                last_time.store(time.system_time_ns, Ordering::Relaxed);
            }
        })
    };
    let mut stream = Stream::create(driver, &channels, frames, callback)?;
    println!("  createBuffers: {outputs} out x {frames} frames  outputReady: {}", stream.posts_output());
    print_latencies("after createBuffers", driver);
    for spec in &channels {
        let info = driver.channel_info(spec.is_input, spec.channel)?;
        println!("  out {}: active {}", spec.channel, info.active);
    }
    let started = Instant::now();
    stream.start()?;
    pump_messages_for(RUN_TIME);
    stream.stop()?;
    let elapsed = started.elapsed();
    let stats = stream.stats();
    let expected = rate * elapsed.as_secs_f64() / f64::from(frames);
    println!("  ran {:.3} s: {} buffer switches (expected about {expected:.1})", elapsed.as_secs_f64(), stats.switches);
    println!(
        "  frames written: {}  time-info switches: {}  last sample position: {}",
        written.load(Ordering::Relaxed),
        stats.time_info_switches,
        stats.last_sample_position
    );
    let span_ns = last_time.load(Ordering::Relaxed).saturating_sub(first_time.load(Ordering::Relaxed));
    println!("  driver system time span: {:.3} s", Duration::from_nanos(span_ns).as_secs_f64());
    println!(
        "  callback thread: {} (control thread {control_thread}, changes {})",
        stats.callback_thread, stats.thread_changes
    );
    println!(
        "  reentered {} / bad index {} / reset {} / resync {} / latency changes {} / overloads {} / rate changes {}",
        stats.reentered,
        stats.bad_index,
        stats.reset_requests,
        stats.resync_requests,
        stats.latency_changes,
        stats.overloads,
        stats.sample_rate_changes
    );
    drop(stream);
    println!("  stopped, buffers disposed");
    Ok(())
}

/// このスレッドの message を `duration` の間回す (STA の決まり。 `super` の module doc)。
fn pump_messages_for(duration: Duration) {
    let deadline = Instant::now() + duration;
    let mut message = MSG::default();
    while Instant::now() < deadline {
        // SAFETY: `message` は有効な出力先。 このスレッド宛ての message だけを取り出して配る。
        unsafe {
            while PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
