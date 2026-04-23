#![allow(unsafe_op_in_unsafe_fn)]

//! VST3 plugin wrapper.
//!
//! Implements [`LoadedPlugin`] for VST3 backends. Uses `vst3` crate 0.3 for
//! the raw COM bindings and `libloading` to resolve `GetPluginFactory` from
//! the `.vst3` DLL (bundle layout `Contents/x86_64-win/<name>.vst3` on
//! Windows, or a legacy single-file `.vst3` DLL).
//!
//! Lifecycle mirrors CLAP:
//!   load → activate → start_processing → process* → stop_processing →
//!   deactivate → drop.
//!
//! Threading: everything here runs on the plugin-main thread except
//! `start_processing` / `stop_processing` / `process` / `output_buffer` /
//! `drain_out_notes_into`, which the audio thread touches via a raw
//! pointer. VST3 partitions its API the same way CLAP does, so long as
//! plugins respect the spec this is safe.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;
use common::vst3_scan::resolve_vst3_dll;
use libloading::{Library, Symbol};
use vst3::{
    ComPtr, ComWrapper, Interface,
    Steinberg::{
        FUnknown, IBStream, IPlugView, IPlugViewTrait, IPluginBaseTrait, IPluginFactory,
        IPluginFactoryTrait, PClassInfo, TUID, ViewRect, kNotImplemented, kPlatformTypeHWND,
        kResultOk, kResultTrue,
        Vst::{
            AudioBusBuffers, AudioBusBuffers__type0, BusDirections_, Event, Event__type0,
            Event_, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait,
            IEditController, IEditControllerTrait, IEventList, MediaTypes_, NoteOffEvent,
            NoteOnEvent, ProcessContext, ProcessContext_::StatesAndFlags_, ProcessData,
            ProcessModes_, ProcessSetup, SpeakerArr, SpeakerArrangement,
            SymbolicSampleSizes_,
        },
    },
};

use crate::plugin_instance::{HostCallbacks, LoadedPlugin, NoteTransition, TimedNoteEvent};
use crate::vst3_events::{Vst3InEventList, Vst3OutEventList};
use crate::vst3_host::{Vst3ComponentHandler, Vst3HostApp, Vst3PlugFrame};
use crate::vst3_stream::{Vst3ReadStream, Vst3WriteStream};

/// A loaded VST3 plugin (the IComponent + the associated IEditController
/// for GUI/state). The `_library` field keeps the DLL loaded for as long as
/// we hold any ComPtr into it — drop order: `component`/`controller`/`view`
/// Release first (inside the ComPtrs' Drop), then the `Library` unloads.
pub struct Vst3Plugin {
    // DLL handle kept alive while any ComPtr references it. Declared first
    // so it drops LAST (field drop order = declaration order).
    _library: Library,
    id: String,
    name: String,
    path: PathBuf,

    // ComPtrs into the plugin.
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

    // --- Audio processing state ----------------------------------------
    active: bool,
    processing: bool,
    input_channels: u32,
    input_buffers: Vec<Vec<f32>>,
    input_ptrs: Vec<*mut f32>,
    output_channels: u32,
    output_buffers: Vec<Vec<f32>>,
    output_ptrs: Vec<*mut f32>,
    max_frames: u32,
    sample_rate: f64,

    /// Scratch buffer for input events fed to the plugin this tick. Cleared
    /// and re-filled on every `process()`.
    in_event_buffer: Vec<Event>,
    /// Events the plugin emitted during the previous `process()`.
    collected_out_notes: Vec<TimedNoteEvent>,

    /// Reusable IEventList instances fed to the plugin on every
    /// `process()`. Owned here so the audio thread never allocates a new
    /// `ComWrapper` per buffer (which would heap-allocate inside the RT
    /// callback).
    in_event_list: ComWrapper<Vst3InEventList>,
    out_event_list: ComWrapper<Vst3OutEventList>,
    /// Transport / timing block the plugin reads via `ProcessData.processContext`.
    /// Several instruments (SynthMaster 3, some Arturia products) stay silent
    /// when this pointer is null because they gate their voice allocator on
    /// `kPlaying`. Updated in `process()` with the current playhead.
    process_context: ProcessContext,

