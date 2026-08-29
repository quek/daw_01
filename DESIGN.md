# 設計書

VOICEVOX 歌声合成を組み込んだ Rust 製 DAW。Clip ベースのタイムライン、CLAP/VST3
プラグイン、ビルトイン映像 (video/image/text overlay + 立ち絵/口パク) を持つ。

本書は**現行実装の正本**。アーキテクチャの不変条件は CLAUDE.md「アーキテクチャ
不変条件」、2026-07-03 全体改修の設計判断は `docs/plan_arch_refactor.md`。

## アーキテクチャ

### プロセス構成

3 つの独立した実行ファイル（別プロセス）。

| プロセス | 役割 |
|---|---|
| **daw_gui** | UI 表示・編集。Song ドキュメントの SSoT。gui_01 (daw-ui = winit + wgpu + 自作 immediate-mode) |
| **daw_audio** | オーディオ出力・シーケンサ・ルーティンググラフ・ミキサ。CPAL (WASAPI) |
| **daw_plugin_host** | CLAP/VST3/builtin プラグインのロード・実行・エディタ GUI・ARA |

制御プレーンは**星型**: daw_gui が両方の子へ named pipe を張る。audio ↔ plugin_host
間に制御路は無く、両者を繋ぐのはデータプレーン (shared memory + named event) のみ。

```
            ┌── control (named pipe, bincode) ──┐
   AudioCommand/AudioEvent            PluginCommand/PluginEvent
            │                                   │
        daw_audio ◄── WorkerBridge + per-device ProcessData ──► daw_plugin_host
            │            (shmem + named event, RT dispatch)
            └── AudioBridge / MetricsBridge (shmem telemetry) ──► daw_gui (30Hz poll)
```

- 子プロセスは Job Object (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) で daw_gui に紐付き、
  親がどう死んでもゾンビ化しない。
- 子の crash / pipe 断 (read 断・**write 断の両方**) は synthetic `ChildDisconnected` に
  正規化され、daw_gui が respawn → Session/worker pool/LoadSong/plugin slot を冪等に
  再構築する。respawn 時は新 Hello の `device_sample_rate` を採用して session を
  再構成する (デバイス変更に追従)。

### 制御プレーン (named pipe + bincode)

`common/src/{protocol,wire,pipe,client}.rs`。

- **宛先別の型付き enum** (単一 enum は廃止):
  - `AudioCommand` (gui→audio) / `AudioEvent` (audio→gui)
  - `PluginCommand` (gui→plugin_host) / `PluginEvent` (plugin_host→gui)
  - 誤配送・「相手が無視する variant の no-op arm」・無駄 decode が型で消える。
- フレーミング: 4 byte LE 長プレフィクス + bincode body (16MB 上限で DoS 防御)。
- **fingerprint handshake**: 子は Hello に `PROTOCOL_FINGERPRINT` (wire を渡る source
  群の content hash、`common/build.rs` が焼き込み) を載せ、親は不一致で明示 fail
  (「make build を実行」)。ビルド世代混在の silent misdecode を接続時に検出する。
- **wire は blob-less**: `LoadSong(Song)` の `PluginInstance` は手書き bincode impl が
  `state` / `ara_archive` (MB 級 blob) を構造的に除外する。blob が要る操作は専用
  message (`SetSlotPlugin.initial_state` / `SetupAraDocument.archive` /
  `AllPluginStates`) が個別に運ぶ。ARA の in-memory PCM は project cache へ WAV
  materialize してから path で渡す (bulk を pipe に載せない)。

### identity — 安定 device_id (u64)

デバイス (プラグインインスタンス) のアドレスは Song が採番する
**`PluginInstance.id` 一本** (Song-global `next_device_id`、master fx も同 allocator)。
IPC・automation (`AutomationTarget::PluginParam { device_id }`)・MIDI binding・
plugin_host の bookkeeping (`HashMap<u64, InstanceRecord>`)・shmem 名
(`process_data_shmem_id(pid, device_id)`)・worker dispatch token (WorkerBridge の
AtomicU64) がすべて同じ id を使う。chain 内 index は表示順序のみ。

