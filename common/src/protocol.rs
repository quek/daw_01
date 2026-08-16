//! IPC control-plane protocol (`docs/plan_arch_refactor.md` §3)。
//!
//! # 設計原則
//!
//! - **宛先は型で表現する**: gui→audio は [`AudioCommand`]、audio→gui は
//!   [`AudioEvent`]、gui→plugin_host は [`PluginCommand`]、plugin_host→gui は
//!   [`PluginEvent`]。pipe の read/write を型パラメータで縛ることで、誤配送・
//!   「相手が無視する variant の no-op arm 列挙」・無駄 decode が構造的に消える。
//! - **device のアドレスは安定 id (`PluginInstance::id`, u64) 一本**:
//!   `(track, device_index)` の positional addressing は廃止。reorder / 削除で
//!   参照が壊れる class のバグ (3 プロセス貫通の再キー儀式、stale callback) を
//!   構造的に排除する。旧 `ReorderChain` message は不要になり削除。
//! - **wire に MB 級 blob を載せない**: `LoadSong` の `Song` は
//!   `PluginInstance` の手書き bincode impl により `state` / `ara_archive` を
//!   構造的に除外する (常に小さい全量 snapshot)。blob が必要な操作は専用
//!   message (`SetSlotPlugin.initial_state` / `SetupAraDocument.archive` /
//!   `AllPluginStates`) が個別に運ぶ。
//! - **ビルド世代を handshake で検証する**: 子は Hello に
//!   [`PROTOCOL_FINGERPRINT`] を載せ、親は不一致なら明示 fail する
//!   (「protocol 変更後に古い exe が decode 失敗 → 無音 → 同じ exe を respawn」
//!   という診断困難な事故の構造的対策)。fingerprint は wire を渡る source
//!   file 群の content hash (common/build.rs) なので、protocol 未変更の
//!   再ビルドでは変わらない。

use bincode::{Decode, Encode};

use crate::plugin_format::PluginFormat;

/// r.md #36: プラグインエディタ窓 ↔ daw_gui 間で運ぶキーの組み合わせ。
///
/// **Win32 の仮想キーコード (`VK_*`) + 修飾フラグ** という OS 中立でない表現をあえて
/// 選んでいる。plugin-host が見るのは Win32 メッセージそのものであり、ここを抽象化すると
/// plugin-host 側に「キー名 → VK」 の対応表 (= 意味論の複製) が生えるため。
/// 対応表は daw_gui の `SHORTCUTS` 側 1 箇所に閉じ込め、 plugin-host は数値比較だけを行う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct KeyChord {
    /// Win32 virtual-key code (`VK_SPACE` = 0x20 等)。
    pub vk: u16,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// wire を渡る型を定義する source file 群の content hash (FNV-1a 64bit)。
/// `common/build.rs` がコンパイル時に計算する。Hello handshake で照合し、
/// 「ビルド世代の混在」(= bincode enum discriminant のズレによる silent
/// misdecode) を接続時に検出する。
pub const PROTOCOL_FINGERPRINT: u64 = match u64::from_str_radix(env!("DAW_PROTOCOL_FINGERPRINT"), 16) {
    Ok(v) => v,
    Err(_) => panic!("DAW_PROTOCOL_FINGERPRINT must be a hex u64 (set by build.rs)"),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum ChildKind {
    Audio,
    PluginHost,
}

impl ChildKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChildKind::Audio => "audio",
            ChildKind::PluginHost => "plugin_host",
        }
    }
}

/// CLAP `render` extension mode. Sent to the plugin host via
/// `PluginCommand::SetRenderMode` so it can call
/// `clap_plugin_render.set` on every loaded plugin.
///
/// `Realtime` is the default — plugins should optimise for low latency.
/// `Offline` is set during WAV export so plugins are free to use higher
/// quality / non-realtime algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
pub enum RenderMode {
    Realtime,
    Offline,
}

// =====================================================================
// 共有 struct (両 channel から参照される payload)
// =====================================================================

/// Data-plane session parameters, sent to both children right after the
/// handshake. shmem 名と audio format の SSoT。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct AudioSession {
    /// `AudioBridge` (playhead / peaks / mod scalars / preroll mirror) の
    /// shmem os_id。
    pub shmem_id: String,
    /// resource monitor の `MetricsBridge` shmem os_id
    /// (`metrics_bridge::metrics_shmem_id(pid)`)。 daw_audio / daw_plugin_host
    /// がこれで `MetricsBridgeHandle::open` し、 DSP load / per-plugin CPU を publish。
    pub metrics_shmem_id: String,
    /// r.md #50: マスター出力サンプルリング (`scope_bridge::scope_shmem_id(pid)`)
    /// の shmem os_id。daw_audio がこれで `ScopeBridgeHandle::open` し、
    /// `render_master_buffer` の出力を毎バッファ書き込む。daw_plugin_host は使わない。
    pub scope_shmem_id: String,
    pub sample_rate: u32,
    pub max_frames: u32,
    pub channels: u16,
}

