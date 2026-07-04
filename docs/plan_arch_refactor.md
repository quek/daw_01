# plan_arch_refactor — 全体アーキテクチャ改修 (2026-07-03)

6 系統の並列アーキテクチャ分析 (daw_gui app / common / daw_audio / daw_plugin_host / ui /
プロセス間同期) で確定した構造問題を、一括で最終形へ改修する。個別の発見と証拠 (file:line)
はチャットレポート参照。本書は **確定した設計判断の SSoT**。

## 0. 改修の柱 (原則)

1. **identity は安定 id 一本** — positional index をプロセス境界・イベント・永続参照に使わない。
2. **Song は「ドキュメント」、wire は「エンジンが読む分だけ」** — MB 級 blob は wire を渡らない。
3. **RT スレッドは無限待ちしない・確保しない・解放しない** — 重い作業は off-thread で構築し swap。
4. **編集の副作用 (undo/dirty/sync) は単一の口** — 迂回はコンパイルエラーにする。
5. **宛先・カテゴリは型で表現** — doc comment 頼りの規約を enum 分割で置き換える。
6. **live と export は同じ関数** — 二重実装を許さない。
7. **daw-ui core は汎用基盤、DAW ドメイン widget はアプリ側** — mirror 型と翻訳層を全廃。

## 1. identity: DeviceId(u64) 一本化 [B2+A4]

- `PluginInstance.id: u64` を新設 (0 = 未採番 sentinel)。`Song.next_device_id: u64` で採番、
  `ensure_device_ids()` が load 時に旧ファイルへ採番 (track/clip と同 idiom)。
  master_fx_chain のデバイスも同じ allocator。
- **plugin_host の session `plugin_id: u32` 概念を廃止**。shmem 名 (`process_data_shmem_id`)、
  WorkerBridge の dispatch token (AtomicU32→AtomicU64)、イベント addressing、editor window
  keying、全部 `device_id: u64`。
- protocol の `(track, index)` addressing を全廃:
  `SetSlotPlugin { device_id, track_id, .. }` / `RemoveSlotPlugin { device_id }` /
  `RequestSlotState { device_id }` / `OpenSlotGui { device_id, title }` /
  `OpenPluginShmem { device_id, shmem_id }` (track/index 不要 — daw_audio は Song から配置解決) /
  イベント側 `SlotPluginLoaded { device_id, .. }` 等。
- **`ReorderChain` message を削除**。reorder は Song 編集 + LoadSong 再送だけで完結
  (plugin_host は順序を持たない: flat `HashMap<u64, InstanceRecord>`、処理順は daw_audio の
  schedule が Song から compile)。3 プロセス貫通の再キー儀式・`republish_entry_slot`・
  4 本の並行 bookkeeping map を全部削除。
- `HostCallbacks` は device_id を capture (焼き込み座標の stale 問題が消滅)。
- sends: `TrackSend.id: u32` 新設 + `Song.next_send_id`。`AutomationTarget::SendGain` /
  IPC `SetSendGain` を send_id に。`reindex_send_gain_lanes` 削除。
- `AutomationTarget::PluginParam { device_index }` / `BindingTarget::PluginParam` → `device_id`。
  migration: ensure_device_ids で採番後、旧 index を id へ写像 (project version bump)。
- note / automation point / audio event にも安定 id (per-content 採番) を追加し、
  選択 (`selected_notes` 等) と `after_undo_redo` を「解決できない id を落とす retain」に統一。
  `ClipRef` (index pair) を削除し `ClipKey` に一本化。

## 2. wire から blob を外す [B1]

- `PluginInstance` の bincode `Encode`/`Decode` を手書きし、**`state` / `ara_archive` を
  wire format から構造的に除外** (decode 側は常に None)。serde (ファイル保存) は現状維持
  (base64 埋め込み) — ファイル形式は変えない。型レベルで「LoadSong に MB blob が乗らない」を保証。
- blob が要る操作は既に専用メッセージ (SetSlotPlugin.initial_state / SetupAraDocument.archive /
  AllPluginStates) — 変更なし。
