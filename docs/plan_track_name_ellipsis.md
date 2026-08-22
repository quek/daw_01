<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: track header のトラック名 ellipsis 省略 (gui_01 arrangement widget)

## 背景 / 症状

アレンジビューの track header で、長いトラック名のテキストが name 領域を越えて右に溢れる。
描画順は「①トラック名テキスト → ②M/S/R 各ボタンの塗り rect」なので、ボタンが在る場所は塗りに隠れるが、
**ボタン間の gap (2px) や name 領域と最初のボタンの間の隙間から、溢れたトラック名が少しずつ覗いて見える**
（ボタンの「上」に被さるのではなく、ボタンの「隙間」から覗く）。

スクリーンショット: 長いトラック名が「M S R」ボタンの間の隙間まではみ出して覗いている。

## 原因 (gui_01 内で確定済)

- `header_row_layout()` (`crates/ui/src/widgets/arrangement.rs:2010-2061`) は
  `name_w = (inner.w - total_right).max(20.0)` で M/S/R + gap + lane disclosure 分を
  差し引いた幅を `name_rect` に**正しく予約している**。レイアウトは正しい。
- トラック名の描画に使う汎用ボタン関数 `button_at_clicked_sized()`
  (`crates/ui/src/widgets/button.rs:67-152`) が、テキストを rect 幅に合わせて
  **省略 (ellipsis) もクリップもしない**。`button.rs:135-148`:
  - `text_w = ui.measure_text(text, font_size)` を測り
    `tx = rect.x + (rect.w - text_w).max(0.0)*0.5`。`text_w > rect.w` のとき
    `(rect.w - text_w)` が負 → `.max(0.0)` で 0 → `tx = rect.x` (左端)。
  - 続く `push_text` の `GlyphArea` が `clip_rect: None` なので、グリフが rect の
    右端を越えてそのまま全描画され、隣の M/S/R に被さる。
- renderer 側 (`crates/renderer/src/scene.rs:150-152`, `pipelines/glyph.rs:191-209`) には
  ellipsis / max_width / truncation 機構が無く、はみ出し防止は `clip_rect: Some(rect)` の
  ハードな TextBounds scissor クリップのみ (それだけだとグリフが途中で切れるだけで省略記号は出ない)。
- daw_01 側ではクリーンに直せない: daw_01 は `header_w = 160px` しか知らず、widget 内部で
  M/S/R + gap + lane_disc を引いた `name_w` を知らない。daw_01 で再現すると widget レイアウト算の
  複製 = SSoT 違反。**修正は gui_01 側が正しい。**

## 最終的に欲しい完成形

1. **トラック名 (および任意のボタンラベル) は自身の rect を絶対に越えない。**
   rect 幅に収まらないテキストは**末尾を ellipsis '…' で省略**し、収まる最長の prefix + '…' を描画する。
   M/S/R ボタンにも group disclosure (▶/▼) にも二度と被らない。

2. **省略時のトラック名は左寄せ** (先頭が識別に最も重要なため。Reaper / Cubase / GTK PANGO_ELLIPSIZE_END と一致)。
   収まる短いラベル (M/S/R/x/Rescan 等) は従来どおり中央寄せで外観完全不変。

3. **共有ボタン関数に入れる** のが理想 (`button_at_clicked_sized` / `toggle_button_at` 共通の helper)。
   「widget は自分の rect 境界に責任を持つ」を 1 箇所で保証し、将来 rect より広いラベルを
   渡す caller が現れても自動で守られる。既存の固定短ラベル caller は
   `measure_text(full) <= rect.w` で truncation 分岐に入らず**出力バイト完全互換** =
   #076 で確立した「font_size だけ可変・外観完全不変」の不変条件を壊さない。

4. **安全網として同じ `push_text` に `clip_rect: Some(rect)` を設定** (省略後でも 1 文字分の
   半端なオーバーシュートが出る環境を想定し、measure ベース省略 + hard clip の二重化)。

5. click→select / double-click→rename は **rect ベース判定のまま** (グリフ文字列だけ短縮、rect 不変)
   なので操作系は完全不変。group 子トラックでは縮んだ `name_rect_visible` (disclosure 分を引いた幅) に
   対して省略が効き、disclosure にも被らない。

## 参考実装 (gui_01 セッションでの想定変更)

`crates/ui/src/ui.rs` に helper を追加し、`button.rs` / `toggle_button.rs` から呼ぶ:

```rust
// impl Ui
/// `text` を `font_size` で描画したとき幅 `max_w` を超えるなら末尾を ellipsis ('…') で
/// 省略し、(描画文字列, 実描画幅) を返す。収まる場合は元の文字列と実幅をそのまま返す
/// (= 短ラベルは byte 互換)。char 境界単位で prefix を縮め `measure_advance(prefix + '…')
/// <= max_w` を満たす最大 prefix を選ぶ (cosmic-text の実 advance ベースなので wide glyph でも正しい)。
pub(crate) fn fit_text_ellipsized(
    &mut self,
    text: &str,
    font_size: f32,
    max_w: f32,
) -> (std::borrow::Cow<'_, str>, f32) { /* full が収まれば Borrowed、超えれば prefix+'…' を線形/二分探索 */ }
```

`button_at_clicked_sized` (button.rs:135-148) の `text_w = ui.measure_text(...)` →
`let (display, text_w) = ui.fit_text_ellipsized(text, font_size, rect.w);` に置換、
`push_text` の `text: display.into()` / `clip_rect: Some(rect)` に変更。
省略時は左寄せ (`tx = rect.x`)、収まる時は従来の中央寄せ。
`toggle_button_at` (toggle_button.rs:192-211) も同 helper を通す (M/S/R は現状 1 文字で no-op = byte 互換)。

## 確認事項 (gui_01 側で要確認)

- `…` (U+2026) が描画フォント `DEFAULT_FONT_FAMILY` (HackGen Console NF / Nerd Font) に
  字体として存在するか。無ければ豆腐 (□) になるので ASCII の `...` (3 点) にフォールバック。
- `clip_rect` を `None → Some(rect)` に変える件で既存の renderer snapshot / glyph cache test に
  影響が無いか (調査では `buffer_key` が clip_rect を含まないので cache 無効化は起きない見込み)。

## スコープ外 (将来 enhancement)

- ホバー時の full name tooltip 表示 (Cubase 等にある)。本バグの解消には不要。