/// Phase 2 (`docs/plan_automation.md` §7.5): 1 parameter のメタデータ。
/// CLAP `clap_param_info` / VST3 `ParameterInfo` の host 側
/// representation。 `id` の解釈は plugin format ごと:
/// - CLAP: `clap_param_info.id` (`clap_id` = `u32`)
/// - VST3: `Steinberg::Vst::ParamID` = `uint32`
///
/// `min_value` / `max_value` / `default_value` は plain 単位 (= plugin
/// の native スケール)。 VST3 は IEditController が normalized 0..=1 で
/// 扱うため、 plugin_host 側で plain 変換を済ませて送る。
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
    /// 安定 device id (`PluginInstance::id`)。
    pub device_id: u64,
    pub data: Option<Vec<u8>>,
    /// (r.md #5 ARA2) ARA document archive for this device, if it is an ARA
    /// plug-in with a live session. Collected alongside `data` on project save
    /// and stored into `PluginInstance.ara_archive`.
    pub ara_archive: Option<Vec<u8>>,
    pub error: Option<String>,
}

/// Timeline placement + stretch of one ARA playback region, in seconds. Shared
/// by [`AraClipSpec`] (the full clip spec, used to build a document) and
/// [`AraRegionUpdate`] (a lightweight property update matched by id). The
/// *modification* range is the audible source slice — fixed, since the source
/// audio never changes; the *playback* range is where that slice sits on the
/// song timeline. When `time_stretch` is set the plug-in maps the modification
/// slice onto a different playback duration (pitch-preserving), otherwise the
/// two durations are kept equal (Raw — no transformation).
#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct AraRegionPlacement {
    pub start_in_playback_seconds: f64,
    pub duration_in_playback_seconds: f64,
    pub start_in_modification_seconds: f64,
    pub duration_in_modification_seconds: f64,
    /// Enable `kARAPlaybackTransformationTimestretch` on the region. `false` =
    /// playback duration equals modification duration (Raw / no stretch).
    pub time_stretch: bool,
}

/// One audio clip exposed to an ARA plug-in: its source WAV plus its placement
/// on the song timeline. The host (daw_gui) resolves a track's audio clips
/// into these before sending `SetupAraDocument`.
///
/// v29: source は常に絶対 WAV path。旧 `AraSourceSpec::Pcm` (in-memory f32 を
/// wire に直載せ — 3 分 stereo で ~69MB と 16MB wire 上限を必ず超える) は
/// 廃止し、bounce 済み in-memory audio は GUI が project cache へ WAV として
/// materialize してから path を渡す (`docs/plan_arch_refactor.md` §2)。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AraClipSpec {
    /// Decode this absolute WAV path inside the plugin host (on demand).
    pub source_wav: std::path::PathBuf,
    /// Unique, save/restore-stable id for the source within the document.
    pub persistent_id: String,
    pub placement: AraRegionPlacement,
}

/// A lightweight update of an existing ARA playback region's placement, matched
/// to its region by `persistent_id`. Sent via `UpdateAraRegions` when only the
/// timeline placement / stretch of already-present clips changed (manual
/// edge-drag, tempo change, clip move) so the plug-in can
/// `updatePlaybackRegionProperties` in place instead of rebuilding the whole
/// document (which would interrupt playback).
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct AraRegionUpdate {
    pub persistent_id: String,
    pub placement: AraRegionPlacement,
}

// =====================================================================
// gui → daw_audio
// =====================================================================

