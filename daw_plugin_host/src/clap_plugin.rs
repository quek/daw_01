//! CLAP plugin wrapper.
//!
//! Split-half (`docs/plan_arch_refactor.md` §6): [`ClapPlugin`] is the
//! main-thread half (lifecycle / GUI / state / ARA / params), and
//! [`ClapAudioHalf`] is the audio-thread half (everything `process()`
//! touches). The two halves share only the raw `*const clap_plugin` — the
//! plugin's internal state behind it is partitioned by the CLAP spec's
//! main-thread vs audio-thread API split.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, clap_event_header,
    clap_event_note, clap_input_events, clap_output_events,
};
use clap_sys::events::{
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE, CLAP_EVENT_TRANSPORT, CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
    CLAP_TRANSPORT_HAS_SECONDS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO,
    CLAP_TRANSPORT_HAS_TIME_SIGNATURE, CLAP_TRANSPORT_IS_LOOP_ACTIVE, CLAP_TRANSPORT_IS_PLAYING,
    clap_event_param_mod, clap_event_param_value, clap_event_transport,
};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::gui::{
    CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_gui_resize_hints, clap_plugin_gui, clap_window,
    clap_window_handle,
};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_plugin_note_ports};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_HIDDEN, CLAP_PARAM_IS_MODULATABLE,
    CLAP_PARAM_IS_PERIODIC, CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED,
    CLAP_PARAM_REQUIRES_PROCESS, clap_param_info, clap_plugin_params,
};
use clap_sys::ext::render::{
    CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME, clap_plugin_render,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::clap_process;
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::clap_version_is_compatible;
use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;
use libloading::{Library, Symbol};

use crate::clap_host::Host;
use crate::plugin_instance::{
    AudioHalf, AudioProcessorHalf, EditorSizer, HostCallbacks, LoadedPlugin, NoteTransition,
    ResizableProbe, TimedNoteEvent,
};
use crate::process_scaffold::{
    self, TransportBlock, alloc_planar, alloc_planar_ports, copy_aux_inputs_planar,
    copy_input_planar, fold_mod_offset, refresh_ptrs, refresh_ptrs_ports,
};

/// Cached per-param metadata needed on the audio thread to convert a
/// normalized modulation offset. Built once at load.
#[derive(Clone, Copy)]
struct ClapParamMeta {
    min: f64,
    max: f64,
    /// `CLAP_PARAM_IS_MODULATABLE` — eligible for non-destructive `param_mod`.
    modulatable: bool,
}

// ====================================================================
// Audio half
// ====================================================================

/// Audio-thread half of a CLAP instance: every field `process()` reads or
/// writes. Buffers are (re)allocated by [`AudioProcessorHalf::on_activate`]
/// from the plugin-main thread inside a quiesced window.
pub struct ClapAudioHalf {
    plugin: *const clap_plugin,
    /// Defensive gate mirroring the main half's `processing` lifecycle flag
    /// (master lives on the main half; this copy is updated via
    /// `set_processing` inside quiesced windows).
    processing: bool,
    input_channels: u32,
    output_channels: u32,
    aux_input_channels: Vec<u32>,
    aux_output_channels: Vec<u32>,
    input_buffers: Vec<Vec<f32>>,
    input_ptrs: Vec<*mut f32>,
    output_buffers: Vec<Vec<f32>>,
    output_ptrs: Vec<*mut f32>,
    aux_input_buffers: Vec<Vec<Vec<f32>>>,
    aux_input_ptrs: Vec<Vec<*mut f32>>,
    aux_output_buffers: Vec<Vec<Vec<f32>>>,
    aux_output_ptrs: Vec<Vec<*mut f32>>,
    /// Pre-allocated `clap_audio_buffer` arrays reused every buffer
    /// (main first, then aux ports) — RT-safe clear()+push() reuse.
    process_input_bufs: Vec<clap_audio_buffer>,
    process_output_bufs: Vec<clap_audio_buffer>,
    /// Pre-allocated input note event buffer; filled per call.
    pending_events: Vec<clap_event_note>,
    /// Pre-allocated input parameter event buffer (absolute values).
    pending_param_events: Vec<clap_event_param_value>,
    /// Pre-allocated `CLAP_EVENT_PARAM_MOD` buffer (modulatable params).
    pending_param_mods: Vec<clap_event_param_mod>,
    /// Per-param `(min, max, modulatable)` cached at load so process() can
    /// convert a normalized modulation offset without main-thread calls.
    param_meta: std::collections::HashMap<u32, ClapParamMeta>,
    /// Last absolute value the host sent per param (base for the
    /// non-modulatable fold path). Seeded with defaults at load — process()
    /// only updates existing keys (no RT heap alloc).
    last_param_value: std::collections::HashMap<u32, f64>,
    /// Pre-allocated time-ascending merge order over the three event
    /// streams (CLAP requires `in_events` sorted by `header.time`).
    event_order: Vec<EventOrderRef>,
    /// Notes / param gestures emitted by the plugin during the previous
    /// `process()` (drained by the process server).
    collected_out_notes: Vec<TimedNoteEvent>,
    collected_out_param_touches: Vec<u32>,
    collected_out_param_values: Vec<(u32, f64)>,
    collected_out_param_releases: Vec<u32>,
}

// SAFETY: the raw plugin pointer is only used for the CLAP audio-thread API
// (`process`) under the AudioHalf exclusive-access contract; the buffers are
// plain owned data.
unsafe impl Send for ClapAudioHalf {}

impl AudioProcessorHalf for ClapAudioHalf {
    fn process(
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
        // Param events → clap_event_param_value / clap_event_param_mod.
        // `ParamEventKind::Mod` events carry a *normalized* offset: on
        // modulatable params we emit a non-destructive PARAM_MOD (amount =
        // offset·(max−min)); non-modulatable params get the offset folded
        // into an absolute value over the cached base (scaffold helpers).
        use crate::plugin_instance::ParamEventKind;
        process_scaffold::update_param_base_cache(&mut self.last_param_value, param_events);
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
                        // No CLAP modulation channel: fold into an absolute
                        // value over the cached base (plain units).
                        let base = self
                            .last_param_value
                            .get(&ev.param_id)
                            .copied()
                            .unwrap_or(meta.min);
                        let value = fold_mod_offset(base, amount, meta.min, meta.max);
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

        // Copy inputs / aux inputs into pre-allocated planar buffers and
        // refresh channel pointers (format-independent scaffold).
        let n = frames as usize;
        copy_input_planar(&mut self.input_buffers, input_audio, n);
        refresh_ptrs(&mut self.input_buffers, &mut self.input_ptrs);
        copy_aux_inputs_planar(&mut self.aux_input_buffers, aux_inputs, n);
        refresh_ptrs_ports(&mut self.aux_input_buffers, &mut self.aux_input_ptrs);
        refresh_ptrs(&mut self.output_buffers, &mut self.output_ptrs);
        refresh_ptrs_ports(&mut self.aux_output_buffers, &mut self.aux_output_ptrs);

        // Assemble the clap_audio_buffer arrays — main port first, then aux
        // ports (pre-allocated Vec reuse; no RT alloc).
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
        self.process_output_bufs.clear();
        if self.output_channels > 0 {
            self.process_output_bufs.push(clap_audio_buffer {
                data32: self.output_ptrs.as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: self.output_channels,
                latency: 0,
                constant_mask: 0,
            });
        }
        for port_idx in 0..self.aux_output_channels.len() {
            self.process_output_bufs.push(clap_audio_buffer {
                data32: self.aux_output_ptrs[port_idx].as_mut_ptr(),
                data64: std::ptr::null_mut(),
                channel_count: self.aux_output_channels[port_idx],
                latency: 0,
                constant_mask: 0,
            });
        }

        // Time-ascending merge order over note + param events. Non-allocating
        // `sort_unstable_by_key` with composite key reproduces a stable order
        // (same time ⇒ Note < Param < ParamMod, then push order).
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
        self.event_order
            .sort_unstable_by_key(|r| (r.time, r.stream, r.idx));

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
        let (audio_outputs, audio_outputs_count) = if self.process_output_bufs.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (
                self.process_output_bufs.as_mut_ptr(),
                self.process_output_bufs.len() as u32,
            )
        };

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
        Ok(status)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        self.output_buffers.get(channel).map(Vec::as_slice)
    }

    /// パラアウト: **Port 0 is the plugin's MAIN output bus**; ports `1..`
    /// are its `is_main=false` aux buses. Single-output plugins report no
    /// paraout ports at all.
    fn aux_output_buffer(&self, port: usize, channel: usize) -> Option<&[f32]> {
        if self.aux_output_channels.is_empty() {
            return None;
        }
        if port == 0 {
            self.output_buffers.get(channel).map(Vec::as_slice)
        } else {
            self.aux_output_buffers
                .get(port - 1)
                .and_then(|p| p.get(channel))
                .map(Vec::as_slice)
        }
    }

    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>) {
        out.append(&mut self.collected_out_notes);
    }

    fn drain_out_param_touches_into(&mut self, out: &mut Vec<u32>) {
        out.append(&mut self.collected_out_param_touches);
    }

    fn drain_out_param_values_into(&mut self, out: &mut Vec<(u32, f64)>) {
        out.append(&mut self.collected_out_param_values);
    }

    fn drain_out_param_releases_into(&mut self, out: &mut Vec<u32>) {
        out.append(&mut self.collected_out_param_releases);
    }

    fn on_activate(&mut self, _sample_rate: f64, max_frames: u32) {
        let mf = max_frames as usize;
        (self.input_buffers, self.input_ptrs) = alloc_planar(self.input_channels as usize, mf);
        (self.output_buffers, self.output_ptrs) = alloc_planar(self.output_channels as usize, mf);
        (self.aux_input_buffers, self.aux_input_ptrs) =
            alloc_planar_ports(&self.aux_input_channels, mf);
        (self.aux_output_buffers, self.aux_output_ptrs) =
            alloc_planar_ports(&self.aux_output_channels, mf);
        self.process_input_bufs = Vec::with_capacity(1 + self.aux_input_channels.len());
        self.process_output_bufs = Vec::with_capacity(1 + self.aux_output_channels.len());
    }

    fn on_deactivate(&mut self) {
        self.output_buffers.clear();
        self.output_ptrs.clear();
        self.aux_output_buffers.clear();
        self.aux_output_ptrs.clear();
    }

    fn set_processing(&mut self, on: bool) {
        self.processing = on;
    }
}

