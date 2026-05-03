# Plan: M9 Phase 41e + 41f — piano_roll を library widget 化 + docs 完了記録

## Context

**なぜこの変更を行うか**

`F:\dev\gui_01\crates\examples\piano_roll\src\main.rs` (1480 LOC) は M9 Phase 41a-d で `Edit::snapshot_inverse` 経由の note edit (add/delete/move/resize/select、すべて Undoable) を備えた状態にある。Phase 41 計画 (`docs/plan_phase41.md`) では 41e で **library widget 化を完遂** することが定められており、CLAUDE.md 冒頭の「理想とベストプラクティスを追求する。そのためは大胆に破壊して作り直す」方針に基づき、validation 期間中も breaking 変更を恐れず逐次反映する。

**達成したい状態**:

1. `crates/ui/src/widgets/piano_roll.rs` に library widget として `Note` / `NoteId` / `PianoRollResponse` / `PianoRollStyle` / `PianoRollView` / `NotesEditRequest` / `MoveDelta` / `ResizeDelta` を pub 公開し、`Ui::piano_roll(...)` 1 method で 100k notes 級の piano roll が描ける。
2. example (`crates/examples/piano_roll/src/main.rs`) は library widget を呼ぶ薄いシェルになり、HUD / view state / window 起動 / Edit factory dispatch のみを残す (1480 → ~700 LOC)。
3. テスト 22 ケースは library 側に 16 ケース移植 + 新規 7 ケース (Response 動作 / 複数インスタンス独立 state) で計 23 ケース。
4. Phase 41f で `docs/plan.md` の M9 進捗表と `docs/history.md` 末尾に M9 Phase 41 完了ブロックを追記。

**docs/plan.md L142 と plan_phase41.md L14 の方針矛盾の解消**: ユーザ判断により plan_phase41.md L14 の「Phase 41e で完了させる」を正とする。`docs/plan.md` L142 の「widget 化を後回し」記述は Phase 41f で更新 (= Phase 41 内で widget 化済みに書き換え)。