/// Commands from daw_gui to the audio engine. `LoadSong(Song)` is the
/// dominant size variant, but the `Song` wire form is blob-less by
/// construction (see module doc) so boxing per-variant is unnecessary.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum AudioCommand {
    /// Handshake reply to `AudioEvent::Hello`.
    Ack,
    Play,
    Stop,
    /// パニックボタン — master 出力を declick フェードで一瞬ミュート
    /// する。 panic は直後に（master がミュートされてから）plugin_host へ
    /// `ReinitAllPlugins` が送られるので、 全 plugin を mix から外す瞬間の
    /// 段差クリックがフェードで隠れる。 audio engine は master を fade-out
    /// して **`PanicRelease` が来るまで 0 で hold** する（plugin-host hang
    /// 用に数秒の安全 auto-release あり）。transport の Stop とは独立。
    Panic,
    /// パニックの declick hold を解除して fade-in に移す。 daw_gui が
    /// `PluginEvent::PluginsReinitDone` を受けてから送る。
    PanicRelease,
    Session(AudioSession),
    /// 全量 Song snapshot の再送 (編集 → frame 末 flush の 1 経路のみ)。
    /// wire 形は blob-less (`PluginInstance` の手書き Encode)。
    LoadSong(crate::model::Song),
    /// 再生ループの状態を丸ごと更新する (ON/OFF と範囲は 1 つの値 =
    /// [`crate::model::LoopRegion`] で、 別コマンドに割らない)。 ループは `Song` に
    /// 属さない session state なので `LoadSong` では届かず、 この経路だけが engine の
    /// ループ状態を書き換える。
    SetLoop(crate::model::LoopRegion),
    SetMasterGain(f32),
    /// プラグインが報告した自身の processing latency (samples)。 PDC の入力。
    ///
    /// これは **ユーザーが作った中身ではなく実行時の観測値** なので `Song` には
    /// 載せない (`Song` に持つと保存され、開き直したときに host の報告と食い違って
    /// 「開いただけで `*`」 になる — r.md #9)。 真実源は plugin host で、
    /// daw_gui は `PluginEvent::PluginLatencyChanged` をそのまま中継するだけ。
    ///
    /// 宛先は安定 `device_id` (アーキ不変条件 1)。 track 合計は engine 側が
    /// `compile_schedule` で chain から導出するので、 GUI は集計しない。
    /// device が消えたときは `samples = 0` を送って entry を畳む。
    SetDeviceLatency { device_id: u64, samples: u32 },
    /// Offline-render the song to a WAV file. daw_audio freewheels through
    /// the song using its existing AudioWorker pool + plugin handshake, then
    /// replies with `AudioEvent::ExportWavComplete`.
    ///
    /// `range`:
    /// - `None` — full-song export (frame 0 → `length_beats` + tail).
    /// - `Some((start_frame, end_frame))` — render only that frame window
    ///   (cold start at `start_frame`; matches pressing Play there).
    ExportWav {
        path: std::path::PathBuf,
        range: Option<(u64, u64)>,
        /// Write the modulation-envelope sidecar (`.modenv`) next to the WAV.
        /// Only the offline video render consumes it.
        write_mod_sidecar: bool,
    },
    /// Abort the in-flight offline render (= `ExportWav`). No-op when no
    /// export is running.
    CancelExport,
    /// Offline-render a clip range with the **full plugin chain** (= post-FX)
    /// to a WAV file (`Bounce (with FX)`, `docs/plan_audio_clip.md` §3.8).
    /// Walks the song from frame 0 so plugin state at `start_frame` is fully
    /// accumulated. Replies with `AudioEvent::BounceClipFxComplete`.
    BounceClipFxOnline {
        path: std::path::PathBuf,
        source_track: u32,
        source_clip: u32,
        start_frame: u64,
        end_frame: u64,
    },
    /// Reposition the audio engine's playhead. `samples` is the absolute
    /// frame offset at the engine sample rate. Takes effect on the next
    /// audio buffer regardless of `playing` state.
    SeekTo { samples: u64 },
    /// Tell the audio engine the current project directory so it can
    /// resolve `AudioSourcePath::ProjectRelative` entries.
    SetProjectDir(Option<std::path::PathBuf>),
    /// `track` は stable な `Track::id`。 audio engine 側は
    /// `s.tracks.iter_mut().find(|t| t.id == track)` で look up する。
    /// 値のみの更新 (再 compile 不要、engine が live-read)。
    SetTrackVolume { track: u32, volume: f32 },
    SetTrackPan { track: u32, pan: f32 },
    SetTrackMuted { track: u32, muted: bool },
    SetTrackSolo { track: u32, solo: bool },
    /// Realtime aux-send level update。 `track` = source の `Track::id`、
    /// `send_id` = その track の `sends` 内 stable `Send::id` (v29)。
    /// 値のみの更新 — graph は再 compile されない。
    SetSendGain { track: u32, send_id: u32, gain: f32 },
    /// Realtime per-send mute toggle. Same idiom as `SetSendGain`.
    SetSendEnabled { track: u32, send_id: u32, enabled: bool },
    /// Record-arm 状態。 audio thread は track.armed を Song に反映するのみ。
    SetTrackArmed { track: u32, armed: bool },
    /// BPM 軽量更新 (transport scrub 中に毎 frame 流れうる)。値のみ。
    SetSongBpm { bpm: f32 },
    /// TimeSig 分子の軽量更新。 1..=32 で clamp 想定。
    SetSongTimeSigNumerator { num: u8 },
    /// GUI が現在 recording 中の lane (track + target) を audio thread に
    /// 通知する。 該当 lane の curve eval を bypass する。 空 Vec = なし。
    SetRecordingLanes {
        lanes: Vec<(u32, crate::model::AutomationTarget)>,
    },
    /// メトロノーム on/off。 session-only state。
    SetMetronomeEnabled(bool),
    /// r.md #49: daw_01 の窓 (メイン / 動画プレビュー / プラグインエディタ) のいずれかが
    /// アクティブか。daw_gui が唯一の判定者で、**事実だけ**を運ぶ。
    ///
    /// engine を park するかどうかは engine 側が決める (再生中 / count-in / 書き出し中 /
    /// 出力が無音か、を engine だけが知っているため)。GUI は「窓がアクティブか」という
    /// 事実だけを運ぶ。
    SetAppActive(bool),
    /// 鍵盤レーン click のピッチプレビュー単発 note-on。 `track_id` は
    /// stable `Track::id`。 transport 状態に関係なく発音する。
    PreviewNoteOn {
        track_id: u32,
        pitch: u8,
        velocity: u8,
    },
    /// 鍵盤プレビューの note-off。
    PreviewNoteOff { track_id: u32, pitch: u8 },
    /// r.md #51: 録音セッションの開始。 engine はこれを受けて
    /// 1. `preroll_samples > 0` なら count-in に入る (preroll 中は dispatch /
    ///    clip render を skip して metronome のみ render)、
    /// 2. 曲末の auto-stop を抑止する (録音は曲の後ろへ継ぎ足せる)、
    /// 3. `recording_live` (= 録音要求 && 再生中 && count-in 完了) を publish する。
    ///
    /// 「count-in の開始」ではなく **録音そのもの**を運ぶ。engine が曲末 auto-stop を
    /// 抑止するにも、count-in 明けを GUI に知らせるにも「録音中か」が要るため。
    StartRecording { preroll_samples: u64 },
    /// r.md #51: 録音セッションの終了 (パンチアウト / 停止 / count-in 取り消し)。
    /// engine は preroll を捨て、auto-stop の抑止と `recording_live` を解除する。
    /// transport は **止めない** — 停止は `Stop` の仕事 (パンチアウトは再生継続)。
    StopRecording,
    /// Stand up the per-buffer plugin process worker pool. `n_workers`
    /// audio-engine workers pair 1:1 with plugin-host workers via the named
    /// events listed. イベント名は世代 (generation) 込みで daw_gui が mint
    /// する — pool 再構築時に stale な auto-reset signal を旧世代へ隔離する
    /// (`plugin_ref` の poisoning contract 参照)。
    OpenWorkerPool {
        n_workers: u32,
        worker_bridge_shmem_id: String,
        wake_event_names: Vec<String>,
        done_event_names: Vec<String>,
    },
    /// Tear down the worker pool started by `OpenWorkerPool`.
    CloseWorkerPool,
    /// Map a `ProcessData` shmem region into the audio engine. 配置
    /// (track / chain 位置) は Song 側の `PluginInstance::id` から解決する
    /// ので positional 情報は運ばない。
    OpenPluginShmem { device_id: u64, shmem_id: String },
    /// Drop the `ProcessData` mapping for `device_id` after the plugin
    /// instance is being torn down.
    ClosePluginShmem { device_id: u64 },
}

