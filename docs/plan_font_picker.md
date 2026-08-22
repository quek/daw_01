<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_font_picker — Text クリップのフォントを検索付きピッカーで選ぶ（行=実フォント描画 + ライブプレビュー）

FIXME #25「Text クリップのフォントをプラグインピッカーと同じように選べるように」。
grill-me（2026-06-10）。

## 現状 (2026-06-10)

- `TextEvent.font_family: String`（[model.rs:2521-2562](F:/dev/daw_01/common/src/model.rs)）。
  `""` = renderer default（`HackGen Console NF`、gui_01
  [glyph.rs:23](F:/dev/gui_01/crates/renderer/src/pipelines/glyph.rs)）。
- inspector の Font 欄は **plain text_input**（手入力）
  （[track_inspector.rs:1227-1251](F:/dev/daw_01/daw_gui/src/view/track_inspector.rs)）。
  `ClipTextFontFamilyEditChanged` / `CommitClipTextFontFamilyEdit`、commit は全選択 clip に broadcast
  （[app.rs:11334-11345](F:/dev/daw_01/daw_gui/src/app.rs)）。
- renderer は glyphon（cosmic-text）。`font_family` は `Attrs::family(Family::Name(...))` で渡る
  （[glyph.rs:181](F:/dev/gui_01/crates/renderer/src/pipelines/glyph.rs)）。
  **`FontSystem` / fontdb は `GlyphPipeline` 内に private**
  （[glyph.rs:61](F:/dev/gui_01/crates/renderer/src/pipelines/glyph.rs)）。
  → **インストール済みフォントを列挙する public API が無い**（= gui_01 依存の核）。
- プラグインピッカーは検索付きモーダル + `list_view`
  （[plugin_picker.rs](F:/dev/daw_01/daw_gui/src/view/plugin_picker.rs)）。これを template に。

## 確定仕様 (grill-me 2026-06-10) — 見える挙動

- inspector の Font 欄を「ボタン → ピッカー」に。**プラグインピッカーと同じ検索付きモーダル、
  選択で閉じる**。
- **各行はそのフォント名を、そのフォント自身で描画**（本物のプレビュー）。日本語フォントは名前自体が
  日本語なのでそのまま見本になる。
- **ライブプレビュー**: 候補を ↑↓ / ホバーで辿ると、選択中のテキストクリップが**即その候補フォントで
  描き換わる**。確定で固定、Esc / 外クリックで元のフォントに復帰。
- 先頭に「**デフォルト**」項目（= `""`、renderer default）。
- 複数選択クリップへ**一括適用**（現状 broadcast を踏襲）。

## gui_01 依存（#096 で要望提出）

- **ハード要件**: インストール済みフォントファミリ名の列挙 API
  （例: `daw_ui_renderer::available_font_families() -> Vec<String>`、ソート済み・重複排除、
  renderer が実際に使う fontdb と同一集合）。
- **行ごとの任意フォント描画**: heavy block の `push_text(GlyphArea { font_family, .. })` で
  daw_01 側だけで実現できる見込み（list_view を使わず heavy で行を自前描画）。
  `push_text` が font 指定を取れない場合のみ `label_at`-with-font variant を要望。#096 で確認する。
- **ライブプレビュー自体は gui_01 追加不要**: 既存の text 描画の `font_family` を差し替えるだけ。

## 実装メモ

- `view/font_picker.rs` を `plugin_picker.rs` を template に新設。app 状態:
  `is_font_picker_open` / `font_picker_query` / `font_picker_entries: Vec<String>` /
  `font_picker_cursor` / `font_preview_original: Option<…>`（cancel 復帰用）。
- 起動時 or lazy に列挙 API で `font_picker_entries` を構築（フォント数次第で background thread 1 回 +
  EventLoopProxy。`std::thread::sleep` 系の background 規約に従う）。
- **ライブプレビュー**: cursor 行の `font_family` を「プレビュー override」として render 時にのみ適用。
  SSoT（model の `font_family`）は**確定時のみ**書き換える。cancel で override を捨てるだけ。
- 確定 → 既存 `CommitClipTextFontFamilyEdit` 相当の broadcast で全選択 clip に適用。
- 数値 param ではないので `scrubable_number` idiom（[reuse_inspector_idiom]）は無関係。bespoke
  edit-buffer は新設しない。

関連: [plan_unified_plugin_picker.md](plan_unified_plugin_picker.md)（共通の検索付きピッカー widget）、
[plan_text_overlay.md](plan_text_overlay.md)（TextEvent モデル）、
[plan_text_clip_creation.md](plan_text_clip_creation.md)。
gui_01 要望: `docs/gui_01_conversation.md` #096。
