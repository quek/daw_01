//! ASIO ドライバー 1 つ (`IASIO` の COM object) の薄い wrapper。 スレッドの規約は [`super`] の module doc。
//!
//! FFI 境界の規則: ドライバーが返す数は `i32` (`long`) のまま信用せず、ここで符号と上限を検査してから
//! `u32` / `usize` にする。 文字列はドライバーが長さ無しで書き込む (`getDriverName(char*)`) ので、
//! SDK の契約 (`ASIODriverInfo::name[32]` / `errorMessage[124]`、`asio.h:489-490`) より大きい
//! バッファを渡し、末尾を NUL にしてから読む。

use std::ffi::{c_char, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::ptr::{NonNull, null_mut};

use windows::Win32::Globalization::{CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize,
};
use windows::core::{GUID, HRESULT};

use super::sys;

windows::core::link!("ole32.dll" "system" fn CoCreateInstance(
    rclsid: *const GUID,
    outer: *mut c_void,
    cls_context: u32,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT);

/// ドライバーが申告できる ch 数の上限 (これを超える値は壊れた戻り値として扱う)。
pub const MAX_CHANNELS: u32 = 1024;
/// ドライバーが申告できる buffer 長 (frame) の上限。
pub const MAX_BUFFER_FRAMES: u32 = 1 << 20;

/// ASIO 層の失敗。
#[derive(Debug)]
pub enum Error {
    /// COM の呼び出しが失敗した。
    Com { call: &'static str, hresult: i32 },
    /// `IASIO::init` が false を返した (中身は `getErrorMessage` の文)。
    Init(String),
    /// `IASIO` の method が `ASE_OK` 以外を返した。
    Call { call: &'static str, code: sys::AsioError },
    /// ドライバーの戻り値、または呼び側の引数が範囲外。
    Invalid(String),
    /// 既に別の stream が動いている (ASIO の callback は user data を持てないので同時に 1 本)。
    Busy,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Com { call, hresult } => write!(f, "{call} failed: HRESULT 0x{hresult:08X}"),
            Self::Init(message) => write!(f, "ASIO driver init failed: {message}"),
            Self::Call { call, code } => {
                write!(f, "{call} failed: {} ({code})", sys::error_name(*code))
            }
            Self::Invalid(what) => write!(f, "invalid ASIO value: {what}"),
            Self::Busy => write!(f, "another ASIO stream is already active"),
        }
    }
}

impl std::error::Error for Error {}

fn check(call: &'static str, code: sys::AsioError) -> Result<(), Error> {
    if code == sys::ASE_OK { Ok(()) } else { Err(Error::Call { call, code }) }
}

/// このスレッドの COM apartment (STA)。 drop で `CoUninitialize`。
///
/// **[`Driver`] より後に drop すること** (ドライバーの `Release` は apartment の中で呼ぶ)。
pub struct ComApartment(PhantomData<*mut ()>);

impl ComApartment {
    /// このスレッドを STA で初期化する。 SDK の host は `CoInitialize(0)` (= STA) の上でドライバーを
    /// 作る (`host/pc/asiolist.cpp:203`)。 既に別の方式 (MTA) で初期化済みなら失敗する。
    pub fn enter() -> Result<Self, Error> {
        // SAFETY: 引数は予約値の null と apartment の指定だけ。
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_err() {
            return Err(Error::Com { call: "CoInitializeEx", hresult: hr.0 });
        }
        Ok(Self(PhantomData))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: `enter` が成功したスレッドで、その 1 回ぶんを戻す (`!Send` なので同じスレッド)。
        unsafe { CoUninitialize() };
    }
}

/// buffer 長の選択肢 (`getBufferSize`、`asio.h` の `ASIOGetBufferSize` の説明)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferSizes {
    pub min: u32,
    pub max: u32,
    pub preferred: u32,
    /// 刻み。 `-1` = `min` から 2 倍ずつ、`0` = `preferred` だけ、正 = その刻み。
    pub granularity: i32,
}

impl BufferSizes {
    /// ホストから選べる buffer 長を小さい順に (`out` へ足す。 上限 `cap` 個)。
    ///
    /// 刻みが 0 のときは `preferred` だけ。 仕様上は `min == max` のはずだが、`min != max` のまま
    /// 0 を返すドライバーがある (この PC の "FL Studio ASIO": 256 / 4096 / 512 / 0)。 そういう
    /// ドライバーの buffer 長は設定画面 (`controlPanel`) で変えるもので、ホストは `preferred` に従う。
    pub fn candidates(&self, out: &mut Vec<u32>, cap: usize) {
        let step = |size: u32| match self.granularity {
            -1 => size.checked_mul(2),
            step if step > 0 => size.checked_add(step.unsigned_abs()),
            _ => None,
        };
        if self.granularity != 0 {
            let mut size = self.min;
            while size <= self.max && out.len() < cap {
                out.push(size);
                match step(size) {
                    Some(next) if next > size => size = next,
                    _ => break,
                }
            }
        }
        if !out.contains(&self.preferred) {
            out.push(self.preferred);
            out.sort_unstable();
        }
    }
}

/// ch 1 本の情報 (`getChannelInfo`)。
#[derive(Clone, Debug)]
pub struct ChannelInfo {
    /// `createBuffers` で有効にした ch か。
    pub active: bool,
    pub group: i32,
    pub sample_type: sys::AsioSampleType,
    pub name: String,
}

/// 開いた ASIO ドライバー。 作ったスレッド (STA) に縛る (`!Send`)。 drop で `Release`。
pub struct Driver {
    raw: NonNull<sys::IAsio>,
}

impl Driver {
    /// ドライバーを生成して `init` する。 `sys_handle` はアプリの主窓の HWND
    /// (`ASIODriverInfo::sysRef`、`asio.h:491-492`)。
    ///
    /// IID に **CLSID 自身**を渡すのが ASIO の決まり (`host/pc/asiolist.cpp:230`。 `IASIO` は
    /// 登録された IID を持たない)。
    pub fn open(clsid: &GUID, sys_handle: *mut c_void) -> Result<Self, Error> {
        let mut out: *mut c_void = null_mut();
        // SAFETY: `clsid` は有効な GUID、`out` は有効な出力先。 失敗時 `out` は null のまま。
        let hr = unsafe {
            CoCreateInstance(clsid, null_mut(), CLSCTX_INPROC_SERVER.0, clsid, &raw mut out)
        };
        if hr.is_err() {
            return Err(Error::Com { call: "CoCreateInstance", hresult: hr.0 });
        }
        let raw = NonNull::new(out.cast::<sys::IAsio>())
            .ok_or_else(|| Error::Invalid("CoCreateInstance returned null".into()))?;
        // SAFETY: `raw` は COM object の先頭 (vtable へのポインタ 1 語) を指す。
        if unsafe { raw.as_ref().vtbl.is_null() } {
            return Err(Error::Invalid("driver object has no vtable".into()));
        }
        let driver = Self { raw };
        // SAFETY: vtable は検査済み。 `init` の引数は HWND (null 可)。
        let ok = unsafe { (driver.vtbl().init)(driver.as_ptr(), sys_handle) };
        if ok == sys::ASIO_FALSE {
            return Err(Error::Init(driver.error_message()));
        }
        Ok(driver)
    }

    pub(super) fn as_ptr(&self) -> *mut sys::IAsio {
        self.raw.as_ptr()
    }

    fn vtbl(&self) -> &sys::IAsioVtbl {
        // SAFETY: `open` が null でないことを確かめた。 object の寿命は `self` が握っている。
        unsafe { &*self.raw.as_ref().vtbl }
    }

    /// ドライバーが名乗る名前。
    pub fn name(&self) -> String {
        let mut buf = [0u8; 256];
        // SAFETY: SDK の契約は 32 byte。 それより大きい 256 byte を渡している。
        unsafe { (self.vtbl().get_driver_name)(self.as_ptr(), buf.as_mut_ptr().cast::<c_char>()) };
        ansi_to_string(&mut buf)
    }

    pub fn version(&self) -> i32 {
        // SAFETY: 引数なしの問い合わせ。
        unsafe { (self.vtbl().get_driver_version)(self.as_ptr()) }
    }

    /// 直前の失敗の説明 (ドライバーが持っていれば)。
    pub fn error_message(&self) -> String {
        let mut buf = [0u8; 512];
        // SAFETY: SDK の契約は 124 byte。 それより大きい 512 byte を渡している。
        unsafe { (self.vtbl().get_error_message)(self.as_ptr(), buf.as_mut_ptr().cast::<c_char>()) };
        ansi_to_string(&mut buf)
    }

    /// (入力 ch 数, 出力 ch 数)。
    pub fn channels(&self) -> Result<(u32, u32), Error> {
        let (mut inputs, mut outputs) = (0i32, 0i32);
        // SAFETY: 出力先は有効な `i32` 2 つ。
        let code =
            unsafe { (self.vtbl().get_channels)(self.as_ptr(), &raw mut inputs, &raw mut outputs) };
        check("getChannels", code)?;
        Ok((bounded("input channels", inputs, MAX_CHANNELS)?, bounded("output channels", outputs, MAX_CHANNELS)?))
    }

    /// (入力, 出力) の latency (frame)。 **`createBuffers` の後でないと正しくないドライバーがある**
    /// (SDK の host sample は create の後に取る、`host/sample/hostsample.cpp:475`)。
    pub fn latencies(&self) -> Result<(u32, u32), Error> {
        let (mut input, mut output) = (0i32, 0i32);
        // SAFETY: 出力先は有効な `i32` 2 つ。
        let code =
            unsafe { (self.vtbl().get_latencies)(self.as_ptr(), &raw mut input, &raw mut output) };
        check("getLatencies", code)?;
        Ok((bounded("input latency", input, u32::MAX)?, bounded("output latency", output, u32::MAX)?))
    }

    pub fn buffer_sizes(&self) -> Result<BufferSizes, Error> {
        let (mut min, mut max, mut preferred, mut granularity) = (0i32, 0i32, 0i32, 0i32);
        // SAFETY: 出力先は有効な `i32` 4 つ。
        let code = unsafe {
            (self.vtbl().get_buffer_size)(
                self.as_ptr(),
                &raw mut min,
                &raw mut max,
                &raw mut preferred,
                &raw mut granularity,
            )
        };
        check("getBufferSize", code)?;
        let sizes = BufferSizes {
            min: bounded("min buffer size", min, MAX_BUFFER_FRAMES)?,
            max: bounded("max buffer size", max, MAX_BUFFER_FRAMES)?,
            preferred: bounded("preferred buffer size", preferred, MAX_BUFFER_FRAMES)?,
            granularity,
        };
        if sizes.min == 0 || sizes.min > sizes.max || !(sizes.min..=sizes.max).contains(&sizes.preferred) {
            return Err(Error::Invalid(format!("buffer sizes {sizes:?}")));
        }
        Ok(sizes)
    }

    pub fn can_sample_rate(&self, rate: f64) -> bool {
        // SAFETY: 値渡しの問い合わせ。
        unsafe { (self.vtbl().can_sample_rate)(self.as_ptr(), rate) == sys::ASE_OK }
    }

    /// 今の sample rate。 **まだ決まっていなければ `None`** — `setSampleRate` を 1 度も受けていない
    /// ドライバーは `ASE_OK` で 0 を返すことがある (この PC の "Realtek ASIO" で実測)。 そのときは
    /// 呼び側が [`Self::can_sample_rate`] で選んで [`Self::set_sample_rate`] する。
    pub fn sample_rate(&self) -> Result<Option<f64>, Error> {
        let mut rate = 0f64;
        // SAFETY: 出力先は有効な `f64`。
        let code = unsafe { (self.vtbl().get_sample_rate)(self.as_ptr(), &raw mut rate) };
        check("getSampleRate", code)?;
        Ok((rate.is_finite() && rate > 0.0).then_some(rate))
    }

    pub fn set_sample_rate(&self, rate: f64) -> Result<(), Error> {
        // SAFETY: 値渡し。
        check("setSampleRate", unsafe { (self.vtbl().set_sample_rate)(self.as_ptr(), rate) })
    }

    pub fn channel_info(&self, is_input: bool, channel: u32) -> Result<ChannelInfo, Error> {
        let mut info = sys::AsioChannelInfo {
            channel: i32::try_from(channel).map_err(|_| Error::Invalid(format!("channel {channel}")))?,
            is_input: if is_input { sys::ASIO_TRUE } else { sys::ASIO_FALSE },
            is_active: sys::ASIO_FALSE,
            channel_group: 0,
            sample_type: -1,
            name: [0; 32],
        };
        // SAFETY: `info` は有効な `ASIOChannelInfo` (配置は `sys` の assert が固定)。
        let code = unsafe { (self.vtbl().get_channel_info)(self.as_ptr(), &raw mut info) };
        check("getChannelInfo", code)?;
        let mut name = info.name.map(|c| c.cast_unsigned());
        Ok(ChannelInfo {
            active: info.is_active != sys::ASIO_FALSE,
            group: info.channel_group,
            sample_type: info.sample_type,
            name: ansi_to_string(&mut name),
        })
    }

    /// ドライバーの設定画面を開く。 画面は**呼んだスレッド**に属する窓として出るので、
    /// そのスレッドが message を回し続けていること ([`super`] の module doc)。
    pub fn control_panel(&self) -> Result<(), Error> {
        // SAFETY: 引数なし。
        check("controlPanel", unsafe { (self.vtbl().control_panel)(self.as_ptr()) })
    }

    /// `outputReady` を使うドライバーか (`ASE_OK` を返したら、以後 buffer を書き終えるたびに呼ぶ。
    /// `asio.h` の `ASIOOutputReady` の説明)。
    pub fn output_ready(&self) -> bool {
        // SAFETY: 引数なし。
        unsafe { (self.vtbl().output_ready)(self.as_ptr()) == sys::ASE_OK }
    }

    /// # Safety
    /// `callbacks` は `disposeBuffers` (または drop) まで生きていること。 成功したら、対になる
    /// [`Self::dispose_buffers`] を呼ぶまでドライバーは `infos` に返した buffer と `callbacks` を使う。
    pub(super) unsafe fn create_buffers(
        &self,
        infos: &mut [sys::AsioBufferInfo],
        frames: u32,
        callbacks: *const sys::AsioCallbacks,
    ) -> Result<(), Error> {
        let count = i32::try_from(infos.len()).map_err(|_| Error::Invalid("too many channels".into()))?;
        let frames = i32::try_from(frames).map_err(|_| Error::Invalid(format!("buffer size {frames}")))?;
        // SAFETY: `infos` は `count` 要素の配列。 `callbacks` の寿命は呼び側の契約。
        let code = unsafe {
            (self.vtbl().create_buffers)(self.as_ptr(), infos.as_mut_ptr(), count, frames, callbacks)
        };
        check("createBuffers", code)
    }

    pub(super) fn dispose_buffers(&self) -> Result<(), Error> {
        // SAFETY: 引数なし。
        check("disposeBuffers", unsafe { (self.vtbl().dispose_buffers)(self.as_ptr()) })
    }

    pub(super) fn start(&self) -> Result<(), Error> {
        // SAFETY: 引数なし。
        check("start", unsafe { (self.vtbl().start)(self.as_ptr()) })
    }

    pub(super) fn stop(&self) -> Result<(), Error> {
        // SAFETY: 引数なし。
        check("stop", unsafe { (self.vtbl().stop)(self.as_ptr()) })
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // SAFETY: `open` が取った参照 1 つを返す。 ASIO に exit の method は無く、`Release` が
        // ドライバーを畳む (SDK の `ASIOExit` も release するだけ、`common/asio.cpp:104-116`)。
        unsafe { (self.vtbl().release)(self.as_ptr()) };
    }
}

/// ドライバーが返した `long` を `0..=max` の `u32` にする。
fn bounded(what: &'static str, value: i32, max: u32) -> Result<u32, Error> {
    match u32::try_from(value) {
        Ok(v) if v <= max => Ok(v),
        _ => Err(Error::Invalid(format!("{what} = {value}"))),
    }
}

/// ドライバーが書いた ANSI (システムの code page) の C 文字列を読む。 末尾は必ず NUL にしてから走査する。
/// 前後の空白・改行は落とす (「Prism Sound USB Audio Class 2.0」は ch 名を `"AO 1\n"` と改行付きで返す)。
fn ansi_to_string(buf: &mut [u8]) -> String {
    let Some(last) = buf.last_mut() else { return String::new() };
    *last = 0;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(0);
    if len == 0 {
        return String::new();
    }
    let mut wide = vec![0u16; len];
    // SAFETY: 入力は `len` byte、出力は `len` 文字ぶん (1 byte は高々 1 文字になる)。
    let written = unsafe {
        MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), &buf[..len], Some(&mut wide))
    };
    let written = usize::try_from(written).unwrap_or(0).min(wide.len());
    String::from_utf16_lossy(&wide[..written]).trim().to_owned()
}
