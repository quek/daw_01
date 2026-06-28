use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::plugin_format::PluginFormat;

/// 旧 per-section plugin slot 表現 (MIDI FX chain / 単 Instrument / audio FX
/// chain)。 single-chain 再設計 (`docs/plan_linear_chain.md`) 後の IPC は
/// device index (`u32`) addressing に移行済みで、 この enum は
/// [`crate::model::AutomationTarget::PluginParam`] の `legacy_slot` (= 旧
/// project の save migration) からのみ参照される。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode, Serialize, Deserialize,
)]
pub enum PluginSlot {
    MidiFx(u32),
    Instrument,
    Fx(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum ChildKind {
    Audio,
    PluginHost,
}

/// CLAP `render` extension mode. Sent to the plugin host via
/// `MainToChild::SetRenderMode` so it can call
/// `clap_plugin_render.set` on every loaded plugin.
///
/// `Realtime` is the default — plugins should optimise for low latency.
/// `Offline` is set during WAV export so plugins free to use higher
/// quality / non-realtime algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum RenderMode {
    Realtime,
    Offline,
}

impl ChildKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChildKind::Audio => "audio",
            ChildKind::PluginHost => "plugin_host",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum ChildToMain {
    Hello {
        kind: ChildKind,
        pid: u32,
    },
    /// 子プロセスとの IPC pipe が切断された (= 子プロセスが exit、 panic、
    /// あるいは bincode decode 失敗で receive loop が終了した)。 daw_gui
    /// 内部で `audio_pipe_loop` / `plugin_pipe_loop` が pipe break を
    /// 検知したときに incoming channel へ自前で送出する synthetic event。
    /// AppData は受け取って該当 kind の child を re-spawn し、 Session /
    /// OpenWorkerPool / LoadSong / plugin slots を再構築する。
    ChildDisconnected {
        kind: ChildKind,
    },
    /// Offline WAV export finished, was cancelled, or failed. Sent by
    /// daw_audio when the export thread finalises (or aborts). Exactly one
    /// of the states holds:
    /// - `error == None && !cancelled` — the WAV at the requested path is
    ///   fully written (success).
    /// - `cancelled == true` — the user aborted via
    ///   `MainToChild::CancelExport`; the partial WAV was deleted. `error`
    ///   is `None`. The host treats this as a cancel, not a failure.
    /// - `error == Some(msg)` — render failed; `msg` is the reason.
    ExportWavComplete {
        error: Option<String>,
        cancelled: bool,
    },
    /// Offline WAV render progress, sent by daw_audio while
    /// `MainToChild::ExportWav` freewheels through the song. `done` is the
    /// number of song-body samples rendered so far, `total` the song-body
    /// length in samples (= the progress-bar denominator). Throttled by the
    /// sender (≈ every 0.5 % of `total`) so it never floods the IPC pipe.
    /// The host maps it to `AppEvent::ExportWavProgress` to drive the
    /// determinate progress overlay (= standalone WAV export, and the audio
    /// render phase of a video export).
    ExportWavProgress {
        done: u64,
        total: u64,
    },
    /// The plugin host finished reinitialising (deactivate→activate) every
    /// plugin — reply to `MainToChild::ReinitAllPlugins`. The reinit forces a
    /// clean state even for plugins that ignore CLAP `reset()` (e.g. VCV Rack 2
    /// holding a live voice). For an offline export the host waits
    /// for this before sending `ExportWav`; for the panic button it
    /// is fire-and-forget (the host ignores the reply when no export is queued).
    PluginsReinitDone,
    /// Offline plugin-FX bounce finished (or failed). Mirror of
    /// `ExportWavComplete` but for `MainToChild::BounceClipFxOnline`.
    /// `frames` is the actual number of frames written to the WAV
    /// (= `end_frame - start_frame` plus tail silence kept).
    /// `source_track` / `source_clip` echo the request so the host can
    /// look up which clip the result belongs to.
    BounceClipFxComplete {
        path: std::path::PathBuf,
        source_track: u32,
        source_clip: u32,
        error: Option<String>,
        frames: u64,
    },
    /// builtin VOICEVOX の歌唱合成が `MainToChild::PrepareVocalSynth`
    /// で要求した世代まで完了した (or タイムアウトした) ことを daw_gui に通知する。
    /// daw_gui は `pending_vocal_synth_bounce` が一致すれば歌唱 bounce の offline
    /// render を開始する。 `plugin_id` は要求を echo back する builtin の host id。
    VocalSynthReady {
        plugin_id: u32,
    },
    /// Plugin-host confirmed `SetSlotPlugin` and reported the stable id /
    /// display name of the descriptor that actually loaded.
    /// `plugin_id` is the host's session-unique identifier for this
    /// instance; `shmem_id` names the `ProcessData` shared memory the
    /// host created so daw_audio can `OpenShared` it and use the
    /// worker-pool dispatch to drive `plugin.process()`.
    ///
    /// `state_load_error` は saved project の plugin state を
    /// `state_load(&bytes)` で復元しようとして失敗したときの理由文字列。
    /// この場合 plugin は default 状態で chain に挿さる (= load 自体は成功
    /// しているので silent に進めるのは UX 上の silent corruption)。 daw_gui
    /// は status_message でユーザーに伝えて「設定が復元されなかった」 と
    /// 認識可能にする。 `None` = state_load が呼ばれなかった (= 新規追加)
    /// or 復元成功。
    SlotPluginLoaded {
        track: u32,
        index: u32,
        id: String,
        name: String,
        plugin_id: u32,
        shmem_id: String,
        state_load_error: Option<String>,
        /// パラアウト (docs/plan_paraout.md): how many `is_main=false` audio
        /// output ports this plugin declared (0 for the common single-out
        /// case). The GUI caches it to know how many child tracks to create on
        /// "explode" and how many routing rows to show. CLAP only (VST3 /
        /// builtin report 0).
        aux_output_count: u8,
    },
    /// Reply to `RequestSlotState`. `None` = plugin unavailable or state
    /// extension missing.
    SlotPluginState {
        track: u32,
        index: u32,
        data: Option<Vec<u8>>,
    },
    /// Reply to `RequestAllStates`: one entry per device that had a plugin
    /// loaded at request time. Makes project save a single round-trip.
    AllPluginStates {
        entries: Vec<SlotState>,
    },
    /// GUI opened at the requested size.
    SlotGuiOpened {
        track: u32,
        index: u32,
        width: u32,
        height: u32,
    },
    /// Plugin-initiated close (X button handled by plugin, or `closed`).
    SlotGuiClosed {
        track: u32,
        index: u32,
    },
    /// Plugin host destroyed a plugin instance (RemoveSlotPlugin /
    /// RemoveTrack 経由)。 daw_gui はこれを受け取って
    /// `MainToChild::ClosePluginShmem { plugin_id }` を daw_audio に
    /// 転送し、 audio engine の `plugin_refs` / `slot_to_plugin_id`
    /// から stale entry を消す。 これを送らないと audio thread が
    /// destroy 済 plugin に process() を呼んで「VST3 plugin not
    /// processing」 エラー / deadlock を起こす。
    SlotPluginUnloaded {
        plugin_id: u32,
    },
    /// `SetSlotPlugin` の load が失敗した。 daw_gui は
    /// `pending_plugin_loads` から該当 entry を解放し、 `pending_play`
    /// が立っていれば flush する (= 「失敗 = 完了」 と等価扱いで Play
    /// queue を解放)。 song の slot は touch せず、 旧 plugin が居れば
    /// 継続。 reason は plugin host 側 `tracing::error!` 相当の文字列
    /// (例: "library load failed: ABI mismatch")。
    SlotPluginLoadFailed {
        track: u32,
        index: u32,
        plugin_id: String,
        reason: String,
    },
    /// Plugin が報告した自身の processing latency (samples 単位、 host
    /// sample_rate)。 PR3 PDC pipeline の最終段で、 plugin が active 化
    /// した直後 (CLAP `activate` 完了直後 / VST3 `setActive(true)` 完了
    /// 直後) もしくは plugin が `host->request_restart()` /
    /// `IComponentHandler::restartComponent(kLatencyChanged)` で再 query
    /// を要求して deactivate→activate→get の往復を完了した直後に発火。
    /// daw_gui は plugin_id から (track_id, device index) を逆引きして
    /// `Track::reported_latency_samples` を更新し、 daw_audio に
    /// `LoadSong` を再送して compile_schedule に PDC を再計算させる。
    PluginLatencyChanged {
        plugin_id: u32,
        samples: u32,
    },
    /// Phase 2 (`docs/plan_automation.md` §7.5): plugin の parameter
    /// 一覧。 plugin activate 完了直後に 1 度送信、 `clap_plugin_params
    /// .changed` 等で rescan 要求が来たら再送。 daw_gui は
    /// `AppData.plugin_params` にキャッシュして parameter automation
    /// lane の label / min/max / display 用に使う。
    PluginParamList {
        track: u32,
        index: u32,
        plugin_id: u32,
        params: Vec<PluginParamInfo>,
        /// この plugin が埋め込み GUI (editor window) を持つか
        /// (`LoadedPlugin::gui_is_embed_supported`)。 daw_gui はチェーン行の
        /// ボタンを分岐する: GUI あり = editor window を開く「GUI」、 なし =
        /// インライン param パネルをトグルする「⚙」。 builtin (VOICEVOX /
        /// Silence) は false。
        has_embedded_gui: bool,
    },
    /// Phase 2: plugin GUI で knob を **touch** した通知 (= CLAP
    /// `CLAP_EVENT_PARAM_GESTURE_BEGIN` / VST3 `IComponentHandler
    /// ::beginEdit` 経由)。 daw_gui の `last_touched_param` を更新し、
    /// `A` キー shortcut の source にする。 `display_name` は host が
    /// PluginParamInfo lookup で補完して送る。
    PluginParamTouched {
        track: u32,
        index: u32,
        param_id: u32,
        display_name: String,
    },
    /// Phase 2: plugin GUI 内で parameter 値が変更された通知 (= CLAP
    /// out_events の `CLAP_EVENT_PARAM_VALUE` / VST3 `performEdit`
    /// 経由)。 Phase 4 recording mode で point 生成に使う。 Phase 2 で
    /// は IPC 路を整備するのみ (daw_gui 側は受け取って no-op、 last
    /// value cache に保存)。
    PluginParamValueChanged {
        track: u32,
        index: u32,
        param_id: u32,
        value: f64,
    },
    /// Phase 4 Step C-3 (`docs/plan_automation.md` §6): plugin GUI で knob を
    /// release した通知 (CLAP `CLAP_EVENT_PARAM_GESTURE_END` out event
    /// 経由)。 daw_gui は `AppEvent::ParamGestureEnd` に変換して
    /// `active_param_gestures` から該当 PluginParam target を remove する
    /// (= Touch mode で recording 終了 + curve eval bypass 解除)。
    PluginParamGestureEnd {
        track: u32,
        index: u32,
        param_id: u32,
    },
    /// builtin VOICEVOX plugin の歌唱/読み上げ合成スレッドの状態遷移。
    /// `busy` = いま合成中 (= `queued_gen > done_gen`)、`failing` = 直近の HTTP 試行が
    /// 失敗した (= engine 未起動/起動途中で接続できない)。daw_gui がこれを per-plugin に
    /// 集約し、クリップ上スピナー + 全体オーバーレイ + engine 未接続警告を出す。
    /// bounce 用の one-shot `VocalSynthReady` とは別系統 (こちらは継続的に状態変化を報告)。
    VoicevoxSynthStatus {
        plugin_id: u32,
        busy: bool,
        failing: bool,
    },
}

/// Phase 2 (`docs/plan_automation.md` §7.5): 1 parameter のメタデータ。
/// CLAP `clap_param_info` / VST3 `ParameterInfo` の host 側
/// representation。 `id` の解釈は plugin format ごと:
/// - CLAP: `clap_param_info.id` (`clap_id` = `u32`)
/// - VST3: `Steinberg::Vst::ParamID` = `uint32`
///
/// `min_value` / `max_value` / `default_value` は plain 単位 (= plugin
/// の native スケール)。 VST3 は IEditController が normalized 0..=1 で
/// 扱うため、 plugin_host 側で `getParamNormalized` → plain 変換
/// (`plainParamToNormalized` の逆 = `normalizedParamToPlain`) を済ませて
/// 送る。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct PluginParamInfo {
    pub id: u32,
    pub name: String,
    pub module: String,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
    pub flags: u32,
}

