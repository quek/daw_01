use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, clap_event_header,
    clap_event_note, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{
    CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::gui::{
    CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_plugin_gui, clap_window, clap_window_handle,
};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_plugin_note_ports};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::clap_process;
use clap_sys::version::clap_version_is_compatible;
use libloading::{Library, Symbol};

/// A single transition to inject into the next process() call.
#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { key: u8, velocity: f64 },
    Off { key: u8 },
}

/// A note transition scheduled at a specific frame offset within the next
/// process() buffer.
#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

use crate::clap_host::{Host, HostCallbacks};

/// Loaded CLAP plugin instance. Holds every resource alive until dropped; the
/// drop order (custom cleanup → fields → Library) ensures `destroy` / `deinit`
/// run before the DLL is unloaded.
pub struct Plugin {
    _library: Library,
    entry: *const clap_plugin_entry,
    plugin: *const clap_plugin,
    _host: Box<Host>,
    /// Stable `clap_plugin_descriptor.id` of the loaded descriptor.
    id: String,
    name: String,
    path: PathBuf,
    active: bool,
    processing: bool,
    input_channels: u32,
    input_buffers: Vec<Vec<f32>>,
    input_ptrs: Vec<*mut f32>,
    output_channels: u32,
    output_buffers: Vec<Vec<f32>>,
    output_ptrs: Vec<*mut f32>,
    /// Pre-allocated input event buffer; filled by process() per call.
    pending_events: Vec<clap_event_note>,
    /// Notes emitted by the plugin during the previous `process()` call.
    /// Populated by the `out_events.try_push` callback and drained by the
    /// caller (e.g. MIDI FX chain) before the next process().
    collected_out_notes: Vec<TimedNoteEvent>,
    /// `clap_plugin_gui` vtable pointer, looked up once after init.
    /// `None` means the plugin does not declare the gui extension.
    gui_ext: Option<*const clap_plugin_gui>,
    /// Whether `gui.create` has been called successfully and `gui.destroy`
    /// has not yet been called. Used by Drop to tear down cleanly.
    gui_created: bool,
    /// `clap_plugin_state` vtable pointer. `None` when the plugin does not
    /// implement the state extension (project save/load will skip it).
    state_ext: Option<*const clap_plugin_state>,
}

// The plugin holds raw pointers but ownership is exclusive within the struct.
unsafe impl Send for Plugin {}

impl Plugin {
    /// Tries to load a plugin from `path`. Scans all descriptors in the file
    /// and instantiates the first one matching `target_id` when provided, or
    /// otherwise the first one for which `matches(features)` returns true.
    /// Returns `Ok(None)` if no descriptor matches (library is unloaded cleanly).
    ///
    /// `callbacks` wires host-side CLAP GUI events (resize / close) back to
    /// the caller.
    pub fn load_matching<F>(
        path: &Path,
        target_id: Option<&str>,
        matches: F,
        callbacks: HostCallbacks,
    ) -> Result<Option<Self>>
    where
        F: Fn(&[String]) -> bool,
    {
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("failed to load CLAP library at {}", path.display()))?;

        let entry_ptr: *const clap_plugin_entry = unsafe {
            let sym: Symbol<*const clap_plugin_entry> = library
                .get(b"clap_entry\0")
                .context("CLAP library does not export clap_entry symbol")?;
            *sym
        };
        anyhow::ensure!(!entry_ptr.is_null(), "clap_entry symbol is null");
        let entry = unsafe { &*entry_ptr };

        anyhow::ensure!(
            clap_version_is_compatible(entry.clap_version),
            "CLAP version {}.{}.{} is incompatible with host",
            entry.clap_version.major,
            entry.clap_version.minor,
            entry.clap_version.revision
        );

        let path_str = path.to_string_lossy();
        let c_path = CString::new(path_str.as_bytes())
            .context("plugin path contains interior nul byte")?;
        let init_fn = entry.init.context("clap_plugin_entry::init is null")?;
        anyhow::ensure!(
            unsafe { init_fn(c_path.as_ptr()) },
            "clap_entry.init returned false for {}",
            path.display()
        );