- `AraSourceSpec::Pcm { samples: Vec<f32> }` variant を**削除** (69MB 崖)。bounce 済み
  in-memory audio は GUI が project cache に WAV として書き出し `WavFile(path)` で渡す。
- LoadSong flood 対症療法の整理: `pending_host_sync` (frame coalesce) を唯一の経路に。
  BeginDrag/EndDrag/is_dragging (emitter ゼロの半死機構) と EndDrag の bare LoadSong 直送を削除。

## 3. protocol 分割 + fingerprint [B3+A9]

- `MainToChild`/`ChildToMain` を廃止し 4 enum に分割:
  - `AudioCommand` (gui→audio) / `AudioEvent` (audio→gui)
  - `PluginCommand` (gui→plugin_host) / `PluginEvent` (plugin_host→gui)
  - 共有 struct (AudioSession / SlotState / AraClipSpec / PluginParamInfo 等) は protocol.rs 残留。
  - pipe の read/write を型パラメータで縛り、誤配送・no-op arm 列挙・無駄 decode を型で殺す。
- `common/build.rs` が wire を渡る source (protocol/model/plugin_metadata/process_data/
  audio_bridge/wire) の **FNV-1a content hash** を計算し `PROTOCOL_FINGERPRINT: u64` として export。
  Hello に載せ、mismatch は handshake で明示 fail (「make build を実行」)。protocol 未変更の
  再ビルドでは値が変わらない (content hash) ので誤検知しない。
- `AudioSession` から死んだ `request_sem_id` / `ready_sem_id` を削除。
  `AudioBridge` の `samples` / `frames_requested` 面 (writer/reader 不在) も削除。
- pipe writer task 死 (16MB 超 encode 失敗等) でも `ChildDisconnected` を合成し respawn 経路へ
  一本化 (現状 read 断のみ検知 = 沈黙のゾンビ)。
- respawn 時、新 Hello の `device_sample_rate` を採用して session を更新 (現状は初回値を再送)。
  respawn は GUI main thread の block_on をやめ非同期化 + 完了 AppEvent。

## 4. RT 境界の有界化 [A1+A8]

- `WorkerSyncRef::dispatch(device_id) -> DispatchOutcome { Done | TimedOut }`。
  `WaitForSingleObject(done, DISPATCH_TIMEOUT_MS)` (定数、初期値 500ms)。
- TimedOut → 該当 device を **quarantine** (per-entry AtomicBool、以後 skip して無音バイパス)、
  AudioEvent で GUI へ通知 (トースト + 該当デバイス赤表示は GUI 側既存 status 経路)。
  plugin_host respawn / SetSlotPlugin 再ロードで解除。
- worker pool の `all_done` 待ちも同 timeout。pool 単位の失敗は「plugin_host 死」と解釈し
  GUI へ通知 → 既存 respawn reconcile。stuck worker の `Drop::join` は poison event +
  bounded join に変更 (respawn 中の二次ハング防止)。
- `pump_commands` から重処理を退去: `OpenPluginShmem`/`ClosePluginShmem`/`OpenWorkerPool` は
  recv loop 側で新 snapshot (`RtBundle { schedule, tempo_map, song, plugin_refs, worker_pool }`)
  を構築し、既存 CompiledRouting と同じ rtrb forward/recycle ring で RT へ渡す。
  旧 bundle は recycle ring 経由で off-thread drop (`Box::leak` と 190KB/往復の shmem leak 解消、
  worker pool の spawn/join も off-thread 化)。
- routing ring が full のときは drop-newest でなく **drop-oldest** (最新編集を優先)。

## 5. daw_audio エンジン品質 [A2+A3+A5+finding6-8]

- **live/export 統一**: dispatch → schedule 実行 → master fx → master gain を
  `render_master_buffer()` に括り両者が呼ぶ (metronome 等 monitoring 専用だけ live 側)。
- **値更新 vs topology 分離**: SetTrackVolume/Pan/SendGain/SetSongBpm 等は song snapshot
  差し替えのみで `compile_schedule` を走らせない。topology 変更 (LoadSong) の再 compile 時は
  `Schedule::adopt_state_from(old)` で DelayLine / FollowerSlot を stable key
  (track_id, device_id / mod_source_id) で移送 (delay 長不変のもののみ、変わったものはリセット)。