/// `PluginParamInfo.flags` のビット定数。 CLAP `clap_param_info_flags`
/// と 1:1 対応 (VST3 backend も同 bitset に正規化して送る)。
pub mod plugin_param_flags {
    pub const STEPPED: u32 = 1 << 0;
    pub const PERIODIC: u32 = 1 << 1;
    pub const READONLY: u32 = 1 << 2;
    pub const HIDDEN: u32 = 1 << 3;
    pub const AUTOMATABLE: u32 = 1 << 4;
    pub const MODULATABLE: u32 = 1 << 5;
    pub const REQUIRES_PROCESS: u32 = 1 << 6;
}

/// Single entry in the `AllPluginStates` reply.
///
/// `data` is the plugin's serialized state (= bytes blob), `None` if:
/// - plugin doesn't implement the state extension (= state save unsupported)
/// - state save returned `Ok(None)` (= plugin opted out)
/// - `error` is `Some(...)` (= state save failed)
///
/// `error` is set when `state_save()` returned `Err(...)`. daw_gui surfaces
/// the aggregated error list in `status_message` so the user notices that
/// their saved project will reload with default plugin state for the
/// affected device(s) (= silent corruption fix). `None` = save succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SlotState {
    pub track: u32,
    pub index: u32,
    pub data: Option<Vec<u8>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct AudioSession {
    pub shmem_id: String,
    pub request_sem_id: String,
    pub ready_sem_id: String,
    /// resource monitor の `MetricsBridge` shmem os_id
    /// (`metrics_bridge::metrics_shmem_id(pid)`)。 daw_audio / daw_plugin_host
    /// がこれで `MetricsBridgeHandle::open` し、 DSP load / per-plugin CPU を publish。
    pub metrics_shmem_id: String,
    pub sample_rate: u32,
    pub max_frames: u32,
    pub channels: u16,
}

