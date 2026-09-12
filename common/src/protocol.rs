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
//! - **プロジェクト (= タブ) は [`ProjectKey`] で名指しする** (`docs/plan_project_tabs.md` §1):
//!   `device_id` / `track_id` / clip id は Song 内でしか一意でない (project ごとに 1 から
//!   採番) ので、複数プロジェクトを同時に開く以上、Song 由来の名前を運ぶ message は
//!   すべて `project` を組で運ぶ。プラグインは [`DeviceAddr`] (`project` + `device_id`)。
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

/// 開いているプロジェクト (= タブ) の住所 (`docs/plan_project_tabs.md` §1.1)。
///
/// daw_gui が採番する単調増加の u64 で、**プロセス生存中に再利用しない** (閉じたタブへの
/// 遅延メッセージが新しいタブへ誤配送されない)。`0` は未割当 sentinel。
///
/// `Song::project_id` (ファイルに永続化されるランダム u64) とは別物 — あちらは
/// 「同じファイルか」(clipboard の link / copy 判定、engine の song-scoped state の
/// リセット判定) を表し、こちらは「どのタブか」を表す。同じファイルを 2 タブで開いても
/// `ProjectKey` は別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ProjectKey(pub u64);

impl ProjectKey {
    /// 未割当 sentinel。shmem の空きスロット印にも使う。
    pub const NONE: ProjectKey = ProjectKey(0);

    #[must_use]
    pub fn is_some(self) -> bool {
        self.0 != 0
    }
}

/// プラグインインスタンスの住所 (`docs/plan_project_tabs.md` §1.2)。
///
/// `device_id` (= `PluginInstance::id`) は Song 内でしか一意でない (`IdAllocators` が
/// project ごとに 1 から採番する) ので、プロセス境界では必ず [`ProjectKey`] と組で運ぶ。
/// daw_plugin_host の帳簿はこの組で引く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct DeviceAddr {
    pub project: ProjectKey,
    pub device_id: u64,
}

impl DeviceAddr {
    #[must_use]
    pub const fn new(project: ProjectKey, device_id: u64) -> Self {
        Self { project, device_id }
    }
}

/// plugin_host が `SetSlotPlugin` ごとに採番する **プロセス生存中に一意な** インスタンス番号
/// (`docs/plan_project_tabs.md` §1.3)。`ProcessData` shmem 名の incarnation と同じ値。
///
/// **worker dispatch (`WorkerBridge.worker_task[i]`) と `MetricsBridge` の per-plugin slot は
/// これで引く** — どちらも「1 ワードで instance を名指しする」必要があり、`DeviceAddr`
/// (2 ワード) をそのまま atomic に載せられない。再利用されないので、閉じたタブの
/// instance を新しい instance と取り違えることも無い。`0` = 未割当。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct InstanceToken(pub u64);

