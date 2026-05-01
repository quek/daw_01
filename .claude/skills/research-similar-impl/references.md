# 調査対象プロジェクト

## Rust GUI ライブラリ (設計対比 / 実装パターン)

| プロジェクト | 特徴 | クローン先 | URL |
|---|---|---|---|
| **iced** | Elm-style msg-passing GUI、`Application::Message: Clone` 必須。**gui_01 が反例として避けている設計**。msg / view / update の構造、`iced_wgpu` の描画は参考になる | /tmp/iced | https://github.com/iced-rs/iced |
| **egui** | pure immediate-mode、内部 `Memory` で状態キャッシュ + ID hash で差分管理。**gui_01 と最も近い設計**。widget の組み立て / 描画 / hit-test を最優先で参照 | /tmp/egui | https://github.com/emilk/egui |
| **druid** | retained-mode、Lens / Data trait ベース。**gui_01 が避けている設計**だが Lens 抽象の対比に有用 | /tmp/druid | https://github.com/linebender/druid |
| **xilem** | Linebender 系、retained + reconciliation (React 風)。druid の後継。Edit / patch パターンの参考 | /tmp/xilem | https://github.com/linebender/xilem |
| **floem** | Signal-based reactive。reactive と retained のハイブリッド対比 | /tmp/floem | https://github.com/lapce/floem |
| **Slint** | DSL ベース、`.slint` ファイルでマークアップ。GUI ツリー構造 / アニメーションの参考 | /tmp/slint | https://github.com/slint-ui/slint |

**最優先**: gui_01 と設計が近い **egui** (immediate-mode + ID hash) を参考に。
次点: **iced** (反面教師として、`Message: Clone` 伝染を避けている本プロジェクトの設計判断の根拠を再確認)。

## 基盤クレート (実 API 確認)

| プロジェクト | 役割 | クローン先 | URL |
|---|---|---|---|
| **winit** | ウィンドウ + イベントループ | /tmp/winit | https://github.com/rust-windowing/winit |
| **wgpu** | GPU レンダリング | /tmp/wgpu | https://github.com/gfx-rs/wgpu |
| **glyphon** | wgpu 上のテキスト描画 (cosmic-text 統合) | /tmp/glyphon | https://github.com/grovesNL/glyphon |
| **cosmic-text** | テキスト shaping / layout / graphemic boundary | /tmp/cosmic-text | https://github.com/pop-os/cosmic-text |
| **taffy** | flexbox / grid layout 計算 | /tmp/taffy | https://github.com/DioxusLabs/taffy |
| **raw-window-handle** | wgpu と winit の橋渡し型 | /tmp/raw-window-handle | https://github.com/rust-windowing/raw-window-handle |

各 crate の `examples/` ディレクトリも参照。

## ⚠️ crates.io 版と GitHub main で API が違う場合

`/tmp/<crate>` は **GitHub main**。gui_01 が実際に使うのは `Cargo.lock` で solver が選んだ
crates.io 版。両者で API が違うなら **crates.io 側を基準に実装**。

既知の乖離:

- **winit 0.29 → 0.30**: `EventLoop::run` 廃止、`ApplicationHandler` trait に移行 (`resumed` / `window_event` / `about_to_wait`)
- **winit 0.30 modifier**: `mods.state().control_key()` (新) vs `mods.ctrl()` (旧)
- **winit `WindowEvent::KeyboardInput`** の `event.physical_key` が `PhysicalKey::Code(KeyCode::...)` の二重 wrap
- **wgpu 0.20 → 29.x**: render pass の取り扱い・surface API・texture format 列挙が複数破壊変更
- **wgpu `SurfaceError`**: `Outdated` / `Lost` / `OutOfMemory` / `Timeout` の意味と回復方針が version で微差
- **raw-window-handle 0.5 → 0.6**: `RawWindowHandle` の variant が re-shape、`HasWindowHandle` trait が新登場
- **taffy 0.7 → 0.10**: `Dimension::Points` → `Dimension::length`、`AvailableSpace` API、`Style.size` のデフォルト挙動 (auto vs percent) が変化
- **taffy 0.10 prelude**: `FlexDirection` / `NodeId` は `taffy::prelude` 経由でしか pub されない
- **glyphon 0.10 → 0.11**: `TextRenderer::new` の引数 (cache 構成) が変わった世代あり
- **cosmic-text** はマイナーバージョン間でも shaping API の変更が頻繁