**daw_01 への影響**: daw_01 (path 依存先 `F:\dev\daw_01\daw_gui\`) は 12 ファイルが `daw_ui_core` を import しているが gui_01 の `Note` 型を直接参照する箇所はゼロ (NoteBox / piano_roll_view.rs はすべて自前型)。よって 41e commit で daw_01 build は壊れない。`cd F:/dev/daw_01 && cargo build` を 41e の DoD に含めて確認する。

---

## 採用する設計

### 1. Widget 化スコープ — **full widget 化 (代案 C)**

`Note` 型 + Edit factory + hit-test + 描画 + input (drag state machine + shortcut + rect select) のすべてを library 側に移す。view 状態 (start_beat / pitch_top / pitch_visible) と `next_note_id` (id 採番) は app 層責務として example に残す。

**library に移すもの** (~+790 LOC):
- 型: `Note`, `NoteId = u32`, `NoteDragKind`, `MoveDelta`, `ResizeDelta`, `NotesEditRequest`, `PianoRollView`, `PianoRollStyle`, `PianoRollResponse`, `PianoRollState` (`pub(crate)`)
- `make_*_notes_edit` 5 個 (snapshot_inverse 経由)
- `note_hit` / `note_hover_cursor` / `rects_intersect`
- `note_to_rect` / `pitch_color` (デフォルト velocity color) / `note_rect_command` / `is_black_key`
- `apply_pending_input` の note_drag 部分
- `build_ui` の grid/keyboard/notes 描画 + selection overlay + drag release + rect select + Insert/Delete shortcut

**example に残すもの** (~700 LOC):
- `PianoRollModel { notes: Vec<Note>, selected_note_ids: Vec<NoteId>, next_note_id: u32, view: PianoRollView, last_action: String, ... }`
- `generate_notes` (100k LCG)
- HUD ラベル / view state pan + wheel zoom (drag_anchor の pan のみ)
- `App::new` / `on_event` / `on_render` + `winit_backend::run_app`
- `make_edit` callback で既存 5 個の `make_*_notes_edit` を dispatch

### 2. State 配置 — **ハイブリッド** (drag 内部 / selected 外部)

| state | 配置 | 理由 |
|---|---|---|
| drag anchor / pending_click | **内部 `WidgetState`** | ephemeral (frame 間で生成 → 消費)、Model に置くと cleanup boilerplate を強要 |
| selected_note_ids | **外部 `&mut Vec<NoteId>`** | persistent (project save / 他 widget が参照)、Model 側 single source of truth |
| view 状態 (start_beat 等) | **値渡し `PianoRollView`** | pan/zoom は app 層責務、widget は描画のみ。view を mutate する API を入れると Phase 41e のスコープが膨らむ |

`tab_view_with_state` ([crates/ui/src/widgets/tab_view.rs:132](F:\dev\gui_01\crates\ui\src\widgets\tab_view.rs)) がハイブリッドの参考実装。複数インスタンス共存は `daw_prototype` の 8ch fader/level_meter (`crates/examples/daw_prototype/src/main.rs:390-422`) で実証済の `(label, index)` 複合 id パターンで担保。

### 3. Edit 発行 API — **make_edit callback (1 個に集約)**

```rust
pub enum NotesEditRequest {
    Add(Vec<Note>),
    Delete(Vec<Note>),
    Move(Vec<MoveDelta>),
    Resize(Vec<ResizeDelta>),
    Select { prev: Vec<NoteId>, next: Vec<NoteId> },
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    pub fn piano_roll<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        notes: &[Note],
        view: PianoRollView,
        selected: &mut Vec<NoteId>,
        style: &PianoRollStyle,
        make_edit: F,
    ) -> PianoRollResponse
    where
        F: Fn(NotesEditRequest) -> Edit<M> + Send + Sync + 'static;
}
```

`automation_curve` の `on_change(idx, pos) -> Edit<M>` callback と同形。`NotesEditRequest` は **1 frame 内で消費される一時 ADT** であり、Application::Message のように Model に保存される / Clone 伝染する性質はなく、メッセージ型禁止の不変条件と矛盾しない。example 側の dispatch:

```rust
ui.piano_roll(id, rect, &m.notes, view, &mut m.selected_note_ids, &style, |req| match req {
    NotesEditRequest::Add(notes)        => make_add_notes_edit(notes),
    NotesEditRequest::Delete(notes)     => make_delete_notes_edit(notes),
    NotesEditRequest::Move(d)           => make_move_notes_edit(d),
    NotesEditRequest::Resize(d)         => make_resize_notes_edit(d),
    NotesEditRequest::Select { prev, next } => make_select_notes_edit(prev, next),
})
```

### 4. PianoRollResponse — 最小

```rust
#[derive(Clone, Debug, Default)]
pub struct PianoRollResponse {
    pub hovered: bool,
    pub hovered_note_id: Option<NoteId>,
    pub hovered_zone: Option<NoteDragKind>,
    pub dragging: Option<NoteDragKind>,
    pub rect_select_active: bool,
    pub selection_changed: bool,
    pub clicked_at_beat_pitch: Option<(f32, f32)>,
}
```

`Vec<Edit<M>>` は載せない (内部 `ui.push_edit` で発行、fader と同パターン)。

### 5. PianoRollStyle — 17 field (Copy)

example の現リテラルから抽出。`Default` impl で Phase 41a-d の見た目を再現。

```rust
pub type NoteFillFn = fn(velocity: u8) -> Color;