- **cpal 0.17 へ更新** (0.15.3 は callback thread の priority boost が壊れている —
  thread id を HANDLE として渡す) + callback 初回に `mmcss::join_pro_audio()` を自前適用
  (RT 優先度ポリシーの SSoT を自プロセスに持つ)。
- sidechain tap: leaf 宛 tap の 1-buffer lag を `input_delay_per_track` に算入して位相統一
  (依存 wave 分割 dispatch は将来課題として docs に明記)。
- engine.rs 分割: schedule 実行 + mix helpers → `graph/execute.rs`、strip 適用
  (equal-power pan/volume/peak の二重インライン) を関数化して `mixer.rs` へ、metronome →
  独立 module。stale module doc (「PR3 stub」) と dead `reduce_master` を削除。

## 6. plugin_host [A4+A7+B8+finding5-8]

- bookkeeping を `HashMap<u64 /*device_id*/, InstanceRecord { plugin, track_id, loaded_id,
  meta, editor, shmem }>` に一本化。(track,index) キーの 4 map + 再キー儀式を削除。
- **CLAP host extension 完備** (VST3 と対称に):
  `clap_host_latency.changed` → 再 query + `PluginLatencyChanged`、
  `clap_host_params.rescan` → param list 再送、
  `request_restart` → 既存 quiesced-reinit 経路、`request_callback` → plugin-main queue で
  `on_main_thread()`。restart には per-plugin cooldown (3 回/10s 超で無視 + warn) —
  reinit ループの構造的防御。
- `ReinitAllPlugins` 完了時にも per-plugin latency 再 query + emit (restartComponent 経路と
  共通関数化)。
- activate 失敗はゾンビ publish せず `SlotPluginLoadFailed` で GUI へ。worker の process Err /
  no-plugin warn は per-entry one-shot (AtomicBool) — RT スレッドでの毎 buffer format+log を排除。
- **ProcessScaffold 抽出** [B8]: 入力 copy / aux copy / bus assembly / transport 導出
  (非有限 sanitize 込み — VST3 側の未 sanitize も同時に是正) / modulation folding を
  format 非依存 struct に一本化。CLAP/VST3 は「scaffold → FFI 型への写像 + 呼び出し」だけ。
  ARA lifecycle dance (deactivate→set_clips→restore→reactivate) も `AraHost` に一本化。
  ClapPlugin の 190 行 trait 転送シムは inherent を trait impl へ移して削除。
- **split-half 化** [finding5]: audio thread が触る状態 (process buffers / folded events 等) を
  `AudioProcessorHalf` として分離し、worker registry にはこちらを渡す。plugin-main の `&mut`
  と worker の `&mut *raw` が同一オブジェクトに並存する aliasing UB を型で解消。
  `PluginPtr` の stale な契約コメント (restart→quiesce) も実態に修正。
- `LoadedPlugin` の VOICEVOX 専用メソッド群は `as_vocal_synth() -> Option<&mut dyn VocalSynth>`
  1 本に集約。status reporter は `HostCallbacks` に統合 (第 2 callback 機構を廃止)。

## 7. daw_gui 再構成 [A10+B3+B4 + agent1 全指摘]

```
daw_gui/src/
  state/
    mod.rs          -- AppData = 合成 struct (下記 field group)
    song_doc.rs     -- song(private) / saved epoch / undo / redo / file_path / dirty
                       ★ edit_song(scope, f) が &mut Song を得る唯一の口:
                         snapshot・dirty・epoch bump・host-sync 予約を無条件実行。
                         EditScope::Gesture(id) で scrub 系 (1 drag = 1 undo) を表現。
    transport.rs / selection.rs / ipc.rs / voicevox.rs / media.rs / recording.rs /
    ui_ephemeral.rs (undo 非対象を型で宣言、view 直書き可)
  event/            -- AppEvent { Edit(EditEvent), System(SystemEvent), Ui(UiEvent) } 2 段化
                       System = IPC 返信 (AudioEvent/PluginEvent 直 wrap — bridge match 削除) /
                       Tick / crash。export gate は「Edit を一括 drop、System を一括通過」。
    handler/        -- domain reducer (handle_event は top dispatch のみ)
  widgets/          -- §8 で移設される arrangement / piano_roll (common::model 直結)
```