これにより「削除/並べ替えで参照を貼り替える補償機構」(旧 ReorderChain の 3 プロセス
貫通再キー、callback に焼き込まれた座標の stale 化) が**存在しなくなる** — reorder は
Song 編集 + LoadSong 再送だけで完結する。send (`Send.id`)、note / audio event /
automation point (per-content 要素 id) も同様に安定 id。

### データプレーン (shared memory + named event)

| 経路 | 用途 |
|---|---|
| `WorkerBridge` + wake/done named event 対 | audio worker ↔ plugin_host worker の 1:1 dispatch。`worker_task[i]` (AtomicU64) が device_id を運ぶ |
| per-device `ProcessData` shmem | 音声バッファ・イベント・transport (per-buffer) |
| `AudioBridge` shmem | telemetry: playhead / peaks / mod scalars / preroll (writer = daw_audio、GUI 30Hz poll) |
| `MetricsBridge` shmem | DSP load / xrun / per-plugin process() μs |

#### RT dispatch の有界性 (poisoning contract)

`WorkerSyncRef::dispatch(device_id, DISPATCH_TIMEOUT_MS)` は done を**有界**で待つ
(`common/src/plugin_ref.rs`)。timeout = その worker pair は **poisoned** (auto-reset
event に待ち手なし signal が残留し得るため、pool 再構築まで dispatch 禁止)。
該当 device は **quarantine** (AtomicBool、以後 mix から無音バイパス、shmem にも
触らない) され、専用 notify スレッドが `AudioEvent::PluginUnresponsive` を 1 回だけ
GUI へ通知する。pool 全体の完了待ち (`all_done`) も有界で、timeout は
`WorkerPoolStalled` → GUI が plugin_host respawn → `OpenWorkerPool` 再送 (worker
event 名は **generation** 込みで mint され、旧世代の stale signal が新 pool に漏れない)。

プラグインの SEH crash / `process()` ハングが CPAL コールバックを永久凍結させる
経路は存在しない — 3 プロセス分離の目的 (crash 隔離) が異常系でも成立する。

#### RT スレッドの構造規約

- CPAL callback / worker では確保・解放・ロック・I/O をしない (`--features rt-assert`
  の allocator hook + テストで機械証明)。
- 重い状態遷移 (plugin_refs map / worker pool / schedule の差し替え) は recv loop が
  off-thread で `RtBundle` を構築し、rtrb forward/recycle ring で RT へ swap 配送。
  旧 bundle の解体 (shmem unmap / thread join / free) も off-thread。ring 満杯は
  drop-oldest (最新編集優先)。
- CPAL callback スレッドは初回に MMCSS Pro Audio へ自前 join (cpal 0.17 の boost に
  依存しない、RT 優先度ポリシーの SSoT は自プロセス)。

### daw_audio エンジン

- **live と export は同じ render 関数**: `graph/execute.rs::render_master_buffer` =
  clear → pass-1 dispatch → schedule 実行 → master fx chain → master gain。
  WAV export / bounce も同経路 (master limiter がエクスポートに乗らない、という
  非対称は構造的に起きない)。metronome / panic declick だけが live 側。
- **値更新 vs topology の分離**: volume / pan / send gain / BPM scrub 等は song
  snapshot の clone-mutate-store のみ (graph 再 compile なし)。topology 変更
  (LoadSong) の再 compile では `Schedule::adopt_state_from` が PDC DelayLine
  (`DelayKey` = track_id) と follower env (`ModSource.id`) を旧 schedule から
  install 時 `mem::swap` で移送 (alloc/free 0) — fader drag 中に補償パスが
  ミュートされたり follower が段差を出したりしない。
- PDC は plugin 報告 latency + **leaf 宛 sidechain tap の 1-buffer lag** を
  `input_delay_per_track` に算入して全経路の位相を揃える。
- モジュール: `engine.rs` (状態 + transport 駆動) / `graph/{compile,schedule,execute,
  delay_line,port_buffer,follower}.rs` / `mixer.rs` (strip 適用) / `metronome.rs` /
  `sequencer.rs` / `export.rs` / `audio_worker.rs` / `audio_clip_renderer.rs`。
- Song は `RtBundle` (song + tempo_map + schedule) として単一経路で publish され、
  RT は schedule と同一の song snapshot を読む。playhead は audio thread 単独 writer。
  WAV decode は専用 `audio-decode` スレッド (RT はディスクに触れない)。

