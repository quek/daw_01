#![allow(unsafe_op_in_unsafe_fn)]

//! VST3 plugin wrapper.
//!
//! Split-half (`docs/plan_arch_refactor.md` §6): [`Vst3Plugin`] is the
//! main-thread half (lifecycle / GUI / state / ARA / params), and
//! [`Vst3AudioHalf`] is the audio-thread half (everything `process()`
//! touches). The audio half holds a **non-owning** `IAudioProcessor`
//! pointer (`ComRef` at call time, no refcount) so a stale registry
//! snapshot dropping its `Arc<AudioHalf>` after the main half unloaded the
//! DLL never calls `Release()` into unmapped code — the pointer is only
//! *used* inside dispatch windows, which the quiesce protocol serializes
//! against teardown.
//!
//! Cross-half shared state is limited to lock-free primitives:
//! `GuiParamEditQueue` (GUI → DSP edits), the render-mode flag, and the
//! one-shot diagnostic flags drained by `stop_processing`.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;
use crate::vst3_scan::resolve_vst3_dll;
use crate::vst3_scan::{c_array_to_string, tuid_to_hex};
use libloading::{Library, Symbol};
use vst3::{
    ComPtr, ComRef, ComWrapper, Interface,
    Steinberg::{
        FUnknown, IBStream, IPlugView, IPlugViewContentScaleSupport,
        IPlugViewContentScaleSupportTrait, IPlugViewTrait, IPluginBaseTrait, IPluginFactory,
        IPluginFactoryTrait, PClassInfo, TUID, ViewRect, kNotImplemented, kPlatformTypeHWND,
        kResultOk, kResultTrue,
        Vst::{
            AudioBusBuffers, AudioBusBuffers__type0, BusDirection, BusDirections_, BusInfo,
            Event, Event__type0, Event_, IAudioProcessor, IAudioProcessorTrait, IComponent,
            IComponentTrait, IConnectionPoint, IConnectionPointTrait, IEditController,
            IEditControllerTrait, IEventList, IParameterChanges, MediaTypes_, NoteOffEvent,
            NoteOnEvent, ParameterInfo, ParameterInfo_::ParameterFlags_, ProcessContext,
            ProcessContext_::StatesAndFlags_, ProcessData, ProcessModes_, ProcessSetup,
            SpeakerArr, SpeakerArrangement, SymbolicSampleSizes_,
        },
    },
};

use crate::plugin_instance::{
    AudioHalf, AudioProcessorHalf, EditorSizer, HostCallbacks, LoadedPlugin, NoteTransition,
    TimedNoteEvent,
};
use crate::process_scaffold::{
    self, TransportBlock, alloc_planar, alloc_planar_ports, copy_aux_inputs_planar,
    copy_input_planar, fold_mod_offset, refresh_ptrs, refresh_ptrs_ports,
};
use crate::vst3_events::{Vst3InEventList, Vst3OutEventList};
use crate::vst3_host::{Vst3ComponentHandler, Vst3HostApp, Vst3PlugFrame};
use crate::vst3_params::{GuiParamEditQueue, Vst3InParamChanges};
use crate::vst3_stream::{Vst3ReadStream, Vst3WriteStream};

// ====================================================================
// Audio half
// ====================================================================

/// Audio-thread half of a VST3 instance: every field `process()` reads or
/// writes. Buffers are (re)allocated by `on_activate` from the plugin-main
/// thread inside a quiesced window.
pub struct Vst3AudioHalf {
    /// Non-owning `IAudioProcessor` pointer (owning `ComPtr` lives on the
    /// main half). Wrapped as a borrowed `ComRef` per call — never
    /// AddRef'd/Release'd here, so this half's Drop never calls into the
    /// (possibly unloaded) DLL.
    audio_raw: *mut IAudioProcessor,
    /// Defensive gate mirroring the main half's `processing` flag.
    processing: bool,
    sample_rate: f64,
    input_channels: u32,
    output_channels: u32,
    aux_input_channels: Vec<u32>,
    /// Extra (non-main) output buses' channel counts (bus 1..N). VST3
    /// requires `numOutputs` == total output bus count; multi-output synths
    /// (Surge XT) otherwise mismatch and emit no main output.
    extra_output_channels: Vec<u32>,
    input_buffers: Vec<Vec<f32>>,
    input_ptrs: Vec<*mut f32>,
    output_buffers: Vec<Vec<f32>>,
    output_ptrs: Vec<*mut f32>,
    aux_input_buffers: Vec<Vec<Vec<f32>>>,
    aux_input_ptrs: Vec<Vec<*mut f32>>,
    extra_output_buffers: Vec<Vec<Vec<f32>>>,
    extra_output_ptrs: Vec<Vec<*mut f32>>,
    process_input_bufs: Vec<AudioBusBuffers>,
    process_output_bufs: Vec<AudioBusBuffers>,
    /// Scratch buffer for input events fed to the plugin this tick.
    in_event_buffer: Vec<Event>,
    /// Events the plugin emitted during the previous `process()`.
    collected_out_notes: Vec<TimedNoteEvent>,
    /// Reusable host-side COM objects fed to the plugin every `process()`
    /// (no per-buffer heap alloc). These are Rust-owned vtables — their
    /// Drop never calls into the plugin DLL.
    in_event_list: ComWrapper<Vst3InEventList>,
    out_event_list: ComWrapper<Vst3OutEventList>,
    in_param_changes: ComWrapper<Vst3InParamChanges>,
    /// Last absolute normalized value sent per param (mod-fold base).
    /// Seeded with defaults at load → no RT allocation.
    param_mod_base: std::collections::HashMap<u32, f64>,
    /// Pre-allocated scratch: `param_events` with `Mod` offsets folded into
    /// absolute `Value`s.
    folded_param_events: Vec<crate::plugin_instance::TimedParamEvent>,
    /// GUI → DSP parameter-edit bridge (shared with `Vst3ComponentHandler`).
    gui_param_edits: Arc<GuiParamEditQueue>,
    /// Pre-allocated scratch for `gui_param_edits.drain_latest`.
    gui_edit_scratch: Vec<(u32, f64)>,
    /// One-shot diagnostics, shared with the main half which logs them from
    /// `stop_processing()` (off the RT path).
    param_pool_overflowed: Arc<AtomicBool>,
    process_status_err: Arc<AtomicI32>,
    /// Render mode shared with the main half (`set_render_mode` writes,
    /// process() reads per buffer). `true` = offline.
    offline: Arc<AtomicBool>,
    /// Transport block the plugin reads via `ProcessData.processContext`.
    process_context: ProcessContext,
}

// SAFETY: the raw processor pointer is only used inside the AudioHalf
// exclusive-access windows; ComWrapper fields are host-owned Rust objects.
unsafe impl Send for Vst3AudioHalf {}

