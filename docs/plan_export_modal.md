<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: Video export を「真のモーダル」にする

## §0 目的

Video export 実行中（音声 freewheel render → 映像 render）、ユーザーが
再生・編集・FX 追加・トラック操作・**フェーダー / ノブのドラッグ**等を
一切できない「真のモーダル」状態にする。現状は modal パネルは出るが、
背景の widget が依然として pointer に反応してしまう。

## §1 現状の問題

実機テストで、export 中に mixer のフェーダーを**ドラッグするとつまみが
視覚的に動く**。値（`Track.volume`）は変わらない（後述の gate で drop
される）が、「動いて見えるのに反映されない」= 壊れて見える最悪の UX。

### 根本原因（一次情報で確認済み）

1. **gui_01 modal の dimming overlay は既に全画面に描画されている**
   （`gui_01/crates/ui/src/widgets/modal.rs:122-143`：`popup_layer` 内で
   画面全体の `overlay_color`（alpha 0.6 黒）→ 中央 panel の順に push）。
   よって**見た目（背景の暗転）は既に正しい**。漏れているのは**入力だけ**。

2. **背景 widget は popup_layer より前のフレーム前半で描画され、その時に
   pointer を消費する**。modal が pointer をブロックするのは
   `gui_01/crates/ui/src/ui.rs:966` `pointer_blocked_by_modal_popup()` だが、
   これは「pointer が **modal panel の anchor rect 内**にあり、かつ
   `drawing_in_popup == false`」のときだけ `true`。目的は「panel の**裏**に
   隠れた widget が panel 用の入力を盗まないこと」。**panel の外**にある
   widget（画面下の mixer フェーダー等）には適用されない。

3. さらに、この predicate を参照しているのは
   `take_scroll_in_rect` / `take_drag_rect_in_rect` / `take_double_click_in_rect`
   のみ（ui.rs:1204 / 1234 / 1455 / 1548 / 1674）。**`fader_at` は
   `let pointer = self.pointer;`（fader.rs:117）で pointer を raw に読み**、
   predicate を一切参照しない。つまり predicate を「panel 外もブロック」に
   修正しても、フェーダーは止まらない。**入力を pointer source の段階で
   masking する必要がある**。

## §2 理想の最終形態

**modal が 1 つでも開いている間、modal panel の内側（`drawing_in_popup ==
true`）以外の全 widget に pointer / keyboard 入力が一切届かない。**

- 背景 widget：hover / press / drag / double-click / scroll / keyboard すべて無反応
- panel 内 widget（Cancel ボタン等）：通常どおり動作
- 見た目（背景の暗転 + panel）は現状のまま（変更不要）
- export 専用ではなく**全 modal**（plugin picker / save 確認 / recovery /
  export 進捗）に効く SSoT な挙動

これは gui_01 の modal/popup システムの capability であり、daw_01 側では
実現できない（背景 widget の pointer 読みは gui_01 の `Ui` が所有する）。
→ **gui_01 への要望（#065）として提出**。実装方針は gui_01 に委ねる。

## §3 daw_01 側（gui_01 #065 では届かない部分・恒久的に必要）

gui_01 #065 が遮断するのは「main window 上の widget への pointer/keyboard 入力」
だけ。以下は #065 が入っても **redundant にならない**（= gui_01 を経由しない、
または別 OS window の経路）ため daw_01 側に残す。

1. **`AppData::handle_event` 冒頭の export gate**（`daw_gui/src/app.rs:3854`）：
   `pending_video_export.is_some() || export_progress.is_some()` の間、
   `ExportWavComplete` / `ExportProgress` / `ExportFinished` / `CancelExport`
   以外の AppEvent を全 drop（undo snapshot push の前に return）。
   **恒久的な存在意義は非 UI イベント源**：
   - **MIDI ハードウェア入力スレッド**（`daw_gui/src/midi.rs:49 dispatch` が
     `proxy.send_event(AppEvent::MidiNoteOn / MidiControlChange)` を直送）。
     export の音声 freewheel 中のライブ MIDI / CC（MIDI Learn 経由で TrackVolume
     も動く）が offline render を乱すのを防ぐ。背景スレッド発なので #065 では
     遮断不可。
   - **IPC bridge**（ChildToMain → AppEvent）。
   widget 由来の UI event もここで結果的に落ちるが、その視覚的遮断は #065 の責務。

