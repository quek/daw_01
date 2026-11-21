//! ASIO の buffer (`createBuffers`) と callback の受け口。
//!
//! **ASIO の callback は user data を持たない** (`bufferSwitch(long index, ASIOBool direct)`、
//! `asio.h:400-433`)。 だから動いている stream の状態は static ([`ACTIVE`]) で引くしかなく、
//! **プロセス内で同時に動かせる stream は 1 本** ([`CLAIMED`])。
//!
//! # 状態の寿命
//!
//! callback はドライバーのスレッドから来る。 `stop` から戻った後は呼ばれない決まり
//! (`asio.h` の `ASIOStop` の Notes) だが、それに頼って状態を解放すると、決まりを破るドライバーで
//! use-after-free になる。 そこで callback は**入口で [`PINS`] を増やしてから** [`ACTIVE`] を読み、
//! 片付けは **[`ACTIVE`] を null にしてから** [`PINS`] が 0 になるのを待つ (どちらも `SeqCst`)。
//! null を見た callback は何も触らず、非 null を見た callback は片付けより先に [`PINS`] を
//! 増やしているので、片付けはその callback が抜けるまで解放しない。 待ちは有界で、抜けなかったら
//! 状態は解放せず (leak)、[`CLAIMED`] も返さない (このプロセスでは開き直せなくなるが、壊れない)。
//!
//! # callback の中 (RT)
//!
//! 確保・ロック・I/O をしない。 数えるものは atomic、利用側の処理は確保済みの `Box<dyn FnMut>` を
//! 呼ぶだけ。 `bufferSwitch` は再入しうる (`asio.h` の `ASIOGetLatencies` の説明に "possibly
//! recursively") ので、処理中に来た分は数えるだけで捨てる (`FnMut` の排他)。

use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::System::Threading::GetCurrentThreadId;

use super::driver::{Driver, Error};
use super::sys;

/// 1 本の stream で有効にできる ch 数の上限。
pub const MAX_STREAM_CHANNELS: usize = 256;
/// 片付けで callback が抜けるのを待つ上限。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// 有効にする ch 1 本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelSpec {
    pub is_input: bool,
    pub channel: u32,
}

/// 有効にした ch 1 本の buffer (ドライバー所有の 2 面)。
pub struct ChannelBuffer {
    pub is_input: bool,
    #[expect(dead_code, reason = "読み手は sample type 変換 (r.md #156 の次の段) で入る")]
    pub channel: u32,
    #[expect(dead_code, reason = "読み手は sample type 変換 (r.md #156 の次の段) で入る")]
    pub sample_type: sys::AsioSampleType,
    pub bytes_per_sample: usize,
    halves: [NonNull<u8>; 2],
}

/// callback 1 回ぶんの buffer (今の面)。 中身はドライバーの sample type のままの byte 列。
pub struct IoBuffers<'a> {
    half: usize,
    frames: usize,
    channels: &'a [ChannelBuffer],
}

impl IoBuffers<'_> {
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// 有効にした ch (`Stream::create` に渡した順)。
    pub fn channels(&self) -> &[ChannelBuffer] {
        self.channels
    }

    /// `index` 番目が入力 ch なら、届いた sample。
    #[expect(dead_code, reason = "呼び手は入力の録音 (r.md #156 の次の段) で入る")]
    pub fn input(&self, index: usize) -> Option<&[u8]> {
        let ch = self.channels.get(index).filter(|ch| ch.is_input)?;
        // SAFETY: `Stream::create` が null でないことと長さ (`frames * bytes_per_sample` が溢れない
        // こと) を確かめた、ドライバー所有の buffer。 この面は callback の間ホストのもの。
        Some(unsafe { std::slice::from_raw_parts(ch.halves[self.half].as_ptr(), self.frames * ch.bytes_per_sample) })
    }

    /// `index` 番目が出力 ch なら、書き込む先。
    pub fn output(&mut self, index: usize) -> Option<&mut [u8]> {
        let ch = self.channels.get(index).filter(|ch| !ch.is_input)?;
        // SAFETY: `input` と同じ。 `&mut self` なので同じ ch の slice は同時に 1 つ。
        Some(unsafe {
            std::slice::from_raw_parts_mut(ch.halves[self.half].as_ptr(), self.frames * ch.bytes_per_sample)
        })
    }
}

