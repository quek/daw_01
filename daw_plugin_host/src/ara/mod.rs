//! ARA (Audio Random Access) host layer for `daw_plugin_host`.
//!
//! This is the in-process side of ARA: the ARA model graph lives here, next to
//! the plug-in, and the host controller callbacks (audio access, archiving,
//! model updates, …) are serviced from this process. See `docs/plan_ara2.md`.
//!
//! The layer is built up across the ARA implementation steps — companion-API
//! binding (CLAP first, then VST3), host controllers, document lifecycle, and
//! playback-renderer wiring — so a module-wide `dead_code` allowance is kept
//! while the pieces are wired in. `non_camel_case_types` mirrors the C struct
//! names of the hand-written companion glue (as `clap-sys` / `ara-sys` do).
#![allow(dead_code, non_camel_case_types)]

pub mod audio_source;
pub mod clap_ara;
pub mod document;
pub mod extension;
pub mod host_controllers;
pub mod session;
