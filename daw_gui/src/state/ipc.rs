//! S3b-1: AppData state group (IpcState)。 docs/plan_arch_refactor.md §7.5
//! の分割表に従って app.rs の AppData から機械移送したフィールド群。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::plugin_db::PluginDatabase;
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{
    LoadedSlotInfo, PendingClipFxBounce, PendingStateRequest, PendingVocalSynthBounce,
};
use crate::dispatcher::BackgroundDispatcher;

pub struct IpcState {
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
    /// cache。 `(track_id, slot, param_id) -> plain value`。 audio bridge tick
    /// で `current_plain_value(PluginParam)` がここから plain 値を引いて
    /// `AutomationPoint` を生成する。 session-only / Undo 対象外。 plugin
    /// reload で古い entry が残るが、 lane.target も同 plugin_id を持つので
    /// stale 値が誤って record されるリスクは低い (= 念のため Step C-3
    /// follow-up で plugin unload 時に該当 entry を消す)。
    pub plugin_param_values: std::collections::HashMap<
        (u32, u32, u32),
        f64,
    >,
    /// Phase 2 (`docs/plan_automation.md` §7.5): plugin parameter
    /// 一覧キャッシュ。 plugin host が `PluginParamList` IPC で送って
    /// くるたびに上書き。 `(track_id, slot)` で identify、 Parameter
    /// Picker (Phase 3+) / lane の label 解決 / norm↔plain 変換に
    /// 使う。 session-only (save 対象外、 plugin reload で再取得)。
    pub plugin_params: std::collections::HashMap<
        (u32, u32),
        Vec<common::protocol::PluginParamInfo>,
    >,
    /// `(track_id, slot)` ごとに plugin が埋め込み GUI (editor window)
    /// を持つか (`PluginParamList` で host が `gui_is_embed_supported` を通知)。
    /// チェーン行のボタン分岐に使う: GUI あり = 「GUI」 で window を開く、 なし =
    /// 「⚙」 でインライン param パネルをトグル。 plugin_params と同じ寿命・同じ箇所
    /// (insert / reorder / remove / clear) で維持する。
    pub slot_has_gui: std::collections::HashMap<(u32, u32), bool>,
    /// `track_id → 現在ロード済の device_id 列` (v29: 安定
    /// `PluginInstance::id`)。 plugin_host から `SlotPluginLoaded` を
    /// 受信したときに register、 `SlotPluginUnloaded` で drain。
    /// `RemoveTrack` を plugin_host に送る前に audio engine
    /// に直接 `ClosePluginShmem` を発射して plugin_refs / device 表
    /// を空にし、 plugin destroy 中の use-after-free (`pd.prepare()` で
    /// unmapped shmem を踏む → audio worker が AV で silent terminate
    /// → all_done 永久 wait) を防ぐ。 「host に実際に載った device」 を
    /// 保持するための session-only bookkeeping。
    pub track_plugin_ids: std::collections::HashMap<u32, Vec<u64>>,
    /// `(track_id, device_index)` → 現在 plugin_host に load されている
    /// plugin の情報。 Undo/Redo の reconcile (`reconcile_plugins_with_song`)
    /// で「Song の各 device の plugin が host 側と一致しているか」 を device
    /// 粒度で diff するために使う。 [`Self::track_plugin_ids`] が track
    /// 単位の plugin_id 集合だけを持つのに対し、 こちらは device ごとの
    /// 詳細 (どの index にどの plugin string id) まで track する。
    ///
    /// 更新タイミング: `SlotPluginLoaded` 受信時に insert、
    /// `SlotPluginUnloaded` 受信時に reverse-lookup retain、
    /// 削除系編集の `_inner` 関数内で track / device index 単位で remove。
    pub loaded_slots: std::collections::HashMap<(u32, u32), LoadedSlotInfo>,
    /// PR3.3 PDC: `device_id → reported latency samples`。 plugin_host から
    /// `PluginEvent::PluginLatencyChanged` を受信して更新、
    /// `SlotPluginUnloaded` で drop。 各 track の累積 latency は
    /// `track_plugin_ids[track_id].iter().map(|pid| plugin_latencies[pid]).sum()`
    /// で計算して `Track::reported_latency_samples` に書く。 これが
    /// `LoadSong` で daw_audio に渡って `compile_schedule` の PDC 補償に
    /// 反映される (chain 内の plugin が直列に latency を加算する Ardour 流)。
    pub plugin_latencies: std::collections::HashMap<u64, u32>,

