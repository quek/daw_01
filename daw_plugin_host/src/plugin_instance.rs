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
use common::plugin_metadata::NoteMetadata;
use common::protocol::{PluginParamInfo, RenderMode};

use crate::builtin;
use crate::clap_plugin::ClapPlugin;
use crate::vst3_plugin::Vst3Plugin;

/// One MIDI-style transition pushed into the next `process()` call.
///
/// `note_id` (PR-V2.4) is the **stable per-note identifier** used to look
/// up `NoteMetadata` (= 歌詞 / phoneme) and per-note synthesis cache in
/// builtin plugins. CLAP / VST3 backends ignore the field — they map
/// `On { key }` to the legacy MIDI note pipeline. For audio events
/// emitted by `sequencer::collect_events_for_buffer`, the value is the
/// note's flattened index across all clips on the track (same numbering
/// `daw_gui::AppData::sync_vocal_metadata` uses on the host side).
#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { note_id: u32, key: u8, velocity: f64 },
    Off { note_id: u32, key: u8 },
}

/// A note transition scheduled at a specific frame offset inside the next
/// process buffer. The audio thread uses these to feed CLAP's input-event
/// vtable and VST3's `IEventList::addEvent` alike.
#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

/// Phase 2 (`docs/plan_automation.md` §8.3): plugin parameter automation
/// 用の 1 イベント。`time` は buffer 内 sample offset、`param_id` は
/// CLAP `clap_id` / VST3 `ParamID` (共に u32)、`value` は plain 単位
/// (= plugin の `min_value..=max_value` スケール)。 plugin host 内で
/// CLAP `clap_event_param_value` / VST3 `IParameterChanges` に変換。
#[derive(Debug, Clone, Copy)]
pub struct TimedParamEvent {
    pub time: u32,
    pub param_id: u32,
    pub value: f64,
}

/// PR4 sidechain: one aux input port worth of buffers handed to
/// `LoadedPlugin::process`. Mirrors `pd.buffer_aux_in[port]` /
/// `pd.aux_in_active[port]` from the shmem `ProcessData`. Stereo only
/// (CLAP / VST3 both expose stereo aux for the typical sidechain
/// compressor / gate / ducker workflows).
#[derive(Clone, Copy)]
pub struct AuxInputBuf<'a> {
    /// Whether the audio engine wrote real audio into `l` / `r` this
    /// buffer. Inactive ports are still passed (so plugin's port count
    /// stays stable across calls) but with silent slices, and CLAP
    /// backends are free to pass `data32: null` instead.
    pub active: bool,
    pub l: &'a [f32],
    pub r: &'a [f32],
}

/// Host callbacks plugins may trigger on *any* thread (usually the
/// plugin's GUI thread). Implementations must be `Send + Sync` and must
/// not block the caller — plugins often hold an internal lock across
/// these.
#[derive(Clone)]
pub struct HostCallbacks {
    pub on_request_resize: Arc<dyn Fn(u32, u32) + Send + Sync>,
    pub on_closed: Arc<dyn Fn() + Send + Sync>,
    /// VST3 only: plugin GUI で param を触り始めた (`IComponentHandler::
    /// beginEdit`)。 引数は param_id。 daw_gui の last-touched workflow
    /// (`A` キー) の起点。 CLAP は out_events 経由なのでこれを使わない
    /// (= CLAP plugin では呼ばれない)。
    pub on_param_gesture_begin: Arc<dyn Fn(u32) + Send + Sync>,
    /// VST3 only: plugin GUI で param 値が変わった (`IComponentHandler::
    /// performEdit`)。 引数は (param_id, normalized_value)。 daw_gui の
    /// plugin_param_values cache に積まれ、 automation lane の現在値 source
    /// になる。
    pub on_param_value: Arc<dyn Fn(u32, f64) + Send + Sync>,
    /// VST3 only: plugin GUI で param を離した (`IComponentHandler::
    /// endEdit`)。 引数は param_id。 gesture lifecycle を閉じる。
    pub on_param_gesture_end: Arc<dyn Fn(u32) + Send + Sync>,
}