### daw_plugin_host

- **`HashMap<u64 /*device_id*/, InstanceRecord>` 一本** (plugin / editor window /
  track_id / loaded meta / shmem を 1 record に集約)。順序概念なし。
- **split-half**: `LoadedPlugin` (main-thread half: lifecycle / GUI / state / ARA /
  param) と `AudioProcessorHalf` (process バッファ・event scratch を所有) を型で分離。
  worker registry には audio half のみ渡り、main の `&mut` と worker の `&mut` が
  同一オブジェクトに並存する aliasing は構造的に消滅。quiesce
  (detach → DispatchCounter 待ち → mutate → republish) は維持。
- **ProcessScaffold** (`process_scaffold.rs`): 入力/aux copy・bus assembly・
  transport 導出 (非有限 sanitize 込み)・modulation folding を CLAP/VST3 で共有。
  backend は「scaffold → FFI 型への写像 + 呼び出し」だけ。ARA lifecycle
  (deactivate → set_clips → restore → reactivate) も共有実装。
- **CLAP host extension は VST3 と対称**: `clap_host_latency.changed` → 再 query +
  `PluginLatencyChanged`、`clap_host_params.rescan` → param list 再送、
  `request_restart` → quiesced reinit (per-plugin **cooldown** 10s/3 回で
  reinit ループを構造的に防止、VST3 restartComponent も同じ tracker)、
  `request_callback` → plugin-main queue で `on_main_thread()`。
- activate 失敗はゾンビ publish せず `SlotPluginLoadFailed` (generation echo)。
  RT ログは per-entry one-shot。`ReinitAllPlugins` 完了時も latency 再 query。
- builtin (VOICEVOX / Silence / video 系) は外部プラグインと**同じ** InstanceRecord /
  worker / shmem / state 経路。VOICEVOX 固有 API は `as_vocal_synth()` capability
  (`VocalSynth` trait) に分離、status 通知は `HostCallbacks` に統合。
- スレッド: tokio (IPC) / plugin-main (Win32 pump + CLAP `[main-thread]` 直列化、
  callback は device_id capture で `HostNotify` channel に集約) / RT worker N 本。

### daw_gui

- **Song ドキュメントの SSoT**。編集は `state/song_doc.rs` の `edit_song()`
  チョークポイント一本 (song field は private): undo snapshot (gesture squash 対応) /
  dirty (epoch 比較) / 子プロセス sync 予約を無条件で実施。export 中は編集自体を
  拒否する (イベント whitelist は存在しない)。
- **AppEvent は 3 分類**: `Edit(EditEvent)` / `System(SystemEvent)` (IPC event の
  直 wrap + tick + job 完了) / `Ui(UiEvent)`。state は
  `state/{song_doc,transport,selection,ipc,voicevox,media,recording,ui_prefs,
  ui_ephemeral}` に分割、reducer は `handler/` 配下。
- **sync は pull 型一本**: frame 末に `edit_epoch != last_synced_epoch` なら
  unified sync (ports 解決 → SetProjectDir → blob-less LoadSong → vocal metadata →
  ARA → lipsync) を 1 回。scrub の coalesce は frame flush が構造的に担う。
- `SetSlotPlugin` は per-device generation で応答を突き合わせ、最新世代のみ受理
  (A→B 連続差し替えの stale 応答 race を排除)。
- GPU テクスチャ / HWND は AppData でなく runner 側 `MediaResources` が所有
  (model は renderer 型に依存しない)。preview 合成は `PreviewCompositor`。
- native file dialog は共通ヘルパで別スレッド + owner-modal (GUI スレッド同期禁止)。

### GUI ライブラリ境界 (daw-ui)

- `ui/crates/{platform,renderer,ui}` は**汎用 immediate-mode 基盤** (frame / input /
  popup / focus / heavy キャッシュ / 汎用 widget)。DAW ドメイン知識・mirror 型・
  翻訳 request enum を持たない。retained state は公開 API (`stateful`) で
  アプリ側 widget からも使える。
- **DAW 固有 widget (arrangement / piano_roll) は `daw_gui/src/widgets/`** に住み、
  `common::model` を直接読み、`Edit<AppData>` を直接発行する (中間表現なし)。
