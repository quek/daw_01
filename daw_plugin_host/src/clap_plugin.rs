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
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::gui::{
    CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_plugin_gui, clap_window, clap_window_handle,
};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_plugin_note_ports};
use clap_sys::events::{
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE,
    CLAP_TRANSPORT_IS_PLAYING, clap_event_param_mod, clap_event_param_value, clap_event_transport,
};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_HIDDEN,
    CLAP_PARAM_IS_MODULATABLE, CLAP_PARAM_IS_PERIODIC, CLAP_PARAM_IS_READONLY,
    CLAP_PARAM_IS_STEPPED, CLAP_PARAM_REQUIRES_PROCESS, clap_param_info,
    clap_plugin_params,
};
use clap_sys::ext::render::{
    CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME, clap_plugin_render,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::clap_process;
use clap_sys::version::clap_version_is_compatible;
use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;
use libloading::{Library, Symbol};

use crate::clap_host::Host;
use crate::plugin_instance::{HostCallbacks, LoadedPlugin, NoteTransition, TimedNoteEvent};

/// Cached per-param metadata needed on the audio thread to convert a
/// normalized modulation offset (`docs/plan_modulation_routing_redesign.md`
/// §3.2). Built once at load from `enumerate_params`.
#[derive(Clone, Copy)]
struct ClapParamMeta {
    min: f64,
    max: f64,
    /// `CLAP_PARAM_IS_MODULATABLE` — eligible for non-destructive `param_mod`.
    modulatable: bool,
}

/// Loaded CLAP plugin instance. Holds every resource alive until dropped.
/// Drop sequence:
///   1. `impl Drop` body — explicit `gui.destroy` → `plugin.destroy` →
///      `entry.deinit` (all DLL calls).
///   2. fields in declaration order (Rust reference: "fields of a struct
///      are dropped in the same order as they were declared"). `_library`
///      is declared LAST so `FreeLibrary` runs after every other field's
///      Drop — none of which call into the DLL today, but ordering it
///      this way keeps the host robust against future fields that might.
pub struct ClapPlugin {
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
    /// Pre-allocated input note event buffer; filled by process() per call.
    pending_events: Vec<clap_event_note>,
    /// Phase 2b (`docs/plan_automation.md` §8.3): pre-allocated input
    /// parameter event buffer; filled by process() per call from the
    /// caller's `param_events` slice. Merged with `pending_events` into
    /// the plugin's `in_events` via `EventListView`.
    pending_param_events: Vec<clap_event_param_value>,
    /// `docs/plan_modulation_routing_redesign.md` §3.2: pre-allocated input
    /// `CLAP_EVENT_PARAM_MOD` buffer. Filled by process() for `ParamEventKind::
    /// Mod` events on **modulatable** params — a non-destructive offset the
    /// plugin adds to its own (automation-driven) base value (Bitwig二層).
    pending_param_mods: Vec<clap_event_param_mod>,
    /// `docs/plan_modulation_routing_redesign.md` §3.2 / §4: per-param
    /// `(min, max, modulatable)` cached at load so the audio-thread process()
    /// can convert a normalized modulation offset to a CLAP plain `amount`
    /// (`offset·(max−min)`) and pick the param_mod-vs-fold path without
    /// calling the main-thread-only `enumerate`.
    param_meta: std::collections::HashMap<u32, ClapParamMeta>,
    /// Last absolute value the host sent per param (base for the
    /// non-modulatable fold path). Pre-populated with every param's default at
    /// load so process() only ever updates existing keys (no RT heap alloc).
    last_param_value: std::collections::HashMap<u32, f64>,
    /// Pre-allocated merge order over the note / param_value / param_mod
    /// streams, sorted by `header.time`. CLAP requires `in_events` to be
    /// time-ascending (`clap/events.h`); each stream arrives pre-sorted, so we
    /// stable-merge them here each `process()` (in-place sort of a pre-allocated
    /// buffer — no RT heap alloc).
    event_order: Vec<EventOrderRef>,
    /// Notes emitted by the plugin during the previous `process()` call.
    /// Populated by the `out_events.try_push` callback and drained by the
    /// caller (e.g. MIDI FX chain) before the next process().
    collected_out_notes: Vec<TimedNoteEvent>,
    /// Phase 2c (`docs/plan_automation.md` §7.5 / CLAP gesture spec):
    /// plugin GUI で knob を touch したとき発火する
    /// `CLAP_EVENT_PARAM_GESTURE_BEGIN` の param_id 列。 host は
    /// drain して `PluginParamTouched` IPC で daw_gui に通知し、
    /// `last_touched_param` を plugin param で更新させる。
    collected_out_param_touches: Vec<u32>,
    /// Phase 2c: plugin GUI で knob 値を変更したとき発火する
    /// `CLAP_EVENT_PARAM_VALUE` の (param_id, value)。 Phase 4 recording
    /// mode で point 生成 source として使う、 Phase 2 では daw_gui の
    /// last value cache に積むのみ。
    collected_out_param_values: Vec<(u32, f64)>,
    /// Phase 4 Step C-3 (`docs/plan_automation.md` §6): plugin GUI で knob
    /// release した瞬間の `CLAP_EVENT_PARAM_GESTURE_END` param_id 列。
    /// host は drain して `PluginParamGestureEnd` IPC で daw_gui に通知し、
    /// `active_param_gestures` から該当 PluginParam target を remove する。
    collected_out_param_releases: Vec<u32>,
    /// `clap_plugin_gui` vtable pointer, looked up once after init.
    /// `None` means the plugin does not declare the gui extension.
    gui_ext: Option<*const clap_plugin_gui>,
    /// Whether `gui.create` has been called successfully and `gui.destroy`
    /// has not yet been called. Used by Drop to tear down cleanly.
    gui_created: bool,
    /// `clap_plugin_state` vtable pointer. `None` when the plugin does not
    /// implement the state extension (project save/load will skip it).
    state_ext: Option<*const clap_plugin_state>,
    /// `clap_plugin_latency` vtable pointer (PR3.3). `None` when the
    /// plugin doesn't implement the latency extension.
    latency_ext: Option<*const clap_plugin_latency>,
    /// Phase 2: `clap_plugin_params` vtable pointer. `None` when the
    /// plugin doesn't implement the params extension. Looked up once
    /// during init, used by `enumerate_params()`.
    params_ext: Option<*const clap_plugin_params>,
    /// PR4 sidechain: per-aux-input-port channel counts in the plugin's
    /// declared port order. Length capped at `MAX_AUX_IN`. Empty when
    /// the plugin has no `is_main=false` input ports.
    aux_input_channels: Vec<u32>,
    /// Pre-allocated planar buffers for each aux input port. Outer:
    /// aux port idx. Middle: channel idx within port. Inner: per-frame
    /// f32 (capped at `max_frames`).
    /// Phase 5 follow-up review: process() の hot path で毎 buffer に
    /// allocate していた `Vec<clap_audio_buffer>` を pre-allocated field
    /// に re-use 化。 capacity は activate 時に `1 (main) + aux 数` で確保、
    /// 毎 buffer 頭で `clear()` + `push()` で reuse する (= RT 安全)。
    process_input_bufs: Vec<clap_audio_buffer>,
    aux_input_buffers: Vec<Vec<Vec<f32>>>,
    /// Per-aux-port channel pointer scratch (filled each `process` call).
    aux_input_ptrs: Vec<Vec<*mut f32>>,
    /// DLL handle. Declared LAST so `FreeLibrary` runs after every other
    /// field's Drop. See struct doc-comment for the full sequence.
    _library: Library,
}

// The plugin holds raw pointers but ownership is exclusive within the struct.
unsafe impl Send for ClapPlugin {}

impl ClapPlugin {
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
        let aux_input_channels = query_aux_input_channels(plugin_ptr, get_ext);
        tracing::info!(
            input_channels,
            output_channels,
            aux_input_count = aux_input_channels.len(),
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

        // PR3.3: Look up optional clap.latency extension for PDC.
        let latency_ptr = unsafe { get_ext(plugin_ptr, CLAP_EXT_LATENCY.as_ptr()) }
            as *const clap_plugin_latency;
        let latency_ext = if latency_ptr.is_null() {
            None
        } else {
            Some(latency_ptr)
        };
        tracing::info!(has_latency = latency_ext.is_some(), "plugin latency extension");

        // Phase 2: Look up optional clap.params extension. Plugins without
        // it (= no automatable parameters) return an empty param list.
        let params_ptr = unsafe { get_ext(plugin_ptr, CLAP_EXT_PARAMS.as_ptr()) }
            as *const clap_plugin_params;
        let params_ext = if params_ptr.is_null() {
            None
        } else {
            Some(params_ptr)
        };
        tracing::info!(has_params = params_ext.is_some(), "plugin params extension");

        let mut plugin = Self {
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
            pending_param_events: Vec::with_capacity(256),
            pending_param_mods: Vec::with_capacity(256),
            param_meta: std::collections::HashMap::new(),
            last_param_value: std::collections::HashMap::new(),
            event_order: Vec::with_capacity(512),
            collected_out_param_touches: Vec::with_capacity(64),
            collected_out_param_values: Vec::with_capacity(256),
            collected_out_param_releases: Vec::with_capacity(64),
            collected_out_notes: Vec::with_capacity(256),
            gui_ext,
            gui_created: false,
            state_ext,
            latency_ext,
            aux_input_channels,
            process_input_bufs: Vec::new(),
            aux_input_buffers: Vec::new(),
            aux_input_ptrs: Vec::new(),
            params_ext,
        };
        // `docs/plan_modulation_routing_redesign.md` §4: cache param min/max/
        // modulatable + seed the base-value cache so the audio thread never
        // needs the main-thread-only `enumerate` and never allocates.
        plugin.init_param_meta();
        Ok(Some(plugin))
    }

    /// Populate `param_meta` + `last_param_value` from the plugin's param list.
    /// Called once at load (main thread). After this the audio-thread
    /// `process()` only reads / updates existing keys — no allocation.
    fn init_param_meta(&mut self) {
        let infos = self.enumerate_params();
        self.param_meta.reserve(infos.len());
        self.last_param_value.reserve(infos.len());
        for info in &infos {
            let modulatable =
                info.flags & common::protocol::plugin_param_flags::MODULATABLE != 0;
            self.param_meta.insert(
                info.id,
                ClapParamMeta {
                    min: info.min_value,
                    max: info.max_value,
                    modulatable,
                },
            );
            self.last_param_value.insert(info.id, info.default_value);
        }
    }

    /// Phase 2 (`docs/plan_automation.md` §7.5): enumerate every
    /// parameter the plugin exposes. Calls `clap_plugin_params.count`
    /// then `get_info` for each index, mapping CLAP flag bits to the
    /// host's `plugin_param_flags::*`. Names that aren't UTF-8 are
    /// passed through `from_utf8_lossy`.
    pub fn enumerate_params(&self) -> Vec<common::protocol::PluginParamInfo> {
        let Some(params) = self.params_ext else {
            return Vec::new();
        };
        let count_fn = unsafe { (*params).count };
        let get_info_fn = unsafe { (*params).get_info };
        let (Some(count_fn), Some(get_info_fn)) = (count_fn, get_info_fn) else {
            return Vec::new();
        };
        // plugin が返す count は untrusted。 異常値で巨大確保しないよう
        // 上限を入れる (= 64Ki param で十分すぎる)。
        const MAX_PARAMS: u32 = 65536;
        let count = unsafe { count_fn(self.plugin) }.min(MAX_PARAMS);
        let mut out: Vec<common::protocol::PluginParamInfo> = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: clap_param_info is plain old data; zeroed start is
            // legal because get_info fully overwrites the fields it
            // populates (CLAP plugins must.)
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            let ok = unsafe { get_info_fn(self.plugin, i, &raw mut info) };
            if !ok {
                tracing::warn!(index = i, "clap_plugin_params.get_info returned false");
                continue;
            }
            let name = c_str_to_string(info.name.as_ptr());
            let module = c_str_to_string(info.module.as_ptr());
            let mut flags: u32 = 0;
            if info.flags & CLAP_PARAM_IS_STEPPED != 0 {
                flags |= common::protocol::plugin_param_flags::STEPPED;
            }
            if info.flags & CLAP_PARAM_IS_PERIODIC != 0 {
                flags |= common::protocol::plugin_param_flags::PERIODIC;
            }
            if info.flags & CLAP_PARAM_IS_READONLY != 0 {
                flags |= common::protocol::plugin_param_flags::READONLY;
            }
            if info.flags & CLAP_PARAM_IS_HIDDEN != 0 {
                flags |= common::protocol::plugin_param_flags::HIDDEN;
            }
            if info.flags & CLAP_PARAM_IS_AUTOMATABLE != 0 {
                flags |= common::protocol::plugin_param_flags::AUTOMATABLE;
            }
            if info.flags & CLAP_PARAM_IS_MODULATABLE != 0 {
                flags |= common::protocol::plugin_param_flags::MODULATABLE;
            }
            if info.flags & CLAP_PARAM_REQUIRES_PROCESS != 0 {
                flags |= common::protocol::plugin_param_flags::REQUIRES_PROCESS;
            }
            out.push(common::protocol::PluginParamInfo {
                id: info.id,
                name,
                module,
                min_value: info.min_value,
                max_value: info.max_value,
                default_value: info.default_value,
                flags,
            });
        }
        out
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

    /// PR3.3: query CLAP plugin's reported latency. Spec: must be called on
    /// main-thread while plugin is active. Returns 0 if the plugin doesn't
    /// implement `clap.latency`.
    pub fn query_latency_samples(&self) -> u32 {
        let Some(ext_ptr) = self.latency_ext else {
            return 0;
        };
        let Some(ext) = (unsafe { ext_ptr.as_ref() }) else {
            return 0;
        };
        let Some(get) = ext.get else {
            return 0;
        };
        unsafe { get(self.plugin) }
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
        // PR4 sidechain: allocate planar buffers for each aux input port,
        // one Vec<f32> per channel, sized to max_frames. The
        // `data32` pointer scratch (`aux_input_ptrs`) is rebuilt every
        // process() call.
        self.aux_input_buffers = self
            .aux_input_channels
            .iter()
            .map(|&ch_count| {
                (0..ch_count as usize)
                    .map(|_| vec![0.0f32; max_frames as usize])
                    .collect()
            })
            .collect();
        self.aux_input_ptrs = self
            .aux_input_channels
            .iter()
            .map(|&ch_count| vec![std::ptr::null_mut(); ch_count as usize])
            .collect();
        // Phase 5 follow-up review: process() で毎 buffer 確保していた
        // `Vec<clap_audio_buffer>` を pre-allocate して reuse 化。 capacity =
        // 1 (main) + aux port 数。
        self.process_input_bufs =
            Vec::with_capacity(1 + self.aux_input_channels.len());
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

    /// Phase 2c: drain plugin-emitted parameter gestures (PARAM_GESTURE_BEGIN)
    /// into `out`. Like `drain_out_notes_into`, preserves capacity.
    pub fn drain_out_param_touches_into(&mut self, out: &mut Vec<u32>) {
        out.append(&mut self.collected_out_param_touches);
    }

    /// Phase 2c: drain plugin-emitted parameter value changes
    /// (PARAM_VALUE) into `out`.
    pub fn drain_out_param_values_into(&mut self, out: &mut Vec<(u32, f64)>) {
        out.append(&mut self.collected_out_param_values);
    }

    /// Phase 4 Step C-3: drain plugin-emitted gesture releases
    /// (PARAM_GESTURE_END) into `out`。 daw_gui で
    /// `active_param_gestures` から該当 PluginParam target を remove する。
    pub fn drain_out_param_releases_into(&mut self, out: &mut Vec<u32>) {
        out.append(&mut self.collected_out_param_releases);
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
        param_events: &[crate::plugin_instance::TimedParamEvent],
        input_audio: &[&[f32]],
        aux_inputs: &[crate::plugin_instance::AuxInputBuf<'_>],
        transport: &crate::plugin_instance::TransportContext,
    ) -> Result<i32> {
        anyhow::ensure!(self.processing, "plugin not processing");

        self.pending_events.clear();
        for ev in events {
            let mut e = encode_note(ev.event);
            e.header.time = ev.time;
            self.pending_events.push(e);
        }
        // Phase 2b: param events → clap_event_param_value (CLAP_EVENT_PARAM_VALUE)。
        // `cookie: null_mut` で OK (CLAP spec: 「Some plugin's may not use
        // cookies and instead require the id, this is the case for plugins
        // that share parameters between instances」)、 host が cookie cache
        // を持たないので nullを渡して param_id 直引き。
        //
        // `docs/plan_modulation_routing_redesign.md` §3.2: `ParamEventKind::Mod`
        // events carry a *normalized* modulation offset. On **modulatable**
        // params we emit a non-destructive `CLAP_EVENT_PARAM_MOD` (amount =
        // offset·(max−min)); the plugin adds it to its own automation-driven
        // base (Bitwig 二層). Non-modulatable params have no mod channel, so we
        // fold the offset into an absolute `param_value` using the cached base.
        use crate::plugin_instance::ParamEventKind;
        // Pre-pass: refresh the base-value cache from this buffer's absolute
        // Value events so any non-modulatable fold below sees the current base
        // regardless of the (unstable) time sort order.
        for ev in param_events {
            if ev.kind == ParamEventKind::Value
                && let Some(slot) = self.last_param_value.get_mut(&ev.param_id)
            {
                *slot = ev.value;
            }
        }
        self.pending_param_events.clear();
        self.pending_param_mods.clear();
        let param_value_header = |time: u32| clap_event_header {
            size: std::mem::size_of::<clap_event_param_value>() as u32,
            time,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        };
        for ev in param_events {
            match ev.kind {
                ParamEventKind::Value => {
                    self.pending_param_events.push(clap_event_param_value {
                        header: param_value_header(ev.time),
                        param_id: ev.param_id,
                        cookie: std::ptr::null_mut(),
                        note_id: -1,
                        port_index: -1,
                        channel: -1,
                        key: -1,
                        value: ev.value,
                    });
                }
                ParamEventKind::Mod => {
                    let Some(meta) = self.param_meta.get(&ev.param_id).copied() else {
                        continue; // unknown param id — nothing to modulate
                    };
                    let amount = ev.value * (meta.max - meta.min);
                    if meta.modulatable {
                        self.pending_param_mods.push(clap_event_param_mod {
                            header: clap_event_header {
                                size: std::mem::size_of::<clap_event_param_mod>() as u32,
                                time: ev.time,
                                space_id: CLAP_CORE_EVENT_SPACE_ID,
                                type_: CLAP_EVENT_PARAM_MOD,
                                flags: 0,
                            },
                            param_id: ev.param_id,
                            cookie: std::ptr::null_mut(),
                            note_id: -1,
                            port_index: -1,
                            channel: -1,
                            key: -1,
                            amount,
                        });
                    } else {
                        // No CLAP modulation channel for this param: fold the
                        // offset into the absolute value (host-computed, like
                        // VST3). Base = cached last value (seeded with default).
                        let base = self
                            .last_param_value
                            .get(&ev.param_id)
                            .copied()
                            .unwrap_or(meta.min);
                        let value = (base + amount).clamp(meta.min, meta.max);
                        self.pending_param_events.push(clap_event_param_value {
                            header: param_value_header(ev.time),
                            param_id: ev.param_id,
                            cookie: std::ptr::null_mut(),
                            note_id: -1,
                            port_index: -1,
                            channel: -1,
                            key: -1,
                            value,
                        });
                    }
                }
            }
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
        // PR4 sidechain: copy aux inputs into our pre-allocated planar
        // aux_input_buffers and rebuild aux_input_ptrs scratch. Inactive
        // ports get silence so the plugin sees a consistent buffer
        // regardless of routing state.
        for (port_idx, port_bufs) in self.aux_input_buffers.iter_mut().enumerate() {
            let aux = aux_inputs.get(port_idx).copied();
            for (ch, buf) in port_bufs.iter_mut().enumerate() {
                let cap = n.min(buf.len());
                let src: &[f32] = match (aux, ch) {
                    (Some(a), 0) if a.active => a.l,
                    (Some(a), 1) if a.active => a.r,
                    // Plugin asked for >2 channels — feed silence; not
                    // supported by our stereo-only sidechain pipeline.
                    _ => &[],
                };
                let copy_n = cap.min(src.len());
                buf[..copy_n].copy_from_slice(&src[..copy_n]);
                if copy_n < cap {
                    buf[copy_n..cap].fill(0.0);
                }
            }
            for (ch, ptrs) in self.aux_input_ptrs[port_idx].iter_mut().enumerate() {
                *ptrs = port_bufs[ch].as_mut_ptr();
            }
        }
        // Refresh output channel pointers (buffers are pre-allocated).
        for i in 0..self.output_buffers.len() {
            self.output_ptrs[i] = self.output_buffers[i].as_mut_ptr();
        }

        // PR4 sidechain: build the full clap_audio_buffer array — main
        // input first (matching CLAP convention that port 0 is main),
        // then each aux port. Length = 1 (main) + aux count when main is
        // present, else 0 (instrument with no audio input + no aux).
        // Phase 5 follow-up review: 旧 `Vec::with_capacity` を pre-allocated
        // `self.process_input_bufs` に置換。 RT 安全 (= heap alloc なし、
        // `clear()` + `push()` で容量再利用)。
        self.process_input_bufs.clear();
        if self.input_channels > 0 {
            self.process_input_bufs.push(clap_audio_buffer {
                data32: self.input_ptrs.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: self.input_channels,
                latency: 0,
                constant_mask: 0,
            });
        }
        for port_idx in 0..self.aux_input_channels.len() {
            self.process_input_bufs.push(clap_audio_buffer {
                data32: self.aux_input_ptrs[port_idx].as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: self.aux_input_channels[port_idx],
                latency: 0,
                constant_mask: 0,
            });
        }
        let mut audio_out = clap_audio_buffer {
            data32: self.output_ptrs.as_mut_ptr(),
            data64: std::ptr::null_mut(),
            channel_count: self.output_channels,
            latency: 0,
            constant_mask: 0,
        };

        // Build the time-ascending merge order over note + param events.
        // CLAP requires `in_events` sorted by `header.time`; notes and params
        // are each sorted within their own stream but interleave by time, so
        // we stable-merge them here. `sort_by_key` is stable, so notes keep
        // their relative order ahead of params at the same `time`. In-place
        // sort of a pre-allocated buffer ⇒ no heap alloc on the RT path.
        self.event_order.clear();
        for (idx, e) in self.pending_events.iter().enumerate() {
            self.event_order.push(EventOrderRef {
                time: e.header.time,
                stream: EventStream::Note,
                idx: idx as u32,
            });
        }
        for (idx, e) in self.pending_param_events.iter().enumerate() {
            self.event_order.push(EventOrderRef {
                time: e.header.time,
                stream: EventStream::Param,
                idx: idx as u32,
            });
        }
        for (idx, e) in self.pending_param_mods.iter().enumerate() {
            self.event_order.push(EventOrderRef {
                time: e.header.time,
                stream: EventStream::ParamMod,
                idx: idx as u32,
            });
        }
        self.event_order.sort_by_key(|r| r.time);

        // Phase 2b: note + param (+ param_mod) events を 1 view にまとめて
        // vtable に渡す。 EventListView は process() の lifetime 内だけ存続する
        // local 変数で、 plugin.process() が return するまで生きている
        // ので raw pointer cast は安全。
        let event_view = EventListView {
            notes: &self.pending_events,
            params: &self.pending_param_events,
            param_mods: &self.pending_param_mods,
            order: &self.event_order,
        };
        let in_events = clap_input_events {
            ctx: std::ptr::from_ref(&event_view) as *mut c_void,
            size: Some(in_events_size),
            get: Some(in_events_get),
        };
        // Phase 2c: collector points at ClapPlugin の 3 vec を per-process
        // でリセット (clear) してから raw pointer を OutEventCollector に
        // 渡す。 plugin.process() 中だけ存続するローカル変数。
        self.collected_out_param_touches.clear();
        self.collected_out_param_values.clear();
        self.collected_out_param_releases.clear();
        let mut out_collector = OutEventCollector {
            notes: std::ptr::from_mut(&mut self.collected_out_notes),
            param_touches: std::ptr::from_mut(&mut self.collected_out_param_touches),
            param_values: std::ptr::from_mut(&mut self.collected_out_param_values),
            param_releases: std::ptr::from_mut(&mut self.collected_out_param_releases),
        };
        let out_events = clap_output_events {
            ctx: std::ptr::from_mut(&mut out_collector) as *mut c_void,
            try_push: Some(collect_out_note_try_push),
        };

        let (audio_inputs, audio_inputs_count) = if self.process_input_bufs.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (
                self.process_input_bufs.as_ptr(),
                self.process_input_bufs.len() as u32,
            )
        };
        let (audio_outputs, audio_outputs_count) = if self.output_channels == 0 {
            (std::ptr::null_mut(), 0)
        } else {
            (&raw mut audio_out, 1)
        };

        // Phase 5 Step 5.3 (`docs/plan_automation.md` §10): build
        // `clap_event_transport` from the per-buffer `TransportContext` so
        // tempo-sync plugins (Delay sync to beat, Arp etc.) see the host
        // BPM / time-signature / playhead. `song_pos_*` are fixed-point
        // (1 << 31 per beat / sec). `bar_*` are populated from the
        // current tsig_num (= bar starts at `bar_number * tsig_num`
        // beats since song start)。 loop fields are zeroed when not in
        // loop mode to avoid plugins acting on stale ranges.
        let transport_event = build_clap_transport_event(transport, frames);
        let process_ctx = clap_process {
            steady_time: -1,
            frames_count: frames,
            transport: &transport_event,
            audio_inputs,
            audio_outputs,
            audio_inputs_count,
            audio_outputs_count,
            in_events: &in_events,
            out_events: &out_events,
        };

        let process = unsafe { (*self.plugin).process }.context("plugin.process is null")?;
        let status = unsafe { process(self.plugin, &process_ctx) };
        // self.process_input_bufs lives across calls (pre-allocated on
        // activate). data32 pointers it stored remain valid through the
        // FFI call above because input_buffers / aux_input_buffers (=
        // backing storage) are themselves stable in self.
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
    pub fn state_load(&mut self, data: &[u8]) -> Result<()> {
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

/// Phase 2c: ctx for `out_events.try_push` — gathers note transitions
/// (existing MIDI FX path) **and** plugin-initiated parameter gestures
/// (PARAM_GESTURE_BEGIN) + parameter value changes (PARAM_VALUE) into
/// 3 separate vectors. Built on the stack at the top of every
/// `process()` call; the raw pointers point at fields on the
/// `ClapPlugin` instance and are valid for the duration of the FFI
/// call.
struct OutEventCollector {
    notes: *mut Vec<TimedNoteEvent>,
    param_touches: *mut Vec<u32>,
    param_values: *mut Vec<(u32, f64)>,
    /// Phase 4 Step C-3: PARAM_GESTURE_END collector。 plugin GUI で knob を
    /// release した瞬間に push される。
    param_releases: *mut Vec<u32>,
}

/// `try_push` callback shared by all CLAP plugins. `ctx` points at an
/// `OutEventCollector`. Note transitions and param events get routed
/// to the appropriate collector vector; unknown types are accepted
/// (return `true`) but not recorded.
unsafe extern "C" fn collect_out_note_try_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    if list.is_null() || event.is_null() {
        return false;
    }
    let ctx = unsafe { (*list).ctx } as *mut OutEventCollector;
    if ctx.is_null() {
        return true;
    }
    let collector = unsafe { &*ctx };
    let header = unsafe { &*event };
    if header.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return true;
    }
    match header.type_ {
        t if t == CLAP_EVENT_NOTE_ON => {
            let note = unsafe { &*(event as *const clap_event_note) };
            let note_id = note.note_id.max(0) as u32;
            let transition = NoteTransition::On {
                note_id,
                key: note.key.clamp(0, 127) as u8,
                velocity: note.velocity,
            };
            if !collector.notes.is_null() {
                let out = unsafe { &mut *collector.notes };
                out.push(TimedNoteEvent { time: header.time, event: transition });
            }
        }
        t if t == CLAP_EVENT_NOTE_OFF => {
            let note = unsafe { &*(event as *const clap_event_note) };
            let note_id = note.note_id.max(0) as u32;
            let transition = NoteTransition::Off {
                note_id,
                key: note.key.clamp(0, 127) as u8,
            };
            if !collector.notes.is_null() {
                let out = unsafe { &mut *collector.notes };
                out.push(TimedNoteEvent { time: header.time, event: transition });
            }
        }
        t if t == CLAP_EVENT_PARAM_GESTURE_BEGIN => {
            // Phase 2c: plugin GUI 内で knob を touch した瞬間。 host は
            // 同 param に最も近い PluginParamInfo を引いて
            // `PluginParamTouched` を daw_gui に送り、 `last_touched_param`
            // を更新させる。 `clap_event_param_gesture` は param_id を
            // 持つ (header 直後の u32)。 clap_sys に struct がない場合は
            // raw offset で読む。
            //
            // Phase 6 review (FFI 安全): plugin が malformed event を
            // try_push してきたケースに備えて `header.size` を必ず検証
            // (= header + u32 以上)。 検証せず deref すると 1-byte OOB
            // read。 spec: clap/events.h, header.size はイベント本体総 byte。
            const REQUIRED: u32 =
                (std::mem::size_of::<clap_event_header>() + std::mem::size_of::<u32>()) as u32;
            if header.size < REQUIRED {
                // malformed plugin event。 RT パスなので I/O (tracing) は
                // 増やさず黙って破棄する。 「accepted but not recorded」 path
                // と同じく true で抜ける (= plugin に try_push の retry を
                // 促さない、 不正 event は破棄)。
                return true;
            }
            let param_id = unsafe {
                let body = event as *const u8;
                let id_ptr = body.add(std::mem::size_of::<clap_event_header>())
                    as *const u32;
                *id_ptr
            };
            if !collector.param_touches.is_null() {
                let out = unsafe { &mut *collector.param_touches };
                // RT 安全: 事前確保した capacity を超える push は realloc を
                // 起こす。 溢れた gesture (knob flood) は drop して RT 制約を
                // 守る (process_server の append も再確保しない invariant)。
                if out.len() < out.capacity() {
                    out.push(param_id);
                }
            }
        }
        t if t == CLAP_EVENT_PARAM_GESTURE_END => {
            // Phase 4 Step C-3: plugin GUI 内で knob を release した瞬間。
            // host は drain して `PluginParamGestureEnd` IPC で daw_gui に
            // 通知し、 `active_param_gestures` から該当 PluginParam target
            // を remove する (= Touch mode で curve 復帰、 Latch / Write で
            // 引き続き latched は維持)。
            //
            // Phase 6 review (FFI 安全): GESTURE_BEGIN と同様、 header.size
            // を検証してから raw offset を deref する (= OOB 防御)。
            const REQUIRED: u32 =
                (std::mem::size_of::<clap_event_header>() + std::mem::size_of::<u32>()) as u32;
            if header.size < REQUIRED {
                // malformed plugin event。 GESTURE_BEGIN と同様、 RT パス
                // なので tracing を増やさず黙って破棄する。
                return true;
            }
            let param_id = unsafe {
                let body = event as *const u8;
                let id_ptr = body.add(std::mem::size_of::<clap_event_header>())
                    as *const u32;
                *id_ptr
            };
            if !collector.param_releases.is_null() {
                let out = unsafe { &mut *collector.param_releases };
                if out.len() < out.capacity() {
                    out.push(param_id);
                }
            }
        }
        t if t == CLAP_EVENT_PARAM_VALUE => {
            let pv = unsafe { &*(event as *const clap_event_param_value) };
            if !collector.param_values.is_null() {
                let out = unsafe { &mut *collector.param_values };
                if out.len() < out.capacity() {
                    out.push((pv.param_id, pv.value));
                }
            }
        }
        _ => {}
    }
    true
}

/// Phase 5 Step 5.3 (`docs/plan_automation.md` §10): build a
/// `clap_event_transport` from the host-side `TransportContext` at the
/// start of every `process()` call. Plugins read this via
/// `clap_process.transport` and use it for tempo-sync (Delay sync to
/// beat, Arp tempo, etc.). `frames` is unused for the per-buffer
/// transport (= we set song_pos_* to the buffer-start values); CLAP
/// plugins increment internally for sub-buffer interpolation.
///
/// `song_pos_beats` = playhead_samples * bpm / (60 * SR) * (1 << 31)
/// `song_pos_seconds` = playhead_samples / SR * (1 << 31)
/// `bar_start` / `bar_number`: compute current bar index from beats
/// using tsig_num (= beats_per_bar). Bar start in beats is
/// `bar_number * tsig_num` (= integer beats since song start).
///
/// Transport の song position (beats / seconds) を `as i64` する前に
/// 非有限 (NaN / inf) を 0 に倒す。 これらは `* FACTOR` 後に整数化される
/// ので、 NaN / inf が紛れると CLAP plugin に未規定の timeline 値を渡して
/// しまう。 RT 安全 (分岐のみ、 確保なし)。
#[inline]
fn sanitize_pos(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

fn build_clap_transport_event(
    transport: &crate::plugin_instance::TransportContext,
    _frames: u32,
) -> clap_event_transport {
    let bpm = f64::from(transport.bpm).max(1.0);
    let sample_rate = f64::from(transport.sample_rate).max(1.0);
    let playhead_samples = transport.playhead_samples as f64;
    // seconds は sample 由来 (= テンポ非依存で正確)。
    // 非有限 (NaN / inf) を `as i64` すると未規定 / saturate になり下流の
    // bar 計算も汚染するので、 bar_number の clamp と同様 0 に倒す。
    let song_pos_seconds_f64 = sanitize_pos(playhead_samples / sample_rate);
    // beats は daw_audio が tempo automation を積分した真の拍位置を使う
    // (= `samples × bpm` の一定テンポ逆算は廃止)。 これで途中でテンポが
    // 変わった曲でも plugin が正しい拍 / 小節位置を見る。
    let song_pos_beats_f64 = sanitize_pos(transport.song_pos_beats);
    let song_pos_beats: i64 = (song_pos_beats_f64 * CLAP_BEATTIME_FACTOR as f64) as i64;
    let song_pos_seconds: i64 = (song_pos_seconds_f64 * CLAP_SECTIME_FACTOR as f64) as i64;
    let tsig_num = transport.tsig_num.max(1) as f64;
    let bar_number = (song_pos_beats_f64 / tsig_num).floor();
    let bar_start_beats_f64 = bar_number * tsig_num;
    let bar_start: i64 = (bar_start_beats_f64 * CLAP_BEATTIME_FACTOR as f64) as i64;
    let mut flags = CLAP_TRANSPORT_HAS_TEMPO
        | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
        | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
        | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
    if transport.is_playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    if transport.is_looping && transport.loop_end_beats > transport.loop_start_beats {
        flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
    }
    let loop_start_beats: i64 =
        (transport.loop_start_beats * CLAP_BEATTIME_FACTOR as f64) as i64;
    let loop_end_beats: i64 =
        (transport.loop_end_beats * CLAP_BEATTIME_FACTOR as f64) as i64;
    clap_event_transport {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_transport>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        },
        flags,
        song_pos_beats,
        song_pos_seconds,
        tempo: bpm,
        tempo_inc: 0.0,
        loop_start_beats,
        loop_end_beats,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start,
        // bar_number i32: 長時間再生で overflow リスク (= 2 billion 拍以上で
        // wrap-around)。 通常使用では到達不能だが defensive で clamp。
        #[allow(clippy::cast_possible_truncation)]
        bar_number: bar_number.clamp(f64::from(i32::MIN), f64::from(i32::MAX))
            as i32,
        tsig_num: transport.tsig_num,
        tsig_denom: transport.tsig_denom,
    }
}

/// Phase 2b: 1 buffer 分の note + param event を 1 つの list として
/// plugin に渡すための view。 `process()` のローカル変数として作られ、
/// Which pre-allocated stream an [`EventOrderRef`] indexes into.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EventStream {
    Note,
    Param,
    ParamMod,
}

/// `clap_input_events.ctx` が `*const Self` を指す。 plugin.process()
/// 呼び出しの間だけ存続する短命オブジェクト。
/// One entry in [`EventListView::order`]: references a note / param_value /
/// param_mod event by index, tagged with its `header.time` so the merged
/// stream can be presented to the plugin in time-ascending order (a CLAP
/// `clap_input_events` contract — `clap/events.h`).
#[derive(Clone, Copy)]
struct EventOrderRef {
    time: u32,
    stream: EventStream,
    idx: u32,
}

struct EventListView<'a> {
    notes: &'a [clap_event_note],
    params: &'a [clap_event_param_value],
    param_mods: &'a [clap_event_param_mod],
    /// Time-sorted merge order over `notes` + `params` + `param_mods`. Built
    /// per `process()`.
    order: &'a [EventOrderRef],
}

/// `ctx` of the in_events vtable points to `&EventListView`.
unsafe extern "C" fn in_events_size(list: *const clap_input_events) -> u32 {
    let ctx = unsafe { (*list).ctx } as *const EventListView<'_>;
    if ctx.is_null() {
        return 0;
    }
    let view = unsafe { &*ctx };
    view.order.len() as u32
}

unsafe extern "C" fn in_events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    let ctx = unsafe { (*list).ctx } as *const EventListView<'_>;
    if ctx.is_null() {
        return std::ptr::null();
    }
    let view = unsafe { &*ctx };
    let Some(r) = view.order.get(index as usize) else {
        return std::ptr::null();
    };
    match r.stream {
        EventStream::Note => std::ptr::from_ref(&view.notes[r.idx as usize].header),
        EventStream::Param => std::ptr::from_ref(&view.params[r.idx as usize].header),
        EventStream::ParamMod => std::ptr::from_ref(&view.param_mods[r.idx as usize].header),
    }
}

fn encode_note(transition: NoteTransition) -> clap_event_note {
    // PR-V2.4: `note_id` を取り出して CLAP `clap_event_note.note_id` に詰める。
    // i32 で `-1` (= "未指定") を sentinel に使う仕様なので、 host 側 0 は
    // そのまま 0 として送る (= 0 は valid な note_id)。
    let (type_, key, velocity, note_id) = match transition {
        NoteTransition::On { note_id, key, velocity } => {
            let nid = if note_id <= i32::MAX as u32 { note_id as i32 } else { -1 };
            (CLAP_EVENT_NOTE_ON, key, velocity, nid)
        }
        NoteTransition::Off { note_id, key } => {
            let nid = if note_id <= i32::MAX as u32 { note_id as i32 } else { -1 };
            (CLAP_EVENT_NOTE_OFF, key, 0.0, nid)
        }
    };
    clap_event_note {
        header: clap_event_header {
            size: std::mem::size_of::<clap_event_note>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_,
            flags: 0,
        },
        note_id,
        port_index: 0,
        channel: 0,
        key: key as i16,
        velocity,
    }
}

fn query_output_channel_count(plugin: *const clap_plugin, get_ext: GetExtFn) -> u32 {
    query_port_channel_count(plugin, get_ext, false)
}

/// PR4 sidechain: enumerate the plugin's `is_main=false` input ports and
/// return their channel counts in declaration order. Capped at
/// `common::process_data::MAX_AUX_IN` (extras logged + ignored). Returns
/// an empty Vec for plugins without aux inputs (the typical instrument /
/// non-sidechain effect case).
fn query_aux_input_channels(plugin: *const clap_plugin, get_ext: GetExtFn) -> Vec<u32> {
    let ext_ptr =
        unsafe { get_ext(plugin, CLAP_EXT_AUDIO_PORTS.as_ptr()) } as *const clap_plugin_audio_ports;
    if ext_ptr.is_null() {
        return Vec::new();
    }
    let ext = unsafe { &*ext_ptr };
    let Some(count_fn) = ext.count else {
        return Vec::new();
    };
    let Some(get) = ext.get else {
        return Vec::new();
    };
    let port_count = unsafe { count_fn(plugin, true) };
    let mut aux: Vec<u32> = Vec::new();
    for i in 0..port_count {
        let mut info = std::mem::MaybeUninit::<clap_audio_port_info>::zeroed();
        let ok = unsafe { get(plugin, i, true, info.as_mut_ptr()) };
        if !ok {
            continue;
        }
        let info = unsafe { info.assume_init() };
        if info.flags & CLAP_AUDIO_PORT_IS_MAIN == 0 {
            if aux.len() >= common::process_data::MAX_AUX_IN {
                tracing::warn!(
                    port_index = i,
                    cap = common::process_data::MAX_AUX_IN,
                    "plugin declared more aux input ports than the host caps to"
                );
                break;
            }
            aux.push(info.channel_count);
        }
    }
    aux
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

impl Drop for ClapPlugin {
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
    // NULL 終端が無い malformed descriptor で無限 walk しないよう上限を
    // 入れる (= common/src/plugin_db.rs の read_feature_list と同様)。
    const MAX_FEATURES: usize = 256;
    let mut p = ptr;
    unsafe {
        let mut hit_cap = true;
        for _ in 0..MAX_FEATURES {
            let s_ptr = *p;
            if s_ptr.is_null() {
                hit_cap = false;
                break;
            }
            out.push(CStr::from_ptr(s_ptr).to_string_lossy().into_owned());
            p = p.add(1);
        }
        if hit_cap {
            tracing::warn!(
                "feature list reached {MAX_FEATURES} entries without NULL terminator, truncating",
            );
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

/// FIXME #29: CLAP プラグインを一時 instantiate して note-ports / audio-ports
/// extension から port 構成を読む。 daw_plugin_host の `--probe-clap` one-shot
/// モードから呼ばれる。 CLAP descriptor の feature には note 出力の有無が無い
/// (`note-effect` 等の文字列があるだけ) ため、 dual-role (note 出力する instrument、
/// 例: Scaler 2 の CLAP 版) を正しく拾うには instance 生成後の port query が必須。
/// VST3 の [`crate::vst3_plugin::probe_ports`] と対称。 activate / process / GUI は
/// しない (= 軽量・副作用最小)。 失敗時は呼び元 (scan) が scan-time 暫定値を保持。
pub fn probe_ports(path: &Path, target_id: &str) -> Result<common::port_config::PortConfig> {
    let library = unsafe { Library::new(path) }
        .with_context(|| format!("loading CLAP at {}", path.display()))?;
    let entry_ptr: *const clap_plugin_entry = unsafe {
        let sym: Symbol<*const clap_plugin_entry> = library
            .get(b"clap_entry\0")
            .context("missing clap_entry symbol")?;
        *sym
    };
    anyhow::ensure!(!entry_ptr.is_null(), "clap_entry is null");
    let entry = unsafe { &*entry_ptr };
    anyhow::ensure!(
        clap_version_is_compatible(entry.clap_version),
        "incompatible CLAP version"
    );
    let c_path =
        CString::new(path.to_string_lossy().as_bytes()).context("path has interior nul")?;
    let init_fn = entry.init.context("clap_plugin_entry::init is null")?;
    anyhow::ensure!(
        unsafe { init_fn(c_path.as_ptr()) },
        "clap_entry.init returned false"
    );

    // entry.init 成功後はどの経路でも deinit + library drop して資源を返す。
    let probe = probe_ports_after_entry_init(entry, target_id);

    if let Some(deinit) = entry.deinit {
        unsafe { deinit() };
    }
    drop(library);
    probe
}

fn probe_ports_after_entry_init(
    entry: &clap_plugin_entry,
    target_id: &str,
) -> Result<common::port_config::PortConfig> {
    let get_factory = entry.get_factory.context("get_factory is null")?;
    let factory_ptr = unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) }
        as *const clap_plugin_factory;
    anyhow::ensure!(!factory_ptr.is_null(), "clap factory is null");
    let factory = unsafe { &*factory_ptr };
    let get_count = factory.get_plugin_count.context("get_plugin_count is null")?;
    let get_desc = factory
        .get_plugin_descriptor
        .context("get_plugin_descriptor is null")?;
    let create = factory.create_plugin.context("create_plugin is null")?;

    // target_id 一致の descriptor、 空なら最初。
    let count = unsafe { get_count(factory_ptr) };
    let mut selected: Option<u32> = None;
    for i in 0..count {
        let desc_ptr = unsafe { get_desc(factory_ptr, i) };
        if desc_ptr.is_null() {
            continue;
        }
        let desc = unsafe { &*desc_ptr };
        if target_id.is_empty() || c_str_to_string(desc.id) == target_id {
            selected = Some(i);
            break;
        }
    }
    let index = selected.context("no matching CLAP descriptor")?;
    let desc_ptr = unsafe { get_desc(factory_ptr, index) };
    anyhow::ensure!(!desc_ptr.is_null(), "selected descriptor became null");
    let plugin_id = unsafe { (*desc_ptr).id };

    let host = Host::new(HostCallbacks::noop());
    let host_ptr: *const clap_host = &host.clap;
    let plugin_ptr = unsafe { create(factory_ptr, host_ptr, plugin_id) };
    anyhow::ensure!(!plugin_ptr.is_null(), "create_plugin returned null");

    let cfg = clap_plugin_port_config(plugin_ptr);

    // create 済みなので init 成否に関わらず破棄する。
    if let Some(destroy) = unsafe { (*plugin_ptr).destroy } {
        unsafe { destroy(plugin_ptr) };
    }
    drop(host);
    cfg
}

fn clap_plugin_port_config(
    plugin_ptr: *const clap_plugin,
) -> Result<common::port_config::PortConfig> {
    let plugin_init = unsafe { (*plugin_ptr).init }.context("plugin.init is null")?;
    anyhow::ensure!(
        unsafe { plugin_init(plugin_ptr) },
        "plugin.init returned false"
    );
    let get_ext = unsafe { (*plugin_ptr).get_extension }.context("get_extension is null")?;

    let (note_in, note_out) = {
        let ext = unsafe { get_ext(plugin_ptr, CLAP_EXT_NOTE_PORTS.as_ptr()) }
            as *const clap_plugin_note_ports;
        match (ext.is_null(), unsafe { ext.as_ref() }.and_then(|e| e.count)) {
            (false, Some(count)) => {
                (unsafe { count(plugin_ptr, true) }, unsafe { count(plugin_ptr, false) })
            }
            _ => (0, 0),
        }
    };
    // audio port count: `count(plugin, is_input)` — false=output, true=input。
    let (audio_in, audio_out) = {
        let ext = unsafe { get_ext(plugin_ptr, CLAP_EXT_AUDIO_PORTS.as_ptr()) }
            as *const clap_plugin_audio_ports;
        match (ext.is_null(), unsafe { ext.as_ref() }.and_then(|e| e.count)) {
            (false, Some(count)) => (
                unsafe { count(plugin_ptr, true) },
                unsafe { count(plugin_ptr, false) },
            ),
            _ => (0, 0),
        }
    };
    Ok(common::port_config::PortConfig {
        has_note_input: note_in > 0,
        has_note_output: note_out > 0,
        has_audio_output: audio_out > 0,
        has_audio_input: audio_in > 0,
        // CLAP は映像 port を持たない (内蔵映像効果は GUI 側 device、probe しない)。
        has_video_input: false,
        has_video_output: false,
    })
}

impl LoadedPlugin for ClapPlugin {
    fn id(&self) -> &str {
        self.id()
    }

    fn name(&self) -> &str {
        self.name()
    }

    fn format(&self) -> PluginFormat {
        PluginFormat::Clap
    }

    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()> {
        self.activate(sample_rate, min_frames, max_frames)
    }

    fn deactivate(&mut self) {
        self.deactivate();
    }

    fn start_processing(&mut self) -> Result<()> {
        self.start_processing()
    }

    fn stop_processing(&mut self) {
        self.stop_processing();
    }

    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        param_events: &[crate::plugin_instance::TimedParamEvent],
        input_audio: &[&[f32]],
        aux_inputs: &[crate::plugin_instance::AuxInputBuf<'_>],
        transport: &crate::plugin_instance::TransportContext,
    ) -> Result<i32> {
        self.process(frames, events, param_events, input_audio, aux_inputs, transport)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        self.output_buffer(channel)
    }

    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>) {
        self.drain_out_notes_into(out);
    }

    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        if self.plugin.is_null() {
            return false;
        }
        let plugin = unsafe { &*self.plugin };
        let Some(get_ext) = plugin.get_extension else {
            return false;
        };
        let ext_ptr =
            unsafe { get_ext(self.plugin, CLAP_EXT_RENDER.as_ptr()) } as *const clap_plugin_render;
        if ext_ptr.is_null() {
            return false;
        }
        let render_mode = match mode {
            RenderMode::Realtime => CLAP_RENDER_REALTIME,
            RenderMode::Offline => CLAP_RENDER_OFFLINE,
        };
        let set_fn = unsafe { (*ext_ptr).set };
        let Some(set_fn) = set_fn else {
            return false;
        };
        unsafe { set_fn(self.plugin, render_mode) }
    }

    fn query_latency(&mut self) -> u32 {
        self.query_latency_samples()
    }

    fn enumerate_params(&self) -> Vec<common::protocol::PluginParamInfo> {
        ClapPlugin::enumerate_params(self)
    }

    fn drain_out_param_touches_into(&mut self, out: &mut Vec<u32>) {
        ClapPlugin::drain_out_param_touches_into(self, out);
    }

    fn drain_out_param_values_into(&mut self, out: &mut Vec<(u32, f64)>) {
        ClapPlugin::drain_out_param_values_into(self, out);
    }

    fn drain_out_param_releases_into(&mut self, out: &mut Vec<u32>) {
        ClapPlugin::drain_out_param_releases_into(self, out);
    }

    fn state_save(&self) -> Result<Option<Vec<u8>>> {
        self.state_save()
    }

    fn state_load(&mut self, data: &[u8]) -> Result<()> {
        self.state_load(data)
    }

    fn gui_is_embed_supported(&self) -> bool {
        self.gui_is_embed_supported()
    }

    fn gui_create_embedded(&mut self) -> Result<()> {
        self.gui_create_embedded()
    }

    fn gui_get_size(&self) -> Option<(u32, u32)> {
        self.gui_get_size()
    }

    fn gui_set_scale(&self, scale: f64) -> Result<bool> {
        self.gui_set_scale(scale)
    }

    fn gui_can_resize(&self) -> bool {
        self.gui_can_resize()
    }

    fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()> {
        self.gui_set_parent_hwnd(hwnd)
    }

    fn gui_show(&self) -> Result<bool> {
        self.gui_show()
    }

    fn gui_hide(&self) -> Result<()> {
        self.gui_hide()
    }

    fn gui_set_size(&self, width: u32, height: u32) -> Result<()> {
        self.gui_set_size(width, height)
    }

    fn gui_destroy(&mut self) {
        self.gui_destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_instance::TransportContext;

    fn ctx() -> TransportContext {
        TransportContext {
            bpm: 120.0,
            sample_rate: 48_000,
            // 1 秒 = 48000 samples。 一定テンポ逆算なら 120*1/60 = 2.0 拍。
            playhead_samples: 48_000,
            // だが真の拍位置は 999.0 (= tempo automation で曲頭からの累積が
            // sample×bpm の線形換算と一致しないケースを模擬)。
            song_pos_beats: 999.0,
            tsig_num: 4,
            tsig_denom: 4,
            is_playing: true,
            is_looping: false,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
        }
    }

    #[test]
    fn transport_event_uses_song_pos_beats_directly() {
        // SSoT 回帰防止: song_pos_beats は transport.song_pos_beats を直接
        // fixed-point 化したもので、 samples × bpm の一定テンポ逆算ではない。
        let ev = build_clap_transport_event(&ctx(), 256);
        let expected = (999.0_f64 * CLAP_BEATTIME_FACTOR as f64) as i64;
        assert_eq!(ev.song_pos_beats, expected);
        // seconds は sample 由来 (= テンポ非依存で正確)、 1 秒。
        let expected_sec = (1.0_f64 * CLAP_SECTIME_FACTOR as f64) as i64;
        assert_eq!(ev.song_pos_seconds, expected_sec);
        assert_eq!(ev.tempo, 120.0);
        assert_ne!(ev.flags & CLAP_TRANSPORT_IS_PLAYING, 0);
        // loop region 未定義 → IS_LOOP_ACTIVE は立たない。
        assert_eq!(ev.flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE, 0);
    }

    #[test]
    fn loop_active_requires_both_toggle_and_region() {
        let mut c = ctx();
        c.is_looping = true;
        c.loop_start_beats = 4.0;
        c.loop_end_beats = 8.0;
        let ev = build_clap_transport_event(&c, 256);
        assert_ne!(ev.flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE, 0);
        // toggle on でも region 無し (end <= start) なら flag は立たない。
        c.loop_end_beats = c.loop_start_beats;
        let ev2 = build_clap_transport_event(&c, 256);
        assert_eq!(ev2.flags & CLAP_TRANSPORT_IS_LOOP_ACTIVE, 0);
    }
}