- `is_undoable` whitelist (102 variants)、手動 `push_undo_snapshot` 28 箇所、view からの
  song 直接編集 (SetTrackParent 二重実装) を全廃 — song private 化で迂回はコンパイルエラー。
- sync: `sync_song_to_plugin_host` (命名も嘘) を解体。edit_song の epoch bump →
  runner frame flush が「epoch 変化時に full sync 1 回」を実行する pull 型一本。
  bounce isolation の direct send だけ明示 API で残す。dirty 判定の毎フレーム
  Song 全比較も epoch 比較に置換。
- 世代 guard: `SetSlotPlugin` に request generation を載せ `SlotPluginLoaded` で echo、
  slot ごと最新 gen のみ受理 (`pending_plugin_loads` を HashMap<(..), gen> に)。
- `video_texture_cache` / `image_texture_cache` / HWND を AppData から runner 側
  `MediaResources` へ退去 (dispatcher.rs と同じ抽象で headless テスト可能性を回復)。
- runner.rs の video 合成 (L1080-1620) を `PreviewCompositor` (video_playback 側) へ抽出。
  runner は「OS イベント→AppEvent + frame 駆動」専任。
- BPM/TimeSig/Text 等の編集バッファ mirror (`resync_song_edit_texts`) を廃し
  scrubable_number / focused-commit idiom に統一。

## 7.5 S3b 詳細設計 (daw_gui 再構成の実装仕様)

### AppEvent 3 分類 (2 段 enum)

```rust
pub enum AppEvent {
    Edit(EditEvent),     // song を変える意図 (undo/dirty/sync の対象)
    System(SystemEvent), // 外界からの通知: IPC event / tick / job 完了 / MIDI 入力 / dialog 結果
    Ui(UiEvent),         // view 状態のみ: 選択 / zoom / scroll / picker / rename buffer / transport 操作
}
pub enum SystemEvent {
    Audio(common::protocol::AudioEvent),   // bridge match を丸ごと削除 (直 wrap)
    Plugin(common::protocol::PluginEvent),
    Tick, AutosaveTick, MidiIn(..), FileDialog(..), JobDone(..), ...
}
```

分類規約: **「song を変えるか」でなく「発火元と処理面」で分類する**。MIDI 入力は System
(録音中なら handler が edit_song を呼ぶ)。Commit 系 (CommitBpmEdit / CommitRenameClip) は
Edit、Begin/Changed/Cancel 系は Ui。Play/Stop/Seek/Panic は Ui (song 非編集。loop 範囲変更は
Edit)。Undo/Redo は Edit (song_doc 内の専用復元経路)。

### export gate の再設計 (whitelist 全廃)

旧 allow-list (negative default、入れ忘れ→GUI 永久ロック事故 3 件) は**イベント遮断を
やめる**ことで解決する: System/Ui は export 中も全部流す。song の凍結は
**`edit_song()` が export_active 中は編集を拒否** (status message + None) する 1 箇所で
保証。transport 系 (Play 等) の export 中挙動は各 handler の既存 exporting チェックに委譲。
「新 variant の分類し忘れ = deadlock」という故障モードが型ごと消える。

### edit_song チョークポイント (state/song_doc.rs)

```rust
pub enum EditScope {
    Discrete,            // 1 操作 = 1 undo step
    Gesture(u64),        // drag/scrub: 同一 gesture id の連続編集は 1 undo step に squash
}
impl SongDoc {
    // &mut Song を得る唯一の口。song field は private。
    pub fn edit<R>(&mut self, scope: EditScope, f: impl FnOnce(&mut Song) -> R) -> Option<R>;
    pub fn song(&self) -> &Song;                  // 読みは自由
    pub fn edit_epoch(&self) -> u64;              // frame flush / dirty が読む
    // undo/redo/load/save は SongDoc のメソッド (スナップショット方式は維持)
}
```
edit() の無条件副作用: undo snapshot (Gesture 継続中は skip) → 編集実行 → `edit_epoch += 1`。
dirty は `edit_epoch != saved_epoch` の O(1) 派生 (毎フレーム Song 全比較を置換)。
`is_undoable` whitelist (102 variants)・手動 `push_undo_snapshot` 28 箇所・view からの
song 直接編集・`resync_song_edit_texts` は全廃。

