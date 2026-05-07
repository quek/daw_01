# Plan: M9 Phase 41 — piano_roll note edit + multi-select 統合 (7 commits)

## Context

[docs/plan.md](plan.md) M9 (Real DAW Validation) の主軸 Phase。`Edit::with_inverse` の ergonomic を実コードで実証し、boilerplate が出れば library helper / widget 化で吸収する。

並行プラン [docs/plan_daw01_feedback.md](plan_daw01_feedback.md) (P0-P3 11 項目) のうち P1-3 (HeavyCtx delegate) を Phase 41pre として先行実施する (rect multi-select が heavy 内で必要)。P1-4 (double-click) は Phase 41 外、P1-5 (menu 拡張) は Phase 41 完了後に挟む。

### 全体方針

- **breaking change を恐れない**: 1 workspace + Edition 2024 の利点を活かし、library API 変更は全 example / test / docs を 1 commit で一括更新する (CLAUDE.md「理想とベストプラクティスを追求」)
- **note 操作は最初から複数対応**: DAW では multi-select が常態。helper は最初から `Arc<[Note]>` ベースで `make_*_notes_edit` (複数形) で実装。single note は `Arc::from([note])` で呼ぶ
- **`begin_group / end_group` は Phase 41 では実装しない**: 複数対応 helper なら multi-delete = 1 個の Edit で完結し、group が不要。group が必要になるのは「異なる種類の Edit を 1 step に」(例: add → select → move)。Phase 42 以降で実需が出たら別途実装
- **library widget 化は Phase 41e で完了させる**: validation 中の API ロックを恐れず、Phase 42-43 で発見した改善は widget API を breaking 変更で逐次反映する。daw_01 が Phase 42 開始時点で widget を直接使える状態にしておくとフィードバックサイクルが速い

### 共通 DoD (各 commit で同じ)

- `cargo build --workspace` ✅
- `cargo test --workspace` ✅ (新規 tests + 既存 tests pass)
- `cargo clippy --workspace --tests -- -D warnings` ✅
- 影響する example で `cargo run --bin <name>` 動作確認 (UI 系は目視)
- `cd ../daw_01 && cargo build` で path 依存先の build が壊れていないこと (breaking change を入れた場合は daw_01 側も同 commit で更新する)
- 1 commit、message prefix: `feat(M9 Phase 41<x>): <短い概要>` または P1-3 のみ `feat(M9): P1-3 — <概要>`

---

## 設計判断の結論

### 1. history group の API 形 → **Phase 41 では実装しない**

複数対応 helper で multi-delete / multi-move が 1 Edit に集約されるため、`begin_group / end_group` の need が Phase 41 では発生しない。Phase 42 以降で「異なる種類の Edit を 1 step」の実需が出たら別途実装。

### 2. `Ui::set_cursor` の最小公開範囲 → **A: `Ui::set_cursor(CursorIcon)` 1 関数 + 「最後勝ち」semantics**

`UiHost` の `redraw_request: Box<dyn Fn()>` と同じパターンで `set_cursor_request: Box<dyn Fn(CursorIcon)>` を追加。`with_window` で内部生成。`Ui` に transient な `pending_cursor: Option<CursorIcon>` を持ち、frame 末尾で flush。

cursor stack 風 (push/pop) は不要 (DAW UX で同 frame 複数 widget の cursor 競合は実用上発生しない)。

### 3. multi-delete / move / resize の inverse capture 戦略 → **`Arc<[T]>` snapshot を直接 capture (Phase 41a-c で 5 ペア)、Phase 41d で `Edit::snapshot_inverse` library 化**

- `Arc<[Note]>: Send + Sync + Clone` で `Fn` の中で `arc.clone()` して値復元する pattern が最も素直
- Phase 41a-c で 5 ペアの Arc capture を書いてから Phase 41d で `Edit::snapshot_inverse(label, fwd, restore_from)` helper に統合 (CLAUDE.md「3 回繰り返されたら抽象化」の閾値超え)

### 4. piano_roll の library widget 化 → **Phase 41e で widget 化 (breaking 容認方針)**