impl AudioProcessorHalf for Vst3AudioHalf {
    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        param_events: &[crate::plugin_instance::TimedParamEvent],
        input_audio: &[&[f32]],
        aux_inputs: &[crate::plugin_instance::AuxInputBuf<'_>],
        transport: &crate::plugin_instance::TransportContext,
    ) -> Result<i32> {
        anyhow::ensure!(self.processing, "VST3 plugin not processing");

        // --- Copy inputs / aux inputs and refresh channel pointers
        // (format-independent scaffold).
        let n = frames as usize;
        copy_input_planar(&mut self.input_buffers, input_audio, n);
        refresh_ptrs(&mut self.input_buffers, &mut self.input_ptrs);
        copy_aux_inputs_planar(&mut self.aux_input_buffers, aux_inputs, n);
        refresh_ptrs_ports(&mut self.aux_input_buffers, &mut self.aux_input_ptrs);
        refresh_ptrs(&mut self.output_buffers, &mut self.output_ptrs);
        refresh_ptrs_ports(&mut self.extra_output_buffers, &mut self.extra_output_ptrs);

        // --- Build Event buffer and hand it to the reusable input list.
        self.in_event_buffer.clear();
        for te in events {
            self.in_event_buffer.push(encode_event(te));
        }
        self.in_event_list.set_events(&self.in_event_buffer);
        self.collected_out_notes.clear();

        // --- Parameter changes: automation lane values + GUI edits.
        // VST3 has no modulation channel, so `ParamEventKind::Mod` offsets
        // are folded into absolute normalized values over the cached base
        // (scaffold pre-pass + fold — same helpers as CLAP's fold path).
        use crate::plugin_instance::ParamEventKind;

        // r.md #4: drain GUI-originated edits (controller `performEdit`,
        // queued by `Vst3ComponentHandler` on the UI thread) and feed them
        // to the processor at sample offset 0.
        self.gui_edit_scratch.clear();
        self.gui_param_edits.drain_latest(&mut self.gui_edit_scratch);

        for &(id, val) in &self.gui_edit_scratch {
            if let Some(slot) = self.param_mod_base.get_mut(&id) {
                *slot = val;
            }
        }
        process_scaffold::update_param_base_cache(&mut self.param_mod_base, param_events);

        self.folded_param_events.clear();
        // GUI edits first, at sample offset 0, so a non-automated knob turn
        // reaches the DSP; automation events follow in ascending time order
        // (VST3 `IParamValueQueue` requires non-decreasing offsets).
        for &(id, val) in &self.gui_edit_scratch {
            self.folded_param_events.push(crate::plugin_instance::TimedParamEvent {
                time: 0,
                param_id: id,
                value: val,
                kind: ParamEventKind::Value,
            });
        }
        for ev in param_events {
            match ev.kind {
                ParamEventKind::Value => self.folded_param_events.push(*ev),
                ParamEventKind::Mod => {
                    let base = self.param_mod_base.get(&ev.param_id).copied().unwrap_or(0.0);
                    // VST3 params are normalized 0..=1 ⇒ offset_scaled == offset.
                    let value = fold_mod_offset(base, ev.value, 0.0, 1.0);
                    self.folded_param_events.push(crate::plugin_instance::TimedParamEvent {
                        time: ev.time,
                        param_id: ev.param_id,
                        value,
                        kind: ParamEventKind::Value,
                    });
                }
            }
        }
        if self.in_param_changes.set_changes(&self.folded_param_events) {
            // RT path: raise the flag only; the log happens once in
            // `stop_processing()` on the main half.
            self.param_pool_overflowed.store(true, Ordering::Relaxed);
        }

        // `to_com_ptr` only bumps refcounts of host-owned objects (no heap
        // alloc); the ComPtrs' Drop at end-of-scope balances the addRef.
        let in_list_ptr = self
            .in_event_list
            .to_com_ptr::<IEventList>()
            .context("Vst3InEventList has no IEventList")?;
        let out_list_ptr = self
            .out_event_list
            .to_com_ptr::<IEventList>()
            .context("Vst3OutEventList has no IEventList")?;
        let in_param_changes_ptr = self
            .in_param_changes
            .to_com_ptr::<IParameterChanges>()
            .context("Vst3InParamChanges has no IParameterChanges")?;

        // --- Assemble AudioBusBuffers (main + aux inputs / main + extras).
        self.process_input_bufs.clear();
        if self.input_channels > 0 {
            self.process_input_bufs.push(AudioBusBuffers {
                numChannels: self.input_channels as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 {
                    channelBuffers32: self.input_ptrs.as_mut_ptr(),
                },
            });
        }
        for bus_idx in 0..self.aux_input_channels.len() {
            self.process_input_bufs.push(AudioBusBuffers {
                numChannels: self.aux_input_channels[bus_idx] as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 {
                    channelBuffers32: self.aux_input_ptrs[bus_idx].as_mut_ptr(),
                },
            });
        }
        self.process_output_bufs.clear();
        if self.output_channels > 0 {
            self.process_output_bufs.push(AudioBusBuffers {
                numChannels: self.output_channels as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 {
                    channelBuffers32: self.output_ptrs.as_mut_ptr(),
                },
            });
        }
        for bus_idx in 0..self.extra_output_channels.len() {
            self.process_output_bufs.push(AudioBusBuffers {
                numChannels: self.extra_output_channels[bus_idx] as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 {
                    channelBuffers32: self.extra_output_ptrs[bus_idx].as_mut_ptr(),
                },
            });
        }

        // --- Transport (shared TransportBlock — 非有限 sanitize 込み。旧
        // 実装は VST3 側だけ projectTimeMusic を無検査で渡していた)。
        let b = TransportBlock::derive(transport, self.sample_rate);
        self.process_context.sampleRate = self.sample_rate;
        self.process_context.tempo = b.bpm;
        self.process_context.timeSigNumerator = i32::from(b.tsig_num);
        self.process_context.timeSigDenominator = i32::from(b.tsig_denom);
        // `projectTimeSamples` derives from the authoritative song_pos_beats
        // (the engine leaves `steady_time` at 0), keeping it consistent with
        // `projectTimeMusic` and the ARA playback regions. Without this,
        // ARA plug-ins (Melodyne) see a frozen position 0 and render the
        // region's first frame forever.
        self.process_context.projectTimeSamples = b.song_pos_samples;
        self.process_context.continousTimeSamples = b.song_pos_samples;
        self.process_context.projectTimeMusic = b.song_pos_beats;
        self.process_context.barPositionMusic = b.bar_start_beats;
        self.process_context.cycleStartMusic = b.loop_start_beats;
        self.process_context.cycleEndMusic = b.loop_end_beats;
        let mut state = (StatesAndFlags_::kTempoValid
            | StatesAndFlags_::kTimeSigValid
            | StatesAndFlags_::kProjectTimeMusicValid
            | StatesAndFlags_::kBarPositionValid
            | StatesAndFlags_::kCycleValid
            | StatesAndFlags_::kContTimeValid) as u32;
        if b.is_playing {
            state |= StatesAndFlags_::kPlaying as u32;
        }
        if b.cycle_active {
            state |= StatesAndFlags_::kCycleActive as u32;
        }
        self.process_context.state = state;

        let num_inputs = self.process_input_bufs.len() as i32;
        let inputs_ptr = if self.process_input_bufs.is_empty() {
            std::ptr::null_mut()
        } else {
            self.process_input_bufs.as_mut_ptr()
        };
        let mut data = ProcessData {
            // export 中 (`set_render_mode(Offline)`) は per-buffer processMode
            // を kOffline に切替える (spec 準拠の代替 — `setIoMode` は
            // initialize 前限定)。
            processMode: if self.offline.load(Ordering::Relaxed) {
                ProcessModes_::kOffline
            } else {
                ProcessModes_::kRealtime
            },
            symbolicSampleSize: SymbolicSampleSizes_::kSample32,
            numSamples: frames as i32,
            numInputs: num_inputs,
            numOutputs: self.process_output_bufs.len() as i32,
            inputs: inputs_ptr,
            outputs: if self.process_output_bufs.is_empty() {
                std::ptr::null_mut()
            } else {
                self.process_output_bufs.as_mut_ptr()
            },
            inputParameterChanges: in_param_changes_ptr.as_ptr(),
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: in_list_ptr.as_ptr(),
            outputEvents: out_list_ptr.as_ptr(),
            processContext: &mut self.process_context,
        };
        // SAFETY: `audio_raw` is valid for the duration of this dispatch
        // window (AudioHalf contract — teardown quiesces first). ComRef is
        // borrowed: no AddRef/Release.
        let audio = unsafe { ComRef::<IAudioProcessor>::from_raw(self.audio_raw) }
            .context("IAudioProcessor pointer is null")?;
        let status = unsafe { audio.process(&mut data) };
        if status != kResultOk {
            // Record (don't log) on the RT thread; the warning fires once
            // (off-RT) from `stop_processing()`.
            self.process_status_err.store(status, Ordering::Relaxed);
        }

        // Drain collected events before the ComPtrs drop.
        self.out_event_list.drain_into(&mut self.collected_out_notes);

        Ok(status)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        self.output_buffers.get(channel).map(|v| v.as_slice())
    }

    /// パラアウト: **Port 0 is the MAIN output bus**; ports `1..` are the
    /// extra (non-main) buses. Single-output plugins report no ports.
    fn aux_output_buffer(&self, port: usize, channel: usize) -> Option<&[f32]> {
        if self.extra_output_channels.is_empty() {
            return None;
        }
        if port == 0 {
            self.output_buffers.get(channel).map(|v| v.as_slice())
        } else {
            self.extra_output_buffers
                .get(port - 1)
                .and_then(|bus| bus.get(channel))
                .map(|v| v.as_slice())
        }
    }

    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>) {
        out.append(&mut self.collected_out_notes);
    }

    fn on_activate(&mut self, sample_rate: f64, max_frames: u32) {
        let mf = max_frames as usize;
        self.sample_rate = sample_rate;
        (self.input_buffers, self.input_ptrs) = alloc_planar(self.input_channels as usize, mf);
        (self.output_buffers, self.output_ptrs) = alloc_planar(self.output_channels as usize, mf);
        (self.aux_input_buffers, self.aux_input_ptrs) =
            alloc_planar_ports(&self.aux_input_channels, mf);
        (self.extra_output_buffers, self.extra_output_ptrs) =
            alloc_planar_ports(&self.extra_output_channels, mf);
        self.process_input_bufs = Vec::with_capacity(1 + self.aux_input_channels.len());
        self.process_output_bufs = Vec::with_capacity(1 + self.extra_output_channels.len());
        // Prime the transport block so the very first `process()` already
        // sees `kPlaying` — some plugins (SynthMaster 3) refuse to output
        // anything when `processContext` is null or lacks `kPlaying`.
        self.process_context.state = (StatesAndFlags_::kPlaying
            | StatesAndFlags_::kTempoValid
            | StatesAndFlags_::kTimeSigValid) as u32;
        self.process_context.sampleRate = sample_rate;
        self.process_context.tempo = 120.0;
        self.process_context.timeSigNumerator = 4;
        self.process_context.timeSigDenominator = 4;
    }

    fn on_deactivate(&mut self) {
        self.output_buffers.clear();
        self.output_ptrs.clear();
        self.extra_output_buffers.clear();
        self.extra_output_ptrs.clear();
        self.process_output_bufs.clear();
    }

    fn set_processing(&mut self, on: bool) {
        self.processing = on;
    }
}