impl HostCallbacks {
    #[allow(dead_code)]
    pub fn noop() -> Self {
        Self {
            on_request_resize: Arc::new(|_, _| {}),
            on_closed: Arc::new(|| {}),
            on_param_gesture_begin: Arc::new(|_| {}),
            on_param_value: Arc::new(|_, _| {}),
            on_param_gesture_end: Arc::new(|_| {}),
        }
    }
}

/// The host-side handle to a loaded plugin. Lives on the plugin-main
/// thread; `process()` / `start_processing()` / `stop_processing()` are
/// invoked from the audio thread via raw-pointer snapshots (see
/// `PluginPtr` in `main.rs`).
/// Phase 5 Step 5.3 (`docs/plan_automation.md` §10): per-buffer transport
/// snapshot fed into `LoadedPlugin::process` so CLAP backends can build
/// `clap_event_transport` and set `clap_process.transport`. VST3
/// backends consume the same fields via `IProcessContext`. VoicevoxBuiltin
/// / Silence backends ignore everything except `playhead_samples`
/// (= they already use steady_time for sample positioning).
///
/// `bpm` etc. are populated from `ProcessData.bpm` etc. which daw_audio
/// fills via `engine::set_pd_transport` at buffer head. The fields live
/// in `ProcessData` (= shmem-portable) so the plugin host only needs to
/// repackage them into the CLAP / VST3 transport struct.
#[derive(Debug, Clone, Copy)]
pub struct TransportContext {
    pub bpm: f32,
    pub sample_rate: u32,
    /// Sample-domain playhead. Convertible to `song_pos_seconds` via
    /// `playhead_samples / sample_rate` and to `song_pos_beats` via
    /// `playhead_samples * bpm / (60 * sample_rate)`.
    pub playhead_samples: u64,
    /// 累積拍位置 (= daw_audio が tempo automation を積分した真の song 位置)。
    /// `playhead_samples × bpm / (60 × SR)` の一定テンポ逆算ではなくこの値を
    /// CLAP `song_pos_beats` / VST3 `projectTimeMusic` に直接使う。これで
    /// テンポオートメーション中も plugin の tempo-sync が正しい拍に追従する。
    pub song_pos_beats: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub is_playing: bool,
    pub is_looping: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

impl TransportContext {
    /// Phase 5 Step 5.3: build from a `ProcessData` populated by daw_audio.
    /// `pd.playing` u8 → bool conversion + clamp `bpm` to a sane minimum
    /// (= 1.0) for divide safety in downstream computations.
    pub fn from_process_data(pd: &common::process_data::ProcessData) -> Self {
        Self {
            bpm: pd.bpm.max(1.0),
            sample_rate: pd.sample_rate.max(1),
            playhead_samples: pd.steady_time,
            song_pos_beats: pd.song_pos_beats,
            tsig_num: pd.tsig_num.max(1),
            tsig_denom: pd.tsig_denom.max(1),
            is_playing: pd.playing != 0,
            is_looping: pd.looping != 0,
            loop_start_beats: pd.loop_start_beats,
            loop_end_beats: pd.loop_end_beats,
        }
    }
}

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
    /// `aux_inputs` carries PR4 sidechain audio: one entry per `is_main=false`
    /// input port the plugin declared, in declaration order. `active=false`
    /// means the host has no source wired to this aux port — backends pass
    /// silence (or null `data32` for CLAP) so the plugin observes silence.
    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        param_events: &[TimedParamEvent],
        input_audio: &[&[f32]],
        aux_inputs: &[AuxInputBuf<'_>],
        transport: &TransportContext,
    ) -> Result<i32>;
    /// Planar output. `None` means "no such channel" (e.g. mono plugin
    /// queried for channel 1).
    fn output_buffer(&self, channel: usize) -> Option<&[f32]>;
    /// Moves MIDI-style events emitted during the previous `process()`
    /// into `out`, draining the plugin's buffer in place (pre-allocated
    /// capacity preserved).
    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>);

    /// Phase 2c (`docs/plan_automation.md` §7.5): drain plugin-emitted
    /// PARAM_GESTURE_BEGIN events from the last `process()` call.
    /// Default no-op for backends that don't emit them (Silence /
    /// VoicevoxBuiltin / VST3 backend until Phase 3+).
    fn drain_out_param_touches_into(&mut self, _out: &mut Vec<u32>) {}

    /// Phase 2c: drain plugin-emitted PARAM_VALUE events (= plugin GUI
    /// knob value changes). Default no-op.
    fn drain_out_param_values_into(&mut self, _out: &mut Vec<(u32, f64)>) {}

    /// Phase 4 Step C-3: drain plugin-emitted PARAM_GESTURE_END events
    /// (= plugin GUI knob release)。 Default no-op (= Silence /
    /// VoicevoxBuiltin / VST3 はまだ対応していない)。
    fn drain_out_param_releases_into(&mut self, _out: &mut Vec<u32>) {}

    // --- render-mode hint (CLAP `render` ext) ---------------------------
    /// Tell the plugin whether the next `process()` calls are realtime
    /// or offline (during WAV export). Returns `true` if the plugin
    /// accepted the change. CLAP plugins forward to
    /// `clap_plugin_render.set`; backends without the extension return
    /// `false` and continue at whatever mode they were already in.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool;

    /// PR3.3 PDC: query the plugin's reported processing latency in
    /// samples (host sample_rate). Called once right after `activate()`
    /// succeeds (CLAP spec: `clap_plugin_latency.get` requires
    /// `[main-thread & active]`; VST3 spec: `IAudioProcessor::
    /// getLatencySamples` requires Setup Done state). Backends without
    /// the extension or that don't expose latency return 0.
    fn query_latency(&mut self) -> u32;

    /// Phase 2 (`daw_01/docs/plan_automation.md` §7.5): enumerate every
    /// parameter the plugin exposes. Called from the plugin-main thread
    /// once right after `activate()` succeeds. CLAP / VST3 backends walk
    /// `clap_plugin_params.{count,get_info}` / `IEditController::
    /// {getParameterCount, getParameterInfo}` respectively. Default impl
    /// returns empty (= builtin plugins / Silence). Names that don't fit
    /// in `String::from_utf8_lossy` are truncated at the first invalid
    /// byte.
    fn enumerate_params(&self) -> Vec<PluginParamInfo> {
        Vec::new()
    }

    // --- persistence (plugin-main thread) -------------------------------
    fn state_save(&self) -> Result<Option<Vec<u8>>>;
    /// PR-V2.5 で `&mut self` 化。 builtin plugin が parameter を実際に
    /// 復元できるようにするための変更。 既存 CLAP / VST3 backend は内部
    /// で `&self` ベース API (interior mutability) に forward していた
    /// だけなので、 signature 変更だけで動作不変。
    fn state_load(&mut self, data: &[u8]) -> Result<()>;

    // --- per-note metadata (Builtin plugin only, PR-V2.2 / V2.3) --------
    /// Builtin plugin (`PluginFormat::Builtin`) 専用の per-note metadata
    /// flush。 CLAP / VST3 plugin は default no-op (= 規格に存在しない
    /// 概念なので)、 builtin plugin は `entries` を内部にバッファして
    /// 次の synthesis pass で参照する。
    ///
    /// 呼び出しは plugin-main thread から、 GUI 側で歌詞 / phoneme が
    /// 編集されるたびに実施。 `entries` は `note_id` ascending に並んで
    /// いる必要は無い (= builtin が必要なら自分でソートする)。
    /// `bpm` は note の `start_beat` を frames に変換するときに使う
    /// (= VOICEVOX `singing_query` のフレーム計算)。 song の bpm 変更時
    /// にも flush される。
    /// `docs/plan_voicevox_synth.md` PR-V2.2 / V2.3 で導入。
    fn set_note_metadata(&mut self, _bpm: f32, _entries: &[NoteMetadata]) {}

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
        PluginFormat::Builtin => {
            // `path` here is a `builtin://...` URI (see plugin_format::
            // PluginFormat docs). `plugin_id` is unused for builtins —
            // the URI itself is the descriptor id, mirroring CLAP's
            // single-id-per-descriptor convention. Some upstream
            // call-sites may still pass a non-empty `plugin_id` (= the
            // database entry's `id` field, which equals the URI); we
            // simply ignore it. Future builtins with multiple
            // descriptors per URI can switch to `plugin_id`-based
            // dispatch without changing the protocol.
            let _ = plugin_id;
            let uri = path.to_string_lossy();
            builtin::load_builtin(&uri, callbacks)
        }
    }
}