impl InstanceToken {
    pub const NONE: InstanceToken = InstanceToken(0);
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
    /// r.md #117: CLAP `CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID` (ノート単位の `param_mod`)。
    /// VST3 は立てない。
    pub const MODULATABLE_PER_NOTE_ID: u32 = 1 << 7;
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
///
/// `project` は外側 ([`PluginEvent::AllPluginStates`]) が持つので、ここは Song 内の
/// `device_id` だけ。
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

/// Commands from daw_gui to the audio engine. `LoadSong` is the
/// dominant size variant, but the `Song` wire form is blob-less by
/// construction (see module doc) so boxing per-variant is unnecessary.
///
/// **`project` を持つ variant はその project (= タブ) の engine slot に効く**
/// (`docs/plan_project_tabs.md` §3)。持たない variant はデバイス全体
/// (Panic / park / worker pool / Global Sampler の試聴 / 終了)。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum AudioCommand {
    /// Handshake reply to `AudioEvent::Hello`.
    Ack,
    Session(AudioSession),
    /// プロジェクト (= タブ) の engine slot を作る。`LoadSong` より前に必ず 1 回。
    /// engine は off-thread で per-project の RT 状態 (scratch / launcher / mod tick /
    /// bus) を構築し、telemetry (`AudioBridge`) の空き slot を `project` で claim する。
    /// `MAX_PROJECTS` を超えたら無視 (GUI 側が先に cap を守る)。
    OpenProject { project: ProjectKey },
    /// プロジェクトの engine slot を捨てる (タブを閉じた)。RT から外し off-thread で
    /// drop、telemetry slot を解放する。以後この `project` 宛の command は捨てられる。
    CloseProject { project: ProjectKey },
    /// `ScopeBridge` (マスターメーター / ラウドネス) と Global Sampler の `Master`
    /// ソースが書く master をこの project にする (= アクティブなタブ)。
    /// タブ切替で daw_gui が送る。
    SetScopeProject { project: ProjectKey },
    Play { project: ProjectKey },
    /// r.md #118: 停止した位置から続ける再生 (Shift+Space)。 `Play` と違いランチャーのセルを
    /// 撃ち直さず、 止まったときの位相のまま鳴らす。
    PlayContinue { project: ProjectKey },
    Stop { project: ProjectKey },
    /// パニックボタン — master 出力を declick フェードで一瞬ミュート
    /// する。 panic は直後に（master がミュートされてから）plugin_host へ
    /// `ReinitAllPlugins` が送られるので、 全 plugin を mix から外す瞬間の
    /// 段差クリックがフェードで隠れる。 audio engine は master を fade-out
    /// して **`PanicRelease` が来るまで 0 で hold** する（plugin-host hang
    /// 用に数秒の安全 auto-release あり）。transport の Stop とは独立。
    /// デバイス全体 (全 project の合算出力) に掛かる。
    Panic,
    /// パニックの declick hold を解除して fade-in に移す。 daw_gui が
    /// `PluginEvent::PluginsReinitDone` を受けてから送る。
    PanicRelease,
    /// 全量 Song snapshot の再送 (編集 → frame 末 flush の 1 経路のみ)。
    /// wire 形は blob-less (`PluginInstance` の手書き Encode)。
    LoadSong { project: ProjectKey, song: crate::model::Song },
    /// 再生ループの状態を丸ごと更新する (ON/OFF と範囲は 1 つの値 =
    /// [`crate::model::LoopRegion`] で、 別コマンドに割らない)。 ループは `Song` に
    /// 属さない session state なので `LoadSong` では届かず、 この経路だけが engine の
    /// ループ状態を書き換える。
    SetLoop { project: ProjectKey, region: crate::model::LoopRegion },
    SetMasterGain { project: ProjectKey, gain: f32 },
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
    SetDeviceLatency { project: ProjectKey, device_id: u64, samples: u32 },
    /// Offline-render the song to a WAV file. daw_audio freewheels through
    /// the song using its existing AudioWorker pool + plugin handshake, then
    /// replies with `AudioEvent::ExportWavComplete`.
    ///
    /// `range` (r.md #54 で **拍** へ統一):
    /// - `None` — full-song export (frame 0 → `length_beats` + tail).
    /// - `Some((start_beat, end_beat))` — render only that window
    ///   (cold start at `start_beat`; matches pressing Play there).
    ///
    /// 拍→サンプル換算は **daw_audio 側だけ**が行う (`beats_to_samples` =
    /// tempo automation を積分する SSoT)。GUI 側で定数 BPM 換算してフレームを
    /// 送っていた旧形は、テンポカーブのある曲で engine の走査位置と範囲の端が
    /// ずれるため撤去した。
    ///
    /// オフライン描画はエンジン全体で同時に 1 本 (`export_running` 予約)。描画中は
    /// **全 project の live 出力が無音**になり、他のタブの再生位置もその間だけ止まる。
    ExportWav {
        project: ProjectKey,
        path: std::path::PathBuf,
        range: Option<(f64, f64)>,
        /// **オフラインの動画書き出しが差す sidecar 一式**を WAV の隣へ書くか。
        ///
        /// - `.modenv` — 変調エンベロープ
        ///   ([`common::mod_sidecar::ModEnvSidecar`](crate::mod_sidecar::ModEnvSidecar))
        /// - `.launcher` — ランチャーの走行状態の遷移列 (r.md #87)
        ///   ([`common::launcher_sidecar::LauncherSidecar`](crate::launcher_sidecar::LauncherSidecar))
        ///
        /// どちらも「動画側は live の shmem 平面を読めない」という同じ理由で焼く。
        /// 単体の WAV 書き出し / クリップ bounce は消費者が居ないので `false`。
        /// (フィールド名は `.modenv` しか無かった頃のまま — GUI 側の呼び名を
        /// 変えると 2 つの束が同時に触ることになるので、名前は据え置いてある)
        write_mod_sidecar: bool,
    },
    /// (r.md #54) 範囲のラウドネスをオフラインで解析する。`ExportWav` と
    /// **同じ freewheel 経路** (`render_master_buffer` → 出力先だけ差し替え) を
    /// 通るので、解析値は同じ範囲を書き出した WAV の値と一致する。
    ///
    /// `range` は拍。`None` = 全曲。減衰 tail は測らない (範囲ちょうど)。
    /// 途中経過は [`AudioEvent::LoudnessAnalysisProgress`]、確定は
    /// [`AudioEvent::LoudnessAnalysisComplete`]。中断は `CancelExport` と共通。
    AnalyzeLoudness { project: ProjectKey, range: Option<(f64, f64)> },
    /// Abort the in-flight offline render (= `ExportWav` / `AnalyzeLoudness`).
    /// No-op when no offline render is running.
    CancelExport,
    /// Offline-render a range of the **currently loaded song** to a WAV file
    /// (`Bounce`, `docs/plan_audio_clip.md` §3.8 / `J` Glue の焼き込み、
    /// `docs/plan_glue_bake.md`)。 送り手が先に `LoadSong` で対象トラックだけを
    /// 残した song を積むので、ここでは「今の song の `[start_beat, end_beat)` を
    /// 焼く」以上の意味は持たない。 Replies with `AudioEvent::BounceClipFxComplete`。
    /// 範囲は拍 (`ExportWav` と同じく換算は engine 側 SSoT)。
    ///
    /// `warm` = 曲頭から走査するか。 plugin chain を通す bounce は tail /
    /// sidechain / パラメータランプが `start_beat` 時点で積み上がっている必要が
    /// あるので `true`。 insert を外して素材だけを焼く Glue は「その位置で再生を
    /// 押した音」が欲しいので `false` (= 範囲頭から cold で走る。 曲頭からの空走査を
    /// トラック数ぶん繰り返さない)。
    BounceClipFxOnline {
        project: ProjectKey,
        path: std::path::PathBuf,
        source_track: u32,
        source_clip: u32,
        start_beat: f64,
        end_beat: f64,
        warm: bool,
    },
    /// Reposition the audio engine's playhead. `samples` is the absolute
    /// frame offset at the engine sample rate. Takes effect on the next
    /// audio buffer regardless of `playing` state.
    SeekTo { project: ProjectKey, samples: u64 },
    /// Tell the audio engine the project directory so it can
    /// resolve `AudioSourcePath::ProjectRelative` entries.
    SetProjectDir { project: ProjectKey, dir: Option<std::path::PathBuf> },
    /// `track` は stable な `Track::id`。 audio engine 側は
    /// `s.tracks.iter_mut().find(|t| t.id == track)` で look up する。
    /// 値のみの更新 (再 compile 不要、engine が live-read)。
    SetTrackVolume { project: ProjectKey, track: u32, volume: f32 },
    SetTrackPan { project: ProjectKey, track: u32, pan: f32 },
    SetTrackMuted { project: ProjectKey, track: u32, muted: bool },
    SetTrackSolo { project: ProjectKey, track: u32, solo: bool },
    /// 内蔵チャンネルストリップ (コンプ + EQ) の設定を丸ごと差し替える
    /// (`docs/plan_channel_strip.md`)。値のみの更新 — graph は再 compile しない。
    ///
    /// パラメータ 1 個ごとの variant を並べず **構造体 1 個**を送る:
    /// `ChannelStrip` は 20 個ほどの数値だけの `Copy` 型 (数十バイト) なので
    /// bulk には当たらず (不変条件 2)、「どのフィールドを送ったか」を送信側と
    /// 受信側の 2 か所で数え合わせる必要も無くなる。
    SetTrackStrip { project: ProjectKey, track: u32, strip: crate::model::ChannelStrip },
    /// マスターストリップ (バスコンプ + トーン EQ + リミッター) を丸ごと差し替える
    /// (`docs/plan_master_strip.md`)。`SetTrackStrip` と同じく値のみの更新。
    SetMasterStrip { project: ProjectKey, strip: crate::model::MasterStrip },
    /// Realtime aux-send level update。 `track` = source の `Track::id`、
    /// `send_id` = その track の `sends` 内 stable `Send::id` (v29)。
    /// 値のみの更新 — graph は再 compile されない。
    SetSendGain { project: ProjectKey, track: u32, send_id: u32, gain: f32 },
    /// Realtime per-send mute toggle. Same idiom as `SetSendGain`.
    SetSendEnabled { project: ProjectKey, track: u32, send_id: u32, enabled: bool },
    /// r.md #110 Parallel: chain の gain (linear) の値のみ更新。 `track` = chain を持つ
    /// track (master は `MASTER_TRACK_ID`)、 `chain_id` = 安定 `ParallelChain::id`。
    /// graph は再 compile されない (`SetSendGain` と同じ idiom)。
    SetChainGain { project: ProjectKey, track: u32, chain_id: u64, gain: f32 },
    /// r.md #110 Parallel: chain の pan (`-1..=1`) の値のみ更新。
    SetChainPan { project: ProjectKey, track: u32, chain_id: u64, pan: f32 },
    /// r.md #110 Parallel: chain の mute。 RT は Song snapshot から live-read する。
    SetChainMuted { project: ProjectKey, track: u32, chain_id: u64, muted: bool },
    /// r.md #110 Parallel: chain の solo (同じ Parallel 内の他 chain を黙らせる)。
    SetChainSolo { project: ProjectKey, track: u32, chain_id: u64, solo: bool },
    /// r.md #110 Parallel: 出力 trim (linear) の値のみ更新。 `parallel_id` = 安定 `Parallel::id`。
    SetParallelOutGain { project: ProjectKey, track: u32, parallel_id: u64, gain: f32 },
    /// r.md #110 Parallel: gain match の on / off (RT は Song snapshot から live-read)。
    SetParallelGainMatch { project: ProjectKey, track: u32, parallel_id: u64, on: bool },
    /// r.md #112 Parallel: 帯域分割のクロスオーバー周波数 (Hz) の値のみ更新。 順序 (`low <= high`)
    /// と値域は `Parallel::set_split_freq` が両プロセスで同じ規則を通す。 分割モードの切替
    /// (`Split` の variant) は構造変更なので `LoadSong` で運ぶ。
    SetParallelSplitFreq {
        project: ProjectKey,
        track: u32,
        parallel_id: u64,
        edge: crate::model::SplitEdge,
        hz: f32,
    },
    /// r.md #114 Parallel Selector: アクティブ chain (安定 `ParallelChain::id`) の値のみ更新。 RT は
    /// Song snapshot から live-read し、 クロスフェードで切り替える。 規則は
    /// `Parallel::set_active_chain` が両プロセスで同じ。
    SetParallelActiveChain { project: ProjectKey, track: u32, parallel_id: u64, chain_id: u64 },
    /// r.md #114 Parallel Selector: クロスフェード時間 (ms、 値域 `SELECTOR_FADE_RANGE`) の値のみ更新。
    SetParallelSelectorFade { project: ProjectKey, track: u32, parallel_id: u64, fade_ms: f32 },
    /// Record-arm 状態。 audio thread は track.armed を Song に反映するのみ。
    SetTrackArmed { project: ProjectKey, track: u32, armed: bool },
    /// BPM 軽量更新 (transport scrub 中に毎 frame 流れうる)。値のみ。
    SetSongBpm { project: ProjectKey, bpm: f32 },
    /// TimeSig 分子の軽量更新。 1..=32 で clamp 想定。
    SetSongTimeSigNumerator { project: ProjectKey, num: u8 },
    /// GUI が現在 recording 中の lane (track + target) を audio thread に
    /// 通知する。 該当 lane の curve eval を bypass する。 空 Vec = なし。
    SetRecordingLanes {
        project: ProjectKey,
        lanes: Vec<(u32, crate::model::AutomationTarget)>,
    },
    /// メトロノーム on/off。 session-only state (project ごと)。
    SetMetronomeEnabled { project: ProjectKey, enabled: bool },
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
        project: ProjectKey,
        track_id: u32,
        pitch: u8,
        velocity: u8,
    },
    /// 鍵盤プレビューの note-off。
    PreviewNoteOff { project: ProjectKey, track_id: u32, pitch: u8 },
    /// r.md #51: 録音セッションの開始。 engine はこれを受けて
    /// 1. `preroll_samples > 0` なら count-in に入る (preroll 中は dispatch /
    ///    clip render を skip して metronome のみ render)、
    /// 2. 曲末の auto-stop を抑止する (録音は曲の後ろへ継ぎ足せる)、
    /// 3. `recording_live` (= 録音要求 && 再生中 && count-in 完了) を publish する。
    ///
    /// 「count-in の開始」ではなく **録音そのもの**を運ぶ。engine が曲末 auto-stop を
    /// 抑止するにも、count-in 明けを GUI に知らせるにも「録音中か」が要るため。
    StartRecording { project: ProjectKey, preroll_samples: u64 },
    /// r.md #51: 録音セッションの終了 (パンチアウト / 停止 / count-in 取り消し)。
    /// engine は preroll を捨て、auto-stop の抑止と `recording_live` を解除する。
    /// transport は **止めない** — 停止は `Stop` の仕事 (パンチアウトは再生継続)。
    StopRecording { project: ProjectKey },
    /// Stand up the per-buffer plugin process worker pool. `n_workers`
    /// audio-engine workers pair 1:1 with plugin-host workers via the named
    /// events listed. イベント名は世代 (generation) 込みで daw_gui が mint
    /// する — pool 再構築時に stale な auto-reset signal を旧世代へ隔離する
    /// (`plugin_ref` の poisoning contract 参照)。pool はデバイス全体で 1 つ
    /// (全 project が共有)。
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
    /// ので positional 情報は運ばない。`token` は worker dispatch で plugin_host の
    /// instance を名指しする 1 ワード ([`InstanceToken`])。
    OpenPluginShmem {
        project: ProjectKey,
        device_id: u64,
        shmem_id: String,
        token: InstanceToken,
    },
    /// Drop the `ProcessData` mapping for `device_id` after the plugin
    /// instance is being torn down.
    ClosePluginShmem { project: ProjectKey, device_id: u64 },
    /// r.md #87 (クリップランチャー): セルを 1 つ撃つ / 離す。
    ///
    /// 宛先は安定 id — `track_id` = [`Track::id`](crate::model::Track)、
    /// `lane_id` = オートメーションレーン行なら [`AutomationLane::id`](crate::model::AutomationLane)、
    /// **トラック行なら `0`** (レーン行とトラック行を 1 本の「行」で表す唯一の区別)。
    /// `clip_id` は撃つセルの `clip.id` (列 = `scene_id` はセル側が持つので運ばない)。
    ///
    /// `pressed` で押下 / 離しを運ぶ。engine が
    /// [`LaunchMode`](crate::model::LaunchMode) 4 種 (Trigger / Gate / Toggle / Repeat) を
    /// 解釈するので、GUI は「押した」「離した」という事実だけを送る。
    LaunchCell {
        project: ProjectKey,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        pressed: bool,
        /// 量子化を待たず今すぐ撃つ (r.md #126: ▶ の Alt+click)。
        immediate: bool,
    },
    /// セルを **セル内の拍 `phase_beats` から** 鳴らす (ピアノロールの `f` =
    /// Live のクリップビューでの頭出し)。
    ///
    /// [`LaunchCell`](Self::LaunchCell) と違い **[`LaunchMode`](crate::model::LaunchMode)
    /// を見ない** — 「ここから鳴らせ」に押下 / 離しの対は無く、Toggle で鳴っている
    /// セルを止めたり Gate で握ったりしてはいけない。量子化はセルの設定に従う。
    /// `phase_beats` はセルの `start_beat` からの拍で、engine がループ長へ折り返す
    /// (ワンショットは末尾で切る)。
    LaunchCellFrom {
        project: ProjectKey,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        phase_beats: f64,
    },
    /// ランチャーが**セルを鳴らしている全行**を、それぞれのセル内の拍 `phase_beats`
    /// へ揃える (ピアノロールの `f` = 「全体をカーソルの拍から」)。
    ///
    /// 対象は供給元がセルの行だけ — 停止中 (Stop Clips) の行とアレンジ主導の行は
    /// 触らない (アレンジ側は同時に送る `SeekTo` が動かす)。予約が生きている行は
    /// その予約の発火位置に位相を載せる。折り返し / 量子化は
    /// [`LaunchCellFrom`](Self::LaunchCellFrom) と同じ。
    RephaseLauncherRows { project: ProjectKey, phase_beats: f64 },
    /// r.md #87: 列 ([`Scene::id`](crate::model::Scene)) をまとめて撃つ / 離す。
    /// その列にセルを持たない行は **停止**する (Q11、空セル = 停止)。
    /// `immediate` = 量子化を待たず今すぐ (r.md #126)。
    LaunchScene { project: ProjectKey, scene_id: u32, pressed: bool, immediate: bool },
    /// r.md #87: 1 行を止める (Stop Clips)。アレンジへは戻さない —
    /// ランチャーが主導権を握ったまま無音になる
    /// ([`RowPlayback::LauncherStopped`](crate::model::RowPlayback))。
    /// `immediate` = 量子化を待たず今すぐ止める (r.md #126: 停止ボタンの Alt+click)。
    StopRow { project: ProjectKey, track_id: u32, lane_id: u32, immediate: bool },
    /// r.md #87: 全行を止める (グローバル Stop Clips)。 `immediate` は [`Self::StopRow`] と同じ。
    StopAllRows { project: ProjectKey, immediate: bool },
    /// r.md #87: 1 行の主導権をアレンジへ返す
    /// ([`RowPlayback::Arranger`](crate::model::RowPlayback))。
    SwitchRowToArranger { project: ProjectKey, track_id: u32, lane_id: u32 },
    /// r.md #87: 全行の主導権をアレンジへ返す。
    SwitchAllToArranger { project: ProjectKey },
    /// Global Sampler (`docs/plan_global_sampler.md` §3.2): daw_gui が create した
    /// 音声リング (`sampler_ring::sampler_shmem_id`) を open し、次の buffer から
    /// `source` の音を書き続ける。既に開いているリングは置き換える (旧世代は
    /// off-thread で drop)。一時停止はリングのヘッダ flag (command 不要)。
    OpenSamplerRing { shmem_id: String, source: SamplerSource },
    /// Global Sampler の試聴: リングの `[start_frame, end_frame)` (`write_frames`
    /// 座標) を master 出力へ加算する。リングへの書き込みの **後**に足すので
    /// 試聴音は再録されない。範囲がリングから押し出されたら engine が自分で止める。
    SamplerPreview { start_frame: u64, end_frame: u64 },
    SamplerPreviewStop,
    /// MIDI Capture の試聴: `track_id` のインストへノート列を送る。`offset_frames`
    /// は受信時点からのフレーム数。engine は buffer 頭で「この buffer に入る
    /// ノート」を `pending_preview` に注入する (= buffer 精度)。
    PreviewSequence { project: ProjectKey, track_id: u32, notes: Vec<PreviewNote> },
    /// 走行中の `PreviewSequence` を止め、鳴っているノートを消音する。
    PreviewSequenceStop { project: ProjectKey },
    /// (r.md #61) **プロセスを正常終了しろ**。daw_gui の終了シーケンス
    /// (`daw_gui::shutdown`) だけが送る。
    ///
    /// 受け側は receive loop を抜け、CPAL stream を明示的に pause + drop
    /// (= WASAPI デバイスの解放) してから `main` を return する。
    /// 「終わった」の真実源は **プロセスの exit そのもの**で、返信 event は
    /// 用意しない — 「返事を書けた」と「デバイスを解放し終えた」は別の事実で、
    /// 親が欲しいのは後者だけだから (親は `Child::try_wait` で観測する)。
    ///
    /// 親が crash した場合の pipe EOF 経路も同じ teardown に合流するので、
    /// 「終わり方」の実装は 1 つしかない。
    Shutdown,
}