// ====================================================================
// Main half
// ====================================================================

/// A loaded VST3 plugin (main half: the IComponent + the associated
/// IEditController for GUI/state). Field drop order is **declaration
/// order**, so `_library` is declared LAST: every ComPtr's `Release()`
/// (which calls back into the DLL) must run before the `Library` Drop
/// unloads the DLL.
pub struct Vst3Plugin {
    id: String,
    name: String,
    path: PathBuf,

    // ComPtrs into the plugin. Drop runs `Release()` which calls into
    // the DLL — must precede `_library` drop below.
    component: ComPtr<IComponent>,
    audio: ComPtr<IAudioProcessor>,
    /// Either == component (single-component plugin) or a distinct object
    /// retrieved via factory.createInstance for the controller CID.
    controller: ComPtr<IEditController>,

    // Host-side classes given to the plugin. Held to keep them alive.
    _host_app: ComWrapper<Vst3HostApp>,
    _component_handler: ComWrapper<Vst3ComponentHandler>,

    /// Whether controller is a separate instance that needs `terminate` on
    /// drop. `false` for single-component plugins.
    controller_separate: bool,

    /// ARA session bound to this instance, if any.
    ara: Option<crate::ara::session::AraSession>,
    /// Last activate params (ARA setup の deactivate → reactivate 用)。
    last_activate: Option<(f64, u32, u32)>,

    active: bool,
    processing: bool,
    /// パラアウト port 数 (bus 構成から load 時に確定)。
    paraout_port_count: usize,

    /// Cross-half shared diagnostics / render mode (audio half stores,
    /// this half drains / sets).
    gui_param_edits: Arc<GuiParamEditQueue>,
    param_pool_overflowed: Arc<AtomicBool>,
    process_status_err: Arc<AtomicI32>,
    offline: Arc<AtomicBool>,

    // --- GUI state -----------------------------------------------------
    view: Option<ComPtr<IPlugView>>,
    /// Plug-frame used to relay resize requests back to the host.
    plug_frame: ComWrapper<Vst3PlugFrame>,
    gui_attached: std::cell::Cell<bool>,
    /// r.md #65: `view` の生ポインタを WNDPROC へ貸してよいか。
    /// `gui_create_embedded` で立て、**`gui_destroy` の先頭**で落とす。
    /// [`Vst3Sizer`] はこれを見て `removed()` 済みの view を二度と触らない
    /// (view の所有はこの struct 1 箇所 = SSoT のまま、AddRef を増やさない)。
    view_alive: Arc<AtomicBool>,
    /// r.md #65: attach 中のエディタコンテナ HWND (`0` = 未 attach)。
    /// `Vst3PlugFrame::resizeView` が同期で窓を直すために読む。
    editor_hwnd: Arc<std::sync::atomic::AtomicU64>,

    /// Audio half (shared with the worker registry via `audio_half()`).
    /// Its Drop never calls into the DLL (non-owning processor pointer),
    /// so a stale registry snapshot outliving this struct is benign.
    audio_half: Arc<AudioHalf>,

    /// DLL handle. Declared LAST so it drops LAST.
    _library: Library,
}

unsafe impl Send for Vst3Plugin {}

impl Vst3Plugin {
    pub fn load(path: &Path, target_id: &str, callbacks: HostCallbacks) -> Result<Self> {
        let dll_path = resolve_vst3_dll(path)
            .with_context(|| format!("resolving VST3 at {}", path.display()))?;
        let library = unsafe { Library::new(&dll_path) }
            .with_context(|| format!("LoadLibrary {}", dll_path.display()))?;

        // Call InitDll() if the module exports it (VST3 3.6.x requirement on
        // Windows). Absent = fine.
        unsafe {
            if let Ok(init_dll) = library.get::<Symbol<extern "system" fn() -> bool>>(b"InitDll\0")
                && !init_dll()
            {
                anyhow::bail!("InitDll returned false for {}", dll_path.display());
            }
        }

        // Resolve the factory entry.
        let factory_raw: *mut IPluginFactory = unsafe {
            let sym: Symbol<extern "system" fn() -> *mut IPluginFactory> = library
                .get(b"GetPluginFactory\0")
                .context("missing GetPluginFactory export")?;
            sym()
        };
        anyhow::ensure!(!factory_raw.is_null(), "GetPluginFactory returned null");
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(factory_raw) }
            .context("factory came back null via from_raw")?;

        // Scan class infos for Audio Module Class entries. `target_id` is
        // matched as a 32-hex-digit UUID against the class CID; anything
        // else means "first Audio Module Class wins".
        let count = unsafe { factory.countClasses() };
        tracing::info!(path = %dll_path.display(), count, "VST3 classes in factory");
        let is_uuid =
            target_id.len() == 32 && target_id.chars().all(|c| c.is_ascii_hexdigit());
        let mut selected: Option<(i32, PClassInfo)> = None;
        for i in 0..count {
            let mut info = std::mem::MaybeUninit::<PClassInfo>::zeroed();
            let res = unsafe { factory.getClassInfo(i, info.as_mut_ptr()) };
            if res != kResultOk {
                continue;
            }
            let info = unsafe { info.assume_init() };
            let category = c_array_to_string(&info.category);
            let name = c_array_to_string(&info.name);
            let cid_hex = tuid_to_hex(&info.cid);
            tracing::info!(index = i, %name, %category, %cid_hex, "VST3 class");
            if category != "Audio Module Class" {
                continue;
            }
            let matches_target = !is_uuid || cid_hex.eq_ignore_ascii_case(target_id);
            if matches_target && selected.is_none() {
                selected = Some((i, info));
            }
        }
        let Some((_index, class_info)) = selected else {
            anyhow::bail!(
                "no matching Audio Module Class in {} (target_id = '{}')",
                dll_path.display(),
                target_id
            );
        };
        let class_name = c_array_to_string(&class_info.name);
        let class_cid = class_info.cid;
        let class_cid_hex = tuid_to_hex(&class_cid);
        // Keep the caller-provided id when present so it round-trips through
        // project save / plugin_db lookup.
        let display_id = if target_id.is_empty() {
            class_cid_hex.clone()
        } else {
            target_id.to_string()
        };

        // Create the component.
        let component = create_instance::<IComponent>(&factory, &class_cid)
            .context("factory.createInstance for IComponent failed")?;

        // Build the host objects.
        let host_app = ComWrapper::new(Vst3HostApp::new());
        // r.md #4: the GUI→DSP parameter-edit bridge.
        let gui_param_edits = Arc::new(GuiParamEditQueue::new());
        let component_handler = ComWrapper::new(Vst3ComponentHandler::new(
            callbacks.clone(),
            gui_param_edits.clone(),
        ));
        let plug_frame = ComWrapper::new(Vst3PlugFrame::new(callbacks.clone()));

        // initialize(component, IHostApplication*)
        let host_app_ptr: *mut FUnknown = host_app
            .to_com_ptr::<FUnknown>()
            .context("host_app has no FUnknown")?
            .into_raw();
        let init_res = unsafe { component.initialize(host_app_ptr) };
        let _ = unsafe { ComPtr::<FUnknown>::from_raw(host_app_ptr) };
        anyhow::ensure!(
            init_res == kResultOk,
            "IComponent::initialize returned {:#x}",
            init_res
        );

        // Cast to IAudioProcessor.
        let audio = component
            .cast::<IAudioProcessor>()
            .context("component does not implement IAudioProcessor")?;