example で 41a-d まで書き心地確認した直後に library widget 化。`crates/ui/src/widgets/piano_roll.rs` 新設、`PianoRoll`, `PianoRollResponse`, `Note` (library 型) を pub。Phase 42-43 で発見した改善は breaking 変更で widget API に随時反映 (CLAUDE.md「破壊的変更を恐れない」)。

### 5. P1-3 (HeavyCtx delegate) のタイミング → **41pre で包括 14 method delegate**

rect-select は heavy 内必須。最小限ではなく包括版採用 (heavy 抽象の漏れを 1 commit で塞ぐ)。各 method は 1 行 forward なので LOC コストは低い。

### 6. P1-4 (double-click) のタイミング → **Phase 41 外**

note の double-click UX (velocity edit popup 等) は basic 編集完成後の拡張。Phase 41 のスコープ (basic 編集 + multi-select) には不要。Phase 41 完了直後または Phase 42 と並行で実装。

### 7. note の identity 戦略 → **`id: u32` 不変**

natural key `(start_beat, pitch)` は move で identity が変わる、index は delete で ずれる。`id: u32` (生成時 unique、編集中も保持) のみが multi-select の identity を安定させる。daw_01 の `NoteBox::note: u32` も同じ判断。

`PianoRollModel` に `next_note_id: u32` を持ち、`add_notes` で fetch_add 風に increment。selected は Model 側 `Vec<u32> selected_note_ids` (Note 自身は selected を持たない、single source of truth)。

### 8. `Note` の最小 schema → **`{ id: u32, start_beat: f32, len_beats: f32, pitch: u8, velocity: u8 }` (id 追加のみ、Copy)**

memory: 14 bytes + padding = 16 bytes/note。100k notes = 1.6MB (現状と微増 only)。

pan / lyric / channel / midi raw は Phase 41 では追加しない (boilerplate 計測の S/N を上げるため)。

---

## Commit 分解 (7 commits、依存順)

### 41pre — `feat(M9): P1-3 — HeavyCtx に input/popup pull API を包括 delegate` ✅ done (63b361f)

**着手契機**: 41a の context_menu / shortcut も heavy 内で書きたいため最先行。

**Files**:
- 修正: [crates/ui/src/widgets/heavy.rs](../crates/ui/src/widgets/heavy.rs) (+~150 LOC、各 1 行 forward × 14 method)

**新規 delegate**:
```rust
impl<'b, 'a, M: ?Sized + 'static> HeavyCtx<'b, 'a, M> {
    pub fn take_drag_rect_in_rect(&mut self, wid: WidgetId, bounds: Rect) -> Option<DragRect>;
    pub fn take_file_drop_in_rect(&mut self, rect: Rect) -> Option<DroppedFiles>;
    pub fn is_file_hovering_in_rect(&self, rect: Rect) -> bool;
    pub fn take_clipboard_paste(&mut self) -> Option<String>;
    pub fn set_clipboard_text(&mut self, s: String);
    pub fn take_shortcut(&mut self, name: &'static str) -> bool;
    pub fn shortcut_for(&self, name: &'static str) -> Option<String>;
    pub fn take_scroll_in_rect(&mut self, rect: Rect) -> (f32, f32);
    pub fn context_menu_for<F>(&mut self, rect: Rect, items: &[&str], on_select: F)
        where F: FnOnce(usize) -> Edit<M>;
    pub fn request_redraw(&mut self);
    pub fn request_undo(&mut self);
    pub fn request_redo(&mut self);
    pub fn can_undo(&self) -> bool;
    pub fn can_redo(&self) -> bool;
}
```

**Tests**:
- `heavy_take_drag_rect_in_rect_inside_cached`
- `heavy_take_shortcut_consume_outside_cached`
- `heavy_context_menu_for_opens_popup`

**LOC**: +~150 source、+~80 tests

**依存**: なし

---

### 41a — `feat(M9 Phase 41a): piano_roll に Note id 導入 + 複数対応 add/delete を Edit::with_inverse 化` ✅ done (8c2c49e)

