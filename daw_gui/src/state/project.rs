//! `docs/plan_project_tabs.md` §5.1: **プロジェクト (= タブ) ごと**の状態。
//!
//! `AppData` は「いま見えているタブ」を [`ProjectState`] として `cur` に持ち、他のタブは
//! [`crate::state::Tabs`] に parked する。ここにあるのは **Song 由来の名前 (track / device /
//! clip id) で引くもの・タブごとに独立であるべきもの**だけ。アプリ全体で 1 つのもの
//! (IPC の tx / plugin DB / テーマ / app_config / 最近使ったファイル / モーダル) は
//! `AppData` 直下の group (`IpcState` / `UiPrefs` / `UiEphemeral` ...) に残す。
//!
//! 旧 `IpcState` / `VoicevoxState` / `UiPrefs` / `UiEphemeral` から機械的に切り出したので、
//! field の doc はそのまま。

use std::path::PathBuf;

use common::model::Song;
use common::protocol::ProjectKey;

use crate::app::{
    ARRANGE_PX_PER_BEAT, ARRANGE_TRACK_HEIGHT, ArrLabelCache, ArrangeViewSnapshot,
    ArrangeZoomAnchor, AutomationPointKeyRef, ClipKey, ColorPickerTarget, DEFAULT_NOTE_DURATION,
    LoadedDeviceInfo, PendingClipFxBounce, PendingStateRequest, PendingVocalSynthBounce,
    SendPickerState, TempoMapCache,
    TouchedParam, VocalSynthStatus, track_with,
};
use crate::audio_source_cache::AudioSourceCache;
use crate::handler::glue::PendingGlueBake;
use crate::state::{
    DeviceParamKey, LauncherUiState, LoudnessState, MediaState, ModRackHover, RecordingState,
    ScrubGesture, SelectionState, SongDoc, TransportState,
};

/// 旧 `IpcState` の per-project 部分: plugin_host / daw_audio との帳簿 (device_id は
/// **この project 内の名前**。wire へは `ProjectKey` と組で出る)。
pub struct ProjectIpc {
    /// (r.md #5 ARA2) Last ARA clip-spec set sent to the plugin host per
    /// device (v29: 安定 `device_id` keyed)。 `SetupAraDocument` is sent only
    /// when an ARA device's track audio clips actually change — rebuilding the
    /// ARA document deactivates/reactivates the plug-in, so it must not happen
    /// on every song sync. Devices that disappear (removed / no longer ARA)
    /// get `ClearAraDocument`.
    pub(crate) ara_doc_cache: std::collections::HashMap<u64, Vec<common::protocol::AraClipSpec>>,
    /// (v29 §2) 旧 `AraSourceSpec::Pcm` の置換: in-memory (`Generated`) audio
    /// source を ARA に見せるために app cache へ書き出した WAV の path。
    /// key = `AudioSourceId`。 Generated source は immutable (bounce は毎回
    /// 新 source id) なので session 内 1 回の materialize で足りる。
    pub(crate) ara_pcm_materialized:
        std::collections::HashMap<common::model::AudioSourceId, PathBuf>,
    /// Phase 4 Step C-3 (`docs/plan_automation.md` §6): plugin GUI で knob 値が
    /// 変更されるたびに `PluginParamValueChangedFromChild` で受け取る最新値の
    /// cache。 `(device_id, param_id) -> plain value`。 audio bridge tick
    /// で `current_plain_value(PluginParam)` がここから plain 値を引いて
    /// `AutomationPoint` を生成する。 session-only / Undo 対象外。 device が
    /// 消える経路 (unload / 削除 / project 切替) で当該 entry を落とす。
    pub plugin_param_values: std::collections::HashMap<DeviceParamKey, f64>,
    /// Phase 2 (`docs/plan_automation.md` §7.5): plugin parameter
    /// 一覧キャッシュ。 plugin host が `PluginParamList` IPC で送って
    /// くるたびに上書き。 安定 `device_id` で identify、 Parameter
    /// Picker (Phase 3+) / lane の label 解決 / norm↔plain 変換に
    /// 使う。 session-only (save 対象外、 plugin reload で再取得)。
    pub plugin_params: std::collections::HashMap<u64, Vec<common::protocol::PluginParamInfo>>,
    /// device ごとに plugin が埋め込み GUI (editor window)
    /// を持つか (`PluginParamList` で host が `gui_is_embed_supported` を通知)。
    /// チェーン行のボタン分岐に使う: GUI あり = 「GUI」 で window を開く、 なし =
    /// 「⚙」 でインライン param パネルをトグル。 plugin_params と同じ寿命・同じ箇所
    /// (insert / remove / clear) で維持する。
    pub slot_has_gui: std::collections::HashMap<u64, bool>,
    /// `device_id` → 現在 plugin_host に load されている plugin の情報。
    /// Undo/Redo の reconcile (`reconcile_plugins_with_song`) で「Song の各
    /// device の plugin が host 側と一致しているか」 を device 粒度で diff する
    /// ために使う。 track を消す前に **Song から** device を列挙して
    /// `ClosePluginShmem` を撃つので (`plan_track_removal_ipc`)、 track → device
    /// の帳簿は持たない (r.md #71 プラグインのコピー / 移動: device の帰属は
    /// Song が SSoT、 複製すると移動で stale になる)。
    ///
    /// 更新タイミング: `SlotPluginLoaded` 受信時に insert、
    /// `SlotPluginUnloaded` 受信時と削除系編集の `_inner` 関数内で remove。
    pub loaded_devices: std::collections::HashMap<u64, LoadedDeviceInfo>,
    /// Phase 2 PR-C: plugin-FX bounce が進行中なら `Some`。 `None` で
    /// 新規 bounce を受け付ける。 同時 1 件のみ。 `AudioCommand::
    /// BounceClipFxOnline` 発火時に `Some` 化、 `AudioEvent::
    /// BounceClipFxComplete` 受信で `None` に戻す + 新 track / 新 clip
    /// 配置。 path / source_track / source_clip は IPC echo back と
    /// pending entry を identifier 照合するために保持。
    pub pending_clip_fx_bounce: Option<PendingClipFxBounce>,
    /// `J` (Glue) の焼き込みが進行中なら `Some` (`docs/plan_glue_bake.md`)。
    /// clip bounce と同じ offline render を共有するので**同時には走らせない**
    /// (どちらかが `Some` の間は新規を受け付けない)。トラックを 1 本ずつ焼き、
    /// 最後の `BounceClipFxComplete` で song へ適用してから `None` に戻す。
    pub pending_glue_bake: Option<PendingGlueBake>,
    /// 歌唱クリップ bounce の合成待ち。`PrepareVocalSynth` を送って
    /// `VocalSynthReady` を待つ間 stable id を退避し、 ready 受信で現在位置へ
    /// 解決して `start_clip_bounce` を呼ぶ。歌唱以外の bounce では使わない。
    pub pending_vocal_synth_bounce: Option<PendingVocalSynthBounce>,
    /// r.md #75: WAV 書き出し前の合成完了待ち。`PrepareVocalSynth` を送った device の
    /// 集合で、`VocalSynthReady` で 1 つずつ減らす。空になったら `ReinitAllPlugins` へ
    /// 進む。bounce (1 件) と違い **曲中の全 VOICEVOX device** が対象。
    ///
    /// 逐次 publish (フレーズ単位) にした以上、待たずに render すると **部分ミックス**を
    /// 掴む。plugin host 側で `done_gen` を守っても、待つ人がいなければ意味がない。
    pub pending_vocal_synth_export: std::collections::HashSet<u64>,
    /// which device (`PluginInstance::id`) plugin editors are currently open.
    /// The editor *windows* are now created and owned by the plugin-host process
    /// (so JUCE cascade sub-menus work); daw_gui only tracks open/closed
    /// state here for toggle / dedup / cleanup. Not `#[cfg(windows)]` because
    /// it's a plain id set — the window FFI lives in the plugin-host process.
    pub open_plugin_guis: std::collections::HashSet<u64>,
    /// v29: `device_id → 要求 generation`。 `SetSlotPlugin` を送ったが
    /// `SlotPluginLoaded` / `SlotPluginLoadFailed` がまだの device 集合。
    /// While non-empty, Play is queued so the audio engine doesn't
    /// dispatch silent buffers for tracks whose plugins are still being
    /// loaded. 応答は entry の generation と一致するものだけ受理する
    /// (A→B 連続差し替えの stale 応答 race 対策、
    /// `docs/plan_arch_refactor.md` §7 世代 guard)。
    pub pending_plugin_loads: std::collections::HashMap<u64, u64>,
    /// `device_id → 直近の load 失敗理由`。 `SlotPluginLoadFailed` を受けた
    /// device は plugin_host に instance が無い (= そのセッション中ずっと
    /// 無音) 状態で song には残る。 ここに残すことでインスペクタが
    /// 「未ロード」として可視化し、 ユーザーが明示的に再 load できる
    /// (`AppEvent::ReloadDevice`)。 自動リトライはしない — plugin 側の
    /// 恒常的な失敗で無限ループになるため。
    ///
    /// entry の寿命: `track_pending_load` (= 新しい load 要求を送る唯一の
    /// 口) で消え、 `on_plugin_load_failed_from_child` で入る。 device が
    /// 消える経路 (`SlotPluginUnloaded` / device 削除 / project 切替) でも
    /// 落とす。 session-only (保存対象外)。
    pub failed_plugin_loads: std::collections::HashMap<u64, String>,
    /// ユーザーが plugin picker で手動追加した plugin の集合 (値 = GUI 自動 open
    /// するか)。load 完了 (`on_plugin_loaded_from_child`) で consume し、(1) daw_audio
    /// へ `LoadSong` を再送して新 plugin を signal path に入れ (Shift 追加でも必須)、
    /// (2) 値が true なら GUI 自動 open を `gui_open_requests` に queue する。
    /// `select_plugin_from_db` と device コピー (r.md #71) が `device_id` を積む。
    /// プロジェクト読込時の一斉復元では積まれない (= project-open 時の初回 LoadSong が
    /// 全 chain を渡すので per-plugin の再 sync は不要、 GUI も自動 open しない)。
    pub pending_added_plugin_finalize: std::collections::HashMap<u64, bool>,
    /// load 完了して「いま開く」段になった GUI auto-open 要求の queue。runner の
    /// frame loop が `drain_pending_gui_opens` で消費し `open_slot_gui` を呼ぶ。
    /// handle_event (IPC 受信) から直接 window を作らず frame loop へ 1 フレーム
    /// 遅延させる seam (headless test は frame loop を回さない → window を作らない)。
    pub(crate) gui_open_requests: Vec<u64>,
    /// `RequestAllStates` を発行した順に保持するキュー (**このタブの** round-trip)。
    /// front が現在 in-flight の request、後続は先行 request の応答後に順次 dispatch
    /// される。空の間は新規 request を発行するときに即時 `RequestAllStates` を送る。
    /// 詳細は [`PendingStateRequest`] / [`crate::app_types::DeferredEdit`]。
    /// `AllPluginStates { project }` は `AppData::handle_event` がこのタブへ配るので、
    /// タブごとに独立した queue でよい (in-flight は host 側も project ごと)。
    pub pending_state_queue: std::collections::VecDeque<PendingStateRequest>,
    /// いま in-flight な `RequestAllStates` (plugin-state round-trip) を送った時刻。
    /// `dispatch_front_state_request` が送信の瞬間に `Some(now)` を立て、
    /// `on_all_states_from_child` で応答が来たら `None` に戻す。plugin host が crash
    /// でなく **hang** した場合は応答が永久に来ないので、`on_tick` の watchdog が
    /// この時刻からの無応答経過を見て round-trip を破棄し、脱出口を作る。
    pub(crate) state_request_sent_at: Option<std::time::Instant>,
    /// `pending_state_queue` の drain 待ちで保留しているガード操作 (`docs/plan_project_tabs.md`
    /// §5.1)。**このタブの round-trip に紐づくのでタブごとに持つ** — アプリ全体に 1 つだと、
    /// 別のタブの round-trip が drain した瞬間に「そのタブの dirty 状態」で再評価され、
    /// 未保存のタブが無確認で閉じられる。
    pub guard_pending_action: Option<crate::app_types::DirtyGuardAction>,
    /// 子プロセス sync (pull 型、 docs/plan_arch_refactor.md §7.5) の世代キー。
    /// [`AppData::flush_song_sync`] が最後に 6 段 choreography を実行したときの
    /// `SongDoc::sync_epoch()` (= 中身が変わった世代、再生状態の変更を含む)。
    /// runner の frame flush が `sync_epoch != last_synced_epoch` を見て 1 frame 1 回だけ LoadSong を送り、 scrub / MIDI-CC
    /// 等の連続編集を構造的に coalesce する (旧 `pending_host_sync` flag +
    /// `flush_pending_host_sync` 経路の置換)。 初期値 0 (SongDoc の epoch は 1 始まり
    /// なので初回 flush が必ず走る)。 session-only。
    pub last_synced_epoch: u64,
}