        // Resolve controller (same instance or a separate one).
        let mut ctrl_cid: TUID = [0; 16];
        let ctrl_res = unsafe { component.getControllerClassId(&mut ctrl_cid as *mut TUID) };
        let (controller, controller_separate) = if ctrl_res == kResultOk && ctrl_cid != class_cid {
            let c = create_instance::<IEditController>(&factory, &ctrl_cid)
                .context("failed to create IEditController")?;
            let host_app_ptr2: *mut FUnknown = host_app
                .to_com_ptr::<FUnknown>()
                .context("host_app has no FUnknown (2)")?
                .into_raw();
            let init = unsafe { c.initialize(host_app_ptr2) };
            let _ = unsafe { ComPtr::<FUnknown>::from_raw(host_app_ptr2) };
            anyhow::ensure!(init == kResultOk, "IEditController::initialize -> {:#x}", init);
            (c, true)
        } else if let Some(c) = component.cast::<IEditController>() {
            (c, false)
        } else {
            anyhow::bail!("could not obtain IEditController from component");
        };

        // Wire up the component handler.
        let handler_ptr = component_handler
            .to_com_ptr::<vst3::Steinberg::Vst::IComponentHandler>()
            .context("component_handler has no IComponentHandler")?
            .into_raw();
        let set_res = unsafe { controller.setComponentHandler(handler_ptr) };
        let _ = unsafe {
            ComPtr::<vst3::Steinberg::Vst::IComponentHandler>::from_raw(handler_ptr)
        };
        if set_res != kResultOk {
            tracing::warn!(res = format!("{set_res:#x}"), "setComponentHandler non-OK (continuing)");
        }

        // For plugins with a separate IEditController, tie component and
        // controller together (connection points + state priming).
        if controller_separate {
            connect_component_and_controller(&component, &controller);
            transfer_component_state(&component, &controller);
        }

        log_all_buses(&component);

        let input_channels = main_audio_bus_channel_count(&component, BusDirections_::kInput);
        let output_channels = main_audio_bus_channel_count(&component, BusDirections_::kOutput);
        let aux_input_channels = aux_input_bus_channels(&component);
        let extra_output_channels = output_bus_extra_channels(&component);
        tracing::info!(
            input_channels,
            output_channels,
            aux_input_count = aux_input_channels.len(),
            extra_output_bus_count = extra_output_channels.len(),
            "VST3 audio bus channel counts"
        );

        // Seed the modulation base cache with every param's default
        // (normalized) so the audio-thread fold never allocates.
        let infos = enumerate_vst3_params(&controller);
        let mut param_mod_base = std::collections::HashMap::with_capacity(infos.len());
        for info in &infos {
            param_mod_base.insert(info.id, info.default_value.clamp(0.0, 1.0));
        }

        // パラアウト port 数 = 1 (main) + extra bus 数 (multi-out のみ)。
        let paraout_port_count = if extra_output_channels.is_empty() {
            0
        } else {
            (1 + extra_output_channels.len()).min(common::process_data::MAX_AUX_OUT)
        };

        let param_pool_overflowed = Arc::new(AtomicBool::new(false));
        let process_status_err = Arc::new(AtomicI32::new(kResultOk));
        let offline = Arc::new(AtomicBool::new(false));

        let audio_half = AudioHalf::new(Box::new(Vst3AudioHalf {
            audio_raw: audio.as_ptr(),
            processing: false,
            sample_rate: 0.0,
            input_channels,
            output_channels,
            aux_input_channels,
            extra_output_channels,
            input_buffers: Vec::new(),
            input_ptrs: Vec::new(),
            output_buffers: Vec::new(),
            output_ptrs: Vec::new(),
            aux_input_buffers: Vec::new(),
            aux_input_ptrs: Vec::new(),
            extra_output_buffers: Vec::new(),
            extra_output_ptrs: Vec::new(),
            process_input_bufs: Vec::new(),
            process_output_bufs: Vec::new(),
            in_event_buffer: Vec::with_capacity(256),
            collected_out_notes: Vec::with_capacity(256),
            in_event_list: ComWrapper::new(Vst3InEventList::new()),
            out_event_list: ComWrapper::new(Vst3OutEventList::new()),
            in_param_changes: ComWrapper::new(Vst3InParamChanges::new()),
            param_mod_base,
            // MAX_EVENTS (256) automation/mod events + up to 64 folded-in
            // GUI edits → never reallocs on the RT path.
            folded_param_events: Vec::with_capacity(256 + 64),
            gui_param_edits: Arc::clone(&gui_param_edits),
            gui_edit_scratch: Vec::with_capacity(64),
            param_pool_overflowed: Arc::clone(&param_pool_overflowed),
            process_status_err: Arc::clone(&process_status_err),
            offline: Arc::clone(&offline),
            process_context: unsafe { std::mem::zeroed() },
        }));

        Ok(Self {
            id: display_id,
            name: class_name,
            path: path.to_path_buf(),
            component,
            audio,
            controller,
            _host_app: host_app,
            _component_handler: component_handler,
            controller_separate,
            ara: None,
            last_activate: None,
            active: false,
            processing: false,
            paraout_port_count,
            gui_param_edits,
            param_pool_overflowed,
            process_status_err,
            offline,
            view: None,
            plug_frame,
            gui_attached: std::cell::Cell::new(false),
            view_alive: Arc::new(AtomicBool::new(false)),
            editor_hwnd: Arc::clone(&callbacks.editor_hwnd),
            audio_half,
            _library: library,
        })
    }

    /// Exclusive access to the audio half.
    ///
    /// # Safety
    /// Caller (plugin-main thread) must be inside a quiesced window
    /// (`AudioHalf::get` contract).
    #[allow(clippy::mut_from_ref)] // UnsafeCell 経由。契約は Safety 節。
    unsafe fn audio_half_mut(&self) -> &mut (dyn AudioProcessorHalf + 'static) {
        unsafe { self.audio_half.get() }
    }
}

/// `String128` ([TChar; 128] = u16 array) を Rust String に。
fn utf16_buf_to_string<const N: usize>(buf: &[u16; N]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(N);
    String::from_utf16_lossy(&buf[..end])
}

/// Audio + Event bus を全 enumerate して 1 件ずつ INFO ログ。
fn log_all_buses(component: &ComPtr<IComponent>) {
    for (media_label, media) in [
        ("Audio", MediaTypes_::kAudio),
        ("Event", MediaTypes_::kEvent),
    ] {
        for (dir_label, dir) in [
            ("In", BusDirections_::kInput),
            ("Out", BusDirections_::kOutput),
        ] {
            let count = unsafe { component.getBusCount(media, dir) };
            for i in 0..count {
                let mut info: BusInfo = unsafe { std::mem::zeroed() };
                let res = unsafe { component.getBusInfo(media, dir, i, &mut info) };
                if res != kResultOk {
                    tracing::warn!(
                        media = media_label,
                        dir = dir_label,
                        index = i,
                        res = format!("{res:#x}"),
                        "VST3 getBusInfo failed"
                    );
                    continue;
                }
                tracing::info!(
                    media = media_label,
                    dir = dir_label,
                    index = i,
                    channels = info.channelCount,
                    bus_type = info.busType,
                    flags = format!("{:#x}", info.flags),
                    name = %utf16_buf_to_string(&info.name),
                    "VST3 bus"
                );
            }
        }
    }
}

/// 指定 audio bus の channel count から SpeakerArrangement を導出。
fn arrangement_for_bus(
    component: &ComPtr<IComponent>,
    dir: BusDirection,
    index: i32,
    fallback: SpeakerArrangement,
) -> SpeakerArrangement {
    let mut info: BusInfo = unsafe { std::mem::zeroed() };
    let res = unsafe {
        component.getBusInfo(MediaTypes_::kAudio, dir, index, &mut info)
    };
    if res != kResultOk {
        return fallback;
    }
    // C4 (r.md #8): channel 数に応じた正確な SpeakerArrangement。標準
    // surround config をマップ、非標準 (5/7/9+) のみ fallback。
    match info.channelCount {
        0 => 0,
        1 => SpeakerArr::kMono,
        2 => SpeakerArr::kStereo,
        3 => SpeakerArr::k30Cine, // L R C
        4 => SpeakerArr::k40Music, // quad: L R Ls Rs
        6 => SpeakerArr::k51, // 5.1: L R C Lfe Ls Rs
        8 => SpeakerArr::k71Music, // 7.1 (side surround)
        _ => fallback,
    }
}

/// PR4 sidechain: enumerate `kAux` input buses' channel counts.
fn aux_input_bus_channels(component: &ComPtr<IComponent>) -> Vec<u32> {
    use vst3::Steinberg::Vst::{BusDirections_, BusInfo, BusTypes_, MediaTypes_};
    let count = unsafe { component.getBusCount(MediaTypes_::kAudio, BusDirections_::kInput) };
    let mut aux: Vec<u32> = Vec::new();
    for i in 0..count {
        let mut info: BusInfo = unsafe { std::mem::zeroed() };
        let res = unsafe {
            component.getBusInfo(MediaTypes_::kAudio, BusDirections_::kInput, i, &mut info)
        };
        if res != vst3::Steinberg::kResultOk {
            continue;
        }
        if info.busType == BusTypes_::kAux {
            if aux.len() >= common::process_data::MAX_AUX_IN {
                tracing::warn!(
                    bus_index = i,
                    cap = common::process_data::MAX_AUX_IN,
                    "VST3 plugin declared more aux input buses than the host caps to"
                );
                break;
            }
            let ch = (info.channelCount.max(0) as u32)
                .min(common::process_data::MAX_CHANNELS as u32);
            aux.push(ch);
        }
    }
    aux
}