impl AudioCommand {
    /// この command が効く project (`None` = デバイス全体)。recv loop の配送と、
    /// 「閉じたタブへの遅延 command を捨てる」判定の SSoT。
    #[must_use]
    pub fn project(&self) -> Option<ProjectKey> {
        use AudioCommand::*;
        match self {
            OpenProject { project }
            | CloseProject { project }
            | SetScopeProject { project }
            | Play { project }
            | PlayContinue { project }
            | Stop { project }
            | LoadSong { project, .. }
            | SetLoop { project, .. }
            | SetMasterGain { project, .. }
            | SetDeviceLatency { project, .. }
            | ExportWav { project, .. }
            | AnalyzeLoudness { project, .. }
            | BounceClipFxOnline { project, .. }
            | SeekTo { project, .. }
            | SetProjectDir { project, .. }
            | SetTrackVolume { project, .. }
            | SetTrackPan { project, .. }
            | SetTrackMuted { project, .. }
            | SetTrackSolo { project, .. }
            | SetTrackStrip { project, .. }
            | SetMasterStrip { project, .. }
            | SetSendGain { project, .. }
            | SetSendEnabled { project, .. }
            | SetChainGain { project, .. }
            | SetChainPan { project, .. }
            | SetChainMuted { project, .. }
            | SetChainSolo { project, .. }
            | SetParallelOutGain { project, .. }
            | SetParallelGainMatch { project, .. }
            | SetParallelSplitFreq { project, .. }
            | SetParallelActiveChain { project, .. }
            | SetParallelSelectorFade { project, .. }
            | SetTrackArmed { project, .. }
            | SetSongBpm { project, .. }
            | SetSongTimeSigNumerator { project, .. }
            | SetRecordingLanes { project, .. }
            | SetMetronomeEnabled { project, .. }
            | PreviewNoteOn { project, .. }
            | PreviewNoteOff { project, .. }
            | StartRecording { project, .. }
            | StopRecording { project }
            | OpenPluginShmem { project, .. }
            | ClosePluginShmem { project, .. }
            | LaunchCell { project, .. }
            | LaunchCellFrom { project, .. }
            | RephaseLauncherRows { project, .. }
            | LaunchScene { project, .. }
            | StopRow { project, .. }
            | StopAllRows { project, .. }
            | SwitchRowToArranger { project, .. }
            | SwitchAllToArranger { project }
            | PreviewSequence { project, .. }
            | PreviewSequenceStop { project } => Some(*project),
            OpenSamplerRing { source, .. } => source.project(),
            Ack
            | Session(_)
            | Panic
            | PanicRelease
            | CancelExport
            | SetAppActive(_)
            | OpenWorkerPool { .. }
            | CloseWorkerPool
            | SamplerPreview { .. }
            | SamplerPreviewStop
            | Shutdown => None,
        }
    }
}