### sync 一本化 (pull 型)

runner の frame 末: `edit_epoch != last_synced_epoch` なら unified sync を 1 回実行
(ports 解決 → SetProjectDir → blob-less LoadSong → vocal metadata → ARA docs/regions →
lipsync mark) して `last_synced_epoch = edit_epoch`。`pending_host_sync` フラグ・
BeginDrag/EndDrag/is_dragging (emitter ゼロの半死機構)・EndDrag の bare LoadSong・
直接 send 6 箇所は全廃。bounce isolation だけ明示 API で残す。scrub の coalesce は
frame flush が構造的に担う (1 frame ≦ 1 LoadSong 保証)。

### state/ モジュール分割 (AppData 213 フィールドの帰属)

| module | 中身 (代表) |
|---|---|
| song_doc | song(private) / undo / redo / file_path / saved_epoch / edit_epoch / autosave |
| transport | is_playing / looping / playhead / master_gain / peaks / preroll / metronome / export_active |
| selection | selected_track / clips / notes / audio events / automation points + last-selection-wins tier |
| ipc | audio_tx: Sender&lt;AudioCommand&gt; / plugin_tx: Sender&lt;PluginCommand&gt; / supervisor / pending_plugin_loads (device_id→gen) / loaded_slots (device_id) / worker pool gen |
| voicevox | singers / speakers / per-device synth status / lipsync |
| media | audio_source_cache / staging uploads / import jobs |
| recording | midi_recording / count_in / param gestures / recording lanes |
| ui_prefs | zoom / scroll / panel / snap (ViewState 永続分) |
| ui_ephemeral | picker / rename buffer / hover / menu / modal / drag session (undo 非対象を型で宣言) |

GPU テクスチャ (`TextureHandle`) と HWND は AppData から runner 側 `MediaResources` へ退去
(dispatcher.rs と同じ抽象、headless テスト可能性の回復、Linux 対応の前提)。

### 実行順 (S3a 完了後)

1. state/ + event/ の骨格を作り、フィールド群を機械移送 (compile-fix driven)
2. edit_song 導入 + whitelist/手動 snapshot/半死機構の削除 (意味論の核)
3. handle_event を tier dispatch + handler/ モジュール分割へ機械移送
4. 周辺退去 (PreviewCompositor / MediaResources / BPM 編集バッファ→scrubable /
   ClipRef→ClipKey / 選択の stable-id retain / resource_monitor の device_id メトリクス)

### metrics の device_id 化 (S2b 要望への対応)

`MetricsBridge.plugin_dsp_us[u32 slot]` を `PluginMetricSlot { device_id: AtomicU64,
us: AtomicU32 }` の配列に変更。plugin_host が load 時 (非 RT) に空きスロットを claim し
RT は小さい slot index へ store、GUI は device_id で scan して読む。u64 単調増加 id が
slot 上限を超えて計測が silently drop する問題の根治。

## 8. daw-ui 境界 [B5]

- `widgets/arrangement.rs` (15.9k) / `piano_roll.rs` (8.1k) を **daw_gui/src/widgets/ へ移設**、
  `common::model` 直結 + Edit<AppData> 直発行。以下を全廃:
  ArrangementClip/Track 等 mirror 型、`ArrangementEditRequest` 60 変種、`make_edit` 736 行、
  型変換ヘルパ、毎フレーム二重 clone、rect 輸出 + アプリ上書き描画のマジックインセット
  (clip 中身は widget が直接描く)、`ArrangementStyle` の未使用ノブ。
- 移設時に `Ui::arrangement()` 単一 4,600 行関数を interaction 単位
  (clip drag / section / automation / reorder / header / ruler / draw) に分解。
  in-file テスト 5.6k 行は daw_gui/tests/ へ移して common::model ベースに書き直す。
