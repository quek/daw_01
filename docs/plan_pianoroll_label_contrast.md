<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: piano roll 鍵盤オクターブラベルの可読性 (FIXME #20)

## 症状 / 最終形態

ピアノロールの鍵盤レーンに描かれる **オクターブラベル (C5 / root の "C2" 等) が
背景と同系色でコントラストが低く読みにくい**。

最終形態: scale の有無・モード (None / Highlight / Fold)・root の鍵 (白鍵/黒鍵)・
overlay の有無に関わらず、**オクターブラベルが常にはっきり読める**。

## 原因 (一次情報)

`crates/ui/src/widgets/piano_roll.rs` の label 色 default が **鍵盤 key fill ではなく
dark な `keyboard_bg` (0.22) を背景と想定して調色されている** ため、実際に label が
描かれる **明るい key fill / warm overlay の上で輪郭が立たない**。

- ラベルは key fill (`white_key = 0.92` / `black_key = 0.10`) の上、
  root 行は `root_row_overlay = rgba(1.0, 0.80, 0.30, 0.32)` (warm) が重なり **cream** になる。
- `root_label_fg = rgb(0.95, 0.78, 0.40)` (warm yellow) → **cream 背景に warm-on-warm** で低コントラスト
  (= 実機 `C2` が読みにくい症状)。
- `in_scale_label_fg = rgb(0.78, 0.80, 0.85)` (light) → `white_key 0.92` 上で **light-on-light**。
  Fold モードでは全 in-scale 行に label が出るので白鍵/黒鍵を跨ぎ、単一色では両立不可能。
- default コメントも "keyboard_bg (0.22) 上で読める明度" と書いており、想定背景が key fill と
  食い違っている。

該当描画 (`draw_keyboard_lane` 相当):
```rust
// Fold: 全 in-scale 行
let (text, color) = if is_root { (format!("{name}{octave}"), style.root_label_fg) }
                    else        { (name.to_string(),          style.in_scale_label_fg) };
// Highlight: root pitch class オクターブのみ → style.root_label_fg
// None: C オクターブのみ → style.c_label_color
hctx.push_text(GlyphArea { text, left: kbd.x + 4.0, top: y, color, .. });  // ← key fill 上
```

## gui_01 への依頼

clip 名で既に入っている **fill 輝度由来の WCAG auto-contrast (gui_01 #060)** を、
**鍵盤オクターブラベルにも適用** してほしい。各ラベルを描く直前にその行の実効背景
(key fill + 重なる overlay) の相対輝度を見て、ラベル色を dark / light に自動反転する。

- 対象: Fold (root + in-scale + 予約 out-of-scale)、Highlight (root)、None (C) の全 label パス。
- 期待挙動: 白鍵 (0.92) / cream root 行 → 濃色ラベル、黒鍵 (0.10) / dim out 行 → 淡色ラベル。
- 既存 `root_label_fg` 等の色 field は **hue ヒント** として残しても、auto-contrast で輝度だけ
  反転する形でも可 (gui_01 判断)。root の "強調" を残したい場合は auto-contrast 後に hue を
  warm 寄りに保つ等で両立。
- default-on が理想 (clip 名と同様)。flag 化する場合は daw_01 が `PianoRollStyle` で有効化する。

代替案 (auto-contrast が重ければ): ラベルに **1px outline / halo** (`GlyphArea.outline_color` /
`outline_width_px`) を付け、任意背景でエッジを立てる。ただし auto-contrast の方が見た目が素直。

## daw_01 側

`piano_roll_view.rs` は `PianoRollStyle::default()` を渡すだけ (static 色 override は撤去済)。
auto-contrast が default-on なら **無修正で反映**。flag 化された場合のみ landing 後に有効化する。

## 検証 (landing 後)

- None / Highlight / Fold それぞれで C / root / in-scale ラベルがはっきり読める。
- root を白鍵 (C/D/E…) と黒鍵 (C#/F#…) の両方に設定して、どちらでも読める。
- 実機 `C2` (warm cream 背景) で確認。
