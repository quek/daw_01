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
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use common::plugin_format::PluginFormat;
use common::protocol::RenderMode;
use common::vst3_scan::resolve_vst3_dll;
use libloading::{Library, Symbol};
use common::vst3_scan::{c_array_to_string, tuid_to_hex};
use vst3::{
    ComPtr, ComWrapper, Interface,
    Steinberg::{
        FUnknown, IBStream, IPlugView, IPlugViewTrait, IPluginBaseTrait, IPluginFactory,
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

use crate::plugin_instance::{HostCallbacks, LoadedPlugin, NoteTransition, TimedNoteEvent};
use crate::vst3_events::{Vst3InEventList, Vst3OutEventList};
use crate::vst3_host::{Vst3ComponentHandler, Vst3HostApp, Vst3PlugFrame};
use crate::vst3_params::Vst3InParamChanges;
use crate::vst3_stream::{Vst3ReadStream, Vst3WriteStream};

/// A loaded VST3 plugin (the IComponent + the associated IEditController
/// for GUI/state). Field drop order is **declaration order** (Rust
/// reference: "The fields of a struct are dropped in the same order as
/// they were declared"), so `_library` is declared LAST: every ComPtr's
/// `Release()` (which calls back into the DLL) must run before the
/// `Library` Drop unloads the DLL.
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

    /// (r.md #5 ARA2) ARA session bound to this instance, if any. Dropped before
    /// the component is released (its drop calls back into this plug-in).
    ara: Option<crate::ara::session::AraSession>,
    /// Last activate params, kept so ARA setup — which must bind before
    /// `setActive` — can deactivate → bind → reactivate.
    last_activate: Option<(f64, u32, u32)>,

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
    /// PR4 sidechain: per-aux-input-bus channel counts in the plugin's
    /// declared bus order (skipping the main bus). Capped at
    /// `MAX_AUX_IN`. Empty when the plugin has only the main input bus.
    aux_input_channels: Vec<u32>,
    /// Pre-allocated planar buffers for each aux input bus (bus × ch × frame).
    aux_input_buffers: Vec<Vec<Vec<f32>>>,
    /// Per-aux-bus channel pointer scratch (refreshed each process()).
    aux_input_ptrs: Vec<Vec<*mut f32>>,
    /// Phase 6 review (RT 安全): VST3 `process()` の hot path で毎 buffer に
    /// `Vec<AudioBusBuffers>::with_capacity(1 + aux)` を確保 → drop して
    /// いた。 audio thread の heap alloc/free は RT 違反。 `activate()` で
    /// pre-allocate、 `process()` 入口で `clear()` + `push()` で reuse 化。
    /// CLAP 側の `process_input_bufs` と同 pattern。
    process_input_bufs: Vec<AudioBusBuffers>,
    /// Extra (non-main) output buses' channel counts, in declared bus order
    /// (skipping the main output bus at index 0). Empty when the plugin has
    /// a single output bus. VST3 spec requires `process()`'s `numOutputs` to
    /// equal the plugin's total output bus count; multi-output synths
    /// (Surge XT declares Output + Scene A + Scene B) otherwise get a
    /// mismatch and emit no main output. We provide (discarded) scratch for
    /// each extra bus so the count matches and the main bus is honoured.
    extra_output_channels: Vec<u32>,
    /// Pre-allocated scratch planar buffers for each extra output bus
    /// (bus × ch × frame). The plugin writes its unused-by-us extra outputs
    /// here; we read only the main bus via `output_buffer()`.
    extra_output_buffers: Vec<Vec<Vec<f32>>>,
    /// Per-extra-output-bus channel pointer scratch (refreshed each process()).
    extra_output_ptrs: Vec<Vec<*mut f32>>,
    /// Pre-allocated `Vec<AudioBusBuffers>` for output buses (main + extras),
    /// mirroring `process_input_bufs`. clear()/push() reuse on the RT path.
    process_output_bufs: Vec<AudioBusBuffers>,

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
    /// Reusable `IParameterChanges` fed to the plugin via
    /// `ProcessData.inputParameterChanges` every `process()`. Carries
    /// automation lane values (`TimedParamEvent`) into the plugin. Owned
    /// here so the audio thread never allocates a COM object per buffer.
    in_param_changes: ComWrapper<Vst3InParamChanges>,
    /// `docs/plan_modulation_routing_redesign.md` §3.3: last absolute
    /// normalized value sent per param. VST3 has no modulation channel, so the
    /// host folds a `ParamEventKind::Mod` offset into `base + offset` using this
    /// cache (VST3 params are normalized `0..=1`, so `amount == offset`).
    /// Pre-seeded with every param's default at load → no RT allocation.
    param_mod_base: std::collections::HashMap<u32, f64>,
    /// Pre-allocated scratch holding `param_events` with all `Mod` offsets
    /// folded into absolute `Value`s, handed to `set_changes` each `process()`.
    folded_param_events: Vec<crate::plugin_instance::TimedParamEvent>,
    /// Set by `process()` (RT thread) when the param-change pool overflowed
    /// (> capacity distinct params in one buffer; extra param events dropped).
    /// Logged once from `stop_processing()` (RT-external) so the hot path never
    /// formats a message or touches `tracing`. `Relaxed` is sufficient: this is
    /// a best-effort diagnostic flag, not a synchronization signal.
    param_pool_overflowed: AtomicBool,
    /// Debug-only: set by `process()` when `IAudioProcessor::process` returned
    /// a non-OK status, logged once (with the status) from `stop_processing()`.
    /// Same flag idiom as `param_pool_overflowed` so a misbehaving plugin can't
    /// flood the log every buffer.
    #[cfg(debug_assertions)]
    process_status_err: std::sync::atomic::AtomicI32,
    /// Transport / timing block the plugin reads via `ProcessData.processContext`.
    /// Several instruments (SynthMaster 3, some Arturia products) stay silent
    /// when this pointer is null because they gate their voice allocator on
    /// `kPlaying`. Updated in `process()` with the current playhead.
    process_context: ProcessContext,

    /// Phase 7 B1-R (2026-05-13): export 中 (= `set_render_mode(Offline)`
    /// 受信後) は `ProcessData::processMode` を `kOffline` に切り替える。
    /// VST3 spec の `IComponent::setIoMode` は `initialize` 前にしか呼べ
    /// ない (= 既に active な plugin への動的切替は spec 違反 + plugin 依存
    /// で動かない) ため、 spec 準拠の代替として process 毎の `processMode`
    /// を切替える方式を採用。 plugin が process() 毎の processMode を尊重
    /// するか `setupProcessing` 時の値を固定するかは plugin 実装依存だが、
    /// 多くの reverb / convolution / lookahead 系 plugin は process 毎を
    /// 読んで「offline = 高品質 algo に切替」 等を判定する。
    render_mode: RenderMode,

    // --- GUI state -----------------------------------------------------
    view: Option<ComPtr<IPlugView>>,
    /// Plug-frame used to relay resize requests back to daw_gui.
    plug_frame: ComWrapper<Vst3PlugFrame>,
    gui_attached: std::cell::Cell<bool>,

    /// DLL handle. Declared LAST so it drops LAST (field drop = declaration
    /// order): every ComPtr/ComWrapper above must Release into the still-loaded
    /// DLL before `FreeLibrary` runs here. Reversing this order produces an
    /// AV ~84-100ms after Drop on plugins (e.g. MeldaProduction VST3) whose
    /// `IComponent::Release` does heavy global cleanup — by the time their
    /// `DllMain(DLL_PROCESS_DETACH)` finishes the host has already torn the
    /// vtable down.
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

        // For plugins with a separate IEditController (BioTek 2 etc.) the
        // controller is a fresh COM object that doesn't yet know about the
        // component's state. Two host-side hooks tie them together — without
        // these, controllers may refuse to create their editor view (CLAP-
        // style "no editor" return) or simply mirror stale defaults:
        //
        //   1. `IConnectionPoint::connect` — bidirectional message bus used
        //      by some SDKs to keep parameter values in sync.
        //   2. `IComponent::getState` → `IEditController::setComponentState`
        //      — primes the controller's parameters from the component.
        //
        // Both calls are best-effort. Plugins that don't implement them, or
        // that signal `kNotImplemented`, just fall through.
        if controller_separate {
            connect_component_and_controller(&component, &controller);
            transfer_component_state(&component, &controller);
        }

        // 全 bus (audio + event, in + out) を enumerate して 1 件ずつログ。
        // MeldaProduction の MSoundFactory のように main bus 以外に sidechain /
        // sub bus を多数持つプラグインを debug するため、 channel count や
        // busType (Main / Aux) を明示する。
        log_all_buses(&component);

        // Query main audio bus channel count for activate()-time buffer
        // allocation. 旧実装は「bus 数 > 0 なら 2ch 決め打ち」 だったが、 main
        // bus が mono / 4ch の plugin で破綻する。 main bus (busType == Main)
        // の channelCount をそのまま採用する。
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

        let mut plugin = Self {
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
            ara: None,
            last_activate: None,
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
            aux_input_channels,
            aux_input_buffers: Vec::new(),
            aux_input_ptrs: Vec::new(),
            process_input_bufs: Vec::new(),
            extra_output_channels,
            extra_output_buffers: Vec::new(),
            extra_output_ptrs: Vec::new(),
            process_output_bufs: Vec::new(),
            in_event_buffer: Vec::with_capacity(256),
            collected_out_notes: Vec::with_capacity(256),
            in_event_list: ComWrapper::new(Vst3InEventList::new()),
            out_event_list: ComWrapper::new(Vst3OutEventList::new()),
            in_param_changes: ComWrapper::new(Vst3InParamChanges::new()),
            param_mod_base: std::collections::HashMap::new(),
            folded_param_events: Vec::with_capacity(256),
            param_pool_overflowed: AtomicBool::new(false),
            #[cfg(debug_assertions)]
            process_status_err: std::sync::atomic::AtomicI32::new(kResultOk),
            process_context: unsafe { std::mem::zeroed() },
            render_mode: RenderMode::Realtime,
            view: None,
            plug_frame,
            gui_attached: std::cell::Cell::new(false),
        };
        // `docs/plan_modulation_routing_redesign.md` §3.3 / §4: seed the
        // modulation base cache with every param's default (normalized) so the
        // audio-thread fold only updates existing keys (no RT allocation).
        plugin.init_param_mod_base();
        Ok(plugin)
    }

    /// Populate `param_mod_base` from the plugin's param list (main thread,
    /// once at load). Values are normalized `0..=1` (VST3 convention).
    fn init_param_mod_base(&mut self) {
        let infos = self.enumerate_params();
        self.param_mod_base.reserve(infos.len());
        for info in &infos {
            self.param_mod_base
                .insert(info.id, info.default_value.clamp(0.0, 1.0));
        }
    }
}