/// Global Sampler の録音源 (`docs/plan_global_sampler.md` Q3)。session-only
/// (永続化しない — `Track` は開いているタブの `ProjectKey` に縛られる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Encode, Decode)]
pub enum SamplerSource {
    /// master fx / master gain 適用後、metronome を重ねる前 (= `ScopeBridge` と同じ点)。
    /// 複数プロジェクト時は [`AudioCommand::SetScopeProject`] の project の master。
    #[default]
    Master,
    /// `project` の任意 track の pre-fx / post-fx / post-fader。
    Track { project: ProjectKey, tap: crate::model::AudioTap },
}

impl SamplerSource {
    /// この録音源が縛られている project (`Master` はアクティブなタブに追従するので `None`)。
    #[must_use]
    pub fn project(&self) -> Option<ProjectKey> {
        match self {
            SamplerSource::Master => None,
            SamplerSource::Track { project, .. } => Some(*project),
        }
    }
}

/// [`AudioCommand::PreviewSequence`] の 1 ノート。
#[derive(Debug, Clone, Copy, PartialEq, Encode, Decode)]
pub struct PreviewNote {
    pub offset_frames: u64,
    pub duration_frames: u64,
    pub pitch: u8,
    pub velocity: u8,
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
        project: ProjectKey,
        error: Option<String>,
        cancelled: bool,
    },
    /// Offline WAV render progress (throttled by sender).
    ExportWavProgress { project: ProjectKey, done: u64, total: u64 },
    /// (r.md #54) 範囲ラウドネス解析の途中経過 (送信側でスロットル済み)。
    /// 進捗バーだけでなく、その時点までの数値と曲線も入っているので、
    /// レポート窓は走査に合わせて左から伸びていくグラフを描ける。
    LoudnessAnalysisProgress {
        project: ProjectKey,
        report: Box<crate::loudness_report::LoudnessReport>,
    },
    /// (r.md #54) 範囲ラウドネス解析の確定。
    ///
    /// `report` は走査が始まっていれば `Some` (中止でもそこまでの値が入る。
    /// 範囲全体の Integrated ではないので、受け側は `cancelled` を見て捨てる)。
    /// 走査に入る前に失敗した場合だけ `None` + `error`。
    LoudnessAnalysisComplete {
        project: ProjectKey,
        report: Option<Box<crate::loudness_report::LoudnessReport>>,
        error: Option<String>,
        cancelled: bool,
    },
    /// Offline plugin-FX bounce finished (or failed).
    BounceClipFxComplete {
        project: ProjectKey,
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
    PluginUnresponsive { device: DeviceAddr },
    /// (v29) worker pool 全体の完了待ちが timeout した = plugin_host が
    /// 応答不能 (ハード crash / ハング)。 GUI は plugin_host を respawn する。
    WorkerPoolStalled,
}

impl AudioEvent {
    /// この event が属する project (`None` = デバイス全体 / handshake)。daw_gui の
    /// 配送 (`tabs.get_mut(key)`) の SSoT。
    #[must_use]
    pub fn project(&self) -> Option<ProjectKey> {
        use AudioEvent::*;
        match self {
            ExportWavComplete { project, .. }
            | ExportWavProgress { project, .. }
            | LoudnessAnalysisProgress { project, .. }
            | LoudnessAnalysisComplete { project, .. }
            | BounceClipFxComplete { project, .. } => Some(*project),
            PluginUnresponsive { device } => Some(device.project),
            Hello { .. } | ChildDisconnected | WorkerPoolStalled => None,
        }
    }
}