/// 旧 `VoicevoxState` の per-project 部分 (口パク / 合成状態は track / device / clip id 付き)。
pub struct ProjectVoicevox {
    /// 口パク (lip-sync) 自動再生成の debounce 用世代カウンタ。song 変更で
    /// bump し、`mark_lipsync_dirty` が timer thread に値を渡す。timer 発火時に
    /// 値が一致していれば (= それ以降変更なし) 再生成する (rapid 編集を coalesce)。
    pub lipsync_gen: u64,
    /// 口パク (lip-sync) 背景生成が in-flight な出力先 (口 track id) の集合。
    /// `regenerate_lipsync_for_track` の spawn 時に insert、`LipsyncGenerated` 受信で remove。
    /// クリップ上スピナー / 全体オーバーレイ「口パク生成中」の駆動に使う (派生 UI 状態、非保存)。
    pub lipsync_inflight: std::collections::HashSet<u32>,
    /// 口パク再生成の入力 fingerprint。target (口) track id → 最後に再生成を
    /// 発注した時点の入力ハッシュ (`lipsync_input_fingerprint`)。`LipsyncDebounceFired`
    /// で現在値と比較し、入力 (notes / 歌詞 / bpm / mouth_map / binding / clip 位置) が
    /// 変わった target だけ再生成する。track rename / 色 / mute / volume 等、口パク
    /// 出力に無関係な編集では fingerprint が変わらず再生成をスキップする (派生状態、非保存)。
    pub lipsync_fingerprints: std::collections::HashMap<u32, u64>,
    /// builtin VOICEVOX (歌唱/読み上げ) 合成の per-plugin 状態。key = 安定
    /// device id (v29)。
    /// `VoicevoxSynthStatus` IPC で更新。`busy` = 合成中、`failing_since = Some` は直近 HTTP が
    /// 失敗中 (= engine 未起動/起動途中)。一定時間 (= `VOICEVOX_ENGINE_WARNING`) 続いたら
    /// engine 未接続警告へ切り替える。plugin unload (`SlotPluginUnloadedFromChild`) で entry を消す。
    pub voicevox_synth_status: std::collections::HashMap<u64, VocalSynthStatus>,
    /// r.md #27: builtin VOICEVOX device ごとに、最後に `SetBuiltinPluginNoteMetadata`
    /// で送った `(bpm, chunk_secs, notes, talk)`。`sync_vocal_metadata` は epoch bump のたび
    /// (= あらゆる編集) に呼ばれるが、この device の歌唱/読み上げ入力が前回送信から
    /// 変わっていなければ **再送しない** (= builtin plugin が不要な再合成を走らせない。
    /// Transform 等の非 vocal 編集で VOICEVOX 合成が走る問題の修正)。差分検出で送信を
    /// 抑える `sync_ara_documents` の `ara_doc_cache` と同 idiom。device (re)load 時に
    /// 該当 entry を破棄して初回 seed 合成を保証する (`SlotPluginLoadedFromChild`)。
    pub voicevox_metadata_sent: std::collections::HashMap<
        u64,
        (
            f32,
            f32,
            Vec<common::plugin_metadata::NoteMetadata>,
            Vec<common::plugin_metadata::TalkMetadata>,
        ),
    >,
    /// r.md #75: builtin VOICEVOX device ごとに、最後に `SetVocalSynthPriority` で送った
    /// 再生ヘッド位置 (拍)。1 拍以上動いたときだけ再送するための記憶 (= トランスポート中
    /// でも IPC は数 Hz 以下に収まる)。**再合成はトリガしない**軽量ヒントなので、
    /// `voicevox_metadata_sent` (再送デデュープ) とは別に持つ。
    pub priority_sent: std::collections::HashMap<u64, f64>,
}

