# r.md #129 — プロジェクトタブ (REAPER 流の複数プロジェクト同時オープン)

1 つの daw_gui 窓で複数のプロジェクトをタブで開き、各タブが独立した transport を持ち、
背景タブの音も同じ出力デバイスへミックスされる (REAPER の「Run background projects」)。

一次情報:

- REAPER User Guide v7.79 §2.29 Project Tabs (p54-55): タブ帯は 2 つ以上で表示 / 新規タブは
  "Unsaved" / 右クリックメニュー (New project tab / Close project / Close all projects but current /
  Close all projects / Run background projects / Hide background project FX/MIDI windows ...) /
  ドラッグ並べ替え / タブ末端の ✕ / タブ間 copy-paste は通常の Ctrl+C → タブ切替 → Ctrl+V。
- REAPER whatsnew v3.11 (background projects を current と一緒に再生 + start 同期)、v7.23
  (タブに play state indicator、背景の曲末停止で前景を止めない)、v4.7 (終了時に未保存タブ一覧)。
- ReaScript API: undo / edit cursor / play state / time selection / arrange view はすべて
  `ReaProject*` 引数付き = **per-project**。

## 0. grill で確定した設計判断

| #  | 決定 |
|----|------|
| Q1 | タブを切り替えても元のタブの再生は**続き、音は同じ出力へミックス**される。エンジンは 1 デバイスに全プロジェクトを載せる |
| Q2 | タブ帯はメニューバー直下・トランスポートの上。**タブが 2 つ以上のときだけ表示** (REAPER 既定) |
| Q3 | タブ 1 枚 = ファイル名 (未保存は `Untitled`) + 未保存変更の `*` + 再生中の ▶ + 右端の ✕。ホバーでフルパスの tooltip |
| Q4 | `File > Open` / 最近使ったファイルは**常に新しいタブ**に開く。ただし現在のタブが「何も触っていない空の Untitled」ならそのタブを置き換える |
| Q5 | 最後の 1 タブを閉じると空の Untitled に戻る (アプリは終了しない)。タブ帯は消える |
| Q6 | 背景タブのプラグインエディタ窓は**開いたまま残す**。どのタブの窓かはタイトルで区別 |
| Q7 | `Ctrl+N` = 新規プロジェクトを新タブに / `Ctrl+Tab` `Ctrl+Shift+Tab` = 次 / 前 / `Ctrl+W` = 現在のタブを閉じる。`Ctrl+Shift+W` (全プラグイン窓を閉じる) は据え置き |
| Q8 | ドラッグで並べ替え + 右クリックメニュー (新規タブ / 閉じる / 他のタブを全部閉じる / 全部閉じる) |
| Q9 | 終了時はタブごとに順番に保存確認。次回起動は今までどおり空プロジェクト 1 つ (タブ復元なし) |
| Q10 | **タブをまたぐ D&D に対応**: クリップ / トラックのドラッグ中にタブへ重ねる (0.5 秒) か `Ctrl+Tab` / `Ctrl+Shift+Tab` を押すとタブが切り替わり、そのままアレンジへ落とせる。落としたものは独立コピー (元のタブからは消えない) |

質問せずに決めたもの (REAPER の挙動と既存設計から導出):

- **transport / Play / Stop / 録音 / ループ / メトロノーム / 書き出しはアクティブなタブに効く。**
  背景タブは自分の状態のまま走り続ける。Panic (全停止 + フェード) だけはデバイス全体。
- **編集 / undo / 選択 / ズーム / スクロール / インスペクタ / ピアノロール / ランチャー状態は
  すべて per-project** (ReaScript の `ReaProject*` と同じ粒度)。
- **マスターメーター / スコープ / ラウドネス / Global Sampler の Master ソースは「アクティブなタブの
  master」** (ScopeBridge と同じ点、metronome 前)。タブ切替でメーター表示も切り替わる。
- **書き出し中はエンジン全体が無音** (既存の export gate と同じ)。背景タブの再生位置は
  書き出しの間だけ止まり、終わると続きから走る。REAPER の render もモーダル。
- タブ間の copy / paste は既存の clipboard envelope (`source_project_id` で link / copy を判定) が
  そのまま「別プロジェクトからの貼り付け = 独立コピー」になる。追加実装なし。
- 自動保存 / 復旧候補はタブごと (`SongDoc::recovery_session_id` が既に per-doc)。
- 同時に開けるタブは **32** (`MAX_PROJECTS`。`MAX_TRACKS` / `MAX_WORKERS` と同じ cap 思想)。

## 1. 識別子 — 3 プロセスを貫く住所

### 1.1 `ProjectKey` (wire 型、`common/src/protocol.rs`)

```rust
/// 開いているプロジェクト (= タブ) の住所。daw_gui が採番する単調増加の u64、
/// **プロセス生存中に再利用しない**。0 = 未割当 sentinel。
/// `Song::project_id` (ファイルに永続化されるランダム u64) とは別物 —
/// 同じファイルを 2 タブで開いても ProjectKey は別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ProjectKey(pub u64);
```

### 1.2 `DeviceAddr` (wire 型)

```rust
/// プラグインインスタンスの住所。device_id は Song 内でしか一意でない (project ごとに 1 から
/// 採番) ので、プロセス境界では必ず ProjectKey と組で運ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub struct DeviceAddr { pub project: ProjectKey, pub device_id: u64 }
```

### 1.3 `InstanceToken` (wire 型) — worker dispatch と metrics slot の鍵

daw_plugin_host が `SetSlotPlugin` ごとに採番する **プロセス生存中に一意な u64** (既存の
`next_shmem_incarnation` をそのまま昇格)。`SlotPluginLoaded.token` で daw_gui へ返り、
daw_gui が `OpenPluginShmem { .., token }` で daw_audio へ渡す。

- `WorkerBridge.worker_task[i]` が運ぶ値は device_id ではなく **token** になる
  (`WorkerSyncRef::dispatch(token, ..)`、`PluginRef.token`)。plugin_host の worker registry は
  `HashMap<InstanceToken, PluginEntry>`。