// =====================================================================
// gui → daw_plugin_host
// =====================================================================

/// ホストアプリの窓を指す **platform 固有ハンドル**を、platform 非依存に運ぶ値。
///
/// Windows は `HWND`、macOS は `NSWindow*`、X11 は `Window` (XID) が入る想定。
/// **wire はこれを「不透明な機械語ワード 1 つ」としてしか扱わない** — どう解釈するかは
/// 受け手の platform コードだけが知っている。だから型名にも doc にも platform 固有の
/// 型名を持ち込まない (`HWND` 専用の型にすると macOS 対応で作り直しになる)。
///
/// r.md #65: エディタコンテナ窓の **owner** を「窓の作成時に」決めるために渡す。
/// 「HWND は IPC を渡らない」という旧不変条件は 2026-08-22 に撤回された
/// (撤回理由は CLAUDE.md「プラグインエディタ窓と Win32」節。要約すると、旧不変条件は
///  「daw_gui を owner にしてはいけない」から導出されていたが、その禁止自体が
///  JUCE のソース読み違いに基づく誤りだった)。
///
/// **0 は「窓が無い」を表す**ので、生値からは [`Self::from_raw`] で `Option` に落とす。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct PlatformWindowHandle(u64);

impl PlatformWindowHandle {
    /// platform の生ハンドルから。**0 (= 窓が無い) は `None`** になる。
    pub fn from_raw(raw: u64) -> Option<Self> {
        (raw != 0).then_some(Self(raw))
    }

