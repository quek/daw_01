// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `ara-sys` — vendored Rust bindings to the ARA (Audio Random Access) C API.
//!
//! These are *pure type definitions*: ARA exposes no linkable symbols, so there
//! is nothing for this crate to link against. The host obtains an [`ARAFactory`]
//! from the plug-in (through the CLAP / VST3 companion APIs, wired up in
//! `daw_plugin_host`) and then drives the entire ARA model through the
//! function-pointer tables defined here.
//!
//! The bindings are generated from `vendor/ARA_API/ARAInterface.h` (ARA 2.3.0.001,
//! Apache-2.0) and committed as `src/bindings.rs`. Regenerate with
//! `LIBCLANG_PATH=<dir> cargo build -p ara-sys --features regen` (see `build.rs`).
//!
//! ARA structs use 1-byte packing on x86/x64 (`#pragma pack(push, 1)`), so the
//! generated structs are `#[repr(C, packed)]` where it matters; the generated
//! layout tests (`cargo test -p ara-sys`) assert the ABI against the C header.
//!
//! # Licensing (r.md #60)
//!
//! This crate is **not** uniformly GPL-3.0-or-later like the rest of daw_01.
//! `vendor/ARA_API/**` and the `src/bindings.rs` generated from it stay under the
//! **Apache License 2.0**, Copyright (c) 2012-2025 Celemony Software GmbH. GPLv3 §7
//! lets a GPL work carry material under such permitted additional terms, and the
//! notice-preservation duty of Apache-2.0 §4 keeps applying to that subtree: do not
//! strip or rewrite the copyright headers in `vendor/ARA_API/*.h`, and do not delete
//! `vendor/ARA_API/LICENSE.txt`. The wrapper written here (`build.rs`, `wrapper.h`,
//! `shim/`, this file) is GPL-3.0-or-later. Attribution lives in the repository's
//! `NOTICE`; the machine-readable declaration lives in `REUSE.toml`.
//!
//! Implementing the ARA API does not make an implementation a derived work of the
//! SDK, so `daw_plugin_host/src/ara/*.rs` is ordinary daw_01 code (GPL-3.0-or-later).
#![allow(clippy::all, clippy::pedantic)]

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
mod bindings {
    include!("bindings.rs");
}

pub use bindings::*;