/// 旧 `UiPrefs` の per-project 部分 = `ViewState` として `.daw` に保存されるもの +
/// session-only だが Song スコープの id で引くもの。
#[derive(Debug)]
pub struct ProjectView {
    /// 折り畳み中の group track id 集合。 group 自身が `kind == Group`
    /// (= 子を持つ) かつこの set に含まれていれば子孫の row を hide。
    /// **arrangement と mixer が共有する SSoT** で、 反転は
    /// `AppEvent::ToggleGroupCollapsed` の 1 経路のみ (r.md #74)。
    /// session-only: プロジェクト load / New で clear、 track 削除 / ungroup /
    /// undo-redo 後の照合で生存 id へ prune。 save / Undo 対象外。
    pub collapsed_groups: std::collections::HashSet<u32>,
    /// mixer strip の Comp セクションを開いているか (**全 ch 一括**、
    /// `docs/plan_channel_strip.md` §4)。既定は折り畳み。
    /// session-only: 保存 / Undo 対象外 (見方の都合)。
    pub strip_comp_open: bool,
    /// mixer strip の EQ セクションを開いているか (同上)。
    pub strip_eq_open: bool,
    /// gui_01 #028 (M14 Phase 63n-1): automation lane 群を **展開中** の
    /// track id 集合 (Bitwig 流: 既定は折り畳み)。 含まれない track の
    /// `automation_lanes_collapsed = true` を widget へ渡す。 `+` / `-` click
    /// で `ToggleTrackAutomationCollapsed` イベント経由に insert/remove。
    /// プロジェクト保存対象ではない (= session-only): UI 状態は再起動で
    /// 既定 (全 collapsed) に戻る。
    pub expanded_automation_tracks: std::collections::HashSet<u32>,
    /// r.md #110: 折り畳んでいる Parallel / chain の id (既定は展開、閉じた id だけ持つ)。
    /// 「見方の都合」 なので dirty は立てないが `ViewState` で保存する。
    pub collapsed_parallel_nodes: std::collections::HashSet<u64>,
    /// gui_01 #034 (Phase 63n-10): master row の automation 展開状態。
    /// `expanded_automation_tracks` と直交した 1 bool で持つ (= track id
    /// 集合に MASTER_TRACK_ID を入れる方式は sentinel が混ざって SSoT が
    /// 曖昧、 master 専用 field の方が intent が明瞭)。 起動時 false、
    /// `ToggleTrackAutomationCollapsed { track_id: MASTER_TRACK_ID }` で flip。
    /// session-only / Undo / save 対象外。
    pub master_row_automation_expanded: bool,
    /// v37: アレンジで隠しているオートメーションレーン (既定は表示、隠したものだけ持つ)。
    /// パラメータを触ったときに自動生成されるレーンが入り、Alt+A (A ボタン) で全部出す /
    /// 全部隠す。「見方の都合」 なので **Song には置かず dirty も立てない**、`ViewState` で保存。
    pub hidden_automation_lanes: std::collections::HashSet<common::model::AutomationLaneKey>,
    /// gui_01 #031 (M14 Phase 63n-6): track ごとの row 高さ override。
    /// `Some(px)` で個別 track 高さ、`None` (= map に entry なし) で
    /// global default `arrange_track_row_h` を使う。 widget の Alt+drag
    /// or 下端 splitter drag で `SetSingleTrackRowH` 発火 → ここに反映。
    /// Alt+wheel は引き続き global を変える (`SetTrackRowH`)。
    /// session-only (= save / Undo 対象外、 必要になったら `Track.row_h`
    /// として model 化する)。
    pub track_row_overrides: std::collections::HashMap<u32, u16>,
    /// 下部パネル。`None` = 閉じている (アレンジが右カラムの全高を使う)、
    /// `Some(0)` = Mixer タブ、`Some(1)` = Piano Roll (Audio Editor) タブ。
    /// 閉じている間に「どのタブだったか」は持たない — 開く経路はすべて
    /// タブを明示する (`SelectBottomPanel` / `ToggleMixerPanel`)。
    pub bottom_panel: Option<u8>,
    /// (B13 r.md #8) Audio Editor の波形 縦 gain (振幅表示の拡大率)。 Alt+wheel で
    /// 増減。 session-only (描画スケールのみ、 model / 音声には非影響)。
    pub audio_editor_vertical_gain: f32,
    /// Audio Editor の表示状態を **クリップごと** (`ClipKey`) に記憶する
    /// (Ableton Live / Bitwig 流)。 entry が無い (= 初回) クリップは
    /// `open_audio_editor` がクリップ全長を見せる初期 view を入れる。 値域は
    /// `audio_editor_view_state` / `set_audio_editor_*` 経由で読み書きし、 描画前に
    /// clip 長で clamp する。 `ViewState.audio_editor_views` として永続化される。
    pub audio_editor_views:
        std::collections::HashMap<common::model::ClipKey, common::model::AudioEditorViewState>,
    pub arrange_zoom_x: f32,
    pub arrange_scroll_beat: f32,
    /// 再生中プレイヘッド追従スクロールの方式 (Off / Scroll / Page、 `Alt+F` で循環)。
    /// `ViewState` でプロジェクト単位に保存 (snap 設定と同じ idiom)。 再生中に
    /// ユーザーが手動で横スクロール / ズームすると `Off` に落ちる (ユーザー選択)。
    /// `on_tick` が再生中のみこの mode に応じて `arrange_scroll_beat` を更新する。
    pub arrange_follow: common::model::FollowMode,
    /// arrangement の縦 scroll offset (px、 smooth)。 `0.0` で first track
    /// が lanes 上端、 wheel scroll で増減。 widget 側で `SetTrackTop` を
    /// 発火するので handler がここに書き込む。 overscroll (lanes 領域
    /// 外への描画) の scissor は widget 側 (gui_01 #048) の責務。
    pub arrange_track_top: f32,
    /// arrangement の 1 track row 高さ (px)。Alt+wheel で 16..96 に縦ズーム。
    /// default は `ARRANGE_TRACK_HEIGHT`。
    pub arrange_track_row_h: f32,
    /// automation lane の行高 session override (= `Z` 縦ズームで選択
    /// automation clip のレーンを画面いっぱいに拡大した一時値。 model の
    /// `AutomationLane.height_px` は保存対象なので汚さず、 これで上書き表示する)。
    /// 該当 lane の splitter resize (`set_lane_height`) と `X` (zoom back) で解除。
    /// `track_row_overrides` の lane 版。 session-only (save / Undo 対象外)。
    pub automation_lane_row_overrides:
        std::collections::HashMap<common::model::AutomationLaneKey, u16>,
    /// arrangement の track header 幅 (px、 default 160.0)。 header と
    /// lanes の境界 (右端 splitter) drag で gui_01 arrangement widget が
    /// `SetHeaderW` を発火 → `SetArrangeHeaderW` 経由でここを更新する。 widget は
    /// 毎フレーム `view.header_w` としてこの値を読む。 session-only (= save /
    /// Undo 対象外、 `arrange_track_row_h` と同じ扱い)。
    pub arrange_header_w: f32,
    /// ピアノロールの表示状態を **クリップごと** (`ClipKey`) に記憶する
    /// (Ableton Live / Bitwig 流)。 旧来のフラットな `pianoroll_zoom_x` 等は撤去し、
    /// `selected_clip` (= 現在ピアノロールで開いているクリップの `ClipKey`) で引く
    /// accessor (`pianoroll_zoom_x()` / `pianoroll_zoom_y()` / `pianoroll_top_pitch()` /
    /// `pianoroll_scroll_beat()`) に一本化した (= 重複所有を作らない、 SSoT)。 entry が
    /// 無い (= 初回選択) クリップは `select_clip` が `fit_piano_roll_to_clip` で埋める。
    /// `ViewState.piano_roll_views` として永続化される。
    pub piano_roll_views:
        std::collections::HashMap<common::model::ClipKey, common::model::PianoRollViewState>,
    /// **複数クリップ同時表示**中の共有 viewport (song-absolute scroll)。
    /// 単一表示は per-clip 永続 `piano_roll_views` を使うが、複数表示は表示クリップ集合に
    /// 依存する 1 つの transient viewport を使う (非永続)。`multi_clip_view_key` と組で持ち、
    /// 表示クリップ集合が変わったら union bbox に再 fit する。
    pub multi_clip_view: common::model::PianoRollViewState,
    /// `multi_clip_view` がどの表示クリップ集合 (`shown_pianoroll_clips` の
    /// `ClipKey` 列) に対して fit 済かを記録。draw でこれと現在の集合が違えば再 fit。
    pub multi_clip_view_key: Vec<common::model::ClipKey>,
    /// ピアノロールで「ロック (参照専用)」にした **トラック** (track id)。lock された
    /// トラックの (表示中) note は淡色ゴーストで描画され、hit-test / 選択 / 編集から除外される。
    /// 凡例がトラック単位なのでロックもトラック単位 (そのトラックの表示クリップ全部に効く)。
    /// session 内 transient (非永続)。legend のロックトグルで増減。
    ///
    /// **これは「ユーザーの意思」 であって効力ではない** (r.md #64)。 実際に効くのは
    /// `AppData::is_pianoroll_clip_locked_in` = 「凡例に行が出ているトラック
    /// (`AppData::pianoroll_lock_rows_in`)」 ∧ 「この集合に居る」 の派生値。 凡例は複数クリップ
    /// 同時表示のときだけ出るので、 単一表示に絞ると **ロックは自動的に効かなくなり**、
    /// 複数表示に戻すと元どおり効く。 こうしないと「ロック中トラックのクリップを 1 つだけ開くと
    /// ゴーストのまま編集できず、解除ボタンも画面に無い」 詰みが起きる。
    /// 効力を毎回導出することで『効いている ⟺ 解除ボタンが見えている』 が構造的に保たれる。
    pub locked_pr_tracks: std::collections::HashSet<u32>,
    /// FL Studio の smart length 互換: 直近に作成 / リサイズ / クリック選択した
    /// ノートの長さ (拍)。次の新規追加時のデフォルト長として使う。session 内
    /// in-memory のみ、永続化はしない。`add_note` / `resize_notes` /
    /// `SetNoteSelection` ハンドラで更新。
    pub last_note_duration_beats: f64,
    /// piano_roll の Snap on/off (Snap toggle / `G` キー)。
    pub pianoroll_snap_enabled: bool,
    /// `view::snap::SNAP_LABELS` の index。`view::snap::choice_to_mode` で SnapMode に変換。
    pub pianoroll_snap_choice: u8,
    pub arrange_snap_enabled: bool,
    pub arrange_snap_choice: u8,
    /// Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw toggle。 ON のとき
    /// piano_roll で note 追加時の pitch を `Song.scale_at(beat).snap(pitch)` で
    /// in-scale に寄せる。 piano_roll header の toggle で切替、 session-only
    /// state (project save しない)。 Highlight mode が前提 (Fold mode は
    /// widget 側で既に in-scale pitch を push する)。
    pub snap_on_draw: bool,
    /// r.md #65: プラグインエディタ窓の位置 / client サイズ (device_id → geometry)。
    /// 窓を所有するのは daw_plugin_host なので、値の一次情報は
    /// `PluginEvent::SlotGuiGeometry` (open 時 + ドラッグ確定時 + close 直前) だけ。
    /// ここは **その最新値のキャッシュ**で、`ViewState.plugin_editor_windows` として
    /// プロジェクトに保存され、次に開くときに `OpenSlotGuiEmbedded` へ載って復元される。
    /// 「見方の都合」なので更新しても dirty は立てない (memory `project_dirty_flag_rule`)。
    pub plugin_editor_windows:
        std::collections::HashMap<u64, common::model::EditorWindowGeometry>,
    /// Phase 7 B5 (`docs/plan_scale.html` §4.4): piano_roll が Fold mode か。
    /// `true` で out-of-scale 行を非表示 (Ableton K キー Fold to Scale 相当)、
    /// `false` で Highlight mode (root 行強調 + in-scale 通常 + out 行 dim)。
    /// piano_roll snap toolbar の「Fold」 toggle で切替、 session-only state。
    /// `Song.scale_changes` が空のときは `view.scale = None` で機能 OFF。
    pub piano_roll_fold: bool,
    /// ランチャー帯とアレンジのレーンをどう見せるか (`Tab` で巡回)。
    pub launcher_layout: common::model::LauncherLayout,
    /// [`LauncherLayout::Both`](common::model::LauncherLayout::Both) のときの
    /// ランチャー帯の幅 (px)。`0` 以下 = 未設定 (widget の既定幅)。
    /// アレンジと下部パネルの境界比率 (上の取り分)。`ViewState` に保存され、
    /// プロジェクトを開き直しても境界位置が戻らない。`0.0` = 未設定。
    pub arrangement_split_ratio: f32,
    pub launcher_width: f32,
    /// シーン 1 列の幅 (px、全列共通)。`0` 以下 = 未設定 (widget の既定幅)。
    pub launcher_scene_col_w: f32,
    /// ランチャー帯の横スクロール位置 (列数、小数可)。
    pub launcher_scroll_scene: f32,
}