/// buffer の時刻。 `bufferSwitchTimeInfo` が運んだ値か、それを使わないドライバーなら
/// `getSamplePosition` の値 (どちらもドライバーが有効と答えたときだけ)。
#[derive(Clone, Copy, Debug)]
pub struct SwitchTime {
    /// この buffer の先頭の sample 位置 (`start` からの通し番号)。
    pub sample_position: u64,
    /// その位置に対応する system time (ns)。
    pub system_time_ns: u64,
}

/// buffer 1 枚ごとに呼ばれる処理。 **RT**: 確保・ロック・I/O をしないこと。
pub type Callback = Box<dyn FnMut(&mut IoBuffers<'_>, Option<SwitchTime>) + Send>;

/// callback が数えるもの (読むのは制御スレッド)。
#[derive(Default)]
struct Stats {
    switches: AtomicU64,
    time_info_switches: AtomicU64,
    reentered: AtomicU64,
    bad_index: AtomicU64,
    reset_requests: AtomicU32,
    resync_requests: AtomicU32,
    latency_changes: AtomicU32,
    overloads: AtomicU32,
    sample_rate_changes: AtomicU32,
    callback_thread: AtomicU32,
    thread_changes: AtomicU32,
    last_sample_position: AtomicU64,
}

/// [`Stats`] を読んだ値。
#[derive(Clone, Copy, Debug, Default)]
pub struct StatsSnapshot {
    /// 処理した buffer の枚数。
    pub switches: u64,
    /// そのうち `bufferSwitchTimeInfo` で来た枚数。
    pub time_info_switches: u64,
    /// 処理中に再入してきて捨てた回数。
    pub reentered: u64,
    /// 面の番号が 0 / 1 でなかった回数。
    pub bad_index: u64,
    /// `kAsioResetRequest` (ドライバーを畳んで開き直せ) の回数。
    pub reset_requests: u32,
    pub resync_requests: u32,
    pub latency_changes: u32,
    pub overloads: u32,
    pub sample_rate_changes: u32,
    /// callback が来たスレッドの id (0 = まだ来ていない)。
    pub callback_thread: u32,
    /// callback のスレッドが途中で変わった回数。
    pub thread_changes: u32,
    pub last_sample_position: u64,
}

struct State {
    /// `outputReady` を callback のスレッドから呼ぶための object (寿命は `Stream` の `&Driver`)。
    driver: *mut sys::IAsio,
    post_output: bool,
    frames: usize,
    channels: Box<[ChannelBuffer]>,
    /// `in_switch` を取った callback だけが触る。
    callback: UnsafeCell<Callback>,
    in_switch: AtomicBool,
    stats: Stats,
}

/// stream が 1 本だけであることの印 (`Stream::create` で取り、片付けが済んだら返す)。
static CLAIMED: AtomicBool = AtomicBool::new(false);
/// 動いている stream の状態。 callback はここから引く (module doc)。
static ACTIVE: AtomicPtr<State> = AtomicPtr::new(null_mut());
/// [`ACTIVE`] を読んでいる最中の callback の数 (module doc)。
static PINS: AtomicU32 = AtomicU32::new(0);

/// ドライバーへ渡す callback の表。 ドライバーはポインタを持ち続けるので static に置く。
static CALLBACKS: sys::AsioCallbacks = sys::AsioCallbacks {
    buffer_switch,
    sample_rate_did_change,
    asio_message,
    buffer_switch_time_info,
};

/// 動いている stream があれば、その状態で `f` を呼ぶ。 RT-safe (atomic だけ)。
fn with_state<R>(f: impl FnOnce(&State) -> R) -> Option<R> {
    PINS.fetch_add(1, Ordering::SeqCst);
    let state = ACTIVE.load(Ordering::SeqCst);
    // SAFETY: 非 null を見たなら、片付けは `PINS` が 0 になるまでこの状態を解放しない (module doc)。
    let result = unsafe { state.as_ref() }.map(f);
    PINS.fetch_sub(1, Ordering::SeqCst);
    result
}

/// buffer 1 枚の入口。 `time_info` = `bufferSwitchTimeInfo` で来た (`time` はその中身)。
fn on_switch(index: i32, time_info: bool, time: Option<SwitchTime>) {
    with_state(|state| {
        if state.in_switch.swap(true, Ordering::Acquire) {
            state.stats.reentered.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if time_info {
            state.stats.time_info_switches.fetch_add(1, Ordering::Relaxed);
        }
        // time info を使わないドライバーには位置を問い合わせる (SDK の host sample の
        // `bufferSwitch` と同じ、`host/sample/hostsample.cpp:324-340`)。
        let time = if time_info { time } else { state.sample_position() };
        state.process(index, time);
        state.in_switch.store(false, Ordering::Release);
    });
}

impl State {
    /// `getSamplePosition` (callback のスレッドから)。 ドライバーが答えなければ `None`。 RT-safe。
    fn sample_position(&self) -> Option<SwitchTime> {
        let mut position = sys::AsioSamples::default();
        let mut stamp = sys::AsioTimeStamp::default();
        // SAFETY: `driver` は `Stream` が借りている `Driver` の object。 出力先は有効な 2 つの struct。
        let code = unsafe {
            ((*(*self.driver).vtbl).get_sample_position)(self.driver, &raw mut position, &raw mut stamp)
        };
        (code == sys::ASE_OK)
            .then(|| SwitchTime { sample_position: position.get(), system_time_ns: stamp.get() })
    }

    /// buffer 1 枚ぶん。 `in_switch` を取った callback から呼ぶ。 RT-safe。
    fn process(&self, index: i32, time: Option<SwitchTime>) {
        let stats = &self.stats;
        let half = match index {
            0 => 0,
            1 => 1,
            _ => {
                stats.bad_index.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        // SAFETY: TEB を読むだけの呼び出し。
        let thread = unsafe { GetCurrentThreadId() };
        let previous = stats.callback_thread.swap(thread, Ordering::Relaxed);
        if previous != 0 && previous != thread {
            stats.thread_changes.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(time) = time {
            stats.last_sample_position.store(time.sample_position, Ordering::Relaxed);
        }
        let mut buffers = IoBuffers { half, frames: self.frames, channels: &self.channels };
        // SAFETY: `in_switch` を取っているので、この `FnMut` に触るのは自分だけ。
        let callback = unsafe { &mut *self.callback.get() };
        callback(&mut buffers, time);
        if self.post_output {
            // SAFETY: `driver` は `Stream` が借りている `Driver` の object。 `outputReady` は
            // buffer を書き終えた callback のスレッドから呼ぶもの (`host/sample/hostsample.cpp:311-313`)。
            unsafe { ((*(*self.driver).vtbl).output_ready)(self.driver) };
        }
        stats.switches.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "C" fn buffer_switch(index: i32, _direct_process: sys::AsioBool) {
    on_switch(index, false, None);
}

unsafe extern "C" fn buffer_switch_time_info(
    params: *mut sys::AsioTime,
    index: i32,
    _direct_process: sys::AsioBool,
) -> *mut sys::AsioTime {
    let time = if params.is_null() {
        None
    } else {
        // SAFETY: null でない `ASIOTime` (148 byte、`sys` の assert が配置を固定)。 packed なので
        // 揃いを仮定せずに読む。
        let info = unsafe { params.read_unaligned() }.time_info;
        let valid = sys::K_SYSTEM_TIME_VALID | sys::K_SAMPLE_POSITION_VALID;
        let (flags, position, system_time) = (info.flags, info.sample_position, info.system_time);
        (flags & valid == valid)
            .then(|| SwitchTime { sample_position: position.get(), system_time_ns: system_time.get() })
    };
    on_switch(index, true, time);
    // SDK の host sample は 0 を返す (`host/sample/hostsample.cpp:320`)。
    null_mut()
}

unsafe extern "C" fn sample_rate_did_change(_rate: sys::AsioSampleRate) {
    with_state(|state| state.stats.sample_rate_changes.fetch_add(1, Ordering::Relaxed));
}

/// `asioMessage` (`asio.h:424, 436-476`)。 `createBuffers` の最中にも呼ばれる (そのときはまだ
/// 状態が無いので、数えるだけの selector は捨てる)。
unsafe extern "C" fn asio_message(
    selector: i32,
    value: i32,
    _message: *mut c_void,
    _opt: *mut f64,
) -> i32 {
    let count = |pick: fn(&Stats) -> &AtomicU32| {
        with_state(|state| pick(&state.stats).fetch_add(1, Ordering::Relaxed));
        1
    };
    match selector {
        sys::K_ASIO_SELECTOR_SUPPORTED => i32::from(matches!(
            value,
            sys::K_ASIO_ENGINE_VERSION
                | sys::K_ASIO_RESET_REQUEST
                | sys::K_ASIO_RESYNC_REQUEST
                | sys::K_ASIO_LATENCIES_CHANGED
                | sys::K_ASIO_SUPPORTS_TIME_INFO
                | sys::K_ASIO_OVERLOAD
        )),
        // ホストが実装している ASIO の版 (2 以上で `asioMessage` / time info が使える)。
        sys::K_ASIO_ENGINE_VERSION => 2,
        // 1 を返すと、以後 `bufferSwitch` の代わりに `bufferSwitchTimeInfo` が来る。
        sys::K_ASIO_SUPPORTS_TIME_INFO => 1,
        // ここでは畳めない (ドライバーの中から呼ばれている)。 印だけ立てて制御スレッドに任せる。
        sys::K_ASIO_RESET_REQUEST => count(|s| &s.reset_requests),
        sys::K_ASIO_RESYNC_REQUEST => count(|s| &s.resync_requests),
        sys::K_ASIO_LATENCIES_CHANGED => count(|s| &s.latency_changes),
        sys::K_ASIO_OVERLOAD => count(|s| &s.overloads),
        _ => 0,
    }
}

/// [`CLAIMED`] を取った印。 `keep` しないまま drop すると返す。
struct Claim;

impl Claim {
    fn take() -> Result<Self, Error> {
        CLAIMED
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| Self)
            .map_err(|_| Error::Busy)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        CLAIMED.store(false, Ordering::SeqCst);
    }
}

/// buffer を作った ASIO の stream。 drop で `stop` → `disposeBuffers` → 状態の片付け。
pub struct Stream<'d> {
    driver: &'d Driver,
    state: NonNull<State>,
    running: bool,
}

impl<'d> Stream<'d> {
    /// `channels` を有効にして buffer を作る (まだ鳴らない)。 出力の 2 面は無音で埋めておく
    /// (`start` の直後に鳴るのはホストが `start` の前に埋めた面、`asio.h` の `ASIOStart` の説明)。
    pub fn create(
        driver: &'d Driver,
        channels: &[ChannelSpec],
        frames: u32,
        callback: Callback,
    ) -> Result<Self, Error> {
        let claim = Claim::take()?;
        let layout = channel_layout(driver, channels, frames)?;
        let mut infos: Vec<sys::AsioBufferInfo> = channels
            .iter()
            .map(|spec| sys::AsioBufferInfo {
                is_input: if spec.is_input { sys::ASIO_TRUE } else { sys::ASIO_FALSE },
                // `channel_layout` が ch 数 (<= `MAX_CHANNELS`) の内側であることを確かめた。
                channel_num: spec.channel.cast_signed(),
                buffers: [null_mut(); 2],
            })
            .collect();
        // SAFETY: `CALLBACKS` は static。 buffer は下の `dispose_buffers` か `Stream::drop` で返す。
        unsafe { driver.create_buffers(&mut infos, frames, &raw const CALLBACKS) }?;
        let buffers = match adopt_buffers(channels, &layout, &infos, frames) {
            Ok(buffers) => buffers,
            Err(err) => {
                let _ = driver.dispose_buffers();
                return Err(err);
            }
        };
        let state = Box::new(State {
            driver: driver.as_ptr(),
            post_output: driver.output_ready(),
            frames: frames as usize,
            channels: buffers,
            callback: UnsafeCell::new(callback),
            in_switch: AtomicBool::new(false),
            stats: Stats::default(),
        });
        let state = NonNull::from(Box::leak(state));
        ACTIVE.store(state.as_ptr(), Ordering::SeqCst);
        std::mem::forget(claim);
        Ok(Self { driver, state, running: false })
    }

    /// ドライバーが `outputReady` を使うか。
    pub fn posts_output(&self) -> bool {
        // SAFETY: 状態は `self` の drop まで生きている。
        unsafe { self.state.as_ref() }.post_output
    }

    pub fn start(&mut self) -> Result<(), Error> {
        self.driver.start()?;
        self.running = true;
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), Error> {
        self.running = false;
        self.driver.stop()
    }

    pub fn stats(&self) -> StatsSnapshot {
        // SAFETY: 状態は `self` の drop まで生きている。 読むのは atomic だけ。
        let s = &unsafe { self.state.as_ref() }.stats;
        StatsSnapshot {
            switches: s.switches.load(Ordering::Relaxed),
            time_info_switches: s.time_info_switches.load(Ordering::Relaxed),
            reentered: s.reentered.load(Ordering::Relaxed),
            bad_index: s.bad_index.load(Ordering::Relaxed),
            reset_requests: s.reset_requests.load(Ordering::Relaxed),
            resync_requests: s.resync_requests.load(Ordering::Relaxed),
            latency_changes: s.latency_changes.load(Ordering::Relaxed),
            overloads: s.overloads.load(Ordering::Relaxed),
            sample_rate_changes: s.sample_rate_changes.load(Ordering::Relaxed),
            callback_thread: s.callback_thread.load(Ordering::Relaxed),
            thread_changes: s.thread_changes.load(Ordering::Relaxed),
            last_sample_position: s.last_sample_position.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        if self.running {
            let _ = self.driver.stop();
        }
        // `disposeBuffers` は `stop` を含む (`asio.h` の `ASIODisposeBuffers` の Notes)。
        let _ = self.driver.dispose_buffers();
        ACTIVE.store(null_mut(), Ordering::SeqCst);
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        while PINS.load(Ordering::SeqCst) != 0 {
            if Instant::now() >= deadline {
                // callback が抜けない。 状態は解放せず、stream の印も返さない (module doc)。
                tracing::error!("ASIO callback did not return after stop; leaking stream state");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // SAFETY: `create` が `Box::leak` した状態。 `ACTIVE` は null で `PINS` は 0 なので、
        // もう誰も参照していない。
        drop(unsafe { Box::from_raw(self.state.as_ptr()) });
        CLAIMED.store(false, Ordering::SeqCst);
    }
}

/// ch ごとの (sample type, 1 sample の byte 数)。 引数の検査もここで済ませる。
fn channel_layout(
    driver: &Driver,
    channels: &[ChannelSpec],
    frames: u32,
) -> Result<Vec<(sys::AsioSampleType, usize)>, Error> {
    if channels.is_empty() || channels.len() > MAX_STREAM_CHANNELS {
        return Err(Error::Invalid(format!("{} stream channels", channels.len())));
    }
    let sizes = driver.buffer_sizes()?;
    if !(sizes.min..=sizes.max).contains(&frames) {
        return Err(Error::Invalid(format!("buffer size {frames} outside {sizes:?}")));
    }
    let (inputs, outputs) = driver.channels()?;
    let mut layout = Vec::with_capacity(channels.len());
    for (i, spec) in channels.iter().enumerate() {
        let available = if spec.is_input { inputs } else { outputs };
        if spec.channel >= available || channels[..i].contains(spec) {
            return Err(Error::Invalid(format!("stream channel {spec:?}")));
        }
        let info = driver.channel_info(spec.is_input, spec.channel)?;
        let bytes = sys::bytes_per_sample(info.sample_type).ok_or_else(|| {
            Error::Invalid(format!("unsupported sample type {} on {spec:?}", info.sample_type))
        })?;
        (frames as usize)
            .checked_mul(bytes)
            .ok_or_else(|| Error::Invalid(format!("buffer size {frames}")))?;
        layout.push((info.sample_type, bytes));
    }
    Ok(layout)
}

/// `createBuffers` が返したポインタを検査して受け取り、出力の 2 面を無音にする。
fn adopt_buffers(
    channels: &[ChannelSpec],
    layout: &[(sys::AsioSampleType, usize)],
    infos: &[sys::AsioBufferInfo],
    frames: u32,
) -> Result<Box<[ChannelBuffer]>, Error> {
    let mut buffers = Vec::with_capacity(channels.len());
    for ((spec, &(sample_type, bytes_per_sample)), info) in channels.iter().zip(layout).zip(infos) {
        let raw = info.buffers;
        let (Some(a), Some(b)) = (NonNull::new(raw[0].cast::<u8>()), NonNull::new(raw[1].cast::<u8>()))
        else {
            return Err(Error::Invalid(format!("createBuffers returned null for {spec:?}")));
        };
        if !spec.is_input {
            let len = frames as usize * bytes_per_sample;
            // SAFETY: ドライバーが返した、この ch の 2 面 (各 `len` byte)。 まだ `start` していない。
            unsafe {
                a.as_ptr().write_bytes(0, len);
                b.as_ptr().write_bytes(0, len);
            }
        }
        buffers.push(ChannelBuffer {
            is_input: spec.is_input,
            channel: spec.channel,
            sample_type,
            bytes_per_sample,
            halves: [a, b],
        });
    }
    Ok(buffers.into_boxed_slice())
}