        let get_factory = entry
            .get_factory
            .context("clap_plugin_entry::get_factory is null")?;
        let factory_ptr = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) }
            as *const clap_plugin_factory;
        anyhow::ensure!(!factory_ptr.is_null(), "clap_plugin_factory is null");
        let factory = unsafe { &*factory_ptr };

        let get_count = factory
            .get_plugin_count
            .context("factory.get_plugin_count is null")?;
        let get_desc = factory
            .get_plugin_descriptor
            .context("factory.get_plugin_descriptor is null")?;
        let create = factory
            .create_plugin
            .context("factory.create_plugin is null")?;

        let count = unsafe { get_count(factory_ptr) };
        tracing::info!(path = %path.display(), count, "plugins in factory");

        let mut selected: Option<u32> = None;
        for i in 0..count {
            let desc_ptr = unsafe { get_desc(factory_ptr, i) };
            if desc_ptr.is_null() {
                continue;
            }
            let desc = unsafe { &*desc_ptr };
            log_descriptor(i, desc);
            if selected.is_some() {
                continue;
            }
            if let Some(want) = target_id {
                // Exact ID match takes precedence over feature matching when
                // the caller asks for a specific descriptor (project-load path).
                if c_str_to_string(desc.id) == want {
                    selected = Some(i);
                }
            } else {
                let features = read_feature_list(desc.features);
                if matches(&features) {
                    selected = Some(i);
                }
            }
        }

        let Some(index) = selected else {
            // No descriptor matched — unload cleanly and report no match.
            if let Some(deinit) = entry.deinit {
                unsafe { deinit() };
            }
            drop(library);
            return Ok(None);
        };

        let desc_ptr = unsafe { get_desc(factory_ptr, index) };
        anyhow::ensure!(!desc_ptr.is_null(), "selected descriptor became null");
        let desc = unsafe { &*desc_ptr };
        let plugin_id = desc.id;
        let id = c_str_to_string(plugin_id);
        let name = c_str_to_string(desc.name);

        let host = Host::new(callbacks);
        let host_ptr: *const clap_host = &host.clap;

        let plugin_ptr = unsafe { create(factory_ptr, host_ptr, plugin_id) };
        anyhow::ensure!(!plugin_ptr.is_null(), "create_plugin returned null");

        let plugin_init = unsafe { (*plugin_ptr).init }.context("clap_plugin::init is null")?;
        anyhow::ensure!(
            unsafe { plugin_init(plugin_ptr) },
            "clap_plugin.init returned false"
        );
        tracing::info!(%name, index, "plugin initialized");

        let get_ext = unsafe { (*plugin_ptr).get_extension }
            .context("clap_plugin::get_extension is null")?;
        log_audio_ports(plugin_ptr, get_ext);
        log_note_ports(plugin_ptr, get_ext);
        let input_channels = query_port_channel_count(plugin_ptr, get_ext, true);
        let output_channels = query_output_channel_count(plugin_ptr, get_ext);
        tracing::info!(
            input_channels,
            output_channels,
            "plugin audio channel count"
        );

        // Look up optional clap.gui extension; missing → embedded GUI not supported.
        let gui_ptr = unsafe { get_ext(plugin_ptr, CLAP_EXT_GUI.as_ptr()) } as *const clap_plugin_gui;
        let gui_ext = if gui_ptr.is_null() { None } else { Some(gui_ptr) };
        tracing::info!(has_gui = gui_ext.is_some(), "plugin gui extension");

        // Look up optional clap.state extension for project save / restore.
        let state_ptr = unsafe { get_ext(plugin_ptr, CLAP_EXT_STATE.as_ptr()) }
            as *const clap_plugin_state;
        let state_ext = if state_ptr.is_null() {
            None
        } else {
            Some(state_ptr)
        };
        tracing::info!(has_state = state_ext.is_some(), "plugin state extension");

        Ok(Some(Self {
            _library: library,
            entry: entry_ptr,
            plugin: plugin_ptr,
            _host: host,
            id,
            name,
            path: path.to_path_buf(),
            active: false,
            processing: false,
            input_channels,
            input_buffers: Vec::new(),
            input_ptrs: Vec::new(),
            output_channels,
            output_buffers: Vec::new(),
            output_ptrs: Vec::new(),
            // Capacity sized so 64 events from the song plus arpeggio /
            // MIDI FX expansion (up to ~4 stages * 64) never trigger an
            // allocation in the audio thread. Plugins exceeding this just
            // re-alloc; we log on the audio side if it happens.
            pending_events: Vec::with_capacity(256),
            collected_out_notes: Vec::with_capacity(256),
            gui_ext,
            gui_created: false,
            state_ext,
        }))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    // --- GUI extension wrappers ------------------------------------------

    /// Returns true when the plugin advertises the `clap.gui` extension and
    /// supports embedded (non-floating) Win32 windows.
    pub fn gui_is_embed_supported(&self) -> bool {
        let Some(gui) = self.gui_ext.and_then(|p| unsafe { p.as_ref() }) else {
            return false;
        };
        let Some(f) = gui.is_api_supported else { return false };
        unsafe { f(self.plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) }
    }

    /// Create the plugin's embedded (Win32) GUI resources. Idempotent: calling
    /// twice returns success without a second `create`.
    pub fn gui_create_embedded(&mut self) -> Result<()> {
        if self.gui_created {
            return Ok(());
        }
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let create = gui.create.context("gui.create is null")?;
        anyhow::ensure!(
            unsafe { create(self.plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) },
            "gui.create returned false"
        );
        self.gui_created = true;
        Ok(())
    }

    /// Returns the plugin's preferred initial size, or `None` if the call
    /// fails. Must be called between `create` and `set_parent`/`show`.
    pub fn gui_get_size(&self) -> Option<(u32, u32)> {
        let gui = self.gui_ref()?;
        let f = gui.get_size?;
        let mut w = 0u32;
        let mut h = 0u32;
        if unsafe { f(self.plugin, &mut w, &mut h) } {
            Some((w, h))
        } else {
            None
        }
    }

    /// Tells the plugin the host-side scaling factor. Returning `false` is
    /// allowed (plugin preferred to compute its own scale); propagate only
    /// hard errors (extension missing / function pointer null).
    pub fn gui_set_scale(&self, scale: f64) -> Result<bool> {
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let Some(f) = gui.set_scale else {
            // Optional entry; treat missing as "plugin ignored the scale".
            return Ok(false);
        };
        Ok(unsafe { f(self.plugin, scale) })
    }

    /// Returns true if the plugin supports mouse-drag resize. `false` when
    /// the extension is missing or the plugin reports non-resizable.
    pub fn gui_can_resize(&self) -> bool {
        let Some(gui) = self.gui_ref() else { return false };
        let Some(f) = gui.can_resize else { return false };
        unsafe { f(self.plugin) }
    }

    /// Embed into the given host Win32 HWND (passed as a raw `u64` pointer).
    pub fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()> {
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let f = gui.set_parent.context("gui.set_parent is null")?;
        let window = clap_window {
            api: CLAP_WINDOW_API_WIN32.as_ptr(),
            specific: clap_window_handle {
                win32: hwnd as *mut c_void,
            },
        };
        anyhow::ensure!(
            unsafe { f(self.plugin, &window) },
            "gui.set_parent returned false"
        );
        Ok(())
    }

    /// Calls `gui.show`. Returns `Ok(true)` if the plugin reports success,
    /// `Ok(false)` if the plugin returns false (some plugins — e.g. VCV
    /// Rack 2 — return false even after successfully showing, as a no-op
    /// sentinel). Only returns `Err` if the function pointer is missing.
    pub fn gui_show(&self) -> Result<bool> {
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let f = gui.show.context("gui.show is null")?;
        Ok(unsafe { f(self.plugin) })
    }

    pub fn gui_hide(&self) -> Result<()> {
        // Plugins reject hide() calls when create() hasn't happened (or was
        // already rolled back by the plugin on a failed show). Skip silently.
        if !self.gui_created {
            return Ok(());
        }
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let f = gui.hide.context("gui.hide is null")?;
        anyhow::ensure!(unsafe { f(self.plugin) }, "gui.hide returned false");
        Ok(())
    }

    pub fn gui_set_size(&self, width: u32, height: u32) -> Result<()> {
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let f = gui.set_size.context("gui.set_size is null")?;
        anyhow::ensure!(
            unsafe { f(self.plugin, width, height) },
            "gui.set_size returned false"
        );
        Ok(())
    }

    /// Tear down the GUI. Safe to call even if `gui_create_embedded` was not
    /// called (no-op in that case). Idempotent.
    pub fn gui_destroy(&mut self) {
        if !self.gui_created {
            return;
        }
        if let Some(gui) = self.gui_ref()
            && let Some(f) = gui.destroy
        {
            unsafe { f(self.plugin) };
        }
        self.gui_created = false;
    }

    fn gui_ref(&self) -> Option<&clap_plugin_gui> {
        self.gui_ext.and_then(|p| unsafe { p.as_ref() })
    }

    pub fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()> {
        anyhow::ensure!(!self.active, "plugin already active");
        let activate = unsafe { (*self.plugin).activate }.context("plugin.activate is null")?;
        anyhow::ensure!(
            unsafe { activate(self.plugin, sample_rate, min_frames, max_frames) },
            "plugin.activate returned false"
        );
        self.active = true;
        self.input_buffers = (0..self.input_channels as usize)
            .map(|_| vec![0.0f32; max_frames as usize])
            .collect();
        self.input_ptrs = vec![std::ptr::null_mut(); self.input_channels as usize];
        self.output_buffers = (0..self.output_channels as usize)
            .map(|_| vec![0.0f32; max_frames as usize])
            .collect();
        self.output_ptrs = vec![std::ptr::null_mut(); self.output_channels as usize];
        tracing::info!(sample_rate, max_frames, "plugin activated");
        Ok(())
    }

    #[allow(dead_code)]
    pub fn input_channels(&self) -> u32 {
        self.input_channels
    }

    #[allow(dead_code)]
    pub fn output_channels(&self) -> u32 {
        self.output_channels
    }

    /// Drain note events collected during the previous `process()` into
    /// `out`, preserving the plugin's pre-allocated capacity. RT-safe:
    /// `Vec::append` moves elements and leaves `self.collected_out_notes`
    /// empty with its capacity intact (unlike `mem::take`, which would
    /// replace the buffer with a freshly-allocated empty one).
    pub fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>) {
        out.append(&mut self.collected_out_notes);
    }

    pub fn start_processing(&mut self) -> Result<()> {
        anyhow::ensure!(self.active, "plugin not active");
        anyhow::ensure!(!self.processing, "plugin already processing");
        let start = unsafe { (*self.plugin).start_processing }
            .context("plugin.start_processing is null")?;
        anyhow::ensure!(
            unsafe { start(self.plugin) },
            "plugin.start_processing returned false"
        );
        self.processing = true;
        Ok(())
    }

    pub fn stop_processing(&mut self) {
        if !self.processing {
            return;
        }
        if let Some(stop) = unsafe { (*self.plugin).stop_processing } {
            unsafe { stop(self.plugin) };
        }
        self.processing = false;
    }

    pub fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        if let Some(deact) = unsafe { (*self.plugin).deactivate } {
            unsafe { deact(self.plugin) };
        }
        self.active = false;
        self.output_buffers.clear();
        self.output_ptrs.clear();
    }

    /// Calls the plugin's process() with optional input audio and zero or
    /// more timed note events. Must be called on the audio thread only.
    /// Events must be sorted by ascending `time` (CLAP requirement).
    ///
    /// `input_audio[c]` feeds channel `c` of the plugin's first input port.
    /// Channels beyond `self.input_channels` are ignored; channels the
    /// plugin expects but weren't provided are filled with silence.
    /// Pass an empty slice when processing an instrument (no audio input).
    ///
    /// Fills `output_buffers` (planar) with the rendered audio. Any note
    /// events the plugin emits via `out_events.try_push` are collected into
    /// `self.collected_out_notes` (drained with `take_out_notes`).
    pub fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        input_audio: &[&[f32]],
    ) -> Result<i32> {
        anyhow::ensure!(self.processing, "plugin not processing");

        self.pending_events.clear();
        for ev in events {
            let mut e = encode_note(ev.event);
            e.header.time = ev.time;
            self.pending_events.push(e);
        }
        self.collected_out_notes.clear();

        // Copy caller-provided audio into our pre-allocated input buffers.
        // Channels not supplied by the caller are zeroed so the plugin
        // doesn't see stale data.
        let n = frames as usize;
        for (ch, buf) in self.input_buffers.iter_mut().enumerate() {
            let buf_len = buf.len();
            let cap = n.min(buf_len);
            if ch < input_audio.len() {
                let src = input_audio[ch];
                let copy_n = cap.min(src.len());
                buf[..copy_n].copy_from_slice(&src[..copy_n]);
                if copy_n < cap {
                    buf[copy_n..cap].fill(0.0);
                }
            } else {
                buf[..cap].fill(0.0);
            }
        }
        for i in 0..self.input_buffers.len() {
            self.input_ptrs[i] = self.input_buffers[i].as_mut_ptr();
        }
        // Refresh output channel pointers (buffers are pre-allocated).
        for i in 0..self.output_buffers.len() {
            self.output_ptrs[i] = self.output_buffers[i].as_mut_ptr();
        }

        let audio_in = clap_audio_buffer {
            data32: self.input_ptrs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: self.input_channels,
            latency: 0,
            constant_mask: 0,
        };
        let mut audio_out = clap_audio_buffer {
            data32: self.output_ptrs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: self.output_channels,
            latency: 0,
            constant_mask: 0,
        };

        let in_events = clap_input_events {
            ctx: std::ptr::from_ref(&self.pending_events) as *mut c_void,
            size: Some(in_events_size),
            get: Some(in_events_get),
        };
        let out_events = clap_output_events {
            ctx: std::ptr::from_mut(&mut self.collected_out_notes) as *mut c_void,
            try_push: Some(collect_out_note_try_push),
        };

        let (audio_inputs, audio_inputs_count) = if self.input_channels == 0 {
            (std::ptr::null(), 0)
        } else {
            (&raw const audio_in, 1)
        };
        let (audio_outputs, audio_outputs_count) = if self.output_channels == 0 {
            (std::ptr::null_mut(), 0)
        } else {
            (&raw mut audio_out, 1)
        };

        let process_ctx = clap_process {
            steady_time: -1,
            frames_count: frames,
            transport: std::ptr::null(),
            audio_inputs,
            audio_outputs,
            audio_inputs_count,
            audio_outputs_count,
            in_events: &in_events,
            out_events: &out_events,
        };

        let process = unsafe { (*self.plugin).process }.context("plugin.process is null")?;
        let status = unsafe { process(self.plugin, &process_ctx) };
        Ok(status)
    }

    pub fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        self.output_buffers.get(channel).map(Vec::as_slice)
    }

    /// Loads a plugin in the file. If `target_id` is non-empty, selects that
    /// specific descriptor; otherwise loads the first descriptor in the file.
    pub fn load(path: &Path, target_id: &str, callbacks: HostCallbacks) -> Result<Self> {
        let opt_id = if target_id.is_empty() {
            None
        } else {
            Some(target_id)
        };
        Self::load_matching(path, opt_id, |_| true, callbacks)?.ok_or_else(|| {
            if target_id.is_empty() {
                anyhow::anyhow!("no plugins in {}", path.display())
            } else {
                anyhow::anyhow!("plugin id '{}' not found in {}", target_id, path.display())
            }
        })
    }

    // --- State extension wrappers ----------------------------------------

    /// Serializes the plugin's internal state via `clap_plugin_state.save`.
    /// Returns `Ok(None)` if the plugin does not implement the state
    /// extension. Runs on the CLAP main-thread.
    pub fn state_save(&self) -> Result<Option<Vec<u8>>> {
        let Some(state) = self.state_ext else {
            return Ok(None);
        };
        let state = unsafe { &*state };
        let Some(save) = state.save else {
            return Ok(None);
        };
        let mut buf: Vec<u8> = Vec::new();
        let stream = clap_ostream {
            ctx: std::ptr::from_mut(&mut buf) as *mut c_void,
            write: Some(stream_write),
        };
        let ok = unsafe { save(self.plugin, &stream) };
        anyhow::ensure!(ok, "plugin_state.save returned false");
        Ok(Some(buf))
    }

    /// Restores previously captured state via `clap_plugin_state.load`. No-op
    /// when the extension is missing (returns `Ok(())` silently so project
    /// loads tolerate plugin upgrades that dropped state support).
    pub fn state_load(&self, data: &[u8]) -> Result<()> {
        let Some(state) = self.state_ext else {
            tracing::warn!("plugin has no state extension; skipping state restore");
            return Ok(());
        };
        let state = unsafe { &*state };
        let Some(load) = state.load else {
            tracing::warn!("plugin_state.load is null; skipping state restore");
            return Ok(());
        };
        let mut cursor = StateCursor { data, pos: 0 };
        let stream = clap_istream {
            ctx: std::ptr::from_mut(&mut cursor) as *mut c_void,
            read: Some(stream_read),
        };
        let ok = unsafe { load(self.plugin, &stream) };
        anyhow::ensure!(ok, "plugin_state.load returned false");
        Ok(())
    }
}