/// 旧 `UiEphemeral` の per-project 部分 (hover / rename / スクラブ / テクスチャキャッシュ等、
/// Song スコープの id で引く一時状態)。
pub struct ProjectEphemeral {
    /// D3/D4: track/clip 名の `Arc<str>` キャッシュ ([`ArrLabelCache`])。 view から
    /// (`&self`) 更新するので `RefCell`。 AppData は GUI メインスレッド専有なので可。
    pub(crate) arr_label_cache: std::cell::RefCell<ArrLabelCache>,
    /// r.md #56: 秒表示用 `TempoMap` の世代キャッシュ ([`TempoMapCache`])。
    /// `arr_label_cache` と同じく view から (`&self`) 更新するので `RefCell`。
    pub(crate) tempo_map_cache: std::cell::RefCell<TempoMapCache>,
    /// GPU-side video thumbnail textures keyed by `VideoSourceId`.
    /// Written by the runner (P3.5) after a successful texture upload;
    /// read by `arrangement_view.rs` (P3.6) and passed to
    /// `ClipView.thumbnail`.
    /// 直近 `AppData::reset_song_scoped_state` が確定した `Song::project_id`
    /// (プロジェクト同一性の SSoT、v24)。同じ project の再読込ではキャッシュを
    /// 捨てないための照合用。`0` = 未確定。
    pub loaded_project_id: u64,
    pub video_texture_cache:
        std::collections::HashMap<common::model::VideoSourceId, daw_ui_renderer::TextureHandle>,
    /// v13: GPU-side image textures keyed by `ImageSourceId`.
    ///
    /// **main window の `Renderer` が払い出した handle だけ**を入れる
    /// (arrangement のクリップサムネイル用)。`TextureHandle` は renderer-local な
    /// id 空間なので、preview 側の handle を混ぜると別名衝突する (r.md #42)。
    /// preview の合成が使う画像テクスチャは `PreviewWindowState::image_textures`。
    pub image_texture_cache: std::collections::HashMap<
        common::model::ImageSourceId,
        daw_ui_renderer::TextureHandle,
    >,
    /// Snapped mouse hover beat inside the arrangement canvas. `None`
    /// outside the canvas. `arrangement_view::draw` updates it every
    /// frame using the current `SnapConfig`. Used by Split (E) so the
    /// split lands at the user's pointer (REAPER edit-cursor flavour)
    /// instead of the playhead (`docs/plan_audio_clip.md` §3.3).
    pub arrangement_hover_beat: Option<f64>,
    /// Same as above but **without** snap applied. Used by Alt+E
    /// (split with snap temporarily disabled).
    pub arrangement_hover_beat_raw: Option<f64>,
    /// `(track, clip)` index pair for the clip the mouse is currently
    /// over (or `None` outside any clip). Lets Split work without an
    /// explicit selection — hover over a clip, press `E`, and that
    /// clip is split. Falls back to the existing `selected_clips`
    /// when no clip is under the cursor.
    pub arrangement_hover_clip: Option<ClipKey>,
    /// アレンジで端オートスクロールの対象になるドラッグ (クリップ / オートメーション /
    /// リージョン / ルーラー等) が進行中か (`ArrangementResponse.edge_scroll_drag` の mirror)。
    /// 再生追従はこの間だけ**一時停止**する — 止めないと Scroll 追従が毎 tick 中央へ
    /// 引き戻し、端スクロールと綱引きになる。離せば追従が続く (解除はしない)。
    pub arrange_drag_active: bool,
    /// `docs/plan_project_tabs.md` §5.6: クリップ Move / トラック並べ替えのドラッグ中か
    /// (`ArrangementResponse.xfer_drag_active` の mirror)。この間の Ctrl+Tab は
    /// 即切替ではなく `ui_ephemeral.pending_tab_switch` に積む。
    pub arrange_xfer_drag_active: bool,
    /// ポインタ下のトラック id (`ArrangementResponse.hovered_track` の
    /// mirror)。トラック paste の挿入先 (= マウス下トラックの直上) に使う。
    /// `arrangement_view::draw` が毎フレーム更新。ヘッダ列・クリップレーンどちらの
    /// 上でも同じトラック行を返す。
    pub arrange_hovered_track: Option<u32>,
    /// Arranger (section 帯) の画面 rect (`ArrangementResponse.arranger_rect` の mirror)。
    /// r.md #128: `R` がポインタ位置で「選択パートの範囲」 か「選択クリップの範囲」 かを
    /// 決めるのに読む。 `arrangement_view::draw` が毎フレーム更新。
    pub arrange_arranger_rect: daw_ui_renderer::Rect,
    /// ランチャー帯全体の画面 rect (`ArrangementResponse.launcher_pane_rect` の mirror)。
    /// `X` がポインタ位置で「帯の全体表示」 か「アレンジの全体表示」 かを決めるのに読む。
    /// `arrangement_view::draw` が毎フレーム更新。 帯が無ければ零 rect。
    pub launcher_pane_rect: daw_ui_renderer::Rect,
    /// ランチャー帯のセル格子の画面 rect (`ArrangementResponse.launcher_grid_rect` の mirror)。
    /// 帯の全体表示 (`fit_launcher_to_scenes`) が「全シーンを何 px に収めるか」 の唯一の根拠。
    /// `last_arrange_lanes_size` と同じ「レイアウト SSoT の実 rect を記録する」 idiom で、
    /// 零 rect は「未測定 / 畳まれている」 (fit を skip)。
    pub launcher_grid_rect: daw_ui_renderer::Rect,
    /// ミキサーでポインタ直下の strip の track id。`mixer_strips::draw`
    /// が毎フレーム更新 (arrangement の `arrange_hovered_track` と同 idiom)。S キーで
    /// マウス直下のストリップを solo するために `dispatch_shortcuts` が読む。master
    /// strip は solo を持たないので None 扱い。
    pub mixer_hovered_track: Option<u32>,
    /// mixer strip の内蔵チャンネルストリップ帯で、いまカーソルが乗っている
    /// `(track_id, セクション)` (`docs/plan_channel_strip.md`)。常設帯 (GR / カーブ) と
    /// 開いているセクションの両方が対象。`Q` (mute) がこれを見て、トラックの mute
    /// ではなく **そのセクションのバイパス**を切り替える。
    /// 算出は `view::strip_sections` の 1 か所 (SSoT)、strip 外は `None`。
    pub mixer_hovered_strip_section: Option<(u32, crate::event::StripSection)>,
    /// r.md #105: インスペクタのチェーンで、いまカーソルが乗っている行の device id。
    /// `track_inspector` が毎フレーム更新 (`mixer_hovered_track` と同 idiom)。`Q` が
    /// これを見て「選択 device があればそれら、無ければこの行」を bypass 切替する。
    /// チェーン外 / インスペクタ非表示は `None`。
    pub inspector_hovered_device: Option<u64>,
    /// r.md #115: インスペクタの変調ラックで、 いまカーソルが乗っているモジュレーター
    /// (ヘッダ行 / 展開した本体) または routing 行。 `modulation_rack` が毎フレーム更新
    /// (`inspector_hovered_device` と同 idiom)。 `Q` がこれを見てバイパスを切り替える。
    pub inspector_hovered_mod: Option<ModRackHover>,
    /// マスターストリップで、いまカーソルが乗っているブロック
    /// (`docs/plan_master_strip.md` §3)。`Q` がこれを見てそのセクションの
    /// バイパスを切り替える。算出は `view::master_strip_ui` の 1 か所。
    pub master_hovered_section: Option<crate::event::MasterSection>,
    /// マスターフェーダーを掴んでいるか (undo gesture の edge 検出用)。
    /// `Song.master_gain` を編集するようになったので、drag 全体を 1 undo step に
    /// bracket しないと per-frame の編集が履歴を埋める (group transform /
    /// inspector scrub と同じ罠)。session-only。
    pub master_gain_dragging: bool,
    /// ピアノロール grid 上のポインタ拍 (clip-local, snap 済)。
    /// ノート paste の配置位置に使う。`piano_roll` widget が毎フレーム更新、
    /// grid 外 / 非 piano-roll は `None`。
    pub pianoroll_hover_beat: Option<f64>,
    /// ピアノロール grid 上のポインタ拍を **song-absolute かつ snap なし**
    /// (= clip_start_beat を引く前の生 beat) で mirror。`f` キー (PlayFromCursor) は
    /// song-absolute の grid で snap する必要があるため、`pianoroll_hover_beat`
    /// (clip-local snap 済) とは別に保持する。grid 外 / 非 piano-roll は `None`。
    pub pianoroll_hover_beat_song_raw: Option<f64>,
    /// ピアノロール grid 上のポインタ直下の note index (= clip 内 notes Vec の
    /// index、`selected_notes` と同空間)。`q` キーで「選択が無ければカーソル直下 note を
    /// mute」する対象解決に使う。`piano_roll` widget が `note_hit` で毎フレーム更新、
    /// grid 外 / note 外 / 非 piano-roll は `None`。
    pub pianoroll_hover_note: Option<u32>,
    /// r.md #119: ピアノロール grid 上のポインタの鍵盤行 (MIDI pitch)。 Ctrl+A の 1 段目
    /// (その行の全ノート) が見る。 `piano_roll` widget が毎フレーム更新、 grid 外は `None`。
    pub pianoroll_hover_pitch: Option<u8>,
    /// inline 数値入力中の automation point (`Some` のとき
    /// `arrangement_view` が当該 point の rect に `text_input_at_focused`
    /// overlay を出す)。session-only / Undo・save 対象外。点をダブルクリック
    /// で `Some`、 確定 (Enter) / blur / Esc で `None`。
    pub editing_automation_point: Option<AutomationPointKeyRef>,
    /// gui_01 #028 §7.3: 最後にユーザーが触った parameter。`A` キー
    /// shortcut で「対応 lane を所有 track に追加」 する source。
    /// session-only (起動 None、Undo / save 対象外)。
    pub last_touched_param: Option<TouchedParam>,
    /// r.md #88: 変調ラックのプレビュー用 `beat → song 秒` の直近 1 件 memo
    /// `(edit_epoch, beat, secs)`。 テンポカーブのある曲では `beats_to_samples` が
    /// **O(拍)** の積分で、 描画は毎フレーム走る。 1 フレームの中で同じ拍が何度も
    /// 問われる (transport 位置 + `build_plan` の anchor) ので 1 件で足りる。
    /// `edit_epoch` を鍵に含めるので、 テンポカーブを描き替えたら自動で無効になる。
    pub preview_secs_memo: std::cell::Cell<Option<(u64, f64, f64)>>,
    /// r.md #10: `Home` の 2 段トグル state。 直前の `Home` が「先頭 (時間的に
    /// 最初) のクリップの頭」 へ飛んだなら `true`。 次の `Home` はこれを見て
    /// 1.1.1 (beat 0) へ戻す。 **live playhead 位置で判定しない**理由: 再生中は
    /// playhead が毎フレーム進むので位置比較では 2 度押しが成立しない
    /// (レビュー指摘)。 明示 seek (`seek_playhead_to`) / 停止で false にリセット
    /// され、 再生中の playhead poll では触らないので、 再生中でも確実にトグルする。
    pub(crate) home_toggle_at_first: bool,
    /// gui_01 #068: 前フレームに arrangement でホバーされた clip の
    /// `content_id` (= 連動ハイライトの active group 計算に使う held-value)。
    /// widget の `ArrangementResponse.hovered_clip` を毎フレーム解決して
    /// 保持する。 session-only。
    pub arrange_hover_content: Option<common::model::ContentId>,
    /// gui_01 #090: ポインタが今乗っている automation lane の key
    /// (`ArrangementResponse.hovered_automation_lane` を毎フレーム mirror)。
    /// Ctrl+A の「lane の全ポイント選択 → 全クリップへ段階拡大」振り分けに
    /// 使う。 `None` = clip 領域 / lane 外。 1 フレーム遅延だが pointer は
    /// 瞬間移動しないので実用上問題なし (= `arrange_hover_content` と同 idiom)。
    pub arrange_hovered_automation_lane: Option<common::model::AutomationLaneKey>,
    /// arrangement ヘッダのトラック音量スライダを drag 中のトラック id
    /// (`ArrangementResponse.dragging_track_volume` を前フレーム値として mirror)。
    /// None↔Some の edge で `ParamGestureBegin`/`End` を発火し、 mixer フェーダーと
    /// 同じ「1 drag = 1 undo step」 経路 (gesture begin で 1 snapshot) に乗せる。
    /// session-only (`arrange_hover_content` と同 idiom)。
    pub arrange_dragging_track_volume: Option<u32>,
    /// piano_roll widget が歌詞 inline 編集 (gui_01 #017、 note 上の L キー編集) の
    /// text_input overlay を出している間 `true`。 widget 内部状態 (`PianoRollState`) の
    /// session-only ミラーで、 `piano_roll` widget が毎フレーム `resp.lyric_editing`
    /// から更新する (project save には含めない)。
    ///
    /// 用途は root.rs の Esc dispatch との調停。 `dispatch_shortcuts` は
    /// piano_roll widget より前に走って `take_shortcut("escape")` を消費するため、
    /// このミラーが立っている間は Esc を消費せず widget に委ねる (widget 側が歌詞編集を
    /// キャンセルする)。 ミラーは 1 frame 遅延だが、 編集モードは L 押下〜Esc 押下まで
    /// 複数フレーム持続するので調停に支障はない。
    pub piano_roll_lyric_editing: bool,
    /// r.md #67: ピアノロールの note grid の表示範囲 `(拍数, 半音数)`。
    /// `piano_roll` widget が毎フレーム mirror する session-only 値 (project save に含めない)。
    ///
    /// 表示範囲は grid の px サイズ ÷ zoom で決まるので **view しか知らない**。 一方、
    /// カーソルキーで動かしたノートを画面内に追う処理は handler 側にある
    /// (`AppData::nudge_selected_notes_*`)。 そのために「今どれだけ見えているか」 だけを
    /// mirror する。 `None` = ピアノロールがまだ 1 度も描かれていない (追従しない)。
    pub pianoroll_viewport: Option<(f64, f32)>,
    /// `Some(target)` で Audio Editor (= clip ダブルクリックで開く波形
    /// 編集 view) が開いている。 bottom_panel の Piano Roll タブが
    /// audio_editor view に切り替わる (`docs/plan_audio_clip.md` §3.10
    /// 「piano_roll の領域を流用」)。 `None` なら通常の Piano Roll が
    /// 表示される。 audio clip ダブルクリックで `Some` 化、 Esc / Audio
    /// Editor close で `None` に戻る。
    pub audio_editor_clip: Option<ClipKey>,
    /// ピアノロールの**対象 (target) クリップ**の focus ヒント。
    ///
    /// 選択そのものではない (選択の SSoT は `SelectionState::time` 1 本) —
    /// 「同時表示している MIDI クリップのうち、どれを編集の主対象にするか」 という
    /// カーソル位置に相当する。 凡例の行クリック (`SetPianoRollTargetClip`) で動き、
    /// 表示集合に居ない値は [`AppData::pianoroll_target_clip`] が無視するので stale で
    /// 壊れない。 `None` なら「範囲の先頭に最も近いクリップ」 が対象になる。
    pub pianoroll_focus_clip: Option<ClipKey>,
    /// Audio Editor 内のマウス hover 位置を clip 内 beat (clip 始端 = 0)
    /// に変換した値。 audio_editor.rs が毎フレーム push、 マウスが
    /// waveform 領域外なら `None`。 E キー (split) と将来の波形クリック
    /// 系操作で「マウス位置を cursor として使う」 ために保持する。
    pub audio_editor_hover_beat_in_clip: Option<f64>,
    /// `Z` キーの段階ズーム履歴。 1 回目 push で横ズーム前の view、
    /// 2 回目 push で縦ズーム前の view を積む。 `X` が pop して 1 段ずつ戻し、
    /// 空になったら全体フィットに落ちる。 load / new / recovery で clear。
    pub(crate) arrange_zoom_history: Vec<ArrangeViewSnapshot>,
    /// `Z` 段階ズームの現在アンカー (直近 Z が適用した選択 + view + 段数)。
    /// 次の Z で選択 or view が食い違えば段階 0 (横) から仕切り直す。 session-only。
    pub(crate) arrange_zoom_anchor: Option<ArrangeZoomAnchor>,
    /// `Z` の縦ズームがレーンを viewport 高いっぱいへ広げた**その 1 行**。
    ///
    /// 拡大は `ui_prefs.automation_lane_row_overrides` に書くが、あの map には
    /// **fit (`X`) が縮めた行高**も同居する。 どちらが書いたかを持たずに map ごと
    /// 捨てると、fit で縮めたレーンが次の `Z` で model 高さへ跳ね上がる
    /// (実機報告「1 回目の Z でオートメーションレーンが高くなる」)。 誰が書いたかは
    /// ここが持つ。 session-only。
    pub(crate) zoom_lane_fill: Option<common::model::AutomationLaneKey>,
    /// inspector の param セクション (title 下〜chain 上) の実描画高さ
    /// (px)。 immediate-mode なので「前フレームに測った高さ」を `scroll_area` の
    /// content_size として使う (= lag-by-one)。 描画末尾で実測値に更新。
    /// session-only (save / Undo 対象外)。
    pub inspector_body_h: f32,
    /// チェーン行アコーディオンで開いているデバイスの param パネル実高さ
    /// (px、 前フレーム測定値)。 `reorderable_list_expandable` の `row_extra_h` に渡して
    /// 開いた行の直下に確保する展開高に使う (lag-by-one、 `inspector_body_h` と同 idiom)。
    /// session-only。
    pub inspector_device_panel_h: f32,
    /// auto-fit (`X` キー / `Fit` ボタン / SelectClip 経由) で参照する piano_roll
    /// grid 領域サイズ (px)。`view::root` / `view::bottom_panel` が piano_roll タブ
    /// 描画時に毎フレーム書き込む。0 は「未測定」フラグ扱い (auto-fit を skip)。
    pub last_pianoroll_grid_size: (f32, f32),
    /// piano_roll がまだ一度も描画されていない (= `last_pianoroll_grid_size` 未測定)
    /// 状態で auto-fit が要求された場合に立つフラグ。 初回描画で grid_size が
    /// 確定したフレームの Edit 内で消費 → `fit_piano_roll_to_clip` を再実行する。
    /// これが無いと「Piano Roll タブ未表示で clip を選択 → タブを開いても fit
    /// されない、 2 回目以降のみ fit」 という初回 fit 喪失バグになる。
    pub pending_pianoroll_fit: bool,
    /// r.md #63: arrangement widget が分割した **`lanes` Rect の実寸** (px)。
    /// ruler と Arranger (section) 帯を除いた、 track 行が実際に描かれ scissor される領域。
    /// `last_pianoroll_grid_size` と同じ「レイアウト SSoT が返した実 rect を記録する」 idiom で、
    /// widget が毎フレーム書き込む。 `0.0` は「未測定」 (auto-fit を skip)。
    ///
    /// **式で再導出しないこと**。 以前は widget 側が `area.h - RULER_H` と独立に計算しており、
    /// Arranger 帯 18px を引き忘れて `X` の全体表示が常に下へはみ出していた。
    pub last_arrange_lanes_size: (f32, f32),
    /// r.md #63: arrangement widget がこのフレームに縦へ積んだ行の一覧 (描画順、 culling 前)。
    /// `X` の全体表示 / `Z` の縦ズームが行数・行高・行の content-Y を引く唯一の根拠
    /// (= 可視 track / 展開 lane の集合をモデルから再導出しない)。 widget 未描画なら空。
    pub last_arrange_rows: Vec<crate::widgets::arrangement::ArrangementRow>,
    /// r.md #110: `SC` パネルを展開中の device (plugin 行の直下に port ごとの配線を出す)。
    pub open_sidechain_panel: Option<u64>,
    /// r.md #110: 名前を編集中の chain / Parallel (id) とその編集バッファ。
    pub renaming_chain: Option<(u64, String)>,
    /// 内蔵映像 FX は plugin window を持たないので、チェーン行の "GUI"
    /// ボタンはインスペクタ内のパラメータ調整パネルを開く。`Some(device_id)`
    /// で 1 つだけ開く（別の FX の GUI を押すと切り替わる）。
    ///
    /// r.md #71 (プラグインのコピー / 移動): cursor track 以外の device を指して
    /// いたら **閉じるのではなく描画側 gate が非表示にする**。 こうすると device を
    /// 別トラックへ移してもパネルが自然に追従する。 `None` に落とすのは device が
    /// song から消えたときだけ。
    pub open_video_fx_params: Option<u64>,
    /// 埋め込み GUI を持たない plugin (VOICEVOX builtin / GUI 無し
    /// CLAP・VST3) の「⚙」ボタンで開くインライン param パネル。
    /// `open_video_fx_params` と同 idiom。
    pub open_plugin_params: Option<u64>,
    /// rename 中の track の **安定 ID** (positional index ではない)。 index で持つと
    /// track の reorder / delete で別 track に rename がすり替わる SSoT 違反になる
    /// (2026-06-09 の「最上段だけ rename できない / フリーズ」バグの原因)。 None で非 rename。
    pub track_rename_id: Option<u32>,
    pub track_rename_text: String,
    /// 編集中の clip rename。 `Some` のとき該当 clip rect に inline
    /// text_input を重ね描きする (track rename の clip 版)。 `ClipKey` は
    /// 安定 id なので rename 中に並べ替えても対象は動かない。
    pub clip_rename: Option<ClipKey>,
    pub clip_rename_text: String,
    /// v18 (`docs/plan_track_clip_color.md`): color_picker (gui_01 #058) の
    /// 開いている編集対象 (track / clip / automation lane / automation clip / section /
    /// scene)。`None` で非表示。`open_color_picker` で `Some` に、`close_color_picker`
    /// (dismiss / 対象消失) で `None` に戻す。
    pub color_picker_target: Option<ColorPickerTarget>,
    /// color_picker overlay の anchor 矩形 (popup を出す基準位置)。開いた場所
    /// (右クリックした header / clip rect、inspector のスウォッチ rect) を保持し、
    /// どの view から開いても同じ位置に popup が出るようにする。
    pub color_picker_anchor: Option<daw_ui_renderer::Rect>,
    /// gui_01 #071: 空きレーン右クリック (空きレーン右クリック SecondaryClickEmpty)
    /// で開く clip 生成コンテキストメニューの stash。`Some((track_id, snap 済み beat,
    /// 右クリック viewport pos))` の間、毎フレーム `ui.context_menu_at` で `pos` に
    /// メニューを描画する (color_picker overlay と同 idiom)。on_select (= Text クリップ
    /// 生成) で `None` に戻す。
    pub clip_create_menu: Option<(u32, f64, (f32, f32))>,
    /// 上記メニューの 1-shot open trigger。`SecondaryClickEmpty` 受信 Edit で `true` に
    /// し、overlay が `open_at = Some(pos)` を 1 フレームだけ渡したら `false` に戻す
    /// (毎フレーム `Some` を渡すと outside-click で閉じても翌フレーム再 open するため)。
    pub clip_create_menu_open: bool,
    /// Arranger セクション帯の右クリックメニュー stash `(section_id, 右クリック pos)`。
    /// `SecondaryClickSection` 受信で set、 overlay が pos にメニュー (ループ / 帯削除 /
    /// 範囲削除) を描画、 on_select で `None` に戻す (`clip_create_menu` と同 idiom)。
    pub section_menu: Option<(u32, (f32, f32))>,
    /// 上記セクションメニューの 1-shot open trigger (`clip_create_menu_open` と同 idiom)。
    pub section_menu_open: bool,
    /// inline 改名中のセクション id (`track_rename_id` の section 版)。`Some` の間、
    /// arrangement view が該当帯 rect に text_input を重ねる。
    pub section_rename_id: Option<u32>,
    /// 上記改名の編集中文字列。
    pub section_rename_text: String,
    /// Transport BPM 入力欄の編集中文字列。 commit (Enter) で parse + clamp +
    /// `song.bpm` に反映、 song を切り替える際 (open / new / undo / redo) は
    /// `resync_song_edit_texts` で formatted な現値に書き戻す。
    pub bpm_edit_text: String,
    /// Transport time_sig numerator 入力欄の編集中文字列。 同上。
    pub time_sig_num_edit_text: String,
    /// 現 buffer がどの clip 用にロードされているか。 `selected_clip` が
    /// 変わったら view 側が `AppEvent::ResyncClipEditBuffers(target)` を
    /// 発火して `resync_clip_audio_event_edit_buffers` で再生成。 `None`
    /// は「未ロード」 (= 起動直後 / clip 未選択)。 編集 buffer の中身が
    /// この target の現値と整合する保証はないが (= ユーザー入力中はズレる)、
    /// commit / resync で必ず書き戻す。
    /// audio / image inspector の数値 field は scrubable_number
    /// (drag + type) 化されたため、 個別の名前付き edit-buffer
    /// (`clip_gain_db_edit_text` 等 / `clip_image_*_edit_text`) は撤去
    /// (scrubable が編集状態を自前で内包)。 `clip_edit_buffer_target` は
    /// content / font_family 文字列 buffer の resync 判定に引き続き使う。
    pub clip_edit_buffer_target: Option<ClipKey>,
    /// **いま開いている undo bracket の所有者** (`None` = idle)。
    ///
    /// インスペクタ / レーン既定値 / 変調深さ / 変調ラック / グループ変換の
    /// スクラブは、どれも `Song` を毎フレーム書くので 1 本の gesture に束ねる。
    /// 追跡側を面ごとに分けていた頃は、`SongDoc` の bracket が 1 本しか無いのに
    /// tracker が 5 本あり「A が開けたまま B が閉じる」が黙って作れた。
    /// 出入りは [`crate::view::scrub_gesture`] だけを通す。 session-only。
    pub scrub_gesture: Option<ScrubGesture>,
    /// [`Self::scrub_gesture`] の所有者が **今フレームも自分を描いたか**。
    ///
    /// 毎フレーム末に [`crate::view::scrub_gesture::sweep`] が見て、描かれて
    /// いなければ gesture を閉じる。開始と終了の対に頼ると、欄が画面から消えた
    /// (選択が変わる / パネルが閉じる / トラックが削除される) ときに終了が
    /// 来ず、**以降の編集が全部 1 undo step へ束ねられ続ける**。
    pub scrub_gesture_seen: bool,
    /// docs/plan_modulation.md §3: true while an envelope-follower attack /
    /// release scrub is being dragged. The scrub mutates the value + marks
    /// dirty each frame but defers the (recompiling) `flush_song_sync`
    /// to the drag-end edge, avoiding a per-frame LoadSong storm.
    ///
    /// **undo bracket のエッジ検出には使わない** — それは
    /// [`Self::scrub_gesture`] ([`ScrubGesture::ModRack`]) が持つ。ここは
    /// `SetModFollowerScrubbing` が記録する事実だけ。
    pub mod_follower_scrub_active: bool,
    /// docs/plan_modulation_routing_redesign.md §6: the `ModSource` currently
    /// **armed** for assignment (Bitwig 流). `Some(id)` ⇒ every modulatable
    /// inspector param control shows depth-drag edit mode (`scrubable_number_at`
    /// の `Modulation::edit`); dragging a control assigns / sets that source's
    /// depth on the control's target. `None` ⇒ controls show existing routings
    /// (entries + live tick) but aren't editable. session-only (not persisted).
    pub armed_mod_source: Option<u32>,
    /// the set of `ModSource`s whose inspector row is **expanded** to its
    /// full Bitwig 風グラフィカルエディタ (MSEG curve canvas / Steps grid / LFO·Random
    /// preview + 全コントロール). **Multi-expand** — 複数同時に開ける (Bitwig 同様)。
    /// 行頭の disclosure (r.md #74 で `view::disclosure` へ一本化。 rack 行は縦積みで
    /// 中身が下に開くので開示軸 Block = 折り畳み中 ▶ / 展開中 ▼) クリックで toggle。
    /// session-only (not persisted)。
    pub expanded_mod_sources: std::collections::HashSet<u32>,
    /// docs/plan_text_overlay.md §4 P5: text inspector の文字列 edit buffer。
    /// `text` / `font_family` は文字列 field なので text_input のまま
    /// standalone (= scrubable 化されない)。 Enter / focus 喪失で
    /// `CommitClipText{Content,FontFamily}Edit` を発火。
    /// 25 numeric field は scrubable_number 化され、 `clip_text_num_edits`
    /// HashMap は撤去 (scrubable が編集状態を自前で内包)。
    pub clip_text_content_edit_text: String,
    pub clip_text_font_family_edit_text: String,
    /// 「＋ Send」 ボタンで開く宛先トラックピッカーの状態。 `Some` の間
    /// modal が開いており、 宛先選択 or 閉じる操作で `None` に戻る。
    /// plugin picker の `is_plugin_picker_open` と同 idiom。
    pub send_picker: Option<SendPickerState>,
}