/// IPC messages from the host (daw_gui) to children (daw_audio /
/// daw_plugin_host). `LoadSong(Song)` is the dominant size driver
/// (~304 bytes incl. v12 video_* fields), but the message is sent at
/// most once per project load — boxing the `Song` would push every
/// IPC frame through a heap allocation just to soothe clippy. Accept
/// the size disparity here; if a future IPC variant grows even larger
/// we can revisit per-variant boxing then.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum MainToChild {
    Ack,
    Play,
    Stop,
    /// パニックボタン — master 出力を declick フェードで一瞬ミュート
    /// する。 panic は直後に（master がミュートされてから）`ReinitAllPlugins` を
    /// 送るので、 全 plugin を mix から外す瞬間の段差クリックがフェードで隠れる。
    /// audio engine は master を fade-out して **`PanicRelease` が来るまで 0 で
    /// hold** する（plugin-host hang 用に数秒の安全 auto-release あり）。transport
    /// の Stop とは独立。
    Panic,
    /// パニックの declick hold を解除して fade-in に移す。 daw_gui が
    /// `ReinitAllPlugins` の完了通知 `PluginsReinitDone` を受けてから送る。 master
    /// のミュート解除を「reinit が実際に終わった瞬間」 に結びつけることで、 GUI
    /// メインスレッド stall や巨大 reinit でも plugin が mix に残ったまま master が
    /// 戻る（クリック / reverb tail 復活）ことを防ぐ。
    PanicRelease,
    Session(AudioSession),
    LoadSong(crate::model::Song),
    SetLoop(bool),
    SetMasterGain(f32),
    /// Offline-render the song to a WAV file. Sent to daw_audio, which
    /// freewheels through the song using its existing AudioWorker pool +
    /// plugin handshake, then replies with `ChildToMain::ExportWavComplete`.
    ///
    /// `range`:
    /// - `None` — full-song export (frame 0 → `length_beats` + tail).
    /// - `Some((start_frame, end_frame))` — render only that frame window
    ///   (user-chosen export range). The render walks the song
    ///   **from `start_frame`** (cold start), so audio whose note began
    ///   before `start_frame` (e.g. a VOICEVOX phrase still ringing, a held
    ///   note) is *not* retriggered — the result matches pressing Play at
    ///   `start_frame`. Only frames in `[start_frame, end_frame)` (plus tail
    ///   decay) are written. Frames are absolute sample offsets at the
    ///   session sample rate; the GUI converts the chosen beat range with
    ///   `samples_per_beat = sample_rate * 60 / bpm`. (Plugin tails start
    ///   dry; the warm-from-0 walk is `BounceClipFxOnline`'s job, not this.)
    ExportWav {
        path: std::path::PathBuf,
        range: Option<(u64, u64)>,
        /// Write the modulation-envelope sidecar (`.modenv`) next to the WAV.
        /// Only the offline video render consumes it (it samples the sidecar
        /// to reproduce LFO/envelope modulation frame-accurately). A
        /// standalone WAV export has the modulation already baked into the
        /// audio, so it passes `false` and doesn't litter the user's folder
        /// with a file nothing reads. The GUI sets `true` only for the
        /// video-export temp WAV.
        write_mod_sidecar: bool,
    },
    /// Abort the in-flight offline render (= `ExportWav`). daw_audio sets
    /// `EngineShared::export_cancel`; the freewheel loop checks it every
    /// buffer, breaks, deletes the partial WAV, and replies with
    /// `ChildToMain::ExportWavComplete { error: None, cancelled: true }`
    /// (cancel is signalled by the typed `cancelled` flag, not an error
    /// string). No-op when no export is running. Sent by the progress
    /// overlay's Cancel button (and Esc) while the audio render phase is active.
    CancelExport,
    /// Reinitialise (deactivate→activate) every loaded plugin to a clean state.
    /// CLAP `reset()` is not enough for some plugins (VCV Rack 2 keeps a live
    /// voice ringing through reset / start-stop_processing); a full
    /// deactivate→activate cycle is the only reliable clean slate. Handled on
    /// the plugin-main thread via the proven detach→quiesce→mutate→republish
    /// pattern (so it is safe even while the live callback is running), then
    /// replies `ChildToMain::PluginsReinitDone`.
    ///
    /// Two callers share this single operation (SSoT for "force all plugins to
    /// silence / clean state"):
    /// - sent **before** `ExportWav` (for a user range / cold
    ///   render) so an offline cold render starts from a clean state.
    /// - the transport **panic button** — kills stuck voices /
    ///   reverb tails / held preview notes immediately. Fire-and-forget.
    ReinitAllPlugins,
    /// Offline-render a clip range with the **full plugin chain** (= post-FX)
    /// to a WAV file. Used by `Bounce (with FX)` (`docs/plan_audio_clip
    /// .md` §3.8). Same freewheel pipeline as `ExportWav` but the WAV
    /// captures only frames in `[start_frame, end_frame)` (plus tail
    /// silence cutoff). The render walks the song from frame 0 so plugin
    /// state at `start_frame` is fully accumulated (= reverb tails /
    /// parameter ramps / sidechain history are correct). The host sends
    /// this with `SetRenderMode(Offline)` bookended around it. The audio
    /// engine replies with `ChildToMain::BounceClipFxComplete`.
    ///
    /// `source_track` / `source_clip` are echoed back in the completion
    /// reply so the host can resolve which clip the freshly-written WAV
    /// belongs to (= multiple bounces in flight not supported in M1, but
    /// the field structure leaves room for it).
    BounceClipFxOnline {
        path: std::path::PathBuf,
        source_track: u32,
        source_clip: u32,
        start_frame: u64,
        end_frame: u64,
    },
    /// Builtin plugin (`PluginFormat::Builtin`) に per-note metadata を
    /// flush する。 daw_gui が plugin_id 別に、 「note_id → metadata」 の
    /// 一括 vector を送る。 daw_plugin_host だけが consume (= daw_audio
    /// は ignore)。 CLAP / VST3 plugin に対する flush は trait の default
    /// no-op 実装で吸収するので、 plugin format をホスト側で気にする必要
    /// はない (`docs/plan_voicevox_synth.md` PR-V2.2)。
    SetBuiltinPluginNoteMetadata {
        plugin_id: u32,
        bpm: f32,
        entries: Vec<crate::plugin_metadata::NoteMetadata>,
        /// (talk) 同トラックの `ClipContent::Text` 由来の読み上げ群
        /// (`docs/plan_voicevox_talk.md` §3.2)。歌唱 (`entries`) と talk を同じ
        /// flush で運び、builtin が 1 つの合成 job (= 1 連続バッファ) に統合する。
        /// IPC 専用 (= 全プロセス同時 rebuild) なので bincode 後方互換は不要。
        talk: Vec<crate::plugin_metadata::TalkMetadata>,
    },
    /// 歌唱 bounce の前に builtin VOICEVOX の合成完了を要求する。 直前に
    /// 送った `SetBuiltinPluginNoteMetadata` の世代まで synth が終わったら plugin host が
    /// `ChildToMain::VocalSynthReady` を返す (非同期 HTTP 合成が offline render より
    /// 遅れて無音になるのを防ぐ)。 `plugin_id` は対象 builtin の host id。
    PrepareVocalSynth {
        plugin_id: u32,
    },
    /// Tell the plugin host to switch every loaded plugin's CLAP render
    /// mode (Realtime ↔ Offline). The audio side bookends an export
    /// with `Offline` / `Realtime` so plugins that implement the CLAP
    /// `render` extension can pick higher-quality algorithms during
    /// export and revert afterwards.
    SetRenderMode(RenderMode),
    /// Reposition the audio engine's playhead. Sent by daw_gui when the
    /// user clicks/drags the arrangement ruler. `samples` is the absolute
    /// frame offset from the start of the song at the engine sample rate.
    /// Takes effect on the next audio buffer regardless of `playing`
    /// state, so click-to-seek works both while stopped and during
    /// playback.
    SeekTo { samples: u64 },
    // PR-V4: `MainToChild::SetGeneratedAudio` 削除済。 旧 path で
    // VOICEVOX 合成結果を audio engine に流していたが、 builtin
    // instrument plugin (`PluginFormat::Builtin`) が plugin host 内で
    // 合成を完結させるため不要に。
    /// Tell the audio engine the current project directory so it can
    /// resolve `AudioSourcePath::ProjectRelative` entries against
    /// `<project_dir>/samples/<...>`. `None` for unsaved projects —
    /// in that state any `ProjectRelative` AudioSource fails to load
    /// and the corresponding clip plays silence with a "missing
    /// source" badge in the GUI. Spec: §9.2.
    SetProjectDir(Option<std::path::PathBuf>),
    /// `track` は stable な `Track::id` (= Phase 6 review で Vec index 由来の
    /// race risk を取り除いた)。 audio engine 側は `s.tracks.iter_mut().find(
    /// |t| t.id == track)` で look up する。 reorder / delete 中に楽勝で
    /// 通っていた race (= GUI が新 idx で送るが audio が旧 LoadSong 直前
    /// で旧 Vec position 解釈) を防ぐ。
    SetTrackVolume { track: u32, volume: f32 },
    SetTrackPan { track: u32, pan: f32 },
    SetTrackMuted { track: u32, muted: bool },
    SetTrackSolo { track: u32, solo: bool },
    /// Realtime aux-send level update (`docs/plan_routing_graph.md`).
    /// `track` is the stable `Track::id` of the **source** track,
    /// `send_idx` the index into its `sends`. Same lightweight, id-based
    /// idiom as `SetTrackVolume`: while the user drags a send knob during
    /// playback the engine clone-mutate-stores `shared.song` and re-reads
    /// the live gain (ramped per-sample), so the routing graph is **not**
    /// recompiled per frame. Adding / removing a send, or changing its
    /// destination / mode, is a structural change that still goes through
    /// `LoadSong` (full `compile_schedule`).
    SetSendGain { track: u32, send_idx: u8, gain: f32 },
    /// Realtime per-send mute toggle. Same idiom as `SetSendGain`. The
    /// send's graph edge always exists once wired (so toggling needs no
    /// recompile); the engine gates the contribution to silence at
    /// mix time when `enabled == false`.
    SetSendEnabled { track: u32, send_idx: u8, enabled: bool },
    /// Phase 7 B4 (2026-05-13): Record-arm 状態を audio engine に伝達。
    /// audio thread は track.armed を Song に反映するのみ (= 録音書き込み
    /// 自体は GUI process で行うため audio 側は値を保持するだけ、 将来の
    /// audio input 録音で audio thread 側書き込みに使う)。
    SetTrackArmed { track: u32, armed: bool },
    /// Phase 5 Step 5.1 follow-up (gui_01 #035 + daw_01 transport scrub):
    /// BPM 軽量更新。 transport の scrubable_number drag 中に毎 frame 流れ
    /// うる想定なので、 `LoadSong` (= 全 Song serialize) ではなく単 field の
    /// 更新のみで済むよう新 variant 化。 audio engine は `shared.song` を
    /// ArcSwap で clone-mutate-store して反映 (= 1 frame 内で SongTempo lane
    /// があっても `evaluate_song_tempo` が new bpm を引く)。 1.0..=400.0 で
    /// clamp 想定。
    SetSongBpm { bpm: f32 },
    /// Phase 5 Step 5.1 follow-up: TimeSig 分子 (numerator) の軽量更新。
    /// 1..=32 で clamp 想定。 `SetSongBpm` と同 idiom。
    SetSongTimeSigNumerator { num: u8 },
    /// Phase 4 Step C-2 (`docs/plan_automation.md` §6): GUI が現在 recording
    /// 中の lane (track + target の 2 つ組) を audio thread に通知する。
    /// audio thread は受信時 SharedState 上の HashSet を ArcSwap で更新し、
    /// `fill_track_param_ramps` で該当 lane の curve eval を bypass する
    /// (= track.volume / track.pan の live value をそのまま出力)。 空 Vec は
    /// 「現在 recording 中の lane なし = 全 curve eval する」 を意味する。
    /// daw_gui は recording_mode / active / latched gesture の変化 (= currently
    /// recording set の edge) と stop / Read 遷移で送る。
    SetRecordingLanes {
        lanes: Vec<(u32, crate::model::AutomationTarget)>,
    },
    /// Phase 7 B3 (2026-05-13): メトロノーム on/off。 `true` で audio thread
    /// が beat 境界ごとに internal click 音 (sine, accent: downbeat 880Hz /
    /// 他 440Hz, 40ms linear decay, peak -12 dB) を master mix に重ねる。
    /// `false` で無音 (= mix step を skip)。 起動時 default false。
    /// session-only state (project save には含めない)。
    SetMetronomeEnabled(bool),
    /// 鍵盤レーン click のピッチプレビュー (gui_01 #055,
    /// `docs/plan_pianoroll_keyboard_preview.md`)。 piano_roll widget の
    /// `keyboard_active_pitch` を GUI が前フレーム値と差分して導出する
    /// 単発 note-on。 `track_id` は対象トラックの id (= reorder race-free、
    /// `SetTrackVolume` 等と同じ id ベース)、 daw_audio 側で現 song snapshot
    /// から Vec index に解決して該当トラックの音源プラグインへ届ける。
    /// `velocity` は 0..=127 の MIDI 値 (固定 100)。 transport 状態に
    /// 関係なく発音する (instrument dispatch は playing 非依存)。
    PreviewNoteOn {
        track_id: u32,
        pitch: u8,
        velocity: u8,
    },
    /// 鍵盤プレビューの note-off (gui_01 #055)。 mouse release /
    /// glissando の旧 pitch off / 鍵盤外移動で発火。 `track_id` は
    /// [`PreviewNoteOn`](Self::PreviewNoteOn) と同じく対象トラック id。
    PreviewNoteOff {
        track_id: u32,
        pitch: u8,
    },
    /// Phase 7 B4 Step C (2026-05-13): count-in 開始 IPC。 audio engine が
    /// `EngineShared::preroll_total_samples` / `preroll_remaining_samples` を
    /// `samples` で store、 `process_buffer` で preroll > 0 のとき dispatch /
    /// clip render を skip + metronome のみ render。 0 到達で normal 再生に
    /// 戻る。 GUI 側は audio_bridge の preroll mirror を on_tick で poll、
    /// 0 検出で midi_recording_pending → midi_recording 遷移。 `samples = 0`
    /// で count-in を即時 cancel (= stop_recording 中の preroll キャンセル用)。
    StartCountIn { samples: u64 },
    // --- Per-track plugin device management ---------------------------
    /// Load / replace the plugin at `(track, device index)`. `format`
    /// routes the request to the CLAP or VST3 backend. Empty `plugin_id`
    /// picks the first descriptor in `path`; non-empty selects by id (CLAP
    /// stable id or VST3 FUID as hex). `initial_state`, when `Some`, is
    /// applied via the backend's state-restore entry right after activate.
    SetSlotPlugin {
        track: u32,
        index: u32,
        format: PluginFormat,
        path: std::path::PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
    },
    /// Remove the plugin at `(track, device index)` if any.
    RemoveSlotPlugin {
        track: u32,
        index: u32,
    },
    /// apply an arbitrary chain reorder as a glitch-free **live
    /// move**. `moves` is the COMPLETE new layout of `track`'s chain: one
    /// `(old_index, new_index)` per loaded plugin (including
    /// `old_index == new_index` for ones that did not move), where
    /// `old_index` is the plugin's current device index and `new_index` is
    /// its device index after the drag. The set of `old_index` values must
    /// exactly cover the track's currently-loaded devices and the
    /// `new_index` values must form a contiguous `0..n` permutation.
    ///
    /// Sent to BOTH children:
    /// - the plugin host permutes its live `Box<dyn LoadedPlugin>`s in place
    ///   (heap address preserved → no re-instantiation, no audio glitch, open
    ///   editor windows follow) and re-keys every `(track, device index)`
    ///   book plus the worker registry entry index;
    /// - the audio engine atomically re-keys `slot_to_plugin_id` so each
    ///   device index resolves to the moved plugin (the processing order
    ///   itself follows the subsequent `LoadSong`).
    ReorderChain {
        track: u32,
        moves: Vec<(u32, u32)>,
    },
    /// Drop the entire chain for `track` (every MIDI FX / Instrument / FX
    /// slot), tearing down each plugin's GUI first. `track` is a stable
    /// `Track::id` (since PR2.1 the plugin host's chain map is keyed by
    /// id, not Vec position). Sent when the user removes a whole track
    /// so the audio thread stops rendering it.
    RemoveTrack { track: u32 },
    /// Ask the plugin_host to capture state for one device. Reply is
    /// `ChildToMain::SlotPluginState`.
    RequestSlotState {
        track: u32,
        index: u32,
    },
    /// Ask the plugin_host to capture state for every device at once.
    /// Reply is `ChildToMain::AllPluginStates` containing one entry per
    /// loaded plugin. Used for project save.
    RequestAllStates,
    // --- GUI management ----------------------------------------------
    /// Open the plugin editor. The editor's top-level window is
    /// now created and owned by the plugin-host process (on its plugin-main
    /// thread), NOT by daw_gui. That makes the editor's `GA_ROOTOWNER`
    /// resolve into the plugin-host process so JUCE's
    /// `Process::isForegroundProcess()` becomes true when the editor is
    /// focused — which is what lets cascade sub-menus stay open. `title`
    /// is the window caption daw_gui composed (track / device context).
    OpenSlotGuiEmbedded {
        track: u32,
        index: u32,
        title: String,
    },
    CloseSlotGui {
        track: u32,
        index: u32,
    },
    // --- A2 audio engine refactor -------------------------------------
    /// Stand up the per-buffer plugin process worker pool. `n_workers`
    /// audio-engine workers will pair 1:1 with `n_workers` plugin-host
    /// workers via the named events listed (one wake + one done per
    /// pair). The `WorkerBridge` shmem published under
    /// `worker_bridge_shmem_id` carries the per-worker `worker_task`
    /// (plugin id) the audio side writes before each dispatch.
    OpenWorkerPool {
        n_workers: u32,
        worker_bridge_shmem_id: String,
        wake_event_names: Vec<String>,
        done_event_names: Vec<String>,
    },
    /// Tear down the worker pool started by `OpenWorkerPool`. Plugin-host
    /// workers exit and the IDs they held become invalid.
    CloseWorkerPool,
    /// Map a `ProcessData` shmem region into the consumer (daw_audio).
    /// `track` and `index` let the audio engine slot the plugin into the
    /// right place in its routing graph; `plugin_id` is the host's
    /// session-unique id and `shmem_id` names the shmem the host
    /// already created.
    OpenPluginShmem {
        plugin_id: u32,
        shmem_id: String,
        track: u32,
        index: u32,
    },
    /// Drop the `ProcessData` mapping for `plugin_id` after the plugin
    /// instance is being torn down.
    ClosePluginShmem { plugin_id: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(msg: &T) -> T
    where
        T: Encode + Decode<()>,
    {
        let config = bincode::config::standard();
        let bytes = bincode::encode_to_vec(msg, config).unwrap();
        let (decoded, _) = bincode::decode_from_slice(&bytes, config).unwrap();
        decoded
    }

    #[test]
    fn child_kind_as_str() {
        assert_eq!(ChildKind::Audio.as_str(), "audio");
        assert_eq!(ChildKind::PluginHost.as_str(), "plugin_host");
    }

    #[test]
    fn child_to_main_hello_roundtrip() {
        let msg = ChildToMain::Hello {
            kind: ChildKind::Audio,
            pid: 12345,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn main_to_child_ack_roundtrip() {
        let msg = MainToChild::Ack;
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn main_to_child_play_stop_roundtrip() {
        assert_eq!(roundtrip(&MainToChild::Play), MainToChild::Play);
        assert_eq!(roundtrip(&MainToChild::Stop), MainToChild::Stop);
    }

    #[test]
    fn main_to_child_load_song_roundtrip() {
        let msg = MainToChild::LoadSong(crate::model::Song::default());
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn child_to_main_export_wav_complete_roundtrip() {
        // 成功 / キャンセル / 失敗 の 3 状態すべて wire roundtrip する。
        let ok = ChildToMain::ExportWavComplete { error: None, cancelled: false };
        assert_eq!(roundtrip(&ok), ok);
        let cancelled = ChildToMain::ExportWavComplete { error: None, cancelled: true };
        assert_eq!(roundtrip(&cancelled), cancelled);
        let failed = ChildToMain::ExportWavComplete {
            error: Some("render failed".to_string()),
            cancelled: false,
        };
        assert_eq!(roundtrip(&failed), failed);
    }

    #[test]
    fn child_to_main_voicevox_synth_status_roundtrip() {
        for (busy, failing) in [(false, false), (true, false), (true, true)] {
            let msg = ChildToMain::VoicevoxSynthStatus { plugin_id: 7, busy, failing };
            assert_eq!(roundtrip(&msg), msg);
        }
    }

    #[test]
    fn child_to_main_export_wav_progress_roundtrip() {
        let msg = ChildToMain::ExportWavProgress { done: 123, total: 48_000 };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn main_to_child_cancel_export_roundtrip() {
        let msg = MainToChild::CancelExport;
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn child_to_main_all_plugin_states_roundtrip() {
        let msg = ChildToMain::AllPluginStates {
            entries: vec![
                SlotState {
                    track: 0,
                    index: 1,
                    data: Some(vec![1, 2, 3, 4]),
                    error: None,
                },
                SlotState {
                    track: 3,
                    index: 4,
                    data: None,
                    error: Some("state save failed".to_string()),
                },
            ],
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn child_to_main_plugin_param_list_roundtrip() {
        let msg = ChildToMain::PluginParamList {
            track: 1,
            index: 0,
            plugin_id: 7,
            params: vec![
                PluginParamInfo {
                    id: 100,
                    name: "Gain".to_string(),
                    module: "Mixer".to_string(),
                    min_value: 0.0,
                    max_value: 1.0,
                    default_value: 0.5,
                    flags: plugin_param_flags::AUTOMATABLE,
                },
                PluginParamInfo {
                    id: 101,
                    name: "Cutoff".to_string(),
                    module: String::new(),
                    min_value: 20.0,
                    max_value: 20000.0,
                    default_value: 1000.0,
                    flags: plugin_param_flags::STEPPED
                        | plugin_param_flags::PERIODIC,
                },
            ],
            has_embedded_gui: true,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn main_to_child_reorder_chain_roundtrip() {
        let msg = MainToChild::ReorderChain {
            track: 4,
            moves: vec![(0, 1), (1, 0), (2, 3), (3, 2)],
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn main_to_child_builtin_note_metadata_roundtrip() {
        let msg = MainToChild::SetBuiltinPluginNoteMetadata {
            plugin_id: 9,
            bpm: 128.0,
            entries: vec![
                crate::plugin_metadata::NoteMetadata {
                    note_id: 0,
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    pitch: 60,
                    velocity: 100,
                    lyric: "あ".to_string(),
                    clip_id: 5,
                    speaker_id: 3061,
                },
                crate::plugin_metadata::NoteMetadata {
                    note_id: 1,
                    start_beat: 1.0,
                    duration_beats: 0.5,
                    pitch: 62,
                    velocity: 90,
                    lyric: String::new(),
                    clip_id: 5,
                    speaker_id: 3061,
                },
            ],
            talk: vec![crate::plugin_metadata::TalkMetadata {
                event_id: crate::plugin_metadata::talk_event_id(7, 0),
                start_beat: 4.0,
                text: "こんにちは".to_string(),
                speaker_id: 3,
                speed_scale: 1.1,
                pitch_scale: 0.0,
                intonation_scale: 1.0,
                volume_scale: 1.0,
            }],
        };
        assert_eq!(roundtrip(&msg), msg);
    }
}
