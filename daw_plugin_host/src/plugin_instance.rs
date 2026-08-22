//! Format-agnostic plugin interfaces (`docs/plan_arch_refactor.md` §6).
//!
//! Split-half design (clack 方式): 1 つのロード済み plugin は 2 つの Rust
//! オブジェクトで表現される。
//!
//! - [`LoadedPlugin`] — **main half**。plugin-main thread が所有する
//!   `Box<dyn LoadedPlugin>`。lifecycle (activate / deactivate /
//!   start・stop_processing)、state save/load、GUI、ARA、param 列挙など
//!   main-thread API を持つ。
//! - [`AudioProcessorHalf`] — **audio half**。`process()` が触る状態
//!   (入出力 planar buffer、event scratch、param cache、collected out
//!   events) だけを持つ別 heap allocation。worker pool の registry には
//!   こちら ([`AudioHalf`] = `Arc<UnsafeCell<Box<dyn AudioProcessorHalf>>>`
//!   相当) を渡す。
//!
//! これにより「worker が `&mut *raw` で process() 実行中に、plugin-main が
//! 同一オブジェクトへ `&mut` / `&` を発行する」旧構造の aliasing UB が型で
//! 消える: 並行に走り得る main-thread 呼び出し (state_save / ARA notify /
//! GUI / set_note_metadata) は main half のフィールドしか触らず、worker は
//! audio half のフィールドしか触らない。両 half は生の FFI ポインタ
//! (CLAP `*const clap_plugin` / VST3 `ComPtr`) を共有するが、その先の状態は
//! CLAP / VST3 仕様の thread partitioning (main-thread API vs audio-thread
//! API) が分離を保証する (Rust の aliasing model の外)。
//!
//! # `AudioHalf` の動的排他契約
//!
//! audio half への `&mut` は 2 経路からしか作られない:
//!
//! 1. **worker** — registry で entry を resolve した dispatch-critical
//!    section 内 (`DispatchCounter::enter`/`exit` で囲まれる)。
//! 2. **plugin-main** — その entry を registry から外し
//!    (`registry_remove`) `WorkerPool::quiesce` を済ませた *quiesced
//!    window* 内 (activate のバッファ再確保 / start・stop の gate 更新)。
//!
//! 両者は quiesce プロトコルで動的に直列化される (process_server.rs の
//! module docs 参照)。`AudioHalf::get` はこの契約を `unsafe fn` の
//! Safety 節として要求する。

use std::cell::UnsafeCell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::Result;

use common::plugin_format::PluginFormat;
use common::plugin_metadata::{NoteMetadata, TalkMetadata};
use common::protocol::{PluginParamInfo, RenderMode};

use crate::builtin;
use crate::clap_plugin::ClapPlugin;
use crate::vst3_plugin::Vst3Plugin;

/// One MIDI-style transition pushed into the next `process()` call.
///
/// `note_id` is the **stable per-note identifier** used to look up
/// `NoteMetadata` (= 歌詞 / phoneme) and per-note synthesis cache in
/// builtin plugins. CLAP / VST3 backends map it onto the formats' note-id
/// fields.
#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { note_id: u32, key: u8, velocity: f64 },
    Off { note_id: u32, key: u8 },
}

/// A note transition scheduled at a specific frame offset inside the next
/// process buffer.
#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

/// Whether a [`TimedParamEvent`] carries an absolute value or a normalized
/// modulation offset (`docs/plan_modulation_routing_redesign.md` §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParamEventKind {
    /// Absolute parameter value (automation / direct set).
    #[default]
    Value,
    /// Normalized (`-1..=1`) modulation offset. CLAP modulatable params get a
    /// non-destructive `clap_event_param_mod`; VST3 / non-modulatable params
    /// have it folded into the absolute value the host sends.
    Mod,
}

/// One plugin-parameter automation event. `time` is the buffer-relative
/// sample offset, `param_id` is CLAP `clap_id` / VST3 `ParamID` (both u32).
#[derive(Debug, Clone, Copy)]
pub struct TimedParamEvent {
    pub time: u32,
    pub param_id: u32,
    pub value: f64,
    pub kind: ParamEventKind,
}

