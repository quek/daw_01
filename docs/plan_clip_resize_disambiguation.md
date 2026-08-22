<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# plan: 隣接クリップ resize の端つかみ優先規則 (FIXME #43)

## ゴール

アレンジビューで 2 つのクリップが隣接 (A の右端 == B の左端) しているとき、A の右端を
掴んでリサイズしようとすると B の左端リサイズになってしまうバグを直す。**カーソルが視覚的に
乗っている側のクリップの端を掴む** ようにする (境界より左にカーソルがあれば左クリップの右端、
右にあれば右クリップの左端)。

原因: gui_01 の `clip_hit()` がリサイズ判定でクリップを順に走査し「ループ後勝ち」で上書き
するだけ。隣接クリップは互いの resize handle 帯 (`resize_handle_px`) が共有境界の両側に
張り出して重なるため、A の内側 (例 cx=159) でも storage 順で後ろの B が勝ち、左を掴んだのに
右がリサイズされる。

ピアノロールの note resize は既に同じ問題を `note_hit_in()` の優先規則で解決済み (daw_01 #053)。
本件はその規則をクリップ側へ移植するだけ。

## 確定設計 (インタビュー済、再議論しない)

- カーソルが乗っている方のクリップの端が勝つ (= ノートと同じ規則)。常に左優先やホバー
  ハイライト等の別案は採らない。
- 修正の主体は **gui_01 の arrangement widget** (`clip_hit`)。daw_01 側に配線は不要
  (`clip_hit` は widget 内部で完結し、caller は同一シグネチャのまま正しい結果を受け取る)。

## (A) gui_01 要望仕様

> 提出先: `docs/gui_01_conversation.md` に `[要望]` として追記。
> 関連仕様: `docs/plan_clip_resize_disambiguation.md` を必須で含める。
> 段階分割せず最終形態を全部書く。

`crates/ui/src/widgets/arrangement.rs` の `clip_hit()` (~1675-1700) のループを、
`crates/ui/src/widgets/piano_roll.rs` の `note_hit_in()` (~913-950) と**構造的に同一**の
優先規則に書き換える。

### 優先規則 (note_hit_in と同じ)

各クリップ候補について `clip_zone_at` (~1635-1668) が返す zone (`ClipDragKind`) と、
「カーソルがそのクリップの rect 内側か (`inside`)」「リサイズ端までの水平距離 (`edge_dist`)」
を求め、以下で勝者を決める:

1. **in-rect は張り出しハンドルに無条件で勝つ** (`inside == true` が `inside == false` に勝つ)。
2. 同じ tier (両方 in-rect or 両方 outer) なら **端までの距離が近い方が勝つ**。
3. 距離も同点なら **後勝ち** (storage 順、現状と同じ。クリップは sort されていないが
   この規則は sort に依存しない)。

置換対応: Note→clip、`note_zone_at`→`clip_zone_at`、`note_to_rect`→`clip_to_rect` (~1526-1538)、
`NoteId`→`ClipKey` (ループ内)、`NoteDragKind`→`ClipDragKind` (~451)。

### 引数・lint

`clip_hit` は 7 引数 (clippy `too_many_arguments` は >7 で警告 = 7 はOK)。`clip_zone_at` は
8 引数で `#[allow(clippy::too_many_arguments)]` を ~1634 に持つ。新ループは引数を増やさない
ので新規 lint なし。`clip_hit` の `#[must_use]` はそのまま。

### テスト (gui_01)

- **旧テスト `clip_hit_adjacent_clips_back_wins_at_shared_handle` (~8957-8975) は削除** する
  (rename ではなく削除。cx=161 ケースは新テストに包含され、DRY)。
- 新テストを `note_hit_adjacent_notes_inside_note_owns_shared_handle` (piano_roll.rs ~3270-3293)
  と対になる形で追加。test_view `len_beats=16` / test_lanes `w=640` ⇒ `beat_to_px=40` なので
  A=[0,160] / B=[160,320]、境界=160、edge=4 ⇒ 張り出し [156,164)、cy=16 は track-0 rect y[2,30]:
  - `cx=159` → A `ResizeRight` (回帰ケース。旧 last-wins では B だった)
  - `cx=161` → B `ResizeLeft`
  - `cx=160` → B `ResizeLeft`
- 既存の単一クリップ assertion 8 件 (center/left/right/none/outer-left/outer-right/none-past/
  short-inside/short-outer) は**すべて不変**であることを確認 (孤立候補は常に採用される
  = 1 周目 `hit_inside=false`、`inside==false`→`better=(dist<=INF)=true`、`inside==true`→
  `better=inside=true`)。

### 影響なし (検証済)

- RT-audio: なし (`clip_hit` は UI/event スレッド専用、~4871/6109/7623/7648/7914/8015 から呼ばれる。
  `clip_to_rect` は Copy な Rect を返し、heap/lock/IO なし)。
- protocol/bincode: なし (`ClipKey`/`ClipDragKind` は widget-local 型で Encode/Decode なし)
  ⇒ `cargo build --workspace` は protocol 理由では不要。
- 全 6 caller は同一シグネチャ `(visible_tracks, tops, view, lanes, cx, cy, resize_handle_px)
  -> Option<(ClipKey, ClipDragKind)>` のまま。
- 真の重なり (同 tier・同距離) は `dist <=` で後勝ち = 現状の last-wins と同一。新たな順序
  仮定を導入しない。
- 範囲外: `automation_clip_zone_at` (~4073)、`audio_grip_hit_in_lanes` (~1602) は本件対象外。

## (B) daw_01 側配線

**なし。** `clip_hit` は widget 内部呼び出しのみで、daw_01 はシグネチャを通じて正しい結果を
受け取るだけ。landing 後に `cargo build -p daw_gui` が通り、実機で隣接 2 クリップの左端/右端
リサイズが意図どおりになることを目視確認するのみ。

## エッジケース

- カーソルが完全に境界線上 (cx=160): B の rect が 160 を含むので B `ResizeLeft` (新テストどおり)。
- 完全重なりクリップ (overlap): 同 tier・同距離なら後勝ち = 現状維持。
- リサイズ帯幅 (`resize_handle_px`) は caller が渡す値をそのまま使用 (MOVE 判定と同じ値)。

## ビルド/検証

- gui_01 (要望 landing 後、gui_01 session 側): `cargo test -p daw-ui` (新テスト + 旧テスト削除)、
  `cargo clippy -p daw-ui -- -D warnings`。
- daw_01: `cargo build -p daw_gui` → 実機で隣接クリップの端リサイズを目視 (左クリップの右端を
  掴んで左クリップが伸縮すること)。
- 本件は protocol 変更なしだが、commit 後は規約どおり `cargo build --workspace --release` で green 確認。

## 待機中の進め方

daw_01 側に先行実装できるものは無い (配線ゼロ)。要望提出後は landing を待ち、landing 後に
実機目視のみ。