- ui core (daw-ui):
  - retained state API を公開 (`Ui::widget_state` 相当を `stateful` として pub) —
    「stateful widget は lib 内にしか書けない」構造ポンプを解消。
  - **lib 側 undo 機構を削除** (`Edit::Undoable` / UiHost history / with_history_capacity /
    EditRequest の prev フィールド / Fn+Clone 拘束)。undo SSoT はアプリ (song_doc) 一本。
    fader/knob/scrubable_number は Mutate 発行に変更、examples も追従。
  - `snap.rs` / `time.rs` (音楽ドメイン) → common へ。`split_into_morae` → common::voicevox へ。
  - 意味色トークン (SOLO/PLAYHEAD/RECORD/CLIP_DEFAULT) を renderer から退去
    (renderer は Color 型 + 演算のみ、汎用 UI トークンは ui-core、DAW トークンは daw_gui theme)。
  - FontSystem 二重ロードを解消 (renderer 所有の FontSystem を measure 手段として ui へ注入)。
  - piano_roll の `notes_generation` (実証済みバグ源、内部 hash と二重) を削除し
    content-hash 一本化。
- examples/{arrangement, piano_roll, daw_prototype} は移設に伴い**削除** (moved widget の
  dev harness だったもの。以後は daw_gui 本体が harness)。他 examples は維持。

### S4 実行分解 (widget 移設の順序)

surface 実測 (2026-07-04): `arrangement.rs` 15,870 行 (mirror 型 + `ArrangementEditRequest`
58 variant + 4,600 行の `Ui::arrangement()` + in-file テスト ~5,663 行)、`piano_roll.rs`
8,099 行 (`PianoRollEditRequest`)。`daw_gui/view/arrangement_view.rs` に mirror 構築
(L198〜) + `make_edit` 736 行 (L1506) + `widget_to_model_*` 変換。lib undo = `edit.rs`
(244) + `history.rs` (306)、`Edit::Undoable` emit は fader/knob/scrubable_number/
drag_rect/arrangement/ui.rs/examples/tests — **daw_gui は emit していない** (= 未 replay
の死荷重、確定)。

runner.rs が S3b と S4 の共有チョークポイント (lib undo の `with_history_capacity`
呼び出し) なので S3b 完了後に着手。ファイル単位の逐次サブタスク:

- **S4a: lib undo 撤去 + ui core stateful 公開** — `edit.rs` の `Undoable`/`with_inverse`/
  `with_shared`、`history.rs` 全体、`UiHost::with_history_capacity`/`request_undo` を削除。
  fader/knob/scrubable_number/drag_rect を `Edit::Mutate` emit に変更 (`Edit<M>` 戻り型は
  不変なので daw_gui 呼び出し側は無影響のはず)。`Ui::widget_state` 相当を `stateful` として
  pub 化。examples/ui tests 追従。runner.rs の history 配線削除。
- **S4b: arrangement 移設** — `daw_gui/src/widgets/arrangement/` へ移し `common::model` 直結 +
  `edit_song` 経由の `Edit<AppData>` 直発行。mirror 型 / EditRequest 58 / make_edit 736 /
  変換 / 二重 clone / rect 輸出上書き描画を全廃。4,600 行関数を interaction 単位
  (clip drag / section / automation / reorder / header / ruler / draw) に分解。in-file
  テストは `daw_gui/tests/` へ移し common::model ベースに。
- **S4c: piano_roll 移設** — 同様。`notes_generation` 削除 (content-hash 一本化)。
- **S4d: renderer/core 衛生** — 意味色 (SOLO/PLAYHEAD/RECORD/CLIP_DEFAULT) を renderer から
  ui-core / daw_gui theme へ、FontSystem 二重ロード解消、`snap.rs`/`time.rs`/
  `split_into_morae` を common へ (S5 と調整)。examples/{arrangement,piano_roll,daw_prototype} 削除。

## 9. common 縮退 [B6]

- daw_gui へ移動: video_fx.rs / app_config.rs / window_state.rs / recent.rs / scale.rs /
  voicevox_engine.rs (エンジン起動)。