impl ProjectEphemeral {
    /// inline リネームの入力欄が出ているか (トラック名 / セクション名 / クリップ名)。
    ///
    /// **rename 状態を足したらここへ足す。** `AppData::edit_surface` の手順 0 は
    /// この述語で「リネーム中はどの面も対象にしない」を決めており、漏らすと
    /// 入力欄が画面外へ出た瞬間に typing lock が外れて Delete が編集面へ抜ける
    /// (r.md #87 の列見出し rename は `LauncherUiState` 側にあるので、面の
    /// arbiter がその 1 つを別に足している)。
    #[must_use]
    pub fn inline_rename_active(&self) -> bool {
        self.track_rename_id.is_some()
            || self.section_rename_id.is_some()
            || self.clip_rename.is_some()
    }
}

/// 1 タブぶんの全状態。`AppData::cur` (いま見えているタブ) か `Tabs::parked` に居る。
pub struct ProjectState {
    /// タブの住所 (プロセス生存中に再利用しない、`docs/plan_project_tabs.md` §1.1)。
    pub key: ProjectKey,
    /// Song 文書 + undo/redo + dirty/epoch (編集は `SongDoc::edit` 経由のみ)。
    pub song_doc: SongDoc,
    /// 再生 / metering / export 進行。
    pub transport: TransportState,
    /// 選択集合 (clip / note / automation / track / section) + last-wins tier。
    pub selection: SelectionState,
    /// メディア import staging / decode cache。
    pub media: MediaState,
    /// MIDI 録音 / step 入力 / param gesture 録音。
    pub recording: RecordingState,
    /// r.md #87: クリップランチャーの一時状態 (フォーカス / hover / 列名の
    /// 編集中テキスト / MIDI bind 表)。曲の中身は `Song`、見方の都合は
    /// `ProjectView` なので、ここは **保存しないもの**だけ。
    pub launcher: LauncherUiState,
    /// r.md #54: 範囲ラウドネス解析の進行とレポート (session-only)。
    pub loudness: LoudnessState,
    /// 子プロセスとの帳簿 (device_id keyed)。
    pub pipc: ProjectIpc,
    /// VOICEVOX / 口パクの per-project 状態。
    pub pvv: ProjectVoicevox,
    /// View 構成 (zoom / scroll / snap / panel)。
    pub view: ProjectView,
    /// 一時 UI 状態 (hover / rename / scrub / テクスチャキャッシュ)。
    pub peph: ProjectEphemeral,
}