/// Read-only cursor over a `&[u8]` used by the istream callback below.
struct StateCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

/// `clap_ostream.write`: write up to `size` bytes from `buffer` into the
/// `Vec<u8>` referenced by `ctx`. Returns number of bytes written, or -1 on
/// error. We never fail to grow a `Vec`, so return the full size.
unsafe extern "C" fn stream_write(
    stream: *const clap_ostream,
    buffer: *const c_void,
    size: u64,
) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let ctx = unsafe { (*stream).ctx } as *mut Vec<u8>;
    if ctx.is_null() {
        return -1;
    }
    let Ok(n) = usize::try_from(size) else {
        return -1;
    };
    let slice = unsafe { std::slice::from_raw_parts(buffer as *const u8, n) };
    unsafe { (*ctx).extend_from_slice(slice) };
    n as i64
}

/// `clap_istream.read`: copy up to `size` bytes from the cursor into the
/// plugin's buffer. Returns number actually read (0 on EOF) or -1 on error.
unsafe extern "C" fn stream_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    if stream.is_null() || buffer.is_null() {
        return -1;
    }
    let ctx = unsafe { (*stream).ctx } as *mut StateCursor<'_>;
    if ctx.is_null() {
        return -1;
    }
    let Ok(want) = usize::try_from(size) else {
        return -1;
    };
    let cursor = unsafe { &mut *ctx };
    let remaining = cursor.data.len().saturating_sub(cursor.pos);
    let n = want.min(remaining);
    if n == 0 {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            cursor.data.as_ptr().add(cursor.pos),
            buffer as *mut u8,
            n,
        )
    };
    cursor.pos += n;
    n as i64
}

