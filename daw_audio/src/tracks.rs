//! Audio engine routing snapshot. Contains everything the audio worker
//! needs to process a single buffer for a track: mixer params, the vocal
//! sample-playback handle, and per-stage plugin shmem refs.
//!
//! Owned by daw_audio. The plugin-host counterpart lives in
//! `daw_plugin_host::process_server` and only knows about plugin
//! instances — it has no view of tracks or song structure.

#![allow(dead_code)]

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use common::plugin_ref::PluginRef;
use common::track_params::TrackAudioParams;

use crate::vocal::VocalAudio;

pub struct TrackRouting {
    pub track_id: u32,
    pub params: Arc<TrackAudioParams>,
    pub vocal: Arc<ArcSwapOption<VocalAudio>>,
    pub midi_fx_chain: Vec<PluginRef>,
    pub instrument: Option<PluginRef>,
    pub fx_chain: Vec<PluginRef>,
}

pub struct AudioRouting {
    pub tracks: Vec<TrackRouting>,
}

impl AudioRouting {
    pub fn empty() -> Self {
        Self { tracks: Vec::new() }
    }
}

impl Default for AudioRouting {
    fn default() -> Self {
        Self::empty()
    }
}