- `MetricsBridge.plugin_metrics[].device_id` → `token` (per-plugin μs の鍵)。daw_gui は
  `LoadedDeviceInfo.token` で引く。
- shmem 名 `process_data_shmem_id(pid, device_id, incarnation)` は incarnation = token のまま。

## 2. Protocol (`common/src/protocol.rs`)

### 2.1 AudioCommand

| 区分 | variant | 変更 |
|---|---|---|
| **新設** | `OpenProject { project }` | project slot 生成 (ProjectShared + ProjectRt を off-thread で構築、telemetry slot を確保) |
| **新設** | `CloseProject { project }` | slot 撤去 (RT から外し off-thread で drop、telemetry slot 解放) |
| **新設** | `SetScopeProject { project }` | ScopeBridge / Global Sampler Master が書く master をこの project にする |
| project 付与 | `LoadSong { project, song }`, `Play { project }`, `PlayContinue { project }`, `Stop { project }`, `SeekTo { project, samples }`, `SetLoop { project, region }`, `SetMasterGain { project, gain }`, `SetDeviceLatency { project, device_id, samples }`, `SetProjectDir { project, dir }`, `SetRecordingLanes { project, lanes }`, `SetMetronomeEnabled { project, enabled }`, `PreviewNoteOn/Off { project, .. }`, `StartRecording { project, preroll_samples }`, `StopRecording { project }`, `OpenPluginShmem { project, device_id, shmem_id, token }`, `ClosePluginShmem { project, device_id }`, `ExportWav { project, .. }`, `AnalyzeLoudness { project, range }`, `BounceClipFxOnline { project, .. }`, track / send / chain / parallel / strip / bpm / time-sig の値更新すべて `{ project, .. }`, ランチャー 8 種 `{ project, .. }`, `PreviewSequence { project, .. }`, `PreviewSequenceStop { project }`, `OpenSamplerRing { shmem_id, source }` の `SamplerSource::Track { project, tap }` | |
| 据え置き (デバイス全体) | `Ack`, `Session`, `Panic`, `PanicRelease`, `SetAppActive`, `OpenWorkerPool`, `CloseWorkerPool`, `SamplerPreview`, `SamplerPreviewStop`, `CancelExport`, `Shutdown` | |

### 2.2 AudioEvent

`PluginUnresponsive { project, device_id }`, `ExportWavComplete { project, .. }`,
`ExportWavProgress { project, .. }`, `LoudnessAnalysisProgress { project, .. }`,
`LoudnessAnalysisComplete { project, .. }`, `BounceClipFxComplete { project, .. }`。
`Hello` / `ChildDisconnected` / `WorkerPoolStalled` は据え置き。

### 2.3 PluginCommand

| 区分 | variant |
|---|---|
| **新設** | `UnloadProject { project }` (旧 `UnloadAllPlugins` の置き換え。タブを閉じる / respawn 後の再構築で使う) |
| device 住所化 | `device_id: u64` を持つ variant はすべて `device: DeviceAddr` に置換: `SetBuiltinPluginNoteMetadata`, `PrepareVocalSynth`, `SetVocalSynthPriority`, `SetSlotPlugin`, `RemoveSlotPlugin`, `RequestSlotState`, `OpenSlotGuiEmbedded`, `CloseSlotGui`, `SetEditorSendAllKeys`, `SetupAraDocument`, `ClearAraDocument`, `UpdateAraRegions` |
| project 付与 | `SetProjectDir { project, dir }` (**今は送り手が居ない**。daw_gui の flush_song_sync から送るように直す)、`RequestAllStates { project }`, `ReinitAllPlugins { project: Option<ProjectKey> }` (None = 全 project、Panic 用 / Some = 書き出しクリーンスタート) |
| 据え置き | `Ack`, `Session`, `SetRenderMode`, `CloseAllSlotGuis`, `SetEditorForwardedKeys`, `OpenWorkerPool`, `CloseWorkerPool`, `Shutdown` |

`UnloadAllPlugins` は削除する (project 単位の撤去で置き換わる。「全部消す」は respawn 時だけで、
それは子プロセス自体が新しいので不要)。

### 2.4 PluginEvent

`device_id: u64` を持つ variant はすべて `device: DeviceAddr` へ (`EditorKey`, `VocalSynthReady`,
`SlotPluginLoaded` (+ `token: InstanceToken` 追加), `SlotPluginLoadFailed`, `SlotPluginState`,
`SlotGuiGeometry`, `SlotGuiClosed`, `SlotPluginShmemReleased`, `SlotPluginUnloaded`,
`PluginLatencyChanged`, `PluginParamList`, `PluginParamTouched/ValueChanged/GestureEnd`,
`VoicevoxSynthStatus`)。`AllPluginStates { project, entries }`、`PluginsReinitDone { project: Option<ProjectKey> }`。
`SlotState` は device_id のまま (project は外側が持つ)。

### 2.5 AudioBridge (`common/src/audio_bridge.rs`)

```rust
pub const MAX_PROJECTS: usize = 32;

#[repr(C)]
pub struct ProjectTelemetry {
    /// `ProjectKey.0`。0 = 空きスロット。writer = daw_audio (OpenProject で claim / CloseProject で解放)。
    pub project_key: AtomicU64,
    pub playhead_samples: AtomicU64,
    pub playing: AtomicU32,
    pub recording_live: AtomicU32,
    pub preroll_remaining_samples: AtomicU64,
    pub track_peaks: [[AtomicU32; 2]; MAX_TRACKS],
    pub track_gr_db: [AtomicU32; MAX_TRACKS],
    pub master_gr_db: [AtomicU32; 2],
    pub mod_scalars / mod_slot_ids / mod_plane_generation,
    pub launcher_rows: [LauncherRowState; MAX_LAUNCHER_ROWS],
    pub voice_* ...,
}

#[repr(C)]
pub struct AudioBridge {
    pub projects: [ProjectTelemetry; MAX_PROJECTS],
}
```

