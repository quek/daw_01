//! ASIO の ABI (型・定数・`IASIO` の vtable)。 規約は [`super`] の module doc。
//!
//! ここは宣言だけで、呼び出しは [`super::driver`] / [`super::stream`] が持つ。
//! 出典は ASIO SDK 2.3 の `common/asio.h` / `common/iasiodrv.h` (行番号は各項目の doc)。
//!
//! **Windows の `long` は 32 bit** なので、SDK の `long` / `ASIOBool` / `ASIOError` /
//! `ASIOSampleType` はすべて `i32`、`unsigned long` は `u32`。
//!
//! **packing**: `asio.h` は全体が `#pragma pack(push,4)` の中にある (`asio.h:88-89` 〜 `1071-1072`)。
//! 8 byte 境界に揃う型 (`double` / pointer) を持つ struct は自然な配置とずれるので
//! `#[repr(C, packed(4))]` で宣言する。 実際にずれるのは `ASIOTimeCode` (84 byte。 自然配置なら 88) と
//! それを含む `ASIOTime` (148 byte)。 下の `const _` の assert は、 SDK のヘッダを MSVC (x64) で
//! コンパイルして出した `sizeof` / `offsetof` と同じ値を固定している。

// ABI の写しなので、こちらが使わない定数・vtable の slot も宣言として要る (slot は順序が意味)。
#![allow(dead_code)]

use std::ffi::{c_char, c_void};
use std::mem::{offset_of, size_of};

use windows::core::{GUID, HRESULT};

/// `ASIOBool` (`asio.h:130`)。
pub type AsioBool = i32;
/// `ASIOError` (`asio.h:217`)。
pub type AsioError = i32;
/// `ASIOSampleRate` (`asio.h:121-122`。 Windows は `IEEE754_64FLOAT` = 1、`asiosys.h:30`)。
pub type AsioSampleRate = f64;
/// `ASIOSampleType` (`asio.h:137`)。
pub type AsioSampleType = i32;

pub const ASIO_FALSE: AsioBool = 0;
pub const ASIO_TRUE: AsioBool = 1;

// `ASIOError` の値 (`asio.h:218-228`)。
pub const ASE_OK: AsioError = 0;
/// `ASIOFuture` だけが返す成功値 (`ASE_OK` では足りない、`asio.h:220`)。
pub const ASE_SUCCESS: AsioError = 0x3f48_47a0;
pub const ASE_NOT_PRESENT: AsioError = -1000;
pub const ASE_HW_MALFUNCTION: AsioError = -999;
pub const ASE_INVALID_PARAMETER: AsioError = -998;
pub const ASE_INVALID_MODE: AsioError = -997;
pub const ASE_SP_NOT_ADVANCING: AsioError = -996;
pub const ASE_NO_CLOCK: AsioError = -995;
pub const ASE_NO_MEMORY: AsioError = -994;

// `ASIOSampleType` の値 (`asio.h:138-171`)。 x86 のドライバーが返すのは実質 LSB 系。
pub const ASIOST_INT16_MSB: AsioSampleType = 0;
pub const ASIOST_INT24_MSB: AsioSampleType = 1;
pub const ASIOST_INT32_MSB: AsioSampleType = 2;
pub const ASIOST_FLOAT32_MSB: AsioSampleType = 3;
pub const ASIOST_FLOAT64_MSB: AsioSampleType = 4;
pub const ASIOST_INT32_MSB16: AsioSampleType = 8;
pub const ASIOST_INT32_MSB18: AsioSampleType = 9;
pub const ASIOST_INT32_MSB20: AsioSampleType = 10;
pub const ASIOST_INT32_MSB24: AsioSampleType = 11;
pub const ASIOST_INT16_LSB: AsioSampleType = 16;
pub const ASIOST_INT24_LSB: AsioSampleType = 17;
pub const ASIOST_INT32_LSB: AsioSampleType = 18;
pub const ASIOST_FLOAT32_LSB: AsioSampleType = 19;
pub const ASIOST_FLOAT64_LSB: AsioSampleType = 20;
pub const ASIOST_INT32_LSB16: AsioSampleType = 24;
pub const ASIOST_INT32_LSB18: AsioSampleType = 25;
pub const ASIOST_INT32_LSB20: AsioSampleType = 26;
pub const ASIOST_INT32_LSB24: AsioSampleType = 27;
pub const ASIOST_DSD_INT8_LSB1: AsioSampleType = 32;
pub const ASIOST_DSD_INT8_MSB1: AsioSampleType = 33;
pub const ASIOST_DSD_INT8_NER8: AsioSampleType = 40;