**Files**:
- 修正: [crates/examples/piano_roll/src/main.rs](../crates/examples/piano_roll/src/main.rs)
  - `Note` に `id: u32` 追加
  - `PianoRollModel` に `next_note_id: u32`, `selected_note_ids: Vec<u32>` 追加 (`selected_note_index: Option<usize>` 削除 = breaking、example 内のみ)
  - heavy() 内で `context_menu_for` 経由「Add note here / Delete selected」実装
  - `take_shortcut("delete")` で selected を delete edit に投げる (single-select 段階)

**新規 helper (example local)**:
```rust
fn make_add_notes_edit(notes: Arc<[Note]>) -> Edit<PianoRollModel> {
    let n_fwd = Arc::clone(&notes);
    let n_inv = notes;
    Edit::with_inverse(
        if n_fwd.len() == 1 { "add note" } else { "add notes" },
        move |m: &mut PianoRollModel| {
            for note in n_fwd.iter() { m.notes.push(*note); }
            m.notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap());
            m.notes_generation += 1;
        },
        move |m: &mut PianoRollModel| {
            let ids: HashSet<u32> = n_inv.iter().map(|n| n.id).collect();
            m.notes.retain(|x| !ids.contains(&x.id));
            m.notes_generation += 1;
        },
    )
}

fn make_delete_notes_edit(notes: Arc<[Note]>) -> Edit<PianoRollModel> {
    let n_fwd = Arc::clone(&notes);
    let n_inv = notes;
    Edit::with_inverse(
        if n_fwd.len() == 1 { "delete note" } else { "delete notes" },
        move |m: &mut PianoRollModel| {
            let ids: HashSet<u32> = n_fwd.iter().map(|n| n.id).collect();
            m.notes.retain(|x| !ids.contains(&x.id));
            m.notes_generation += 1;
        },
        move |m: &mut PianoRollModel| {
            for note in n_inv.iter() { m.notes.push(*note); }
            m.notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap());
            m.notes_generation += 1;
        },
    )
}
```

**Tests**:
- `add_notes_then_undo_round_trip` — 単数 / 複数両方
- `delete_notes_then_undo_round_trip` — 単数 / 複数両方
- `delete_then_add_inverse_orderings` — 削除 → undo で元位置に戻る (sort 効いている)
- `notes_generation_advances_on_forward_and_inverse`

**LOC**: 修正 +~80, helper +~80, test +~120 = **+~280 total**

**依存**: 41pre

**Arc capture**: 2 ペア (add/delete 各 1 ペア)

---

### 41b — `feat(M9 Phase 41b): piano_roll に複数対応 move/resize + Ui::set_cursor (EwResize/Move)` ✅ done (c81e685)

**Files**:
- 修正: [crates/ui/src/ui.rs](../crates/ui/src/ui.rs) (`Ui::set_cursor` + `pending_cursor`、`UiHost::with_window` 内部拡張)
- 修正: [crates/ui/src/lib.rs](../crates/ui/src/lib.rs) (`CursorIcon` re-export)
- 修正: [crates/examples/piano_roll/src/main.rs](../crates/examples/piano_roll/src/main.rs)
  - hit-test 拡張: note rect の左右 4px = resize handle, 中央 = move 領域
  - hover で `Ui::set_cursor(CursorIcon::EwResize/Move)`
  - drag 開始時に anchor 保存 → 中は `Edit::mutate` で値更新 → release で `make_move_notes_edit` / `make_resize_notes_edit` を発行

**新規 library API**:
```rust
impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// このフレームの cursor 形状を要求。同 frame 内で複数回呼ばれた場合は最後勝ち。
    /// `WindowBackend::set_cursor` callback が registered されていなければ no-op。
    pub fn set_cursor(&mut self, cursor: CursorIcon);
}
```