    /// platform コードが自分の型へ戻すための生値。
    pub fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub enum PluginCommand {
    /// Handshake reply to `PluginEvent::Hello`.
    Ack,
    Session(AudioSession),
    /// Project directory (ARA WAV 解決等)。project ごと。daw_gui の frame 末 sync
    /// (`flush_song_sync`) が `LoadSong` の直前に送る。
    SetProjectDir { project: ProjectKey, dir: Option<std::path::PathBuf> },
    /// Reinitialise (deactivate→activate) loaded plugins to a clean state.
    /// `project = None` → 全 project (パニックボタン)、`Some` → その project だけ
    /// (書き出し前のクリーンスタート。他のタブの再生を乱さない)。
    /// 完了で `PluginEvent::PluginsReinitDone { project }` を返す。
    ReinitAllPlugins { project: Option<ProjectKey> },
    /// 全 loaded plugin の CLAP render mode を切り替える (export bookend)。
    /// オフライン描画はエンジン全体で 1 本なので project を持たない。
    SetRenderMode(RenderMode),
    /// Builtin plugin (`PluginFormat::Builtin`) に per-note metadata を
    /// flush する (`docs/plan_voicevox_synth.md` PR-V2.2)。
    SetBuiltinPluginNoteMetadata {
        device: DeviceAddr,
        bpm: f32,
        /// 塊 (= `/sing_frame_audio_query` 1 回) の長さ (秒)。アプリ設定
        /// (`app_config.json` の `voicevox_chunk_secs`) が SSoT で、
        /// `voicevox_phrase::{MIN,MAX}_CHUNK_SECS` にクランプ済みの値が来る。
        /// **合成結果を変える入力**なので、daw_gui 側の再送デデュープの比較対象に
        /// 含め、かつ **フレーズ WAV のキャッシュキーにも混ぜる**
        /// (`voicevox_cache::key_for_sing_phrase`)。両方に入れて初めて
        /// 「設定を変えたら音が変わる」が成立する (キーに入れないと、再送しても
        /// 全フレーズが cache hit して何も変わらない)。
        chunk_secs: f32,
        entries: Vec<crate::plugin_metadata::NoteMetadata>,
        /// (talk) 同トラックの `ClipContent::Text` 由来の読み上げ群。
        talk: Vec<crate::plugin_metadata::TalkMetadata>,
    },
    /// 歌唱 bounce / 曲全体の WAV 書き出しの前に builtin VOICEVOX の合成完了を
    /// 要求する。 完了で `PluginEvent::VocalSynthReady` が返る。
    PrepareVocalSynth { device: DeviceAddr },
    /// builtin VOICEVOX の合成順序ヒント。ここに近いフレーズから合成させる
    /// (本家 `selectPriorPhrase`: その位置を含む → 後ろ → 前)。
    ///
    /// **`SetBuiltinPluginNoteMetadata` に相乗りさせてはいけない** — あちらは
    /// daw_gui 側で `(bpm, chunk_secs, entries, talk)` の一致による再送デデュープが
    /// 掛かっており、playhead を比較に入れれば**トランスポートを動かすたびに再合成**、
    /// 入れなければ **playhead が永久に stale** になる。どちらも壊れているので、
    /// 合成をトリガしない専用の軽量メッセージにする。
    ///
    /// r.md #87: `playhead_beats` は **合成タイムラインの拍** —
    /// [`crate::plugin_metadata::NoteMetadata::start_beat`] と同じ座標系で、
    /// song の再生位置とは限らない。ランチャーのセルはアレンジの終端より後ろの
    /// 仮想区間 (`cell_base_beat`) に置かれるので、song の playhead を送ると
    /// **セルは常に最遠 = 最後**になり、撃っても当分歌わない。送り手
    /// (`daw_gui::AppData::vocal_synth_priority_beat`) が「その行がいま何を
    /// 鳴らす行か」で座標を選ぶ。 (名前は song の playhead を送っていた頃の名残)
    SetVocalSynthPriority { device: DeviceAddr, playhead_beats: f64 },
    /// Load / replace the plugin instance for `device` (安定 id、
    /// `Song.next_device_id` 採番 + project)。 `generation` は per-device 単調増加の
    /// 要求世代 — 応答 (`SlotPluginLoaded` / `SlotPluginLoadFailed`) に echo され、
    /// GUI は最新世代のみ受理する (A→B 連続差し替えの stale 応答 race 対策)。
    ///
    /// r.md #71 (プラグインのコピー / 移動): 所属 track は載せない。 host は
    /// device の帰属も順序も持たず、アドレスは `DeviceAddr` 一本 —
    /// 帰属を二重所有すると device を別トラックへ移した瞬間に host 側が stale に
    /// なり、「元トラックを削除したら移動先の device が破棄される」が起きる。
    SetSlotPlugin {
        device: DeviceAddr,
        format: PluginFormat,
        path: std::path::PathBuf,
        plugin_id: String,
        initial_state: Option<Vec<u8>>,
        generation: u64,
    },
    /// Remove the plugin instance for `device` if any.
    RemoveSlotPlugin { device: DeviceAddr },
    /// **タブを閉じたので、その project の instance を全部捨てる。**
    ///
    /// 列挙元は daw_gui の帳簿ではなく plugin_host 自身の `instances`
    /// (`DeviceAddr.project == project`) — open 応答が返る前の device も漏らさない。
    /// 閉じる 1 台ごとに通常の teardown と同じ event 列
    /// (`SlotPluginShmemReleased` → `SlotPluginUnloaded`) が返る。project の
    /// `SetProjectDir` も忘れる。
    ///
    /// 旧 `UnloadAllPlugins` (「別プロジェクトに切り替わった」) はこれに置き換わった:
    /// 複数タブでは「全部捨てる」場面が無く (respawn は子プロセス自体が新しい)、
    /// device_id の衝突は `DeviceAddr` が構造的に解いている。
    UnloadProject { project: ProjectKey },
    /// Ask the plugin_host to capture state for one device. Reply is
    /// `PluginEvent::SlotPluginState`.
    RequestSlotState { device: DeviceAddr },
    /// Ask the plugin_host to capture state for every device of `project` at once.
    /// Reply is `PluginEvent::AllPluginStates`. Used for project save.
    RequestAllStates { project: ProjectKey },
    /// Open the plugin editor (top-level window は plugin-host プロセス所有)。
    /// `title` is the window caption daw_gui composed (プロジェクト名込み —
    /// 背景タブの窓も開いたまま残るので、どのタブの窓かはタイトルで区別する)。
    ///
    /// r.md #65: `geometry` は前回このプロジェクトで閉じたときの窓の位置 /
    /// client サイズ (`ViewState.plugin_editor_windows` 由来、`None` = 初回)。
    /// 位置は常に復元し、**サイズはプラグインが `canResize` / `can_resize` で
    /// リサイズ可と答えたときだけ**復元する (固定サイズ GUI に前回のサイズを
    /// 押し付けない)。CLAP の GUI 手順 9 「resizable かつ前回セッションのサイズが
    /// 分かっているときだけ `set_size`」と同じ規約。
    ///
    /// r.md #65: `owner_main_window` は **daw_gui の本体窓** (preview 窓ではない)。
    /// plugin_host はこれをエディタコンテナ窓の **owner** にする (REAPER の FX 窓と
    /// 同じ構成)。これで
    /// *"An owned window is always above its owner in the z-order"* が効き、
    /// エディタ窓が本体窓の後ろに回らなくなる。`None` = 本体窓がまだ無い / 既に閉じた
    /// (その場合 plugin_host は owner 無しで作り、`WS_EX_TOOLWINDOW` も付けない)。
    OpenSlotGuiEmbedded {
        device: DeviceAddr,
        title: String,
        geometry: Option<crate::model::EditorWindowGeometry>,
        owner_main_window: Option<PlatformWindowHandle>,
    },
    CloseSlotGui { device: DeviceAddr },
    /// r.md #55: **開いているエディタ窓を全部閉じる** (`Ctrl+Shift+W`)。全 project。
    ///
    /// 列挙元は daw_gui ではなく **窓を所有する plugin_host** (`instances` の
    /// `editor.is_some()`) — daw_gui 側の `open_plugin_guis` を列挙元にすると、
    /// open 応答が返る前の device が帳簿に載っておらず永久に閉じ残る。
    ///
    /// 閉じた 1 枚ごとに `SlotGuiClosed` が返るので、daw_gui 側の帳簿は
    /// 個別に ✕ を押したときとまったく同じ経路で整合する。
    CloseAllSlotGuis,
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
    SetEditorSendAllKeys { device: DeviceAddr, enabled: bool },
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
        device: DeviceAddr,
        clips: Vec<AraClipSpec>,
        /// Project tempo (bpm) and time signature. Fed into the ARA musical
        /// context so the plug-in's editor grid aligns to the song.
        bpm: f64,
        time_sig: (u16, u16),
        /// Prior ARA edit archive to restore after (re)building the document
        /// (from `PluginInstance.ara_archive`). `None` for a fresh document.
        archive: Option<Vec<u8>>,
    },
    /// (r.md #5 ARA2) Tear down the ARA document/session for `device`.
    ClearAraDocument { device: DeviceAddr },
    /// (r.md #7 ARA2) Update only the playback-region placements of an
    /// existing ARA document (matched by `persistent_id`).
    UpdateAraRegions {
        device: DeviceAddr,
        regions: Vec<AraRegionUpdate>,
    },
    /// (r.md #61) **プロセスを正常終了しろ**。daw_gui の終了シーケンス
    /// (`daw_gui::shutdown`) だけが送る。
    ///
    /// これは「全 plugin の unload + worker pool 停止 + プロセス終了」の
    /// **合成**であり、unload の実装は [`PluginCommand::UnloadProject`] と
    /// **同じ 1 本** (`teardown_device` → `teardown_plugin`) を通る。列挙元も同じく
    /// plugin_host 自身の `instances` なので、daw_gui の帳簿には一切依存しない。
    /// 「全部畳め」の実装が 2 つに割れないよう、
    /// このコマンドは *追加で何をするか* (pool 停止 + exit) だけを足す。
    ///
    /// 完了の真実源は **プロセスの exit そのもの**で、返信 event は用意しない
    /// — 「返事を書けた」と「DLL を unload し終えた」は別の事実で、親が欲しい
    /// 保証は後者だけだから (親は `Child::try_wait` で観測する)。
    ///
    /// 親が crash した場合の pipe EOF 経路も同じ teardown に合流する。
    Shutdown,
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
    EditorKey { device: DeviceAddr, chord: KeyChord },
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
    /// Reply to `PluginCommand::ReinitAllPlugins` (`project` は要求の echo)。
    PluginsReinitDone { project: Option<ProjectKey> },
    /// builtin VOICEVOX の歌唱合成が `PrepareVocalSynth` で要求した世代まで
    /// 完了した (or タイムアウトした)。
    VocalSynthReady { device: DeviceAddr },
    /// Plugin-host confirmed `SetSlotPlugin` and reported the descriptor
    /// that actually loaded. `shmem_id` names the `ProcessData` shmem so
    /// daw_audio can `OpenShared` it; `token` は worker dispatch / metrics slot で
    /// この instance を名指しする 1 ワード (daw_gui が `OpenPluginShmem` へ写す)。
    ///
    /// `state_load_error` は saved state の復元失敗理由 (`None` = 新規 or
    /// 成功)。 `generation` は要求の echo (最新世代のみ受理)。
    SlotPluginLoaded {
        device: DeviceAddr,
        id: String,
        name: String,
        shmem_id: String,
        token: InstanceToken,
        state_load_error: Option<String>,
        /// パラアウト: how many `is_main=false` audio output ports this
        /// plugin declared (0 for the common single-out case)。
        aux_output_count: u8,
        /// r.md #110: how many `is_main=false` audio **input** ports (= sidechain
        /// 候補) this plugin declared。 inspector の SC 制御の表示 gate。
        aux_input_count: u8,
        generation: u64,
    },
    /// `SetSlotPlugin` の load が失敗した。 song の slot は touch されない。
    SlotPluginLoadFailed {
        device: DeviceAddr,
        plugin_id: String,
        reason: String,
        generation: u64,
    },
    /// Reply to `RequestSlotState`. `None` = plugin unavailable or state
    /// extension missing.
    SlotPluginState {
        device: DeviceAddr,
        data: Option<Vec<u8>>,
    },
    /// Reply to `RequestAllStates { project }`: one entry per loaded device of that project.
    AllPluginStates { project: ProjectKey, entries: Vec<SlotState> },
    /// r.md #65: エディタ窓のジオメトリが確定した。open 直後と、以後
    /// **ユーザーのドラッグが終わった / プラグイン起点のリサイズが済んだ**
    /// たびに送る (ドラッグ中は送らない — `WM_EXITSIZEMOVE` で 1 回)。
    ///
    /// 窓を所有するのは plugin_host なので、位置 / サイズの一次情報はここにしか
    /// 無い。daw_gui はこれを `plugin_editor_windows` に貯め、保存時に
    /// `ViewState` へ書き出して次回 open で復元する。
    ///
    /// 旧 `SlotGuiOpened { width, height }` はこれに置き換えた: 受け手
    /// (`on_gui_opened`) が空実装で「開いた」以上の意味を運んでおらず、
    /// 同じ内容を 2 つの message で表す方が SSoT を割る。
    SlotGuiGeometry {
        device: DeviceAddr,
        geometry: crate::model::EditorWindowGeometry,
    },
    /// Plugin-initiated close (X button handled by plugin, or `closed`).
    SlotGuiClosed { device: DeviceAddr },
    /// Plugin host がこの device の `ProcessData` shmem を破棄した。
    /// **teardown のたびに必ず先行して発火する** (replace = 同 device に別
    /// plugin を載せ直す経路も含む)。 daw_gui はこれを受けて
    /// `AudioCommand::ClosePluginShmem` を daw_audio に転送し、
    /// audio engine の stale mapping を落とす。
    ///
    /// 「shmem が死んだ」 は 「device が空になった」 ([`Self::SlotPluginUnloaded`])
    /// とは**別の事実**なので別 event にしてある。 replace 経路で前者だけを
    /// 送らないと、 daw_audio が **旧** mapping へ入力を書き、 plugin_host の
    /// worker は registry の **新** mapping を読む窓が開く (数バッファ分の
    /// 無音 / 取りこぼし)。 close が先行していれば daw_audio 側 entry が
    /// 一時的に消え、 RT はその device を skip する (= ロード完了までの
    /// dispatch 抑止)。
    SlotPluginShmemReleased { device: DeviceAddr },
    /// Plugin host destroyed a plugin instance (`RemoveSlotPlugin` /
    /// `UnloadProject` 経由)。 daw_gui はこれを受けて daw_gui ローカルの bookkeeping
    /// (`loaded_devices` / latency 等) を片付ける。 shmem の解放は必ず先行する
    /// [`Self::SlotPluginShmemReleased`] が担う (SSoT — 二重に送らない)。
    SlotPluginUnloaded { device: DeviceAddr },
    /// Plugin が報告した自身の processing latency (samples 単位)。 activate
    /// 直後、および restart / reinit 完了直後に発火。 daw_gui は
    /// これを [`AudioCommand::SetDeviceLatency`] としてそのまま engine へ中継し、
    /// engine が PDC を再計算する (集計も保存もしない — r.md #9)。
    PluginLatencyChanged { device: DeviceAddr, samples: u32 },
    /// Plugin の parameter 一覧。 activate 完了直後に 1 度、 rescan 要求で
    /// 再送。
    PluginParamList {
        device: DeviceAddr,
        params: Vec<PluginParamInfo>,
        /// この plugin が埋め込み GUI を持つか。
        has_embedded_gui: bool,
    },
    /// Plugin GUI で knob を **touch** した通知。
    PluginParamTouched {
        device: DeviceAddr,
        param_id: u32,
        display_name: String,
    },
    /// Plugin GUI 内で parameter 値が変更された通知。
    PluginParamValueChanged {
        device: DeviceAddr,
        param_id: u32,
        value: f64,
    },
    /// Plugin GUI で knob を release した通知 (gesture end)。
    PluginParamGestureEnd { device: DeviceAddr, param_id: u32 },
    /// builtin VOICEVOX plugin の合成スレッドの状態遷移 + 進捗。
    VoicevoxSynthStatus {
        device: DeviceAddr,
        progress: VocalSynthProgress,
    },
}

