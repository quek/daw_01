# リソースモニター (CPU / FPS / DSP load 表示) — 最終形プラン

r.md #3。DAW 標準のパフォーマンスメーターを daw_01 に実装する。

## ゴール

再生のカクつき・音切れ診断 (オーディオ DSP 負荷) と、アプリ自体の軽さ (FPS / システム
CPU / メモリ) を**同格**で常時監視できるようにする。重い箇所はトラック別・プラグイン別まで
特定できる。

## 確定仕様 (grill-me 2026-06-28)

- DSP 負荷とアプリ負荷を**同格**で常駐。
- **ステータスバー (画面下) 右側に色付き小型メーター常駐** + クリックで**ウィンドウ内
  オーバーレイ詳細パネル** (non-modal、再生継続)。
- 内訳は**トラック別＋プラグイン別**まで。
- デフォルト常駐 on、表示 on/off は `app_config.json` に永続化 (プロジェクト非依存)。

## アーキテクチャ — 計測フローと SSoT

新しい共有メモリ `MetricsBridge` に全計測値を集約 (SSoT)。既存の `AudioBridge` /
`WorkerBridge` と同じ流儀: **daw_gui (親) が `bootstrap.rs` で create、daw_audio と
plugin_host が `open`**。`os_id` は `metrics_bridge_shmem_id(pid)`。受け渡しは
`AudioSession` 構造体に `metrics_shmem_id: String` を 1 本足し、既存の
`MainToChild::Session` で両子に届ける (protocol 変更 → `cargo build --workspace` 必須)。

| 指標 | writer | 計測方法 | reader |
|---|---|---|---|
| DSP load peak (RT) | daw_audio CPAL callback | `処理時間 ÷ (frames/SR)` の直近窓 worst-case。`fetch_max`、GUI poll 時に `swap(0)` でリセット | daw_gui poller |
| DSP load average | daw_audio CPAL callback | callback ローカルで EMA (`a=a*0.9+load*0.1`) → store。plugin 処理は worker pool でブロッキング同期されるため callback 時間に**自然に含まれる** | daw_gui poller |
| xrun / dropout | daw_audio | `load>1.0` 検出で `fetch_add(1)` (monotonic) | daw_gui poller |
| buffer frames / SR | daw_audio | 起動時に 1 回 publish (静的) | daw_gui poller |
| per-plugin CPU (μs) | plugin_host worker | `process()` 前後で `Instant::now()`、`plugin_dsp_us[plugin_id].store(μs)` | daw_gui poller |
| per-track CPU | **daw_gui で集計** | `track_plugin_ids[track].iter().map(|pid| plugin_dsp_us[pid]).sum()` (既存 `plugin_latencies` の合算 idiom と同型) | — |
| System CPU% / Memory | **daw_gui UI 側** | `sysinfo` crate で 3 プロセス (gui/audio/plugin_host) を ~1Hz ポーリング。**DSP load とは別ラベル** | — |
| GUI FPS | **daw_gui runner** | 既存 `dt` (runner.rs:862) の EMA → `fps = 1/dt_ema` | — |

RT 安全性: callback / worker 内は `Instant::now()` (許容) と atomic store のみ。heap /
lock / format! を増やさない。`sysinfo` ポーリングは UI 側の専用スレッドで RT パス外。

## データ構造

### common/src/metrics_bridge.rs (新規)
```rust
pub const MAX_PLUGINS: usize = 512;  // MAX_TRACKS(32) × 妥当な device 数。超過分は track_peaks と同じく silently drop

#[repr(C)]
pub struct MetricsBridge {
    pub dsp_load_peak: AtomicU32,   // f32 bits, RT/worst-case (GUI が swap(0) でリセット)
    pub dsp_load_avg:  AtomicU32,   // f32 bits, EMA
    pub xrun_count:    AtomicU64,   // monotonic
    pub buffer_frames: AtomicU32,   // 静的
    pub sample_rate:   AtomicU32,   // 静的
    _pad: u32,
    pub plugin_dsp_us: [AtomicU32; MAX_PLUGINS],  // per-plugin 直近 process μs
}
```
- `MetricsBridgeHandle::{create, open, bridge}` + setter/getter は `audio_bridge.rs` と同型
  (`f32::to_bits` / `from_bits`、`fetch_max` で peak、`swap` でリセット)。