**新規 helper (example local)**:
```rust
type NoteId = u32;

fn make_move_notes_edit(deltas: Arc<[(NoteId, f32, u8, f32, u8)]>) -> Edit<PianoRollModel> {
    // (id, prev_start, prev_pitch, next_start, next_pitch)
    let d_fwd = Arc::clone(&deltas);
    let d_inv = deltas;
    Edit::with_inverse(
        if d_fwd.len() == 1 { "move note" } else { "move notes" },
        move |m: &mut PianoRollModel| {
            for (id, _, _, ns, np) in d_fwd.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.start_beat = ns; n.pitch = np;
                }
            }
            m.notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap());
            m.notes_generation += 1;
        },
        move |m: &mut PianoRollModel| {
            for (id, ps, pp, _, _) in d_inv.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.start_beat = ps; n.pitch = pp;
                }
            }
            m.notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap());
            m.notes_generation += 1;
        },
    )
}

fn make_resize_notes_edit(deltas: Arc<[(NoteId, f32, f32)]>) -> Edit<PianoRollModel> {
    // (id, prev_len, next_len)
    let d_fwd = Arc::clone(&deltas);
    let d_inv = deltas;
    Edit::with_inverse(
        if d_fwd.len() == 1 { "resize note" } else { "resize notes" },
        move |m: &mut PianoRollModel| {
            for (id, _, nl) in d_fwd.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.len_beats = nl;
                }
            }
            m.notes_generation += 1;
        },
        move |m: &mut PianoRollModel| {
            for (id, pl, _) in d_inv.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.len_beats = pl;
                }
            }
            m.notes_generation += 1;
        },
    )
}
```

**Tests**:
- (library) `ui_set_cursor_calls_callback_on_frame_end`
- (library) `ui_set_cursor_no_op_when_callback_unset`
- (library) `ui_set_cursor_last_call_wins_within_frame`
- (example) `move_notes_then_undo_restores_positions`
- (example) `resize_notes_then_undo_restores_lengths`

**LOC**: library +~120, example +~150, test +~150 = **+~420 total**

**依存**: 41a

**Arc capture**: 2 ペア追加 (move/resize 各 1 ペア) → 41pre 終了時点で計 4 ペア

---

### 41c — `feat(M9 Phase 41c): piano_roll に rect multi-select + selection state Undoable 化` ✅ done (b0ac62d)

**Files**:
- 修正: [crates/examples/piano_roll/src/main.rs](../crates/examples/piano_roll/src/main.rs)
  - heavy 内で `take_drag_rect_in_rect` (P1-3 経由) → 中の note を id で collect → `make_select_notes_edit` 発行
  - Shift+drag で selected 全体を平行 move (`make_move_notes_edit` に複数 delta を渡す)
  - Delete shortcut で selected を `make_delete_notes_edit` に投げる (multi-select 対応)

**新規 helper (example local)**:
```rust
fn make_select_notes_edit(prev: Arc<[NoteId]>, next: Arc<[NoteId]>) -> Edit<PianoRollModel> {
    let p_fwd = Arc::clone(&prev);
    let p_inv = prev;
    let n_fwd = Arc::clone(&next);
    let n_inv = next;
    Edit::with_inverse(
        "select notes",
        move |m: &mut PianoRollModel| { m.selected_note_ids = n_fwd.to_vec(); },
        move |m: &mut PianoRollModel| { m.selected_note_ids = p_inv.to_vec(); },
    )
}
```

待って、上記は forward でも prev を使う必要があるので、書き直し:

```rust
fn make_select_notes_edit(prev: Arc<[NoteId]>, next: Arc<[NoteId]>) -> Edit<PianoRollModel> {
    Edit::with_inverse(
        "select notes",
        {
            let next = Arc::clone(&next);
            move |m: &mut PianoRollModel| { m.selected_note_ids = next.to_vec(); }
        },
        {
            let prev = Arc::clone(&prev);
            move |m: &mut PianoRollModel| { m.selected_note_ids = prev.to_vec(); }
        },
    )
}
```

**Tests**:
- `rect_select_then_delete_undo_restores_all_with_selection`
- `rect_select_then_drag_move_undo_restores_positions`
- `select_notes_then_undo_restores_prev_selection`

**LOC**: example +~250, test +~120 = **+~370 total**

**依存**: 41a + 41b + 41pre

**Arc capture**: 1 ペア追加 (select_notes) → **41c 終了時点で計 5 ペア**

---

### 41d — `feat(M9 Phase 41d): Edit::snapshot_inverse helper を library 化 (Arc capture pattern を吸収)` ✅ done (c0fe6b6)