/// `String128` ([TChar; 128] = u16 array) を Rust String に。 null terminator
/// 以降は捨てる。 Bus 名等の人間可読ログ表示用。
fn utf16_buf_to_string<const N: usize>(buf: &[u16; N]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(N);
    String::from_utf16_lossy(&buf[..end])
}

/// Audio + Event bus を全 enumerate して 1 件ずつ INFO ログ。 channel count や
/// busType (Main / Aux) を出すことで、 multi-bus plugin の構成把握 + 不整合
/// debug を可能にする。
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

/// 指定 audio bus の channel count から SpeakerArrangement を導出。 mono → kMono、
/// 2 → kStereo、 0 → 0 (= 空 arrangement、 plugin に「この bus は使わない」 を伝える)、
/// その他は fallback (kStereo) を返す。 channel count 取得失敗時も fallback。
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
    match info.channelCount {
        0 => 0,
        1 => SpeakerArr::kMono,
        2 => SpeakerArr::kStereo,
        _ => fallback,
    }
}

/// `busType == Main` の最初の audio bus の channel count を返す。 main bus が
/// 無ければ index 0 (= 多くの plugin で main 相当)、 それも無ければ 0。
/// PR4 sidechain: enumerate `is_main=false` (= `kAux`) input buses and
/// return their channel counts in declaration order. Capped at
/// `MAX_AUX_IN`. Empty when the plugin has only the main input bus.
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
            // Match the main-bus side's `.max(0)` (channelCount is i32 and can
            // be negative on malformed plugins) and clamp to the host's
            // per-bus channel ceiling. The shmem aux buffer only carries
            // `MAX_CHANNELS` planes (`buffer_aux_in[..][MAX_CHANNELS][..]`), and
            // `process()` only ever fills l/r from `AuxInputBuf`, so anything
            // above that would be allocated-but-never-fed silence.
            let ch = (info.channelCount.max(0) as u32)
                .min(common::process_data::MAX_CHANNELS as u32);
            aux.push(ch);
        }
    }
    aux
}