    // -------- Resource monitor (r.md #3) --------
    /// 集計済みリソース指標 (DSP load / system CPU / fps / xrun / mem)。 poller
    /// (DSP/xrun/buffer) / sysinfo スレッド (CPU/mem) / runner (fps) が別々に
    /// 更新し、 status bar 常駐メーターと詳細パネルが読む。
    pub metrics: common::metrics_bridge::ResourceMetrics,
    /// MetricsBridge ハンドル (per-plugin CPU の直接読み出し用)。 GUI mode のみ
    /// `Some`、 script / test は `None`。 詳細パネルが `track_plugin_ids` の
    /// 各 plugin_id について `plugin_dsp_us` を直接読む。
    pub metrics_bridge: Option<Arc<common::metrics_bridge::MetricsBridgeHandle>>,

    // -------- Plugin database / picker --------
    pub plugin_db: Option<Arc<PluginDatabase>>,

    // -------- Save flow / IPC --------
    /// `RequestAllStates` を発行した順に保持するキュー。 front が現在 in-flight
    /// の request、 後続は先行 request の応答後に順次 dispatch される。 空の
    /// 間は新規 request を発行するときに即時 `RequestAllStates` を送る。
    /// 詳細は [`PendingStateRequest`] / [`DeferredEdit`]。
    pub pending_state_queue: VecDeque<PendingStateRequest>,
    /// いま in-flight な `RequestAllStates` (plugin-state round-trip) を
    /// 送った時刻。 `dispatch_front_state_request` が送信の瞬間に `Some(now)` を立て、
    /// `on_all_states_from_child` で応答が来たら `None` に戻す (後続 request が
    /// あれば dispatch が再武装する)。 plugin host が crash でなく **hang** した
    /// (プロセス・パイプは生存のまま `state_save` 等で停止) 場合は
    /// `ChildDisconnected` も発火せず `AllStatesReceived` が永久に来ないので、
    /// `pending_state_queue` が drain せず保存 / New / Open / Open Recent / 終了(✕)
    /// が恒久ロックする (#63 のダーティーガードが round-trip 完了を待つため)。
    /// `on_tick` の watchdog がこの時刻からの無応答経過を見て round-trip を破棄し、
    /// 脱出口を作る (export watchdog と同型)。 `None` = round-trip 非進行。
    pub(crate) state_request_sent_at: Option<std::time::Instant>,
    pub audio_tx: Option<UnboundedSender<AudioCommand>>,
    pub plugin_tx: Option<UnboundedSender<PluginCommand>>,
    /// 子プロセス自動再起動 supervisor (`bootstrap::ChildSupervisor`)。
    /// production (GUI mode) では `Some`、 script / test 経路では `None`。
    /// `ChildDisconnected` event 受信時に `respawn(kind)` で新 child を
    /// spawn + handshake + Session/OpenWorkerPool 再送し、 新 tx で
    /// `audio_tx` / `plugin_tx` を差し替える。
    pub supervisor: Option<Arc<crate::bootstrap::ChildSupervisor>>,
    /// (A1 r.md #8) オーディオセッションの実サンプルレート (= daw_audio が報告した
    /// デバイス実レート、 `bootstrap.sample_rate`)。 拍↔sample 変換 (seek / export
    /// range / clip 尺) はこの値を使い、 engine と一致させる。 session-only。
    pub sample_rate: u32,
    /// 直近の child 切断時刻 (kind 別)。短時間に閾値以上切断したら crash-loop と
    /// 判断して自動 respawn を止める (= 落ちるプラグインを抱えたプロジェクトで
    /// respawn→reload→再 crash の無限ループに陥り GUI が固まるのを防ぐ)。session-only。
    pub child_disconnect_log: Vec<(common::protocol::ChildKind, std::time::Instant)>,

