//! daw_01-bundled instrument / FX plugins.
//!
//! Builtin plugins implement [`crate::plugin_instance::LoadedPlugin`] (+
//! a separate [`crate::plugin_instance::AudioProcessorHalf`], split-half
//! design) so they share **every** code path with external CLAP / VST3
//! plugins on the audio thread (process scheduling, PDC, sidechain wiring,
//! state save / restore, project file format). The only difference is how
//! they are loaded:
//!
//! - External CLAP / VST3: `path` is a filesystem path, the host opens
//!   the binary with `libloading` and calls into the C ABI factory.
//! - Builtin: `path` is a URI like `builtin://daw_01.silence`, and the
//!   host dispatches it to a Rust constructor in this module via
//!   [`load_builtin`].
//!
//! ## Adding a new builtin
//!
//! 1. Implement `LoadedPlugin` (+ an audio half) for your struct.
//! 2. Register it in `common::plugin_db::builtin_descriptors` with a unique
//!    `builtin://...` URI as `id`.
//! 3. Add a constructor branch to [`load_builtin`].

use std::sync::Arc;

use anyhow::{Result, bail};
use common::plugin_db::{BUILTIN_ID_SILENCE, BUILTIN_ID_VOICEVOX};
use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;

use crate::plugin_instance::{
    AudioHalf, AudioProcessorHalf, AuxInputBuf, HostCallbacks, LoadedPlugin, TimedNoteEvent,
};

mod voicevox;
mod voicevox_render;
mod voicevox_synth;

pub use voicevox::VoicevoxBuiltin;

/// Stable URI prefix used for all builtin plugin identifiers.
const BUILTIN_URI_PREFIX: &str = "builtin://";

/// Construct a builtin plugin instance from a `builtin://` URI.
/// `callbacks` is the same host-callback bundle CLAP / VST3 loaders get;
/// VOICEVOX uses `on_vocal_synth_status` for synthesis progress reporting.
pub fn load_builtin(
    uri: &str,
    callbacks: HostCallbacks,
) -> Result<Box<dyn LoadedPlugin>> {
    if !uri.starts_with(BUILTIN_URI_PREFIX) {
        bail!("builtin loader: expected `builtin://` URI, got {uri:?}");
    }
    match uri {
        BUILTIN_ID_SILENCE => Ok(Box::new(Silence::new()) as Box<dyn LoadedPlugin>),
        BUILTIN_ID_VOICEVOX => Ok(Box::new(VoicevoxBuiltin::new(
            callbacks.on_vocal_synth_status.clone(),
        )) as Box<dyn LoadedPlugin>),
        other => bail!("unknown builtin plugin id: {other}"),
    }
}

// ============================================================
// Silence — minimal builtin instrument (reference impl)
// ============================================================

/// Audio half of [`Silence`]: writes stereo silence every `process()`.
struct SilenceAudioHalf {
    out_l: Vec<f32>,
    out_r: Vec<f32>,
}

impl AudioProcessorHalf for SilenceAudioHalf {
    fn process(
        &mut self,
        frames: u32,
        _events: &[TimedNoteEvent],
        _param_events: &[crate::plugin_instance::TimedParamEvent],
        _input_audio: &[&[f32]],
        _aux_inputs: &[AuxInputBuf<'_>],
        _transport: &crate::plugin_instance::TransportContext,
    ) -> Result<i32> {
        // Capacity was sized at on_activate(); zero-fill the live window.
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

    fn on_activate(&mut self, _sample_rate: f64, max_frames: u32) {
        let cap = max_frames as usize;
        self.out_l.clear();
        self.out_l.resize(cap, 0.0);
        self.out_r.clear();
        self.out_r.resize(cap, 0.0);
    }
}

/// A no-op instrument: ignores all MIDI input and outputs stereo silence.
/// Validates the Builtin format end-to-end and serves as the minimal
/// reference implementation for future builtins.
pub struct Silence {
    activated: bool,
    audio: Arc<AudioHalf>,
}

impl Silence {
    fn new() -> Self {
        Self {
            activated: false,
            audio: AudioHalf::new(Box::new(SilenceAudioHalf {
                out_l: Vec::new(),
                out_r: Vec::new(),
            })),
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

    fn audio_half(&self) -> Arc<AudioHalf> {
        Arc::clone(&self.audio)
    }

    fn activate(
        &mut self,
        sample_rate: f64,
        _min_frames: u32,
        max_frames: u32,
    ) -> Result<()> {
        // SAFETY: quiesced window (install / reinit call sites).
        unsafe { self.audio.get().on_activate(sample_rate, max_frames) };
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

    fn set_render_mode(&mut self, _mode: RenderMode) -> bool {
        // No internal state changes between Realtime / Offline.
        true
    }

    fn query_latency(&mut self) -> u32 {
        0
    }

    fn state_save(&self) -> Result<Option<Vec<u8>>> {
        // Stateless.
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

    fn gui_set_parent_hwnd(&self, _hwnd: u64) -> Result<()> {
        bail!("Silence builtin has no GUI")
    }

    fn gui_show(&self) -> Result<bool> {
        Ok(false)
    }

    fn gui_hide(&self) -> Result<()> {
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
        let mut h = SilenceAudioHalf {
            out_l: Vec::new(),
            out_r: Vec::new(),
        };
        h.on_activate(48000.0, 256);
        // Pretend prior process polluted the buffer.
        h.out_l[0] = 0.7;
        h.out_r[1] = -0.4;
        let transport = crate::plugin_instance::TransportContext::from_process_data(
            &common::process_data::ProcessData::empty(),
        );
        h.process(128, &[], &[], &[], &[], &transport).unwrap();
        assert!(h.out_l[..128].iter().all(|&v| v == 0.0));
        assert!(h.out_r[..128].iter().all(|&v| v == 0.0));
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
