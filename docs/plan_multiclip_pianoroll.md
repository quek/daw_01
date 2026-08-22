<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# FIXME #93 — 複数選択クリップの同時ピアノロール表示・編集

「Ctrl+Click などで複数選択したクリップを同時にピアノロールに表示し編集」する。よくある DAW の機能。
業界標準は一次情報で調査済 (Ableton / FL / Cubase / Studio One / Bitwig / Logic / REAPER / Cakewalk)。

## 確定した設計 (grill-me 2026-06-27)

| # | 決定 | 出典 |
|---|------|------|
| 表示トリガ | 選択追従。`selected_clips` の MIDI クリップを**全部同時表示**。Ctrl+Click 等で選択を変えると追従 | user 要望 |
| A 編集モデル | **全クリップ編集可**。非対象クリップのノートも淡色で表示しそのまま掴める。個別 lock で参照専用化可 | user (推奨) |
| B トラック跨ぎ | **可**。別トラックのクリップも混在表示・編集 | 理想 (Cubase/Logic/Bitwig 標準) |
| C 色分け | **常にトラック色** (`effective_track_color`)。velocity は明るさ。対象トラックは鮮やか・非対象トラックは淡色。単一表示時もトラック色 | user (2026-06-27 実機レビューで clip 色→**トラック色**に変更) |
| D 対象切替 | 凡例 (legend) の**トラック行**クリック or ノートクリックで「対象トラック」を切替。対象 = `selected_clip` anchor の所属トラック (SSoT)。凡例クリックは anchor をそのトラックの代表クリップへ移す | user「選択可能」 |
| E 新規ノート所属 | **対象クリップ** (anchor) へ入る (描いた位置に依らず) | user「選択可能」 |

