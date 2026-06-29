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
#![allow(clippy::all, clippy::pedantic)]

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
mod bindings {
    include!("bindings.rs");
}

pub use bindings::*;