- daw_plugin_host へ移動: voicevox.rs (HTTP 合成) / voicevox_cache.rs (builtin が唯一の実行場所)。
  ※ GUI が使う speaker 一覧 fetch 等は GUI 残留分を精査して分割。
- plugin scan (clap_scan/vst3_scan — DLL 実ロード) を daw_plugin_host へ移動し、
  rescan は `PluginCommand::RescanPlugins` → `PluginEvent::PluginDbUpdated` の IPC に。
  plugin_db.rs (純データ) は common 残留。
- track_params.rs (参照ゼロ) 削除。
- 成功指標: common の Cargo.toml から `reqwest` が消える (libloading / clap-sys / vst3 も
  可能な範囲で退去)。

## 10. Song / serde 衛生 [common agent 4-5]

- `ClipContent` の `#[serde(untagged)]` に tag を導入 (`type` field)。旧 untagged ファイルは
  load 時の JSON 前処理 (`project.rs` の既存 idiom) で一回変換。variant 追加時の
  silent-misparse リスク (content 型数の 2 乗) を解消。
- migration 専用 legacy field 8 個 (`Clip.name`/`Clip.notes`/`Track.legacy_*` 3 種/
  `PluginInstance.legacy_aux_sources`/`AutomationTarget.legacy_slot`/`BindingTarget.legacy_slot`)
  を JSON 前処理層へ吸収し in-memory 型から削除。`PluginSlot` enum も migration 層へ。
- migration を `(version, fn)` の一覧表 (単一 dispatch table) に一本化。
- Song の sub-struct 化 (`MediaPools` / `IdAllocators` 等) は本改修では **見送らず実施**
  — ただし wire/save 互換への影響が大きいため §1-9 完了後の最終段で。

## 11. 再発防止 (ワークフロー) [ユーザー依頼 2 点目]

- **CLAUDE.md に「アーキテクチャ不変条件」節を追加** (§0 の 7 原則を検査可能な形で)。
- **`make arch-lint`** (bash scripts/arch_lint.sh): 機械検査 —
  RT パスの `INFINITE`、`(u32, u32)` positional key、`#[serde(untagged)]` 新設、
  protocol への `Vec<f32>`/`Arc<[u8]>` 混入、file 行数 budget (>3000 warn)、
  common の依存 (reqwest 等) 逆流。clippy と並ぶ検証段として Makefile に組み込む。
- **guards.jsonl 追記** (承認不要の主経路): INFINITE / positional tuple key /
  `push_undo_snapshot` 直呼び / untagged 追加 / MainToChild 復活 の warn ルール。
- **新 skill `/arch-review`**: 本セッションの 6 レンズ並列分析 + arch-lint + 行数 budget を
  定型化 (四半期/大機能後に回す)。
- **implement skill に「アーキテクチャ影響チェック」段を追加**: 新機能が
  (a) 新しい id/addressing を導入するか (b) 新しい同期経路を生やすか (c) enum をどの層に
  足すか (d) god file を太らせるか、を実装前に列挙させる。
- **review skill に不変条件チェックリストを参照追加**。

## 12. 検証

- `make check` → `make clippy` → `make build` → `make test` (全段 green)。
- `cargo run -p daw_gui --features script -- --script` の headless テスト
  (loadSongFile/play/stop/exportWavRange) で export/master fx/PDC 経路を自動確認。
- `daw_gui --smoke-test` (video visual regression)。
- 実機 sign-off (ユーザー) → commit。旧ファイル (v28 以前) の load 互換をテストで担保。

## 13. 意図的に変えないもの

- wire framing (length-prefix + 16MB 上限)・atomic save・decode 世代 guard・
  worker pool quiesce/UAF 対策・ARA session のレイヤ構造・dispatcher.rs 抽象 —
  いずれも健全と裏取り済み。
- Song 全量 snapshot undo (Ardour 系実績方式) — blob Arc 共有で軽量、方式は維持。
- 全量 LoadSong 同期そのもの — blob 分離 + epoch coalesce 後は「常に小さい全量」で、
  diff protocol より一貫性が強い。