- undo は app 側 (song_doc) の単一系統。lib 側 history 機構は持たない。

## データモデル

```
Song ─ tracks: Vec<Track> ─ devices: Vec<PluginInstance>   (id: u64 安定)
     │                    ─ clips: Vec<Clip> ── content_id ─→ Song.clip_contents
     │                    ─ sends: Vec<Send> (id: u32 安定)
     │                    ─ automation_lanes / mod_routings
     │                    ─ parent_group_id (Reaper folder 流のグループ)
     ├ clip_contents: HashMap<ContentId, ClipContent>       (linked clip 共有)
     │    ClipContent = Midi(notes: id 付き) | Audio(events: id 付き)
     │                | Automation(points: id 付き) | Video | Image | Text
     ├ audio/video/image_sources (メタデータ pool、バッファは各プロセスで decode)
     ├ master_fx_chain: Vec<PluginInstance>
     ├ song_lanes (tempo/time-sig automation) / mod_sources / sections
     └ 各種 stable id allocator (next_*_id、0 = 未採番 sentinel)
```

- **Clip ベースのタイムライン** (Pattern 不採用): VOICEVOX のアウフタクト・フレーズ
  単位合成と自然にマップする。`start_beat` は f64・負値可。
- 永続化: `.daw` JSON (serde) アトミック書き込み。現在の版は
  `common/src/model.rs` の `CURRENT_VERSION` が SSoT (**ここに数値を書き写さない** —
  書き写した数値は静かに古くなる。`common/src/model/tests.rs` が実値を assert している)。
  旧版は deserialize 専用 legacy field + `ensure_ids` (採番 + positional → id 写像) +
  JSON 前処理 migration で forward-load する。blob (plugin state / ARA archive) は
  ドキュメントには base64 で残る (wire にだけ載らない)。
- IPC は bincode (`Encode/Decode` derive + PluginInstance のみ手書き)。

## VOICEVOX 統合

HTTP API (`http://localhost:50021`、engine は自動起動 + Job Object 紐付け)。
合成は **plugin_host 内の builtin instrument** が実行する (歌唱 = per-clip 声、
talk = Text clip 由来の読み上げ)。daw_gui は note/talk metadata を
`SetBuiltinPluginNoteMetadata { device_id }` で flush し、builtin が非同期合成 +
WAV cache → RT process() でキャッシュ済み音声を鳴らす。字幕は video 系 builtin
device が gate する。歌唱 wav 先頭の leading rest 分は配置側で補正する。

## 検証

- `make test` (テスト保有 package のみ。**一部の target が daw_gui 本体を subprocess 起動して
  audio device を開く** ので、DAW を開いたまま回さない。`scripts/preflight_no_running_app.sh` が
  止める。起動を伴わない範囲だけなら `make test-nolaunch`) / `make clippy` / `make check`
- **`make arch-lint`**: アーキテクチャ不変条件 (CLAUDE.md 参照) の機械検査
- `daw_gui --smoke-test <fixture.mp4>`: video preview の visual regression
  (build/test/clippy をすり抜ける描画破壊を pixel capture で検出)
- `daw_gui --script <js>` (feature `script`): headless 自動テスト
  (loadSongFile / play / stop / reinitForExport / exportWavRange)

## 参照

| プロジェクト | 参考ポイント |
|---|---|
| sing_like_coding (自作前作) | IPC, CLAP ホスト, オーディオエンジンの原型 (プロト品質 — 構造のみ参考) |
| REAPER VOICEVOX スクリプト (自作) | VOICEVOX API フロー, 歌詞分割, 自動起動 |
| [clap-host (free-audio)](https://github.com/free-audio/clap-host) | CLAP ホストリファレンス (C++) |
| [clack](https://github.com/prokopyl/clack) | Rust 製 CLAP ライブラリ (split-half の先例) |
| [Meadowlark](https://github.com/MeadowlarkDAW/Meadowlark) | Rust 製 DAW, RT オーディオ |
| [nih-plug](https://github.com/robbert-vdh/nih-plug) | Rust 製プラグインフレームワーク |
| Ardour / REAPER / Bitwig manual | DAW 挙動の一次情報 (export freewheel, PDC, folder track, automation) |