GUI 側 reader は `project_key` を線形走査して自分の slot を見つける (≤32、tick ごと)。
既存の accessor (`set_playhead_samples` 等) は `ProjectTelemetry` のメソッドに移る。

### 2.6 MetricsBridge

`PluginMetricSlot { device_id }` → `{ token: AtomicU64 }`。`claim_plugin_metric_slot(token)` /
`plugin_dsp_us(token)` / `reclaim_plugin_metric_slots(live: &HashSet<u64 /*token*/>)`。

### 2.7 fingerprint

`common/build.rs` の `WIRE_SOURCES` は protocol.rs / audio_bridge.rs / metrics_bridge.rs /
worker_bridge.rs / plugin_ref.rs を既に含む。新ファイルを切らない限り追加不要。

## 3. daw_audio — 複数プロジェクトを 1 デバイスに載せる

### 3.1 状態の 2 層化

| 今 | 後 |
|---|---|
| `SharedState` + `EngineShared` (1 つ) | **`DeviceShared`** (1 つ): `panic_declick`, `panic_release`, `app_active`, `idle_silent_samples`, `park_requested`, `worker`, `sampler`, `export_running`, `export_cancel`, `live_parked`, `last_buffer_frames`, `mmcss_*`, **`scope_project: AtomicU64`** |
| | **`ProjectShared`** (project ごと、`Arc`): `key`, `song: ArcSwapOption<Song>`, `playback`, `loop_region`, `playhead`, `pending_seek`, `recording_lanes`, `metronome_enabled`, `plugin_refs: ArcSwap<PluginRefs>`, `preview_sequence`, `audio_clip_renderer`, `schedule_generation`, `last_published_generation`, `project_dir`, `preroll_total/remaining`, `recording_requested`, `master_gain`, `device_latencies`, `delivered_engines_per_track`, `loaded_project_id` (同じ slot で Song::project_id が変わった = Untitled タブへ Open した、の検出), stretch pool ring 対, bundle ring 対 (forward / recycle) |
| `LocalState` (1 つ) | **`DeviceRt`**: `master_l/r` (デバイス最終ミックス), `projects: Vec<Box<ProjectRt>>` (**capacity = MAX_PROJECTS を事前確保**、push/swap_remove は再確保しない), `project_rx / project_recycle_tx` (rtrb、`ProjectDelivery::{Open(Box<ProjectRt>), Close(ProjectKey)}`), `worker`, `sampler`, `sampler_rt`, `frames_rendered`, `cmd_rx`, `shared: Arc<DeviceShared>` |
| | **`ProjectRt`**: `key`, `shared: Arc<ProjectShared>`, `telemetry: usize` (AudioBridge slot index), `scratch: Vec<TrackScratch>` (MAX_TRACKS), `bus_l/r` (この project の master 出力), `master_strip`, `playing`, `playhead_beats`, `last_known_playhead`, `metronome_voice`, `cached_schedule`, `cached_song`, `tempo_map`, `plugin_refs`, `preview_sequence`, `launcher: LauncherRuntime`, `mod_tick`, `follower_cols`, `follower_env_of_slot`, `bundle_rx / bundle_recycle_tx`, `stretch_pool_rx / recycle_tx` |

`BundlePublisher` は project ごと (recv loop 側
`HashMap<ProjectKey, ProjectCtl { shared: Arc<ProjectShared>, publisher, .. }>`)。

**`ProjectRt.scratch` は曲が要る本数だけ持つ。** `TrackScratch` は 1 本 ~450 KB
(大半は PDC / sidechain 用の 1 秒 DelayLine) なので、`MAX_TRACKS` (32) を無条件に確保すると
**タブ 1 枚あたり ~14 MB** を、使うかどうかに関わらず先に取る (32 タブで ~450 MB)。
`RtBundle` に `scratch_growth: Option<Vec<TrackScratch>>` を足し、`song` と **同じ便で**
`min(tracks.len(), MAX_TRACKS)` 本を off-thread で確保して運ぶ (別便にすると song だけ
先に着いた buffer が無音になる)。RT 側は既存の行を要素ごと swap で新しい Vec へ移して
(move だけ = 確保も解放もしない、走行状態を保つ)、押し出した古い Vec を bundle に載せ替えて
recycle ring へ返す (drop は off-thread)。増える方向にだけ動かす。

**stretch engine の配送 (`StretchPoolDelivery`) は「行がまだ無い」だけで捨てない** —
peek して `track_idx >= scratch.len()` なら ring に残す。pop して捨てると publish 側の
「配送済み」(`delivered_engines_per_track`) だけが進み、その track のストレッチが二度と揃わない。

### 3.2 CPAL callback / `process_buffer` の順序

1. `pump_commands()` — `EngineCommand` は `project` を持ち、`projects` を線形走査 (≤32) して配る。
2. `project_rx` を drain: `Open` → `projects.push` (容量内)、`Close(key)` → `swap_remove` して
   `project_recycle_tx` へ (off-thread drop)、telemetry slot の `project_key` を 0 に戻す。
3. デバイス `master_l/r[..n]` を zero。
4. **export gate**: `export_running` なら `live_parked = true` + 全 project の launcher request を
   捨てて無音 return (今と同じ、全 project が止まる)。
5. project ごとに (順序は `projects` の並び。ミックスは加算なので順序不問):
   `refresh_bundle` → `refresh_stretch_pools` → `consume_transport_requests` → telemetry
   (`playing` / `recording_live` / `preroll`) → count-in gate (この project だけ metronome、
   他は通常描画) → seek 検出 → mod tick → launcher update → `render_master_buffer` を
   **`bus_l/r` へ** → `scope_project == key` なら `scope.write_block(bus)` + sampler Master 書き込み
   → metronome overlay を `bus` へ → per-track telemetry → transport advance →
   `launcher.publish` → **`master_l/r += bus_l/r`**。
6. デバイス側: declick / interleave / peaks / DSP load / idle park (`playing` はどれか 1 つでも
   走っていれば true、`preroll` も同様)。