#[derive(Clone, Copy, Debug)]
pub struct PianoRollStyle {
    pub bg: Color,                     pub keyboard_bg: Color,
    pub white_key: Color,              pub black_key: Color,
    pub black_row_overlay: Color,      pub bar_line: Color,
    pub beat_line: Color,              pub bar_line_width_px: f32,
    pub beat_line_width_px: f32,       pub note_fill_fn: NoteFillFn,
    pub note_border_radius_px: f32,    pub note_selected_fill: Color,
    pub note_selected_border: Color,   pub note_selected_border_w: f32,
    pub note_selected_pad_px: f32,     pub resize_handle_px: f32,
    pub c_label_color: Color,          pub c_label_font_px: f32,
}
```

---

## 実装順序 (sub-step)

1. **`crates/ui/src/widgets/piano_roll.rs` 新設** — 型定義 + state + Style default + 5 個の Edit factory + hit-test
2. **`Ui::piano_roll` inherent impl** — `heavy()` ブロック + `cached(viewport_key, ...)` で背景描画 + visible 範囲二分探索 (partition_point) + selection overlay (cached 外) + hit-test + drag state machine + Insert/Delete shortcut + rect select (Alt+drag)
3. **テスト移植** — 既存 16 ケース (round-trip 8 + hit-test 5 + rects_intersect 3) + 新規 7 ケース (下記「テスト戦略」)
4. **`crates/ui/src/widgets/mod.rs`** — `pub mod piano_roll;` 1 行
5. **`crates/ui/src/lib.rs`** — `Note` / `NoteId` / `NoteDragKind` / `PianoRollResponse` / `PianoRollStyle` / `PianoRollView` / `NotesEditRequest` / `MoveDelta` / `ResizeDelta` の 9 type re-export
6. **example 書き換え** — `crates/examples/piano_roll/src/main.rs` を widget 利用版に refactor (1480 → ~700 LOC)
7. **DoD 検証** — `cargo build/test/clippy --workspace` + `cd F:/dev/daw_01 && cargo build` + `cargo run --bin piano_roll` で目視
8. **commit** — `feat(M9 Phase 41e): piano_roll を crates/ui/src/widgets/piano_roll.rs に library 化`
9. **Phase 41f docs** — `docs/plan.md` の M9 進捗表を 41 完了に更新 + L142 の「widget 化を後回し」記述を「Phase 41e で完了済み」に書き換え + `docs/history.md` 末尾に M9 Phase 41 完了ブロック (41pre/41a/41b/41c/41d/41e の 6 commits まとめ) 追加。`docs/plan_phase41.md` は完了マークのみ追記。
10. **commit** — `docs(M9 Phase 41): 完了記録 + Phase 44 評価項目更新`

---

## Critical Files

### 新規
- [crates/ui/src/widgets/piano_roll.rs](F:/dev/gui_01/crates/ui/src/widgets/piano_roll.rs) (~+790 LOC: widget 本体 + 16 ケース移植 + 7 ケース新規)

### 修正
- [crates/ui/src/widgets/mod.rs](F:/dev/gui_01/crates/ui/src/widgets/mod.rs) (`pub mod piano_roll;` 1 行)
- [crates/ui/src/lib.rs](F:/dev/gui_01/crates/ui/src/lib.rs) (re-export 9 type)
- [crates/examples/piano_roll/src/main.rs](F:/dev/gui_01/crates/examples/piano_roll/src/main.rs) (1480 → ~700 LOC)
- [docs/plan.md](F:/dev/gui_01/docs/plan.md) (M9 進捗表 + L142 記述更新、L142 の「widget 化を後回し」を「Phase 41e で完了済み」に書き換え)
- [docs/history.md](F:/dev/gui_01/docs/history.md) (M9 Phase 41 完了ブロック追加)
- [docs/plan_phase41.md](F:/dev/gui_01/docs/plan_phase41.md) (各 commit ヘッダに ✅ done マーク追記)

### 参考 (再利用)
- [crates/ui/src/widgets/waveform.rs](F:/dev/gui_01/crates/ui/src/widgets/waveform.rs) — heavy 内 widget の inherent impl テンプレート (`Ui::waveform`)
- [crates/ui/src/widgets/automation.rs](F:/dev/gui_01/crates/ui/src/widgets/automation.rs) — `on_change` callback で Edit を受け取るパターン
- [crates/ui/src/widgets/tab_view.rs:132](F:/dev/gui_01/crates/ui/src/widgets/tab_view.rs) — ハイブリッド state pattern (`tab_view_with_state`)
- [crates/ui/src/widgets/fader.rs](F:/dev/gui_01/crates/ui/src/widgets/fader.rs) — 内部 `push_edit` (drag 中 Mutate / release Undoable) パターン
- [crates/ui/src/widgets/heavy.rs](F:/dev/gui_01/crates/ui/src/widgets/heavy.rs) — 41pre で追加された `take_drag_rect_in_rect` / `take_shortcut` / `context_menu_for` / `set_cursor` / `push_edit` の HeavyCtx delegate
- [crates/ui/src/edit.rs:69-112](F:/dev/gui_01/crates/ui/src/edit.rs) — `Edit::snapshot_inverse` (widget 内部から直接呼ぶ)

---

## テスト戦略

### library 側に移植 (16 ケース)

`crates/ui/src/widgets/piano_roll.rs` の `#[cfg(test)] mod tests` に移植。`run_pair` helper と `make_test_model` (`struct TestModel { notes: Vec<Note>, selected: Vec<NoteId>, generation: u64 }`) も library 内で再現。