// ====================================================================
// Main half
// ====================================================================

/// Loaded CLAP plugin instance (main half). Holds every resource alive
/// until dropped. Drop sequence:
///   1. `impl Drop` body — explicit `gui.destroy` → `plugin.destroy` →
///      `entry.deinit` (all DLL calls).
///   2. fields in declaration order; `_library` is declared LAST so
///      `FreeLibrary` runs after every other field's Drop.
pub struct ClapPlugin {
    entry: *const clap_plugin_entry,
    plugin: *const clap_plugin,
    /// CLAP host 実装。`host_data` としてプラグインが握る生ポインタの寿命を持つ。
    /// r.md #65 以降は `host.callbacks.editor_hwnd` (エディタ窓の公開先) も参照する。
    host: Box<Host>,
    /// Stable `clap_plugin_descriptor.id` of the loaded descriptor.
    id: String,
    /// ARA session bound to this instance, if any.
    ara: Option<crate::ara::session::AraSession>,
    /// Last successful `activate` params (ARA setup の deactivate →
    /// reactivate 用)。
    last_activate: Option<(f64, u32, u32)>,
    name: String,
    path: PathBuf,
    active: bool,
    /// Processing gate の SSoT (audio half の同名 flag は quiesced window で
    /// 同期される defensive copy)。
    processing: bool,
    /// パラアウト port 数 (audio half の bus 構成から load 時に確定)。
    paraout_port_count: usize,
    gui_ext: Option<*const clap_plugin_gui>,
    gui_created: bool,
    /// r.md #65: `plugin` / `gui_ext` の生ポインタをエディタ窓の WNDPROC へ
    /// 貸してよいか。`gui_create_embedded` で立て、**`gui_destroy` の先頭**で落とす。
    /// [`ClapSizer`] はこれを見て destroy 済みの GUI を二度と触らない。
    gui_alive: Arc<AtomicBool>,
    state_ext: Option<*const clap_plugin_state>,
    latency_ext: Option<*const clap_plugin_latency>,
    params_ext: Option<*const clap_plugin_params>,
    /// Audio half (shared with the worker registry via `audio_half()`).
    audio: Arc<AudioHalf>,
    /// DLL handle. Declared LAST so `FreeLibrary` runs after every other
    /// field's Drop.
    _library: Library,
}