/// PR4 sidechain: one aux input port worth of buffers handed to
/// [`AudioProcessorHalf::process`]. Stereo only.
#[derive(Clone, Copy)]
pub struct AuxInputBuf<'a> {
    /// Whether the audio engine wrote real audio into `l` / `r` this buffer.
    pub active: bool,
    pub l: &'a [f32],
    pub r: &'a [f32],
}

/// Host callbacks plugins may trigger on *any* thread (usually the
/// plugin's GUI thread). Implementations must be `Send + Sync` and must
/// not block the caller — plugins often hold an internal lock across these.
///
/// v29: すべての callback は load 時に **安定 device id** を capture した
/// closure として `main.rs::make_callbacks(device_id)` が生成する。旧
/// `(track, index)` capture は削除 / 並べ替えで stale になり「別デバイスの
/// GUI を destroy する」class のバグ源だった。
#[derive(Clone)]
pub struct HostCallbacks {
    pub on_request_resize: Arc<dyn Fn(u32, u32) + Send + Sync>,
    pub on_closed: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host_gui.request_show` / `request_hide`.
    pub on_request_show: Arc<dyn Fn() + Send + Sync>,
    pub on_request_hide: Arc<dyn Fn() + Send + Sync>,
    /// VST3 `IComponentHandler::restartComponent(flags)`.
    pub on_restart_component: Arc<dyn Fn(i32) + Send + Sync>,
    /// CLAP `clap_host.request_restart` — deactivate → activate の全 reinit
    /// 要求。plugin-main の quiesced-reinit 経路 (per-plugin cooldown 付き)
    /// へ配線される。
    pub on_request_restart: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host.request_callback` — plugin-main thread で
    /// [`LoadedPlugin::on_main_thread`] を 1 回呼ぶ要求 (JUCE 系の
    /// main-thread task 駆動)。
    pub on_request_callback: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host_latency.changed` — latency 再 query +
    /// `PluginLatencyChanged` 再 emit の要求。
    pub on_latency_changed: Arc<dyn Fn() + Send + Sync>,
    /// CLAP `clap_host_params.rescan` — param 一覧再送 (`PluginParamList`)
    /// の要求。
    pub on_params_rescan: Arc<dyn Fn() + Send + Sync>,
    /// VST3 only: plugin GUI param gesture begin (`beginEdit`).
    pub on_param_gesture_begin: Arc<dyn Fn(u32) + Send + Sync>,
    /// VST3 only: plugin GUI param value change (`performEdit`).
    pub on_param_value: Arc<dyn Fn(u32, f64) + Send + Sync>,
    /// VST3 only: plugin GUI param gesture end (`endEdit`).
    pub on_param_gesture_end: Arc<dyn Fn(u32) + Send + Sync>,
    /// builtin VOICEVOX の合成状態 `(busy, failure)` 報告。旧「第 2 の
    /// callback 登録機構」(`set_voicevox_status_reporter`) を廃止して
    /// ここに統合 (`docs/plan_arch_refactor.md` §6)。synth thread が任意
    /// スレッドから呼ぶ。`failure` は engine 到達可否を区別する
    /// (`common::protocol::VocalSynthFailure`)。
    pub on_vocal_synth_status:
        Arc<dyn Fn(bool, common::protocol::VocalSynthFailure) + Send + Sync>,
    /// r.md #65: このインスタンスのエディタ**コンテナ窓の HWND** (`0` = 未 open)。
    ///
    /// **`on_request_resize` (非同期 channel) では VST3 の契約を満たせない**ので置く。
    /// `iplugview.h` の "Sizing of a view" は「`IPlugFrame::resizeView` の後、
    /// **同じコールスタックで** ホストが窓をリサイズして `IPlugView::onSize` を呼ぶ」
    /// と規定していて、次周回に回すと `getSize` が旧サイズを返し続ける。実測では
    /// Renoise Redux がこれを見て **自分の view をコンテナから切り離し WS_POPUP の
    /// owned top-level に変える** (2026-08-22、`--editor-selftest` で確認)。
    ///
    /// 書き込むのは `gui_set_parent_hwnd` / `gui_destroy` の 1 対だけ (= 「今どの窓に
    /// attach しているか」がそのまま値になる = SSoT)。読むのは `Vst3PlugFrame` /
    /// CLAP `Host` の resize callback。窓の所有は plugin-main の `EditorWindow` のまま。
    pub editor_hwnd: Arc<AtomicU64>,
}

/// CLAP `clap_gui_resize_hints` 相当 (`clap/ext/gui.h` L91-103)。VST3 に対応 API は
/// 無いので `None` を返す。
///
/// `preserve_aspect_ratio` は **両軸ともリサイズ可のときだけ**意味を持ち、`false` の
/// ときは 2 つの ratio 値は未使用 (= 読んではいけない) — ヘッダのコメントどおり。
#[derive(Debug, Clone, Copy)]
pub struct ResizeHints {
    pub can_resize_horizontally: bool,
    pub can_resize_vertically: bool,
    pub preserve_aspect_ratio: bool,
    pub aspect_ratio_width: u32,
    pub aspect_ratio_height: u32,
}

/// エディタ窓の WNDPROC が **同じコールスタックで** プラグインへ問い合わせる口
/// (r.md #65)。
///
/// なぜ必要か: ホスト起点 (ユーザーのドラッグ) もプラグイン起点 (`resizeView` /
/// `request_resize`) も、両フォーマットが **同期**のシーケンスを規定している
/// (`iplugview.h` "Sizing of a view" / `clap/ext/gui.h` L35-45)。窓メッセージを
/// channel 経由で plugin-main の次周回へ回すと live resize が 1 周期遅れ、
/// modal size ループ中は永久に遅れる。
///
/// # 所有と生存
///
/// 実装は **borrowed な FFI ポインタしか持たない**。view / plugin instance の所有は
/// [`LoadedPlugin`] 側にあり (SSoT — `ComPtr` を二重に AddRef して WNDPROC 側にも
/// 持たせると `gui_destroy` の `removed()` と競合して UAF になる)。
/// `gui_destroy` は **先頭で** `alive` を落とす契約で、以後 [`Self::is_alive`] が
/// `false` を返し WNDPROC は一切 FFI を呼ばない。`pump_pending_messages` 由来の
/// nested dispatch で `gui_destroy` 実行中に WM_SIZE が再入しても、この 1 点で塞がる。
pub trait EditorSizer: Send {
    /// ユーザーのドラッグ矩形 (client px) を、プラグインが受け入れるサイズへ矯正する。
    /// VST3 `IPlugView::checkSizeConstraint` / CLAP `clap_plugin_gui.adjust_size`。
    /// 矯正できなければ入力をそのまま返す。**ホスト起点ドラッグ専用** —
    /// プラグイン起点の resize には掛けない (どちらのシーケンス図にも出てこない)。
    fn constrain_client_size(&self, w: u32, h: u32) -> (u32, u32);
    /// プラグインが今表示している client サイズ (VST3 `getSize` / CLAP `get_size`)。
    fn current_client_size(&self) -> Option<(u32, u32)>;
    /// 確定した client サイズを通知する (VST3 `onSize` / CLAP `set_size`)。
    fn notify_client_size(&self, w: u32, h: u32);
    /// ユーザーが窓枠でリサイズしてよいか (VST3 `canResize` / CLAP `can_resize`)。
    fn can_resize(&self) -> bool;
    /// CLAP `get_resize_hints` 相当。VST3 は `None`。
    fn resize_hints(&self) -> Option<ResizeHints>;
    /// `gui_destroy` 済みなら `false`。`false` の間は他のメソッドを呼んではいけない。
    fn is_alive(&self) -> bool;
}

impl HostCallbacks {
    pub fn noop() -> Self {
        Self {
            on_request_resize: Arc::new(|_, _| {}),
            on_closed: Arc::new(|| {}),
            on_request_show: Arc::new(|| {}),
            on_request_hide: Arc::new(|| {}),
            on_restart_component: Arc::new(|_| {}),
            on_request_restart: Arc::new(|| {}),
            on_request_callback: Arc::new(|| {}),
            on_latency_changed: Arc::new(|| {}),
            on_params_rescan: Arc::new(|| {}),
            on_param_gesture_begin: Arc::new(|_| {}),
            on_param_value: Arc::new(|_, _| {}),
            on_param_gesture_end: Arc::new(|_| {}),
            on_vocal_synth_status: Arc::new(|_, _| {}),
            editor_hwnd: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Per-buffer transport snapshot fed into [`AudioProcessorHalf::process`].
///
/// 真の再生位置は `song_pos_beats` 一本 (= daw_audio が tempo automation を
/// 積分した累積拍位置)。sample / seconds / bar 表現は
/// [`crate::process_scaffold::TransportBlock`] がここから導出する
/// (`ProcessData::steady_time` は engine が設定しない = 常に 0 なので
/// sample 由来の位置は運ばない)。
#[derive(Debug, Clone, Copy)]
pub struct TransportContext {
    pub bpm: f32,
    pub sample_rate: u32,
    /// 累積拍位置 (= daw_audio が tempo automation を積分した真の song 位置)。
    pub song_pos_beats: f64,
    pub tsig_num: u16,
    pub tsig_denom: u16,
    pub is_playing: bool,
    pub is_looping: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

impl TransportContext {
    /// Build from a `ProcessData` populated by daw_audio.
    pub fn from_process_data(pd: &common::process_data::ProcessData) -> Self {
        Self {
            bpm: pd.bpm.max(1.0),
            sample_rate: pd.sample_rate.max(1),
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

// ====================================================================
// Audio half
// ====================================================================

/// Audio-thread half of a loaded plugin: **`process()` が触る状態の全て**
/// (入出力 planar buffer / event scratch / param cache / collected out
/// events)。worker registry にはこの trait object ([`AudioHalf`] 経由) を
/// 渡す。main half ([`LoadedPlugin`]) からは lifecycle hook (`on_activate`
/// / `on_deactivate` / `set_processing`) のみ、**quiesced window 内で**
/// 呼ばれる。
pub trait AudioProcessorHalf: Send {
    /// Runs one buffer. `events` / `param_events` must be sorted by
    /// ascending `time` (CLAP requirement, also honoured for VST3).
    fn process(
        &mut self,
        frames: u32,
        events: &[TimedNoteEvent],
        param_events: &[TimedParamEvent],
        input_audio: &[&[f32]],
        aux_inputs: &[AuxInputBuf<'_>],
        transport: &TransportContext,
    ) -> Result<i32>;

    /// Planar output. `None` means "no such channel".
    fn output_buffer(&self, channel: usize) -> Option<&[f32]>;

    /// パラアウト: planar aux output (port 0 = main bus when the plugin is
    /// multi-out). `None` = no such port / channel.
    fn aux_output_buffer(&self, _port: usize, _channel: usize) -> Option<&[f32]> {
        None
    }

    /// Moves MIDI-style events emitted during the previous `process()` into
    /// `out` (capacity-preserving drain).
    fn drain_out_notes_into(&mut self, out: &mut Vec<TimedNoteEvent>);

    /// Drain plugin-emitted PARAM_GESTURE_BEGIN param ids. Default no-op.
    fn drain_out_param_touches_into(&mut self, _out: &mut Vec<u32>) {}

    /// Drain plugin-emitted PARAM_VALUE `(param_id, value)`. Default no-op.
    fn drain_out_param_values_into(&mut self, _out: &mut Vec<(u32, f64)>) {}

    /// Drain plugin-emitted PARAM_GESTURE_END param ids. Default no-op.
    fn drain_out_param_releases_into(&mut self, _out: &mut Vec<u32>) {}

    // --- lifecycle hooks (plugin-main thread, quiesced window のみ) ------

    /// (Re)allocate the process buffers for the new activation params.
    /// Called by the main half right after the format-level activate
    /// succeeded, inside a quiesced window.
    fn on_activate(&mut self, _sample_rate: f64, _max_frames: u32) {}

    /// Free / clear activation-scoped buffers. Quiesced window のみ。
    fn on_deactivate(&mut self) {}

    /// Mirror of the main half's processing gate (defensive check inside
    /// `process()`). Quiesced window のみ。
    fn set_processing(&mut self, _on: bool) {}
}

/// Shared cell that owns the [`AudioProcessorHalf`] allocation. The worker
/// registry and the main half ([`LoadedPlugin::audio_half`]) each hold an
/// `Arc`; the allocation therefore outlives any stale registry snapshot
/// (no dangling `Box` reads), while *access* is serialized dynamically by
/// the quiesce protocol (module docs).
pub struct AudioHalf {
    inner: UnsafeCell<Box<dyn AudioProcessorHalf>>,
}

// SAFETY: the inner Box is only dereferenced through `get()`, whose
// contract requires dynamically exclusive access (registry dispatch window
// XOR quiesced window). `dyn AudioProcessorHalf` is `Send`.
unsafe impl Send for AudioHalf {}
unsafe impl Sync for AudioHalf {}

impl AudioHalf {
    pub fn new(inner: Box<dyn AudioProcessorHalf>) -> Arc<Self> {
        Arc::new(Self {
            inner: UnsafeCell::new(inner),
        })
    }

    /// # Safety
    ///
    /// The caller must hold dynamically exclusive access per the module-doc
    /// contract: either (a) a worker inside its dispatch-critical section
    /// with this entry resolved from the *current* registry snapshot, or
    /// (b) the plugin-main thread inside a quiesced window (entry removed
    /// from the registry + `WorkerPool::quiesce` completed) or before the
    /// entry was ever published.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get(&self) -> &mut (dyn AudioProcessorHalf + 'static) {
        unsafe { &mut **self.inner.get() }
    }
}

// ====================================================================
// VOICEVOX capability (docs/plan_arch_refactor.md §6)
// ====================================================================

/// Vocal-synthesis capability implemented by builtin VOICEVOX. External
/// CLAP / VST3 plugins have no equivalent concept, so the capability is an
/// opt-in downcast ([`LoadedPlugin::as_vocal_synth`]) instead of a set of
/// default-no-op methods on every plugin.
pub trait VocalSynth {
    /// Per-note metadata flush (歌詞 + talk)。plugin-main thread から、GUI
    /// 側で歌詞 / phoneme が編集されるたびに呼ばれる。
    fn set_note_metadata(&mut self, bpm: f32, entries: &[NoteMetadata], talk: &[TalkMetadata]);

    /// `(queued_gen, done_gen)` 世代カウンタ。歌唱 bounce 前の合成完了待ち
    /// (`PrepareVocalSynth`) が `done >= queued` を poll する。
    fn synth_progress(&self) -> (Arc<AtomicU64>, Arc<AtomicU64>);
}

// ====================================================================
// Main half
// ====================================================================

/// The host-side main-thread handle to a loaded plugin. Lives on the
/// plugin-main thread. The audio thread never touches this object — it
/// works on the separate [`AudioProcessorHalf`] obtained at publish time
/// via [`Self::audio_half`].
#[allow(dead_code)] // `format()` is wired up for future UI display.
pub trait LoadedPlugin: Send {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn format(&self) -> PluginFormat;

    /// The audio half backing this instance (Arc clone). Published into the
    /// worker registry; also used by the main half's own lifecycle hooks.
    fn audio_half(&self) -> Arc<AudioHalf>;

    // --- lifecycle (plugin-main thread; quiesced window when live) -------
    fn activate(&mut self, sample_rate: f64, min_frames: u32, max_frames: u32) -> Result<()>;
    fn deactivate(&mut self);
    fn start_processing(&mut self) -> Result<()>;
    fn stop_processing(&mut self);

    /// Clear the plugin's audio processing state (tails / voices) without
    /// touching parameters. CLAP forwards to `clap_plugin.reset()`; others
    /// default no-op. Quiesced window のみ。
    fn reset(&mut self) {}

    /// CLAP `clap_plugin.on_main_thread()` — plugin が `request_callback`
    /// で予約した main-thread task を 1 回実行する。他 format は no-op。
    fn on_main_thread(&mut self) {}

    // --- render-mode hint -------------------------------------------------
    /// Realtime / Offline hint. CLAP は `clap_plugin_render.set`、VST3 は
    /// per-buffer `ProcessData::processMode` 切替 (audio half と共有の
    /// atomic 経由)。
    fn set_render_mode(&mut self, mode: RenderMode) -> bool;

    /// PDC: query the plugin's reported processing latency in samples.
    /// Requires the plugin to be active.
    fn query_latency(&mut self) -> u32;

    /// Enumerate every parameter the plugin exposes (plugin-main thread).
    fn enumerate_params(&self) -> Vec<PluginParamInfo> {
        Vec::new()
    }

    // --- persistence (plugin-main thread) --------------------------------
    fn state_save(&self) -> Result<Option<Vec<u8>>>;
    fn state_load(&mut self, data: &[u8]) -> Result<()>;

    /// パラアウト: how many parallel-out ports this plugin declared.
    fn aux_output_port_count(&self) -> usize {
        0
    }

    /// VOICEVOX capability downcast. Default `None` (external plugins).
    fn as_vocal_synth(&mut self) -> Option<&mut dyn VocalSynth> {
        None
    }

    // --- ARA (r.md #5) ----------------------------------------------------
    /// If ARA-capable, create the document controller and bind the instance
    /// (before the first activate / state load / GUI, per ARA spec).
    fn bind_ara_if_capable(&mut self) -> Result<bool> {
        Ok(false)
    }

    /// Update the bound ARA document to expose `clips` (deactivate →
    /// set_clips → restore archive → reactivate は
    /// [`crate::ara::run_setup_ara`] に一本化)。
    fn setup_ara(
        &mut self,
        _clips: &[common::protocol::AraClipSpec],
        _bpm: f64,
        _time_sig: (u16, u16),
        _archive: Option<&[u8]>,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Update only the placement / stretch of existing ARA regions
    /// (safe while rendering — no deactivate).
    fn update_ara_regions(&self, _regions: &[common::protocol::AraRegionUpdate]) {}

    /// Tear down this instance's ARA session, if any.
    fn clear_ara(&mut self) {}

    /// Drive the bound ARA document's deferred work / analysis
    /// (plugin-main timer).
    fn notify_ara_model_updates(&self) {}

    /// Whether this instance currently holds a live ARA session.
    fn has_ara_session(&self) -> bool {
        false
    }

    /// Serialise this instance's ARA edit state for project save.
    fn store_ara_archive(&self) -> Option<Vec<u8>> {
        None
    }

    // --- embedded Win32 GUI (plugin-main thread) --------------------------
    fn gui_is_embed_supported(&self) -> bool;
    fn gui_create_embedded(&mut self) -> Result<()>;
    fn gui_get_size(&self) -> Option<(u32, u32)>;
    fn gui_set_scale(&self, scale: f64) -> Result<bool>;
    fn gui_set_parent_hwnd(&self, hwnd: u64) -> Result<()>;
    fn gui_show(&self) -> Result<bool>;
    fn gui_hide(&self) -> Result<()>;
    fn gui_destroy(&mut self);
    /// r.md #65: エディタ窓の WNDPROC がサイズ交渉に使う借用ハンドル
    /// ([`EditorSizer`])。`gui_create_embedded` 済みでないと `None`。
    /// builtin / VOICEVOX は GUI を持たないので常に `None`。
    ///
    /// 旧 `gui_can_resize` / `gui_set_size` はここへ吸収した: 「サイズ交渉」の
    /// FFI 面が trait と WNDPROC の 2 箇所に割れていると、ホスト起点とプラグイン
    /// 起点で別々の実装が育つ (実際に `checkSizeConstraint` を掛ける / 掛けないが
    /// 食い違っていた)。
    fn gui_sizer(&self) -> Option<Box<dyn EditorSizer>> {
        None
    }
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
            // `path` here is a `builtin://...` URI. `plugin_id` is unused —
            // the URI itself is the descriptor id.
            let _ = plugin_id;
            let uri = path.to_string_lossy();
            builtin::load_builtin(&uri, callbacks)
        }
    }
}
