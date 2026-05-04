# Plan: A3 — WAV 書き出し / mixdown (freewheel offline render)

## Context

### 動機
A2 (旧 audio thread 削除 + audio quality) で `daw_plugin_host::export_wav_offline` と GUI の Export メニュー / `Ctrl+E` shortcut / `action_export_wav` を削除した。 機能消失中なので新機能 (A6 等) より先に **A3 で機能復旧** する (memory: feedback_recovery_priority)。

### ゴール
- File → Export WAV で再び WAV ファイルを書き出せる
- 出力は DAW 内再生と聴感上一致 (track-parallel mixer + plugin process が export でも効く)
- export 中は通常再生を停止 (freewheel)、 完了後通常再生に復帰可能
- CLAP plugin が `clap.render` ext を提供していれば `CLAP_RENDER_OFFLINE` を通知し高品質モードに切替

### 採用方針 (一次情報根拠)
- **freewheel + 同 instance 流用**: Ardour [Export Dialog manual](https://manual.ardour.org/exporting/export-dialog/) の標準動作。再生継続 / plugin 複製は非現実的 (memory: feedback_export_premise)
- **CLAP `clap_plugin_render` ext**: [clap/ext/render.h](https://github.com/free-audio/clap/blob/main/include/clap/ext/render.h)、 clap-sys 0.5 に binding あり (`CLAP_EXT_RENDER`, `CLAP_RENDER_REALTIME=0`, `CLAP_RENDER_OFFLINE=1`, `clap_plugin_render { has_hard_realtime_requirement, set }`)
- **CPAL Stream は止めない**: cpal `StreamTrait::pause()` は platform 依存で WASAPI shared mode で失敗可能性あり ([docs.rs/cpal](https://docs.rs/cpal/latest/cpal/traits/trait.StreamTrait.html))。 安全策として `export_running: AtomicBool` を立てて CPAL callback 内で `process_buffer` を skip + 1 buffer ぶん無音、 CPAL stream 自体は走らせ続ける

---

## 現状 LocalState 分析

[daw_audio/src/engine.rs:121-162](../daw_audio/src/engine.rs) の `LocalState` の各 field を「CPAL 専用」と「export thread と共有が必要」に分類:

| Field | 役割 | 分類 |
|---|---|---|
| `scratch: Vec<TrackScratch>` | per-track 中間 buffer (32 entries pre-alloc) | **CPAL 専用** (export 用は別 instance) |
| `master_l/r: Vec<f32>` | master 出力 buffer | **CPAL 専用** |
| `playing: bool` | Play/Stop edge 検出用 | **CPAL 専用** |
| `cmd_rx: UnboundedReceiver<AudioCommand>` | IPC コマンド受信 | **CPAL 専用** |
| `last_heartbeat_playhead` | debug 用 throttle | **CPAL 専用** |
| `worker_bridge: Option<WorkerBridgeHandle>` | per-worker shmem handle | **共有必要** (export thread の dispatch_and_wait) |
| `worker_syncs: Vec<WorkerSyncRef>` | per-worker event ペア | **共有必要** |
| `plugin_refs: HashMap<u32, PluginRef>` | plugin_id → ProcessData ptr | **共有必要** |
| `slot_to_plugin_id: HashMap<(u32, PluginSlot), u32>` | (track, slot) → plugin_id | **共有必要** |
| `vocal_store: HashMap<u32, Arc<ArcSwapOption<VocalAudio>>>` | per-track 事前合成サンプル | **共有必要** |
| `worker_pool: Option<AudioWorkerPool>` | track 並列 dispatch | **共有必要** |

---

## 設計

### 方針: `EngineShared` 抽出

「共有必要」 6 field を `Arc<EngineShared>` に移管。 CPAL callback と export thread の両方が clone を保持。

```rust
// daw_audio/src/engine.rs
pub struct EngineShared {
    pub worker_bridge: ArcSwapOption<WorkerBridgeHandle>,
    pub worker_syncs: ArcSwap<Vec<WorkerSyncRef>>,
    pub plugin_refs: ArcSwap<HashMap<u32, PluginRef>>,
    pub slot_to_plugin_id: ArcSwap<HashMap<(u32, PluginSlot), u32>>,
    pub vocal_store: ArcSwap<HashMap<u32, Arc<ArcSwapOption<VocalAudio>>>>,
    pub worker_pool: ArcSwapOption<AudioWorkerPool>,
    /// True while export thread holds the audio resources. CPAL
    /// callback skips its `process_buffer` and writes silence so the
    /// export thread can drive `plugin.process()` exclusively.
    pub export_running: AtomicBool,
}
```

ArcSwap で wait-free 共有: RT で `load()` は atomic ptr swap のみ、 mutex 不要。 plugin add/remove で snapshot を新規 HashMap に作って `store()`。

### 縮小後の `LocalState`

```rust
pub struct LocalState {
    pub scratch: Vec<TrackScratch>,
    pub master_l: Vec<f32>,
    pub master_r: Vec<f32>,
    pub playing: bool,
    pub cmd_rx: tokio::sync::mpsc::UnboundedReceiver<AudioCommand>,
    pub shared: Arc<EngineShared>,
    #[cfg(debug_assertions)]
    pub last_heartbeat_playhead: u64,
}
```

`process_buffer` / `process_track_owned` の引数も `Arc<EngineShared>` を参照する形に書き換え。

### export thread の lifecycle

```
GUI File → Export WAV → MainToChild::ExportWav { path }
    ↓ daw_audio recv_loop で受信
    ↓ shared.export_running.store(true)
    ↓ shared.playback.store(Stop) で再生中なら停止
    ↓ std::thread::spawn で export thread 起動
        ↓ export 用 LocalState (scratch + master_l/r) を新規作成
        ↓ playhead = 0 から song の length_beats まで loop
        │   process_track_owned を全 track ぶん呼ぶ (shared から resource 借用)
        │   reduce_master で master 累積
        │   hound::WavWriter で出力 (2ch / 48kHz / f32)
        ↓ writer.finalize()
        ↓ shared.export_running.store(false)
        ↓ ChildToMain::ExportWavComplete を IPC 送信
```

CPAL callback は `shared.export_running` を見て早期 return (silence)。

### CLAP `clap_plugin_render` ext の host → plugin 呼び出し

`LoadedPlugin` trait に `set_render_mode(mode: ClapRenderMode) -> bool` を追加。

```rust
// common (or daw_plugin_host) に
pub enum ClapRenderMode { Realtime, Offline }

// LoadedPlugin trait
fn set_render_mode(&mut self, mode: ClapRenderMode) -> bool;
```

`ClapPlugin` 実装で:
1. `plugin.get_extension(CLAP_EXT_RENDER)` で `*const clap_plugin_render` 取得
2. `*set` callback を呼ぶ
3. `Vst3Plugin` は no-op (M1 では VST3 未対応)

export thread は開始時に全 plugin に `set_render_mode(Offline)`、 完了で `Realtime`。

ただし、 export thread は plugin instance pointer に直接アクセスしないので、 plugin_host に IPC で「全 plugin に set_render_mode(Offline)」 を要求する必要がある。 IPC 追加:

```rust
// MainToChild
SetRenderMode(ClapRenderMode),
```

plugin_host 側 plugin_main_loop で受信 → 全 chain の plugin に `set_render_mode` を呼ぶ。

完了後 `SetRenderMode(Realtime)` を送る。

### IPC protocol 復活 ([common/src/protocol.rs](../common/src/protocol.rs))

```rust
// MainToChild に追加
ExportWav { path: PathBuf },
SetRenderMode(ClapRenderMode),  // (新規)

// ChildToMain に追加
ExportWavComplete { error: Option<String> },
```

`ClapRenderMode` も bincode derive 必要 (CLAUDE.md 指針)。

### GUI 復活

[daw_gui/src/view/root.rs](../daw_gui/src/view/root.rs) の File menu に "Export WAV..." 追加 + `Ctrl+E` shortcut。
[daw_gui/src/app.rs](../daw_gui/src/app.rs) で `AppEvent::ExportWav` / `ExportWavComplete` 復活、 `action_export_wav` で `send_audio(MainToChild::ExportWav { path })`。
status bar に「Export 中」 表示 + 完了メッセージ。

---

## 段階実装ステップ (5 PR)

| PR | 内容 | 検証 |
|---|---|---|
| **PR1** | `EngineShared` 抽出: `LocalState` から 6 field を `Arc<EngineShared>` に移管、 `process_buffer` / `process_track_owned` のシグネチャを書き換え。 機能変化なし | `cargo build / clippy / test` clean、 ユーザー smoke test (再生 / mute / solo / volume が依然動く) |
| **PR2** | `LoadedPlugin::set_render_mode` 追加、 `ClapPlugin` で `CLAP_EXT_RENDER` 取得 + `set` 呼び出し。 `Vst3Plugin` は no-op | unit test (set_render_mode が plugin に届くか mock で確認) |
| **PR3** | protocol 拡張: `MainToChild::ExportWav` / `SetRenderMode` 再追加、 `ChildToMain::ExportWavComplete` 再追加、 plugin_host 側 `PluginCommand::SetRenderMode` handler | `cargo build` |
| **PR4** | `daw_audio/src/export.rs` 新規作成: export thread + freewheel render loop + WavWriter。 daw_audio recv_loop で `MainToChild::ExportWav` 受信 → SetRenderMode(Offline) を plugin_host に送信 → export thread spawn → 完了で SetRenderMode(Realtime) + ExportWavComplete | smoke test: 1 track / Surge XT で WAV 出力、 DAW 内再生と聴感比較 |
| **PR5** | GUI 復活: File menu / `Ctrl+E` / `action_export_wav` / status bar、 `AppEvent::ExportWav` / `ExportWavComplete` 復活 | full smoke test: 2 track + Vocal track の同時 export、 export 中の Play 押下が reject されること |

各 PR ごとに `cargo build / clippy / test` clean を確認してから次へ。

---

## ファイル変更

### 新規
- `daw_audio/src/export.rs` — offline render loop + WAV writer

### 変更
- `daw_audio/src/engine.rs` — `EngineShared` 抽出、 `LocalState` 縮小、 `process_buffer` / `process_track_owned` 書き換え
- `daw_audio/src/main.rs` — `MainToChild::ExportWav` 受信、 export thread spawn
- `daw_audio/Cargo.toml` — `hound` 追加
- `daw_plugin_host/Cargo.toml` — (戻さない、 export は audio 側のみ)
- `daw_plugin_host/src/main.rs` — `PluginCommand::SetRenderMode` handler、 全 chain plugin に `set_render_mode` 呼び出し
- `daw_plugin_host/src/plugin_instance.rs` — `LoadedPlugin::set_render_mode` trait method
- `daw_plugin_host/src/clap_plugin.rs` — `CLAP_EXT_RENDER` 取得 + `set` 呼び出し
- `daw_plugin_host/src/vst3_plugin.rs` — no-op 実装
- `common/src/protocol.rs` — `MainToChild::ExportWav` / `SetRenderMode` / `ChildToMain::ExportWavComplete` 再追加、 `ClapRenderMode` enum (bincode derive)
- `daw_gui/src/app.rs` — `AppEvent::ExportWav` / `ExportWavComplete` 復活、 `action_export_wav`
- `daw_gui/src/view/root.rs` — File menu / `Ctrl+E` shortcut
- `daw_gui/src/main.rs` — `ChildToMain::ExportWavComplete` arm 復活

---

## 検証

### 静的
- `cargo build --workspace`
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`
- `cargo test --workspace --features daw_audio/rt-assert`

### 動的 (`cargo run -p daw_gui` で実機 smoke test)

1. **基本 export**: 1 track / Surge XT で C-4 を 4 拍鳴らす song → File → Export WAV → wav 書き出し → 別アプリで再生して DAW 内再生と一致
2. **multi-track export**: 2-3 track + 別 instrument → 各 track が混ざった master が WAV に
3. **mixer 反映**: track の volume / pan / mute / solo が export 結果に反映
4. **Vocal track**: VOICEVOX cache 経由 (or `SetVocalAudio`) で WAV 直挿し track → export に含まれる
5. **export 中 Play 押下**: `export_running` 中は Play コマンドを reject (status bar に「Export 中」)
6. **export 完了後**: 通常再生に復帰可能、 plugin instance の挙動が崩れていない (= SetRenderMode(Realtime) が効いている)
7. **失敗系**: hound::WavWriter が path を作成失敗したら ExportWavComplete に error 文字列、 status bar に表示

---

## リスク / 想定外への対応

1. **`cpal::Stream::pause()` を使わない判断**: WASAPI shared mode で `pause()` が失敗する可能性があるので、 export 中も CPAL callback は走らせ続け、 callback 内で `export_running` フラグを見て早期 return + 無音書き込み。 デバイス re-init コストも回避
2. **plugin の `has_hard_realtime_requirement()` が true (hardware proxy)**: M1 では考慮外、 warn ログのみ。 M2 で realtime export モード追加検討
3. **plugin の `clap_plugin_render` 未実装**: `get_extension(CLAP_EXT_RENDER)` が null の場合は no-op、 plugin は realtime 設定で動く (品質低下の可能性ありだが動作はする)
4. **export 中に GUI 操作で plugin add/remove**: `plugin_refs` / `slot_to_plugin_id` の ArcSwap 更新は wait-free だが、 export thread が古い snapshot で render すると新 plugin が抜ける。 解決策: export 中は GUI 側で plugin 操作を disable (Plan 範囲外、 別 PR で UI ガード)
5. **WAV 書き出し中のエラー処理**: hound 例外を Result で受け、 ExportWavComplete に error 文字列、 partial file は finalize 失敗時に削除
6. **大規模 EngineShared 抽出のリグレッション**: PR1 単独で merge して smoke test、 問題があれば revert

---

## 参照

- master plan: [docs/plan.md](plan.md) A3 セクション
- A2 完了 commit: d7a7575
- Ardour Export Dialog manual: https://manual.ardour.org/exporting/export-dialog/
- CLAP render ext: https://github.com/free-audio/clap/blob/main/include/clap/ext/render.h
- clap-sys: `clap_sys::ext::render::{CLAP_EXT_RENDER, CLAP_RENDER_REALTIME, CLAP_RENDER_OFFLINE, clap_plugin_render}`
- cpal Stream: https://docs.rs/cpal/latest/cpal/traits/trait.StreamTrait.html
- 関連 memory: feedback_export_premise, feedback_recovery_priority, feedback_research_responsibility