// =====================================================================
// daw_audio → gui
// =====================================================================

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum AudioEvent {
    Hello {
        pid: u32,
        /// daw_audio がオープン予定の出力デバイスの実サンプルレート (Hz)。
        /// 親 (daw_gui) はこれを `AudioSession.sample_rate` の SSoT にする
        /// (= エンジンはハードウェアのレートで動く)。 query 失敗時は `None`
        /// (親は `audio_bridge::DEFAULT_SAMPLE_RATE` へ fallback)。
        device_sample_rate: Option<u32>,
        /// ビルド世代検証 (module doc 参照)。
        protocol_fingerprint: u64,
    },
    /// IPC pipe が切断された (= 子 exit / panic / decode 失敗 / **writer
    /// task 死**)。 daw_gui 内部の pipe loop が合成する synthetic event。
    /// 受信で該当 child を re-spawn し Session / worker pool / LoadSong /
    /// plugin slots を再構築する。
    ChildDisconnected,
    /// Offline WAV export finished, was cancelled, or failed.
    ExportWavComplete {
        error: Option<String>,
        cancelled: bool,
    },
    /// Offline WAV render progress (throttled by sender).
    ExportWavProgress { done: u64, total: u64 },
    /// Offline plugin-FX bounce finished (or failed).
    BounceClipFxComplete {
        path: std::path::PathBuf,
        source_track: u32,
        source_clip: u32,
        error: Option<String>,
        frames: u64,
    },
    /// (v29, `docs/plan_arch_refactor.md` §4) worker dispatch が
    /// `DISPATCH_TIMEOUT_MS` 内に完了せず、該当 device を quarantine した
    /// (以後 mix から外して無音バイパス)。 GUI は該当デバイスを可視化し、
    /// plugin_host respawn / 再ロードで解除する。
    PluginUnresponsive { device_id: u64 },
    /// (v29) worker pool 全体の完了待ちが timeout した = plugin_host が
    /// 応答不能 (ハード crash / ハング)。 GUI は plugin_host を respawn する。
    WorkerPoolStalled,
}