/// `try_push` callback for the MIDI FX chain. `ctx` is a
/// `*mut Vec<TimedNoteEvent>`; we decode `NOTE_ON`/`NOTE_OFF` events and
/// append them for the next plugin to consume. Unknown types are accepted
/// (return `true`) but not recorded.
unsafe extern "C" fn collect_out_note_try_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    if list.is_null() || event.is_null() {
        return false;
    }
    let ctx = unsafe { (*list).ctx } as *mut Vec<TimedNoteEvent>;
    if ctx.is_null() {
        return true;
    }
    let header = unsafe { &*event };
    if header.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return true;
    }
    let transition = match header.type_ {
        t if t == CLAP_EVENT_NOTE_ON => {
            let note = unsafe { &*(event as *const clap_event_note) };
            Some(NoteTransition::On {
                key: note.key.clamp(0, 127) as u8,
                velocity: note.velocity,
            })
        }
        t if t == CLAP_EVENT_NOTE_OFF => {
            let note = unsafe { &*(event as *const clap_event_note) };
            Some(NoteTransition::Off {
                key: note.key.clamp(0, 127) as u8,
            })
        }
        _ => None,
    };
    if let Some(transition) = transition {
        let out = unsafe { &mut *ctx };
        out.push(TimedNoteEvent {
            time: header.time,
            event: transition,
        });
    }
    true
}