2. **`on_tick` の `CloseSlotGui` gate**（app.rs の per-frame plugin GUI close
   経路）。plugin GUI は **daw_gui が所有する別 top-level OS window**
   （`view/plugin_embed.rs`）で gui_01 widget ではない。main window の modal は
   他 OS window を無効化しないので、#065 では遮断不可。handle_event も通らない
   ので per-frame で個別 gate。

3. **export overlay**（`daw_gui/src/view/export_overlay.rs`）：音声 render
   フェーズ（indeterminate）+ 映像 render フェーズ（フレーム進捗 + Cancel）の
   両方で `ui.modal` を表示。**これが modal を「開く」主体**。#065 は「開いた
   modal の入力遮断挙動」にすぎず、overlay が無ければ export 中に modal は出ない。

### 撤去したもの

- **runner の keyboard ingest gate**（旧 `runner.rs`）は撤去した。shortcut は
  次フレームの `take_shortcut → handle_event` を通るので correctness は §3-1 の
  gate が担保する。「modal 中は背景 widget に keyboard を届かせない」視覚的遮断は
  gui_01 #065 の責務であり、daw_01 側の暫定 keyboard 遮断は #065 が subsume する
  interim workaround だった（CLAUDE.md「gui_01 要望を出す前に interim 実装に
  走らない」原則）。

## §4 gui_01 側（要望 #065・実装済み = Resolved 2026-06-01）

gui_01 Phase 94 で実装済み。**全 `ui.modal` が default で真のモーダル**になり、
開いている間 `drawing_in_popup == false` の widget が読む `Ui::pointer` を
1 箇所で masking（pos = None / buttons false / scroll 0）+ keyboard
（`take_shortcut` 系）を panel 外で遮断する。`fader_at` 等の raw 読み widget も
per-widget 修正ゼロで自動 inert。panel body は生 pointer に戻すので Cancel 等は
動作。見た目・outside-click close・ESC close は不変。

- **daw_01 は無修正**（`ModalStyle` に field 追加せず `open_modal` 内部で
  `capture_input=true`。`cargo check -p daw_gui` clean）。
- export 進捗 modal（`export_overlay.rs`、`close_on_outside_click: false` /
  `close_on_escape: false`）も自動で真のモーダル化 → **export 中フェーダーが
  動いて見える症状は解消**。
- 詳細は `docs/gui_01_conversation.md #065`。

## §4.5 背景クリックで modal がフラッシュ（gui_01 #066 で解決済み 2026-06-01）

実機で、export 中に panel 外（背景）をクリックすると画面が一瞬フラッシュする。

原因（一次情報で確定）：gui_01 `popup_layer`（`crates/ui/src/ui.rs:947-974`）は
outside-click を検出すると **`ModalStyle.close_on_outside_click` を参照せず常に
auto-close** する（この field は modal.rs:30-33 のコメントどおり「意味的フィールド
のみ」で非機能）。その結果：

1. 背景クリック → `popup_layer` が popup を remove（closure 未実行 = overlay+panel
   描画されず）→ その 1 フレームだけ明るい背景 UI が露出 = **フラッシュ**。
2. 次フレームで `export_overlay::draw` が `is_modal_open == false` を見て
   `open_modal` で再 open。

export 進捗は「Cancel / ESC でしか閉じない」blocking modal なので
`close_on_outside_click: false` を honor してほしい。→ gui_01 #066。
daw_01 側の回避策（panel を全画面化等）は dialog の見た目を壊すので採らない。

**ESC は許可**（ユーザー方針）。ただし gui_01 の `close_on_escape: true` は
「ESC フレームで modal が閉じ overlay 未描画 → 1 フレームのフラッシュ」を起こす
（outside-click と同機序）。これを避けるため `close_on_escape: false` のままにし、
**`export_overlay` の body 内で `take_shortcut("escape")` を拾って `CancelExport`** を
発火する（= Cancel ボタンと同一経路）。modal は閉じず「キャンセル中」を表示し、
`ExportFinished` で `active=false` になって自然に閉じる → フラッシュなし。

## §5 検証

- export 中、mixer フェーダー / pan ノブ / arrangement のクリップドラッグ /
  piano roll / track header / transport ボタン / メニューが**視覚的にも一切
  反応しない**こと（実機）。
- export 中の Cancel ボタンは効くこと。
- 音声 freewheel → 映像 render の連続表示。
- 既存の他 modal（plugin picker 等）が引き続き正常動作すること（回帰）。
- `cargo clippy --workspace -- -D warnings` clean。
