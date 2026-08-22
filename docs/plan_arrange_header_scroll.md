<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: arrangement トラックヘッダ上のマウスホイール縦スクロール

## 背景

アレンジビューはマウスホイールで以下を行う:

- plain wheel → 縦スクロール (`track_top`)
- Alt+wheel → 縦ズーム (`row_h`、マウス Y を anchor)
- Ctrl+wheel → 横ズーム (`zoom_x`、マウス beat を anchor)
- Shift+wheel → 横スクロール (`scroll_x`)

しかし現状これらは **lanes キャンバス領域 (ruler 下・header 右)** の上でしか効かない。
gui_01 widget の `arrangement.rs` が `let scroll = self.take_scroll_in_rect(lanes);`
(arrangement.rs:7672) で **`lanes` rect のみ** からスクロールを取得しているため、
カーソルが **track header pane (左 160px の列)** にあるときはホイールが完全に無反応。

一般的な DAW (Ableton / Cubase / REAPER) では、トラックリスト (= header 列) の上でも
ホイールで縦スクロールできる。ここを揃える。

## 理想 (最終形態)

「ruler より下の全域」(= track header pane + lanes canvas、master row header /
automation lane header 列も含む) でホイールが効く。カーソルが header 上 / lanes 上の
どちらでも **縦操作** が同一挙動になる:

- plain wheel → 縦スクロール (`track_top`)。現 lanes 挙動 (`track_top - dy*8.0`) と同一。
- Alt+wheel → 縦ズーム (`row_h`、マウス Y を anchor)。現 lanes 挙動と同一。

**横操作** (Ctrl=`zoom_x` / Shift=`scroll_x`) は時間軸を持たない header 上では発火しない
(no-op)。lanes 上は現状維持。
※ Ctrl zoom は `mx - lanes.x` を beat anchor にするため header 上 (mx < lanes.x) では
意味を成さない。header 上の Ctrl/Shift ホイールは無視するのが正しい。

## 担当境界

- スクロール量・縦ズーム曲線・clamp は widget が所有 (SSoT)。daw_01 側で `-dy*8.0` 等を
  複製しない (DRY 違反 / 二重管理になる)。
- よって本変更は **gui_01 widget 側** で行う。daw_01 は `SetTrackTop` / `SetTrackRowH`
  等の既存 Edit を受けるだけで wire 不要 (= header からのスクロールも同じ Edit 経路)。

## 実装方針 (gui_01 widget 側)

`take_scroll_in_rect(lanes)` を、header pane を含む「ruler 下の content 全域」 rect に
広げる。header 上で発火した場合は plain / Alt のみ適用し、Ctrl / Shift は早期 return。
lanes 上の挙動は完全に現状維持。