impl PluginEvent {
    /// この event が属する project (`None` = プロセス全体 / handshake)。daw_gui の
    /// 配送 (`tabs.get_mut(key)`) の SSoT。
    #[must_use]
    pub fn project(&self) -> Option<ProjectKey> {
        use PluginEvent::*;
        match self {
            EditorKey { device, .. }
            | VocalSynthReady { device }
            | SlotPluginLoaded { device, .. }
            | SlotPluginLoadFailed { device, .. }
            | SlotPluginState { device, .. }
            | SlotGuiGeometry { device, .. }
            | SlotGuiClosed { device }
            | SlotPluginShmemReleased { device }
            | SlotPluginUnloaded { device }
            | PluginLatencyChanged { device, .. }
            | PluginParamList { device, .. }
            | PluginParamTouched { device, .. }
            | PluginParamValueChanged { device, .. }
            | PluginParamGestureEnd { device, .. }
            | VoicevoxSynthStatus { device, .. } => Some(device.project),
            AllPluginStates { project, .. } => Some(*project),
            PluginsReinitDone { project } => *project,
            Hello { .. } | ChildDisconnected | HostWindowsActive(_) => None,
        }
    }
}

/// builtin VOICEVOX 合成の進捗。`(busy, failure)` だけだった報告を、フレーズ単位の
/// 残件数とクリップ帰属まで含む 1 つの形に統一する (callback の引数と IPC の payload が
/// 同じ型 = SSoT)。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, Default)]
pub struct VocalSynthProgress {
    /// 合成中か。
    pub busy: bool,
    /// 直近試行の失敗種別 (engine 到達可否で区別)。
    pub failure: VocalSynthFailure,
    /// 未完了フレーズ数 (talk の 1 発話も 1 件として数える)。
    pub pending: u32,
    /// この job の総フレーズ数。`pending / total` は **percent にしない**
    /// (HTTP は中間進捗を返さない = 偽の % を出さない、という既存判断を維持)。
    pub total: u32,
    /// 未完了フレーズ / 未完了 talk 発話が掛かっている clip id (昇順・重複なし)。
    /// クリップ上スピナーを「そのクリップに未完了の仕事があるときだけ」点けるために使う。
    /// 歌唱は `voicevox_phrase::Phrase::clip_ids`、talk は
    /// `plugin_metadata::TalkMetadata::clip_id` から集める — talk を入れ忘れると
    /// Text クリップのスピナーが消える。
    pub pending_clips: Vec<u32>,
}

/// builtin VOICEVOX 合成の失敗種別。engine に**到達できない** (未起動 / 起動途中 /
/// timeout = transient、retry する) のか、engine は**到達できたが入力を拒否**した
/// (HTTP 4xx/5xx = 例: 不正な歌詞。同 job を retry しても無駄なので retry しない) のかを
/// 区別する。後者を「engine 未接続」と誤表示しない / 無限 retry しないための SSoT。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, Default)]
pub enum VocalSynthFailure {
    /// 直近試行は失敗していない (成功 or まだ試行なし)。
    #[default]
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