// `asioMessage` の selector (`asio.h:436-476`)。
pub const K_ASIO_SELECTOR_SUPPORTED: i32 = 1;
pub const K_ASIO_ENGINE_VERSION: i32 = 2;
pub const K_ASIO_RESET_REQUEST: i32 = 3;
pub const K_ASIO_BUFFER_SIZE_CHANGE: i32 = 4;
pub const K_ASIO_RESYNC_REQUEST: i32 = 5;
pub const K_ASIO_LATENCIES_CHANGED: i32 = 6;
pub const K_ASIO_SUPPORTS_TIME_INFO: i32 = 7;
pub const K_ASIO_SUPPORTS_TIME_CODE: i32 = 8;
pub const K_ASIO_MMC_COMMAND: i32 = 9;
pub const K_ASIO_SUPPORTS_INPUT_MONITOR: i32 = 10;
pub const K_ASIO_SUPPORTS_INPUT_GAIN: i32 = 11;
pub const K_ASIO_SUPPORTS_INPUT_METER: i32 = 12;
pub const K_ASIO_SUPPORTS_OUTPUT_GAIN: i32 = 13;
pub const K_ASIO_SUPPORTS_OUTPUT_METER: i32 = 14;
pub const K_ASIO_OVERLOAD: i32 = 15;

// `AsioTimeInfo::flags` (`asio.h:269-278`)。
pub const K_SYSTEM_TIME_VALID: u32 = 1;
pub const K_SAMPLE_POSITION_VALID: u32 = 1 << 1;
pub const K_SAMPLE_RATE_VALID: u32 = 1 << 2;

/// `ASIOSamples` (`asio.h:102-105`。 Windows は `NATIVE_INT64` = 0 なので hi / lo の 2 語)。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AsioSamples {
    pub hi: u32,
    pub lo: u32,
}

/// `ASIOTimeStamp` (`asio.h:113-116`)。 単位は ns。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AsioTimeStamp {
    pub hi: u32,
    pub lo: u32,
}

impl AsioSamples {
    pub fn get(self) -> u64 {
        (u64::from(self.hi) << 32) | u64::from(self.lo)
    }
}

impl AsioTimeStamp {
    pub fn get(self) -> u64 {
        (u64::from(self.hi) << 32) | u64::from(self.lo)
    }
}

/// `ASIOTimeCode` (`asio.h:237-244`)。
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeCode {
    pub speed: f64,
    pub time_code_samples: AsioSamples,
    pub flags: u32,
    pub future: [c_char; 64],
}

/// `AsioTimeInfo` (`asio.h:257-267`)。
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTimeInfo {
    pub speed: f64,
    pub system_time: AsioTimeStamp,
    pub sample_position: AsioSamples,
    pub sample_rate: AsioSampleRate,
    pub flags: u32,
    pub reserved: [c_char; 12],
}

/// `ASIOTime` (`asio.h:280-285`)。
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioTime {
    pub reserved: [i32; 4],
    pub time_info: AsioTimeInfo,
    pub time_code: AsioTimeCode,
}

/// `ASIOClockSource` (`asio.h:687-694`)。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioClockSource {
    pub index: i32,
    pub associated_channel: i32,
    pub associated_group: i32,
    pub is_current_source: AsioBool,
    pub name: [c_char; 32],
}

/// `ASIOChannelInfo` (`asio.h:775-783`)。 `channel` / `is_input` が入力、残りが出力。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AsioChannelInfo {
    pub channel: i32,
    pub is_input: AsioBool,
    pub is_active: AsioBool,
    pub channel_group: i32,
    pub sample_type: AsioSampleType,
    pub name: [c_char; 32],
}

/// `ASIOBufferInfo` (`asio.h:824-829`)。 `buffers` は `createBuffers` がドライバー所有の
/// 2 面 (double buffer の A / B) を書いて返す。
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
pub struct AsioBufferInfo {
    pub is_input: AsioBool,
    pub channel_num: i32,
    pub buffers: [*mut c_void; 2],
}

/// `ASIOCallbacks` (`asio.h:400-433`)。 呼び出し規約の指定が無い関数ポインタ = cdecl
/// (x64 では規約は 1 つしかない)。 **user data を運ぶ引数が無い**ので、受け側は static で状態を引く。
#[repr(C, packed(4))]
pub struct AsioCallbacks {
    pub buffer_switch: unsafe extern "C" fn(double_buffer_index: i32, direct_process: AsioBool),
    pub sample_rate_did_change: unsafe extern "C" fn(sample_rate: AsioSampleRate),
    pub asio_message:
        unsafe extern "C" fn(selector: i32, value: i32, message: *mut c_void, opt: *mut f64) -> i32,
    pub buffer_switch_time_info: unsafe extern "C" fn(
        params: *mut AsioTime,
        double_buffer_index: i32,
        direct_process: AsioBool,
    ) -> *mut AsioTime,
}

