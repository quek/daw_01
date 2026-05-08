//! daw_01-bundled instrument / FX plugins. PR-V1 scaffolding for
//! `docs/plan_voicevox_synth.md`.
//!
//! Builtin plugins implement [`crate::plugin_instance::LoadedPlugin`] so
//! they share **every** code path with external CLAP / VST3 plugins on
//! the audio thread (process scheduling, PDC, sidechain wiring, state
//! save / restore, project file format). The only difference is how
//! they are loaded:
//!
//! - External CLAP / VST3: `path` is a filesystem path, the host opens
//!   the binary with `libloading` and calls into the C ABI factory.
//! - Builtin: `path` is a URI like `builtin://daw_01.silence`, and the
//!   host dispatches it to a Rust constructor in this module via
//!   [`load_builtin`].
//!
//! State save / restore: each plugin is responsible for serialising its
//! own parameters via `LoadedPlugin::state_save` / `state_load`. Builtin
//! plugins typically use `bincode` so the on-disk size is small and
//! the format is deterministic.
//!
//! ## PR-V1 deliverable: `Silence`
//!
//! A no-op instrument that emits stereo silence regardless of MIDI
//! input. Used to verify the load → activate → process → state →
//! destroy lifecycle for the Builtin format before VOICEVOX synthesis
//! is wired up in PR-V2. It also serves as the minimal reference
//! implementation for future builtins.
//!
//! ## Adding a new builtin
//!
//! 1. Implement [`crate::plugin_instance::LoadedPlugin`] for your
//!    struct. Stay on the heap (`Box<Self>` is what the loader returns).
//! 2. Register it in [`builtin_descriptors`] with a unique
//!    `builtin://...` URI as `id`.
//! 3. Add a constructor branch to [`load_builtin`].
//!
//! That's it — `plugin_db::scan_system` automatically appends every
//! [`builtin_descriptors`] entry, so the plugin picker UI sees the new
//! plugin without further wiring.

use anyhow::{Result, bail};
use common::plugin_db::{BUILTIN_ID_SILENCE, BUILTIN_ID_VOICEVOX};
use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;

use crate::plugin_instance::{
    AuxInputBuf, HostCallbacks, LoadedPlugin, TimedNoteEvent,
};

mod voicevox;

pub use voicevox::VoicevoxBuiltin;

/// Stable URI prefix used for all builtin plugin identifiers. Never
/// refers to a real filesystem location — `load_builtin` checks the
/// scheme and dispatches to a Rust constructor in this module.
const BUILTIN_URI_PREFIX: &str = "builtin://";

/// Construct a builtin plugin instance from a `builtin://` URI.
/// `_callbacks` is accepted for parity with the CLAP / VST3 loaders;
/// PR-V1 builtins don't request resize / closed callbacks but PR-V2's
/// VOICEVOX plugin will (synthesis progress reporting).
///
/// The list of supported URIs is defined by
/// [`common::plugin_db::builtin_descriptors`] — adding a new constant
/// there + a match arm here is all that's needed to register a new
/// builtin.
pub fn load_builtin(
    uri: &str,
    _callbacks: HostCallbacks,
) -> Result<Box<dyn LoadedPlugin>> {
    if !uri.starts_with(BUILTIN_URI_PREFIX) {
        bail!("builtin loader: expected `builtin://` URI, got {uri:?}");
    }
    match uri {
        BUILTIN_ID_SILENCE => Ok(Box::new(Silence::new()) as Box<dyn LoadedPlugin>),
        BUILTIN_ID_VOICEVOX => Ok(Box::new(VoicevoxBuiltin::new()) as Box<dyn LoadedPlugin>),
        other => bail!("unknown builtin plugin id: {other}"),
    }
}

// ============================================================
// Silence — minimal builtin instrument (PR-V1 reference impl)
// ============================================================

/// A no-op instrument: ignores all MIDI input and writes stereo silence
/// to its output buffers every `process()` call. Used to validate the
/// Builtin format end-to-end before more interesting plugins land.
pub struct Silence {
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    activated: bool,
}

impl Silence {
    fn new() -> Self {
        Self {
            out_l: Vec::new(),
            out_r: Vec::new(),
            activated: false,
        }
    }
}

impl LoadedPlugin for Silence {
    fn id(&self) -> &str {
        BUILTIN_ID_SILENCE
    }

    fn name(&self) -> &str {
        "Silence (builtin)"
    }