    const P1: ProjectKey = ProjectKey(1);
    const P2: ProjectKey = ProjectKey(2);

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
        assert_eq!(roundtrip(&AudioCommand::Play { project: P1 }), AudioCommand::Play { project: P1 });
        assert_eq!(roundtrip(&AudioCommand::Stop { project: P2 }), AudioCommand::Stop { project: P2 });
    }

    #[test]
    fn load_song_roundtrip() {
        let msg = AudioCommand::LoadSong { project: P1, song: crate::model::Song::default() };
        assert_eq!(roundtrip(&msg), msg);
    }

    /// `docs/plan_project_tabs.md` §2: project を持つ / 持たない command の仕分けは
    /// `AudioCommand::project` が SSoT。デバイス全体の command だけが `None`。
    #[test]
    fn audio_command_project_addressing() {
        assert_eq!(AudioCommand::OpenProject { project: P2 }.project(), Some(P2));
        assert_eq!(AudioCommand::CloseProject { project: P2 }.project(), Some(P2));
        assert_eq!(AudioCommand::SeekTo { project: P1, samples: 3 }.project(), Some(P1));
        assert_eq!(
            AudioCommand::OpenSamplerRing {
                shmem_id: "x".into(),
                source: SamplerSource::Track {
                    project: P2,
                    tap: crate::model::AudioTap::post_fader(1),
                },
            }
            .project(),
            Some(P2)
        );
        assert_eq!(
            AudioCommand::OpenSamplerRing { shmem_id: "x".into(), source: SamplerSource::Master }
                .project(),
            None
        );
        for global in [
            AudioCommand::Ack,
            AudioCommand::Panic,
            AudioCommand::PanicRelease,
            AudioCommand::CancelExport,
            AudioCommand::SetAppActive(true),
            AudioCommand::CloseWorkerPool,
            AudioCommand::SamplerPreviewStop,
            AudioCommand::Shutdown,
        ] {
            assert_eq!(global.project(), None, "{global:?}");
        }
    }

    #[test]
    fn audio_event_project_addressing() {
        assert_eq!(
            AudioEvent::ExportWavProgress { project: P1, done: 1, total: 2 }.project(),
            Some(P1)
        );
        assert_eq!(
            AudioEvent::PluginUnresponsive { device: DeviceAddr::new(P2, 7) }.project(),
            Some(P2)
        );
        assert_eq!(AudioEvent::WorkerPoolStalled.project(), None);
        assert_eq!(AudioEvent::ChildDisconnected.project(), None);
    }

    #[test]
    fn plugin_event_project_addressing() {
        assert_eq!(
            PluginEvent::SlotGuiClosed { device: DeviceAddr::new(P2, 3) }.project(),
            Some(P2)
        );
        assert_eq!(
            PluginEvent::AllPluginStates { project: P1, entries: vec![] }.project(),
            Some(P1)
        );
        assert_eq!(PluginEvent::PluginsReinitDone { project: None }.project(), None);
        assert_eq!(PluginEvent::PluginsReinitDone { project: Some(P1) }.project(), Some(P1));
        assert_eq!(PluginEvent::HostWindowsActive(true).project(), None);
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
            devices: vec![dev.into()],
            ..Track::default()
        });

        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(AudioCommand::LoadSong { project: P1, song }, cfg).unwrap();
        assert!(
            bytes.len() < 64 * 1024,
            "LoadSong with 8MB of blobs must stay small on the wire, got {} bytes",
            bytes.len()
        );
        let (decoded, _): (AudioCommand, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        let AudioCommand::LoadSong { project, song: s } = decoded else {
            panic!("expected LoadSong");
        };
        assert_eq!(project, P1);
        let d = s.tracks[0].devices[0].as_plugin().expect("plugin device");
        assert_eq!(d.id, 1);
        assert!(d.state.is_none());
        assert!(d.ara_archive.is_none());
    }

    #[test]
    fn export_wav_complete_roundtrip() {
        let ok = AudioEvent::ExportWavComplete { project: P1, error: None, cancelled: false };
        assert_eq!(roundtrip(&ok), ok);
        let cancelled = AudioEvent::ExportWavComplete { project: P1, error: None, cancelled: true };
        assert_eq!(roundtrip(&cancelled), cancelled);
        let failed = AudioEvent::ExportWavComplete {
            project: P2,
            error: Some("render failed".to_string()),
            cancelled: false,
        };
        assert_eq!(roundtrip(&failed), failed);
    }

    #[test]
    fn plugin_unresponsive_roundtrip() {
        let msg = AudioEvent::PluginUnresponsive { device: DeviceAddr::new(P1, 77) };
        assert_eq!(roundtrip(&msg), msg);
        assert_eq!(roundtrip(&AudioEvent::WorkerPoolStalled), AudioEvent::WorkerPoolStalled);
    }

    #[test]
    fn set_slot_plugin_roundtrip() {
        let msg = PluginCommand::SetSlotPlugin {
            device: DeviceAddr::new(P2, 42),
            format: PluginFormat::Clap,
            path: std::path::PathBuf::from("C:/plugins/test.clap"),
            plugin_id: "test.synth".into(),
            initial_state: Some(vec![1, 2, 3]),
            generation: 9,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    /// r.md #65: エディタ窓のジオメトリは wire を渡って往復する
    /// (daw_gui → plugin_host が復元値、plugin_host → daw_gui が観測値)。
    /// 位置は **マルチモニタで負値になり得る**ので、そこも往復させる。
    #[test]
    fn editor_window_geometry_roundtrips_both_directions() {
        let geometry = crate::model::EditorWindowGeometry {
            x: -1920,
            y: -8,
            width: 1105,
            height: 687,
        };
        let device = DeviceAddr::new(P1, 42);
        let open = PluginCommand::OpenSlotGuiEmbedded {
            device,
            title: "Plugin — Renoise Redux".to_string(),
            geometry: Some(geometry),
            // r.md #65: owner は上位ビットが立った値も通る (64bit の HWND)。
            owner_main_window: PlatformWindowHandle::from_raw(0x0000_7ff8_1234_5678),
        };
        assert_eq!(roundtrip(&open), open);
        // 初回 open (保存値なし) かつ本体窓もまだ無い。
        let fresh = PluginCommand::OpenSlotGuiEmbedded {
            device,
            title: "Plugin — Test".to_string(),
            geometry: None,
            owner_main_window: None,
        };
        assert_eq!(roundtrip(&fresh), fresh);
        let report = PluginEvent::SlotGuiGeometry { device, geometry };
        assert_eq!(roundtrip(&report), report);
    }

    /// r.md #65: **0 は「窓が無い」**。生値からの変換で `None` に落ちること。
    /// これを取り違えると owner に `HWND(0)` を渡してしまい、owner 無しなのに
    /// `WS_EX_TOOLWINDOW` だけ付く唯一の悪化パターンに落ちる。
    #[test]
    fn platform_window_handle_treats_zero_as_absent() {
        assert_eq!(PlatformWindowHandle::from_raw(0), None);
        assert_eq!(PlatformWindowHandle::from_raw(1).map(|h| h.raw()), Some(1));
        assert_eq!(
            PlatformWindowHandle::from_raw(u64::MAX).map(|h| h.raw()),
            Some(u64::MAX)
        );
    }

    #[test]
    fn all_plugin_states_roundtrip() {
        let msg = PluginEvent::AllPluginStates {
            project: P2,
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
            device: DeviceAddr::new(P1, 3),
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
            device: DeviceAddr::new(P1, 9),
            bpm: 128.0,
            chunk_secs: 60.0,
            entries: vec![crate::plugin_metadata::NoteMetadata {
                note_id: 0,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 60,
                velocity: 100,
                lyric: "あ".to_string(),
                clip_id: 5,
                speaker_id: 3061,
                cell_base_beat: 0.0,
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
                clip_id: 7,
                cell_base_beat: 0.0,
            }],
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn vocal_synth_progress_roundtrip() {
        let msg = PluginEvent::VoicevoxSynthStatus {
            device: DeviceAddr::new(P1, 4),
            progress: VocalSynthProgress {
                busy: true,
                failure: VocalSynthFailure::Rejected {
                    detail: "lyricが不正です: ー".to_string(),
                },
                pending: 3,
                total: 11,
                pending_clips: vec![2, 5, 9],
            },
        };
        assert_eq!(roundtrip(&msg), msg);
        // Default は「idle・失敗なし・件数 0」。stop 時の報告に使う。
        assert_eq!(VocalSynthProgress::default().failure, VocalSynthFailure::None);
        assert!(!VocalSynthProgress::default().busy);
    }

    #[test]
    fn vocal_synth_priority_roundtrip() {
        let msg = PluginCommand::SetVocalSynthPriority {
            device: DeviceAddr::new(P2, 12),
            playhead_beats: 37.25,
        };
        assert_eq!(roundtrip(&msg), msg);
    }

    #[test]
    fn slot_plugin_loaded_carries_token() {
        let msg = PluginEvent::SlotPluginLoaded {
            device: DeviceAddr::new(P2, 5),
            id: "x".into(),
            name: "X".into(),
            shmem_id: "daw_01_process_data_1_5_17".into(),
            token: InstanceToken(17),
            state_load_error: None,
            aux_output_count: 0,
            aux_input_count: 1,
            generation: 3,
        };
        assert_eq!(roundtrip(&msg), msg);
    }
}