/// `ctx` of the in_events vtable points to `&Vec<clap_event_note>`.
unsafe extern "C" fn in_events_size(list: *const clap_input_events) -> u32 {
    let ctx = unsafe { (*list).ctx } as *const Vec<clap_event_note>;
    if ctx.is_null() {
        return 0;
    }
    unsafe { (*ctx).len() as u32 }
}

unsafe extern "C" fn in_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let ctx = unsafe { (*list).ctx } as *const Vec<clap_event_note>;
    if ctx.is_null() {
        return std::ptr::null();
    }
    let events = unsafe { &*ctx };
    let Some(ev) = events.get(index as usize) else {
        return std::ptr::null();
    };
    std::ptr::from_ref(&ev.header)
}

fn encode_note(transition: NoteTransition) -> clap_event_note {
    let (type_, key, velocity) = match transition {
        NoteTransition::On { key, velocity } => (CLAP_EVENT_NOTE_ON, key, velocity),
        NoteTransition::Off { key } => (CLAP_EVENT_NOTE_OFF, key, 0.0),
    };
    clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_,
            flags: 0,
        },
        note_id: -1,
        port_index: 0,
        channel: 0,
        key: key as i16,
        velocity,
    }
}

fn query_output_channel_count(plugin: *const clap_plugin, get_ext: GetExtFn) -> u32 {
    query_port_channel_count(plugin, get_ext, false)
}