- Edit factory round-trip (8): add_single / add_multi / delete / delete_clears_selection / add_redo / move_round_trip / resize_round_trip_right / resize_round_trip_left / move_preserves_sort_order / select_undo
- hit-test (5): note_hit_move_center / note_hit_resize_left / note_hit_resize_right / note_hit_outside_grid / note_hit_empty_grid_area
- rects_intersect (3): overlapping / disjoint / touching
- note_hover_cursor (4): EwResize_left / EwResize_right / Move_center / None_outside

### 新規追加 (7 ケース、Response + 複数インスタンス検証)

- `piano_roll_response_emits_selection_changed_on_rect_drag` — Alt+drag の release frame で `*selected` 書き換わり `resp.selection_changed = true`
- `piano_roll_response_emits_clicked_at_beat_pitch_on_short_click` — drag<16px の release で `clicked_at_beat_pitch = Some((beat, pitch))`
- `piano_roll_pushes_delete_edit_on_delete_shortcut` — `selected = vec![1, 2]` + Delete shortcut → returned edits 中に `Edit::Undoable { label: "delete notes", ... }`
- `piano_roll_pushes_move_edit_on_drag_release` — note 中央 press → 100px drag → release で Undoable label="move note" 発行 + forward 後の note 位置が drag 終端と一致
- `piano_roll_response_hovered_zone_resize_at_edge` — pointer を note 左端 2px 内側に置いたフレームで `resp.hovered_zone == Some(NoteDragKind::ResizeLeft)`
- `piano_roll_visible_culling_on_zoomed_view` — view_start_beat=10, view_len_beats=4 で beat=0 / beat=20 の note は scene.rects に乗らない
- `piano_roll_two_instances_independent_state` — `("piano_roll", 0)` と `("piano_roll", 1)` で同 frame 描画、片方の selected が他方に漏れない (DAW で複数 MIDI track が並ぶケースの担保)

### example 側

example の `tests` mod は実質ゼロに縮小 (round-trip も hit-test も library 化済み helper / type に対するテストなので library が自然な置き場所)。`make_*_notes_edit` 5 個の dispatch closure はそのまま残るが、これらは library 側 helper を呼ぶだけになるためテストはなくてよい。

### no-Clone 制約の回帰防止

`crates/ui/tests/ui/pass/basic.rs` (trybuild) に `Ui::piano_roll` 呼び出しを追加し、`Clone`/`PartialEq`/`Hash`/`Default` を実装していない Model でコンパイル可能なことを CI 固定。

---

## Verification

### Phase 41e commit 後

```bash
cargo build --workspace                                # 全 crate ビルド成功
cargo test --workspace                                 # 既存 + 新規 23 ケース pass
cargo clippy --workspace --tests -- -D warnings        # warnings ゼロ
cargo test -p daw-ui-core --test no_clone_required     # trybuild で no-Clone 維持
cd F:/dev/daw_01 && cargo build                        # path 依存先 daw_01 が壊れていない
cd F:/dev/gui_01 && cargo run --bin piano_roll         # 実機目視
```

