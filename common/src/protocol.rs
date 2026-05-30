use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::plugin_format::PluginFormat;

/// Addresses a single plugin slot inside a track. A track has:
/// - MIDI FX chain: `MidiFx(0)`, `MidiFx(1)`, ...
/// - one Instrument slot: `Instrument`
/// - audio FX chain: `Fx(0)`, `Fx(1)`, ...
///
/// Indices within `MidiFx` / `Fx` are stable while the chain is unchanged;
/// explicit `MoveSlot` messages rewrite them after a reorder.
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
    /// Offline WAV export finished (or failed). Sent by daw_audio when
    /// the export thread finalises (or hits an error). `error == None`
    /// means the WAV file at the requested path is fully written.
    ExportWavComplete {
        error: Option<String>,
    },
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
        slot: PluginSlot,
        id: String,
        name: String,
        plugin_id: u32,
        shmem_id: String,
        state_load_error: Option<String>,
    },
    /// Reply to `RequestSlotState`. `None` = plugin unavailable or state
    /// extension missing.
    SlotPluginState {
        track: u32,
        slot: PluginSlot,
        data: Option<Vec<u8>>,
    },
    /// Reply to `RequestAllStates`: one entry per slot that had a plugin
    /// loaded at request time. Makes project save a single round-trip.
    AllPluginStates {
        entries: Vec<SlotState>,
    },
    /// GUI opened at the requested size.
    SlotGuiOpened {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    /// Plugin-initiated resize via `clap_host_gui.request_resize`.
    SlotGuiRequestResize {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    /// Plugin-initiated close (X button handled by plugin, or `closed`).
    SlotGuiClosed {
        track: u32,
        slot: PluginSlot,
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
        slot: PluginSlot,
        plugin_id: String,
        reason: String,
    },
    /// Plugin が報告した自身の processing latency (samples 単位、 host
    /// sample_rate)。 PR3 PDC pipeline の最終段で、 plugin が active 化
    /// した直後 (CLAP `activate` 完了直後 / VST3 `setActive(true)` 完了
    /// 直後) もしくは plugin が `host->request_restart()` /
    /// `IComponentHandler::restartComponent(kLatencyChanged)` で再 query
    /// を要求して deactivate→activate→get の往復を完了した直後に発火。
    /// daw_gui は plugin_id から (track_id, slot) を逆引きして
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
        slot: PluginSlot,
        plugin_id: u32,
        params: Vec<PluginParamInfo>,
    },
    /// Phase 2: plugin GUI で knob を **touch** した通知 (= CLAP
    /// `CLAP_EVENT_PARAM_GESTURE_BEGIN` / VST3 `IComponentHandler
    /// ::beginEdit` 経由)。 daw_gui の `last_touched_param` を更新し、
    /// `A` キー shortcut の source にする。 `display_name` は host が
    /// PluginParamInfo lookup で補完して送る。
    PluginParamTouched {
        track: u32,
        slot: PluginSlot,
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
        slot: PluginSlot,
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
        slot: PluginSlot,
        param_id: u32,
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
/// affected slot(s) (= silent corruption fix). `None` = save succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SlotState {
    pub track: u32,
    pub slot: PluginSlot,
    pub data: Option<Vec<u8>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct AudioSession {
    pub shmem_id: String,
    pub request_sem_id: String,
    pub ready_sem_id: String,
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
    Session(AudioSession),
    LoadSong(crate::model::Song),
    SetLoop(bool),
    SetMasterGain(f32),
    /// Offline-render the entire song to a WAV file. Sent to daw_audio,
    /// which freewheels through the song using its existing AudioWorker
    /// pool + plugin handshake, then replies with
    /// `ChildToMain::ExportWavComplete`.
    ExportWav {
        path: std::path::PathBuf,
    },
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
    // --- Per-track plugin slot management -----------------------------
    /// Load / replace the plugin in `(track, slot)`. `format` routes the
    /// request to the CLAP or VST3 backend. Empty `plugin_id` picks the
    /// first descriptor in `path`; non-empty selects by id (CLAP stable id
    /// or VST3 FUID as hex). `initial_state`, when `Some`, is applied via
    /// the backend's state-restore entry right after activate.
    SetSlotPlugin {
        track: u32,
        slot: PluginSlot,
        format: PluginFormat,
        path: std::path::PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
    },
    /// Remove the plugin at `(track, slot)` if any.
    RemoveSlotPlugin {
        track: u32,
        slot: PluginSlot,
    },
    /// Reorder: move the plugin at `(track, from)` to `(track, to)`. Only
    /// valid within the same section (`MidiFx → MidiFx`, `Fx → Fx`).
    MoveSlot {
        track: u32,
        from: PluginSlot,
        to: PluginSlot,
    },
    /// Drop the entire chain for `track` (every MIDI FX / Instrument / FX
    /// slot), tearing down each plugin's GUI first. `track` is a stable
    /// `Track::id` (since PR2.1 the plugin host's chain map is keyed by
    /// id, not Vec position). Sent when the user removes a whole track
    /// so the audio thread stops rendering it.
    RemoveTrack { track: u32 },
    /// Ask the plugin_host to capture state for one slot. Reply is
    /// `ChildToMain::SlotPluginState`.
    RequestSlotState {
        track: u32,
        slot: PluginSlot,
    },
    /// Ask the plugin_host to capture state for every slot at once.
    /// Reply is `ChildToMain::AllPluginStates` containing one entry per
    /// loaded plugin. Used for project save.
    RequestAllStates,
    // --- GUI management ----------------------------------------------
    OpenSlotGuiEmbedded {
        track: u32,
        slot: PluginSlot,
        host_hwnd: u64,
    },
    CloseSlotGui {
        track: u32,
        slot: PluginSlot,
    },
    ResizeSlotGui {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
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
    /// `track` and `slot` let the audio engine slot the plugin into the
    /// right place in its routing graph; `plugin_id` is the host's
    /// session-unique id and `shmem_id` names the shmem the host
    /// already created.
    OpenPluginShmem {
        plugin_id: u32,
        shmem_id: String,
        track: u32,
        slot: PluginSlot,
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
}