Agent に調査を依頼するときは「crates.io の `<crate> = \"X.Y.Z\"` 基準で」と明記。

## API リファレンス・ガイド

| ドキュメント | URL |
|---|---|
| winit | https://docs.rs/winit |
| winit examples | https://github.com/rust-windowing/winit/tree/main/examples |
| wgpu | https://docs.rs/wgpu |
| wgpu examples | https://github.com/gfx-rs/wgpu/tree/trunk/examples |
| WGSL 仕様 | https://www.w3.org/TR/WGSL/ |
| glyphon | https://docs.rs/glyphon |
| cosmic-text | https://docs.rs/cosmic-text |
| taffy | https://docs.rs/taffy |
| taffy examples | https://github.com/DioxusLabs/taffy/tree/main/examples |
| raw-window-handle | https://docs.rs/raw-window-handle |
| egui | https://docs.rs/egui |

## 機能と API の対応例

gui_01 で頻出する実装トピックと、その調査でぶつかる API。

| 機能 | 主な API / 参考実装 |
|---|---|
| widget hit-test | `daw_ui_renderer::Rect::contains`、自作 widget の `pointer.pos` 判定 (egui の `Response` パターンも参考) |
| ドラッグ状態管理 | `state.drag_anchor: Option<DragAnchor>`、egui の `is_being_dragged` |
| ダブルクリック | `Instant::now()` + `last_click: Option<ClickRecord>` (gui_01 Phase 4d)、egui の `double_clicked` |
| Ctrl+drag 高精度 | `pointer.modifiers.ctrl` で sensitivity 切替、mid-drag で再 anchor |
| flex layout | `LayoutPass::flex(direction, gap, padding, children)` + taffy `Style.flex_*` |
| flex_grow 比例配分 | `LayoutPass::leaf_grow(grow)` (`flex_basis: 0` + `flex_grow`)、taffy `compute_layout` |
| line strip 描画 | `crates/renderer/src/pipelines/line.rs` (instanced segment → quad 展開、scissor)、wgpu の vertex shader expansion パターン |
| LOD ピラミッド | `crates/ui/src/widgets/waveform.rs` の MinMax level、egui の plot ライブラリ |
| 波形描画 | line strip + scissor、samples_per_pixel から level 選択 |
| keyboard focus | `UiHost::focused: Option<WidgetId>`、egui の `Memory::focus` |
| IME 統合 | `WindowBackend::set_ime_allowed` + `set_ime_cursor_area`、winit `WindowEvent::Ime`、cosmic-text の preedit |
| text shaping (実 measure) | cosmic-text `Buffer::layout_runs`、glyphon `TextArea` |
| no-Clone Model | `Edit<M> = Box<dyn FnOnce(&mut M)>`、msg-passing なし、egui の Closure / iced の Message と対比 |
| widget state cache | `state: HashMap<WidgetId, Box<dyn WidgetState>>` + downcast、egui の `Memory::data` |
| 差分検出 (M4) | widget ID + プリミティブ末端値の hash、egui の widget id |
| heavy() (M5) | `HeavyCtx::cached(viewport_key, draw_fn)`、egui の `paint_at` キャッシュ |

## 実装で特に注意するポイント

- **no-Clone load-bearing**: ユーザ Model に `Clone` / `PartialEq` / `Hash` / `Default` を要求しない。新 API のシグネチャに紛れ込まないよう trybuild で固定
- **derive マクロ禁止**: Lens / Data 等は使わない
- **Ui<'a> の借用ライフタイム統一**: GAT を使わずに stable Rust で表現
- **library は audio / IPC を持ち込まない**: `Edit<M>` を返すところで責務終了
- **winit Event の中立化**: 自作 `AppEvent` で吸収、UI 層は winit を直接知らない (`crates/platform/src/event.rs`)
- **wgpu pipeline の最小化**: 既存の rect / glyph / line 4 本体制を崩さない
- **テスト性**: `UiHost::frame` 直接呼びで widget 単体テストできること