### 実機目視チェックリスト (`cargo run --bin piano_roll`)

- [ ] 100k notes が viewport 内のみ描画されている (frame_ms < 16ms 程度)
- [ ] 黒鍵 row overlay / 拍線 / keyboard が現状と同じ見た目
- [ ] note 単一クリックで selection が 1 個になり、yellow border が出る
- [ ] note 中央 drag で move、左右 4px drag で resize、cursor が `Move` / `EwResize` に切替
- [ ] Alt+drag で rect-select 範囲内 note がすべて selected
- [ ] Insert で新規 note 追加、Delete で selected を一括削除
- [ ] Ctrl+Z で undo (move / resize / delete / add / select すべて)、Ctrl+Shift+Z で redo
- [ ] context_menu (右クリック)「Add note here / Delete selected」動作
- [ ] view pan (無修飾 drag) と zoom (wheel / Ctrl+wheel) が現状と同じ動作

### Phase 41f commit 後

- [ ] `docs/plan.md` の M9 Phase 41 進捗表が「Phase 41 完了 (6 commits)」になっている
- [ ] `docs/plan.md` L142 の「widget 化を後回し」記述が「Phase 41e で完了済み」に更新されている
- [ ] `docs/history.md` 末尾に M9 Phase 41 完了ブロックがある (41pre/41a/41b/41c/41d/41e の commit hash 列 + 主な学び + 設計判断 + Phase 42 への引き継ぎ)
- [ ] `docs/plan_phase41.md` の各 commit ヘッダに `✅ done (commit <hash>)` が追記されている

---

## Phase 44 への引き継ぎ事項 (Phase 41f に書く)

- **library widget 化の閾値判定**: 5 helper + 1 enum (`NotesEditRequest`) で吸収できた。callback 5 個個別ではなく単一 callback + ADT で API 簡潔化の precedent。Phase 42 で同様の `NotesEditRequest` 風 ADT を audio buffer 系に作るかの判断材料。
- **`Edit::snapshot_inverse` の汎用性**: Vec<Note> で 5 ペア吸収。Phase 42 の Vec<f32> 系 (sample_edit_ops trim/fade) で同 helper が再利用できるか検証。
- **Note schema の daw_01 統合**: gui_01 の `Note { id: u32, start_beat: f32, len_beats: f32, pitch: u8, velocity: u8 }` (32 byte / 16 byte align) と daw_01 の `NoteBox { note: u32, lyric, ... }` (f64 + lyric) は schema 不一致。Phase 44 で統合判断 (`f64` 化 / `lyric: Option<Arc<str>>` 追加 等) が必要。Phase 41e でこの不一致が顕在化したことを history.md に記録。
- **history group API**: Phase 41 では実装不要だった (複数対応 helper で multi-delete / multi-move が 1 Edit に集約)。Phase 42 (audio trim → fade の連続適用 等) で実需が出たら別途実装。

---

## ファイル運用 (CLAUDE.md / memory feedback_plan_storage)

承認後、本 plan を `F:\dev\gui_01\docs\plan_phase41e.md` にコピーする (`~/.claude/plans/` 配下のハッシュ命名は directory rename で破壊される)。Phase 41e の commit 内容に plan ファイルも含める。

---

## 実装ノート (Phase 41e 実装中の deviation 記録)

実装中に発見した、上記 plan からの逸脱と理由:

### Deviation 1: `selected: &mut Vec<NoteId>` を `selected: &[NoteId]` に変更

**当初の plan**: tab_view_with_state と同パターンで `&mut Vec<NoteId>` を borrow し、widget 内で書き込む。

**実装での発見**: `UiHost::frame` の closure シグネチャ `for<'a> FnOnce(&'a M, &mut Ui<'a, M>)` は `model: &M` (immutable) のため、closure 内で `&mut model.selected_note_ids` を取れない (borrow checker E0596)。