/// `busType == Main` の最初の audio bus の channel count を返す。
fn main_audio_bus_channel_count(
    component: &ComPtr<IComponent>,
    dir: BusDirection,
) -> u32 {
    let count = unsafe { component.getBusCount(MediaTypes_::kAudio, dir) };
    if count == 0 {
        return 0;
    }
    // Pass 1: BusType::kMain (= 0) を探す。
    for i in 0..count {
        let mut info: BusInfo = unsafe { std::mem::zeroed() };
        let res = unsafe {
            component.getBusInfo(MediaTypes_::kAudio, dir, i, &mut info)
        };
        if res == kResultOk && info.busType == 0 {
            return info.channelCount.max(0) as u32;
        }
    }
    // Pass 2: 何も Main が無ければ index 0 をそのまま採用。
    let mut info: BusInfo = unsafe { std::mem::zeroed() };
    let res = unsafe {
        component.getBusInfo(MediaTypes_::kAudio, dir, 0, &mut info)
    };
    if res == kResultOk {
        info.channelCount.max(0) as u32
    } else {
        0
    }
}

/// Channel counts of every output bus *after* index 0.
fn output_bus_extra_channels(component: &ComPtr<IComponent>) -> Vec<u32> {
    use vst3::Steinberg::Vst::{BusDirections_, BusInfo, MediaTypes_};
    let count = unsafe { component.getBusCount(MediaTypes_::kAudio, BusDirections_::kOutput) };
    let mut extra: Vec<u32> = Vec::new();
    for i in 1..count {
        let mut info: BusInfo = unsafe { std::mem::zeroed() };
        let res = unsafe {
            component.getBusInfo(MediaTypes_::kAudio, BusDirections_::kOutput, i, &mut info)
        };
        extra.push(if res == kResultOk { info.channelCount.max(0) as u32 } else { 0 });
    }
    extra
}

/// Connect the component's and controller's `IConnectionPoint` interfaces
/// in both directions (best-effort).
fn connect_component_and_controller(
    component: &ComPtr<IComponent>,
    controller: &ComPtr<IEditController>,
) {
    let Some(comp_cp) = component.cast::<IConnectionPoint>() else {
        tracing::debug!("component does not implement IConnectionPoint; skipping connect");
        return;
    };
    let Some(ctrl_cp) = controller.cast::<IConnectionPoint>() else {
        tracing::debug!("controller does not implement IConnectionPoint; skipping connect");
        return;
    };
    let r1 = unsafe { comp_cp.connect(ctrl_cp.as_ptr()) };
    let r2 = unsafe { ctrl_cp.connect(comp_cp.as_ptr()) };
    if r1 != kResultOk || r2 != kResultOk {
        tracing::warn!(
            comp_to_ctrl = format!("{r1:#x}"),
            ctrl_to_comp = format!("{r2:#x}"),
            "IConnectionPoint::connect non-OK (continuing)"
        );
    } else {
        tracing::debug!("VST3 component <-> controller connection points wired");
    }
}

/// Stream the component's serialized state into the controller via
/// `IEditController::setComponentState` (best-effort).
fn transfer_component_state(
    component: &ComPtr<IComponent>,
    controller: &ComPtr<IEditController>,
) {
    let write = ComWrapper::new(Vst3WriteStream::new());
    let Some(write_ibstream) = write.to_com_ptr::<IBStream>() else {
        tracing::warn!("Vst3WriteStream has no IBStream; cannot transfer component state");
        return;
    };
    let write_raw = write_ibstream.into_raw();
    let get_res = unsafe { component.getState(write_raw) };
    let _ = unsafe { ComPtr::<IBStream>::from_raw(write_raw) };
    if get_res != kResultOk {
        tracing::debug!(
            res = format!("{get_res:#x}"),
            "IComponent::getState non-OK; skipping setComponentState"
        );
        return;
    }
    let bytes = write.take_buffer();
    let read = ComWrapper::new(Vst3ReadStream::new(&bytes));
    let Some(read_ibstream) = read.to_com_ptr::<IBStream>() else {
        tracing::warn!("Vst3ReadStream has no IBStream; cannot transfer component state");
        return;
    };
    let read_raw = read_ibstream.into_raw();
    let set_res = unsafe { controller.setComponentState(read_raw) };
    let _ = unsafe { ComPtr::<IBStream>::from_raw(read_raw) };
    if set_res != kResultOk && set_res != kNotImplemented {
        tracing::warn!(
            res = format!("{set_res:#x}"),
            "IEditController::setComponentState non-OK (continuing)"
        );
    } else {
        tracing::debug!(
            bytes = bytes.len(),
            "VST3 controller primed with component state"
        );
    }
}

fn create_instance<I: Interface>(
    factory: &ComPtr<IPluginFactory>,
    cid: &TUID,
) -> Option<ComPtr<I>> {
    let mut obj: *mut c_void = std::ptr::null_mut();
    let iid_guid = I::IID;
    let res = unsafe {
        factory.createInstance(
            cid.as_ptr() as *const _,
            iid_guid.as_ptr() as *const _,
            &mut obj,
        )
    };
    if res != kResultOk || obj.is_null() {
        return None;
    }
    unsafe { ComPtr::<I>::from_raw(obj as *mut I) }
}

/// VST3 param 一覧を `IEditController` から列挙 (plugin-main thread)。
/// VST3 の param は仕様上常に normalized [0,1] なので min/max は 0/1 固定。
fn enumerate_vst3_params(
    controller: &ComPtr<IEditController>,
) -> Vec<common::protocol::PluginParamInfo> {
    let count = unsafe { controller.getParameterCount() };
    if count <= 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        // SAFETY: ParameterInfo is plain data; zeroed start is legal.
        let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
        let res = unsafe { controller.getParameterInfo(i, &raw mut info) };
        if res != kResultOk {
            tracing::warn!(index = i, "IEditController::getParameterInfo non-OK");
            continue;
        }
        let name = utf16_buf_to_string(&info.title);
        let mut flags: u32 = 0;
        if info.flags & ParameterFlags_::kCanAutomate != 0 {
            flags |= common::protocol::plugin_param_flags::AUTOMATABLE;
        }
        if info.flags & ParameterFlags_::kIsReadOnly != 0 {
            flags |= common::protocol::plugin_param_flags::READONLY;
        }
        if info.flags & ParameterFlags_::kIsHidden != 0 {
            flags |= common::protocol::plugin_param_flags::HIDDEN;
        }
        // VST3 の離散 param は stepCount > 0 (kIsList は enum 型)。
        if info.stepCount > 0 || info.flags & ParameterFlags_::kIsList != 0 {
            flags |= common::protocol::plugin_param_flags::STEPPED;
        }
        if info.flags & ParameterFlags_::kIsWrapAround != 0 {
            flags |= common::protocol::plugin_param_flags::PERIODIC;
        }
        out.push(common::protocol::PluginParamInfo {
            id: info.id,
            name,
            // VST3 の module/grouping は unitId + IUnitInfo 階層。flat 表示
            // なので module は空。
            module: String::new(),
            min_value: 0.0,
            max_value: 1.0,
            default_value: info.defaultNormalizedValue,
            flags,
        });
    }
    out
}

