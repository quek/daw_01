---
name: research-similar-impl
description: |
  Rust GUI 系ライブラリ (iced / egui / druid / xilem / floem / Slint 等) と
  winit / wgpu / glyphon / cosmic-text / taffy のソースコード・公式リファレンスを調査し、
  実装方針レポートを出力する。
  「実装して」「追加して」「修正して」「対応して」「機能を作って」「バグを直して」等、
  コード変更を伴う指示があったとき、または winit / wgpu / taffy / cosmic-text API の
  使い方や挙動が不明なときに発動。調査のみ行い、コードの編集は行わない。
argument-hint: "[調査対象の機能名]"
allowed-tools: Bash(git clone *), Bash(git pull *), Read, Grep, Glob, WebSearch, WebFetch, Agent
---

# 類似 Rust GUI / API リファレンス調査 (gui_01)

$ARGUMENTS に関する調査を行い、gui_01 での実装方針を立てるためのレポートを出力する。

## 手順

### 1. 調査対象の特定

ユーザの要求から実装対象の機能 / 利用 API を特定する。
[references.md](references.md) の「機能と API の対応例」を参照。

### 2. リポジトリのクローン

[references.md](references.md) の調査対象プロジェクトから関連するものを `/tmp` にクローン。

```bash
[ -d /tmp/iced ]        || git clone --depth 1 https://github.com/iced-rs/iced.git /tmp/iced
[ -d /tmp/egui ]        || git clone --depth 1 https://github.com/emilk/egui.git /tmp/egui
[ -d /tmp/druid ]       || git clone --depth 1 https://github.com/linebender/druid.git /tmp/druid
[ -d /tmp/xilem ]       || git clone --depth 1 https://github.com/linebender/xilem.git /tmp/xilem
[ -d /tmp/floem ]       || git clone --depth 1 https://github.com/lapce/floem.git /tmp/floem
[ -d /tmp/winit ]       || git clone --depth 1 https://github.com/rust-windowing/winit.git /tmp/winit
[ -d /tmp/wgpu ]        || git clone --depth 1 https://github.com/gfx-rs/wgpu.git /tmp/wgpu
[ -d /tmp/glyphon ]     || git clone --depth 1 https://github.com/grovesNL/glyphon.git /tmp/glyphon
[ -d /tmp/cosmic-text ] || git clone --depth 1 https://github.com/pop-os/cosmic-text.git /tmp/cosmic-text
[ -d /tmp/taffy ]       || git clone --depth 1 https://github.com/DioxusLabs/taffy.git /tmp/taffy
```

- クローン先は `/tmp` 配下 (作業ディレクトリを汚さない)
- `--depth 1` で軽量クローン
- 既存ならスキップ

### 3. 並列調査 (Agent を並列起動)

以下の A) と B) を **Agent を並列起動して同時に** 実行する。

#### A) 類似 GUI ライブラリのソースコード調査

クローン済みリポジトリを Grep / Read で横断検索。調査ポイント:

- **immediate-mode vs retained-mode** のトレードオフ実装 (egui = pure immediate、iced = msg-passing retained、druid/xilem = retained、floem = signal reactive)
- **layout 計算の流し込み**: taffy / 自前 / DSL のどれを使い、widget からどう呼んでいるか
- **入力イベントの流し方**: hit-test の所在、focus 管理、IME 連携、modifier 状態
- **描画パイプライン**: wgpu との接続、scenegraph / display list の有無、テキストレンダリング (glyphon / cosmic-text / 別)
- **状態管理パターン**: Signal / Lens / Edit / Msg / GAT、Model 制約 (Clone / Hash 要否)
- **波形・大量描画の最適化**: LOD、scissor、heavy ビューのキャッシュ戦略
- **examples の典型構造**: 1 widget の最小例 / 複合 widget / 大量 widget の例

#### B) 公式 API リファレンス・ガイド調査

[references.md](references.md) の API ドキュメント URL を WebFetch / WebSearch で:

- **winit** スレッドモデル、`ApplicationHandler` / `ActiveEventLoop` の契約、IME / Modifiers の扱い
- **wgpu** SurfaceError 種別、resize 中の race、複数キューの使い分け
- **glyphon** Buffer / FontSystem の lifecycle、Cache 共有
- **cosmic-text** 実 measure (`Buffer::layout_runs`)、shaping、graphemic boundary
- **taffy** Style の細部 (flex_wrap / flex_basis / min_size / max_size / aspect_ratio)、percent 解釈、auto の意味
- **raw-window-handle** バージョン互換性 (0.5 / 0.6 で type が変わる)

### 4. crates.io / Cargo.lock との整合性確認 (最優先)

`/tmp/<crate>` の clone は **GitHub main ブランチ**。crates.io 公開版と API が違うことがある。
**main だけ見てレポートすると誤った設計判断になる**。

対策手順:
1. `F:\dev\gui_01\Cargo.lock` で実際に solver が選んだバージョンを確認
2. `~/.cargo/registry/src/index.crates.io-*/<crate>-<version>/` の実ファイルを必ず Read / Grep
3. `/tmp/<crate>` の情報と食い違ったら **crates.io 側 (実際にビルドされる方) を信じる**
4. Agent に指示するときは「crates.io の `<crate> = \"X.Y.Z\"` を基準に調査」と明示

既知の差異:
- **winit 0.29 → 0.30** で `EventLoop::run` 廃止、`ApplicationHandler` trait に変更。Modifiers の getter 名も `ctrl()` → `control_key()`
- **wgpu 0.20 → 29.x** で render pass / surface API が複数破壊的変更
- **raw-window-handle 0.5 → 0.6** で `RawWindowHandle` の variant 構造変更
- **taffy 0.7 → 0.10** で `Dimension::Points` → `Dimension::length`、`AvailableSpace` の API 変更

### 5. 自プロジェクトの既存パターン確認

`F:\dev\gui_01` 内に類似実装がある場合は **最も信頼性の高い参照元**:

- 入力配線: `crates/platform/src/winit_backend.rs`、`crates/ui/src/input.rs`
- widget 様式: `crates/ui/src/widgets/{button, fader, knob, checkbox, text_input, waveform}.rs`
- パイプライン: `crates/renderer/src/pipelines/{rect, glyph, line}.rs`
- レイアウト: `crates/ui/src/layout.rs` (LayoutPass + taffy)
- テスト: `crates/ui/src/widgets/fader.rs::tests` (UiHost::frame 経由)

### 6. レポート出力

[report-template.md](report-template.md) の形式で日本語でまとめる。

## 制約

- **調査のみ**。ファイルの編集・作成・ビルド・インストール等は一切行わない
- gui_01 の load-bearing 設計不変条件 (no-Clone / no-derive-macro / Edit<M> / `Ui<'a>` 借用) を破る方針は提案しない
- crates.io 側の API を基準にする。GitHub main の API を鵜呑みにしない
- audio / IPC / FFI は本ライブラリのスコープ外なので調査しない (利用例で必要なら別 crate で)
