//! Format-agnostic plugin interface used by the audio thread and the
//! plugin-main thread. CLAP (`ClapPlugin`) and VST3 (`Vst3Plugin`) both
//! implement [`LoadedPlugin`], letting `Chain` hold `Box<dyn LoadedPlugin>`
//! and drive either backend with the same call-sites.
//!
//! Keep this trait minimal and behavioural — do not expose format-specific
//! raw pointers. Everything here must work whether the underlying
//! representation is CLAP's `clap_plugin_gui.set_parent` or VST3's
//! `IPlugView::attached(hwnd, "HWND")`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;

use crate::clap_plugin::ClapPlugin;
use crate::vst3_plugin::Vst3Plugin;

/// One MIDI-style transition pushed into the next `process()` call.
#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { key: u8, velocity: f64 },
    Off { key: u8 },
}

/// A note transition scheduled at a specific frame offset inside the next
/// process buffer. The audio thread uses these to feed CLAP's input-event
/// vtable and VST3's `IEventList::addEvent` alike.
#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

/// Host callbacks plugins may trigger on *any* thread (usually the
/// plugin's GUI thread). Implementations must be `Send + Sync` and must
/// not block the caller — plugins often hold an internal lock across
/// these.
#[derive(Clone)]
pub struct HostCallbacks {
    pub on_request_resize: Arc<dyn Fn(u32, u32) + Send + Sync>,
    pub on_closed: Arc<dyn Fn() + Send + Sync>,
}

impl HostCallbacks {
    #[allow(dead_code)]
    pub fn noop() -> Self {
        Self {
            on_request_resize: Arc::new(|_, _| {}),
            on_closed: Arc::new(|| {}),
        }
    }
}

/// The host-side handle to a loaded plugin. Lives on the plugin-main
/// thread; `process()` / `start_processing()` / `stop_processing()` are
/// invoked from the audio thread via raw-pointer snapshots (see
/// `PluginPtr` in `main.rs`).
#[allow(dead_code)] // `format()` is wired up for future UI display.
pub trait LoadedPlugin: Send {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn format(&self) -> PluginFormat;

    // --- lifecycle (plugin-main thread) ---------------------------------
    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()>;
    fn deactivate(&mut self);

    // --- audio-thread entry points --------------------------------------
    fn start_processing(&mut self) -> Result<()>;
    fn stop_processing(&mut self);
    /// Runs one buffer. `events` must be sorted by ascending `time` (CLAP
    /// requirement, also honoured by VST3 for consistency).
    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        input_audio: &[&[f32]],
    ) -> Result<i32>;
    /// Planar output. `None` means "no such channel" (e.g. mono plugin
    /// queried for channel 1).
    fn output_buffer(&self, channel: usize) -> Option<&[f32]>;
    /// Moves MIDI-style events emitted during the previous `process()`
    /// into `out`, draining the plugin's buffer in place (pre-allocated
    /// capacity preserved).
    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>);

    // --- render-mode hint (CLAP `render` ext) ---------------------------
    /// Tell the plugin whether the next `process()` calls are realtime
    /// or offline (during WAV export). Returns `true` if the plugin
    /// accepted the change. CLAP plugins forward to
    /// `clap_plugin_render.set`; backends without the extension return
    /// `false` and continue at whatever mode they were already in.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool;

    // --- persistence (plugin-main thread) -------------------------------
    fn state_save(&self) -> Result<Option<Vec<u8>>>;
    fn state_load(&self, data: &[u8]) -> Result<()>;

    // --- embedded Win32 GUI (plugin-main thread) ------------------------
    //
    // Methods match the existing CLAP `Plugin` inherent impl so the trait
    // impl can forward with `self.gui_xxx(..)` (inherent method resolution
    // wins, so no infinite recursion). VST3 internal state changes that
    // would ordinarily require `&mut self` go through `Cell` / `RefCell`.
    fn gui_is_embed_supported(&self) -> bool;
    fn gui_create_embedded(&mut self) -> Result<()>;
    fn gui_get_size(&self) -> Option<(u32, u32)>;
    fn gui_set_scale(&self, scale: f64) -> Result<bool>;
    fn gui_can_resize(&self) -> bool;
    fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()>;
    fn gui_show(&self) -> Result<bool>;
    fn gui_hide(&self) -> Result<()>;
    fn gui_set_size(&self, width: u32, height: u32) -> Result<()>;
    fn gui_destroy(&mut self);
}

/// Loads a plugin at `path` using the backend selected by `format`.
/// `plugin_id` narrows to a specific descriptor inside a multi-plugin
/// library; empty means "pick the first descriptor".
pub fn load_plugin(
    format: PluginFormat,
    path: &Path,
    plugin_id: &str,
    callbacks: HostCallbacks,
) -> Result<Box<dyn LoadedPlugin>> {
    match format {
        PluginFormat::Clap => {
            let plugin = ClapPlugin::load(path, plugin_id, callbacks)?;
            Ok(Box::new(plugin) as Box<dyn LoadedPlugin>)
        }
        PluginFormat::Vst3 => {
            let plugin = Vst3Plugin::load(path, plugin_id, callbacks)?;
            Ok(Box::new(plugin) as Box<dyn LoadedPlugin>)
        }
    }
}