// =====================================================================
// gui → daw_plugin_host
// =====================================================================

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum PluginCommand {
    /// Handshake reply to `PluginEvent::Hello`.
    Ack,
    Session(AudioSession),
    /// Project directory (ARA WAV 解決等)。
    SetProjectDir(Option<std::path::PathBuf>),
    /// Reinitialise (deactivate→activate) every loaded plugin to a clean
    /// state (export 前 / パニックボタン)。 完了で
    /// `PluginEvent::PluginsReinitDone` を返す。
    ReinitAllPlugins,
    /// 全 loaded plugin の CLAP render mode を切り替える (export bookend)。
    SetRenderMode(RenderMode),
    /// Builtin plugin (`PluginFormat::Builtin`) に per-note metadata を
    /// flush する (`docs/plan_voicevox_synth.md` PR-V2.2)。
    SetBuiltinPluginNoteMetadata {
        device_id: u64,
        bpm: f32,
        entries: Vec<crate::plugin_metadata::NoteMetadata>,
        /// (talk) 同トラックの `ClipContent::Text` 由来の読み上げ群。
        talk: Vec<crate::plugin_metadata::TalkMetadata>,
    },
    /// 歌唱 bounce の前に builtin VOICEVOX の合成完了を要求する。 完了で
    /// `PluginEvent::VocalSynthReady` が返る。
    PrepareVocalSynth { device_id: u64 },
    /// Load / replace the plugin instance for device `device_id` (安定 id、
    /// `Song.next_device_id` 採番)。 `track_id` は所属 track (master fx は
    /// `MASTER_TRACK_ID`) — teardown (`RemoveTrack`) 用の帰属情報で、
    /// アドレスには使わない。 `generation` は per-device 単調増加の要求世代
    /// — 応答 (`SlotPluginLoaded` / `SlotPluginLoadFailed`) に echo され、
    /// GUI は最新世代のみ受理する (A→B 連続差し替えの stale 応答 race 対策)。
    SetSlotPlugin {
        device_id: u64,
        track_id: u32,
        format: PluginFormat,
        path: std::path::PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
        generation: u64,
    },
    /// Remove the plugin instance for `device_id` if any.
    RemoveSlotPlugin { device_id: u64 },
    /// Drop every plugin instance belonging to `track_id` (= track 削除)。
    RemoveTrack { track_id: u32 },
    /// **別プロジェクトに切り替わったので instance を全部捨てる。**
    ///
    /// `device_id` (= `PluginInstance::id`) は Song スコープの名前で、
    /// `IdAllocators` が project ごとに 1 から再採番する。plugin_host は
    /// `instances` をこの id で引くので、前 project の instance が残っていると
    /// 新 project の `SetSlotPlugin` が **同 id・同 plugin_id の dedup に吸収され**、
    /// 保存済み state を復元しないまま旧 instance で鳴ってしまう。
    ///
    /// `RemoveTrack` の積み重ねでは塞げない: 列挙元は daw_gui 側の帳簿
    /// (`loaded_slots`) で、load 応答が返る前に切り替わった device は帳簿に
    /// 載っておらず、以後 **永久に** 回収対象にならない (漏れが自己増殖する)。
    /// 「全部捨てろ」は帳簿に依存しない唯一の表現。
    UnloadAllPlugins,
    /// Ask the plugin_host to capture state for one device. Reply is
    /// `PluginEvent::SlotPluginState`.
    RequestSlotState { device_id: u64 },
    /// Ask the plugin_host to capture state for every device at once.
    /// Reply is `PluginEvent::AllPluginStates`. Used for project save.
    RequestAllStates,
    /// Open the plugin editor (top-level window は plugin-host プロセス所有)。
    /// `title` is the window caption daw_gui composed.
    OpenSlotGuiEmbedded { device_id: u64, title: String },
    CloseSlotGui { device_id: u64 },
    /// r.md #36: プラグインエディタ窓で押されたとき daw_gui へ転送してよいキーの一覧。
    ///
    /// **キー割り当ての意味論は daw_gui の `SHORTCUTS` テーブルだけが持つ**。
    /// plugin-host は受け取った chord 列と Win32 の仮想キー / 修飾を突き合わせるだけで、
    /// 「Space = 再生」 のような policy を一切知らない (= SSoT を割らない)。
    /// handshake 後に 1 度送り、 以後 SHORTCUTS が実行時に変わったら再送する。
    SetEditorForwardedKeys { chords: Vec<KeyChord> },
    /// r.md #36: この device のエディタ窓で **キーを一切横取りしない** (= REAPER の
    /// 「Send all keyboard input to plug-in」)。 Dear ImGui / 自前 OpenGL 系のように
    /// 「今テキスト入力中か」 を外から知る手段が原理的に無い GUI 用の逃げ道。
    SetEditorSendAllKeys { device_id: u64, enabled: bool },
    /// Worker pool の plugin_host 側 open (audio 側と対で送られる)。
    OpenWorkerPool {
        n_workers: u32,
        worker_bridge_shmem_id: String,
        wake_event_names: Vec<String>,
        done_event_names: Vec<String>,
    },
    CloseWorkerPool,
    /// (r.md #5 ARA2) Build/replace the ARA document for the ARA-capable
    /// device: expose `clips` as ARA audio sources + playback regions and
    /// bind the instance for playback rendering.
    SetupAraDocument {
        device_id: u64,
        clips: Vec<AraClipSpec>,
        /// Project tempo (bpm) and time signature. Fed into the ARA musical
        /// context so the plug-in's editor grid aligns to the song.
        bpm: f64,
        time_sig: (u16, u16),
        /// Prior ARA edit archive to restore after (re)building the document
        /// (from `PluginInstance.ara_archive`). `None` for a fresh document.
        archive: Option<Vec<u8>>,
    },
    /// (r.md #5 ARA2) Tear down the ARA document/session for `device_id`.
    ClearAraDocument { device_id: u64 },
    /// (r.md #7 ARA2) Update only the playback-region placements of an
    /// existing ARA document (matched by `persistent_id`).
    UpdateAraRegions {
        device_id: u64,
        regions: Vec<AraRegionUpdate>,
    },
}