// The plugin holds raw pointers but ownership is exclusive within the struct.
unsafe impl Send for ClapPlugin {}

impl ClapPlugin {
    /// Tries to load a plugin from `path`. Scans all descriptors in the file
    /// and instantiates the first one matching `target_id` when provided, or
    /// otherwise the first one for which `matches(features)` returns true.
    /// Returns `Ok(None)` if no descriptor matches.
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
        let aux_output_channels = query_aux_output_channels(plugin_ptr, get_ext);
        tracing::info!(
            input_channels,
            output_channels,
            aux_input_count = aux_input_channels.len(),
            aux_output_count = aux_output_channels.len(),
            "plugin audio channel count"
        );

        // Look up optional extensions.
        let gui_ptr =
            unsafe { get_ext(plugin_ptr, CLAP_EXT_GUI.as_ptr()) } as *const clap_plugin_gui;
        let gui_ext = if gui_ptr.is_null() { None } else { Some(gui_ptr) };
        tracing::info!(has_gui = gui_ext.is_some(), "plugin gui extension");

        let state_ptr = unsafe { get_ext(plugin_ptr, CLAP_EXT_STATE.as_ptr()) }
            as *const clap_plugin_state;
        let state_ext = if state_ptr.is_null() {
            None
        } else {
            Some(state_ptr)
        };
        tracing::info!(has_state = state_ext.is_some(), "plugin state extension");

        let latency_ptr = unsafe { get_ext(plugin_ptr, CLAP_EXT_LATENCY.as_ptr()) }
            as *const clap_plugin_latency;
        let latency_ext = if latency_ptr.is_null() {
            None
        } else {
            Some(latency_ptr)
        };
        tracing::info!(has_latency = latency_ext.is_some(), "plugin latency extension");

        let params_ptr = unsafe { get_ext(plugin_ptr, CLAP_EXT_PARAMS.as_ptr()) }
            as *const clap_plugin_params;
        let params_ext = if params_ptr.is_null() {
            None
        } else {
            Some(params_ptr)
        };
        tracing::info!(has_params = params_ext.is_some(), "plugin params extension");

        // Seed the audio half's param caches from the param list (main
        // thread, before the instance is ever published) so process() never
        // allocates.
        let infos = enumerate_clap_params(plugin_ptr, params_ext);
        let mut param_meta = std::collections::HashMap::with_capacity(infos.len());
        let mut last_param_value = std::collections::HashMap::with_capacity(infos.len());
        for info in &infos {
            let modulatable =
                info.flags & common::protocol::plugin_param_flags::MODULATABLE != 0;
            param_meta.insert(
                info.id,
                ClapParamMeta {
                    min: info.min_value,
                    max: info.max_value,
                    modulatable,
                },
            );
            last_param_value.insert(info.id, info.default_value);
        }

        // パラアウト port 数 = 1 (main) + aux bus 数 (multi-out のみ)。
        let paraout_port_count = if aux_output_channels.is_empty() {
            0
        } else {
            (1 + aux_output_channels.len()).min(common::process_data::MAX_AUX_OUT)
        };