`render_master_buffer` の引数は既に「1 buffer を描くのに要る全部」なので、`ProjectRt` の
フィールドを渡すだけ。**live / export 共通の不変条件 6 は不変。**

worker pool `dispatch_and_wait` は project ごとに直列に呼ぶ (`DispatchShared` は 1 project 分の
ポインタ束なので、呼び出し 1 回 = 1 project)。

### 3.3 recv loop

- `OpenProject` → `ProjectShared::new` + `ProjectRt::new` (scratch 等の確保は全部ここ、off-thread) →
  AudioBridge の空き slot を claim (`project_key` CAS) → `project_tx.push(Open(..))`。
  空き slot が無い / MAX_PROJECTS 超過は `warn!` + 無視 (GUI 側が先に cap を守る)。
- `CloseProject` → `project_tx.push(Close(key))` → recycle から戻った `ProjectRt` を drop
  (shmem unmap / thread join は今と同じ off-thread)。`ProjectCtl` を map から外す。
- project 付き command は `projects.get(&key)` で `ProjectShared` を引いて今と同じ処理。
  未知の key は `debug!` で捨てる (閉じたタブへの遅延メッセージ)。
- `LoadSong { project, song }`: `loaded_project_id` の比較で `reset_song_scoped_state` を決める
  (今と同じ判定を slot 単位に)。
- `ExportWav { project, .. }` / `AnalyzeLoudness` / `BounceClipFxOnline`: エンジン全体の
  `export_running` 予約は今のまま、描画する材料 (song / plugin_refs / renderer /
  device_latencies / project_dir) は `Arc<ProjectShared>` から取る。完了 event に `project` を載せる。
- `SetScopeProject` → `DeviceShared.scope_project.store`。
- decode thread の `DecodeJob` は `Arc<ProjectShared>` を持ち、その project の
  `audio_clip_renderer` へ publish。
- `ModPhaseTableBuilder` (1 スレッド) は request に `ProjectKey` を付け、結果を該当 project の
  publisher へ渡す。
- housekeeping は全 `ProjectCtl` を回す。
- 通知スレッド (`PluginUnresponsive` / `WorkerPoolStalled`) は全 project の `plugin_refs` を走査。

### 3.4 Global Sampler

`SamplerSource::Master` = `scope_project` の bus (metronome 前)。`SamplerSource::Track { project, tap }`
= その project の track tap。`SamplerRig` は 1 つのまま。

### 3.5 テスト (`daw_audio`)

- 2 project を Open して片方だけ Play → もう片方の playhead が動かない / 出力はミックス。
- Close した project の bus がミックスから消え、telemetry slot が解放される。
- `export_running` 中は全 project の playhead が止まり、解除で続きから進む。
- RT 経路の `rt-assert` (allocator hook) を 2 project で通す。
- 既存の engine / graph / launcher テストは `ProjectRt` 1 つの形へ機械的に移す。

## 4. daw_plugin_host — `DeviceAddr` で帳簿を引く

- `PluginHost.instances: HashMap<DeviceAddr, InstanceRecord>`、`InstanceRecord.token: InstanceToken`。
- `PluginHost.projects: HashMap<ProjectKey, ProjectCtx { project_dir: Option<PathBuf> }>`
  (`SetProjectDir { project, dir }` で upsert、`UnloadProject` で削除)。
- `registry: HashMap<InstanceToken, PluginEntry>`。`run_worker` は `worker_task[idx]` の値を
  token として引く。
- `set_slot_plugin` の dedup (`requested_id == plugin_id` なら再ロードしない) は `DeviceAddr`
  単位でそのまま正しい。
- `UnloadProject { project }` = その project の全 instance を teardown (editor 窓を閉じ、ARA を
  clear、shmem を release、`SlotPluginUnloaded { device }` を返す) + `ProjectCtx` 削除。
- `ReinitAllPlugins { project }` = None なら全 instance、Some なら該当 project だけ。
  `PluginsReinitDone { project }` を echo。
- `RequestAllStates { project }` → `AllPluginStates { project, entries }` (該当 project だけ)。
- editor_keys router: `editors: Vec<(DeviceAddr, hwnd)>`、`send_all_keys: HashSet<DeviceAddr>`。
  `EditorKey { device: DeviceAddr, chord }`。
- `HostCallbacks` / `StatusFn` / `HostNotify` の device_id capture は `DeviceAddr` capture へ。
- metrics slot は token で claim。
- テスト: 同じ device_id を 2 project で `SetSlotPlugin` → instance が 2 つ / `UnloadProject`
  は片方だけ消す / worker が token で正しい instance を引く。

## 5. daw_gui — タブと ProjectState

### 5.1 AppData の再構成