// =====================================================================
// daw_plugin_host → gui
// =====================================================================

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum PluginEvent {
    Hello {
        pid: u32,
        /// ビルド世代検証 (module doc 参照)。
        protocol_fingerprint: u64,
    },
    /// IPC pipe 切断の synthetic event (`AudioEvent::ChildDisconnected` 同様)。
    ChildDisconnected,
    /// r.md #36: プラグインエディタ窓でキーが押され、 **プラグインがそれを消化しなかった**
    /// ので daw_gui へ返す。 daw_gui は自分の `SHORTCUTS` で chord → shortcut 名を解決し、
    /// メインウィンドウで押されたのと同じ経路 (`take_shortcut`) に合流させる。
    ///
    /// 「消化しなかった」 の判定根拠 (`daw_plugin_host::editor_keys` 参照):
    /// - JUCE / iPlug2 は未消化キーを親 / ルート HWND (= 我々のエディタ窓) へ転送する規約を
    ///   持つので、 **こちらの WNDPROC に届いた時点で未消化が確定**する。
    /// - VSTGUI 等はフレーム窓が `WM_GETDLGCODE` に応答しない一方、 文字編集中は本物の
    ///   Win32 EDIT を生成するので、 フォーカス窓への `WM_GETDLGCODE` 問い合わせで判別できる。
    EditorKey { device_id: u64, chord: KeyChord },
    /// r.md #49: このプロセスが所有する窓 (= プラグインエディタ) がアクティブになった /
    /// 非アクティブになった。`WM_ACTIVATEAPP` 由来。
    ///
    /// エディタ窓は **daw_plugin_host が所有する owner 無し top-level** で、daw_gui を
    /// owner にすることは設計上禁止されている (`GetAncestor(GA_ROOTOWNER)` が daw_gui に
    /// 解決すると JUCE の cascade サブメニューが `isForegroundProcess()` 判定で即 dismiss
    /// される — `daw_plugin_host::editor_window` の冒頭コメント)。よって「プラグイン GUI を
    /// 触っている間もアプリはアクティブ」を daw_gui 内の情報だけで判定することは**原理的に
    /// できず**、このプロセスが自分で報告するしかない。
    HostWindowsActive(bool),
    /// Reply to `PluginCommand::ReinitAllPlugins`.
    PluginsReinitDone,
    /// builtin VOICEVOX の歌唱合成が `PrepareVocalSynth` で要求した世代まで
    /// 完了した (or タイムアウトした)。
    VocalSynthReady { device_id: u64 },
    /// Plugin-host confirmed `SetSlotPlugin` and reported the descriptor
    /// that actually loaded. `shmem_id` names the `ProcessData` shmem so
    /// daw_audio can `OpenShared` it.
    ///
    /// `state_load_error` は saved state の復元失敗理由 (`None` = 新規 or
    /// 成功)。 `generation` は要求の echo (最新世代のみ受理)。
    SlotPluginLoaded {
        device_id: u64,
        id: String,
        name: String,
        shmem_id: String,
        state_load_error: Option<String>,
        /// パラアウト: how many `is_main=false` audio output ports this
        /// plugin declared (0 for the common single-out case)。
        aux_output_count: u8,
        generation: u64,
    },
    /// `SetSlotPlugin` の load が失敗した。 song の slot は touch されない。
    SlotPluginLoadFailed {
        device_id: u64,
        plugin_id: String,
        reason: String,
        generation: u64,
    },
    /// Reply to `RequestSlotState`. `None` = plugin unavailable or state
    /// extension missing.
    SlotPluginState {
        device_id: u64,
        data: Option<Vec<u8>>,
    },
    /// Reply to `RequestAllStates`: one entry per loaded device.
    AllPluginStates { entries: Vec<SlotState> },
    /// GUI opened at the requested size.
    SlotGuiOpened {
        device_id: u64,
        width: u32,
        height: u32,
    },
    /// Plugin-initiated close (X button handled by plugin, or `closed`).
    SlotGuiClosed { device_id: u64 },
    /// Plugin host がこの device の `ProcessData` shmem を破棄した。
    /// **teardown のたびに必ず先行して発火する** (replace = 同 device に別
    /// plugin を載せ直す経路も含む)。 daw_gui はこれを受けて
    /// `AudioCommand::ClosePluginShmem { device_id }` を daw_audio に転送し、
    /// audio engine の stale mapping を落とす。
    ///
    /// 「shmem が死んだ」 は 「device が空になった」 ([`Self::SlotPluginUnloaded`])
    /// とは**別の事実**なので別 event にしてある。 replace 経路で前者だけを
    /// 送らないと、 daw_audio が **旧** mapping へ入力を書き、 plugin_host の
    /// worker は registry の **新** mapping を読む窓が開く (数バッファ分の
    /// 無音 / 取りこぼし)。 close が先行していれば daw_audio 側 entry が
    /// 一時的に消え、 RT はその device を skip する (= ロード完了までの
    /// dispatch 抑止)。
    SlotPluginShmemReleased { device_id: u64 },
    /// Plugin host destroyed a plugin instance (RemoveSlotPlugin /
    /// RemoveTrack 経由)。 daw_gui はこれを受けて daw_gui ローカルの
    /// bookkeeping (`track_plugin_ids` / `loaded_slots` / latency 等) を
    /// 片付ける。 shmem の解放は必ず先行する
    /// [`Self::SlotPluginShmemReleased`] が担う (SSoT — 二重に送らない)。
    SlotPluginUnloaded { device_id: u64 },
    /// Plugin が報告した自身の processing latency (samples 単位)。 activate
    /// 直後、および restart / reinit 完了直後に発火。 daw_gui は
    /// これを [`AudioCommand::SetDeviceLatency`] としてそのまま engine へ中継し、
    /// engine が PDC を再計算する (集計も保存もしない — r.md #9)。
    PluginLatencyChanged { device_id: u64, samples: u32 },
    /// Plugin の parameter 一覧。 activate 完了直後に 1 度、 rescan 要求で
    /// 再送。
    PluginParamList {
        device_id: u64,
        params: Vec<PluginParamInfo>,
        /// この plugin が埋め込み GUI を持つか。
        has_embedded_gui: bool,
    },
    /// Plugin GUI で knob を **touch** した通知。
    PluginParamTouched {
        device_id: u64,
        param_id: u32,
        display_name: String,
    },
    /// Plugin GUI 内で parameter 値が変更された通知。
    PluginParamValueChanged {
        device_id: u64,
        param_id: u32,
        value: f64,
    },
    /// Plugin GUI で knob を release した通知 (gesture end)。
    PluginParamGestureEnd { device_id: u64, param_id: u32 },
    /// builtin VOICEVOX plugin の合成スレッドの状態遷移。
    VoicevoxSynthStatus {
        device_id: u64,
        busy: bool,
        /// 直近試行の失敗種別 (engine 到達可否で区別)。
        failure: VocalSynthFailure,
    },
}

