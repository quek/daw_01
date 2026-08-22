<!--
SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Plan: piano_roll_view.rs を gui_01 Ui::piano_roll widget で書き換え (本体実装)

## Context

前段階として gui_01 へ要望 3 件 (#002 / #003 / #004) を提出 (commit b350976)。gui_01 の M9 Phase 44d で対応 + 回答受領済み (commit e3c6e61 で `docs/gui_01_conversation_archive_001.md` に保管)。

**gui_01 側対応の要点**:
- **#002 解決**: rect select 修飾キーを **Alt+drag → Shift+drag (加算挙動)** にデフォルト完全置換。daw_01 旧仕様と一致するため widget をそのまま呼べば OK (daw_01 側で modifier 設定不要)
- **#003 解決**: API 拡張は行わず、daw_01 側で `take_double_click_in_rect` + `note_hit` でエミュレート。gui_01 から具体的な sample コード提示済 (1/16 grid snap)
- **#004 解決**: arrangement clip dbl-click → Piano Roll タブ遷移は別タスクで実施 (今回 scope 外、推奨パターン共有のみ)

`cargo build --workspace` で gui_01 上流 breaking change の影響なし確認済 (warnings 4 件は既存 dead code)。`piano_roll_view.rs` は前段階で触らずに残しているため未影響、本実装で widget 化する。

## 設計判断サマリ

- **Shift+drag rect select**: gui_01 widget が自動でハンドリング、daw_01 側で modifier check を書く必要なし
- **空白 dbl-click → AddNote**: gui_01 #003 回答の sample コードを採用。widget 呼び出しの **後** に `take_double_click_in_rect(grid_rect)` + `note_hit(&widget_notes, ...).is_none()` で空白判定 → `AppEvent::AddNote` dispatch
- **AppEvent**: 既存 bulk move event `SetNotePositions` を `Move` に流用。Resize 用に新 bulk event `ResizeNotes(Vec<(u32, f64, f64)>)` を新設
- **`AppData.pianoroll_notes_generation: u64`** 新設: widget cache invalidation 用、notes 編集 handler 末尾で bump
- **velocity lane / playhead**: 自前維持 (widget 未対応)
- **wheel handler**: `resp.dragging.is_none()` gate で残す
- **bottom_panel タブ機構の `tab_view_with_state` 化** (P0-2 / #004 関連): 別タスク (scope 外)

## AppData / AppEvent 変更

ファイル: `daw_gui/src/app.rs`

1. **フィールド追加** (line 152 付近 + `Self::new` 269 付近):
   ```rust
   pub pianoroll_notes_generation: u64,  // default 0
   ```

2. **`AppEvent` variant 追加** (line 754 付近、`SetNotePositions` の隣):
   ```rust
   ResizeNotes(Vec<(u32, f64, f64)>),  // (note_idx, new_start_beat, new_duration_beats)
   ```

3. **`is_undoable`** (line 543-562) に `AppEvent::ResizeNotes(_) => true` 追加

4. **`handle_event` arm 追加** (`SetNotePositions` の隣):
   ```rust
   AppEvent::ResizeNotes(entries) => self.resize_notes(&entries),
   ```

5. **`fn resize_notes`** (line 1689 付近、`resize_note` の隣に並べる):
   - `selected_clip` を取得し、`entries` を for ループで `note.start_beat` / `note.duration_beats` を一括更新 (single-source-of-truth は `clip.notes`)
   - 末尾で `self.sync_song_to_plugin_host()` (`set_note_positions` と同パターン)

6. **`pianoroll_notes_generation += 1`** を以下の handler 関数末尾に追加 (event arm ではなく関数末尾、漏れ防止):
   - `add_note` (1633)
   - `set_note_positions` (1669)
   - `resize_note` (1689)
   - `resize_notes` (新規)
   - `delete_selected_notes` (1710)
   - `set_selected_note_lyric` (1732)
   - `quantize_selected_notes` (660)
   - **要確認**: `apply_undo` / `apply_redo` 経路 (`push_undo_snapshot` 周辺、line 500) — song 全体を入れ替えるので bump 必須。実装直前に grep で関数を特定し末尾追加

`select_note` / viewport 変更系 (`SetPianoRoll{Zoom,Scroll,TopPitch}`) は **bump しない** — selection は `&[NoteId]` 外部 borrow、viewport は widget 内部の `viewport_key` で吸収。

## piano_roll_view.rs 書き換え (493 LOC → ~170 LOC)

ファイル: `daw_gui/src/view/piano_roll_view.rs`

### 残す
- 定数 `KEYBOARD_W = 56.0`、`VEL_LANE_H = 60.0`
- `draw_velocity_lane` (255-304) — そのまま
- 色定数のうち velocity bar / playhead / hint 用 4-6 個

### 削除
- `draw_canvas` (83-197) — note / grid / keyboard 描画は widget + 自前 push_lines (playhead) に置換
- `draw_keyboard` (199-253) — widget 内蔵
- `handle_input` (306-425) — widget 内蔵 + wheel handler だけ entry に再実装
- `handle_drag_rect_select` (430-493) — widget 内蔵 (Shift+drag、gui_01 #002 で置換済)
- 不要色定数 8 個程度

### 新 entry 関数 (擬似コード)

```rust
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let widget_area = Rect { h: area.h - VEL_LANE_H, ..area };
    let vel_area = Rect {
        x: area.x + KEYBOARD_W,
        y: area.y + area.h - VEL_LANE_H,
        w: area.w - KEYBOARD_W,
        h: VEL_LANE_H,
    };
    let grid_rect = Rect {
        x: widget_area.x + KEYBOARD_W,
        y: widget_area.y,
        w: widget_area.w - KEYBOARD_W,
        h: widget_area.h,
    };

    let Some(clip_ref) = app.selected_clip else {
        return;  // empty placeholder
    };
    let clip = /* app.song.clip(clip_ref) — accessor は実装直前確認 */;

    let widget_notes = to_gui_notes(clip);
    let view = PianoRollView {
        start_beat: app.pianoroll_scroll_beat as f64,
        len_beats: ((widget_area.w - KEYBOARD_W) / app.pianoroll_zoom_x.max(4.0)) as f64,
        pitch_top: app.pianoroll_top_pitch as f32,
        pitch_visible: widget_area.h / app.pianoroll_zoom_y.max(6.0),
        keyboard_w: KEYBOARD_W,
        notes_generation: app.pianoroll_notes_generation,
    };
    let style = PianoRollStyle::default();

    let target = clip_ref;  // Copy
    let make_edit = move |req: NotesEditRequest| -> Edit<AppData> {
        match req {
            NotesEditRequest::Add(notes) => {
                let Some(n) = notes.into_iter().next() else { return Edit::mutate(|_| {}); };
                Edit::mutate(move |app| app.handle_event(AppEvent::AddNote {
                    track: target.track, clip: target.clip,
                    start_beat: n.start_beat, duration: n.len_beats, pitch: n.pitch,
                }))
            }
            NotesEditRequest::Delete(notes) => {
                let ids: Vec<u32> = notes.iter().map(|n| n.id).collect();
                Edit::mutate(move |app| {
                    app.handle_event(AppEvent::SetNoteSelection(ids.clone()));
                    app.handle_event(AppEvent::DeleteSelectedNotes);
                })
            }
            NotesEditRequest::Move(deltas) => {
                let entries: Vec<(u32, f64, u8)> = deltas.iter()
                    .map(|(id, _, _, ns, np)| (*id, *ns, *np)).collect();
                Edit::mutate(move |app| app.handle_event(AppEvent::SetNotePositions(entries.clone())))
            }
            NotesEditRequest::Resize(deltas) => {
                let entries: Vec<(u32, f64, f64)> = deltas.iter()
                    .map(|(id, _, _, ns, nl)| (*id, *ns, *nl)).collect();
                Edit::mutate(move |app| app.handle_event(AppEvent::ResizeNotes(entries.clone())))
            }
            NotesEditRequest::Select { next, .. } => {
                Edit::mutate(move |app| app.handle_event(AppEvent::SetNoteSelection(next.clone())))
            }
        }
    };

    let resp = ui.piano_roll(
        "piano_roll", widget_area, &widget_notes, view, &app.selected_notes, &style, make_edit,
    );

    // 空白 dbl-click → AddNote エミュレート (gui_01 #003 回答の sample 採用、1/16 grid snap)
    if let Some((px, py)) = ui.take_double_click_in_rect(grid_rect) {
        use daw_ui_core::note_hit;
        if note_hit(&widget_notes, view, grid_rect, px, py, style.resize_handle_px).is_none() {
            let beat_to_px = grid_rect.w as f64 / view.len_beats.max(1e-6);
            let pitch_to_px = grid_rect.h / view.pitch_visible.max(1e-6);
            let beat_raw = view.start_beat + (px - grid_rect.x) as f64 / beat_to_px;
            let snapped_beat = (beat_raw * 16.0).round() / 16.0;  // 1/16 snap
            let pitch_raw = view.pitch_top - (py - grid_rect.y) / pitch_to_px;
            let pitch = (pitch_raw.round() as i32).clamp(0, 127) as u8;

            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::AddNote {
                    track: target.track, clip: target.clip,
                    start_beat: snapped_beat, duration: 0.25, pitch,
                });
            }));
        }
    }

    // wheel handler — drag 中は無効
    if resp.dragging.is_none() {
        // 既存 wheel ロジック (Ctrl→ZoomY, Shift→ScrollX, plain→TopPitch) を移植
    }

    // velocity lane (既存実装、view と x 座標を共有)
    draw_velocity_lane(app, ui, vel_area, view.start_beat, view.len_beats);

    // playhead 線 — canvas + velocity lane を縦断
    draw_playhead(app, ui, widget_area, vel_area, view.start_beat, view.len_beats);
}

fn to_gui_notes(clip: &common::model::Clip) -> Vec<daw_ui_core::Note> {
    clip.notes.iter().enumerate().map(|(i, n)| daw_ui_core::Note {
        id: i as u32,
        start_beat: n.start_beat as f64,
        len_beats: n.duration_beats as f64,
        pitch: n.pitch,
        velocity: n.velocity,
        lyric: n.lyric.as_deref().filter(|s| !s.is_empty()).map(Arc::from),
    }).collect()
}
```

ポイント:
- `widget_notes` を `note_hit` 用に再利用 (alloc 1 回)
- gui_01 #003 注意点: 空白 dbl-click は **widget の後** に呼ぶ (`piano_roll → take_double_click_in_rect` の順、release ベース global state)
- `Edit::mutate(|_| {})` で no-op (`Edit::noop()` が無い前提、実装直前に確認)
- `target = clip_ref` を Copy で closure に move (`target.track`, `target.clip`)
- `to_gui_notes` は `clip.notes` から直接変換 (note_boxes() 経由でない)。velocity lane は引き続き `app.note_boxes()` を使うので alloc 二重にならない

## Critical files to modify

| Path | 概要 |
|---|---|
| `daw_gui/src/view/piano_roll_view.rs` | 493 → ~170 LOC 書き換え |
| `daw_gui/src/app.rs` | `pianoroll_notes_generation` field 追加、`AppEvent::ResizeNotes` 追加、`is_undoable` 追加、`resize_notes` method 追加、bump 追加 (7 handler + apply_undo/redo) |

無変更: `daw_gui/src/view/bottom_panel.rs` (`piano_roll_view::draw` の signature 不変)、`daw_gui/src/view/arrangement_view.rs`、gui_01 全体。

## 実装直前に確認する点

1. `daw_gui/Cargo.toml` で `daw_ui_core` から `Note` / `NoteId` / `NotesEditRequest` / `PianoRollStyle` / `PianoRollView` / `PianoRollResponse` / `MoveDelta` / `ResizeDelta` / `note_hit` の re-export 状況
2. `apply_undo` / `apply_redo` の正確な関数名・行 (song 入れ替え経路で bump 必須)
3. `Edit::noop()` の有無 (なければ `Edit::mutate(|_| {})` で代替)
4. `app.song.clip(clip_ref)` の accessor 名 / signature
5. `ClipRef` 型が `Copy` か (closure に move して `target.track` 等で参照する設計)

これらは実装直前に grep / Read で確認、plan 構造に影響なし。

## Verification plan

1. `cargo build -p daw_gui` で warning ゼロ
2. `cargo clippy -p daw_gui --tests -- -D warnings` でクリーン
3. `cargo run -p daw_gui` で目視 (DoD):
   - クリップ選択 → Piano Roll タブで note / 鍵盤 / grid / lyric が旧版同等に表示
   - クリックで note 1 個選択、空白クリックで selection 解除
   - **Shift+drag → 加算 rect select** (旧仕様維持、widget 自動)
   - note 中央 drag release → 位置移動、Ctrl+Z で 1 step 戻る
   - note 端 drag release → resize、Ctrl+Z で 1 step 戻る
   - **空白 dbl-click → AddNote (1/16 grid snap)** (旧仕様維持、エミュレート)
   - Insert キー → AddNote (widget 標準、併存)
   - Delete キー → 選択 note 削除、Ctrl+Z で戻る
   - Ctrl+Z / Ctrl+Shift+Z で undo/redo
   - Ctrl+wheel → zoom_y / Shift+wheel → scroll_x / plain wheel → top_pitch、note drag 中は無効
   - velocity lane の bar が note の x と一致 (view 共有確認)
   - playhead 線が再生中に canvas + velocity lane を縦断

## Scope 外 (将来別タスク)

- velocity 編集の widget 化 (gui_01 widget 未実装、別 phase で gui_01 へ要望)
- arrangement view の clip double-click → Piano Roll タブ遷移 (#004 回答を別タスクで活用)
- bottom_panel タブ機構の `tab_view_with_state` 化 (P0-2)

## Commit

書き換え完了後、1 commit:

```
refactor: piano_roll_view を gui_01 Ui::piano_roll widget で書き換え
```