```rust
pub struct AppData {
    pub tabs: Tabs,                 // 全 project + アクティブ
    // 以下はアプリ全体 (1 つ):
    pub ipc: IpcState,              // audio_tx / plugin_tx / supervisor / plugin_db / metrics /
                                    // sample_rate / child_disconnect_log / rescan / event_proxy /
                                    // pending_state_queue (1 in-flight、project 付き)
    pub voicevox: VoicevoxState,    // singers / talk_speakers / spawned_engine / launch_attempted / job
    pub ui_prefs: UiPrefs,          // app_config.json 由来 + recent + app_dirs + preview_window_visible
    pub ui_ephemeral: UiEphemeral,  // theme 一覧 / plugin picker / font picker / status / clipboard /
                                    // dirty_guard 一式 / recovery modal / export dialog flags / hwnd
    pub export: ExportState,        // 旧 transport.export_* + ipc.pending_*_bounce/glue/export
                                    // を 1 か所に (エンジンは同時に 1 本しか描かない) + `project: ProjectKey`
    pub activity, shutdown, loudness (→ ProjectState へ), meter_control, theme, sampler,
    pub midi_capture, virtual_keyboard,
}

pub struct Tabs {
    order: Vec<ProjectState>,   // タブの並び = 表示順
    active: ProjectKey,         // UI が見せている / 操作が向くタブ
    next_key: u64,              // 1 から単調増加
}

pub struct ProjectState {
    pub key: ProjectKey,
    pub song_doc: SongDoc,
    pub transport: TransportState,      // export_* を抜いた残り (playhead / home / loop / meters / mod_plane / voices / pending_play)
    pub selection: SelectionState,
    pub ipc: ProjectIpc,                // ara_doc_cache / ara_pcm_materialized / plugin_param_values / plugin_params /
                                        // slot_has_gui / loaded_devices (token 込み) / pending_plugin_loads /
                                        // next_plugin_load_generation / failed_plugin_loads /
                                        // pending_added_plugin_finalize / gui_open_requests / open_plugin_guis /
                                        // pending_vocal_synth_export / last_synced_epoch
    pub voicevox: ProjectVoicevox,      // lipsync_gen / lipsync_inflight / lipsync_fingerprints /
                                        // voicevox_synth_status / voicevox_metadata_sent / priority_sent
    pub media: MediaState,
    pub recording: RecordingState,      // midi_input_label は UiPrefs へ
    pub view: ProjectView,              // 旧 UiPrefs の ViewState 相当 + session-only song-scoped
    pub eph: ProjectEphemeral,          // 旧 UiEphemeral の Song-scoped 部分 (texture cache / hover / rename / scrub ...)
    pub launcher: LauncherUiState,
    pub loudness: LoudnessState,
}
```

**アクセサ**: `app.proj()` / `app.proj_mut()` = `tabs.active` の `ProjectState`。
`app.tabs.get(key)` / `get_mut(key)` = 任意のタブ。**`self.song_doc` 等の直アクセスは全部
`self.proj().song_doc` に書き換える** (view の `app.song_doc` も同じ)。

**IPC event の宛先**: `dispatch_audio_event` / `dispatch_plugin_event` は event の `project` /
`device.project` で `tabs.get_mut(key)` を引き、無ければ `debug!` で捨てる (閉じたタブ)。
handler が「アクティブなタブ」前提で書かれている箇所は、**`Tabs::with_target(key, |app| ..)`**
(その間だけ `proj()` が key を返し、抜けると `active` に戻る RAII スコープ) で背景タブへ向ける。
これは ReaScript の「current project」と同じモデル。view / Edit closure は常に `active` で走る。

**ChildDisconnected / respawn** は全タブを回して `OpenProject` → `restore_plugin_from_song` →
loop region / sampler ring の再送。

### 5.2 タブ操作 (`AppEvent::Tab(TabEvent)`、handler は `handler/tabs.rs`)

| event | 挙動 |
|---|---|
| `New` | `ProjectState::new_untitled(key)` を末尾に追加 → `OpenProject` → active に |
| `Open(path)` / `OpenRecent` | active が pristine (`!dirty && file_path.is_none() && undo 空`) ならそのタブへ `action_open_path`、そうでなければ `New` してから load |
| `Switch(key)` / `Next` / `Prev` | `tabs.active = key` → `SetScopeProject` → title 更新 → `eph.project_generation` bump 相当 (runner の preview cache を捨てる) → master analyzer reset |
| `Close(key)` | dirty なら `DirtyGuardAction::CloseTab(key)` (保存 / 破棄 / キャンセル、対象タブを active にしてから出す) → `UnloadProject` + `CloseProject` → autosave 破棄 → `order` から除去。最後の 1 つなら先に `New` してから閉じる |
| `CloseOthers(key)` / `CloseAll` | `Close` を順に (dirty ごとに確認、キャンセルで中断) |
| `Move { key, to: usize }` | `order` 内で移動 |

`DirtyGuardAction` は `Quit` / `CloseTab(key)` / `CloseTabs(Vec<key>)` だけになる
(`New` / `Open` / `OpenPath` はタブを置き換えないので guard 不要)。`Quit` は dirty なタブを
順番に (Q9) — `CloseTabs(all)` の完了後に `begin_shutdown`。

### 5.3 タブ帯 (`view/tab_strip.rs`)

- `root.rs` のレイアウトに `TAB_H = 26.0` を追加。`tabs.len() >= 2` のときだけ menu と transport の
  間に描く (1 つなら高さ 0)。
- 1 タブ = `[▶ ]名前[*]  ✕`。幅は min 80 / max 200、あふれたら均等に縮めて省略記号。
  active は `Palette` の選択色、hover で `ui.tooltip(full path)`。
- ▶ は `ProjectTelemetry.playing` を tick で写した `transport.is_playing` から。
- クリック = Switch、✕ = Close、空き領域ダブルクリック = New、ドラッグ = Move
  (daw-ui の既存 drag idiom、しきい値 4px)、右クリック = context menu 4 項目。
- shortcuts.rs: `new` の説明を「新しいタブに新規プロジェクト」へ、`daw.tab_next` = `Ctrl+Tab`、
  `daw.tab_prev` = `Ctrl+Shift+Tab`、`daw.tab_close` = `Ctrl+W`。File メニューに
  「新しいタブ」「タブを閉じる」を追加。
- 窓タイトル = active タブ (今の規則)。プラグインエディタ窓のタイトルは
  `"<plugin> — <track> [<project 名>]"` (Q6)。

### 5.4 その他の per-app 処理

- **frame 末 sync**: `flush_song_sync` は全タブを回す (背景タブも plugin param 変更等で epoch が
  進む)。送る command は `project` 付き。`SetProjectDir` は audio と plugin_host の両方へ。
- **tick**: AudioBridge の全 slot を読み、`project_key` で該当タブへ写す (`is_playing` は
  タブの ▶ にも使う)。メーター / mod plane / launcher rows / voices は active だけ読めばよいが、
  読む位置は slot なので全部読んでも同じコード。
- **autosave**: 全タブ。**session end** の dirty = いずれかのタブが dirty。
- **recovery**: 起動時の候補は Open と同じ規則 (pristine なら置き換え、それ以外は新タブ)。
- **ExportState.project**: 書き出し中はそのタブの編集を拒否 (`export_lock` は `SongDoc` 単位で既存)。
  他のタブは編集可 (エンジンは無音だが Song 編集自体は妨げない)。