        let audio = AudioHalf::new(Box::new(ClapAudioHalf {
            plugin: plugin_ptr,
            processing: false,
            input_channels,
            output_channels,
            aux_input_channels,
            aux_output_channels,
            input_buffers: Vec::new(),
            input_ptrs: Vec::new(),
            output_buffers: Vec::new(),
            output_ptrs: Vec::new(),
            aux_input_buffers: Vec::new(),
            aux_input_ptrs: Vec::new(),
            aux_output_buffers: Vec::new(),
            aux_output_ptrs: Vec::new(),
            process_input_bufs: Vec::new(),
            process_output_bufs: Vec::new(),
            // Capacity sized so 64 events from the song plus MIDI FX
            // expansion never allocate on the audio thread.
            pending_events: Vec::with_capacity(256),
            pending_param_events: Vec::with_capacity(256),
            pending_param_mods: Vec::with_capacity(256),
            param_meta,
            last_param_value,
            event_order: Vec::with_capacity(512),
            collected_out_notes: Vec::with_capacity(256),
            collected_out_param_touches: Vec::with_capacity(64),
            collected_out_param_values: Vec::with_capacity(256),
            collected_out_param_releases: Vec::with_capacity(64),
        }));

        Ok(Some(Self {
            entry: entry_ptr,
            plugin: plugin_ptr,
            host,
            id,
            ara: None,
            last_activate: None,
            name,
            path: path.to_path_buf(),
            active: false,
            processing: false,
            paraout_port_count,
            gui_ext,
            gui_created: false,
            gui_alive: Arc::new(AtomicBool::new(false)),
            state_ext,
            latency_ext,
            params_ext,
            audio,
            _library: library,
        }))
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

    fn gui_ref(&self) -> Option<&clap_plugin_gui> {
        self.gui_ext.and_then(|p| unsafe { p.as_ref() })
    }

    /// Exclusive access to the audio half.
    ///
    /// # Safety
    /// Caller (plugin-main thread) must be inside a quiesced window — the
    /// instance is not published in the worker registry, or it was detached
    /// and `WorkerPool::quiesce` completed (`AudioHalf::get` contract).
    #[allow(clippy::mut_from_ref)] // UnsafeCell 経由。契約は Safety 節。
    unsafe fn audio_half_mut(&self) -> &mut (dyn AudioProcessorHalf + 'static) {
        unsafe { self.audio.get() }
    }
}

impl crate::ara::AraLifecycleHost for ClapPlugin {
    fn ara_session(&self) -> Option<&crate::ara::session::AraSession> {
        self.ara.as_ref()
    }

    fn ara_session_mut(&mut self) -> &mut Option<crate::ara::session::AraSession> {
        &mut self.ara
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn last_activate_params(&self) -> Option<(f64, u32, u32)> {
        self.last_activate
    }

    fn do_deactivate(&mut self) {
        LoadedPlugin::deactivate(self);
    }

    fn do_activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()> {
        LoadedPlugin::activate(self, sample_rate, min_frames, max_frames)
    }
}

impl LoadedPlugin for ClapPlugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn format(&self) -> PluginFormat {
        PluginFormat::Clap
    }

    fn audio_half(&self) -> Arc<AudioHalf> {
        Arc::clone(&self.audio)
    }

    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()> {
        anyhow::ensure!(!self.active, "plugin already active");
        let activate = unsafe { (*self.plugin).activate }.context("plugin.activate is null")?;
        anyhow::ensure!(
            unsafe { activate(self.plugin, sample_rate, min_frames, max_frames) },
            "plugin.activate returned false"
        );
        self.active = true;
        self.last_activate = Some((sample_rate, min_frames, max_frames));
        // SAFETY: quiesced window (install / reinit / ARA setup — the entry
        // is detached from the registry at every call site).
        unsafe { self.audio_half_mut().on_activate(sample_rate, max_frames) };
        tracing::info!(sample_rate, max_frames, "plugin activated");
        Ok(())
    }

    fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        if let Some(deact) = unsafe { (*self.plugin).deactivate } {
            unsafe { deact(self.plugin) };
        }
        self.active = false;
        // SAFETY: quiesced window (same call sites as `activate`).
        unsafe { self.audio_half_mut().on_deactivate() };
    }

    fn start_processing(&mut self) -> Result<()> {
        anyhow::ensure!(self.active, "plugin not active");
        anyhow::ensure!(!self.processing, "plugin already processing");
        let start = unsafe { (*self.plugin).start_processing }
            .context("plugin.start_processing is null")?;
        anyhow::ensure!(
            unsafe { start(self.plugin) },
            "plugin.start_processing returned false"
        );
        self.processing = true;
        // SAFETY: quiesced window.
        unsafe { self.audio_half_mut().set_processing(true) };
        Ok(())
    }

    fn stop_processing(&mut self) {
        if !self.processing {
            return;
        }
        if let Some(stop) = unsafe { (*self.plugin).stop_processing } {
            unsafe { stop(self.plugin) };
        }
        self.processing = false;
        // SAFETY: quiesced window.
        unsafe { self.audio_half_mut().set_processing(false) };
    }

    /// CLAP `clap_plugin.reset()` — clear tails while keeping parameters.
    /// The caller invokes it after the instance is detached + quiesced.
    fn reset(&mut self) {
        if !self.active {
            return;
        }
        if let Some(reset) = unsafe { (*self.plugin).reset } {
            unsafe { reset(self.plugin) };
        }
    }

    /// CLAP `clap_plugin.on_main_thread()` — runs the task the plugin
    /// scheduled via `clap_host.request_callback`.
    fn on_main_thread(&mut self) {
        if let Some(cb) = unsafe { (*self.plugin).on_main_thread } {
            unsafe { cb(self.plugin) };
        }
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

    /// Query CLAP plugin's reported latency. Spec: `[main-thread & active]`.
    fn query_latency(&mut self) -> u32 {
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

    fn enumerate_params(&self) -> Vec<common::protocol::PluginParamInfo> {
        enumerate_clap_params(self.plugin, self.params_ext)
    }

    /// Serializes the plugin's internal state via `clap_plugin_state.save`.
    fn state_save(&self) -> Result<Option<Vec<u8>>> {
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

    /// Restores previously captured state via `clap_plugin_state.load`.
    fn state_load(&mut self, data: &[u8]) -> Result<()> {
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

    fn aux_output_port_count(&self) -> usize {
        self.paraout_port_count
    }

    // --- ARA -------------------------------------------------------------

    /// ARA bind at load (before the first activate / state load / GUI).
    /// Returns `Ok(false)` when this descriptor exposes no ARA factory.
    fn bind_ara_if_capable(&mut self) -> Result<bool> {
        let id = CString::new(self.id.as_str()).context("plugin id has interior NUL")?;
        let factory =
            match unsafe { crate::ara::clap_ara::ara_factory_for_plugin(&*self.entry, &id) } {
                Some(factory) => factory,
                None => return Ok(false),
            };
        let plugin = self.plugin;
        let session = unsafe {
            crate::ara::session::AraSession::create(factory, |document_controller| {
                crate::ara::clap_ara::bind_to_document(
                    plugin,
                    document_controller,
                    crate::ara::extension::HOST_KNOWN_ROLES,
                    crate::ara::extension::HOST_ASSIGNED_ROLES,
                )
            })
        }?;
        self.ara = Some(session);
        Ok(true)
    }

    fn setup_ara(
        &mut self,
        clips: &[common::protocol::AraClipSpec],
        bpm: f64,
        time_sig: (u16, u16),
        archive: Option<&[u8]>,
    ) -> Result<bool> {
        crate::ara::run_setup_ara(self, clips, bpm, time_sig, archive)
    }

    fn clear_ara(&mut self) {
        crate::ara::run_clear_ara(self);
    }

    fn update_ara_regions(&self, regions: &[common::protocol::AraRegionUpdate]) {
        if let Some(session) = self.ara.as_ref() {
            session.update_regions(regions);
        }
    }

    fn notify_ara_model_updates(&self) {
        if let Some(session) = self.ara.as_ref() {
            session.notify_model_updates();
        }
    }

    fn has_ara_session(&self) -> bool {
        self.ara.is_some()
    }

    fn store_ara_archive(&self) -> Option<Vec<u8>> {
        self.ara.as_ref().and_then(|session| session.store_archive())
    }

    // --- GUI extension wrappers -------------------------------------------

    /// Returns true when the plugin advertises the `clap.gui` extension and
    /// supports embedded (non-floating) Win32 windows.
    fn gui_is_embed_supported(&self) -> bool {
        let Some(gui) = self.gui_ref() else {
            return false;
        };
        let Some(f) = gui.is_api_supported else { return false };
        unsafe { f(self.plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) }
    }

    /// Create the plugin's embedded (Win32) GUI resources. Idempotent.
    fn gui_create_embedded(&mut self) -> Result<()> {
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
        // r.md #65: ここから `gui_destroy` までの間だけ、WNDPROC は plugin /
        // gui 拡張の生ポインタを借りてよい。
        self.gui_alive.store(true, Ordering::Release);
        Ok(())
    }

    /// Returns the plugin's preferred initial size, or `None` if the call
    /// fails. Must be called between `create` and `set_parent`/`show`.
    fn gui_get_size(&self) -> Option<(u32, u32)> {
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
    /// allowed (plugin preferred to compute its own scale).
    fn gui_set_scale(&self, scale: f64) -> Result<bool> {
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let Some(f) = gui.set_scale else {
            // Optional entry; treat missing as "plugin ignored the scale".
            return Ok(false);
        };
        Ok(unsafe { f(self.plugin, scale) })
    }

    fn gui_sizer(&self) -> Option<Box<dyn EditorSizer>> {
        if !self.gui_created {
            return None;
        }
        let gui = self.gui_ext?;
        Some(Box::new(ClapSizer {
            plugin: self.plugin,
            gui,
            alive: Arc::clone(&self.gui_alive),
        }))
    }

    /// Embed into the given host Win32 HWND (passed as a raw `u64` pointer).
    fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()> {
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let f = gui.set_parent.context("gui.set_parent is null")?;
        // `set_parent` の内側から `request_resize` を投げるプラグインがあるので、
        // 窓は呼ぶ前に公開しておく (VST3 `attached` と同じ理由)。
        self.host.callbacks.editor_hwnd.store(hwnd, Ordering::Release);
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

    /// Calls `gui.show`. `Ok(false)` = the plugin returned false (VCV Rack 2
    /// does even after successfully showing).
    fn gui_show(&self) -> Result<bool> {
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let f = gui.show.context("gui.show is null")?;
        Ok(unsafe { f(self.plugin) })
    }

    fn gui_hide(&self) -> Result<()> {
        // Plugins reject hide() calls when create() hasn't happened.
        if !self.gui_created {
            return Ok(());
        }
        let gui = self.gui_ref().context("plugin has no gui extension")?;
        let f = gui.hide.context("gui.hide is null")?;
        anyhow::ensure!(unsafe { f(self.plugin) }, "gui.hide returned false");
        Ok(())
    }

    /// Tear down the GUI. Safe to call even if `gui_create_embedded` was not
    /// called (no-op). Idempotent.
    fn gui_destroy(&mut self) {
        // **先頭で** alive を落とす (r.md #65)。以後 `ClapSizer` は FFI を呼ばない。
        self.gui_alive.store(false, Ordering::Release);
        self.host.callbacks.editor_hwnd.store(0, Ordering::Release);
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
}

/// [`EditorSizer`] の CLAP 実装 (r.md #65)。手順は `clap/ext/gui.h` L41-45 の
/// *"Resizing the window (drag, if embedded)"* に一致させる:
/// `can_resize()` → `adjust_size(new_size)` → `set_size(working_size)`。
///
/// VST3 版と同じく **borrowed なポインタしか持たない**。`plugin` / `gui` の所有は
/// [`ClapPlugin`] にあり、`gui_destroy` が先頭で `alive` を落とす。
struct ClapSizer {
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
    alive: Arc<AtomicBool>,
}

// plugin-main スレッド専用 (`EditorSizer: Send` を満たすためだけの宣言)。
unsafe impl Send for ClapSizer {}

impl ClapSizer {
    fn gui(&self) -> Option<&clap_plugin_gui> {
        if !self.alive.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `alive` が true の間は `ClapPlugin` が plugin instance を保持して
        // いるので、そこから取った拡張ポインタも有効。
        unsafe { self.gui.as_ref() }
    }
}

impl EditorSizer for ClapSizer {
    fn constrain_client_size(&self, w: u32, h: u32) -> (u32, u32) {
        let Some(gui) = self.gui() else { return (w, h) };
        let Some(f) = gui.adjust_size else { return (w, h) };
        let (mut aw, mut ah) = (w, h);
        // ヘッダは *"Returns true if the plugin could adjust the given size."* と
        // 戻り値を **明示的に規定している**ので、VST3 と違いここは戻り値で分岐してよい。
        if unsafe { f(self.plugin, &mut aw, &mut ah) } && aw > 0 && ah > 0 {
            (aw, ah)
        } else {
            (w, h)
        }
    }

    fn plugin_view_size(&self) -> Option<(u32, u32)> {
        let gui = self.gui()?;
        let f = gui.get_size?;
        let (mut w, mut h) = (0u32, 0u32);
        (unsafe { f(self.plugin, &mut w, &mut h) } && w > 0 && h > 0).then_some((w, h))
    }

    fn notify_client_size(&self, w: u32, h: u32) {
        let Some(gui) = self.gui() else { return };
        let Some(f) = gui.set_size else { return };
        // r.md #65: VST3 の `onSize` と同じく戻り値を info で残す
        // (「呼んだ」と「受け入れられた」は別の事実)。
        let accepted = unsafe { f(self.plugin, w, h) };
        tracing::info!(target: "editor_resize", accepted, w, h, "CLAP gui.set_size");
    }

    fn can_resize(&self) -> ResizableProbe {
        // gui 拡張が無い / `can_resize` が null = 問い合わせられない。
        // 「プラグインが不可と答えた」のとは別の事実なので区別して残す。
        let Some(f) = self.gui().and_then(|gui| gui.can_resize) else {
            return ResizableProbe::unavailable();
        };
        let verdict = unsafe { f(self.plugin) };
        ResizableProbe {
            verdict,
            queried: true,
            raw: i32::from(verdict),
            // CLAP は `gui.h` L41-45 で
            // *"Resizing the window (drag, if embedded): 1. Only possible if
            // clap_plugin_gui->can_resize() returns true"* と **前提条件として**
            // 規定している。VST3 と違い申告を尊重する。
            drag_requires_verdict: true,
        }
    }

    fn resize_hints(&self) -> Option<crate::plugin_instance::ResizeHints> {
        let gui = self.gui()?;
        let f = gui.get_resize_hints?;
        let mut hints = clap_gui_resize_hints {
            can_resize_horizontally: false,
            can_resize_vertically: false,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 0,
            aspect_ratio_height: 0,
        };
        if !unsafe { f(self.plugin, &mut hints) } {
            return None;
        }
        Some(crate::plugin_instance::ResizeHints {
            can_resize_horizontally: hints.can_resize_horizontally,
            can_resize_vertically: hints.can_resize_vertically,
            preserve_aspect_ratio: hints.preserve_aspect_ratio,
            aspect_ratio_width: hints.aspect_ratio_width,
            aspect_ratio_height: hints.aspect_ratio_height,
        })
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        // Tear down the ARA session (if any) before the instance is destroyed —
        // its drop issues destroy calls back into this plug-in. Deactivate first
        // so detaching its playback regions is valid.
        if self.ara.is_some() {
            if self.active {
                LoadedPlugin::deactivate(self);
            }
            self.ara = None;
        }
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

// ====================================================================
// Param enumeration (free fn — load-time seeding + trait method の SSoT)
// ====================================================================

/// Enumerate every parameter the plugin exposes via `clap_plugin_params`.
/// Called on the plugin-main thread.
fn enumerate_clap_params(
    plugin: *const clap_plugin,
    params_ext: Option<*const clap_plugin_params>,
) -> Vec<common::protocol::PluginParamInfo> {
    let Some(params) = params_ext else {
        return Vec::new();
    };
    let count_fn = unsafe { (*params).count };
    let get_info_fn = unsafe { (*params).get_info };
    let (Some(count_fn), Some(get_info_fn)) = (count_fn, get_info_fn) else {
        return Vec::new();
    };
    // plugin が返す count は untrusted。異常値で巨大確保しない上限。
    const MAX_PARAMS: u32 = 65536;
    let count = unsafe { count_fn(plugin) }.min(MAX_PARAMS);
    let mut out: Vec<common::protocol::PluginParamInfo> = Vec::with_capacity(count as usize);
    for i in 0..count {
        // SAFETY: clap_param_info is plain old data; zeroed start is legal
        // because get_info fully overwrites the fields it populates.
        let mut info: clap_param_info = unsafe { std::mem::zeroed() };
        let ok = unsafe { get_info_fn(plugin, i, &raw mut info) };
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

// ====================================================================
// State stream callbacks
// ====================================================================

/// Read-only cursor over a `&[u8]` used by the istream callback below.
struct StateCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

/// `clap_ostream.write`: write up to `size` bytes from `buffer` into the
/// `Vec<u8>` referenced by `ctx`.
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

// ====================================================================
// out_events collector
// ====================================================================

/// ctx for `out_events.try_push` — gathers note transitions and
/// plugin-initiated parameter gestures / value changes into the audio
/// half's pre-allocated vectors. Built on the stack at the top of every
/// `process()`; the raw pointers are valid for the FFI call's duration.
struct OutEventCollector {
    notes: *mut Vec<TimedNoteEvent>,
    param_touches: *mut Vec<u32>,
    param_values: *mut Vec<(u32, f64)>,
    param_releases: *mut Vec<u32>,
}

/// `try_push` callback shared by all CLAP plugins. Unknown types are
/// accepted (return `true`) but not recorded.
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
            // FFI 安全: malformed event に備えて size 検証 → deref。
            if header.size < std::mem::size_of::<clap_event_note>() as u32 {
                return true;
            }
            let note = unsafe { &*(event as *const clap_event_note) };
            let note_id = note.note_id.max(0) as u32;
            let transition = NoteTransition::On {
                note_id,
                key: note.key.clamp(0, 127) as u8,
                velocity: note.velocity,
            };
            if !collector.notes.is_null() {
                let out = unsafe { &mut *collector.notes };
                // RT 安全: 事前確保 capacity を超える push は drop。
                if out.len() < out.capacity() {
                    out.push(TimedNoteEvent { time: header.time, event: transition });
                }
            }
        }
        t if t == CLAP_EVENT_NOTE_OFF => {
            if header.size < std::mem::size_of::<clap_event_note>() as u32 {
                return true;
            }
            let note = unsafe { &*(event as *const clap_event_note) };
            let note_id = note.note_id.max(0) as u32;
            let transition = NoteTransition::Off {
                note_id,
                key: note.key.clamp(0, 127) as u8,
            };
            if !collector.notes.is_null() {
                let out = unsafe { &mut *collector.notes };
                if out.len() < out.capacity() {
                    out.push(TimedNoteEvent { time: header.time, event: transition });
                }
            }
        }
        t if t == CLAP_EVENT_PARAM_GESTURE_BEGIN => {
            // `clap_event_param_gesture` は header 直後に param_id (u32)。
            // FFI 安全: size 検証してから raw offset を deref (OOB 防御)。
            const REQUIRED: u32 =
                (std::mem::size_of::<clap_event_header>() + std::mem::size_of::<u32>()) as u32;
            if header.size < REQUIRED {
                return true;
            }
            let param_id = unsafe {
                let body = event as *const u8;
                let id_ptr = body.add(std::mem::size_of::<clap_event_header>()) as *const u32;
                *id_ptr
            };
            if !collector.param_touches.is_null() {
                let out = unsafe { &mut *collector.param_touches };
                if out.len() < out.capacity() {
                    out.push(param_id);
                }
            }
        }
        t if t == CLAP_EVENT_PARAM_GESTURE_END => {
            const REQUIRED: u32 =
                (std::mem::size_of::<clap_event_header>() + std::mem::size_of::<u32>()) as u32;
            if header.size < REQUIRED {
                return true;
            }
            let param_id = unsafe {
                let body = event as *const u8;
                let id_ptr = body.add(std::mem::size_of::<clap_event_header>()) as *const u32;
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
            if header.size < std::mem::size_of::<clap_event_param_value>() as u32 {
                return true;
            }
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

// ====================================================================
// transport
// ====================================================================

/// Build a `clap_event_transport` from the host-side `TransportContext`.
/// Timeline math (beats → seconds / bar derivation / 非有限 sanitize) is the
/// shared [`TransportBlock`] — VST3 maps the same block into its
/// `ProcessContext`, so the two formats can never drift.
fn build_clap_transport_event(
    transport: &crate::plugin_instance::TransportContext,
    _frames: u32,
) -> clap_event_transport {
    let b = TransportBlock::derive(transport, f64::from(transport.sample_rate));
    let song_pos_beats: i64 = (b.pos_beats * CLAP_BEATTIME_FACTOR as f64) as i64;
    let song_pos_seconds: i64 = (b.pos_seconds * CLAP_SECTIME_FACTOR as f64) as i64;
    let bar_start: i64 = (b.bar_start_beats * CLAP_BEATTIME_FACTOR as f64) as i64;
    let mut flags = CLAP_TRANSPORT_HAS_TEMPO
        | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
        | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
        | CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
    if b.is_playing {
        flags |= CLAP_TRANSPORT_IS_PLAYING;
    }
    if b.cycle_active {
        flags |= CLAP_TRANSPORT_IS_LOOP_ACTIVE;
    }
    let loop_start_beats: i64 = (b.loop_start_beats * CLAP_BEATTIME_FACTOR as f64) as i64;
    let loop_end_beats: i64 = (b.loop_end_beats * CLAP_BEATTIME_FACTOR as f64) as i64;
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
        tempo: b.bpm,
        tempo_inc: 0.0,
        loop_start_beats,
        loop_end_beats,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start,
        // bar_number i32: 長時間再生で overflow しないよう defensive clamp。
        #[allow(clippy::cast_possible_truncation)]
        bar_number: b.bar_number.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
        tsig_num: b.tsig_num,
        tsig_denom: b.tsig_denom,
    }
}

// ====================================================================
// in_events view
// ====================================================================

/// Which pre-allocated stream an [`EventOrderRef`] indexes into. `Ord` は
/// merge sort の複合キー用 — 同 `time` では宣言順 (Note → Param → ParamMod)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EventStream {
    Note,
    Param,
    ParamMod,
}

/// One entry in [`EventListView::order`]: references a note / param_value /
/// param_mod event by index, tagged with its `header.time`.
#[derive(Clone, Copy)]
struct EventOrderRef {
    time: u32,
    stream: EventStream,
    idx: u32,
}

/// `clap_input_events.ctx` が `*const Self` を指す。`process()` 呼び出しの
/// 間だけ存続する短命オブジェクト。
struct EventListView<'a> {
    notes: &'a [clap_event_note],
    params: &'a [clap_event_param_value],
    param_mods: &'a [clap_event_param_mod],
    /// Time-sorted merge order over the three streams. Built per `process()`.
    order: &'a [EventOrderRef],
}

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
    // `note_id` を CLAP `clap_event_note.note_id` に詰める。i32 で `-1`
    // (= "未指定") が sentinel。
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

// ====================================================================
// port queries
// ====================================================================

fn query_output_channel_count(plugin: *const clap_plugin, get_ext: GetExtFn) -> u32 {
    query_port_channel_count(plugin, get_ext, false)
}

/// PR4 sidechain: enumerate the plugin's `is_main=false` input ports and
/// return their channel counts in declaration order (capped at MAX_AUX_IN).
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
            // shmem aux buffer は MAX_CHANNELS plane しか持たないので clamp。
            aux.push(info.channel_count.min(common::process_data::MAX_CHANNELS as u32));
        }
    }
    aux
}

/// パラアウト: enumerate the plugin's `is_main=false` **output** ports
/// (capped at MAX_AUX_OUT). Symmetric to `query_aux_input_channels`.
fn query_aux_output_channels(plugin: *const clap_plugin, get_ext: GetExtFn) -> Vec<u32> {
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
    let port_count = unsafe { count_fn(plugin, false) };
    let mut aux: Vec<u32> = Vec::new();
    for i in 0..port_count {
        let mut info = std::mem::MaybeUninit::<clap_audio_port_info>::zeroed();
        let ok = unsafe { get(plugin, i, false, info.as_mut_ptr()) };
        if !ok {
            continue;
        }
        let info = unsafe { info.assume_init() };
        if info.flags & CLAP_AUDIO_PORT_IS_MAIN == 0 {
            if aux.len() >= common::process_data::MAX_AUX_OUT {
                tracing::warn!(
                    port_index = i,
                    cap = common::process_data::MAX_AUX_OUT,
                    "plugin declared more aux output ports than the host caps to"
                );
                break;
            }
            aux.push(info.channel_count.min(common::process_data::MAX_CHANNELS as u32));
        }
    }
    aux
}

/// Queries the plugin's first audio port in the given direction. Returns `0`
/// when the plugin declares no port of that direction.
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
    // NULL 終端が無い malformed descriptor で無限 walk しない上限。
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

// ====================================================================
// probe (one-shot --probe-clap)
// ====================================================================

/// CLAP プラグインを一時 instantiate して note-ports / audio-ports
/// extension から port 構成を読む (`--probe-clap` one-shot モード)。
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

    // target_id 一致の descriptor、空なら最初。
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
        // CLAP は映像 port を持たない。
        has_video_input: false,
        has_video_output: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_instance::TransportContext;

    fn ctx() -> TransportContext {
        TransportContext {
            bpm: 120.0,
            sample_rate: 48_000,
            // 真の拍位置 999.0 (= tempo automation で累積が sample×bpm の
            // 線形換算と一致しないケースを模擬)。
            song_pos_beats: 999.0,
            tsig_num: 4,
            tsig_denom: 4,
            is_playing: true,
            is_looping: false,
            loop_start_beats: 0.0,
            loop_end_beats: 0.0,
            // アレンジ主導の行 = 行の実効拍は song 拍と同値 (engine の写像)。
            row: common::process_data::RowTransport {
                pos_beats: 999.0,
                ..Default::default()
            },
            pin_to_song: false,
        }
    }

    #[test]
    fn transport_event_uses_song_pos_beats_directly() {
        // SSoT 回帰防止: song_pos_beats は transport.song_pos_beats を直接
        // fixed-point 化したもので、samples × bpm の一定テンポ逆算ではない。
        let ev = build_clap_transport_event(&ctx(), 256);
        let expected = (999.0_f64 * CLAP_BEATTIME_FACTOR as f64) as i64;
        assert_eq!(ev.song_pos_beats, expected);
        // seconds も song_pos_beats から導出。999 拍 ÷ (120bpm/60) = 499.5 秒。
        let expected_sec = (999.0_f64 * 60.0 / 120.0 * CLAP_SECTIME_FACTOR as f64) as i64;
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
