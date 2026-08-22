<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Piano Roll ノート複製 (Ctrl+drag copy / D duplicate)

ピアノロールで選択ノートを複製する 2 経路を実装する。

1. **Ctrl+drag コピー** — ノートを Ctrl 押下したまま drag すると、元ノートを残したまま
   複製を drag 先へ配置する (Ableton Live / REAPER の Ctrl+drag duplicate)。
   → gui_01 widget の drag セッションが Ctrl を認識し別 EditRequest を発行する必要がある
     (**gui_01 #054**)。
2. **D で複製** — ピアノロール上でノート選択中に `D` キーを押すと、選択ノート群を
   その選択範囲ぶん後ろにずらして複製し、複製を新しい選択にする (連打で後方へ連鎖)。
   → daw_01 完結 (shortcut 文脈ゲート + model 操作)。

両経路とも「選択ノートを deep clone して時間方向にずらし、元は据え置き」という同一の
model 操作 (`AppData::duplicate_notes`) に集約する (DRY)。

## 最終形態 (完成イメージ)

- **Ctrl+drag**: 選択ノート (単一/複数) を Ctrl+drag → drag 中は「元ノートはその場に残り、
  複製がカーソルに追従するゴースト」が見える。release で複製確定、複製が新選択になる。
  Ctrl 無し drag は従来どおり移動 (Move)。snap は Ctrl 有無に関わらず従来どおり適用。
- **D**: ピアノロール上 (= bottom panel が Piano Roll タブ + マウスが panel 内) でノート選択中に
  `D` → 選択ノート群を `(max_end - min_start)` ぶん後ろにずらして複製、複製を新選択に。
  選択が無ければ no-op。アレンジ文脈 (それ以外) では従来どおり `D` = クリップ複製。

## モデル操作 (daw_01, 両経路共通)

`AppData` に複製ロジックを実装する:

```rust
/// 選択中ノート (`selected_notes` の id 群) を複製する。
/// `beat_offset` ぶん start_beat を後ろにずらし、元ノートは据え置き、
/// 複製を選択中 clip の notes に追加して `selected_notes` を複製の新 id 群に更新する。
/// Undoable。複製先 clip は `selected_clip`。
fn duplicate_selected_notes(&mut self, beat_offset: f64) { ... }
```

- 複製ノートの id は clip 内 index ベース (`build_widget_notes` と整合) で再採番される。
- D 経路の `beat_offset` = 選択ノートの `max(start+dur) - min(start)`。
- Ctrl+drag 経路は gui_01 が `new_start_beat` / `new_pitch` を delta で渡すので、
  `beat_offset` ではなく drag 先座標で複製を置く (下記 #054 の `Copy` request 参照)。

## gui_01 #054 で必要な widget 拡張 (Ctrl+drag)

現状:
- `NoteDragSession` は `last_alt: bool` のみ保持、Ctrl/Shift を見ない
  (`crates/ui/src/widgets/piano_roll.rs:1003-1019`)。
- `PianoRollEditRequest` に複製 variant 無し。move は `Move(Vec<MoveDelta>)` のみ
  (同 :385)。
- 先行実装: arrangement widget は drag session に `last_ctrl` / `last_shift` を持ち、
  release で `CloneClipsLinked` を発行 (`arrangement.rs:1868`, `:6687`)。

要望 (最終形態):
1. `NoteDragSession` に `last_ctrl: bool` を追加 (`last_alt` と同じ「continuation frame で
   update、release frame は skip」careful-update パターンで OS event 順序非依存に)。
2. drag 中 `last_ctrl == true` のとき、**move overlay ではなく copy overlay** を描画
   (元ノートをその場に残し、複製ゴーストをカーソルへ追従)。
3. release frame で `last_ctrl == true` なら `Move` ではなく
   `PianoRollEditRequest::Copy(Vec<MoveDelta>)` を発行。payload は `Move` と同形
   (`(NoteId, prev_beat, prev_pitch, new_beat, new_pitch)`)。意味は
   「`NoteId` を **複製** して `new_*` 位置へ、元は据え置き」。
   - ノートは clip 内 raw data でリンク概念が無いため、arrangement の
     Linked/Independent 区別は **不要**。独立コピー 1 種でよい。

daw_01 側: `Copy(deltas)` を受けて `AppEvent::CopyNotes(entries)` に変換、
`duplicate_notes` で各 source を deep clone + `new_beat`/`new_pitch` に配置、
複製を新選択に。

## テスト (daw_01)

`AppData` model テスト (高レイヤー):
- 単一ノート複製: clip に 1 ノート → `duplicate_selected_notes(offset)` → ノート 2 個、
  2 個目が `start + offset`、`selected_notes` が複製の id。
- 複数ノート複製: 2 ノート選択 → 複製で 4 ノート、相対位置維持、選択が複製群。
- 空選択: no-op。
- undo: 複製後 undo で元の 1 ノートに戻る。
