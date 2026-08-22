<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_pianoroll_song_absolute — piano roll を song-absolute 座標系に統一する

FIXME #3。「MIDI エディタのルーラが 5bar から始まるクリップでも常に 1bar から
始まる」を根治する。

## 真因 (root-cause workflow 2026-06-08・敵対的検証済 high)

piano_roll widget は **ruler / bar-beat grid / note / playhead / loop band /
velocity lane を全て単一の `view.start_beat` を原点とする 1 つの座標系**で描く
(gui_01 widget は正しい汎用 API、バグは無い):

- ruler/grid は `view.start_beat * samples_per_beat` を `samples_to_bar_beat`
  に通す。`samples_to_bar_beat` は **sample 0 = bar 1** 固定基準
  ([gui_01 time.rs:50-57](gui_01:crates/ui/src/time.rs)、
  [piano_roll.rs:1937-1939](gui_01:crates/ui/src/widgets/piano_roll.rs))。
  = widget は `view.start_beat` を **song-absolute beat** として解釈する。
- playhead は `(b - view.start_beat) * beat_to_px`、velocity は
  `(n.start_beat - view.start_beat)`、note も同じ `view.start_beat` 原点。

ところが daw_01 caller は座標空間を**混在投入**している:

| widget へ渡す値 | 現状の空間 | 正しい空間 |
|---|---|---|
| `view.start_beat = pianoroll_scroll_beat` ([piano_roll_view.rs:121](F:/dev/daw_01/daw_gui/src/view/piano_roll_view.rs)) | clip-local (`FitPianoRollToClip` が `min_beat-1.0` を入れる [app.rs:10039](F:/dev/daw_01/daw_gui/src/app.rs)) | song-absolute |
| `notes[].start_beat` (build_widget_notes) | clip-local (共有 content のため必須) | song-absolute (描画時のみ) |
| `view.playhead_beat` ([piano_roll_view.rs:128](F:/dev/daw_01/daw_gui/src/view/piano_roll_view.rs)) | song-absolute | song-absolute |
| `view.loop_range` (`song.loop_start/end_beat`) | song-absolute | song-absolute |

→ ruler は `view.start_beat(=clip-local)` を song-absolute と解釈するため、
5bar 始まり clip でも scroll=0 → ルーラ左端が bar1。さらに **playhead / loop が
clip.start_beat ぶんズレて描かれる**（5bar 始まり clip では playhead が
`[view.start_beat, +len_beats]` 範囲外に出て [piano_roll.rs:2099-2101](gui_01:crates/ui/src/widgets/piano_roll.rs)
の可視判定で **再生線が全く描画されない**症状にもなり得る）。

## 確定仕様 (grill-me 2026-06-08)

**piano roll 全体を song-absolute 座標系に統一する。** clip.start_beat を
唯一の絶対オフセット SSoT とし、`view` 入口で加算・model 書き戻し出口で減算する。
グリッド / 小節線は **曲の拍子境界 (song downbeat)** に揃える (clip が小節途中
から始まっても小節線は曲の拍子境界に出る = アレンジ画面と完全一致)。

| # | 項目 | 内容 |
|---|---|---|
| 1 | view 入口 | `view.start_beat = pianoroll_scroll_beat + clip.start_beat`。scroll 自体は **clip-local 保持** (案A。Fit / wheel zoom-anchor / scroll の既存ロジック無改修、変換は view 構築 1 箇所に局所化) |
| 2 | notes | build_widget_notes が各 note を `n.start_beat + clip.start_beat` で song-absolute 化して渡す。model の `Note.start_beat` は clip-local のまま不変 (共有 content を別 start_beat の clip が参照するため絶対化不可) |
| 3 | 出口変換 | widget から返る beat 値を model (clip-local) へ書く前に `- clip.start_beat`。対象 handler: SetPlayheadBeat / SetLoopRange / AddNote / MoveNote(set_note_positions) / ResizeNote。**handler 入口で 1 箇所減算**すれば下記 2 経路を一括カバー |
| 4 | gui_01 | **変更不要**。widget の `view.start_beat` は song-absolute を受け取れる正しい API |
| 5 | 副次治癒 | playhead / loop band の clip.start_beat ぶんのズレ・非表示も同時に解消 (ruler-only fix より根治) |

### 出口変換の 2 経路に注意

AddNote を発火するのは widget の `make_edit` 経由だけでなく、**double-click
AddNote** ([piano_roll_view.rs:271-287](F:/dev/daw_01/daw_gui/src/view/piano_roll_view.rs)
が `beat_raw = view.start_beat + (px-grid.x)/beat_to_px` を view 内で直接計算)
も存在する。view を absolute 化すると double-click 由来 beat も absolute になる。
両経路とも最終的に `add_note` handler ([app.rs:12552](F:/dev/daw_01/daw_gui/src/app.rs))
に集約されるので、**減算を view 入口/make_edit ではなく handler 入口
(add_note / set_note_positions / resize_notes / SetPlayheadBeat / SetLoopRange)
に置けば両経路を 1 箇所で吸収**する (= 描画系 hit-test は view 共有で自動追従、
変換が要るのは「model へ書き戻す出口」のみ、という責務分界)。

## plan_pianoroll_ruler.md との関係

`docs/plan_pianoroll_ruler.md` (ルーラを操作可能にする feature) は座標系を
「**clip-local + caller が push-back 時に offset 加算**」と決めていたが、
これは ruler 表示が clip-local bar (1,2,3…) になる = **本 FIXME #3 のバグそのもの**。
本 plan で **その座標決定を破棄**し、「view 全体 song-absolute / 出口で減算」に
統一する。ruler 操作 (seek / loop) の挙動仕様自体 (plain click=seek 等) は
plan_pianoroll_ruler.md のまま有効。

## 受け入れ基準

- 5bar (= beat16) から始まる clip を開く → ルーラ左端が bar5、小節線が
  5,6,7… と曲の拍子境界に出る。clip が小節途中始まりなら左端は途中拍、
  最初の小節線は途中に出る。
- ノート x がアレンジ画面のクリップ内ノート位置と一致 (同じ拍が同じ小節)。
- 再生中、playhead 線が piano roll に表示され、曲位置に追従する (非表示バグ解消)。
- ルーラ click で playhead がその**曲位置**へ seek (audio engine seek IPC も
  曲位置で正しく飛ぶ)。loop band も曲位置に正しく出る。
- ノート add / move / resize / double-click add が clip-local model に正しく
  書かれる (= 既存挙動維持、座標往復で値が壊れない)。

## 非範囲

- gui_01 widget の変更 (本件は daw_01 caller の座標責務のみ)。
- 複数 clip を跨いだ同時 piano roll 編集 (1 clip 編集前提は不変)。