// SDK のヘッダを MSVC (x64) で通した `sizeof` / `offsetof` と同じ値 (module doc)。
const _: () = {
    assert!(size_of::<AsioSamples>() == 8);
    assert!(size_of::<AsioTimeStamp>() == 8);
    assert!(size_of::<AsioTimeInfo>() == 48);
    assert!(offset_of!(AsioTimeInfo, system_time) == 8);
    assert!(offset_of!(AsioTimeInfo, sample_position) == 16);
    assert!(offset_of!(AsioTimeInfo, sample_rate) == 24);
    assert!(offset_of!(AsioTimeInfo, flags) == 32);
    assert!(size_of::<AsioTimeCode>() == 84);
    assert!(offset_of!(AsioTimeCode, flags) == 16);
    assert!(offset_of!(AsioTimeCode, future) == 20);
    assert!(size_of::<AsioTime>() == 148);
    assert!(offset_of!(AsioTime, time_info) == 16);
    assert!(offset_of!(AsioTime, time_code) == 64);
    assert!(size_of::<AsioChannelInfo>() == 52);
    assert!(offset_of!(AsioChannelInfo, name) == 20);
    assert!(size_of::<AsioBufferInfo>() == 24);
    assert!(offset_of!(AsioBufferInfo, buffers) == 8);
    assert!(size_of::<AsioCallbacks>() == 32);
    assert!(size_of::<AsioClockSource>() == 48);
};

/// `IASIO` の object (`iasiodrv.h:27`)。 先頭 1 語が vtable へのポインタ。
#[repr(C)]
pub struct IAsio {
    pub vtbl: *const IAsioVtbl,
}

/// `IASIO : IUnknown` の vtable (`iasiodrv.h:27-53`)。 **宣言順がそのまま slot 順** (MSVC は
/// overload の無い virtual を宣言順に並べる。 `IASIO` に overload は無い)。
///
/// 呼び出し規約は `extern "system"`。 x64 Windows には規約が 1 つしかないので C++ の member
/// 関数 (`this` が第 1 引数) をそのまま呼べる。 **x86 (32 bit) の `IASIO` は thiscall で、この
/// 宣言では呼べない** — だから module ごと 64 bit 限定にしている。
#[repr(C)]
pub struct IAsioVtbl {
    // IUnknown
    pub query_interface:
        unsafe extern "system" fn(this: *mut IAsio, iid: *const GUID, out: *mut *mut c_void) -> HRESULT,
    pub add_ref: unsafe extern "system" fn(this: *mut IAsio) -> u32,
    pub release: unsafe extern "system" fn(this: *mut IAsio) -> u32,
    // IASIO
    pub init: unsafe extern "system" fn(this: *mut IAsio, sys_handle: *mut c_void) -> AsioBool,
    pub get_driver_name: unsafe extern "system" fn(this: *mut IAsio, name: *mut c_char),
    pub get_driver_version: unsafe extern "system" fn(this: *mut IAsio) -> i32,
    pub get_error_message: unsafe extern "system" fn(this: *mut IAsio, string: *mut c_char),
    pub start: unsafe extern "system" fn(this: *mut IAsio) -> AsioError,
    pub stop: unsafe extern "system" fn(this: *mut IAsio) -> AsioError,
    pub get_channels:
        unsafe extern "system" fn(this: *mut IAsio, inputs: *mut i32, outputs: *mut i32) -> AsioError,
    pub get_latencies:
        unsafe extern "system" fn(this: *mut IAsio, input: *mut i32, output: *mut i32) -> AsioError,
    pub get_buffer_size: unsafe extern "system" fn(
        this: *mut IAsio,
        min: *mut i32,
        max: *mut i32,
        preferred: *mut i32,
        granularity: *mut i32,
    ) -> AsioError,
    pub can_sample_rate: unsafe extern "system" fn(this: *mut IAsio, rate: AsioSampleRate) -> AsioError,
    pub get_sample_rate:
        unsafe extern "system" fn(this: *mut IAsio, rate: *mut AsioSampleRate) -> AsioError,
    pub set_sample_rate: unsafe extern "system" fn(this: *mut IAsio, rate: AsioSampleRate) -> AsioError,
    pub get_clock_sources: unsafe extern "system" fn(
        this: *mut IAsio,
        clocks: *mut AsioClockSource,
        num_sources: *mut i32,
    ) -> AsioError,
    pub set_clock_source: unsafe extern "system" fn(this: *mut IAsio, reference: i32) -> AsioError,
    pub get_sample_position: unsafe extern "system" fn(
        this: *mut IAsio,
        position: *mut AsioSamples,
        time_stamp: *mut AsioTimeStamp,
    ) -> AsioError,
    pub get_channel_info:
        unsafe extern "system" fn(this: *mut IAsio, info: *mut AsioChannelInfo) -> AsioError,
    pub create_buffers: unsafe extern "system" fn(
        this: *mut IAsio,
        buffer_infos: *mut AsioBufferInfo,
        num_channels: i32,
        buffer_size: i32,
        callbacks: *const AsioCallbacks,
    ) -> AsioError,
    pub dispose_buffers: unsafe extern "system" fn(this: *mut IAsio) -> AsioError,
    pub control_panel: unsafe extern "system" fn(this: *mut IAsio) -> AsioError,
    pub future:
        unsafe extern "system" fn(this: *mut IAsio, selector: i32, opt: *mut c_void) -> AsioError,
    pub output_ready: unsafe extern "system" fn(this: *mut IAsio) -> AsioError,
}

