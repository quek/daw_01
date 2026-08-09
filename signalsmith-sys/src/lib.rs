//! Raw C ABI bindings to the vendored Signalsmith Stretch engine.
//!
//! This crate is deliberately thin: it declares the `extern "C"` surface of
//! `shim/stretch_shim.cpp` and nothing else. The safe wrapper (stream
//! continuity, latency compensation, RT contract) lives in
//! `daw_audio::stretch_engine`, next to the code that knows the audio thread's
//! rules.
//!
//! Provenance and the RT / threading contract are documented in
//! [`VENDOR.md`](../VENDOR.md) and `shim/stretch_shim.h`. The short version:
//!
//! - [`sms_create`] / [`sms_destroy`] allocate — **off the audio thread only**.
//!   `sms_create` warms the engine so every internal buffer reaches its
//!   high-water mark before the RT thread touches it.
//! - Every other function is allocation-free and RT-safe.
//! - A handle is owned by one thread at a time; there is no internal locking.

/// Opaque engine handle (`struct sms_stretch` in the shim).
///
/// Declared as an extern type stand-in rather than a real struct so Rust never
/// assumes anything about the C++ layout.
#[repr(C)]
pub struct SmsStretch {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// Allocate + configure + warm up a stereo engine. Returns null on failure.
    pub fn sms_create(sample_rate: f32) -> *mut SmsStretch;
    pub fn sms_destroy(s: *mut SmsStretch);
    pub fn sms_reset(s: *mut SmsStretch);
    /// 位相ランダム化用の乱数列を巻き戻す。 `sms_output_seek` が内部で呼ぶので
    /// 通常は不要。 pool 再利用でも live / export でも発音の頭が同じ乱数位置から
    /// 始まることを保証する。
    pub fn sms_reseed(s: *mut SmsStretch);
    pub fn sms_input_latency(s: *const SmsStretch) -> i32;
    pub fn sms_output_latency(s: *const SmsStretch) -> i32;
    /// `tonality_limit` is a frequency **relative to the sample rate**
    /// (Hz / sample_rate); 0 disables it.
    pub fn sms_set_transpose_semitones(s: *mut SmsStretch, semitones: f32, tonality_limit: f32);
    /// `compensate_pitch != 0` holds the spectral envelope still while the
    /// pitch moves (= a transposed voice keeps its character).
    pub fn sms_set_formant_semitones(s: *mut SmsStretch, semitones: f32, compensate_pitch: i32);
    pub fn sms_output_seek_length(s: *const SmsStretch, playback_rate: f64) -> i32;
    /// Fill the pipeline so the next [`sms_process`] output sample is `in_*[0]`.
    pub fn sms_output_seek(s: *mut SmsStretch, in_l: *const f32, in_r: *const f32, n: i32);
    /// Time-stretch ratio is `in_n / out_n`.
    pub fn sms_process(
        s: *mut SmsStretch,
        in_l: *const f32,
        in_r: *const f32,
        in_n: i32,
        out_l: *mut f32,
        out_r: *mut f32,
        out_n: i32,
    );
    /// C++ 側のヒープ確保回数 (**呼び出しスレッド**単位)。 `alloc-count`
    /// feature でビルドしたときだけ意味を持ち、無効なら `u64::MAX` (= 未計装)。
    /// RT 検査で見たいのは「audio thread が render 中に確保したか」なので、
    /// プロセス全体のカウンタでは他スレッドの確保を拾って使い物にならない。
    ///
    /// Rust の `#[global_allocator]` フックは **C++ の確保を一切見られない**
    /// (CRT へ直行するため) ので、RT 無確保の検証にはこちらが要る。
    pub fn sms_alloc_count() -> u64;
}
