# FIXME #83 — MIDI ノートを重ねない (Bitwig / Ableton 流)

## ゴール

同一ピッチの MIDI ノートが時間的に重ならない不変条件を、ノート編集の全経路で常時強制する。
異なるピッチは自由に重なれる (= 和音)。Ableton / Bitwig 同様、トグルは設けず常時 ON。

## 一次情報 (調査済)

- **同一ピッチのみ**対象。MIDI 仕様上、同一 note/ch で NoteOff 前の 2 つ目の NoteOn は invalid
  (Ardour manual)。異なるピッチは和音として共存。
- **last-note-wins**: 後から置いた / 編集したノートが勝ち、既存を切り詰める
  (REAPER manual "last note … is always preferred"、Ableton manual)。
- **末尾重なり** (loser が winner より前に始まる) → loser の末尾を winner 開始位置でトリム。
- **完全被覆** → loser 削除。
- **長いノートの中央に短いノート** → トリム (切り詰め)。**auto-split しない** (split は別操作)。
  全 DAW 共通 (Ableton / REAPER / Ardour)。
- **先頭重なり** (winner が loser の先頭を覆い、loser が winner より後ろまで伸びる) →
  **loser の後半を残す** (開始を winner 終端へ前送り)。REAPER 流の最も非破壊的な挙動。
  ※ Ableton は丸ごと削除するが、ユーザー判断で「後半を残す」を採用 (2026-06-21)。
- **録音 (リアルタイム MIDI overdub) には適用しない**。Ableton / Bitwig も overdub は別系統で
  重なりを残す。本機能はエディタ操作のみ。

## SSoT: 解消は 1 つの純関数に集約

```rust
/// 同一ピッチの重なりを解消し、不変条件を強制する。
/// winners = 直前に追加/移動/サイズ変更/コピーされたノート index (衝突時に勝つ側)。
/// 戻り値は古い index → 新 index の remap 表 (削除されたら None)。
/// caller は remap_indices() で selected_notes / 新規 winner id を写し替える。
fn resolve_note_overlaps(notes: &mut Vec<Note>, winners: &[u32]) -> Vec<Option<u32>>
fn remap_indices(remap: &[Option<u32>], idxs: &[u32]) -> Vec<u32>
```

### アルゴリズム

1. **Phase B (winner 同士)**: pitch ごとに start 昇順に並べ、隣接する winner が重なれば
   前のノートの末尾を後のノート開始位置でトリム (後勝ち)。長さ 0 になれば削除。
   → 時間量子化・ピッチ量子化・glue でのみ発生 (move/copy 等は並進なので不変)。
2. **Phase A (winner → loser)**: 各 winner について同一ピッチの loser を走査し:
   - 完全被覆 → loser 削除
   - loser が winner より前に始まる → loser 末尾を winner 開始でトリム (末尾重なり + 中央挿入 = truncate-not-split)
   - それ以外 (loser 先頭が覆われ後半が残る) → loser 開始を winner 終端へ前送り、後半を残す
   - トリム結果が ε 以下になれば削除
3. **削除適用 + remap**: 削除は降順 index で `Vec::remove` (既存 `delete_selected_notes` と同 idiom)。
   remap 表 = 古い index から「自分より前の削除数」を引いた新 index (削除は None)。

ノート ID は notes Vec の index (安定 ID 無し、widget が毎フレーム enumerate で再採番)。
widget は commit-by-release なので各操作で解消は 1 回。notes は content_id 単位所有 = linked clip
全 sibling に自動反映 (SSoT)。

## 適用箇所 (sibling-occurrence 全列挙)

| 関数 | winners | selection |
|---|---|---|
| `add_note` | 新規 note | 新規 (remap 後) |
| `set_note_positions` (move) | 移動した index | 移動 note (remap) |
| `resize_notes` | リサイズした index | リサイズ note (remap) |
| `resize_note` (単一) | 当該 note | 既存 selection を remap |
| `copy_notes` (Ctrl+drag) | 複製 id | 複製 (remap) |
| `duplicate_selected_notes` (D) | 複製 id | 複製 (remap) |
| `paste_notes_at` | 貼付 id | 貼付 (remap) |
| `step_input_note_on` | 新規 note | 新規 (remap) |
| `quantize_selected_notes` (時間) | 選択 index | 選択 (remap) |
| `quantize_pitches_to_scale` | pitch 変更した index | 既存 selection を remap |
| `action_glue_selected_clips` | 合成 note 全部 | (note selection 無し) |

**適用しない**: `record_midi_note_on/off` (録音 overdub は対象外)、`set_note_velocities` /
`set_note_lyrics` / `set_clip_voice` 等 (位置・長さ・ピッチを変えない)、
`stretch_clip_content` / `split_clip_at_beat` / `Song::split_clips_at` (線形写像・分割で新たな重なりを生まない)。

## テスト (純関数、`mod note_overlap_tests`)

`fn note(start, dur, pitch)` ヘルパーで Vec<Note> を組み `resolve_note_overlaps` を直接検証:
完全被覆=削除 / 末尾重なり=トリム / 先頭重なり=後半残す / 中央挿入=truncate-not-split (note 数不変) /
異ピッチ不干渉 / 隣接は不干渉 / winner 同士の量子化衝突 / 同位置 winner の dedup / remap 表の正しさ。