/// VST3 spec: BusType の Main は `kMain` (= 0)。
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

/// Channel counts of every output bus *after* index 0 (the main output bus
/// daw_01 reads back via `output_buffer`). VST3 `process()` must provide an
/// `AudioBusBuffers` for every output bus the plugin declares; multi-output
/// synths (Surge XT: Output + Scene A + Scene B) would otherwise hit a
/// `numOutputs` mismatch and emit no main output. Returns one entry per bus
/// index `1..count` (channel count, or `0` if the info read fails) so the
/// process-time `outputs` array stays aligned with the plugin's bus indices.
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
/// in both directions. Best-effort: plugins that don't implement
/// `IConnectionPoint` (the interface is optional in the VST3 spec) are
/// silently skipped. The pair stays usable without the connection — only
/// inter-object messaging breaks, which most hosts don't rely on.
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
/// `IEditController::setComponentState`. Required by Steinberg's spec for
/// plugins with separate controllers — without it some plugins refuse to
/// produce an editor view because they have no parameter snapshot to
/// render. Best-effort: failures are logged, not propagated.
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

/// VST3 のクラスを一時 instantiate して bus 構成から **port 構成**
/// (note 入力 / note 出力 / audio 出力の有無) を読む。 daw_plugin_host の
/// `--probe-vst3` one-shot モードから呼ばれる。 VST3 規格には note-effect の
/// category tag が無く、 bus 構成でしか判別できない (`ivstaudioprocessor.h`
/// PlugType / `ivstcomponent.h` MediaTypes)。 これで note-effect (note in/out・
/// audio out なし) も dual-role (note out かつ audio out、 例: Scaler 2) も拾える。
/// 失敗時は呼び元 (scan) が scan-time 暫定値を保持するので退行しない。
///
/// `load` の前半 (module → factory → class 解決 → component → initialize) を
/// 再現し、 audio processing / controller / activate はしない (= 軽量・副作用最小)。
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

    // クラス検索 (load と同 idiom): target_id が 32-hex UUID ならその CID 一致、
    // さもなくば最初の Audio Module Class。
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

    // bus 構成をそのまま port 構成として返す。v23: audio input bus も拾うことで、
    // engine が「audio を生成する音源 (au_in==0)」と「audio を加工するエフェクト
    // (au_in>0)」を port 直結で区別できる (audio_in 有り = 入力を処理して置換)。
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