    // --- GUI state -----------------------------------------------------
    view: Option<ComPtr<IPlugView>>,
    /// Plug-frame used to relay resize requests back to daw_gui.
    plug_frame: ComWrapper<Vst3PlugFrame>,
    gui_attached: std::cell::Cell<bool>,
}

unsafe impl Send for Vst3Plugin {}

impl Vst3Plugin {
    pub fn load(path: &Path, target_id: &str, callbacks: HostCallbacks) -> Result<Self> {
        let dll_path = resolve_vst3_dll(path)
            .with_context(|| format!("resolving VST3 at {}", path.display()))?;
        let library = unsafe { Library::new(&dll_path) }
            .with_context(|| format!("LoadLibrary {}", dll_path.display()))?;

        // Call InitDll() if the module exports it (VST3 3.6.x requirement on
        // Windows). Absent = fine, some crates don't export it.
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

        // Scan class infos for Audio Module Class entries.
        // `target_id` is matched as a 32-hex-digit UUID against the class
        // CID. Anything else (empty string, bundle stem from plugin_db
        // scanning) is treated as "first Audio Module Class wins".
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
        // project save / plugin_db lookup. Empty id → default to UUID hex so
        // initial scans get something stable to persist.
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
        let component_handler = ComWrapper::new(Vst3ComponentHandler::new(callbacks.clone()));
        let plug_frame = ComWrapper::new(Vst3PlugFrame::new(callbacks.clone()));

        // initialize(component, IHostApplication*)
        let host_app_ptr: *mut FUnknown = host_app
            .to_com_ptr::<FUnknown>()
            .context("host_app has no FUnknown")?
            .into_raw();
        // `initialize` takes ownership of one reference; we re-wrap so Drop
        // releases exactly once after the call below.
        let init_res = unsafe { component.initialize(host_app_ptr) };
        // Re-grab the ComPtr without adding another ref (we already own one
        // via the ComWrapper).
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

        // Resolve controller (same instance or a separate one created via
        // the factory).
        let mut ctrl_cid: TUID = [0; 16];
        let ctrl_res = unsafe { component.getControllerClassId(&mut ctrl_cid as *mut TUID) };
        let (controller, controller_separate) = if ctrl_res == kResultOk && ctrl_cid != class_cid {
            // Separate controller.
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
            // Some instrument plugins only implement IEditController in a
            // separate class even when getControllerClassId returns the
            // component CID (rare). Try createInstance with class_cid just
            // in case, otherwise bail.
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

        // Query audio bus counts for activate()-time buffer allocation.
        let in_bus_count = unsafe {
            component.getBusCount(
                MediaTypes_::kAudio,
                BusDirections_::kInput,
            )
        };
        let out_bus_count = unsafe {
            component.getBusCount(
                MediaTypes_::kAudio,
                BusDirections_::kOutput,
            )
        };
        tracing::info!(in_bus_count, out_bus_count, "VST3 audio bus counts");

        let input_channels = if in_bus_count > 0 { 2 } else { 0 };
        let output_channels = if out_bus_count > 0 { 2 } else { 0 };

        Ok(Self {
            _library: library,
            id: display_id,
            name: class_name,
            path: path.to_path_buf(),
            component,
            audio,
            controller,
            _host_app: host_app,
            _component_handler: component_handler,
            controller_separate,
            active: false,
            processing: false,
            input_channels,
            input_buffers: Vec::new(),
            input_ptrs: Vec::new(),
            output_channels,
            output_buffers: Vec::new(),
            output_ptrs: Vec::new(),
            max_frames: 0,
            sample_rate: 0.0,
            in_event_buffer: Vec::with_capacity(256),
            collected_out_notes: Vec::with_capacity(256),
            in_event_list: ComWrapper::new(Vst3InEventList::new()),
            out_event_list: ComWrapper::new(Vst3OutEventList::new()),
            process_context: unsafe { std::mem::zeroed() },
            view: None,
            plug_frame,
            gui_attached: std::cell::Cell::new(false),
        })
    }
}

fn create_instance<I: Interface>(
    factory: &ComPtr<IPluginFactory>,
    cid: &TUID,
) -> Option<ComPtr<I>> {
    let mut obj: *mut c_void = std::ptr::null_mut();
    let iid_guid = I::IID;
    // IPluginFactory::createInstance takes FIDString (= *const c_char) for
    // cid and iid. Reinterpret as pointers.
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

fn c_array_to_string(buf: &[std::ffi::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn tuid_to_hex(tuid: &TUID) -> String {
    let mut s = String::with_capacity(32);
    for b in tuid {
        s.push_str(&format!("{:02X}", *b as u8));
    }
    s
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        if self.gui_attached.get()
            && let Some(view) = self.view.as_ref()
        {
            unsafe {
                let _ = view.removed();
            }
        }
        self.view = None;
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

    fn activate(&mut self, sample_rate: f64, _min_frames: u32, max_frames: u32) -> Result<()> {
        anyhow::ensure!(!self.active, "VST3 plugin already active");

        // 1. Negotiate speaker arrangements for each bus (MVP: stereo).
        let stereo: SpeakerArrangement = SpeakerArr::kStereo;
        let mut in_arr: Vec<SpeakerArrangement> = if self.input_channels > 0 {
            vec![stereo]
        } else {
            Vec::new()
        };
        let mut out_arr: Vec<SpeakerArrangement> = if self.output_channels > 0 {
            vec![stereo]
        } else {
            Vec::new()
        };
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
                let n = unsafe {
                    self.component
                        .getBusCount(media, dir)
                };
                for i in 0..n {
                    unsafe {
                        self.component
                            .activateBus(media, dir, i, 1);
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

        // 5. Allocate planar buffers for process() and prime the transport
        // block so the very first `process()` already sees `kPlaying` —
        // some plugins (SynthMaster 3 among them) refuse to output anything
        // when `processContext` is null or `state` doesn't include
        // `kPlaying`.
        self.process_context.state = (StatesAndFlags_::kPlaying
            | StatesAndFlags_::kTempoValid
            | StatesAndFlags_::kTimeSigValid) as u32;
        self.process_context.sampleRate = sample_rate;
        self.process_context.tempo = 120.0;
        self.process_context.timeSigNumerator = 4;
        self.process_context.timeSigDenominator = 4;

        self.max_frames = max_frames;
        self.sample_rate = sample_rate;
        self.input_buffers = (0..self.input_channels as usize)
            .map(|_| vec![0.0f32; max_frames as usize])
            .collect();
        self.input_ptrs = vec![std::ptr::null_mut(); self.input_channels as usize];
        self.output_buffers = (0..self.output_channels as usize)
            .map(|_| vec![0.0f32; max_frames as usize])
            .collect();
        self.output_ptrs = vec![std::ptr::null_mut(); self.output_channels as usize];
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
        self.output_buffers.clear();
        self.output_ptrs.clear();
    }

    fn start_processing(&mut self) -> Result<()> {
        anyhow::ensure!(self.active, "VST3 plugin not active");
        anyhow::ensure!(!self.processing, "VST3 plugin already processing");
        let res = unsafe { self.audio.setProcessing(1) };
        // VST3 spec treats `setProcessing` as optional — some plugins (e.g.
        // SynthMaster 3) return `kNotImplemented` instead of accepting the
        // state change. Those plugins are always ready to process; treat
        // `kNotImplemented` the same as `kResultOk`.
        anyhow::ensure!(
            res == kResultOk || res == kNotImplemented,
            "IAudioProcessor::setProcessing(1) -> {:#x}",
            res
        );
        self.processing = true;
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
    }

    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        input_audio: &[&[f32]],
    ) -> Result<i32> {
        anyhow::ensure!(self.processing, "VST3 plugin not processing");

        // --- Copy inputs into pre-allocated planar buffers.
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
        for i in 0..self.output_buffers.len() {
            self.output_ptrs[i] = self.output_buffers[i].as_mut_ptr();
        }

        // --- Build Event buffer and hand it to the reusable input list.
        // No per-process allocation: both `in_event_buffer` and
        // `Vst3InEventList` keep their capacity across calls.
        self.in_event_buffer.clear();
        for te in events {
            self.in_event_buffer.push(encode_event(te));
        }
        self.in_event_list.set_events(&self.in_event_buffer);
        self.collected_out_notes.clear();

        // `to_com_ptr` only bumps the Arc strong count + addRef; no heap
        // allocation. The ComPtrs' Drop at end-of-scope balances the
        // addRef with a release, keeping the ComWrapper's own ref.
        let in_list_ptr = self
            .in_event_list
            .to_com_ptr::<IEventList>()
            .context("Vst3InEventList has no IEventList")?;
        let out_list_ptr = self
            .out_event_list
            .to_com_ptr::<IEventList>()
            .context("Vst3OutEventList has no IEventList")?;

        // --- Assemble AudioBusBuffers.
        let mut in_bus = AudioBusBuffers {
            numChannels: self.input_channels as i32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: self.input_ptrs.as_mut_ptr(),
            },
        };
        let mut out_bus = AudioBusBuffers {
            numChannels: self.output_channels as i32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: self.output_ptrs.as_mut_ptr(),
            },
        };

        // Advance the transport block. `projectTimeSamples` is left
        // free-running (+= frames each call) until the host plumbs a real
        // playhead through — most plugins only need the `kPlaying` flag
        // plus a monotonically-increasing counter.
        self.process_context.projectTimeSamples = self
            .process_context
            .projectTimeSamples
            .saturating_add(frames as i64);

        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32,
            numSamples: frames as i32,
            numInputs: if self.input_channels > 0 { 1 } else { 0 },
            numOutputs: if self.output_channels > 0 { 1 } else { 0 },
            inputs: if self.input_channels > 0 {
                &mut in_bus
            } else {
                std::ptr::null_mut()
            },
            outputs: if self.output_channels > 0 {
                &mut out_bus
            } else {
                std::ptr::null_mut()
            },
            inputParameterChanges: std::ptr::null_mut(),
            outputParameterChanges: std::ptr::null_mut(),
            // as_ptr keeps shared ownership with the ComPtrs above; they
            // release on scope exit, so nothing leaks and no extra release
            // is needed after process().
            inputEvents: in_list_ptr.as_ptr(),
            outputEvents: out_list_ptr.as_ptr(),
            processContext: &mut self.process_context,
        };
        let status = unsafe { self.audio.process(&mut data) };
        #[cfg(debug_assertions)]
        if status != kResultOk {
            tracing::warn!(
                plugin = %self.name,
                status = format!("{status:#x}"),
                "VST3 process returned non-OK"
            );
        }

        // Drain collected events before the ComPtrs drop (order doesn't
        // strictly matter, but keeps the reader's mental model simple).
        self.out_event_list.drain_into(&mut self.collected_out_notes);

        Ok(status)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        self.output_buffers.get(channel).map(|v| v.as_slice())
    }

    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>) {
        out.append(&mut self.collected_out_notes);
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

    fn state_load(&self, data: &[u8]) -> Result<()> {
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

    fn gui_is_embed_supported(&self) -> bool {
        // Create view to probe platform support, then release.
        let view_raw = unsafe { self.controller.createView(c"editor".as_ptr()) };
        if view_raw.is_null() {
            return false;
        }
        let Some(view) = (unsafe { ComPtr::<IPlugView>::from_raw(view_raw) }) else {
            return false;
        };
        let res = unsafe { view.isPlatformTypeSupported(kPlatformTypeHWND) };
        res == kResultTrue || res == kResultOk
    }

    fn gui_create_embedded(&mut self) -> Result<()> {
        if self.view.is_some() {
            return Ok(());
        }
        let view_raw = unsafe { self.controller.createView(c"editor".as_ptr()) };
        anyhow::ensure!(!view_raw.is_null(), "IEditController::createView returned null");
        let Some(view) = (unsafe { ComPtr::<IPlugView>::from_raw(view_raw) }) else {
            anyhow::bail!("createView returned null after from_raw");
        };
        // Attach the plug-frame so resize requests get routed back.
        let frame_ptr = self
            .plug_frame
            .to_com_ptr::<vst3::Steinberg::IPlugFrame>()
            .context("plug_frame has no IPlugFrame")?
            .into_raw();
        let _ = unsafe { view.setFrame(frame_ptr) };
        // Keep one ref in self via `view`; Release the extra we added via
        // into_raw — setFrame does addRef internally so ownership is balanced.
        let _ = unsafe { ComPtr::<vst3::Steinberg::IPlugFrame>::from_raw(frame_ptr) };
        self.view = Some(view);
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

    fn gui_set_scale(&self, _scale: f64) -> Result<bool> {
        // MVP: skip IPlugViewContentScaleSupport; most plugins handle DPI
        // themselves when running at 1.0.
        Ok(false)
    }

    fn gui_can_resize(&self) -> bool {
        let Some(view) = self.view.as_ref() else {
            return false;
        };
        unsafe { view.canResize() == kResultTrue }
    }

    fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()> {
        let view = self
            .view
            .as_ref()
            .context("gui not created — call gui_create_embedded first")?;
        let res = unsafe { view.attached(hwnd as *mut c_void, kPlatformTypeHWND) };
        anyhow::ensure!(
            res == kResultOk,
            "IPlugView::attached(HWND) -> {:#x}",
            res
        );
        self.gui_attached.set(true);
        Ok(())
    }

    fn gui_show(&self) -> Result<bool> {
        // VST3 has no show/hide concept. Consider "attached" == shown.
        Ok(self.gui_attached.get())
    }

    fn gui_hide(&self) -> Result<()> {
        // No-op on VST3 — actual hide is deferred to `gui_destroy` which
        // calls `removed()`.
        Ok(())
    }

    fn gui_set_size(&self, width: u32, height: u32) -> Result<()> {
        let view = self.view.as_ref().context("no view to resize")?;
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        let _ = unsafe { view.checkSizeConstraint(&mut rect) };
        let res = unsafe { view.onSize(&mut rect) };
        anyhow::ensure!(res == kResultOk, "IPlugView::onSize -> {:#x}", res);
        Ok(())
    }

    fn gui_destroy(&mut self) {
        if let Some(view) = self.view.take() {
            if self.gui_attached.get() {
                unsafe {
                    let _ = view.removed();
                }
                self.gui_attached.set(false);
            }
            // view ComPtr drops -> release.
            drop(view);
        }
    }
}

fn encode_event(te: &TimedNoteEvent) -> Event {
    match te.event {
        NoteTransition::On { key, velocity } => {
            let note_on = NoteOnEvent {
                channel: 0,
                pitch: key as i16,
                tuning: 0.0,
                velocity: velocity as f32,
                length: 0,
                noteId: -1,
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
        NoteTransition::Off { key } => {
            let note_off = NoteOffEvent {
                channel: 0,
                pitch: key as i16,
                velocity: 0.0,
                noteId: -1,
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
    // The Event union is too large for any single variant; zero-init is safe
    // as a starting point since `std::ptr::write` fills the chosen variant
    // before the plugin reads it.
    unsafe { std::mem::zeroed() }
}

/// Decode a VST3 Event coming back from the plugin's output event list into
/// our format-agnostic `TimedNoteEvent`. Unknown types are dropped silently
/// (return `None`).
pub(crate) fn decode_event(ev: &Event) -> Option<TimedNoteEvent> {
    let ty = ev.r#type as u32;
    if ty == Event_::EventTypes_::kNoteOnEvent as u32 {
        let note: &NoteOnEvent = unsafe { &*(&ev.__field0 as *const _ as *const NoteOnEvent) };
        Some(TimedNoteEvent {
            time: ev.sampleOffset.max(0) as u32,
            event: NoteTransition::On {
                key: note.pitch.clamp(0, 127) as u8,
                velocity: note.velocity as f64,
            },
        })
    } else if ty == Event_::EventTypes_::kNoteOffEvent as u32 {
        let note: &NoteOffEvent = unsafe { &*(&ev.__field0 as *const _ as *const NoteOffEvent) };
        Some(TimedNoteEvent {
            time: ev.sampleOffset.max(0) as u32,
            event: NoteTransition::Off {
                key: note.pitch.clamp(0, 127) as u8,
            },
        })
    } else {
        None
    }
}