/// Queries the plugin's first audio port in the given direction. `is_input`
/// selects input vs output ports. Returns `0` when the plugin declares no
/// port of that direction (e.g. instrument with no audio input, or pure
/// note effect with no audio output), so the audio thread can skip routing.
fn query_port_channel_count(
    plugin: *const clap_plugin,
    get_ext: GetExtFn,
    is_input: bool,
) -> u32 {
    let ext_ptr =
        unsafe { get_ext(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) } as *const clap_plugin_audio_ports;
    if ext_ptr.is_null() {
        // Extension missing entirely — assume stereo output, no input.
        return if is_input { 0 } else { 2 };
    }
    let ext = unsafe { &*ext_ptr };
    let count_fn = match ext.count {
        Some(f) => f,
        None => return if is_input { 0 } else { 2 },
    };
    let port_count = unsafe { count_fn(plugin, is_input) };
    if port_count == 0 {
        return 0;
    }
    let Some(get) = ext.get else {
        return if is_input { 0 } else { 2 };
    };
    let mut info = std::mem::MaybeUninit::<clap_audio_port_info>::zeroed();
    let ok = unsafe { get(plugin, 0, is_input, info.as_mut_ptr()) };
    if !ok {
        return if is_input { 0 } else { 2 };
    }
    unsafe { info.assume_init() }.channel_count
}