/// VST3 のクラスを一時 instantiate して bus 構成から port 構成を読む
/// (`--probe-vst3` one-shot モード)。
pub fn probe_ports(path: &Path, target_id: &str) -> Result<common::port_config::PortConfig> {
    let dll_path = resolve_vst3_dll(path)
        .with_context(|| format!("resolving VST3 at {}", path.display()))?;
    let library = unsafe { Library::new(&dll_path) }
        .with_context(|| format!("LoadLibrary {}", dll_path.display()))?;
    unsafe {
        if let Ok(init_dll) = library.get::<Symbol<extern "system" fn() -> bool>>(b"InitDll\0")
            && !init_dll()
        {
            anyhow::bail!("InitDll returned false");
        }
    }
    let factory_raw: *mut IPluginFactory = unsafe {
        let sym: Symbol<extern "system" fn() -> *mut IPluginFactory> = library
            .get(b"GetPluginFactory\0")
            .context("missing GetPluginFactory export")?;
        sym()
    };
    anyhow::ensure!(!factory_raw.is_null(), "GetPluginFactory returned null");
    let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(factory_raw) }
        .context("factory came back null")?;

    let count = unsafe { factory.countClasses() };
    let is_uuid =
        target_id.len() == 32 && target_id.chars().all(|c| c.is_ascii_hexdigit());
    let mut class_cid: Option<TUID> = None;
    for i in 0..count {
        let mut info = std::mem::MaybeUninit::<PClassInfo>::zeroed();
        if unsafe { factory.getClassInfo(i, info.as_mut_ptr()) } != kResultOk {
            continue;
        }
        let info = unsafe { info.assume_init() };
        if c_array_to_string(&info.category) != "Audio Module Class" {
            continue;
        }
        let cid_hex = tuid_to_hex(&info.cid);
        if (!is_uuid || cid_hex.eq_ignore_ascii_case(target_id)) && class_cid.is_none() {
            class_cid = Some(info.cid);
        }
    }
    let class_cid = class_cid.context("no matching Audio Module Class")?;

    let component = create_instance::<IComponent>(&factory, &class_cid)
        .context("createInstance(IComponent) failed")?;
    let host_app = ComWrapper::new(Vst3HostApp::new());
    let host_app_ptr: *mut FUnknown = host_app
        .to_com_ptr::<FUnknown>()
        .context("host_app has no FUnknown")?
        .into_raw();
    let init_res = unsafe { component.initialize(host_app_ptr) };
    let _ = unsafe { ComPtr::<FUnknown>::from_raw(host_app_ptr) };
    anyhow::ensure!(init_res == kResultOk, "initialize returned {:#x}", init_res);

    let ev_in = unsafe { component.getBusCount(MediaTypes_::kEvent, BusDirections_::kInput) };
    let ev_out = unsafe { component.getBusCount(MediaTypes_::kEvent, BusDirections_::kOutput) };
    let au_in = unsafe { component.getBusCount(MediaTypes_::kAudio, BusDirections_::kInput) };
    let au_out = unsafe { component.getBusCount(MediaTypes_::kAudio, BusDirections_::kOutput) };
    let _ = unsafe { component.terminate() };

    Ok(common::port_config::PortConfig {
        has_note_input: ev_in > 0,
        has_note_output: ev_out > 0,
        has_audio_output: au_out > 0,
        has_audio_input: au_in > 0,
        // VST3 は映像 port を持たない。
        has_video_input: false,
        has_video_output: false,
    })
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        // Tear down the ARA session before releasing the component — its
        // drop issues destroy calls back into this plug-in. Deactivate first
        // so detaching its playback regions is valid.
        if self.ara.is_some() {
            if self.active {
                LoadedPlugin::deactivate(self);
            }
            self.ara = None;
        }
        // r.md #65: **`gui_destroy` を通す**。手書きで `removed()` だけ呼ぶと
        // (a) `view_alive` / `editor_hwnd` が落ちず、`InstanceRecord` の drop 順
        // (plugin → editor) の隙間で `Vst3Sizer` が「alive なのに dangling」な
        // `*mut IPlugView` を指し、(b) `setFrame(nullptr)` → `removed()` の順序も
        // 踏めない。CLAP 側 (`ClapPlugin::drop`) と同じく契約を 1 箇所に集約する。
        LoadedPlugin::gui_destroy(self);
        if self.processing {
            unsafe {
                self.audio.setProcessing(0);
            }
        }
        if self.active {
            unsafe {
                self.component.setActive(0);
            }
        }
        if self.controller_separate {
            unsafe {
                let _ = self.controller.terminate();
            }
        }
        unsafe {
            let _ = self.component.terminate();
        }
        tracing::info!(name = %self.name, path = %self.path.display(), "VST3 plugin destroyed");
    }
}

impl crate::ara::AraLifecycleHost for Vst3Plugin {
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

impl LoadedPlugin for Vst3Plugin {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn format(&self) -> PluginFormat {
        PluginFormat::Vst3
    }

    fn audio_half(&self) -> Arc<AudioHalf> {
        Arc::clone(&self.audio_half)
    }