// `c_array_to_string` / `tuid_to_hex` live in `common::vst3_scan` as the
// shared SSoT (imported above) so scan-side (writes `cid_hex` to the plugin
// DB) and load-side (this module, matches against it) hex / name decoding can
// never drift.

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        // (r.md #5 ARA2) Tear down the ARA session before releasing the
        // component — its drop issues destroy calls back into this plug-in.
        // Deactivate first so detaching its playback regions is valid.
        if self.ara.is_some() {
            if self.active {
                self.deactivate();
            }
            self.ara = None;
        }
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
        archive: Option<&[u8]>,
    ) -> Result<bool> {
        if self.ara.is_none() {
            return Ok(false);
        }
        // set_clips / region detach require the instance inactive; deactivate
        // around the update, then restore. The bind already happened at load.
        let was_active = self.active;
        let restore = self.last_activate;
        if was_active {
            self.deactivate();
        }
        if let Some(session) = self.ara.as_mut() {
            session.set_clips(clips);
        }
        if let Some(archive) = archive.filter(|a| !a.is_empty())
            && let Some(session) = self.ara.as_ref()
        {
            session.restore_archive(archive);
        }
        if was_active
            && let Some((sample_rate, min_frames, max_frames)) = restore
        {
            self.activate(sample_rate, min_frames, max_frames)?;
        }
        Ok(true)
    }

    fn clear_ara(&mut self) {
        if self.ara.is_none() {
            return;
        }
        let was_active = self.active;
        let restore = self.last_activate;
        if was_active {
            self.deactivate();
        }
        self.ara = None;
        if was_active
            && let Some((sample_rate, min_frames, max_frames)) = restore
        {
            let _ = self.activate(sample_rate, min_frames, max_frames);
        }
    }

    fn store_ara_archive(&self) -> Option<Vec<u8>> {
        self.ara.as_ref().and_then(|session| session.store_archive())
    }

    /// VST3 param 一覧を `IEditController` から列挙。 VST3 の param は仕様上
    /// 常に normalized [0,1] なので min/max は 0/1 固定、 default は
    /// `defaultNormalizedValue`。 plugin-main thread で呼ばれる
    /// (`SetSlotPlugin` 処理経路、 controller は main-thread API)。
    fn enumerate_params(&self) -> Vec<common::protocol::PluginParamInfo> {
        let count = unsafe { self.controller.getParameterCount() };
        if count <= 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: ParameterInfo is plain data; zeroed start is legal
            // because getParameterInfo overwrites the fields it populates.
            let mut info: ParameterInfo = unsafe { std::mem::zeroed() };
            let res = unsafe { self.controller.getParameterInfo(i, &raw mut info) };
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
            // VST3 の離散 param は stepCount > 0 (kIsList は enum 型)。 どちらも
            // CLAP の STEPPED 相当として扱う。
            if info.stepCount > 0 || info.flags & ParameterFlags_::kIsList != 0 {
                flags |= common::protocol::plugin_param_flags::STEPPED;
            }
            if info.flags & ParameterFlags_::kIsWrapAround != 0 {
                flags |= common::protocol::plugin_param_flags::PERIODIC;
            }
            out.push(common::protocol::PluginParamInfo {
                id: info.id,
                name,
                // VST3 の module/grouping は unitId + IUnitInfo 階層で表現される。
                // 現状 daw_gui は flat 表示なので module は空 (CLAP も任意)。
                module: String::new(),
                min_value: 0.0,
                max_value: 1.0,
                default_value: info.defaultNormalizedValue,
                flags,
            });
        }
        out
    }

    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()> {
        anyhow::ensure!(!self.active, "VST3 plugin already active");
        // (r.md #5 ARA2) remember params so ARA setup can deactivate → bind →
        // reactivate (ARA binding must precede setActive).
        self.last_activate = Some((sample_rate, min_frames, max_frames));

        // 1. Negotiate speaker arrangements for each bus (MVP: stereo).
        let stereo: SpeakerArrangement = SpeakerArr::kStereo;
        // VST3 spec: setBusArrangements は **全 audio bus について** 1 つずつ
        // SpeakerArrangement を渡す必要がある。 旧実装は「main bus 1 個だけに
        // stereo」 を渡していたため、 multi-bus plugin (例: MeldaProduction
        // MSoundFactory は main + sidechain + sub) で arrangement 不整合に
        // なり、 plugin が処理を停止していた。
        //
        // 各 bus の channel count を `getBusInfo` で query → mono なら
        // kMono、 2 なら kStereo、 4 以上なら kStereoSurround (落とせる範囲で
        // 近似)。 plugin が拒否 (kResultFalse) した場合はそのまま続行 — 多くの
        // plugin は内部で best-effort fallback する。
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
        // negotiated arrangement を確認 (plugin によっては request と異なる
        // arrangement を採用する)。
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
        // PR4 sidechain: allocate planar buffers + ptr scratch for each
        // aux input bus. Mirrors the main input bus allocation above.
        self.aux_input_buffers = self
            .aux_input_channels
            .iter()
            .map(|&ch| {
                (0..ch as usize)
                    .map(|_| vec![0.0f32; max_frames as usize])
                    .collect()
            })
            .collect();
        self.aux_input_ptrs = self
            .aux_input_channels
            .iter()
            .map(|&ch| vec![std::ptr::null_mut(); ch as usize])
            .collect();
        // Phase 6 review (RT 安全): process() で毎 buffer 確保していた
        // Vec<AudioBusBuffers> を pre-allocate。 capacity = 1 (main) +
        // aux bus 数。 process() 入口で clear() + push() で reuse する。
        self.process_input_bufs =
            Vec::with_capacity(1 + self.aux_input_channels.len());
        // Multi-output-bus support (Surge XT 等): scratch buffers for each
        // extra output bus + the process-time output `Vec<AudioBusBuffers>`
        // (main + extras). Mirrors the aux input allocation above.
        self.extra_output_buffers = self
            .extra_output_channels
            .iter()
            .map(|&ch| {
                (0..ch as usize)
                    .map(|_| vec![0.0f32; max_frames as usize])
                    .collect()
            })
            .collect();
        self.extra_output_ptrs = self
            .extra_output_channels
            .iter()
            .map(|&ch| vec![std::ptr::null_mut(); ch as usize])
            .collect();
        self.process_output_bufs =
            Vec::with_capacity(1 + self.extra_output_channels.len());
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
        self.extra_output_buffers.clear();
        self.extra_output_ptrs.clear();
        self.process_output_bufs.clear();
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
        // Drain the RT-set overflow flag here (off the hot path). Swap-and-test
        // so the warning fires at most once per overflow episode.
        if self.param_pool_overflowed.swap(false, Ordering::Relaxed) {
            tracing::warn!(
                plugin = %self.name,
                "VST3 param changes pool overflow (>64 distinct params/buffer); extra dropped"
            );
        }
        #[cfg(debug_assertions)]
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
        // Multi-output-bus: refresh extra output bus channel pointers so the
        // process-time `AudioBusBuffers` see the latest base pointers.
        for bus_idx in 0..self.extra_output_buffers.len() {
            for ch in 0..self.extra_output_buffers[bus_idx].len() {
                self.extra_output_ptrs[bus_idx][ch] =
                    self.extra_output_buffers[bus_idx][ch].as_mut_ptr();
            }
        }
        // PR4 sidechain: copy aux input audio into our pre-allocated
        // planar bus buffers, mirroring the main bus copy above. Each
        // aux bus channel pointer is refreshed here so AudioBusBuffers
        // sees the latest base pointers.
        for (bus_idx, bus_bufs) in self.aux_input_buffers.iter_mut().enumerate() {
            let aux = aux_inputs.get(bus_idx).copied();
            for (ch, buf) in bus_bufs.iter_mut().enumerate() {
                let cap = n.min(buf.len());
                let src: &[f32] = match (aux, ch) {
                    (Some(a), 0) if a.active => a.l,
                    (Some(a), 1) if a.active => a.r,
                    _ => &[],
                };
                let copy_n = cap.min(src.len());
                buf[..copy_n].copy_from_slice(&src[..copy_n]);
                if copy_n < cap {
                    buf[copy_n..cap].fill(0.0);
                }
            }
            for (ch, ptrs) in self.aux_input_ptrs[bus_idx].iter_mut().enumerate() {
                *ptrs = bus_bufs[ch].as_mut_ptr();
            }
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

        // Phase: automation lane の値 (`TimedParamEvent`) を reusable
        // `IParameterChanges` に詰める。 daw_audio が param lane を評価して
        // ProcessData.events に積み、 process_server が `param_events` として
        // 渡してくる。 VST3 の値は normalized [0,1] (= daw_gui の VST3 param
        // automation も normalized で持つ、 enumerate_params が min0/max1 で
        // 報告するため整合)。
        //
        // `docs/plan_modulation_routing_redesign.md` §3.3: VST3 にはモジュレー
        // ションチャネルが無いので、`ParamEventKind::Mod` のオフセットを host が
        // `base + offset` に畳んで絶対値として送る (VST3 は normalized 0..=1 なので
        // `amount == offset`)。base は last-set 値キャッシュ (`param_mod_base`)。
        use crate::plugin_instance::ParamEventKind;
        // Pre-pass: refresh base cache from this buffer's absolute Value events
        // (unstable time sort ⇒ update before reading).
        for ev in param_events {
            if ev.kind == ParamEventKind::Value
                && let Some(slot) = self.param_mod_base.get_mut(&ev.param_id)
            {
                *slot = ev.value;
            }
        }
        self.folded_param_events.clear();
        for ev in param_events {
            match ev.kind {
                ParamEventKind::Value => self.folded_param_events.push(*ev),
                ParamEventKind::Mod => {
                    let base = self.param_mod_base.get(&ev.param_id).copied().unwrap_or(0.0);
                    let value = (base + ev.value).clamp(0.0, 1.0);
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
            // RT path: just raise a flag. The actual log (which formats
            // `%self.name`) happens once in `stop_processing()`, off the RT
            // thread, to keep `process()` free of heap allocation and tracing.
            self.param_pool_overflowed.store(true, Ordering::Relaxed);
        }

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
        let in_param_changes_ptr = self
            .in_param_changes
            .to_com_ptr::<IParameterChanges>()
            .context("Vst3InParamChanges has no IParameterChanges")?;

        // --- Assemble AudioBusBuffers (main + aux inputs).
        // Phase 6 review (RT 安全): 毎 buffer `Vec::with_capacity` → drop
        // していた hot path alloc を pre-allocated field の clear/push reuse
        // に置換。 CLAP 側の `ClapPlugin::process_input_bufs` と同 pattern。
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
        // Assemble output AudioBusBuffers (main + extras). VST3 requires
        // `numOutputs` to equal the plugin's output bus count; bus 0 is the
        // real main output we read back, buses 1..N are scratch so multi-
        // output synths (Surge XT) honour the main bus instead of mismatching.
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

        // Phase 7 B1-T (2026-05-13): per-buffer transport snapshot を VST3
        // `ProcessContext` に populate。 CLAP `build_clap_transport_event`
        // と同 semantics (= tempo / time_sig / bar_position / cycle 範囲 /
        // playing flag / project time の VST3 版)。 旧来は
        // `projectTimeSamples` を free-running で `+= frames` するだけだった
        // ため、 host の実 playhead と同期せず tempo-sync 系 VST3 plugin
        // (delay / arp / LFO) が host テンポを追随できなかった問題を解消。
        // VST3 spec: ProcessContext は `processContext` field 経由で plugin
        // に届き、 plugin は `state` flag を見て個別 field の有効性を判定する
        // (= flag の立っていない field は plugin 側で無視)。 musical time は
        // 拍 (= TQuarterNotes f64) 単位、 sample time は absolute samples。
        let bpm_f = f64::from(transport.bpm.max(1.0));
        let tsig_num_f = f64::from(transport.tsig_num.max(1));
        // beats は daw_audio が tempo automation を積分した真の拍位置を使う
        // (= CLAP `build_clap_transport_event` と同 SSoT、 一定テンポ逆算廃止)。
        let song_pos_beats = transport.song_pos_beats;
        let bar_number = (song_pos_beats / tsig_num_f).floor();
        let bar_start_beats = bar_number * tsig_num_f;
        self.process_context.sampleRate = self.sample_rate;
        self.process_context.tempo = bpm_f;
        self.process_context.timeSigNumerator =
            i32::from(transport.tsig_num.max(1));
        self.process_context.timeSigDenominator =
            i32::from(transport.tsig_denom.max(1));
        // VST3 spec: `TSamples` は i64 absolute sample position。
        // playhead_samples は u64 だが通常使用範囲では i64 に収まる
        // (= 2^63-1 sample @ 96 kHz で約 3 千年)。 saturating で防御。
        let playhead_i64 = i64::try_from(transport.playhead_samples)
            .unwrap_or(i64::MAX);
        self.process_context.projectTimeSamples = playhead_i64;
        self.process_context.continousTimeSamples = playhead_i64;
        self.process_context.projectTimeMusic = song_pos_beats;
        self.process_context.barPositionMusic = bar_start_beats;
        self.process_context.cycleStartMusic = transport.loop_start_beats;
        self.process_context.cycleEndMusic = transport.loop_end_beats;
        // State flags: kTempoValid / kTimeSigValid / kProjectTimeMusicValid /
        // kBarPositionValid / kCycleValid は常時 valid。 kPlaying /
        // kCycleActive のみ transport 状態依存で動的設定。
        let mut state = (StatesAndFlags_::kTempoValid
            | StatesAndFlags_::kTimeSigValid
            | StatesAndFlags_::kProjectTimeMusicValid
            | StatesAndFlags_::kBarPositionValid
            | StatesAndFlags_::kCycleValid
            | StatesAndFlags_::kContTimeValid) as u32;
        if transport.is_playing {
            state |= StatesAndFlags_::kPlaying as u32;
        }
        if transport.is_looping
            && transport.loop_end_beats > transport.loop_start_beats
        {
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
            // Phase 7 B1-R (2026-05-13): per-buffer の processMode は
            // `set_render_mode` で更新される `self.render_mode` から引く
            // (= export 中は kOffline、 通常再生は kRealtime)。 詳細は
            // `set_render_mode` の comment 参照。
            processMode: match self.render_mode {
                RenderMode::Realtime => ProcessModes_::kRealtime,
                RenderMode::Offline => ProcessModes_::kOffline,
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
            // automation 入力 (host → plugin)。 borrowed ptr: ComWrapper が
            // 自前 ref を保持するので in_param_changes_ptr drop 後も生存
            // (in_list_ptr と同 idiom)。
            inputParameterChanges: in_param_changes_ptr.as_ptr(),
            // output param changes (plugin → host の param 自動化書き戻し) は
            // 現状未使用。 GUI gesture は IComponentHandler 経由で別途取得する。
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
            // Record (don't log) here: `format!` would heap-allocate on the RT
            // thread and an unhappy plugin returns non-OK every buffer. The
            // actual warning fires once from `stop_processing()`.
            self.process_status_err.store(status, Ordering::Relaxed);
        }

        // Drain collected events before the ComPtrs drop (order doesn't
        // strictly matter, but keeps the reader's mental model simple).
        self.out_event_list.drain_into(&mut self.collected_out_notes);

        Ok(status)
    }

    fn output_buffer(&self, channel: usize) -> Option<&[f32]> {
        self.output_buffers.get(channel).map(|v| v.as_slice())
    }

    /// パラアウト (`docs/plan_paraout.md`): expose every output bus as a
    /// parallel-out port. **Port 0 is the MAIN output bus** (`output_buffers`,
    /// the first "part"); ports `1..` are the extra (non-main) buses
    /// (`extra_output_buffers`), which the plugin already writes every
    /// `process()` (we used to discard them). A multi-out drum like MDrummer
    /// puts each part on its own bus, main included — so "explode" can split
    /// all of them into child tracks. Symmetric to `ClapPlugin::aux_output_buffer`.
    fn aux_output_buffer(&self, port: usize, channel: usize) -> Option<&[f32]> {
        // Single-output plugins have no parallel-out ports (port 0 = main is
        // only exposed for splitting when there's ≥1 extra bus). Skip so the
        // process server doesn't needlessly copy main into buffer_aux_out[0].
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

    /// パラアウト: number of parallel-out ports = `1 (main) + extra bus count`,
    /// capped at `MAX_AUX_OUT`. Only multi-output plugins (≥1 extra bus) get
    /// paraout; a single-output plugin reports 0 (no "explode"). MDrummer has
    /// 15 extra buses → 16 ports (main + 15). Reported to the GUI.
    fn aux_output_port_count(&self) -> usize {
        let extra = self.extra_output_channels.len();
        if extra == 0 {
            0
        } else {
            (1 + extra).min(common::process_data::MAX_AUX_OUT)
        }
    }

    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>) {
        out.append(&mut self.collected_out_notes);
    }

    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        // Phase 7 B1-R (2026-05-13): VST3 spec の `IComponent::setIoMode` は
        // `initialize` 前にしか呼べない (= 既に active な plugin への動的
        // 切替は spec 違反 + plugin 依存で動かない) ため、 spec 準拠の代替
        // として `ProcessData::processMode` を per-buffer で
        // `kRealtime` / `kOffline` に切り替える。 plugin が process() 毎の
        // processMode を尊重するか `setupProcessing` 時の値を固定するかは
        // plugin 実装依存だが、 多くの reverb / convolution / lookahead 系
        // plugin は process 毎を読んで「offline = 高品質 algo に切替」 等を
        // 判定する (= effect が plugin 依存で出る、 害は無い)。 daw_audio の
        // export 経路 (= freewheel offline render) で SetRenderMode IPC を
        // 送ると本 setter が呼ばれ、 次 process() から `processMode = kOffline`。
        self.render_mode = mode;
        tracing::info!(name = %self.name, ?mode, "VST3 render mode updated");
        true
    }

    fn query_latency(&mut self) -> u32 {
        // PR3.3: VST3 spec (`IAudioProcessor::getLatencySamples`):
        //   "Gets the current Latency in samples. ... if internally needs
        //    to look in advance (like compressors) 512 samples then this
        //    plug-in should report 512 as latency."
        // Thread requirement: `[UI-thread & Setup Done]` — host must call
        // it after `setupProcessing` completed. We invoke right after our
        // `activate()` ran `setupProcessing` + `setActive(true)`, so we're
        // safely past the Setup Done barrier.
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
        // HWND platform を plugin が受け入れるか確認 (false なら GUI embed
        // 不可、 plugin 側の標準 window で出す必要がある = MVP では未対応)。
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
        // MeldaProduction 等は ここで自身の preferred size を返すので、 0×0
        // ならば描画 surface 未初期化の signal。
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
        // PR-V2.4: VST3 NoteOn/Off の `noteId` 標準 field に `note_id` を
        // 詰める (= host から plugin へ note 識別子を伝搬)。 `i32` 型で
        // 0..i32::MAX に clamp、 越えたら `-1` (= "未指定") に fallback。
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
        // 「未指定」 = -1 → 0 に丸める (= MIDI FX が note_id を返さない
        // ケース)。
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