**Files**:
- 修正: [crates/ui/src/edit.rs](../crates/ui/src/edit.rs) (`Edit::snapshot_inverse` 追加)
- 修正: [crates/examples/piano_roll/src/main.rs](../crates/examples/piano_roll/src/main.rs) (`make_*_notes_edit` 5 個を `Edit::snapshot_inverse` 経由に書き換え、Arc capture boilerplate を削減)

**新規 library API**:
```rust
impl<M: ?Sized + 'static> Edit<M> {
    /// snapshot ベースの inverse を簡潔に作る helper。
    /// `forward` と `restore_from(snapshot)` を渡すと、`restore_from` を inverse 用 closure に
    /// wrap し、`Arc` で snapshot を共有する形で `Edit::with_inverse` を構築する。
    ///
    /// 主用途: `Vec<Note>` / `Vec<f32>` 級の重いデータの inverse capture で
    /// `Arc::clone` の boilerplate を吸収する。
    pub fn snapshot_inverse<S, F, R>(label: &'static str, snapshot: S, forward: F, restore_from: R) -> Self
    where
        S: Send + Sync + 'static,
        F: Fn(&mut M) + Send + Sync + 'static,
        R: Fn(&mut M, &S) + Send + Sync + 'static,
    {
        let snap = Arc::new(snapshot);
        let snap_inv = Arc::clone(&snap);
        Self::with_inverse(
            label,
            forward,
            move |m| restore_from(m, &snap_inv),
        )
    }
}
```

**helper 書き換え例** (41a の `make_delete_notes_edit`):

```rust
fn make_delete_notes_edit(notes: Arc<[Note]>) -> Edit<PianoRollModel> {
    let label = if notes.len() == 1 { "delete note" } else { "delete notes" };
    let ids: Vec<u32> = notes.iter().map(|n| n.id).collect();
    let ids_for_fwd = ids.clone();
    Edit::snapshot_inverse(
        label,
        notes,  // snapshot (Arc<[Note]>)
        move |m: &mut PianoRollModel| {
            let id_set: HashSet<u32> = ids_for_fwd.iter().copied().collect();
            m.notes.retain(|x| !id_set.contains(&x.id));
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &Arc<[Note]>| {
            for note in snap.iter() { m.notes.push(*note); }
            m.notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap());
            m.notes_generation += 1;
        },
    )
}
```

(注: forward の方で snapshot を使わないケースは `with_inverse` 直書きでも良い、ergonomic 上必要なら `snapshot_inverse_pair(snap_prev, snap_next, fwd, inv)` の対称版も検討)

**Tests** (library):
- `edit_snapshot_inverse_round_trip`
- `edit_snapshot_inverse_send_sync` (compile time check via trait bound)

**LOC**: library +~80, test +~50, example refactor net +~30 = **+~160 total**

**依存**: 41a + 41b + 41c

**Arc capture pattern**: example 側の boilerplate が約 5 箇所 × 4 行 = 20 行削減

---

### 41e — `feat(M9 Phase 41e): piano_roll を library widget 化 (crates/ui/src/widgets/piano_roll.rs 新設)` ✅ done (8878388)

**breaking 容認**: 既存 example は library widget を使う形に書き換え。daw_01 が path 依存で取り込んでいる場合、daw_01 側も同 commit で更新する。

**Files**:
- 新規: `crates/ui/src/widgets/piano_roll.rs` (~+400 LOC)
  - `Note` (library 型、`{ id: u32, start_beat: f32, len_beats: f32, pitch: u8, velocity: u8 }`)
  - `NoteId = u32` type alias
  - `PianoRollState` (selected_note_ids 等の state、widget 内部保持 or 外部 borrow か検討)
  - `PianoRollResponse` (selection 変化、edit 発行有無)
  - `PianoRollStyle` (色、サイズ等)
  - public `Ui::piano_roll(id, rect, notes, style) -> PianoRollResponse` または builder pattern