**変更後**: `selected: &[NoteId]` (immutable borrow) に変更。selection 変更は `NotesEditRequest::Select` を `push_edit` で発行し、frame 末で apply される (= 次フレームで反映)。push_edit ベースが no-Clone 不変条件と整合する設計。

### Deviation 2: Edit factory 5 個 (`make_*_notes_edit`) は library 化せず example 残し

**当初の plan**: 5 helper を library 内に移し、user は callback で dispatch するだけ。

**実装での発見**: forward / inverse closure 内で `m.notes` / `m.selected_note_ids` / `m.notes_generation` を mutate する必要がある。これを generic にするには `NotesModel` trait の導入が必要だが、daw_01 のような独自 schema (NoteBox / lyric / f64) を持つアプリでは trait impl 不可能になり、API の拡張性を損なう。

**変更後**: Edit factory は example に残し、library 側は `NotesEditRequest` enum を介した callback パターンで responsibility を分離。Edit 構築は user 責務、widget は描画 + drag SM + hit-test + shortcut + Edit 発行のトリガー検出のみ担う。

### Deviation 3: drag 中の note 更新は library overlay 描画 (commit-by-release pattern)

**当初の plan**: 詳細未確定。元 example では drag 中に直接 `m.notes` を mutate していた。

**実装での選択**: drag 中は library が overlay 描画 (元 notes は不変)、release frame で初めて `NotesEditRequest::Move` / `Resize` を発行 (Undoable Edit 1 個)。これにより `NotesEditRequest` は 5 variants で完結し、drag 中の Mutate Edit 発行や `MoveContinue` variant の追加が不要。history も「drag 1 step = 1 Edit」で綺麗に保たれる。

### Deviation 4: テストレイヤーの再配分

**当初の plan**: 16 ケース (Edit factory round-trip 8 + hit-test 5 + rects_intersect 3) を library に移植 + 7 新規ケース。

**実装での選択**: Edit factory round-trip は PianoRollModel 依存なので library 化困難 → example に残す (9 ケース)。library 側には hit-test (5) + rects_intersect (3) + note_hover_cursor (4) + note_to_rect (1) + is_black_key (1) + Modifiers default (1) = 15 ケースの純粋関数テスト + 8 ケースの Response/integration テスト = 計 23 ケース。example には Edit factory round-trip 9 ケース。

### Deviation 5: Insert shortcut の id 採番

**当初の plan**: id は user 側で `next_note_id` を bump して上書き、widget は placeholder=0 を渡す。

**実装での選択**: 計画通り placeholder=0 で渡し、user 側 `make_edit` callback で `next_id_for_add` capture。`make_add_notes_edit` の forward 内で `m.next_note_id = m.next_note_id.max(note.id + 1)` を bump 内蔵化することで、redo 時にも id 重複しない設計に。`make_edit` callback が 1 Edit しか返せない制約を「forward に bump を内蔵」で吸収。

### Deviation 6: `view: PianoRollView` の `keyboard_w` field

**当初の plan**: `style.keyboard_w` で渡す。

**実装での選択**: `view.keyboard_w` で渡す。view (pan/zoom 状態) と同じ「app 側状態」として一貫させる。Style はあくまで色・サイズ等の見た目固定値のみ。

---

## 結果 (実装後)

- **library**: `crates/ui/src/widgets/piano_roll.rs` 新設 (1565 LOC: 公開型 9 + 純粋関数 6 + Style default + `Ui::piano_roll` 本体 + tests 23 ケース)
- **example**: 1480 → 720 LOC に縮小 (描画 + drag SM + hit-test + shortcut + rect select 削除、Edit factory 5 + tests 9 残存)
- **trybuild**: `tests/ui/pass/basic.rs` に `Ui::piano_roll` 呼び出し追加 (no-Clone Model でコンパイル可能性を CI 固定)
- **検証**: `cargo build --workspace` ✅ / `cargo test --workspace` 全 pass / `cargo clippy --workspace --tests -- -D warnings` clean / `cd ../daw_01 && cargo build` ✅ (gui_01 の Note 型を import していないため影響なし)