- **script (`--script`)**: `daw.newTab()`, `daw.closeTab()`, `daw.switchTab(index)`,
  `daw.tabsJson()` (`[{index, key, path, dirty, playing, active}]`) を追加。既存関数は active に効く。

### 5.5 装置に 1 つしかない状態 (Global Sampler / MIDI 試聴 / スコープ)

`SamplerRig` / `SamplerRt` / `ScopeBridge` は **デバイスに 1 つ** (`DeviceRt` 側) で、
project ごとには増やさない。開いているタブの数だけ毎 buffer 呼ばれるので、**「持ち主の
project の buffer でだけ進める」** を守る:

- `SamplerRt::step_preview_sequence` は走行状態 (`seq_cursor` / `seq_done`) に **持ち主の
  `ProjectKey` を含める**。試聴していないタブの buffer (`seq == None`) で cursor を捨てると、
  毎 buffer 頭からやり直して試聴が鳴り直し続ける。別のタブが鳴らしている間は割り込まない
  (試聴は装置に 1 つ)。
- `SamplerRt::arm_snapshot_flags` は「直前に立てた行」を覚えず、**毎 buffer その project の
  scratch を全部下ろしてから立て直す**。覚える形だと別 project の scratch を下ろしてしまい、
  録音源の行が立ちっぱなしになる。
- **`seq_done` (鳴らし終えた印) は project ごと**の固定長表で持つ。装置に 1 つだと、
  2 つのタブが試聴を終えた状態で互いの印を上書きし合い、両方が延々と鳴り直す。
- **タブを閉じたら RT 側の走行状態も降ろす** (`SamplerRt::forget_project` を
  `ProjectDelivery::Close` で呼ぶ)。降ろさないと持ち主の居ない `seq_cursor` が残り、
  以後どのタブも試聴できない (割り込まない規則なので誰も持ち主になれない)。
- **録音源のタブを閉じたら `SamplerSource::Master` へ戻す** (GUI 側 `close_tab_now`)。
  戻さないと engine は居ない project の buffer を待ち続け、リングに何も書かれない
  (波形も MIDI Capture の時間軸も理由なく止まる)。録音源が **別のタブ** の track のときは、
  サンプラータブの録音源 dropdown にその項目を `[タブ名] トラック名 · tap` で出す
  (出さないと「Master」を指したまま別タブの音を録り続ける)。

### 5.6 タブをまたぐ D&D (Q10)

既存 idiom に乗せる: 下部タブ (サンプラー / MIDI キャプチャ) が `Ui::begin_drag` で持ち出した
payload を `view/capture_drop.rs` がアレンジへ落とす経路 = **「別ペインから来た payload を
アレンジの着地解決に通す」**。タブ間も同じ「別ペイン」扱いにする。

- **payload 型**: `ProjectTransferPayload { envelope: ClipboardEnvelope, grab_beat_offset: f64,
  grab_track_offset: usize }`、札 `PROJECT_XFER_DRAG_KIND`。`envelope` は copy と同じ
  `ClipboardEnvelope` (`Clips` / `Tracks`) を**メモリ上でそのまま持つ** (serialize しないので
  `CLIPBOARD_BLOB_BUDGET` の 4MB 制限は掛からない。プラグイン state の大きいトラックも運べる)。
- **変換のタイミング** (= 元タブ側の内部ドラッグを payload へ昇格させる瞬間):
  1. アレンジのクリップ Move ドラッグ / トラックヘッダのドラッグ中に、ポインタがタブ帯の
     タブ上に **0.5 秒** 留まる (spring-loaded)。
  2. 同じドラッグ中に `Ctrl+Tab` / `Ctrl+Shift+Tab` が押される (`dispatch_shortcuts` は
     ドラッグ中も走る。押している間だけの key grab は不要、単発)。
  どちらも: ドラッグ中の選択 (クリップ集合 / トラック集合) を `ClipboardEnvelope` に写す
  (`copy` と同じ関数) → `ui.begin_drag(PROJECT_XFER_DRAG_KIND, payload)` → 元タブの
  arrangement drag session を cancel (Song は変更しない) → `TabEvent::Switch`。
- **落とす側** (`capture_drop.rs` に arm を足す): `Clips` は pointer の beat / track へ
  (`grab_*_offset` を引いて掴んだ位置関係を保つ)、`Tracks` はヘッダの挿入位置へ。着地処理は
  **既存の cross-project paste** (`source_project_id` 不一致 → 独立コピー) をそのまま呼ぶ。
  ドラッグ中のゴースト表示は既存の drop indicator idiom。
- **行の単位は「見えている行」** (widget が積んだ行の並び)。アレンジ側は
  `ArrangementResponse::rows` (**culling 前**) から「トラック行 + その下に展開している
  レーン行」を 1 本にまとめた帯を組んで解く (= `ArrangementFrame::tops` と同じ区切り)。
  `track_header_rects` は **使わない** — 高さがトラック本体だけで画面外の行も落ちており、
  レーンを展開した行の上でゴーストと別の行に着地する / 最下段のレーンの上で「余白」と
  誤判定して勝手にトラックが増える。帯 (ランチャー) 側は `launcher.row_bands`。payload の
  `track_offset` / `row_offset` は運ぶ前に表示行の差へ翻訳し、落とす側も同じ並びで解いてから
  貼り付け API が使う単位 (`song.tracks` の index / `all_launcher_rows` の index) へ戻す。
  `song.tracks` の index のまま運ぶと、畳んだグループ・master 行・オートメーションレーン行の
  ぶんだけ **ゴーストの行と着地の行がずれる** (畳まれて見えない子トラックに落ちる)。
  置けない行 (master 行 / セルを持てない行) に当たったら **群ごと** 置ける行まで下へ寄せる
  (ゴーストの下限 `.max(min_row)` と同じ規則)。アンカーだけ寄せて各品目を素の base で解くと、
  master 行に当たった 1 個が黙って消えて残りが 1 行ずれる。運ぶ側も同じ向き
  (見えない行のクリップは **捨てる**。`?` で中断するとドラッグ自体が無反応になる)。
