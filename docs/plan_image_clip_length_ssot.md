<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan_image_clip_length_ssot — 画像/動画 clip は「clip 長 = 表示長」を不変条件にする

FIXME #6。「20260512.daw で 9bar (= beat32) 以降、口以外の画像と動画が表示
されない」を根治し、再発を構造的に不能にする。

> **Text も対象 (2026-06-08 追補)**: 同じ不整合が **Text overlay** でも発生する
> (例 20260512.daw のクレジット「ボーカル VOICEVOX:中国うさぎ」= clip@0+48 だが
> TextEvent.event_length=4 → bar2/beat4 で消える)。`text_compose` の可視判定も
> image/video と同じ clip+event 二重 gate のため、不変条件を image / video / **text**
> の 3 種 (= overlay 全般) に拡張する。下記の関数名も `ensure_overlay_event_coverage`
> / `ClipContent::ensure_event_covers_clip` に統一済。

## 真因 (root-cause workflow 2026-06-08・敵対的検証 + 実データ二重証明 high)

image/video clip の可視判定は **CLIP 範囲 AND EVENT 範囲の二重 gate**:

- [image_compose.rs:114-121](F:/dev/daw_01/daw_gui/src/image_compose.rs):
  `event_end = event.event_start_in_clip_beats + event.event_length_beats;
  if clip_local < event_start || clip_local >= event_end { continue; }`
- [video_playback.rs:394-400 / 468-474](F:/dev/daw_01/daw_gui/src/video_playback.rs) も同型。

ところが clip 右端 drag の `resize_clip` ([app.rs:12102-12139](F:/dev/daw_01/daw_gui/src/app.rs))
は **`ClipContent::Audio` の event しか同期せず、Image/Video event を放置**する
(doc-comment [app.rs:3270-3275](F:/dev/daw_01/daw_gui/src/app.rs) も audio のみと明記)。
import 直後は clip 長 = event 長で一致するが (image 既定
`image_clip_length_beats = (song.length_beats*0.5).max(8.0)` [app.rs:15028]、video は source 長)、
clip を後から伸ばすと **clip 長だけ伸び event 長が据え置き**になる。

### 実データ (20260512.daw) で確定

立ち絵 body (眉/黒目/白目/右腕/左腕/!体 = content 16/13/14/11/12/10) と
video (cid107) は全て **clip = 0..48 だが単一 ImageEvent が
`event_start=0, event_length=32` (= end beat32 = bar9)**。song.length_beats=64
なので 32 は import 既定値そのまま。playhead が beat32 を越えると clip 範囲
(0..48) 内でも event 範囲 (0..32) を抜け、event_end gate で skip → 消える。
口 (track id17, content 931..937) は別クリップが beat 0..64 を 4/16-beat 窓で
連続配置 → 常にどれか active で**生存**。「口だけ残る」が完全に説明できる。
(fade=0 / opacity=1 / muted=false を確認済、alpha=0 消失は排除。原因は
event_end gate のみ。逆向き不整合 content30/27 = clip4/event32 も存在し、
resize が image event を同期しない事実の独立した裏付け。)

## 確定仕様 (grill-me 2026-06-08)

**単一画像/動画 clip は「clip 長 = 表示長」。** タイムライン上に clip がある
間は必ず表示される。部分表示は fade / split で表現する。event 列は
multi-event (口パク等) のための clip 内タイル割りの SSoT として残すが、
**単一 event は常に clip 全長 `[0, clip.length_beats]` を張る**ことを不変条件
にし、不整合が構造的に起こり得ないようにする。

| # | 項目 | 内容 |
|---|---|---|
| 1 | resize 同期 | `resize_clip` の Audio 専用分岐を **全 content kind 共通**に作り直す。右端 trim: 単一 event は `event_length = clip.length - event_start` で clip 追従 (image は source 無限長で無条件、video は source_end_micros clamp、audio は既存 source clamp)。左端 trim: event を delta_start スライド (既存 audio ロジックと同思想) |
| 2 | multi-event | split 済み複数 event を持つ clip の右端伸長は **末尾 event のみ** clip 右端まで伸ばす (audio の max_event_len clamp と同じ思想)。先頭側 event は据え置き。口パク等の event 単位 gate は不変 |
| 3 | load 修復 | load 時 (`ensure_*` 系) に不変条件を強制: **単一 event の image/video clip は event を `[0, clip.length_beats]` に正規化**。これで既存 .daw (20260512.daw 含む) が **再 drag 不要で自動修復**され bar9 以降も再表示される |
| 4 | event_length 廃止しない | 単に「event 範囲 gate を撤廃して clip 範囲だけにする」案は multi-event (split / 口パク) の境界情報を失うため不可。event を SSoT に残し edit/load で同期するのが理想 |

### TrimImageEvent との整合

event 単位 trim handler ([app.rs:11682-11720](F:/dev/daw_01/daw_gui/src/app.rs)、
`event_length_beats` を直接操作し clip を auto-expand) は本件と独立操作。
trim 後は event==clip に保たれるので、`event < clip` の単一 event は
**resize バグ以外に発生経路が無い** → load 正規化は安全 (ユーザー意図を壊さない)。
resize 共通化時はこの handler と二重伸長しないよう SSoT を確認する。

## 受け入れ基準

- 20260512.daw を開く → 立ち絵 body と video が **bar9 以降も最後まで表示**
  (load 修復で event が 0..48 に正規化)。口パクは従来通り。
- 画像/動画 clip の右端を drag で伸ばす → 画像が clip 全長で表示され続ける
  (単一 event が追従)。縮める → それに追従。
- 口パク (multi-event) clip の各口形状は従来通り event 単位で切り替わる
  (= regression なし)。

## 非範囲

- 部分表示専用 UI (clip 内で画像を一部期間だけ出す独立 UX) は今回作らない
  (fade / split で表現)。
- multi-event clip の中間 event 境界の編集 UX は本件外。