- 修正: [crates/ui/src/widgets/mod.rs](../crates/ui/src/widgets/mod.rs) (`pub mod piano_roll;`)
- 修正: [crates/ui/src/lib.rs](../crates/ui/src/lib.rs) (re-export `Note`, `NoteId`, `PianoRollResponse` 等)
- 修正: [crates/examples/piano_roll/src/main.rs](../crates/examples/piano_roll/src/main.rs) (library widget を使う形に書き換え、example 内の Note 定義 / make_*_notes_edit helper 等を library 化された型に置換 / 独自 hit-test code 削除)
- 修正 (もしあれば): `F:/dev/daw_01/...` の piano_roll 関連 (path 依存先、breaking)

**設計判断 (41e 内 sub-decision)**:

- (a) `PianoRollState` の保持場所: 「内部 widget_state」+「外部 selected_note_ids: &mut Vec<NoteId>」のハイブリッド (P0-2 の tab_view_with_state と同じパターン)
- (b) Edit 発行は widget 内部で `push_edit` 呼ぶ vs `PianoRollResponse` に Edit を載せて return: 後者 (ユーザが apply タイミング制御)。fader と同じ pattern
- (c) hit-test (resize handle 4px / move 領域) は widget 内部に閉じる、ユーザは note の表示色のみ Style で制御

**Tests**:
- `piano_roll_widget_renders_notes_in_viewport_only`
- `piano_roll_widget_emits_select_edit_on_rect_drag`
- `piano_roll_widget_emits_delete_edit_on_delete_shortcut`
- (LOC test 機能実装後に確定)

**LOC**: library +~400, example refactor -~600 (元 600 行から widget 利用版に縮小)、test +~200 = **net +~0 (例の refactor で相殺)**

**依存**: 41a + 41b + 41c + 41d

**breaking change 影響**:
- `crates/examples/piano_roll/` の Note 型は library 側の `daw_ui_core::Note` に統合 (重複削除)
- daw_01 側で gui_01 の Note 型を直接使っている箇所があれば移行 (daw_01 は自前 NoteBox 使用なので影響軽微の見込み、要確認)

---

### 41f — `docs(M9 Phase 41): 完了記録 + Phase 44 評価項目更新` ✅ done (this commit)

**Files**:
- 修正: [docs/plan.md](plan.md) (M9 Phase 41 完了マーク、Phase 41 で得た学びを Phase 44 評価項目に反映)
- 修正: [docs/history.md](history.md) (M9 Phase 41 完了 1 ブロック追記、6 commits まとめ + 主な学び)

**LOC**: docs +~120

**依存**: 41a-e の merge 後

---

## 全体まとめ

| commit | 概要 | 主 LOC | Arc capture | 依存 |
|---|---|---|---|---|
| 41pre | P1-3 HeavyCtx 包括 delegate | +230 | - | なし |
| 41a | Note id + 複数対応 add/delete helper | +280 | 2 ペア | 41pre |
| 41b | 複数対応 move/resize + Ui::set_cursor | +420 | +2 ペア (計 4) | 41a |
| 41c | rect multi-select + select_notes Undoable | +370 | +1 ペア (計 5) | 41a/b/pre |
| 41d | Edit::snapshot_inverse library 化 | +160 (refactor 含) | helper で吸収 | 41a/b/c |
| 41e | piano_roll を library widget 化 | net +0 | (widget 内に閉込) | 41a/b/c/d |
| 41f | docs 完了記録 | +120 | - | 41a-e |
| **計** | | **+~1580** | | |

---

## 新 API 一覧

### library (crates/ui/) で追加

| API | Signature | file | 出る commit |
|---|---|---|---|
| `Ui::set_cursor` | `pub fn set_cursor(&mut self, cursor: CursorIcon)` | `ui.rs` | 41b |
| `UiHost::with_window` 内部 | `set_cursor_request` callback も同時セット (signature 不変) | `ui.rs` | 41b |
| `Edit::snapshot_inverse` | `pub fn snapshot_inverse<S, F, R>(label, snap, fwd, restore) -> Self` | `edit.rs` | 41d |
| `HeavyCtx` 包括 delegate (14 method) | (P1-3 リスト参照) | `widgets/heavy.rs` | 41pre |
| `daw_ui_core::Note` | `pub struct Note { id, start_beat, len_beats, pitch, velocity }` | `widgets/piano_roll.rs` | 41e |
| `daw_ui_core::NoteId` | `pub type NoteId = u32;` | 同上 | 41e |
| `Ui::piano_roll` | `pub fn piano_roll(&mut self, id, rect, notes, &mut state, style) -> PianoRollResponse` | 同上 | 41e |
| `PianoRollResponse` | edit 列 + selection 変化情報 | 同上 | 41e |
| `PianoRollStyle` | 色 / サイズ等 | 同上 | 41e |