/// sample type 1 つぶんの byte 数。 PCM でない型 (DSD) と未知の値は `None`。
pub fn bytes_per_sample(sample_type: AsioSampleType) -> Option<usize> {
    match sample_type {
        ASIOST_INT16_MSB | ASIOST_INT16_LSB => Some(2),
        ASIOST_INT24_MSB | ASIOST_INT24_LSB => Some(3),
        ASIOST_INT32_MSB
        | ASIOST_INT32_LSB
        | ASIOST_FLOAT32_MSB
        | ASIOST_FLOAT32_LSB
        | ASIOST_INT32_MSB16
        | ASIOST_INT32_MSB18
        | ASIOST_INT32_MSB20
        | ASIOST_INT32_MSB24
        | ASIOST_INT32_LSB16
        | ASIOST_INT32_LSB18
        | ASIOST_INT32_LSB20
        | ASIOST_INT32_LSB24 => Some(4),
        ASIOST_FLOAT64_MSB | ASIOST_FLOAT64_LSB => Some(8),
        _ => None,
    }
}

/// sample type の表示名 (SDK の enum 名)。
pub fn sample_type_name(sample_type: AsioSampleType) -> &'static str {
    match sample_type {
        ASIOST_INT16_MSB => "Int16MSB",
        ASIOST_INT24_MSB => "Int24MSB",
        ASIOST_INT32_MSB => "Int32MSB",
        ASIOST_FLOAT32_MSB => "Float32MSB",
        ASIOST_FLOAT64_MSB => "Float64MSB",
        ASIOST_INT32_MSB16 => "Int32MSB16",
        ASIOST_INT32_MSB18 => "Int32MSB18",
        ASIOST_INT32_MSB20 => "Int32MSB20",
        ASIOST_INT32_MSB24 => "Int32MSB24",
        ASIOST_INT16_LSB => "Int16LSB",
        ASIOST_INT24_LSB => "Int24LSB",
        ASIOST_INT32_LSB => "Int32LSB",
        ASIOST_FLOAT32_LSB => "Float32LSB",
        ASIOST_FLOAT64_LSB => "Float64LSB",
        ASIOST_INT32_LSB16 => "Int32LSB16",
        ASIOST_INT32_LSB18 => "Int32LSB18",
        ASIOST_INT32_LSB20 => "Int32LSB20",
        ASIOST_INT32_LSB24 => "Int32LSB24",
        ASIOST_DSD_INT8_LSB1 => "DSDInt8LSB1",
        ASIOST_DSD_INT8_MSB1 => "DSDInt8MSB1",
        ASIOST_DSD_INT8_NER8 => "DSDInt8NER8",
        _ => "unknown",
    }
}

/// `ASIOError` の表示名。
pub fn error_name(code: AsioError) -> &'static str {
    match code {
        ASE_OK => "ASE_OK",
        ASE_SUCCESS => "ASE_SUCCESS",
        ASE_NOT_PRESENT => "ASE_NotPresent",
        ASE_HW_MALFUNCTION => "ASE_HWMalfunction",
        ASE_INVALID_PARAMETER => "ASE_InvalidParameter",
        ASE_INVALID_MODE => "ASE_InvalidMode",
        ASE_SP_NOT_ADVANCING => "ASE_SPNotAdvancing",
        ASE_NO_CLOCK => "ASE_NoClock",
        ASE_NO_MEMORY => "ASE_NoMemory",
        _ => "unknown",
    }
}