    fn format(&self) -> PluginFormat {
        PluginFormat::Builtin
    }

    fn activate(
        &mut self,
        _sample_rate: f64,
        _min_frames: u32,
        max_frames: u32,
    ) -> Result<()> {
        let cap = max_frames as usize;
        self.out_l.clear();
        self.out_l.resize(cap, 0.0);
        self.out_r.clear();
        self.out_r.resize(cap, 0.0);
        self.activated = true;
        Ok(())
    }

    fn deactivate(&mut self) {
        self.activated = false;
    }

    fn start_processing(&mut self) -> Result<()> {
        Ok(())
    }

    fn stop_processing(&mut self) {}

    fn process(
        &mut self,
        frames: u32,
        _events: &[TimedNoteEvent],
        _input_audio: &[&[f32]],
        _aux_inputs: &[AuxInputBuf<'_>],
    ) -> Result<i32> {
        // Capacity was sized at activate(); zero-fill the live window.
        let n_l = (frames as usize).min(self.out_l.len());
        for v in &mut self.out_l[..n_l] {
            *v = 0.0;
        }
        let n_r = (frames as usize).min(self.out_r.len());
        for v in &mut self.out_r[..n_r] {
            *v = 0.0;
        }
        Ok(0)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        match channel {
            0 => Some(&self.out_l),
            1 => Some(&self.out_r),
            _ => None,
        }
    }

    fn drain_out_notes_into(&mut self, _out: &mut Vec<TimedNoteEvent>) {
        // Builtin instrument with no MIDI output.
    }

    fn set_render_mode(&mut self, _mode: RenderMode) -> bool {
        // No internal state changes between Realtime / Offline; signal
        // "accepted" so the host doesn't log a render-mode warning.
        true
    }

    fn query_latency(&mut self) -> u32 {
        0
    }

    fn state_save(&self) -> Result<Option<Vec<u8>>> {
        // Stateless — saving and restoring nothing keeps project files
        // small and round-trips cleanly.
        Ok(None)
    }

    fn state_load(&mut self, _data: &[u8]) -> Result<()> {
        Ok(())
    }

    // --- Embedded GUI (none) ----------------------------------------
    fn gui_is_embed_supported(&self) -> bool {
        false
    }

    fn gui_create_embedded(&mut self) -> Result<()> {
        bail!("Silence builtin has no GUI")
    }

    fn gui_get_size(&self) -> Option<(u32, u32)> {
        None
    }

    fn gui_set_scale(&self, _scale: f64) -> Result<bool> {
        Ok(false)
    }

    fn gui_can_resize(&self) -> bool {
        false
    }

    fn gui_set_parent_hwnd(&self, _hwnd: u64) -> Result<()> {
        bail!("Silence builtin has no GUI")
    }

    fn gui_show(&self) -> Result<bool> {
        Ok(false)
    }

    fn gui_hide(&self) -> Result<()> {
        Ok(())
    }

    fn gui_set_size(&self, _width: u32, _height: u32) -> Result<()> {
        Ok(())
    }

    fn gui_destroy(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_silence_succeeds() {
        let p = load_builtin(BUILTIN_ID_SILENCE, HostCallbacks::noop()).unwrap();
        assert_eq!(p.id(), BUILTIN_ID_SILENCE);
        assert_eq!(p.format(), PluginFormat::Builtin);
    }

    #[test]
    fn load_unknown_builtin_errors() {
        let result = load_builtin("builtin://nope", HostCallbacks::noop());
        assert!(result.is_err());
    }

    #[test]
    fn load_non_builtin_uri_errors() {
        let result = load_builtin("/tmp/whatever.dll", HostCallbacks::noop());
        assert!(result.is_err());
    }

    #[test]
    fn silence_process_writes_zeros() {
        let mut p = Silence::new();
        p.activate(48000.0, 0, 256).unwrap();
        p.start_processing().unwrap();
        // Pretend prior process polluted the buffer.
        p.out_l[0] = 0.7;
        p.out_r[1] = -0.4;
        p.process(128, &[], &[], &[]).unwrap();
        assert!(p.out_l[..128].iter().all(|&v| v == 0.0));
        assert!(p.out_r[..128].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn silence_state_roundtrip_is_empty() {
        let mut p = Silence::new();
        assert!(p.state_save().unwrap().is_none());
        // Loading garbage bytes is still Ok (= stateless).
        p.state_load(&[]).unwrap();
        p.state_load(&[1, 2, 3]).unwrap();
    }
}