### re-export (crates/ui/src/lib.rs) で追加

- `CursorIcon` (現状 `daw_ui_platform::CursorIcon` のみ pub)
- `Note`, `NoteId`, `PianoRollResponse`, `PianoRollStyle`

---

## ergonomic 評価ポイント (Phase 44 で総括)

### 41 で観測する pattern

1. **Arc<[T]> snapshot capture** → 41a-c で 5 ペア → **41d で `Edit::snapshot_inverse` 化**で吸収済
2. **id-based field set helper** (move / resize) → 41b で 2 個の helper、library 化は不要 (skeleton が深い、generic 化のコスト > local helper のコスト)
3. **`begin_group / end_group` の need** → Phase 41 で 0 (複数対応 helper で 1 Edit に集約済)。Phase 42 (audio buffer trim → fade) で発生したら別途実装

### Phase 44 への引き継ぎ

- `Edit::snapshot_inverse` の使い心地を Phase 42 (sample_edit_ops) で再検証 (Vec<f32> = `Arc<[f32]>` snapshot で同 helper が再利用できるか)
- `PianoRollStyle` / `PianoRollResponse` API の改善余地を Phase 42-43 で発見次第 breaking 変更 (容認方針)
- `begin_group / end_group` を Phase 42 で実装する場合、本 plan の設計判断 1 を更新

---

## リスクと back-out plan

### R1: `Ui::set_cursor` の race condition

**症状**: 同 frame 内で複数 widget が cursor 要求して順序依存 (例: rect-select 中に move handle hover で flickering)。

**back-out**: pending_cursor を `Option<(priority: u8, cursor: CursorIcon)>` に拡張、後方互換維持。コスト: ui.rs 数十行。

### R2: HeavyCtx 包括 delegate が API 表面を膨張させすぎ

**症状**: rustdoc が長い、利用率の低い method がある。

**back-out**: 利用率の低い method を `#[doc(hidden)]` で隠す (削除はしない、heavy 抽象の漏れを塞ぐ目的を優先)。

### R3: `Edit::snapshot_inverse` が generic 制約で example の書き心地を悪化

**症状**: `S: Send + Sync + 'static` の trait bound で型推論失敗、利用者が Arc を明示する必要が出る。

**back-out**: signature を simplify (`S: Send + Sync + Clone + 'static` に格上げして Arc を内部で取らない、等)、または helper を non-generic な per-type 版 (`Edit::snapshot_inverse_arc<T: Send + Sync + ?Sized>`) に分割。

### R4: piano_roll widget 化で API が読みづらい

**症状**: builder pattern が複雑 / Style の field 数が多い / Response の variant が多い。

**back-out**: Phase 42-43 で発見次第 breaking 変更 (容認方針)。Phase 44 で総括して widget API を 1 度大きく整理する commit を入れる。

### R5: daw_01 (path 依存先) の breaking 影響

**症状**: 41e の library widget 化で daw_01 の build が壊れる。

**back-out**: 41e の commit 内で daw_01 側も更新 (同 commit、CLAUDE.md の「全 example / test / docs を 1 commit で一括更新」方針)。daw_01 が gui_01 の Note 型を直接 import していなければ影響軽微。

---

## 進捗管理

各 commit 完了時に本ファイルの該当節先頭に `✅ done (commit <hash>)` を追記する。

[docs/history.md](history.md) には Phase 41 全体の完了概要を 41f で 1 ブロック追記。

`docs/plan.md` の M9 Phase 41 進捗マークも 41f で更新。