- 純粋ロジック (テスト対象、別関数に抽出):
  - `dsp_load(elapsed_s, frames, sr) -> f32`
  - `ema(prev, sample, alpha) -> f32`
  - `load_color(load) -> Color` (閾値 緑<0.7 / 黄<0.9 / 赤)
  - `fps_from_dt(dt_ema_s) -> f32`

### common/src/app_config.rs (新規)
`WindowState` と同型 (serde + `load(path)->Option<T>` + `save(path,&T)->Result<()>`)。
```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct AppConfig { pub resource_monitor_enabled: bool }  // Default: true
```
`AppDirs::app_config()` → `<root>\app_config.json` を追加。

### daw_gui AppData 追加フィールド
- `metrics: ResourceMetrics` — poller が埋める集計済みスナップショット (dsp_peak/avg、xrun、
  buffer/sr、`plugin_dsp: HashMap<u32,f32>`、system_cpu、mem_mb、fps)。
- `resource_monitor_enabled: bool` (app_config から復元、トグルで保存)
- `resource_panel_open: bool` (session-only、Esc / 再クリックで閉じる)

## プロセス別の変更

- **common**: 上記 2 ファイル新規 + `app_dirs.rs` 1 行 + `Cargo.toml` に `sysinfo` + `AudioSession`
  に `metrics_shmem_id` 追加。
- **daw_gui/bootstrap.rs**: `MetricsBridgeHandle::create`、`session.metrics_shmem_id` セット、
  `Bootstrap`/`AppData` へ handle を渡す。
- **daw_gui/main.rs**: 既存 playhead poller (30Hz) に MetricsBridge 読み出しを追加 →
  `AppEvent::MetricsTick`。別途 `sysinfo` 専用スレッド (~1Hz) → `AppEvent::SystemMetricsTick`。
- **daw_gui/view/runner.rs**: `dt` の EMA を AppData へ (FPS)。
- **daw_audio**: Session の `metrics_shmem_id` で open。callback で DSP load 計測・publish・xrun。
- **daw_plugin_host/process_server.rs**: open、worker `process()` 前後計測 → `plugin_dsp_us[id]`。

## UI

- **status_bar.rs 右側に常駐メーター**: `DSP 12% │ CPU 8% │ 60fps │ ●0` を色付き
  (DSP/CPU は `load_color`、xrun>0 で赤点灯)。クリックで `resource_panel_open` トグル。
  `resource_monitor_enabled=false` なら非表示。
- **view/resource_monitor.rs (新規・non-modal オーバーレイ)**: `resource_panel_open` が true の
  時だけ画面右側に描画。全体指標 (DSP peak/avg、system CPU、FPS、mem、xrun、buffer/SR/レイテンシ)
  + トラック別バー (展開でプラグイン別、`load_color` で緑黄赤)。Esc / 再クリック / ✕ で閉じる。
  既存 `heavy()` / `push_rect` / `push_text` / `button_at` idiom。可変背景でないので固定パネル色で可。
- **トグル**: View メニュー項目 + ショートカット (機能 on/off = `ToggleResourceMonitor`、永続化)。

## テスト (純粋ロジック中心、UI は実機検証)

`cargo test --workspace` で:
- `dsp_load` / `ema` / `load_color` / `fps_from_dt` のパラメタライズドテスト
- `MetricsBridge` store/load round-trip (f32 bits、`fetch_max`、`swap` リセット)
- per-track 集計 (`track_plugin_ids` × `plugin_dsp_us` の sum) — ヘルパ関数で
- `AppConfig` serde round-trip (`AppDirs::under(tempdir)`)

UI 描画 (status bar / panel の配置・色) は build/test をすり抜けるので**実機目視**で担保。

## 実装順序 (最終形まで一気に)

1. common: `metrics_bridge.rs` + `app_config.rs` + `app_dirs` + `Cargo.toml(sysinfo)` +
   `AudioSession.metrics_shmem_id` → 純粋ロジックのテスト
2. `cargo build --workspace` (protocol 変更のため子プロセスも再生成)
3. daw_audio: 計測・publish
4. daw_plugin_host: per-plugin 計測
5. daw_gui: bootstrap / poller / sysinfo スレッド / runner FPS / AppData / status_bar /
   resource_monitor panel / toggle / 永続化
6. `cargo test --workspace` + `cargo clippy --workspace -- -D warnings` + `cargo build --workspace`
7. /review → 実機検証 → commit