impl ProjectState {
    /// 空の Untitled プロジェクト (起動時 / `Ctrl+N` の新しいタブ)。
    #[must_use]
    pub fn new_untitled(key: ProjectKey) -> Self {
        let mut song = Song::default();
        // **id は必ず allocator から採る。** `Track::default()` の `id` は
        // 「未採番」の sentinel (0) で、0 を実トラックの住所として使うと
        // `RowKey::packed()` が 0 になり、`audio_bridge` の「空きスロット =
        // row_key 0」規約と衝突する (= ランチャーの走行状態が 1 行も GUI へ
        // 届かず、セルの進捗が永久に出ない)。起動直後の 1 本目でそれを踏んでいた。
        let first_track_id = song.alloc_track_id();
        song.tracks.push(track_with(|t| {
            t.id = first_track_id;
            t.name = "Track 1".into();
        }));
        // 初期プロジェクトにも安定 project_id を採番する
        // (clipboard の同一プロジェクト判定用)。
        song.ensure_project_id();
        Self::from_song(key, song)
    }

    /// `song` を中身にした新しい project 状態 (view / 帳簿は空)。
    #[must_use]
    pub fn from_song(key: ProjectKey, song: Song) -> Self {
        let initial_peak_display = vec![(0.0, 0.0, 0.0); song.tracks.len()];
        let initial_bpm = song.bpm;
        let initial_time_sig_num = song.time_sig.0;
        Self {
            key,
            // r.md #54: 解析はセッション限りなので既定 (Idle / レポート無し)。
            loudness: LoudnessState::default(),
            song_doc: SongDoc::new(song),
            transport: TransportState::new(initial_peak_display),
            selection: SelectionState {
                selected_track_ids: Vec::new(),
                selected_section_ids: Vec::new(),
                selected_scene_ids: Vec::new(),
                selected_automation_clips: Vec::new(),
                last_edit_select: None,
                selected_automation_points: Vec::new(),
                time: None,
                range_anchor: None,
                selected_launcher_cells: Vec::new(),
                launcher_cell_anchor: None,
                selected_device_ids: Vec::new(),
                track_anchor: None,
                section_anchor: None,
                automation_point_anchor: None,
                automation_clip_anchor: None,
                device_anchor: None,
                scene_anchor: None,
            },
            media: MediaState {
                audio_source_cache: AudioSourceCache::new(),
                video_thumbnail_rgba: std::collections::HashMap::new(),
                pending_thumbnail_uploads: Vec::new(),
                image_source_bgra: std::collections::HashMap::new(),
                pending_image_uploads: Vec::new(),
                asset_decode: None,
                load_progress: None,
                load_progress_label: "",
            },
            recording: RecordingState {
                recording_mode: common::model::RecordingMode::default(),
                requested: false,
                live: false,
                count_in_bars: 0,
                midi_recording_active_notes: std::collections::HashMap::new(),
                monitor_notes: std::collections::HashSet::new(),
                metronome_enabled_pre_recording: None,
                midi_learn_target: None,
                active_param_gestures: std::collections::HashSet::new(),
                latched_param_gestures: std::collections::HashSet::new(),
                recording_last_beat: std::collections::HashMap::new(),
                last_sent_recording_lanes: std::collections::HashSet::new(),
                preview_note: None,
                nudge_audition: None,
                midi_input_label: String::new(),
                step_cursor_beat: 0.0,
                step_size_beats: DEFAULT_NOTE_DURATION,
                snap_live_input: false,
            },
            // r.md #87: ランチャーの一時状態 (フォーカス / hover / MIDI bind)。
            launcher: LauncherUiState::default(),
            pipc: ProjectIpc {
                ara_doc_cache: std::collections::HashMap::new(),
                ara_pcm_materialized: std::collections::HashMap::new(),
                plugin_param_values: std::collections::HashMap::new(),
                plugin_params: std::collections::HashMap::new(),
                slot_has_gui: std::collections::HashMap::new(),
                loaded_devices: std::collections::HashMap::new(),
                pending_clip_fx_bounce: None,
                pending_glue_bake: None,
                pending_vocal_synth_bounce: None,
                pending_vocal_synth_export: std::collections::HashSet::new(),
                open_plugin_guis: std::collections::HashSet::new(),
                pending_plugin_loads: std::collections::HashMap::new(),
                failed_plugin_loads: std::collections::HashMap::new(),
                pending_added_plugin_finalize: std::collections::HashMap::new(),
                gui_open_requests: Vec::new(),
                pending_state_queue: std::collections::VecDeque::new(),
                state_request_sent_at: None,
                guard_pending_action: None,
                last_synced_epoch: 0,
            },
            pvv: ProjectVoicevox {
                lipsync_gen: 0,
                lipsync_inflight: std::collections::HashSet::new(),
                lipsync_fingerprints: std::collections::HashMap::new(),
                voicevox_synth_status: std::collections::HashMap::new(),
                voicevox_metadata_sent: std::collections::HashMap::new(),
                priority_sent: std::collections::HashMap::new(),
            },
            view: ProjectView {
                collapsed_groups: std::collections::HashSet::new(),
                strip_comp_open: false,
                strip_eq_open: false,
                expanded_automation_tracks: std::collections::HashSet::new(),
                collapsed_parallel_nodes: std::collections::HashSet::new(),
                master_row_automation_expanded: false,
                hidden_automation_lanes: std::collections::HashSet::new(),
                track_row_overrides: std::collections::HashMap::new(),
                bottom_panel: Some(0),
                audio_editor_vertical_gain: 1.0,
                audio_editor_views: std::collections::HashMap::new(),
                arrange_zoom_x: ARRANGE_PX_PER_BEAT,
                arrange_scroll_beat: 0.0,
                arrange_follow: common::model::FollowMode::default(),
                arrange_track_top: 0.0,
                arrange_track_row_h: ARRANGE_TRACK_HEIGHT,
                automation_lane_row_overrides: std::collections::HashMap::new(),
                arrange_header_w: 160.0,
                piano_roll_views: std::collections::HashMap::new(),
                multi_clip_view: common::model::PianoRollViewState::default(),
                multi_clip_view_key: Vec::new(),
                locked_pr_tracks: std::collections::HashSet::new(),
                last_note_duration_beats: DEFAULT_NOTE_DURATION,
                pianoroll_snap_enabled: true,
                pianoroll_snap_choice: crate::view::snap::CHOICE_PIANOROLL_DEFAULT,
                arrange_snap_enabled: true,
                arrange_snap_choice: crate::view::snap::CHOICE_ARRANGE_DEFAULT,
                snap_on_draw: false,
                plugin_editor_windows: std::collections::HashMap::new(),
                piano_roll_fold: false,
                // r.md #87: 0 = 未設定 → widget が既定幅を使う。
                launcher_layout: common::model::LauncherLayout::default(),
                // 0.0 = 未設定 (view が既定比率へ倒す)。
                arrangement_split_ratio: 0.0,
                launcher_width: 0.0,
                launcher_scene_col_w: 0.0,
                launcher_scroll_scene: 0.0,
            },
            peph: ProjectEphemeral {
                arr_label_cache: std::cell::RefCell::default(),
                tempo_map_cache: std::cell::RefCell::default(),
                loaded_project_id: 0,
                video_texture_cache: std::collections::HashMap::new(),
                image_texture_cache: std::collections::HashMap::new(),
                arrangement_hover_beat: None,
                arrangement_hover_beat_raw: None,
                arrangement_hover_clip: None,
                arrange_drag_active: false,
                arrange_xfer_drag_active: false,
                arrange_hovered_track: None,
                arrange_arranger_rect: daw_ui_renderer::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
                launcher_pane_rect: daw_ui_renderer::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
                launcher_grid_rect: daw_ui_renderer::Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
                mixer_hovered_track: None,
                mixer_hovered_strip_section: None,
                inspector_hovered_device: None,
                inspector_hovered_mod: None,
                master_hovered_section: None,
                master_gain_dragging: false,
                pianoroll_hover_beat: None,
                pianoroll_hover_beat_song_raw: None,
                pianoroll_hover_note: None,
                pianoroll_hover_pitch: None,
                editing_automation_point: None,
                last_touched_param: None,
                preview_secs_memo: std::cell::Cell::new(None),
                home_toggle_at_first: false,
                arrange_hover_content: None,
                arrange_hovered_automation_lane: None,
                arrange_dragging_track_volume: None,
                piano_roll_lyric_editing: false,
                pianoroll_viewport: None,
                audio_editor_clip: None,
                pianoroll_focus_clip: None,
                audio_editor_hover_beat_in_clip: None,
                arrange_zoom_history: Vec::new(),
                arrange_zoom_anchor: None,
                zoom_lane_fill: None,
                inspector_body_h: 800.0,
                inspector_device_panel_h: 0.0,
                last_pianoroll_grid_size: (0.0, 0.0),
                pending_pianoroll_fit: false,
                last_arrange_lanes_size: (0.0, 0.0),
                last_arrange_rows: Vec::new(),
                open_sidechain_panel: None,
                renaming_chain: None,
                open_video_fx_params: None,
                open_plugin_params: None,
                track_rename_id: None,
                track_rename_text: String::new(),
                clip_rename: None,
                clip_rename_text: String::new(),
                color_picker_target: None,
                color_picker_anchor: None,
                clip_create_menu: None,
                clip_create_menu_open: false,
                section_menu: None,
                section_menu_open: false,
                section_rename_id: None,
                section_rename_text: String::new(),
                bpm_edit_text: format!("{initial_bpm:.1}"),
                time_sig_num_edit_text: initial_time_sig_num.to_string(),
                clip_edit_buffer_target: None,
                scrub_gesture: None,
                scrub_gesture_seen: false,
                mod_follower_scrub_active: false,
                armed_mod_source: None,
                expanded_mod_sources: std::collections::HashSet::new(),
                clip_text_content_edit_text: String::new(),
                clip_text_font_family_edit_text: String::new(),
                send_picker: None,
            },
        }
    }
}