- **媒体は貼り先のフォルダ基準へ戻してから取り込む** (`AppData::media_for_import`)。
  写しは絶対パスで運ぶので、そのまま `Song::import_media` へ渡すと同じ音源が
  `ProjectRelative` と `Absolute` の 2 本になる (元のタブへ戻したときに必ず起きる —
  持ち込みは常に独立コピー = `source_project_id = 0` なので取り込みが走る)。
- **一番下の行より下の余白は「新しいトラック」** (Ableton Live と同じ)。落とすと必要な
  本数だけトラックを作ってそこへ置く (ファイル drop の `NewTrackBottom` / 帯の
  `LauncherNewTrack` と同じ約束)。どれだけ下まで引いても増えるのは運んでいる行のぶんだけで
  (掴んだ一番上の行が最初の新しい行に乗る位置で頭打ち)、トラックを足す編集と貼る編集は
  `enter_own_gesture` で **1 undo 手**に束ねる。ゴーストも同じ行に、増える行の下敷き付きで描く。
  **同じプロジェクト内のクリップ移動も同じ規則**にした (`widgets/arrangement/release.rs`
  の Move — 以前は一番下のトラックへ clamp していた)。
- 落とさずに離す / Esc = 何も起きない (host が payload を捨てる。元タブは無変更)。
- 同じタブに戻して落とした場合も**コピー**になる (payload に昇格した時点で「別プロジェクトからの
  持ち込み」に統一。元のクリップは動かない)。
- **retained widget state のキー**: arrangement / piano_roll / launcher / mixer の `stateful` id は
  `ProjectKey` を含める (`("arrangement", key.0)`) — タブごとに hover / drag session / scroll を
  独立させ、切り替えで他タブのドラッグ状態が混ざらないようにする。閉じたタブの retained state は
  `Close` 時に捨てる。

### 5.7 テスト (`daw_gui`)

- `tests/app_state/project_tabs.rs` (コマンド / イベント層):
  New → 2 タブ、active が新しい方 / Open into pristine は置き換え、非 pristine は新タブ /
  Close 最後 → Untitled が残りタブ帯は非表示相当 (`tabs.len()==1`) / Next / Prev の巡回 /
  Move の並び / 編集は active にだけ入り undo も別 / `SlotPluginLoaded { project: 背景 }` が
  背景タブの `loaded_devices` に入る / Quit の dirty 確認が順番に出る / 32 個目以降の New は拒否 /
  ドラッグ中の `Ctrl+Tab` で payload が立ち元タブの Song が無変更、落とすと独立コピー。
- headless: `tests/scripts/project_tabs.js` — 2 ファイルを 2 タブに load、両方 play、
  `transportState` が独立、switch、close。起動を伴うので統合後に 1 回。

## 6. 進め方

並列 worktree はトークン消費が大きい (3 セッションが同じ調査を繰り返し、統合の往復も要る) ので
**この worktree で直列に**進める (2026-09-12 ユーザー判断)。

| 順 | 範囲 | 確認 |
|---|---|---|
| 1 | §1 / §2 (`common/`) — 契約 | `cargo check -p common` + protocol テスト |
| 2 | §4 daw_plugin_host | `cargo check -p daw_plugin_host` + `cargo test -p daw_plugin_host` |
| 3 | §3 daw_audio | `cargo check -p daw_audio` + `cargo test -p daw_audio` |
| 4 | §5 daw_gui | `cargo check -p daw_gui` + `tests/app_state` |
| 5 | 統合 | `make build` → `make clippy` / `make arch-lint` / `make test-nolaunch` を 1 回 → headless script → 実機 sign-off → main |

途中の WIP commit は worktree の branch に積み、sign-off 後に 1 commit へまとめる。

## 7. 検証項目

- 1 プロジェクトだけ開いた状態で **`dsp_load_avg` (MetricsBridge) が変更前と同等**であること
  (同じ曲・同じ buffer size で before / after を実測。「1 タブは今と同じコスト」の裏取り)。
  - **実測 2026-09-12** (`spec/rack/rack.daw`、48kHz / 480 frames、headless `daw.metricsJson()`、
    main `27186b29` と本 branch を交互に 2 回ずつ、再生 6 秒後から 1 秒間隔 8 サンプル):
    before 0.068→0.038 / 0.068→0.041、after 0.067→0.038 / 0.070→0.042 (xrun 0)。差は run 間の
    ばらつき以下 = 劣化なし。手順は `daw.metricsJson()` + `--arg song=` の script。
- 2 タブで両方再生 → 両方聞こえる / 片方 Stop でもう片方は続く / タブ切替でメーターが切り替わる。
- タブを閉じる → 音が消え、plugin_host の instance が消える (resource monitor で確認)。
- 書き出し中に背景タブが止まり、終了後に続きから走る。
- タブ間 D&D (spring-loaded / Ctrl+Tab) で独立コピーが落ち、元タブは無変更。

## 8. 計画からの逸脱 (実装中に判断したら追記する)

- **「余白へ落としたら新しいトラック」を同じプロジェクト内のドラッグにも広げた** (実機の
  要望)。r.md #129 の範囲はタブ間の持ち込みだけだったが、同じ規則が 2 つの経路で違うと
  「Ableton みたいに」が半分しか成り立たない。`compute_clip_drag_track_delta` が最終行より
  下で仮想の行を返すようにし (上限は掴んだ一番上の行が最初の新しい行に乗る位置)、
  `drag_preview_geometry` の下側 clamp を外し、commit が不足分のトラックを作ってから
  `move_time_range` / `copy_time_range` へ渡す。

- **`RtBundle` を変えた**: §3.1 は「`RtBundle` は変えない」としていたが、per-track scratch を
  曲の本数ぶんだけ配るために `scratch_growth` を足した (同 §の追記)。song と同じ便で運ぶのが
  要件なので、別の ring を新設せず bundle に載せた。
