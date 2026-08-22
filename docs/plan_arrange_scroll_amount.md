<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_arrange_scroll_amount — アレンジ縦ホイールスクロール量を縮小する

FIXME #11。「アレンジメントのマウスホイールによる縦スクロール量を小さくしてください」。

## 現状 (2026-06-09)

アレンジの縦スクロールは gui_01 arrangement widget が wheel delta から最終 `track_top` を
算出して `SetTrackTop` を emit し、daw_01 は受け取って `arrange_track_top` に格納するだけ
([arrangement_view.rs:1658-1665](F:/dev/daw_01/daw_gui/src/view/arrangement_view.rs)、書き戻しは :1663)。
スクロール量は **gui_01 内部にハードコード**で、daw_01 側に調整フックは無い
（`ArrangementView` にスクロール感度フィールド無し）。

二重スケールで量が過大になっている:
1. gui_01 入力層が wheel 1 line を **×40px** に変換（`LINE_HEIGHT_PX = 40`、
   [gui_01 input.rs:8,159](gui_01:crates/ui/src/input.rs)）。
2. arrangement widget がその px 差分を **さらに ×8**
   （`new_top = (view.track_top - dy * 8.0).max(0.0)`、
   [gui_01 arrangement.rs:7743](gui_01:crates/ui/src/widgets/arrangement.rs)）。

→ 1 ノッチ = `1 × 40 × 8 = 320px`。行高 40px なら 1 ノッチで約 8 行飛ぶ。他のスクロール領域
（`scroll_area` 等）は ×8 をせず 40px/line のまま使う
（[gui_01 scroll_area.rs:117-118](gui_01:crates/ui/src/widgets/scroll_area.rs)、input.rs:42 の
コメント参照）ので、arrangement だけが異常に速い。

## 確定仕様 (grill-me 2026-06-09)

**1 ノッチ ≒ 1 トラック行**にする（他のスクロール領域と同じ 40px/ノッチ）。arrangement
widget の plain-wheel 縦スクロールにある **二重スケールの ×8 を撤去**し、入力層が既に px 化
した delta をそのまま使う。

これは gui_01 側の修正（**gui_01 #088**）。daw_01 側に変更は不要。

| # | 面 | 修正 | 担当 |
|---|---|---|---|
| 1 | arrangement 縦スクロール | `arrangement.rs:7743` の `dy * 8.0` を `dy`（= ×8 撤去）にし、40px/ノッチ ≒ 1 行へ。Alt+wheel（縦ズーム）/ Ctrl+wheel（zoom_x）/ Shift+wheel（scroll_x）は不変 | **gui_01 #088** |

## gui_01 要望

`docs/gui_01_conversation.md` #088 で arrangement の plain-wheel 縦スクロールの ×8 二重スケール
撤去を要望（本 plan を関連仕様として参照）。memory の方針に従い daw_01 側で interim 実装は
しない（gui_01 landing 後に実機検証）。

## 受け入れ基準

- アレンジでマウスホイール 1 ノッチの縦スクロールが約 1 トラック行分になる。
- Alt+wheel の縦ズーム、Ctrl+wheel の横ズーム、Shift+wheel の横スクロールは従来どおり。
- track header pane 上でも lanes 上でも同じ量（#075 の挙動を維持）。

## 非範囲

- 横スクロール / ズーム系の量。
- daw_01 側の interim 実装。