座標系は既に song-absolute 統一済 (FIXME #3) — 複数クリップ重畳の基盤あり。

> **2026-06-27 実機レビューでの確定 (上表 C/D/legend を上書き)**: 右パネル(凡例)は **クリップ一覧でなくトラック一覧** (1 行 = 1 トラック、`locked_pr_tracks` / `effective_track_color` / トラック行)。色・dim・対象・ロックはすべて **トラック単位**。複数選択は arrangement 上の **Ctrl+Click** (widget の clip 短クリックを modifier-aware 化: Ctrl=Toggle / Shift=Union / 無修飾=Single)。非対象トラックのノートを掴むと対象トラックがそちらへ切り替わる (`SetNoteSelection` で anchor 追従)。

## 現状アーキテクチャ (調査済)

- **選択 SSoT**: `AppData.selected_clips: Vec<ClipKey>` (複数選択, anchor=末尾), `selected_clip: Option<ClipKey>` (anchor)。
  `ClipKey{track_id,clip_id}`=stable id, `ClipRef{track,clip}`=index (別型, 取り違え防止)。
- **PR は単一表示**: `piano_roll_view.rs:48` が `selected_clip_ref()` 1 つだけ取得。
- **ノート id = clip 内 index**: `build_widget_notes` が `id = i as u32`。`selected_notes: Vec<u32>` も clip 内 index。
  → **複数クリップで id 衝突**。これがグローバル id 化の核心。
- **ノート編集 handler** (app.rs) はすべて `selected_clip` 固定 + clip 内 index で note を引く
  (AddNote/DeleteSelectedNotes/SetNoteSelection/SetNotePositions/ResizeNotes/CopyNotes/SetNoteVelocities)。
  例外: `SetNoteLyrics{clip_ref, ...}` だけ対象 clip を明示 (← この形に統一する)。
- **ノート色**: widget `PianoRollStyle.note_fill_fn: fn(velocity)->Color` は **widget 全体で 1 関数** = ノート単位の色不可
  → widget 拡張が必要 (widget は `ui/` 統合済で直接編集可)。
- **クリップ色**: `view/track_color.rs::effective_clip_color(track,clip)->[f32;3]` + `to_renderer()`。
- **viewport**: `piano_roll_views: HashMap<ClipKey, PianoRollViewState>` は **`ViewState` として永続化**、
  scroll は clip-local。accessor は `selected_clip` 経由で anchor のみ読む。fit (`fit_piano_roll_to_clip`) は単一 clip bbox。

## 実装方針

### 1. 表示対象クリップの集合 — `shown_pianoroll_clips()`
新規 `AppData::shown_pianoroll_clips(&self) -> Vec<ClipRef>`:
`selected_clips` を ClipRef へ解決 → **MIDI クリップだけ** filter → 順序維持 (anchor=末尾)。
これが「PR に重畳表示するクリップの順序付きリスト」の SSoT。空なら placeholder。
非 MIDI (audio/video/image/text) は無視 (audio は従来どおり audio editor)。

### 2. グローバル note id — packed `(clip_slot, local_index)`
widget の `Note.id: u32` をグローバル一意にする。
`id = (clip_slot << 24) | local_index` (clip_slot=`shown_pianoroll_clips()` 内の位置 0..255, index 0..16M)。
- `build_widget_notes` を複数 clip 対応に: 各 clip を enumerate し packed id を採番、note は各 clip の `start_beat` で
  song-absolute 化、**色 = その clip の effective_clip_color**、dimmed=(clip≠対象), locked=(clip が lock 中)。
- decode helper `AppData::decode_note_id(id) -> Option<(ClipRef, usize)>` (= shown list を引く)。view と handler が共有。
- **不変条件**: `selected_notes` は現在の shown 構成に対してのみ有効 → **shown セット (selected_clips) が変わったら必ず clear**
  (既存 `select_clip` が `selected_notes.clear()` 済。全変更経路で維持)。単一 clip の index→packed への自然な一般化。

### 3. ノート編集イベントを clip 修飾化 (SetNoteLyrics パターンに統一)
複数選択ノートは複数 clip に跨りうる (全編集可)。各 handler payload に対象 clip を持たせる:
- `selected_notes: Vec<u32>` → そのまま **packed id** を保持 (decode で clip 解決)。
- view は widget 応答 (packed id) を decode → **各ノートの所属 clip の `start_beat` を引いて clip-local 化** →
  clip 修飾 payload で event 発火。
- handler は packed id を decode して該当 clip の note を引く。delete は clip ごとに index 降順で remove。
- `AddNote` は **対象クリップ (selected_clip anchor)** 固定。位置 (song-absolute) → 対象 clip-local。
  末尾の `selected_clips` 縮小ロジックは撤去 (複数選択を保持)。
- 対象切替: ノート select/drag/作成の応答で、その操作が単一 clip 由来なら **その clip を anchor (`selected_clip`) に**。

### 4. widget 拡張 (`ui/crates/ui/src/widgets/piano_roll.rs`)
`Note` に 3 フィールド追加 (additive, 既存挙動は default で完全一致):
- `color: Option<Color>` — `Some`=その色を velocity で shade、`None`=既存 `note_fill_fn(velocity)` (examples 互換)。
- `dimmed: bool` — 非対象クリップ。fill を背景側へ寄せ + alpha 低減 (muted とは別レイヤ)。
- `locked: bool` — 参照専用。**`note_hit`/`note_hit_in` から除外** (掴めない) + 強めに dim 描画。
shade 関数 `shade_by_velocity(base, velocity)` を追加。selection/muted overlay は不変。
全 `Note{...}` 構築箇所 (daw_gui + examples piano_roll/daw_prototype + benches) を 1 commit で更新 (ui/CLAUDE.md の breaking 一括更新)。

### 5. 共有 viewport + union fit
- 複数表示時は **transient な共有 viewport** `AppData.multi_clip_view: PianoRollViewState` (song-absolute scroll, **非永続**) を使う。
  単一表示は既存 per-clip 永続 state のまま (regression なし)。`shown_pianoroll_clips().len()` で分岐。
- `min_start_beat` = 最早 clip の `start_beat` (左に過去クリップまでスクロール可)。
- Fit / auto-fit: shown 全 clip の note bbox を **union** して zoom/scroll/top_pitch 算出。
- scroll/zoom/top_pitch 編集は分岐先 (multi=`multi_clip_view`, single=anchor の per-clip) に書く。

### 6. legend パネル (対象選択 + lock UI) — daw_01 側 (widget 不要)
shown が **2 つ以上**のとき、**ピアノロールの右側に固定幅の縦パネル**を描画 (REAPER/Cakewalk のトラックペイン流, 既存 widget で構築)。
widget 本体 (`ui.piano_roll` に渡す body) は右パネル幅 (LEGEND_W≈150px) を引いた領域にする。
各 clip 行 (縦に並ぶ) = `[色 swatch][clip/track 名][◉ 対象 radio][🔒 lock toggle]`。
- swatch = effective_clip_color。対象行は強調。
- radio (or 行) クリック → `selected_clip` をその clip に (= 対象切替, 新規ノート先)。
- lock toggle → その clip の lock 状態反転 (`AppData.locked_pr_clips: HashSet<ClipKey>`, 非永続)。
- 配置安定性 (implement skill 5.5): 固定幅。開閉で他コントロールを動かさない。grid 幅は右パネル分を引く。
単一表示時は legend 非表示 = body 全幅 = 既存レイアウト不変。

### 7. preview / playback
keyboard レーン preview・ノート作成時の試聴は **対象クリップの track** の instrument。
ノートクリック時の clip 切替で track も追従。

## エッジケース
- 選択に非 MIDI 混在 → MIDI subset のみ表示。MIDI ゼロ → 既存 placeholder。
- 単一 clip → 既存挙動 + クリップ色化 (C)。legend なし。
- 空の対象 clip (note 0) → legend で対象選択でき、新規ノートが入る。
- 異 clip の note が同 pitch/time で重なる → 全描画 (非対象 dim)、対象を最前面。
- locked clip の note → 描画されるが掴めない (hit 除外)。drag/delete/velocity 対象外。
- shown セット変更 → `selected_notes` clear (不変条件)。

## テスト (高レイヤ; model/command 層)
1. `shown_pianoroll_clips` が MIDI を filter・順序維持・anchor 末尾。
2. packed id `(clip_slot,index)` の pack↔decode round-trip。
3. `AddNote` が**対象クリップ**へ入る (位置に依らず) / 複数選択保持 (縮小しない)。
4. cross-clip の move/resize/delete/velocity が **正しい clip の正しい note** に適用。
5. union fit が全 clip bbox を張る。
6. shown セット変更で `selected_notes` clear。
7. widget: `note_hit` が `locked` ノートを除外する (unit)。
視覚 (クリップ色・dim・legend 配置) は build/test をすり抜けるので実機 sign-off で確認。

## 非対象 (out of scope)
- velocity/pitch/channel 等の色モード切替 (clip 色固定。要望外)。
- 複数表示 viewport の永続化 (選択依存ゆえ transient が正)。
- 非 MIDI クリップの PR 表示 (audio は audio editor のまま)。
