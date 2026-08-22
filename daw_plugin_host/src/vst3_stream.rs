#![allow(unsafe_op_in_unsafe_fn)]

// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `IBStream` wrappers used for VST3 state save/load.
//!
//! Save path uses `Vst3WriteStream` (a growing `Vec<u8>`); the finished
//! buffer is extracted with `take_buffer()` after `IComponent::getState`
//! returns. Load path uses `Vst3ReadStream` over a borrowed `&[u8]`.
//!
//! The implementations are single-threaded in practice (state save/load
//! runs on the plugin-main thread), but `ComWrapper<C>` requires
//! `C: Send + Sync`, so we use `UnsafeCell` + manual marker impls rather
//! than `RefCell`/`Mutex` to avoid locking inside the plugin's tight
//! IO loop.

use std::cell::UnsafeCell;
use std::ffi::c_void;

use com_scrape_types::Class;
use vst3::Steinberg::{
    IBStream, IBStreamTrait,
    IBStream_::IStreamSeekMode_,
    int32, int64, kInvalidArgument, kResultOk, tresult,
};

// --- Write stream ----------------------------------------------------------

pub struct Vst3WriteStream {
    inner: UnsafeCell<WriteInner>,
}

struct WriteInner {
    buf: Vec<u8>,
    pos: usize,
}

// SAFETY: VST3 host state calls are serialised on the plugin-main thread
// (see `Vst3Plugin::state_save`). No other thread touches these cells.
unsafe impl Send for Vst3WriteStream {}
unsafe impl Sync for Vst3WriteStream {}

impl Vst3WriteStream {
    pub fn new() -> Self {
        Self {
            inner: UnsafeCell::new(WriteInner {
                buf: Vec::new(),
                pos: 0,
            }),
        }
    }

    pub fn take_buffer(&self) -> Vec<u8> {
        let inner = unsafe { &mut *self.inner.get() };
        std::mem::take(&mut inner.buf)
    }
}

impl Class for Vst3WriteStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Vst3WriteStream {
    unsafe fn read(
        &self,
        _buffer: *mut c_void,
        _num_bytes: int32,
        num_bytes_read: *mut int32,
    ) -> tresult {
        // Write-only stream: reads are always an error.
        if !num_bytes_read.is_null() {
            *num_bytes_read = 0;
        }
        kInvalidArgument
    }

    unsafe fn write(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_written: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            if !num_bytes_written.is_null() {
                *num_bytes_written = 0;
            }
            return kInvalidArgument;
        }
        let n = num_bytes as usize;
        let inner = &mut *self.inner.get();
        // pos + n は理論上 usize オーバーフロー可能 (n ≤ i32::MAX を繰り返すと
        // 2GB stream で破綻)。checked_add で防御。
        let Some(end) = inner.pos.checked_add(n) else {
            if !num_bytes_written.is_null() {
                *num_bytes_written = 0;
            }
            return kInvalidArgument;
        };
        if end > inner.buf.len() {
            inner.buf.resize(end, 0);
        }
        let src = std::slice::from_raw_parts(buffer as *const u8, n);
        inner.buf[inner.pos..end].copy_from_slice(src);
        inner.pos = end;
        if !num_bytes_written.is_null() {
            *num_bytes_written = num_bytes;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let inner = &mut *self.inner.get();
        let len = inner.buf.len() as i64;
        let new_pos = if mode == IStreamSeekMode_::kIBSeekSet {
            pos
        } else if mode == IStreamSeekMode_::kIBSeekCur {
            inner.pos as i64 + pos
        } else if mode == IStreamSeekMode_::kIBSeekEnd {
            len + pos
        } else {
            return kInvalidArgument;
        };
        if new_pos < 0 {
            return kInvalidArgument;
        }
        inner.pos = new_pos as usize;
        if !result.is_null() {
            *result = new_pos;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        let inner = &*self.inner.get();
        *pos = inner.pos as i64;
        kResultOk
    }
}

// --- Read stream -----------------------------------------------------------

pub struct Vst3ReadStream {
    data: Vec<u8>,
    pos: UnsafeCell<usize>,
}

// SAFETY: state_load is plugin-main only; `pos` is never touched from
// another thread.
unsafe impl Send for Vst3ReadStream {}
unsafe impl Sync for Vst3ReadStream {}

impl Vst3ReadStream {
    pub fn new(data: &[u8]) -> Self {
        Self {
            data: data.to_vec(),
            pos: UnsafeCell::new(0),
        }
    }
}

impl Class for Vst3ReadStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Vst3ReadStream {
    unsafe fn read(
        &self,
        buffer: *mut c_void,
        num_bytes: int32,
        num_bytes_read: *mut int32,
    ) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            if !num_bytes_read.is_null() {
                *num_bytes_read = 0;
            }
            return kInvalidArgument;
        }
        let pos = &mut *self.pos.get();
        let want = num_bytes as usize;
        let remaining = self.data.len().saturating_sub(*pos);
        let n = want.min(remaining);
        if n > 0 {
            std::ptr::copy_nonoverlapping(
                self.data.as_ptr().add(*pos),
                buffer as *mut u8,
                n,
            );
            *pos += n;
        }
        if !num_bytes_read.is_null() {
            *num_bytes_read = n as i32;
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        _buffer: *mut c_void,
        _num_bytes: int32,
        num_bytes_written: *mut int32,
    ) -> tresult {
        if !num_bytes_written.is_null() {
            *num_bytes_written = 0;
        }
        kInvalidArgument
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let self_pos = &mut *self.pos.get();
        let len = self.data.len() as i64;
        let new_pos = if mode == IStreamSeekMode_::kIBSeekSet {
            pos
        } else if mode == IStreamSeekMode_::kIBSeekCur {
            *self_pos as i64 + pos
        } else if mode == IStreamSeekMode_::kIBSeekEnd {
            len + pos
        } else {
            return kInvalidArgument;
        };
        if new_pos < 0 {
            return kInvalidArgument;
        }
        *self_pos = new_pos as usize;
        if !result.is_null() {
            *result = new_pos;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        let self_pos = &*self.pos.get();
        *pos = *self_pos as i64;
        kResultOk
    }
}
