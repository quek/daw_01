# Plan: A2 — daw_audio へ責務移管 + track-parallel スレッドプール化

## Context

### 動機
A2 の本質は 2 つ:

1. **責務分担の正常化**: 現状 `daw_plugin_host` が **シーケンサー + mixer + WAV 再生 + master mix** まで全部抱えている。これは [DESIGN.md:14-16](../DESIGN.md) の規定 (`daw_audio` = オーディオ出力 / シーケンサー / ミキサー、`daw_plugin_host` = プラグインのロード / 実行) からの逸脱。前作 [sing_like_coding](file:///F:/dev/sing_like_coding) は正しい責務分担で、参照実装にする
2. **track-parallel 化**: 各 track の処理をスレッドプールで並列化。重い CLAP プラグインを多 track 載せても WASAPI buffer deadline (~10 ms @ 48k / 1024 frames) を超えない

ユーザー方針: **「plugin_host は本当にプラグインのホスト。WAV の再生などをプラグインホストが担うのはおかしい。実装コストは無視してベストを選ぶ」**

### 現状の責務逸脱の網羅 (daw_plugin_host から daw_audio へ移管)

| 機能 | 現状の場所 | 行 |
|---|---|---|
| `run_audio()` audio thread 全体 | [daw_plugin_host/src/main.rs](../daw_plugin_host/src/main.rs) | 1556-1918 |
| `collect_events_for_buffer()` (Song walk → MIDI event) | (同上) | 1929-2017 |
| `export_wav_offline()` (offline render) | (同上) | 2026- |
| `Tracks` / `TrackRouting` / `AudioRouting` | (同上) | 200- |
| `PerTrackState` (active_notes / pending_offs) | (同上) | 1546-1554 |
| `VocalAudio` (Vocal track の WAV 再生) | (同上) | 1148-1154, 1729-1747 |
| `TrackAudioParams` (volume/pan/muted/solo atomics) | (同上) | 208-253 |
| `playhead: u64` + `song_store: ArcSwapOption<Song>` | (同上) | 362, 1564, 1614-1845 |
| mixer (Equal-power pan + master accumulate + peak) | (同上) | 1791-1819 |
| shmem master writer (interleaved stereo 書き込み) | (同上) | 1889-1896 |

### plugin_host に残す機能

- CLAP / VST3 の scan / load / unload
- `plugin.activate` / `deactivate` / `start_processing` / `stop_processing`
- `plugin.process()` の **呼び出し本体** (audio engine からの handshake 経由で)
- CLAP GUI lifecycle (create / show / hide / destroy)
- Plugin state save / restore
- IPC gateway (MainToChild ↔ 内部 PluginCommand)

### CLAP spec の根拠
`include/clap/ext/thread-check.h`:
> "The audio-thread is symbolic, there isn't one OS thread that remains the audio-thread for the plugin lifetime. ... single plugin instance will not be two audio-threads at the same time."

→ 異なる plugin instance を別 OS thread で並列 `process()` 呼び出し可。同じ plugin instance への同時呼び出しは禁止 — daw_01 では **track 単位で dispatch + track 内 chain は serial** で保証する。

### 反面教師
[sing_like_coding/src/singer.rs:328-343](file:///F:/dev/sing_like_coding/sing_like_coding/src/singer.rs) は rayon + `Mutex<ProcessTrackContext>` + 毎 buffer `Vec<Vec<_>>` 確保 = RT 違反。daw_01 では **固定 worker + atomic-wait + per-track scratch** で alloc / lock ゼロを徹底する。

---

## 改修後の責務分担

```
┌─────────┐     control IPC (named pipe + bincode)
│ daw_gui │ ─── Play / Stop / LoadSong / SetTrackVolume / ... ───────┐
└─────────┘                                                          │
     │                                                               ▼
     │  ┌───────────────────────────────────────────────┐    ┌──────────────────┐
     └─▶│ daw_audio                                     │    │ daw_plugin_host  │
        │ ─ CPAL device output (WASAPI)                 │    │ ─ CLAP scan/load │
        │ ─ Sequencer (Song walk → MIDI events)         │    │ ─ plugin.process │
        │ ─ Mixer (per-track buf + vol/pan/mute/solo)   │◀──▶│ ─ plugin GUI     │
        │ ─ Master mix + shmem master writer            │    │ ─ state save     │
        │ ─ Vocal WAV 再生                              │    │ ─ IPC gateway    │
        │ ─ N worker thread pool (track 並列)           │    │ ─ N worker pool  │
        │ ─ Per-track peak meter                        │    │   (1:1 ペア)     │
        │ ─ Loop / playhead / autoStop                  │    │                  │
        │ ─ WAV export (offline render)                 │    │                  │
        └───────────────────────────────────────────────┘    └──────────────────┘
                       data plane (shared memory + Win32 Event):
                       per-plugin ProcessData shmem + per-worker event pair
```

---

## スレッドプール設計 (両プロセス N worker, 1:1 ペア)

```
audio engine 側:  N worker pool (N = available_parallelism - 1)
                  └─ track 単位で dispatch (work-stealing counter)
                  └─ 1 worker = 1 track の chain を serial に処理 (mfx → inst → fx)
                  └─ chain 内の各 plugin process は plugin_host worker[i] に依頼

plugin_host 側:   N worker pool (audio engine と同じ N)
                  └─ worker[i] は audio engine worker[i] と 1:1 ペア
                  └─ 共有 shmem WorkerBridge::worker_task[i] で plugin_id を受信
                  └─ plugin.process() を呼んで done event を signal
```

### 1 plugin process の流れ
```
audio engine worker[i] (track の chain 中):
  1. plugin_data[plugin_id].process_data に input (events_in / buffer_in / frames) を書く
  2. worker_bridge.worker_task[i].store(plugin_id, Release)
  3. SetEvent(worker_wake[i])
  4. WaitForSingleObject(worker_done[i], INFINITE)
  5. plugin_data[plugin_id].process_data の output (events_out / buffer_out) を読む

plugin_host worker[i]:
  loop {
    WaitForSingleObject(worker_wake[i], INFINITE)
    if shutdown { break }
    let plugin_id = worker_bridge.worker_task[i].load(Acquire);
    plugins[plugin_id].process(&plugin_data[plugin_id])
    SetEvent(worker_done[i])
  }
```

### この設計の利点
- **thread 数固定** (= N、CPU コア数程度)。plugin instance が何個あっても増えない (per-plugin thread 案ではなくスレッドプール)
- **CLAP spec 適合**: 同じ plugin instance に同時 process しない (track 単位 dispatch + chain serial で保証)
- **shmem は per-plugin** (`ProcessData` 配列) で track 並列性を確保
- **event handle は per-worker** (= N ペアのみ) で OS リソース節約 (per-plugin event なら 100 plugin で 200 handle)
- **CLAP `is_audio_thread` の正しい応答** が容易: worker thread の TLS フラグだけ見れば良い (per-plugin thread 案では plugin 数だけ TLS を仕込む必要があった)

---

## 採用する技術

| 技術 | 用途 |
|---|---|
| `atomic-wait` crate | Win `WaitOnAddress` / Linux futex 抽象。audio engine worker barrier の wait/wake |
| `thread-priority` crate | worker を `THREAD_PRIORITY_TIME_CRITICAL` |
| Windows `AvSetMmThreadCharacteristicsW("Pro Audio")` | MMCSS による I/O priority boost |
| `assert_no_alloc` (`[features] rt-assert`) | debug ビルドで RT 違反検出 |
| `arc-swap` | Song snapshot の wait-free publish |
| `shared_memory 0.12` (既存) + Win32 Event | per-plugin shmem + per-worker event |
| `AtomicU32 next_track` | audio engine 側 work-stealing counter |
| CLAP `thread_check` ext | worker TLS の `IS_AUDIO_THREAD` フラグ参照 |

---

## 新しい IPC layout

### Control plane (named pipe + bincode、既存を拡張)
- daw_gui ↔ daw_audio: 既存 `MainToAudio` / `AudioToMain` (Play/Stop, LoadSong, SetTrackVolume 等)
- daw_audio ↔ daw_plugin_host: 既存 `MainToChild` / `ChildToMain` を拡張
  - 追加: `OpenWorkerPool { n_workers, worker_bridge_shmem_id, wake_event_names, done_event_names }` — audio engine 起動時に 1 回。N 個の worker を plugin_host 側に立ち上げさせる
  - 追加: `OpenPluginShmem { plugin_id, process_data_shmem_id }` — plugin instance ごと
  - 追加: `ClosePluginShmem { plugin_id }`
  - 削除: `LoadSong`, `SetTrackVolume/Pan/Muted/Solo`, `SetVocalAudio` (これらは audio engine の責務になるので daw_gui ↔ daw_audio に集約)

### Data plane

#### Per-plugin shmem (`common/src/process_data.rs`)
```rust
#[repr(C)]
pub struct ProcessData {
    pub frames: u32,
    pub steady_time: u64,
    pub sample_rate: u32,
    pub playing: u8,
    pub n_events_in: u32,
    pub events_in: [Event; MAX_EVENTS],
    pub n_events_out: u32,
    pub events_out: [Event; MAX_EVENTS],
    pub buffer_in: [[f32; MAX_FRAMES]; MAX_CHANNELS],
    pub buffer_out: [[f32; MAX_FRAMES]; MAX_CHANNELS],
}
```

定数: `MAX_EVENTS = 256`, `MAX_FRAMES = 1024`, `MAX_CHANNELS = 2`。
sizeof(ProcessData) ≈ 16 KB / plugin instance。

#### Per-worker shmem (`common/src/worker_bridge.rs`)
```rust
#[repr(C)]
pub struct WorkerBridge {
    /// audio engine worker[i] が plugin_host worker[i] に伝える plugin id.
    pub worker_task: [AtomicU32; MAX_WORKERS],
}
```
`MAX_WORKERS = 32` (CPU コア数の事実上の上限。env で可変、上限超えたら panic)。

#### Per-worker event pair (Win32 named events)
- `daw_01_worker_wake_{i}` : audio engine → plugin_host
- `daw_01_worker_done_{i}` : plugin_host → audio engine

両方 auto-reset。i ∈ 0..N。

### shmem master output (既存維持、所有を変更)
- `common/src/audio_bridge.rs` の `AudioBridge` (master interleaved stereo + per-track peak meter) はそのまま、ただし **writer が daw_plugin_host から daw_audio に変わる**

---

## ファイル変更

### 新規

#### `daw_audio/src/`
- `sequencer.rs` — `collect_events_for_buffer()`, `effective_loop_bounds()`, `song_ended()`, `playhead` 進行
- `mixer.rs` — `TrackScratch` / per-track buffer + Equal-power pan + master accumulator + peak
- `worker_pool.rs` — `WorkerPool` (atomic-wait barrier + work-stealing)。worker は `WorkerSyncRef` を経由して plugin_host を起こす
- `tracks.rs` — `Tracks`, `TrackRouting`, `AudioRouting` (daw_plugin_host から移管 + per-plugin `PluginRef` 格納)
- `vocal.rs` — `VocalAudio` 構造体と sample-based playback
- `engine.rs` — `run_audio()` 全体 (master スレッド = clap-audio 相当を daw_audio に移植)
- `plugin_client.rs` — per-plugin shmem 確保 + `OpenPluginShmem` IPC、`OpenWorkerPool` IPC
- `export.rs` — `export_wav_offline()` 移管

#### `daw_plugin_host/src/`
- `process_server.rs` — N worker pool (audio engine と 1:1 ペア)、各 worker は wake event を待ち、worker_task に書かれた plugin_id の `plugin.process()` を呼ぶ

#### `common/src/`
- `process_data.rs` — `ProcessData` shmem layout + `Event` enum (NoteOn/NoteOff/Param)
- `track_params.rs` — `TrackAudioParams` (atomic packed) を common に
- `plugin_ref.rs` — `PluginRef` (per-plugin shmem ハンドル) と `WorkerSyncRef` (per-worker event ペア + worker_task atomic ポインタ)
- `worker_bridge.rs` — `WorkerBridge` shmem layout (worker_task array)

### 既存変更

- `daw_audio/src/main.rs` — CPAL callback で `engine::run_audio()` を駆動 (現在の dummy 読み出しを削除)
- `daw_plugin_host/src/main.rs` — `run_audio()` / `collect_events_for_buffer()` / `Tracks` / `VocalAudio` / `TrackAudioParams` / `export_wav_offline()` を全削除。`process_server` を起動するだけのスケルトンに
- `common/src/protocol.rs` — `MainToAudio` / `AudioToMain` 拡張、`MainToChild` から不要メッセージ削除、`OpenWorkerPool` / `OpenPluginShmem` / `ClosePluginShmem` 追加
- `daw_gui/src/communicator.rs` — `LoadSong` / `SetTrackVolume` 等を `daw_plugin_host` ではなく `daw_audio` に送るよう接続変更
- `daw_audio/Cargo.toml` — `atomic-wait`, `thread-priority`, `assert_no_alloc` (optional), `arc-swap` 追加
- `daw_plugin_host/Cargo.toml` — `arc-swap` / `hound` 削除可、handshake 用に `windows` の Event API feature 確認、`thread-priority` 追加
- `Cargo.toml` (workspace) — 上記 crate 追加
- `docs/plan.html` — A2 セクション全面書き換え + 進捗ログ追記

---

## activate / process / deactivate フロー

### activate (`daw_audio` 側)
1. `n_workers = available_parallelism()? - 1`、`max(1, n)`、env `DAW_AUDIO_WORKERS` で override
2. `Box<[TrackScratch; n_tracks]>` 確保 + 各 `Vec` capacity を pre-fill
3. WorkerBridge shmem 作成 (`worker_task[N]` を 0 初期化)
4. N 個の wake / done event を `CreateEventA` で作成
5. `MainToChild::OpenWorkerPool { n_workers, worker_bridge_shmem_id, wake_event_names, done_event_names }` を plugin_host に送信
6. plugin_host 側で N worker spawn、各 worker が `OpenEventA` で wake/done を開き、WorkerBridge shmem を `OpenShared` で開く
7. plugin instance ごとに ProcessData shmem を確保 → `OpenPluginShmem { plugin_id, shmem_id }` を plugin_host に送信
8. audio engine N worker spawn (master clap-audio thread + N worker)。各 worker: `set_thread_priority(TIME_CRITICAL)` + `AvSetMmThreadCharacteristicsW("Pro Audio")` (失敗時 warn) + TLS `IS_AUDIO_THREAD = true`
9. plugin_host worker も同じ priority + MMCSS 設定

### process (per-buffer、master スレッド = `daw_audio` の audio thread)

```
master (clap-audio thread on daw_audio side):
  request_sem.wait (CPAL から)
  shutdown? → break
  Play/Stop transition (PerTrackState clear / pending_offs queue)
  song = song_store.load()
  any_solo, frames, playhead を DispatchShared に Release store
  next_track.store(0, Release)
  pending.store(n_workers, Release)
  for w in workers: w.wake.store(1); atomic_wait::wake_one(&w.wake)
  // master も work-stealing 参加
  loop {
    let idx = next_track.fetch_add(1, AcqRel);
    if idx >= n_tracks { break }
    process_track(&routing.tracks[idx], &mut scratch[idx], &shared, master_sync_ref);
  }
  while pending.load(Acquire) != 0 {
    atomic_wait::wait(&pending, current_observed);
  }
  // serial reduce: master += scratch[*].track_{l,r}
  // bridge.set_track_peak(...) per track
  // shmem master writer (interleaved stereo)
  ready_sem.release (CPAL へ)
```

### `process_track` (= 現 [main.rs:1676-1819](../daw_plugin_host/src/main.rs) の中身を移植)

```
process_track(tr, scratch, shared, worker_sync_ref):
  scratch.midi_bus_a.clear()
  push pending_offs as NoteOff @ frame 0
  collect_events_for_buffer(song, tr.track_id, ..., scratch.midi_bus_a, scratch.state.active_notes)

  for mfx_ref in tr.midi_fx_chain:
    write events_in to mfx_ref.process_data
    worker_sync_ref.dispatch(mfx_ref.plugin_id)   // plugin_host worker に依頼 → wait
    read events_out from mfx_ref.process_data → midi_bus_b
    sort by time, swap a/b

  scratch.track_l/r.fill(0.0)

  if tr.instrument.is_none() && playing && tr.vocal.load().is_some():
    sample-based playback (現 1729-1747 の移植)

  if let Some(inst_ref) = tr.instrument:
    write events_in / buffer_in to inst_ref.process_data
    worker_sync_ref.dispatch(inst_ref.plugin_id)
    read buffer_out → scratch.track_l/r

  for fx_ref in tr.fx_chain:
    write events_in / buffer_in (= scratch.track_l/r) to fx_ref.process_data
    worker_sync_ref.dispatch(fx_ref.plugin_id)
    read buffer_out → scratch.track_l/r

  apply volume / pan (Equal-power) / mute / solo
  scratch.peak_l/r = max(|track_l/r|)
```

### plugin_host 側 (`process_server`、N worker)

```
worker[i] thread loop:
  loop {
    WaitForSingleObject(worker_wake[i], INFINITE)
    if shutdown.load(Acquire) { break }
    let plugin_id = worker_bridge.worker_task[i].load(Acquire);
    let plugin = plugins.get(plugin_id).expect(...);
    plugin.process(&plugin_shmem[plugin_id].process_data)
    SetEvent(worker_done[i])
  }
```

worker thread の priority は `TIME_CRITICAL` + MMCSS。各 worker は spawn 時に named event を `OpenEventA` で開く。

### deactivate
1. audio engine 側: shutdown.store(true) で audio loop 抜け、master が `audio_worker.wake = SHUTDOWN; wake_one` を全 worker に
2. audio engine N worker join
3. plugin_host に `MainToChild::CloseWorkerPool` を送信
4. plugin_host 側: shutdown.store(true) → 各 worker_wake[i] を SetEvent → worker は shutdown フラグを見て break → join
5. plugin shmem / event handle / WorkerBridge shmem 解放

---

## RT 安全性チェックリスト

| 危険箇所 | 対策 |
|---|---|
| master accumulation の同時書込 | per-track scratch に書き、master が wait 後に serial reduce |
| `tracing::info!` を worker から呼ぶ | release では出さない、debug でも heartbeat 1Hz、master が集計 |
| atomic-wait の wake-loss | `wait(addr, expected)` は現在値 != expected で即 return、ループで再 check |
| TrackScratch arena の alloc | activate 時のみ |
| handshake の `WaitForSingleObject(INFINITE)` が deadline 超過 | plugin_host worker が priority 最大、buffer 拡大で対応 (M2 課題) |
| MMCSS handle leak | `Drop` で `AvRevertMmThreadCharacteristics(handle)` 確実呼び |
| `shutdown` 中のデッドロック | `Drop for WorkerPool` で `pending.store(0); wake_all` 保証 |
| worker_task の plugin_id が間違って同 plugin に同時 dispatch | 「track 単位 dispatch + chain serial」で audio engine 側がそもそも同 plugin を 2 worker に同時 assign しない (track が異なれば plugin も異なる前提) |

debug feature `rt-assert` で worker body を `assert_no_alloc!` で wrap、CI で `cargo test --features rt-assert` を回す。

---

## 段階実装ステップ (8 PR)

| PR | 内容 | 検証 |
|---|---|---|
| **PR1** | `common/src/track_params.rs` / `process_data.rs` / `plugin_ref.rs` / `worker_bridge.rs` 新規作成、型定義のみ追加 (まだ誰も使わない) | `cargo build / clippy` clean |
| **PR2** | `daw_audio/src/sequencer.rs` / `vocal.rs` / `mixer.rs` / `tracks.rs` を新規作成、`collect_events_for_buffer` / `process_track` 関数を移植 (使用は次 PR) | コンパイル + テスト |
| **PR3** | `daw_audio/src/engine.rs` で `run_audio()` を実装 (まだ plugin handshake は stub、無音でも動く) + CPAL callback から呼び出し | `cargo run -p daw_gui` で無音再生確認 |
| **PR4** | `daw_plugin_host` 側に `process_server.rs` (N worker pool + worker_wake/done event) 追加、`OpenWorkerPool` / `OpenPluginShmem` IPC ハンドリング | unit test で event 経由 process が走ること |
| **PR5** | `daw_audio/src/plugin_client.rs` で worker_sync 実装、`process_track` 内で実際に plugin.process() を呼ぶ。serial 動作 (audio engine の worker pool 未使用) | 1 track / 1 instrument で音が出る |
| **PR6** | `daw_audio/src/worker_pool.rs` を実装、`run_audio` から `pool.dispatch_and_wait()` を呼ぶ。N audio worker × N plugin_host worker の 1:1 ペアで track 並列化 | 2-4 track 同時鳴動、CPU 使用率測定 |
| **PR7** | thread priority + MMCSS + CLAP `thread_check` ext (host 側) | Surge XT / VCV Rack で warning 無し、underrun 減少 |
| **PR8** | `daw_plugin_host/src/main.rs` から `run_audio()` / `collect_events_for_buffer()` / `Tracks` / `VocalAudio` / `TrackAudioParams` / `export_wav_offline()` 全削除 (= 旧コード除去)。`assert_no_alloc` feature 追加。`docs/plan.html` 書き換え | full regression、release ビルドで underrun 無し |

各 PR は revert 可能、smoke test を経てから次へ。

---

## 検証手順

### 静的
- `cargo build --workspace`
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- `cargo test --workspace --features rt-assert` (PR8 以降)

### 動的 (`cargo run -p daw_gui` 必須)
1. **regression**: 1 track / Surge XT で C-4 発音 (各 PR ごとに必ず)
2. **2 track 同時鳴動**: 別 instrument を 2 track にロード、両方鳴る
3. **3-4 track + 重い FX**: VCV Rack 2 + reverb 4 段、CPU 使用率 + underrun 監視
4. **mute / solo / volume 即応**: GUI 操作が即音に反映
5. **track 増減**: AddTrack / RemoveTrack 連打、次の Play で正常
6. **Vocal track 再生**: VOICEVOX cache 経由 WAV を Vocal track にセット → 再生で音
7. **WAV export**: File → Export WAV、書き出した WAV が DAW 内再生と一致
8. **stress**: 5 分連続再生 / 停止 / track 切替、`tracing::warn!` 無し、worker join デッドロック無し
9. **GUI 終了**: ウィンドウ閉じる → JobObject 経由 全プロセス終了、handle leak 無し

### 性能
- 1 track baseline 比、4 track 並列で latency 1.2x 以下が目標
- bridge に `glitch_count: AtomicU32` を追加し 1Hz heartbeat (debug only)

---

## リスク

1. **handshake の latency**: SetEvent / WaitForSingleObject は μs オーダーだが、4-8 plugin の serial chain だと積み上がる。**N audio worker から並列に異なる track の chain を進める** ことで track 間の latency は隠れる
2. **WASAPI 共有モード buffer deadline**: ~10ms。MMCSS でも限界、buffer サイズ UI を 1024+ で対応 (M1 ではユーザー UI 未実装、M2 課題)
3. **CLAP プラグイン内部 main-thread sync**: 一部プラグインが `process()` で global mutex を取る → thread_check ext で warning ログを出してユーザーに知らせる
4. **shmem サイズ膨張**: `ProcessData` ≈ 16KB × N plugin。100 plugin で 1.6MB → OK
5. **worker 数固定の上限**: `MAX_WORKERS = 32` で固定。env で `DAW_AUDIO_WORKERS=64` 等を指定すると panic で起動失敗 (=設計外、M2 で対応)
6. **handle leak**: shmem / event handle を Drop で確実に閉じる (`HANDLE` を `OwnedHandle` でラップ)
7. **段階実装中の二重実装**: PR3-5 中は daw_plugin_host 側にも旧 audio thread が残っているが「使われない (= dummy buffer 返す)」状態。完全削除は PR8。この期間は二重実装で混乱しないよう、新 audio thread が active のときは旧側を起動しないフラグを入れる
8. **既存 protocol との後方互換**: `MainToChild::LoadSong` 等は破棄、daw_gui ↔ daw_audio に移すので protocol は変わる。.daw ファイル形式 (JSON) は変わらない (Song struct は common にあるので影響なし)

---

## 参照

- DESIGN.md (責務分担の元規定)
- 現状コード: [daw_plugin_host/src/main.rs:1556-2260](../daw_plugin_host/src/main.rs)
- 参照実装: [F:/dev/sing_like_coding/sing_like_coding/src/singer.rs](file:///F:/dev/sing_like_coding/sing_like_coding/src/singer.rs), [common/src/process_data.rs](file:///F:/dev/sing_like_coding/common/src/process_data.rs), [common/src/plugin_ref.rs](file:///F:/dev/sing_like_coding/common/src/plugin_ref.rs)
- 反面教師: rayon + Mutex + 毎 buffer alloc ([sing_like_coding singer.rs:328-343](file:///F:/dev/sing_like_coding/sing_like_coding/src/singer.rs))
- CLAP spec: `clap-sys` crate / `include/clap/ext/thread-check.h`
- atomic-wait crate: https://crates.io/crates/atomic-wait
- thread-priority crate: https://crates.io/crates/thread-priority
- assert_no_alloc crate: https://crates.io/crates/assert_no_alloc
