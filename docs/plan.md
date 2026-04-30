# Rust 製・モデルを Clone しない DAW 向け GUI ライブラリ — 設計計画 (波形表示 UI を M2 に前倒し)

## Context

- **目的**: Rust で DAW (Digital Audio Workstation) のシェル UI を書くための **GUI ライブラリ** をゼロから設計・実装する。
- **本ファイルの位置付け**: 旧計画 (`F:\dev\work\` 用 `rust-clone-daw-gui-zazzy-kazoo.md`) を `F:\dev\gui_01` 用に移植し、現状コード (M1 完了) に合わせて更新したもの。
- **このリビジョンの主たる変更**:
  1. **波形表示 UI を M2 に前倒し**。理由: ユーザの判断「波形が一番重いので早期検証したい」。波形が想定通り動くことを早期確定させ、後続の scenegraph / heavy() の前提を固める。
  2. 旧 M2 (Ui<'a> 充実 + 基本ウィジェット) → M3 にスライド。
  3. **プランの正本を `F:\dev\gui_01\docs\plan.md` に置く**。`~/.claude/plans/` のハッシュ命名はディレクトリリネームで紐付けが切れるため、git 管理下に正本を置いて消失を防ぐ。
- **スコープは GUI のみ**: オーディオは別 exe (別プロセス) として走り、本ライブラリは関与しない。
- **核となる制約 = 「モデルを Clone しない」**:
  1. MIDI クリップ・サンプル・オートメーションなど大きなデータを毎フレーム複製しない
  2. アプリ側がドメイン型を IPC で audio exe に伝送する際の都合 (zero-copy / 部分 snapshot) を library が阻害しない
  3. iced の `Application::Message: Clone` のような型境界をユーザのドメイン型に伝染させない
- **ライブラリの責務**: `&GuiModel` を借りて UI を描画 / 入力からエディット (`Edit<M>`) を収集
- **ライブラリの非責務**: GUI Model の所有・apply・undo/redo / audio exe との IPC・SHM / audio Model の設計

---

## プランの保管場所 (新方針)

| 場所 | 役割 | 同期 |
|---|---|---|
| `F:\dev\gui_01\docs\plan.md` | **正本 (canonical)**, git 管理, リネームで消えない | 編集はこちらが第一 |
| `~/.claude/plans/ui-hashed-stearns.md` | Claude のプランモード作業領域、自動再開用 | 正本からコピーで追従 (手動同期) |

ExitPlanMode 後の最初のアクションで `~/.claude/plans/ui-hashed-stearns.md` の内容を `F:\dev\gui_01\docs\plan.md` にコピーし、git にコミット可能な状態にする。以後のプラン更新は `docs/plan.md` を編集対象とする。

---

## アーキテクチャ要旨 (Hybrid: 即時モード API + 内部 scenegraph + heavy() 脱出口)

DAW UI の二極性 (静的密集型 vs 巨大可変型) をどちらも捌くため:

- 公開 API は **即時モード** に統一 (`derive` マクロ・Lens 不要、`Application::Message: Clone` 伝染なし)
- 内部実装で **scenegraph + input hash** により静的 UI の再描画コストを削減 (M4)
- `ui.heavy(id, |hctx| ...)` 脱出口でピアノロール・タイムライン・大量波形群などは retained-mode 風に最適化 (M5)

```rust
// アプリループ
loop {
    let edits = lib.frame(&gui_model, |m, ui| {
        ui.fader("master", m.master_volume, |v| {
            Edit::mutate(move |m: &mut GuiModel| m.master_volume = v)
        });
        ui.heavy("piano_roll", |hctx| { /* 巨大ビュー */ });
    });
    for e in edits { e.apply(&mut gui_model); }
    app.notify_audio_process(&gui_model);
}
```

**ライフタイム不変条件**: `Ui<'a>` の `'a` は `&GuiModel` の借用と一致。`Edit<M>` は同寿命でフレーム内消費。GAT 不要。

**「Clone しない」をどう守るか**:
- ユーザ Model 型に `Clone` / `PartialEq` / `Hash` / `Default` を要求しない
- メッセージ型を導入しない (Edit は enum + `Box<dyn FnOnce>`)
- 内部 scenegraph の差分検出は **widget ID + プリミティブ末端値の hash** だけで行う
- derive マクロは禁止 (Lens 等)

---

## 基盤クレート選定 (現状確定)

| レイヤ | 採用 | 現状バージョン (M1 時点) |
|---|---|---|
| Window/Event | winit | 0.30.13 |
| Rendering | 自前 wgpu パイプライン | wgpu 29.0.1 |
| Text | glyphon (cosmic-text + swash) | glyphon 0.11.0 |
| Layout | taffy | 0.10.1 |
| Platform handle | raw-window-handle | 0.6.2 |
| Math/binding | bytemuck | 1.25.0 |
| 開発用 | criterion / trybuild | M2 で導入 |
| A11y | AccessKit | M6 |

シェーダは 4 本構成: instanced rect / textured quad / **line strip (波形・メータ・オートメーション)** / SDF glyph (現状は glyphon)。

---

## 現状ワークスペース構成 (F:\dev\gui_01)

```
F:\dev\gui_01\
├── Cargo.toml                       # workspace, edition=2024, rust-version=1.95
├── rust-toolchain.toml
├── docs\                            # ★ M2 開始前に作成、plan.md を置く
│   └── plan.md                      # ★ 本ファイルの正本
├── crates\
│   ├── platform\                    # daw-ui-platform (winit抽象)
│   │   └── src\{lib,event,window,winit_backend}.rs
│   ├── renderer\                    # daw-ui-renderer (wgpu)
│   │   └── src\
│   │       ├── lib.rs / device.rs / scene.rs
│   │       └── pipelines\
│   │           ├── mod.rs
│   │           ├── rect.rs + rect.wgsl   ← 角丸矩形 (実装済)
│   │           └── glyph.rs              ← glyphon 統合 (実装済)
│   ├── ui\                          # daw-ui-core (Ui/Edit/widgets)
│   │   └── src\{lib,edit,id,input,layout,ui}.rs
│   │       └── widgets\{mod,button,label}.rs
│   └── examples\mixer\              # daw-ui-example-mixer (動作確認)
│       └── src\main.rs
```

---

## マイルストーン (改訂版)

### M1 (完了 ✅ — 初期コミット時点)

- ✅ winit でウィンドウ起動 → wgpu surface 初期化 → clear
- ✅ `WindowBackend` trait + 中立 `AppEvent` enum
- ✅ instanced 角丸矩形パイプライン (`rect.wgsl`、4 隅個別半径 + ボーダー + AA)
- ✅ glyphon で日本語/絵文字描画
- ✅ taffy ラッパ (`LayoutPass`, vbox/hbox 基本)
- ✅ `Ui<'a, M>` / `UiHost<M>` / `frame()` API
- ✅ `Edit<M>` enum (`Mutate(Box<dyn FnOnce(&mut M) + Send + 'static>)`)
- ✅ `WidgetId` (FNV-1a 64bit、Model に `Hash` 不要)
- ✅ `PointerFrame` / ヒットテスト (`clicked` / `hovered` / `pressed_inside`)
- ✅ `button` / `label` ウィジェット
- ✅ examples/mixer: 3 ボタン + 1 万矩形ベンチ + 日本語ラベル

### M2 (★★ 波形表示 UI 早期検証 ★★)

**目的**: 波形表示が想定通り動くことを早期に確定する。性能・API・データフロー (LOD ピラミッド) を実コードで検証し、後続マイルストーンの前提を固める。詳細モード (サンプルエディタ) は M5 へ。

**主な成果物**:

1. **`pipelines/line.rs` + `line.wgsl` (line strip パイプライン)**
   - `LinePipeline` struct (`new` / `prepare` / `render`)
   - 入力: `Vec<LineBatch>` (各バッチは線色・線幅・anti-alias 設定 + 頂点列)
   - シェーダ: 線分を 2 三角形で押し出し (geometry shader 不使用、頂点シェーダで側方オフセット計算)
   - 1 本の draw call で数十万頂点を流せること
   - `Scene::line_batches: Vec<LineBatch>` を追加し、`Renderer::render` から呼び出す

2. **LOD ピラミッド (生サンプル → min/max 多段ダウンサンプル)**
   - `UiHost<M>` 内の `waveform_cache: HashMap<WidgetId, WaveformPyramid>`
   - 16 倍ずつ decimation するレベル列 (`MinMaxLevel { pairs: Vec<MinMaxPair>, decimation: u32 }`)
   - `(generation, valid_len, sample_rate, channels)` をキーに再利用判定
   - `valid_len` 拡大時は **インクリメンタル拡張** (録音中追記対応)

3. **`Ui::waveform()` プロトタイプ (クリップ表示モード限定)**
   - 公開 API (詳細は次セクション): `WaveformSource` / `WaveformView` / `WaveformStyle` / `WaveformResponse`
   - 描画モード: **PeakLines のみ** (RMS / SamplePolyline / Auto は M5 へ)
   - チャンネル: Mono / Stereo (Planar) — Interleaved / 任意 N ch は M5 へ
   - クリップ rect でのクリッピング、ヒットテスト (サンプル index 返し)

4. **`examples/waveform_validation/` (ベンチ中心の検証用サンプル)**
   - 1 分ステレオ (sample rate 48kHz, 5.76M サンプル) を表示
   - スクロール / ズーム操作 (左右ドラッグ + マウスホイール)
   - **HUD 表示**: フレーム時間・LOD 構築時間・LOD レベル
   - 録音シミュレーション (1ms 毎に valid_len 拡大) のトグル

5. **基盤の小規模整備**
   - `criterion` を `[workspace.dev-dependencies]` に導入
   - `trybuild` を導入し、`Ui::waveform()` シグネチャに `Clone`/`Hash` が登場しないことを doc test で固定

**M2 完了条件 (Definition of Done)**:
- [ ] line パイプライン: 10 万頂点を 1 draw call で 60fps
- [ ] LOD 初回構築: 5.76M サンプルで < 50ms
- [ ] LOD 再利用: `generation` 一致時の `Ui::waveform()` 呼び出し時間 < 100µs (criterion)
- [ ] 録音シミュレーション: 1ms 追記でフレーム時間 16.7ms 安定
- [ ] examples/waveform_validation 起動 → スクロール/ズーム滑らか
- [ ] `Ui::waveform()` API シグネチャに `Clone`/`Hash` 制約が無いことの trybuild 検証
- [ ] 重大な API 変更が必要な兆候があれば、M3 着手前に設計ドキュメントへ反映

### M3 (Ui<'a> 充実 + 基本ウィジェット拡張) — 旧 M2 の内容

- `Ui::fader` / `Ui::knob` / `Ui::checkbox` / `Ui::text_input` (ASCII + 日本語 IME)
- ドラッグ状態管理: `UiHost.state: HashMap<WidgetId, Box<dyn WidgetState>>` の実利用 (focus / drag start pos / scroll offset)
- キーボード入力のウィジェット組み込み (`AppEvent::Keyboard` → focused widget)
- IME 候補位置の `WindowBackend::set_ime_position()` 実装 (winit 側)
- `LayoutPass` 拡張: padding / gap / fixed size / proportional growth
- examples/mixer 拡張: 8ch ミキサーが GUI Model だけで動く (フェーダドラッグ → Edit → apply)
- **trybuild 検証**: ユーザ Model に `Clone`/`PartialEq`/`Hash`/`Default` を実装していないコードがコンパイル成功することを固定 (回帰防止)

### M4 (内部 scenegraph + 差分検出) — 旧 M3

- 内部 scenegraph (`SlotMap<NodeId, SceneNode>`)
- input hash による差分検出 → 静的 UI の draw call 削減を計測
- 1000 ウィジェット級ミキサーで「変化していない部分の draw call が出ない」ことを確認
- glyphon の Buffer キャッシュ統合 (現状は毎フレーム作り直し)
- input hash 衝突テスト: ランダムなウィジェット入力 1M 件で衝突 0
- 波形ウィジェットの LOD ピラミッドは **M2 の構造のまま scenegraph 配下に再配置** (input_hash に generation を組み込む)

### M5 (heavy() + 巨大ビュー + 詳細波形モード)

- `ui.heavy("...", |hctx| ...)` 脱出口
  - `HeavyCtx::cached(viewport_key, draw_fn)`: ViewportKey が前フレームと同じなら GPU バッファ再利用
  - ヒットテストは毎フレーム実施 (キャッシュと独立)
- 波形ウィジェット **詳細モード**: SamplePolyline + サンプル点マーカー
  - 1 サンプル/ピクセル以下にズームしたとき自動切替 (`WaveformRenderMode::Auto`)
  - 円形マーカーは rect パイプラインを SDF circle に拡張 (or 別 instance shader)
  - 編集マーカー (loop start/end / cue points) を `waveform_marker()` 描画支援関数で
- 波形ウィジェット **RMS モード**: ±RMS バー塗りつぶし (rect 流用)
- 波形ウィジェット **任意 N チャンネル / Interleaved 入力** 対応
- examples/sample_editor: 1 サンプルファイルをロード → 全表示・ズーム・選択範囲ハイライト・カーソル
- examples/piano_roll: 100k notes をスクロール 120fps、heavy() の使い方サンプル
- オートメーションカーブ: ベジエ → CPU flatten → line strip パイプライン

### M6 (Phase 2)

- AccessKit 統合
- baseview バックエンド (`WindowBackend` 第 2 実装) — プラグインホスト版への布石
- プラグイン UI window 埋め込み API (raw-window-handle 受け渡し)
- vello サブシステム併用 (SVG アイコン用に必要なら)
- 波形編集オペレーション (trim / fade / split) のサンプル実装

---

## 波形表示 UI 詳細設計 (M2 で実装、M5 で詳細モード追加)

### 1. 公開 API (M2 で確定、後方互換を意識)

```rust
// crates/ui/src/widgets/waveform.rs

impl<'a, M> Ui<'a, M> {
    pub fn waveform<'s>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        source: WaveformSource<'s>,
        view: WaveformView,
        style: WaveformStyle,
    ) -> WaveformResponse;
}

pub struct WaveformSource<'s> {
    /// 生サンプル。フレーム間で同じ slice が来る限り、内部 LOD を再利用。
    pub samples: SampleSlices<'s>,
    /// 有効長 (録音中など samples.len() より小さい場合あり)。
    pub valid_len: usize,
    /// アプリが内容変更時にインクリメントする。一致なら LOD 再利用。
    pub generation: u64,
    pub sample_rate: u32,
}

pub enum SampleSlices<'s> {
    Mono(&'s [f32]),
    /// 各チャンネル個別 (planar)
    Planar(&'s [&'s [f32]]),
    /// インターリーブ (channels 個ずつ並ぶ)  — M5 で対応
    Interleaved { data: &'s [f32], channels: usize },
}

pub struct WaveformView {
    pub start_sample: u64,
    pub len_samples: u64,
    pub vertical_gain: f32,
}

pub struct WaveformStyle {
    pub fg: Color,
    pub fg_clipped: Color,             // |sample| > 1.0 を強調
    pub fill: Option<Color>,            // RMS 塗りつぶし — M5
    pub baseline: Option<Color>,
    pub channel_layout: ChannelLayout,  // Stack / Overlay / FirstOnly
    pub render_mode: WaveformRenderMode,
    pub line_width_px: f32,
}

pub enum WaveformRenderMode {
    /// pixel あたり 1 本の縦線 (min..max)。M2 で対応する唯一のモード。
    PeakLines,
    /// pixel あたり ±RMS バー — M5
    RmsBars,
    /// 1 サンプル/pixel 以下にズームしたとき: 折れ線 + サンプル点マーカー — M5
    SamplePolyline,
    /// samples_per_pixel から自動切替 — M5
    Auto,
}

pub struct WaveformResponse {
    pub hovered: bool,
    pub clicked_at: Option<WaveformHit>,
    pub dragging_at: Option<WaveformHit>,
}

pub struct WaveformHit {
    pub sample_index: u64,
    pub channel: usize,
    pub local_x_px: f32,
    pub local_y_px: f32,
}
```

**「Clone しない」原則の保ち方**:
- `WaveformSource<'s>` は **借用のみ** で構成 (`&[f32]` / `&[&[f32]]`)。`Clone`/`PartialEq` は要求しない。
- 内部 LOD ピラミッドは派生データ (min/max ペア) で、生サンプルのコピーではない。
- `generation: u64` を **唯一の不変キー** として再構築判定。bitwise hash や中身比較は行わない。
- `WaveformView` は **アプリ側がスクロール/ズーム状態を所有**。ライブラリは描画のみ。

### 2. 内部 LOD ピラミッド

`UiHost<M>` 内に追加:

```rust
struct UiHost<M> {
    state: HashMap<WidgetId, Box<dyn WidgetState>>,
    waveform_cache: HashMap<WidgetId, WaveformPyramid>,   // ★ M2 で追加
    // ...
}

struct WaveformPyramid {
    generation: u64,
    valid_len: usize,
    sample_rate: u32,
    channels: usize,
    /// レベル k は 16^k サンプルあたり 1 ペア (min, max)。
    /// levels[0] は無し (生サンプル参照のため)。levels[1..] が派生データ。
    levels: Vec<MinMaxLevel>,
}

struct MinMaxLevel {
    /// channels × n ペア。チャンネル毎に連続。
    pairs: Vec<MinMaxPair>,
    decimation: u32,        // 16^k
}

#[repr(C)]
struct MinMaxPair { min: f32, max: f32 }
```

**ピラミッド構築アルゴリズム**:
- ベース係数 r = 16 (チューニング可)
- レベル 1: 16 サンプルから min/max を 1 ペア → サイズ N/16
- レベル k+1: レベル k の隣接 r ペアの min/max を再計算 → サイズ N/16^(k+1)
- 最大レベル: ペア数が画面横幅の数倍 (例: 4096) を切るまで
- メモリ: 全レベル合計でおよそ N × 0.133 のスペースで済む (r=16, 等比和)

**ピラミッド再構築の判定**:
1. `(generation, valid_len, sample_rate, channels)` のいずれかが変化 → 完全再構築
2. `valid_len` のみ拡大 (録音中の追記) → **インクリメンタル拡張** (新規分のみ計算)
3. それ以外 → そのまま再利用

### 3. 描画レベル選択

ピクセル幅 W、表示サンプル数 N とすると `samples_per_pixel = N / W`。

| samples_per_pixel | 選択 | M2 対応 |
|---|---|---|
| `>= 16` | レベル k = floor(log_16(samples_per_pixel))、ピラミッドキャッシュ | ✅ |
| `1.5 .. 16` | レベル 0 (生サンプル) → ピクセル毎 min/max 走査 | ✅ |
| `< 1.5` | SamplePolyline (折れ線 + マーカー) | M5 (M2 では PeakLines のまま縮退) |

中間レベルの線形補間は行わない (DAW では一貫した min/max 表示が望ましい)。

### 4. line strip パイプラインへの流し込み

`Scene` を拡張 (M2):

```rust
pub struct Scene {
    pub clear_color: wgpu::Color,
    pub rects: Vec<RectCommand>,
    pub glyph_areas: Vec<GlyphArea>,
    pub line_batches: Vec<LineBatch>,    // ★ M2 で追加
}

pub struct LineBatch {
    pub vertices: Vec<LineVertex>,        // pos2 + color4 + side(±1)
    pub topology: LineTopology,           // Strip / List
    pub line_width_px: f32,
    pub clip_rect: Option<Rect>,          // 波形ウィジェット矩形でクリップ
}
```

PeakLines モード = `LineTopology::List` で 2 頂点 × W 本。

### 5. インタラクションと Edit

- ライブラリは `WaveformResponse` で **位置情報のみ返す**。Edit はアプリ側が組み立てる:
  ```rust
  let resp = ui.waveform("clip0", rect, src, view, style);
  if let Some(hit) = resp.clicked_at {
      ui.push_edit(Edit::mutate(move |m: &mut MyModel| {
          m.set_playhead(hit.sample_index);
      }));
  }
  ```
- 選択範囲・プレイヘッドカーソルはアプリ側 state で持ち、ライブラリには `ui.push_rect()` で重ねる。
- スクロール/ズームのキー入力受信もアプリ側責務。ライブラリは「現在の `WaveformView` を描く」だけ。

### 6. heavy() との関係 (M5)

タイムライン上に多数のクリップ波形が並ぶケースでは `ui.heavy()` で囲み、`ViewportKey { range, zoom, generations }` でフレーム間キャッシュ:

```rust
ui.heavy("track_0_clips", |hctx| {
    let key = ViewportKey {
        range: m.timeline_range(),
        zoom_level: m.zoom_level(),
        clip_generations: m.clip_generations_hash(),
    };
    hctx.cached(key, || {
        for clip in m.clips_in_range() {
            hctx.waveform(clip.id, clip.rect, clip.source(), clip.view(), STYLE);
        }
    });
});
```

**heavy() の中でも LOD ピラミッドキャッシュは引き続き有効** (ピラミッドは `WidgetId` キーのため heavy 境界に影響されない)。

---

## 検証方法

### 共通 (全マイルストーン)
- `cargo build --workspace` / `cargo clippy --workspace -- -D warnings` / `cargo test --workspace`

### M2 (波形 UI 早期検証 — 本マイルストーンの主役)

**性能ベンチ (criterion)**:
- LOD 初回構築: 5.76M サンプル (1 分 × 48kHz × stereo) で < 50ms
- LOD 再利用: `generation` 一致時の `Ui::waveform()` 呼び出し < 100µs
- 録音追記: 1ms 毎に valid_len を 48 サンプル拡大、フレーム時間 16.7ms 安定 1000 フレーム連続
- line strip 単独描画: 10 万頂点を 1 draw call で 60fps

**目視確認 (examples/waveform_validation)**:
- スクロール (左右ドラッグ) が滑らか (60fps)
- ズーム (マウスホイール) で LOD レベルが切り替わる (HUD で確認)
- 録音シミュレーション on にすると右端が伸びていく
- ステレオを Stack 表示・Overlay 表示で切替できる

**回帰防止 (trybuild)**:
- `Ui::waveform()` シグネチャに `Clone`/`Hash` 制約が登場しないことを doc test で監査
- ユーザ Model に `Clone` 等を実装しないコードがコンパイル成功 (M3 でより網羅的に)

**API レビュー**:
- M2 完了時点で `WaveformSource` / `WaveformView` / `WaveformStyle` / `WaveformResponse` の API シグネチャが M5 (詳細モード) でも破壊変更なしに拡張可能か、impl 担当者がチェックリストで確認

### M3 (Ui 充実)
- `cargo bench`: 1 万矩形描画 60fps、IME 入力レイテンシ 1 frame 以下
- examples/mixer 拡張: フェーダドラッグが滑らか、ボタン押下に視覚フィードバック
- trybuild: ユーザ Model に `Clone`/`PartialEq`/`Hash`/`Default` を実装していないコードがコンパイル成功

### M4 (scenegraph)
- input hash 衝突テスト: ランダム widget 入力 1M 件で衝突 0
- 「変化していないフレームで draw call 数が前フレームと同じ」を 1000 フレーム連続で確認
- 波形ウィジェットが scenegraph 統合後も M2 のベンチ値を維持

### M5 (heavy() + 詳細波形 + piano roll)
- examples/sample_editor: 5 分ステレオを全表示 → ズーム → 1 サンプル/ピクセル → 折れ線 + マーカーが見える
- examples/sample_editor: 詳細モードでクリック位置のサンプル index ±1 以内
- examples/piano_roll: 100k notes スクロール 120fps
- 多数クリップ表示 (10 トラック × 各 50 クリップ) を heavy() で 60fps

---

## 設計上の不変条件

1. ライブラリ提供 API は **ユーザ Model 型に `Clone`/`PartialEq`/`Hash`/`Default` を要求しない**。差分検出は ID + プリミティブ末端値の hash でだけ行う。
2. メッセージ型は導入しない (Edit は enum or `Box<dyn FnOnce>`)。`Application::Message: Clone` 伝染を構造的に防ぐ。
3. `derive` マクロは禁止 (Lens 等)。
4. ライブラリは **audio / IPC / プロセス間通信に一切関知しない**。Edit を返すところで責務を切る。
5. heavy() 以外でも viewport culling は前提 (1000 ウィジェット級は通常パスで耐える)。
6. `Ui<'a>` の `'a` で借用ライフタイムを統一し、GAT を使わない。
7. **波形ウィジェット固有**:
   - `WaveformSource` は借用のみ。`samples: &[f32]` の Clone は禁止。
   - LOD ピラミッドは派生データ (min/max ペア) で、生サンプルのコピーは禁止。
   - 再構築判定は `generation: u64` のみ。中身 hash や bitwise 比較は禁止。
   - 録音中の追記 (`valid_len` 拡大) はインクリメンタル拡張で扱う。

---

## 重要ファイル一覧

### M2 で新規作成
- `F:\dev\gui_01\docs\plan.md` — **本ファイルの正本** (ExitPlanMode 直後にコピー)
- `crates/renderer/src/pipelines/line.rs` — line strip パイプライン
- `crates/renderer/src/pipelines/line.wgsl` — 線分 → 三角形展開シェーダ
- `crates/ui/src/widgets/waveform.rs` — `Ui::waveform`、LOD ピラミッド管理
- `crates/examples/waveform_validation/` (新規 example crate) — ベンチ + 目視サンプル
- `crates/ui/benches/waveform.rs` — criterion ベンチ
- `crates/ui/tests/no_clone_required.rs` — trybuild

### M2 で改修
- `crates/renderer/src/scene.rs` — `Scene::line_batches: Vec<LineBatch>` を追加
- `crates/renderer/src/device.rs` — `Renderer::render` に line パイプライン呼び出し
- `crates/renderer/src/pipelines/mod.rs` — `pub mod line;` 追加
- `crates/ui/src/lib.rs` — `widgets::waveform` を再 export
- `crates/ui/src/ui.rs` — `Ui` impl block 追加 or 別ファイル split
- ルート `Cargo.toml` — `criterion`/`trybuild` を `[workspace.dev-dependencies]` に追加、`crates/examples/waveform_validation` をメンバー追加

### M2 で再利用する既存資産
- `crates/ui/src/widgets/button.rs` — `pressed_inside`/`hovered`/`clicked` の使い回し
- `crates/ui/src/id.rs` — `WidgetId::child` で per-widget cache key
- `crates/ui/src/input.rs` — `PointerFrame` をドラッグ判定に流用
- `crates/renderer/src/pipelines/rect.rs` — 同 wgsl パターン (uniform / instance vbuf / vertex draw) を line.rs でも踏襲

### M5 で追加
- `crates/ui/src/widgets/heavy.rs` — `HeavyCtx`、ViewportKey キャッシュ
- `crates/examples/sample_editor/`、`crates/examples/piano_roll/`
- `crates/ui/src/widgets/waveform.rs` の SamplePolyline / RmsBars / Auto 分岐 + マーカー描画支援

---

## ビルド構成 (確定)

- **Rust Edition: 2024** (`rust-toolchain.toml` で固定済)
- **rust-version: 1.95** (workspace ルートで固定済)
- **依存**: `[workspace.dependencies]` で一元管理、メンバー crate は `<crate>.workspace = true` で参照
- **更新方針**: マイルストーン毎に最新安定版を確認、breaking change があれば追従

---

## ExitPlanMode 直後の最初のアクション (備忘)

1. `F:\dev\gui_01\docs\` ディレクトリを作成
2. `~/.claude/plans/ui-hashed-stearns.md` の内容を `F:\dev\gui_01\docs\plan.md` にコピー
3. `git add docs/plan.md && git commit -m "docs: add design plan for M2 waveform UI"` (ユーザの明示指示後に実施)
4. 以後のプラン更新は `docs/plan.md` を編集対象とし、`~/.claude/plans/` 側は必要に応じて手動同期