    /// Phase 2 PR-C: plugin-FX bounce が進行中なら `Some`。 `None` で
    /// 新規 bounce を受け付ける。 同時 1 件のみ。 `AudioCommand::
    /// BounceClipFxOnline` 発火時に `Some` 化、 `AudioEvent::
    /// BounceClipFxComplete` 受信で `None` に戻す + 新 track / 新 clip
    /// 配置。 path / source_track / source_clip は IPC echo back と
    /// pending entry を identifier 照合するために保持。
    pub pending_clip_fx_bounce: Option<PendingClipFxBounce>,
    /// 歌唱クリップ bounce の合成待ち。`PrepareVocalSynth` を送って
    /// `VocalSynthReady` を待つ間 stable id を退避し、 ready 受信で現在位置へ
    /// 解決して `start_clip_bounce` を呼ぶ。歌唱以外の bounce では使わない。
    pub pending_vocal_synth_bounce: Option<PendingVocalSynthBounce>,
    /// which (track, slot) plugin editors are currently open. The
    /// editor *windows* are now created and owned by the plugin-host process
    /// (so JUCE cascade sub-menus work); daw_gui only tracks open/closed
    /// state here for toggle / dedup / cleanup. Not `#[cfg(windows)]` because
    /// it's a plain id set — the window FFI lives in the plugin-host process.
    pub open_plugin_guis: std::collections::HashSet<(u32, u32)>,

    // -------- Plugin load tracking (A7 race-condition fix) -----------
    /// v29: `device_id → 要求 generation`。 `SetSlotPlugin` を送ったが
    /// `SlotPluginLoaded` / `SlotPluginLoadFailed` がまだの device 集合。
    /// While non-empty, Play is queued so the audio engine doesn't
    /// dispatch silent buffers for tracks whose plugins are still being
    /// loaded. 応答は entry の generation と一致するものだけ受理する
    /// (A→B 連続差し替えの stale 応答 race 対策、
    /// `docs/plan_arch_refactor.md` §7 世代 guard)。
    pub pending_plugin_loads: std::collections::HashMap<u64, u64>,
    /// v29: `SetSlotPlugin` の要求世代 counter (AppData-wide 単調増加 =
    /// per-device 単調増加を含意)。 送信ごとに bump して
    /// `pending_plugin_loads` へ記録する。
    pub(crate) next_plugin_load_generation: u64,
    /// ユーザーが plugin picker で手動追加した plugin の集合 (値 = GUI 自動 open
    /// するか)。load 完了 (`on_plugin_loaded_from_child`) で consume し、(1) daw_audio
    /// へ `LoadSong` を再送して新 plugin を signal path に入れ (Shift 追加でも必須)、
    /// (2) 値が true なら GUI 自動 open を `gui_open_requests` に queue する。
    /// `select_plugin_from_db` で (track_id, device_index) を積む。
    /// プロジェクト読込時の一斉復元では積まれない (= project-open 時の初回 LoadSong が
    /// 全 chain を渡すので per-plugin の再 sync は不要、 GUI も自動 open しない)。
    pub(crate) pending_added_plugin_finalize: std::collections::HashMap<(u32, u32), bool>,
    /// load 完了して「いま開く」段になった GUI auto-open 要求の queue。runner の
    /// frame loop が `drain_pending_gui_opens` で消費し `open_slot_gui` を呼ぶ。
    /// handle_event (IPC 受信) から直接 window を作らず frame loop へ 1 フレーム
    /// 遅延させる seam (headless test は frame loop を回さない → window を作らない)。
    pub(crate) gui_open_requests: Vec<(u32, u32)>,

    // -------- Background workers --------
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    pub is_rescanning: bool,
    /// H2 (r.md #8): MIDI-CC → plugin param 等、 1 frame に多数届き得る realtime
    /// edit の plugin-host 再 sync を **1 frame 1 回に coalesce** するフラグ。 handler
    /// が立て、 runner が frame ごとに `flush_pending_host_sync` で消費する
    /// (毎 CC の full LoadSong flood = stutter/dropout を防ぐ)。
    pub pending_host_sync: bool,

    /// 背景スレッド (autosave / playhead poll / MIDI / IPC bridge / VOICEVOX
    /// 合成 / plugin DB rescan) からメインスレッドへ `AppEvent` を送るための
    /// dispatcher。 production は `WinitDispatcher` (winit `EventLoopProxy`
    /// ラップ)、 test は `RecordingDispatcher` (Mutex<Vec> に蓄積)。
    pub event_proxy: Arc<dyn BackgroundDispatcher>,
}