    fn bind_ara_if_capable(&mut self) -> Result<bool> {
        let factory =
            match unsafe { crate::ara::vst3_ara::ara_factory_from_component(&self.component) } {
                Some(factory) => factory,
                None => return Ok(false),
            };
        let component = self.component.clone();
        let session = unsafe {
            crate::ara::session::AraSession::create(factory, |document_controller| {
                crate::ara::vst3_ara::bind(
                    &component,
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

    fn enumerate_params(&self) -> Vec<common::protocol::PluginParamInfo> {
        enumerate_vst3_params(&self.controller)
    }

    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()> {
        anyhow::ensure!(!self.active, "VST3 plugin already active");
        // remember params so ARA setup can deactivate → reactivate.
        self.last_activate = Some((sample_rate, min_frames, max_frames));

        // 1. Negotiate speaker arrangements for every audio bus (per spec).
        let stereo: SpeakerArrangement = SpeakerArr::kStereo;
        let in_arr_count = unsafe {
            self.component.getBusCount(MediaTypes_::kAudio, BusDirections_::kInput)
        };
        let out_arr_count = unsafe {
            self.component.getBusCount(MediaTypes_::kAudio, BusDirections_::kOutput)
        };
        let mut in_arr: Vec<SpeakerArrangement> = (0..in_arr_count)
            .map(|i| arrangement_for_bus(&self.component, BusDirections_::kInput, i, stereo))
            .collect();
        let mut out_arr: Vec<SpeakerArrangement> = (0..out_arr_count)
            .map(|i| arrangement_for_bus(&self.component, BusDirections_::kOutput, i, stereo))
            .collect();
        tracing::info!(
            in_count = in_arr_count,
            out_count = out_arr_count,
            in_arr = ?in_arr,
            out_arr = ?out_arr,
            "VST3 setBusArrangements request"
        );
        let sba = unsafe {
            self.audio.setBusArrangements(
                in_arr.as_mut_ptr(),
                in_arr.len() as i32,
                out_arr.as_mut_ptr(),
                out_arr.len() as i32,
            )
        };
        if sba != kResultOk && sba != kResultTrue {
            tracing::warn!(res = format!("{sba:#x}"), "setBusArrangements non-OK (continuing)");
        }
        for i in 0..in_arr_count {
            let mut got: SpeakerArrangement = 0;
            let res = unsafe {
                self.audio.getBusArrangement(BusDirections_::kInput, i, &mut got)
            };
            tracing::info!(
                dir = "In", index = i, res = format!("{res:#x}"),
                arrangement = format!("{got:#x}"),
                "VST3 negotiated arrangement"
            );
        }
        for i in 0..out_arr_count {
            let mut got: SpeakerArrangement = 0;
            let res = unsafe {
                self.audio.getBusArrangement(BusDirections_::kOutput, i, &mut got)
            };
            tracing::info!(
                dir = "Out", index = i, res = format!("{res:#x}"),
                arrangement = format!("{got:#x}"),
                "VST3 negotiated arrangement"
            );
        }

        // 2. setupProcessing
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32,
            maxSamplesPerBlock: max_frames as i32,
            sampleRate: sample_rate,
        };
        let res = unsafe { self.audio.setupProcessing(&mut setup) };
        anyhow::ensure!(
            res == kResultOk,
            "IAudioProcessor::setupProcessing -> {:#x}",
            res
        );

        // 3. Activate every audio + event bus.
        for dir in [BusDirections_::kInput, BusDirections_::kOutput] {
            for media in [MediaTypes_::kAudio, MediaTypes_::kEvent] {
                let n = unsafe { self.component.getBusCount(media, dir) };
                for i in 0..n {
                    unsafe {
                        self.component.activateBus(media, dir, i, 1);
                    }
                }
            }
        }

        // 4. setActive(true)
        let res = unsafe { self.component.setActive(1) };
        anyhow::ensure!(
            res == kResultOk,
            "IComponent::setActive(1) -> {:#x}",
            res
        );

        // 5. Allocate the audio half's process buffers + prime its
        // transport block.
        // SAFETY: quiesced window (install / reinit / ARA setup call sites).
        unsafe { self.audio_half_mut().on_activate(sample_rate, max_frames) };
        self.active = true;
        tracing::info!(name = %self.name, sample_rate, max_frames, "VST3 plugin activated");
        Ok(())
    }

    fn deactivate(&mut self) {
        if !self.active {
            return;
        }
        unsafe {
            self.component.setActive(0);
        }
        self.active = false;
        // SAFETY: quiesced window.
        unsafe { self.audio_half_mut().on_deactivate() };
    }

    fn start_processing(&mut self) -> Result<()> {
        anyhow::ensure!(self.active, "VST3 plugin not active");
        anyhow::ensure!(!self.processing, "VST3 plugin already processing");
        let res = unsafe { self.audio.setProcessing(1) };
        // Some plugins (SynthMaster 3) return `kNotImplemented`; those are
        // always ready to process.
        anyhow::ensure!(
            res == kResultOk || res == kNotImplemented,
            "IAudioProcessor::setProcessing(1) -> {:#x}",
            res
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
        unsafe {
            self.audio.setProcessing(0);
        }
        self.processing = false;
        // SAFETY: quiesced window.
        unsafe { self.audio_half_mut().set_processing(false) };
        // Drain the RT-set one-shot diagnostics here (off the hot path).
        if self.param_pool_overflowed.swap(false, Ordering::Relaxed) {
            tracing::warn!(
                plugin = %self.name,
                "VST3 param changes pool overflow (>64 distinct params/buffer); extra dropped"
            );
        }
        // r.md #4: GUI→DSP edit ring overflowed. Benign — trailing edits of
        // a drag re-deliver the final value — but worth a one-shot note.
        if self.gui_param_edits.take_overflowed() {
            tracing::warn!(
                plugin = %self.name,
                "VST3 GUI parameter-edit ring overflow; some intermediate edits dropped"
            );
        }
        {
            let status = self.process_status_err.swap(kResultOk, Ordering::Relaxed);
            if status != kResultOk {
                tracing::warn!(
                    plugin = %self.name,
                    status = format!("{status:#x}"),
                    "VST3 process returned non-OK"
                );
            }
        }
    }

    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        // VST3 spec の `IComponent::setIoMode` は `initialize` 前限定なので、
        // spec 準拠の代替として per-buffer `ProcessData::processMode` を
        // 切替える (audio half が shared atomic を毎 buffer 読む)。
        self.offline
            .store(mode == RenderMode::Offline, Ordering::Relaxed);
        tracing::info!(name = %self.name, ?mode, "VST3 render mode updated");
        true
    }

    fn query_latency(&mut self) -> u32 {
        // VST3 spec (`IAudioProcessor::getLatencySamples`): call after
        // `setupProcessing` completed (Setup Done). We invoke right after
        // our `activate()`, so we're safely past the barrier.
        unsafe { self.audio.getLatencySamples() }
    }

    fn state_save(&self) -> Result<Option<Vec<u8>>> {
        let write = ComWrapper::new(Vst3WriteStream::new());
        let stream_ptr = write
            .to_com_ptr::<IBStream>()
            .context("Vst3WriteStream has no IBStream")?;
        let res = unsafe { self.component.getState(stream_ptr.as_ptr()) };
        if res != kResultOk {
            tracing::warn!(res = format!("{res:#x}"), "IComponent::getState non-OK");
            return Ok(None);
        }
        Ok(Some(write.take_buffer()))
    }

    fn state_load(&mut self, data: &[u8]) -> Result<()> {
        let read = ComWrapper::new(Vst3ReadStream::new(data));
        let stream_ptr = read
            .to_com_ptr::<IBStream>()
            .context("Vst3ReadStream has no IBStream")?;
        let res = unsafe { self.component.setState(stream_ptr.as_ptr()) };
        anyhow::ensure!(
            res == kResultOk,
            "IComponent::setState -> {:#x}",
            res
        );
        Ok(())
    }

    fn aux_output_port_count(&self) -> usize {
        self.paraout_port_count
    }

    fn gui_is_embed_supported(&self) -> bool {
        // Create view to probe platform support, then release.
        let view_raw = unsafe { self.controller.createView(c"editor".as_ptr()) };
        if view_raw.is_null() {
            tracing::warn!(
                plugin = %self.name,
                "VST3 createView returned null — plugin reports no editor"
            );
            return false;
        }
        let Some(view) = (unsafe { ComPtr::<IPlugView>::from_raw(view_raw) }) else {
            tracing::warn!(
                plugin = %self.name,
                "VST3 createView returned a non-null pointer that ComPtr rejected"
            );
            return false;
        };
        let res = unsafe { view.isPlatformTypeSupported(kPlatformTypeHWND) };
        let supported = res == kResultTrue || res == kResultOk;
        if !supported {
            tracing::warn!(
                plugin = %self.name,
                "isPlatformTypeSupported(HWND) returned {:#x}",
                res
            );
        }
        supported
    }

    fn gui_create_embedded(&mut self) -> Result<()> {
        if self.view.is_some() {
            return Ok(());
        }
        let view_raw = unsafe { self.controller.createView(c"editor".as_ptr()) };
        if view_raw.is_null() {
            tracing::warn!(
                plugin = %self.name,
                "VST3 createView returned null — plugin has no editor"
            );
            anyhow::bail!("IEditController::createView returned null");
        }
        let Some(view) = (unsafe { ComPtr::<IPlugView>::from_raw(view_raw) }) else {
            anyhow::bail!("createView returned null after from_raw");
        };
        let supported = unsafe { view.isPlatformTypeSupported(kPlatformTypeHWND) };
        tracing::info!(
            plugin = %self.name,
            supported = format!("{supported:#x}"),
            expected_ok = format!("{kResultOk:#x}"),
            "VST3 isPlatformTypeSupported(HWND)"
        );
        // Attach the plug-frame so resize requests get routed back.
        let frame_ptr = self
            .plug_frame
            .to_com_ptr::<vst3::Steinberg::IPlugFrame>()
            .context("plug_frame has no IPlugFrame")?
            .into_raw();
        let set_frame_res = unsafe { view.setFrame(frame_ptr) };
        tracing::info!(
            plugin = %self.name,
            res = format!("{set_frame_res:#x}"),
            "VST3 setFrame"
        );
        let _ = unsafe { ComPtr::<vst3::Steinberg::IPlugFrame>::from_raw(frame_ptr) };
        self.view = Some(view);
        // r.md #65: ここから `gui_destroy` までの間だけ、WNDPROC は view の
        // 生ポインタを借りてよい。
        self.view_alive.store(true, Ordering::Release);
        // The editor view now exists — push the current ARA selection so the
        // plug-in's editor displays the track's regions.
        if let Some(session) = self.ara.as_ref() {
            session.notify_editor_selection();
        }
        Ok(())
    }

    fn gui_get_size(&self) -> Option<(u32, u32)> {
        let view = self.view.as_ref()?;
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let res = unsafe { view.getSize(&mut rect) };
        if res != kResultOk {
            return None;
        }
        let w = (rect.right - rect.left).max(0) as u32;
        let h = (rect.bottom - rect.top).max(0) as u32;
        Some((w, h))
    }

    fn gui_set_scale(&self, scale: f64) -> Result<bool> {
        // C1 (r.md #8): IPlugViewContentScaleSupport を実装する plugin に
        // DPI scale を渡す。非対応 plugin は自前で DPI を扱うので false。
        let Some(view) = self.view.as_ref() else {
            return Ok(false);
        };
        let Some(scale_support) = view.cast::<IPlugViewContentScaleSupport>() else {
            return Ok(false);
        };
        #[allow(clippy::cast_possible_truncation)]
        let res = unsafe { scale_support.setContentScaleFactor(scale as f32) };
        Ok(res == kResultOk)
    }

    fn gui_sizer(&self) -> Option<Box<dyn EditorSizer>> {
        let view = self.view.as_ref()?;
        Some(Box::new(Vst3Sizer {
            view: view.as_ptr(),
            alive: Arc::clone(&self.view_alive),
        }))
    }

    fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()> {
        let view = self
            .view
            .as_ref()
            .context("gui not created — call gui_create_embedded first")?;
        // **`attached` の前**に窓を公開する。`attached` の doc は
        // *"Note that in this call the plug-in could call a IPlugFrame::resizeView ()!"*
        // と明記していて、attach の内側から来る resize もここで同期処理できねばならない。
        self.editor_hwnd.store(hwnd, Ordering::Release);
        let res = unsafe { view.attached(hwnd as *mut c_void, kPlatformTypeHWND) };
        tracing::info!(
            plugin = %self.name,
            hwnd = format!("{hwnd:#x}"),
            res = format!("{res:#x}"),
            "VST3 IPlugView::attached(HWND)"
        );
        anyhow::ensure!(
            res == kResultOk,
            "IPlugView::attached(HWND) -> {:#x}",
            res
        );
        // attached 直後に getSize で plugin が要求する初期サイズを取得 + ログ。
        let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        let size_res = unsafe { view.getSize(&mut rect) };
        tracing::info!(
            plugin = %self.name,
            res = format!("{size_res:#x}"),
            w = rect.right - rect.left,
            h = rect.bottom - rect.top,
            "VST3 getSize after attached"
        );
        self.gui_attached.set(true);
        Ok(())
    }

    fn gui_show(&self) -> Result<bool> {
        // VST3 has no show/hide concept. Consider "attached" == shown.
        Ok(self.gui_attached.get())
    }

    fn gui_hide(&self) -> Result<()> {
        // No-op on VST3 — actual hide is deferred to `gui_destroy`.
        Ok(())
    }

    fn gui_destroy(&mut self) {
        // **先頭で** alive を落とす (r.md #65): これ以降 `Vst3Sizer` は FFI を
        // 呼ばない。`pump_pending_messages` 由来の nested dispatch で
        // `gui_destroy` の途中に WM_SIZE が再入しても、ここで塞がる。
        self.view_alive.store(false, Ordering::Release);
        self.editor_hwnd.store(0, Ordering::Release);
        if let Some(view) = self.view.take() {
            if self.gui_attached.get() {
                unsafe {
                    // editorhost `WindowController::closePlugView` と同じ順:
                    // frame を外してから `removed()`。逆だと `removed()` の中から
                    // 飛んでくる resizeView が、もう窓を持たない frame に届く。
                    let _ = view.setFrame(std::ptr::null_mut());
                    let _ = view.removed();
                }
                self.gui_attached.set(false);
            }
            // view ComPtr drops -> release.
            drop(view);
        }
    }
}

/// [`EditorSizer`] の VST3 実装 (r.md #65)。
///
/// `IPlugView*` を **AddRef せずに借用**する。所有は [`Vst3Plugin::view`] 1 箇所の
/// ままで、`alive` が false になった後は一切触らない (`gui_destroy` が先頭で落とす)。
/// 自前 AddRef して持つと view の所有者が 2 箇所になり、`removed()` → release と
/// 競合したときに UAF になる。
struct Vst3Sizer {
    view: *mut IPlugView,
    alive: Arc<AtomicBool>,
}

// plugin-main スレッド専用。`EditorWindow` と同じ理由で `Send` を宣言する
// (`EditorSizer: Send` を満たすためだけで、実際にスレッドを跨がない)。
unsafe impl Send for Vst3Sizer {}

impl Vst3Sizer {
    /// 生きている view への **借用** (`ComRef` は refcount を触らない)。
    /// `gui_destroy` 後は `None`。
    fn view(&self) -> Option<ComRef<'_, IPlugView>> {
        if !self.alive.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `alive` が true の間は [`Vst3Plugin::view`] が `ComPtr` を保持して
        // いるので生きている (所有はあちら 1 箇所 = SSoT)。`ComRef` は AddRef も
        // Release もしないので、借用が所有権と競合しない。
        unsafe { ComRef::from_raw(self.view) }
    }
}

impl EditorSizer for Vst3Sizer {
    fn constrain_client_size(&self, w: u32, h: u32) -> (u32, u32) {
        let Some(view) = self.view() else { return (w, h) };
        let before = ViewRect {
            left: 0,
            top: 0,
            right: clamp_view_dim(w),
            bottom: clamp_view_dim(h),
        };
        let mut rect = before;
        // **戻り値では分岐しない**。`iplugview.h` は `checkSizeConstraint` の戻り値を
        // 一切規定しておらず (規定しているのは「不可なら rect を許容サイズへ直す」だけ)、
        // dev portal のシーケンス図は実プラグインが `kResultTrue (always)` を返すと
        // 注記している。editorhost は `!= kResultTrue` のときだけ採用する実装だが、
        // それだと丸めが常に捨てられる。JUCE と同じく **呼び出し前後の rect 比較**で判定する。
        let _ = unsafe { view.checkSizeConstraint(&mut rect) };
        let (nw, nh) = (rect.right - rect.left, rect.bottom - rect.top);
        if nw > 0 && nh > 0 && (nw, nh) != (before.right, before.bottom) {
            (nw as u32, nh as u32)
        } else {
            (w, h)
        }
    }

    fn current_client_size(&self) -> Option<(u32, u32)> {
        let view = self.view()?;
        let mut rect = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        if unsafe { view.getSize(&mut rect) } != kResultOk {
            return None;
        }
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        (w > 0 && h > 0).then_some((w as u32, h as u32))
    }

    fn notify_client_size(&self, w: u32, h: u32) {
        let Some(view) = self.view() else { return };
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: clamp_view_dim(w),
            bottom: clamp_view_dim(h),
        };
        let res = unsafe { view.onSize(&mut rect) };
        // `kResultFalse` (0x1) は失敗ではない。`iplugview.h` は `onSize` の戻り値を
        // 規定しておらず、editorhost も JUCE も見ていない。以前ここを
        // `ensure!(res == kResultOk)` にしていたため、正常動作している VST3 が
        // 軒並み WARN を吐いていた。
        if res != kResultOk {
            tracing::debug!(res = format!("{res:#x}"), w, h, "IPlugView::onSize returned non-ok");
        }
    }

    fn can_resize(&self) -> bool {
        self.view()
            .is_some_and(|view| unsafe { view.canResize() } == kResultTrue)
    }

    fn resize_hints(&self) -> Option<crate::plugin_instance::ResizeHints> {
        // VST3 に軸別可否 / アスペクト比の API は無い (`checkSizeConstraint` が
        // 丸めて返してくるのに任せる)。
        None
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

/// `ViewRect` へ入れる前に plugin 由来の寸法を健全な範囲へ切る
/// (`editor_window::clamp_dim` と同じ意図: `u32` → `i32` の折り返し防止)。
fn clamp_view_dim(v: u32) -> i32 {
    v.clamp(1, 16_384) as i32
}

fn encode_event(te: &TimedNoteEvent) -> Event {
    match te.event {
        // VST3 NoteOn/Off の `noteId` 標準 field に `note_id` を詰める。
        NoteTransition::On { note_id, key, velocity } => {
            let vst_note_id =
                if note_id <= i32::MAX as u32 { note_id as i32 } else { -1 };
            let note_on = NoteOnEvent {
                channel: 0,
                pitch: key as i16,
                tuning: 0.0,
                velocity: velocity as f32,
                length: 0,
                noteId: vst_note_id,
            };
            let mut ev = Event {
                busIndex: 0,
                sampleOffset: te.time as i32,
                ppqPosition: 0.0,
                flags: 0,
                r#type: Event_::EventTypes_::kNoteOnEvent as u16,
                __field0: event_type0_zero(),
            };
            unsafe {
                // SAFETY: we wrote a valid NoteOn into the union variant.
                std::ptr::write(
                    &mut ev.__field0 as *mut _ as *mut NoteOnEvent,
                    note_on,
                );
            }
            ev
        }
        NoteTransition::Off { note_id, key } => {
            let vst_note_id =
                if note_id <= i32::MAX as u32 { note_id as i32 } else { -1 };
            let note_off = NoteOffEvent {
                channel: 0,
                pitch: key as i16,
                velocity: 0.0,
                noteId: vst_note_id,
                tuning: 0.0,
            };
            let mut ev = Event {
                busIndex: 0,
                sampleOffset: te.time as i32,
                ppqPosition: 0.0,
                flags: 0,
                r#type: Event_::EventTypes_::kNoteOffEvent as u16,
                __field0: event_type0_zero(),
            };
            unsafe {
                std::ptr::write(
                    &mut ev.__field0 as *mut _ as *mut NoteOffEvent,
                    note_off,
                );
            }
            ev
        }
    }
}

fn event_type0_zero() -> Event__type0 {
    // Zero-init is safe as a starting point since `std::ptr::write` fills
    // the chosen variant before the plugin reads it.
    unsafe { std::mem::zeroed() }
}

/// Decode a VST3 Event coming back from the plugin's output event list into
/// our format-agnostic `TimedNoteEvent`. Unknown types are dropped silently.
pub(crate) fn decode_event(ev: &Event) -> Option<TimedNoteEvent> {
    let ty = ev.r#type as u32;
    if ty == Event_::EventTypes_::kNoteOnEvent as u32 {
        let note: &NoteOnEvent = unsafe { &*(&ev.__field0 as *const _ as *const NoteOnEvent) };
        // 「未指定」 = -1 → 0 に丸める。
        let note_id = note.noteId.max(0) as u32;
        Some(TimedNoteEvent {
            time: ev.sampleOffset.max(0) as u32,
            event: NoteTransition::On {
                note_id,
                key: note.pitch.clamp(0, 127) as u8,
                velocity: note.velocity as f64,
            },
        })
    } else if ty == Event_::EventTypes_::kNoteOffEvent as u32 {
        let note: &NoteOffEvent = unsafe { &*(&ev.__field0 as *const _ as *const NoteOffEvent) };
        let note_id = note.noteId.max(0) as u32;
        Some(TimedNoteEvent {
            time: ev.sampleOffset.max(0) as u32,
            event: NoteTransition::Off {
                note_id,
                key: note.pitch.clamp(0, 127) as u8,
            },
        })
    } else {
        None
    }
}