impl Drop for Plugin {
    fn drop(&mut self) {
        // Tear down GUI resources first so plugin.destroy sees a clean state.
        self.gui_destroy();
        unsafe {
            if let Some(destroy) = (*self.plugin).destroy {
                destroy(self.plugin);
            }
            if let Some(deinit) = (*self.entry).deinit {
                deinit();
            }
        }
        tracing::info!(name = %self.name, path = %self.path.display(), "plugin destroyed");
    }
}

fn c_str_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn read_feature_list(ptr: *const *const c_char) -> Vec<String> {
    let mut out = Vec::new();
    if ptr.is_null() {
        return out;
    }
    let mut p = ptr;
    unsafe {
        loop {
            let s_ptr = *p;
            if s_ptr.is_null() {
                break;
            }
            out.push(CStr::from_ptr(s_ptr).to_string_lossy().into_owned());
            p = p.add(1);
        }
    }
    out
}

fn log_descriptor(index: u32, desc: &clap_plugin_descriptor) {
    let id = c_str_to_string(desc.id);
    let name = c_str_to_string(desc.name);
    let vendor = c_str_to_string(desc.vendor);
    let version = c_str_to_string(desc.version);
    let features = read_feature_list(desc.features);
    tracing::info!(index, %id, %name, %vendor, %version, ?features, "plugin descriptor");
}

type GetExtFn = unsafe extern "C" fn(*const clap_plugin, *const c_char) -> *const c_void;

fn log_audio_ports(plugin: *const clap_plugin, get_ext: GetExtFn) {
    let ext_ptr = unsafe { get_ext(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) }
        as *const clap_plugin_audio_ports;
    if ext_ptr.is_null() {
        tracing::info!("audio-ports extension: not provided");
        return;
    }
    let ext = unsafe { &*ext_ptr };
    let Some(count_fn) = ext.count else {
        tracing::warn!("audio-ports.count is null");
        return;
    };
    let inputs = unsafe { count_fn(plugin, true) };
    let outputs = unsafe { count_fn(plugin, false) };
    tracing::info!(inputs, outputs, "audio-ports");
}

fn log_note_ports(plugin: *const clap_plugin, get_ext: GetExtFn) {
    let ext_ptr = unsafe { get_ext(plugin, CLAP_EXT_NOTE_PORTS.as_ptr()) }
        as *const clap_plugin_note_ports;
    if ext_ptr.is_null() {
        tracing::info!("note-ports extension: not provided");
        return;
    }
    let ext = unsafe { &*ext_ptr };
    let Some(count_fn) = ext.count else {
        tracing::warn!("note-ports.count is null");
        return;
    };
    let inputs = unsafe { count_fn(plugin, true) };
    let outputs = unsafe { count_fn(plugin, false) };
    tracing::info!(inputs, outputs, "note-ports");
}