- **タブ帯の幅の下限**: §5.3 は 80..=200 px としたが、入りきらなくなったら 28 px まで縮めて
  **全部のタブを画面の中に収める**。80 px で止めると、タブが増えたぶんだけ帯が画面の右へ伸びて
  後ろのタブに触れなくなる (閉じることもできない)。細いタブでは ✕ を出さない (右クリック
  メニュー / Ctrl+W で閉じる)。未保存の `*` は名前とは別に右端へ固定で描く (名前に足すと
  長いファイル名で省略されて消え、保存済みに見える)。

- **`AppData` の形**: §5.1 の `tabs.order: Vec<ProjectState>` + `proj()` アクセサではなく、
  `cur: ProjectState` (見えているタブ) + `tabs.parked` (それ以外) にして handler / view は
  `self.cur.*` を直に触る。背景タブは `with_project(key, f)` (対象を `cur` へ swap して回し、
  戻す) だけで扱う。`proj()` 化だと 150 ファイルの全参照に `&mut` 借用の衝突が出る一方、
  swap は `handle_event` の入口 1 か所で済む (`AppEvent::target_project`)。
- **`pending_state_queue` はタブごと** (`ProjectIpc`)。plugin host は `RequestAllStates { project }`
  に `AllPluginStates { project }` で答えるので、in-flight もタブごとに独立でよい。
  `ExportState` の切り出しは行わず、export 状態は従来どおり `transport` (= タブごと) に残した。
- **Open で既に開いているファイル**: 同じファイルを 2 つのタブで編集して互いに上書きし合う
  事故を避けるため、そのタブへ切り替える (新しいタブは作らない)。
- **`PluginCommand::SetProjectDir`** は plugin host が値を使っていない (main でも write-only) ので
  frame flush から送らない (毎 flush 送ると「grouping は host を触らない」等のテスト契約が壊れる)。
- **タブをまたぐトラック D&D の plugin state**: copy (`Ctrl+C`) は `RequestAllStates` の
  round-trip で最新 state を取ってから envelope を作るが、ドラッグの昇格は同期なので Song に
  保存済みの state (最後の sync 時点) を運ぶ。窓の中で回した直後のツマミは落とし先に載らない。
- **Quit の「保存せず終了」** はそのタブを閉じてから次の未保存タブへ進む (閉じないと
  `continue_quit` が同じタブをまた聞く)。閉じる = `UnloadProject` + `CloseProject` を送るので、
  終了直前に host が一度そのタブの instance を畳む (その後 `Shutdown`)。
- **プラグインエディタ窓のタイトル**: タブが 1 つでも常に `[<project 名>]` を付ける
  (タブ数で書式を変えない)。
- **タブ切替時に閉じる一時 UI**: picker / 範囲ダイアログ等は旧タブの id を指すので、切替で
  `close_transient_ui` (終了時に畳むのと同じ集合) を通す。
- **clipboard が媒体を運んでいなかった** (既存の穴、タブで顕在化): クリップ / トラック / セル /
  時間範囲の写しは content だけを運び、音源 / 映像 / 画像のテーブル (`Song.media`) を運んで
  いなかったので、別プロジェクトへ貼ると `source_id` が宙に浮き **オーディオクリップの殻だけ**が
  貼られていた (Ctrl+C / Ctrl+V でも同じ)。`common::model::MediaManifest` を envelope と
  `TimeRangeCopy` に同梱し、貼り先が別 project なら `Song::import_media` で取り込んで
  張り替える (同じパスの音源は流用)。
- **タブ間 D&D の対象を広げた**: §5.6 はアレンジのクリップとトラックだけだったが、実機の要望で
  **ランチャーのセル**も運べるようにし、落とし先も「アレンジのレーン ⇄ セッションの格子」を
  相互に受ける (セル → レーンは開始拍順に、クリップ → セルは同じ行の左の列から)。
  グループトラックを掴んだら **子トラックも一緒に** 運ぶ。
- **着地プレビューは落とし先のタブで描き直す**: 昇格した時点で元タブの drag session は捨てる
  ので、内部ドラッグのゴースト描画は使えない。`widgets/arrangement/xfer_ghost.rs` (レーン /
  ヘッダ列) と `widgets/arrangement/launcher/xfer.rs` (帯) が payload から描く。
  **帯の分は帯自身の描画パス**に置く — ランチャー帯はアレンジ heavy の後に描かれ、格子全面を
  不透明に塗り直すので、アレンジ側で積むと埋もれる (実機で発覚、4 観点の診断で確定)。
- **端の自動スクロールはタブ間ドラッグでも効かせる**: press が別タブで起きているので、session と
  press 位置を前提にした既存のゲートでは永久に発火しなかった。payload が生きている間は
  クリップ移動と同じ両軸で許可する (ゴーストと着地位置は毎フレーム解き直すので anchor 補正は不要)。
- **確認モーダルはボタンから閉じない**: `Ui::modal` は close を検出したフレームに `on_close`
  (= `DirtyGuardCancel`) を積み、それがボタンの edit の **後** に走る。終了シーケンスの
  「保存せず終了」は次の未保存タブの確認を再武装するので、その Cancel に消されて 2 つ目以降の
  タブを聞かないまま止まっていた。閉じる判断は `dirty_guard` を SSoT にする
  (回帰網は実ポインタで駆動する `tests/dirty_guard_click.rs`)。
- **終了時の「保存せず終了」はタブを閉じない**: 閉じる経路は書き出し / 解析中のタブを拒否するので、
  拒否されると dirty が残ったまま同じタブを永久に聞き続ける。変更を捨てるだけにして、
  アプリごと終わる (`begin_shutdown` が全タブの Stop と子プロセスの teardown を担う)。
- **dirty guard のテスト**: New / Open がガードを通らなくなったので `tests/app_state/dirty_guard.rs` の
  New / Open 系 13 件を「タブを閉じる」を破壊操作とする形に書き換えた (期待値の変更は Q4 / Q5 の
  決定に従う)。