/// builtin VOICEVOX 合成の失敗種別。engine に**到達できない** (未起動 / 起動途中 /
/// timeout = transient、retry する) のか、engine は**到達できたが入力を拒否**した
/// (HTTP 4xx/5xx = 例: 不正な歌詞。同 job を retry しても無駄なので retry しない) のかを
/// 区別する。後者を「engine 未接続」と誤表示しない / 無限 retry しないための SSoT。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub enum VocalSynthFailure {
    /// 直近試行は失敗していない (成功 or まだ試行なし)。
    None,
    /// engine に到達できない (接続拒否 / timeout / 未起動・起動途中)。transient。
    Unreachable,
    /// engine は応答したが入力を拒否した (HTTP 4xx/5xx)。`detail` は VOICEVOX が返した
    /// 短い理由 (例: `lyricが不正です: ー`)。同じ入力での retry はしない (編集し直しを待つ)。
    Rejected { detail: String },
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
    fn protocol_fingerprint_is_nonzero() {
        assert_ne!(PROTOCOL_FINGERPRINT, 0);
    }

    #[test]
    fn child_kind_as_str() {
        assert_eq!(ChildKind::Audio.as_str(), "audio");
        assert_eq!(ChildKind::PluginHost.as_str(), "plugin_host");
    }

    #[test]
    fn audio_hello_roundtrip() {
        let msg = AudioEvent::Hello {
            pid: 12345,
            device_sample_rate: Some(44_100),
            protocol_fingerprint: PROTOCOL_FINGERPRINT,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn plugin_hello_roundtrip() {
        let msg = PluginEvent::Hello {
            pid: 4242,
            protocol_fingerprint: PROTOCOL_FINGERPRINT,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn audio_command_transport_roundtrip() {
        assert_eq!(roundtrip(&AudioCommand::Ack), AudioCommand::Ack);
        assert_eq!(roundtrip(&AudioCommand::Play), AudioCommand::Play);
        assert_eq!(roundtrip(&AudioCommand::Stop), AudioCommand::Stop);
    }

    #[test]
    fn load_song_roundtrip() {
        let msg = AudioCommand::LoadSong(crate::model::Song::default());
        assert_eq!(roundtrip(&msg), msg);
    }

    /// r.md #49: アイドル省電力の 2 本の新 wire。
    #[test]
    fn idle_power_roundtrip() {
        for active in [true, false] {
            assert_eq!(
                roundtrip(&AudioCommand::SetAppActive(active)),
                AudioCommand::SetAppActive(active)
            );
            assert_eq!(
                roundtrip(&PluginEvent::HostWindowsActive(active)),
                PluginEvent::HostWindowsActive(active)
            );
        }
    }

    /// wire を渡る `Song` は blob-less であること (`PluginInstance` の手書き
    /// Encode が `state` / `ara_archive` を構造的に除外する)。MB 級 blob を
    /// 持つ Song でも LoadSong の encoded size は KB 級に留まり、decode 側は
    /// blob フィールドを常に `None` で受ける。
    #[test]
    fn load_song_wire_form_is_blob_less() {
        use crate::model::{PluginInstance, Song, Track};
        use crate::plugin_format::PluginFormat;

        let mut song = Song::default();
        let mut dev = PluginInstance::new("test.reverb".into(), PluginFormat::Clap);
        dev.id = 1;
        dev.state = Some(vec![0xAB; 4 * 1024 * 1024].into());
        dev.ara_archive = Some(vec![0xCD; 4 * 1024 * 1024].into());
        song.tracks.push(Track {
            id: 1,
            devices: vec![dev],
            ..Track::default()
        });

        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(AudioCommand::LoadSong(song), cfg).unwrap();
        assert!(
            bytes.len() < 64 * 1024,
            "LoadSong with 8MB of blobs must stay small on the wire, got {} bytes",
            bytes.len()
        );
        let (decoded, _): (AudioCommand, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        let AudioCommand::LoadSong(s) = decoded else {
            panic!("expected LoadSong");
        };
        assert_eq!(s.tracks[0].devices[0].id, 1);
        assert!(s.tracks[0].devices[0].state.is_none());
        assert!(s.tracks[0].devices[0].ara_archive.is_none());
    }

    #[test]
    fn export_wav_complete_roundtrip() {
        let ok = AudioEvent::ExportWavComplete { error: None, cancelled: false };
        assert_eq!(roundtrip(&ok), ok);
        let cancelled = AudioEvent::ExportWavComplete { error: None, cancelled: true };
        assert_eq!(roundtrip(&cancelled), cancelled);
        let failed = AudioEvent::ExportWavComplete {
            error: Some("render failed".to_string()),
            cancelled: false,
        };
        assert_eq!(roundtrip(&failed), failed);
    }

    #[test]
    fn plugin_unresponsive_roundtrip() {
        let msg = AudioEvent::PluginUnresponsive { device_id: 77 };
        assert_eq!(roundtrip(&msg), msg);
        assert_eq!(roundtrip(&AudioEvent::WorkerPoolStalled), AudioEvent::WorkerPoolStalled);
    }

    #[test]
    fn set_slot_plugin_roundtrip() {
        let msg = PluginCommand::SetSlotPlugin {
            device_id: 42,
            track_id: 7,
            format: PluginFormat::Clap,
            path: std::path::PathBuf::from("C:/plugins/test.clap"),
            plugin_id: "test.synth".into(),
            initial_state: Some(vec![1, 2, 3]),
            generation: 9,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn all_plugin_states_roundtrip() {
        let msg = PluginEvent::AllPluginStates {
            entries: vec![
                SlotState {
                    device_id: 1,
                    data: Some(vec![1, 2, 3, 4]),
                    ara_archive: None,
                    error: None,
                },
                SlotState {
                    device_id: 9,
                    data: None,
                    ara_archive: None,
                    error: Some("state save failed".to_string()),
                },
            ],
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn plugin_param_list_roundtrip() {
        let msg = PluginEvent::PluginParamList {
            device_id: 3,
            params: vec![PluginParamInfo {
                id: 100,
                name: "Gain".to_string(),
                module: "Mixer".to_string(),
                min_value: 0.0,
                max_value: 1.0,
                default_value: 0.5,
                flags: plugin_param_flags::AUTOMATABLE,
            }],
            has_embedded_gui: true,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn builtin_note_metadata_roundtrip() {
        let msg = PluginCommand::SetBuiltinPluginNoteMetadata {
            device_id: 9,
            bpm: 128.0,
            entries: vec![crate::plugin_metadata::NoteMetadata {
                note_id: 0,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: "あ".to_string(),
                clip_id: 5,
                speaker_id: 3061,
            }],
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
