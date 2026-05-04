//! `piano_roll` widget — 100k notes 級 piano roll の library widget (M9 Phase 41e)。
//!
//! 設計:
//! - **Note schema** (id / start_beat / len_beats / pitch / velocity) は library 公開型。
//!   id は `u32` で生成時に割り当て、move/delete でも不変 (multi-select identity 安定)。
//! - **描画 + drag state machine + hit-test + shortcut + rect select** は widget 内に閉じる。
//!   heavy() ブロック + cached(viewport_key) で背景描画を粗粒度キャッシュ。
//! - **Edit 構築は callback** で user に委譲 (`make_edit: Fn(NotesEditRequest) -> Edit<M>`)。
//!   widget 自身は ユーザ Model 型を知らないので no-Clone 不変条件と整合する。
//! - **drag 中は library が overlay 描画、release frame で初めて `NotesEditRequest::Move`
//!   / `Resize` を発行** (commit-by-release pattern)。drag 中の Mutate Edit は発行せず、
//!   user の Model.notes は release まで不変。これにより `NotesEditRequest` は 5 variants
//!   で完結し、Mutate/Undoable 区別の boilerplate を排除。
//! - **state 配置**: drag anchor / pending_click は内部 `WidgetState` (ephemeral)、
//!   selected_note_ids は外部 `&[NoteId]` (immutable borrow、Model 側 single source of truth)。
//!   selection 変更は `NotesEditRequest::Select` Edit を push_edit で発行し、frame 末で
//!   model に apply される (= 次フレームで反映)。`UiHost::frame` の closure が `&M` 制約
//!   のため `&mut` borrow は不可、push_edit ベースが no-Clone 不変条件と整合する設計。
//!
//! # 使い方 (example/piano_roll/src/main.rs を参照)
//!
//! ```ignore
//! use daw_ui_core::{Note, NoteId, NotesEditRequest, PianoRollStyle, PianoRollView};
//!
//! ui.piano_roll(
//!     id, rect,
//!     &model.notes, view, &model.selected_note_ids,
//!     &PianoRollStyle::default(),
//!     |req| match req {
//!         NotesEditRequest::Add(notes)        => make_add_notes_edit(notes),
//!         NotesEditRequest::Delete(notes)     => make_delete_notes_edit(notes),
//!         NotesEditRequest::Move(d)           => make_move_notes_edit(d),
//!         NotesEditRequest::Resize(d)         => make_resize_notes_edit(d),
//!         NotesEditRequest::Select { prev, next } => make_select_notes_edit(prev, next),
//!     },
//! );
//! ```

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::Arc;

use daw_ui_platform::CursorIcon;
use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::Ui;
use crate::widgets::playhead::draw_playhead_line;

// ============================================================
// Public types
// ============================================================

/// note 識別子。生成時に割り当て、編集中も不変 (multi-select identity 安定)。
pub type NoteId = u32;

/// piano roll の 1 note (library 公開型)。
///
/// schema (M9 Phase 44c で f64 化 + lyric 追加):
/// - `id: NoteId` — 生成時に割り当て (`PianoRollModel::next_note_id` 等)、move/delete でも不変
/// - `start_beat: f64` — 開始位置 (拍単位、0.0 = 最初の拍)。f64 で長尺 song / sample 精度を確保
/// - `len_beats: f64` — 長さ (拍単位)
/// - `pitch: u8` — MIDI 0..127 (実用 36..96 が C2..C7)
/// - `velocity: u8` — MIDI 0..127 (色濃度に使う、`PianoRollStyle::note_fill_fn` で Color に変換)
/// - `lyric: Option<Arc<str>>` — singing synthesis 用歌詞 (VOICEVOX 等)、note 上に表示。
///   `None` なら lyric 表示せず。`Arc<str>` で多数 note 間の文字列共有を可能にする。
///
/// `Copy` ではない (`Arc<str>` のため `Clone` のみ)。closure capture / sort では参照渡しで対応。
#[derive(Clone, Debug)]
pub struct Note {
    pub id: NoteId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub pitch: u8,
    pub velocity: u8,
    pub lyric: Option<Arc<str>>,
}

/// move helper の delta タプル: (id, prev_start_beat, prev_pitch, next_start_beat, next_pitch)。
pub type MoveDelta = (NoteId, f64, u8, f64, u8);

/// resize helper の delta タプル: (id, prev_start_beat, prev_len_beats, next_start_beat, next_len_beats)。
/// ResizeRight (右端 drag) は prev_start == next_start、ResizeLeft (左端 drag) は両方変わる。
pub type ResizeDelta = (NoteId, f64, f64, f64, f64);

/// note drag の種別 (hit-test 結果)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteDragKind {
    /// note 中央 drag = 平行移動 (start_beat + pitch を更新)。
    Move,
    /// note 左端 4px drag = 左端 resize (start_beat + len_beats を更新)。
    ResizeLeft,
    /// note 右端 4px drag = 右端 resize (len_beats のみ更新)。
    ResizeRight,
}

/// piano roll の view 状態 (pan / zoom)。値渡し (Copy) で widget に渡す。
/// pan/zoom の更新は user 側 (widget は描画と drag のみ担う)。
///
/// 拍は `f64` (`Note.start_beat` / `len_beats` と同精度)、pitch は `f32` (実用上 0..127 の範囲なので
/// f32 で十分)。
#[derive(Clone, Copy, Debug)]
pub struct PianoRollView {
    /// 表示 left の拍 (浮動小数で smooth scroll)。
    pub start_beat: f64,
    /// 表示する拍範囲 (= zoom 倍率の逆数)。例: `view_len_beats=4.0` で 1 小節幅。
    pub len_beats: f64,
    /// 表示 top の MIDI ピッチ (浮動小数で smooth scroll)。
    pub pitch_top: f32,
    /// 表示する pitch 範囲 (例: `24.0` で 2 オクターブ)。
    pub pitch_visible: f32,
    /// 鍵盤領域の幅 (px)。`0.0` で keyboard 非表示、grid のみ。
    pub keyboard_w: f32,
    /// notes / id を編集するたびに bump する hook (cache busting)。
    pub notes_generation: u64,
    /// (M9 Phase 45c) velocity lane の高さ (px)。`0.0` で disabled。
    /// `> 0` のとき rect の下から `velocity_lane_h` px を velocity lane として確保し、
    /// 残りを既存の grid + keyboard に配分する (velocity lane は keyboard 領域には被せない、
    /// grid と同じ x 範囲のみ)。
    pub velocity_lane_h: f32,
    /// (M9 Phase 45c) playhead 線を描く拍位置 (拍)。`None` で disabled。
    /// `Some(b)` で `b` が `[start_beat, start_beat + len_beats]` 範囲内なら、
    /// note grid と velocity lane を縦断する 1 本の線が描かれる。
    pub playhead_beat: Option<f64>,
}

/// piano roll が user に発行する Edit 要求の種別。
///
/// **このタプル/enum は 1 frame 内で消費される一時 ADT** であり、Application::Message
/// のように Model に保存される / Clone 伝染する性質はない。メッセージ型禁止の不変条件と矛盾しない。
///
/// drag 中の連続更新は library が overlay 描画で実現し、release frame でのみ
/// `Move` / `Resize` を発行する (commit-by-release pattern)。`MoveContinue` 等は持たない。
#[derive(Debug)]
pub enum NotesEditRequest {
    /// note を追加 (Insert shortcut)。Undoable。
    Add(Vec<Note>),
    /// 選択中 note を削除 (Delete shortcut)。Undoable。
    Delete(Vec<Note>),
    /// drag release で平行移動。Undoable。
    Move(Vec<MoveDelta>),
    /// drag release で resize。Undoable。
    Resize(Vec<ResizeDelta>),
    /// rect select (Shift+drag、加算) または click で selection を更新。Undoable。
    Select { prev: Vec<NoteId>, next: Vec<NoteId> },
}

/// `Ui::piano_roll` の戻り値。app 側で connection / hover state の表示に使う。
///
/// `Vec<Edit<M>>` は載せない (widget が `ui.push_edit` で内部発行する、fader と同パターン)。
#[derive(Clone, Debug, Default)]
pub struct PianoRollResponse {
    /// pointer が grid 内にあるか (keyboard 領域は除く)。
    pub hovered: bool,
    /// hover 中の note id (note 上にあるとき)。
    pub hovered_note_id: Option<NoteId>,
    /// hover 中の note hit zone (cursor 表示判断用)。
    pub hovered_zone: Option<NoteDragKind>,
    /// drag 中ならその種別。
    pub dragging: Option<NoteDragKind>,
    /// Shift+drag rect select (加算) が active か (HUD / status bar 表示用)。
    pub rect_select_active: bool,
    /// このフレームで `NotesEditRequest::Select` を push_edit したか (= 次フレームで
    /// `selected` が変わる予定であることを app 側 UI に伝える、selection 連動 UI のトリガー)。
    pub selection_changed: bool,
    /// drag<16px の short click の grid 上 (beat: f64, pitch: f32) (snap 前)。Insert 等の代替起点に使える。
    pub clicked_at_beat_pitch: Option<(f64, f32)>,
}

/// velocity → fill Color の関数。`fn` pointer (closure 不可、Style: Copy 維持のため)。
pub type NoteFillFn = fn(velocity: u8) -> Color;

/// piano roll の見た目スタイル。`Default` で example の見た目を再現。
///
/// `note_fill_fn` は velocity を Color に変換する関数 (default = `default_velocity_color`)。
#[derive(Clone, Copy, Debug)]
pub struct PianoRollStyle {
    pub bg: Color,
    pub keyboard_bg: Color,
    pub white_key: Color,
    pub black_key: Color,
    /// grid 内の黒鍵 row 帯 (薄い網)。
    pub black_row_overlay: Color,
    /// 4 拍ごとの太線 (小節線)。
    pub bar_line: Color,
    /// 1 拍ごとの細線。
    pub beat_line: Color,
    pub bar_line_width_px: f32,
    pub beat_line_width_px: f32,
    pub note_fill_fn: NoteFillFn,
    pub note_border_radius_px: f32,
    pub note_selected_fill: Color,
    pub note_selected_border: Color,
    pub note_selected_border_w: f32,
    pub note_selected_pad_px: f32,
    /// resize handle の幅 (px)。note 左右この px 内 = resize、それ以外 = move。
    pub resize_handle_px: f32,
    pub c_label_color: Color,
    pub c_label_font_px: f32,
    /// M9 Phase 44c: note 上に重ねて描画する歌詞 (lyric) の色とフォントサイズ。
    /// `Note.lyric == Some(...)` のとき note rect 上端に label 描画。
    pub lyric_color: Color,
    pub lyric_font_px: f32,
    /// (M9 Phase 45c) playhead 線の色。`PianoRollView::playhead_beat == Some(_)` のときのみ使用。
    pub playhead_color: Color,
    /// (M9 Phase 45c) playhead 線の幅 (px)。bar_line と紛れない程度に太くする。
    pub playhead_width_px: f32,
    /// (M9 Phase 45c) velocity lane の背景色。
    pub velocity_lane_bg: Color,
    /// (M9 Phase 45c) velocity bar の色。selection 反映は将来 phase (drag editing と一緒)。
    pub velocity_bar_color: Color,
    /// (M9 Phase 45c) velocity bar の幅 (px)。
    pub velocity_bar_width_px: f32,
}

/// デフォルト velocity color (青系の濃淡 0.5..0.95)。`PianoRollStyle::note_fill_fn` の初期値。
#[must_use]
pub fn default_velocity_color(velocity: u8) -> Color {
    let t = f32::from(velocity) / 127.0;
    Color::rgba(0.35 + t * 0.35, 0.55 + t * 0.30, 0.85 + t * 0.10, 1.0)
}

impl Default for PianoRollStyle {
    fn default() -> Self {
        Self {
            bg: Color::rgb(0.12, 0.13, 0.16),
            keyboard_bg: Color::rgb(0.22, 0.23, 0.26),
            white_key: Color::rgb(0.92, 0.93, 0.95),
            black_key: Color::rgb(0.10, 0.11, 0.13),
            black_row_overlay: Color::rgba(1.0, 1.0, 1.0, 0.04),
            bar_line: Color::rgba(1.0, 1.0, 1.0, 0.30),
            beat_line: Color::rgba(1.0, 1.0, 1.0, 0.12),
            bar_line_width_px: 1.5,
            beat_line_width_px: 1.0,
            note_fill_fn: default_velocity_color,
            note_border_radius_px: 1.5,
            note_selected_fill: Color::rgb(1.0, 0.85, 0.30),
            note_selected_border: Color::rgb(1.0, 1.0, 1.0),
            note_selected_border_w: 2.0,
            note_selected_pad_px: 2.0,
            resize_handle_px: 4.0,
            c_label_color: Color::rgb(0.30, 0.30, 0.35),
            c_label_font_px: 11.0,
            // M9 Phase 45c: playhead / velocity lane defaults
            // playhead は bar_line (白 alpha 0.3) と紛れないよう強い赤系 + 太め
            playhead_color: Color::rgb(1.0, 0.25, 0.10),
            playhead_width_px: 2.5,
            velocity_lane_bg: Color::rgb(0.16, 0.17, 0.20),
            velocity_bar_color: Color::rgb(0.50, 0.65, 0.85),
            velocity_bar_width_px: 3.0,
            lyric_color: Color::rgb(0.10, 0.10, 0.15),
            lyric_font_px: 9.0,
        }
    }
}

// ============================================================
// Public pure functions (app 側で hit-test / 座標変換に使える)
// ============================================================

/// 1 つの note の screen 座標 rect を返す (pan/zoom 適用後、grid 外も含む raw rect)。
/// app 側で「click 位置から note を逆引き」「drag preview rect」等の座標計算に使える。
///
/// Note は `Clone` のみ (Arc<str> lyric のため) なので参照渡し。
#[must_use]
pub fn note_to_rect(note: &Note, view: PianoRollView, grid: Rect) -> Rect {
    note_geometry_to_rect(note.start_beat, note.len_beats, note.pitch, view, grid)
}

/// 内部 helper: note geometry から rect を計算。`note_to_rect` と drag preview から呼ばれる。
/// 拍は f64、最終的な pixel 座標は f32 にcast (描画用)。
fn note_geometry_to_rect(
    start_beat: f64,
    len_beats: f64,
    pitch: u8,
    view: PianoRollView,
    grid: Rect,
) -> Rect {
    let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
    let pitch_to_px = grid.h / view.pitch_visible.max(1e-6);
    let x = grid.x + ((start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((len_beats * beat_to_px) as f32).max(1.5);
    let y = grid.y + (view.pitch_top - f32::from(pitch)) * pitch_to_px;
    let h = (pitch_to_px - 1.0).max(2.0);
    Rect { x, y, w, h }
}

/// note hit-test (visible filtering 後)。grid 内の cursor 位置で hit する note の id と
/// hit zone (Move / ResizeLeft / ResizeRight) を返す。後勝ち (描画順で前面)。
///
/// `notes` は start_beat 昇順にソート済を仮定 (二分探索で visible 範囲を絞る)。
#[must_use]
pub fn note_hit(
    notes: &[Note],
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<(NoteId, NoteDragKind)> {
    if !grid.contains(cx, cy) {
        return None;
    }
    let view_start = view.start_beat;
    let view_end = view_start + view.len_beats;
    let s_idx = notes.partition_point(|n| n.start_beat + n.len_beats < view_start);
    let e_idx = s_idx + notes[s_idx..].partition_point(|n| n.start_beat <= view_end);
    let visible = &notes[s_idx..e_idx];

    let mut hit: Option<(NoteId, NoteDragKind)> = None;
    for note in visible {
        let r = note_to_rect(note, view, grid);
        if !r.contains(cx, cy) {
            continue;
        }
        let edge = resize_handle_px;
        let kind = if r.w > edge * 2.0 && cx - r.x < edge {
            NoteDragKind::ResizeLeft
        } else if r.w > edge * 2.0 && (r.x + r.w) - cx < edge {
            NoteDragKind::ResizeRight
        } else {
            NoteDragKind::Move
        };
        hit = Some((note.id, kind));
    }
    hit
}

/// hover 中の note hit zone から cursor 形状を決める。
/// 左右 `resize_handle_px` = `EwResize`、中央 = `Move`、grid 外や note 上以外は None。
#[must_use]
pub fn note_hover_cursor(
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<CursorIcon> {
    if !grid.contains(cx, cy) {
        return None;
    }
    let mut hit_cursor: Option<CursorIcon> = None;
    for note in visible {
        let r = note_to_rect(note, view, grid);
        if !r.contains(cx, cy) {
            continue;
        }
        let edge = resize_handle_px;
        let cursor = if r.w > edge * 2.0 && (cx - r.x < edge || (r.x + r.w) - cx < edge) {
            CursorIcon::EwResize
        } else {
            CursorIcon::Move
        };
        hit_cursor = Some(cursor);
    }
    hit_cursor
}

/// 2 つの矩形が交差するか (Shift+drag rect select で使う)。接するだけは交差扱いしない。
#[must_use]
pub fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// 黒鍵判定 (C# / D# / F# / G# / A#)。
#[must_use]
pub fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

// ============================================================
// Internal state (widget 内部のみ、`pub(crate)`)
// ============================================================

/// drag 開始時の各対象 note の値スナップショット。
#[derive(Clone, Copy, Debug)]
struct NoteDragAnchor {
    id: NoteId,
    start_beat: f64,
    pitch: u8,
    len_beats: f64,
}

/// 1 度の note drag セッション (move / resize / 複数同時)。
#[derive(Clone, Debug)]
struct NoteDragSession {
    kind: NoteDragKind,
    /// drag 開始時のマウス位置 (screen)。
    anchor_mouse: (f32, f32),
    anchors: Vec<NoteDragAnchor>,
}

/// piano_roll widget の永続状態 (`UiHost.state` に置かれる)。
#[derive(Debug, Default)]
pub(crate) struct PianoRollState {
    /// note drag (Move / ResizeLeft / ResizeRight) の anchor。drag release で None に戻す。
    note_drag: Option<NoteDragSession>,
}

// ============================================================
// Internal helpers (描画)
// ============================================================

/// note rect を生成 (角丸 + radius 指定)。
fn note_rect_command(rect: Rect, fill: Color, radius_px: f32) -> RectCommand {
    RectCommand {
        rect,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [radius_px; 4],
        clip_rect: None,
    }
}

/// drag preview の shifted note geometry を計算 (drag 中の表示用、元 Note は不変)。
/// kind に応じて start_beat / pitch / len_beats を delta で更新した tuple を返す
/// (Note を返さないのは Note が `Arc<str>` lyric を持つので Copy できないため、
/// drag preview で必要な geometry 3 つだけ返す)。
fn drag_preview_geometry(
    anchor: NoteDragAnchor,
    kind: NoteDragKind,
    beat_delta: f64,
    pitch_delta: i32,
) -> (f64, f64, u8) {
    match kind {
        NoteDragKind::Move => (
            (anchor.start_beat + beat_delta).max(0.0),
            anchor.len_beats,
            (i32::from(anchor.pitch) + pitch_delta).clamp(0, 127) as u8,
        ),
        NoteDragKind::ResizeRight => (
            anchor.start_beat,
            (anchor.len_beats + beat_delta).max(0.05),
            anchor.pitch,
        ),
        NoteDragKind::ResizeLeft => {
            let max_start = anchor.start_beat + anchor.len_beats - 0.05;
            let new_start = (anchor.start_beat + beat_delta).clamp(0.0, max_start);
            let actual_delta = new_start - anchor.start_beat;
            (new_start, (anchor.len_beats - actual_delta).max(0.05), anchor.pitch)
        }
    }
}

// ============================================================
// Public widget API
// ============================================================

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// piano roll widget (M9 Phase 41e)。
    ///
    /// - `id`: 同 frame 内で複数 piano_roll を並べる場合は `(label, index)` 等で一意に。
    /// - `rect`: 描画矩形 (左端 `style.keyboard_w` 分は keyboard 領域、残りが grid)。
    /// - `notes`: 描画対象の note 列 (start_beat 昇順ソート前提、二分探索で visible filter)。
    /// - `view`: pan/zoom 状態 (値渡し)。pan/zoom 更新は user 側責務 (widget は描画のみ)。
    /// - `selected`: 選択中 note の id 集合 (immutable borrow、Model 側 single source of truth)。
    /// - `style`: 見た目スタイル (`PianoRollStyle::default()` で example の見た目を再現)。
    /// - `make_edit`: 各種 Edit 要求を `Edit<M>` に変換する callback。
    ///   `Add` / `Delete` / `Move` / `Resize` / `Select` の 5 variant を dispatch する。
    ///
    /// 戻り値 `PianoRollResponse` で hover / drag / selection 変化を取得できる。
    ///
    /// # 操作
    /// - **note 中央 drag** = move (release で `NotesEditRequest::Move` 発行、Undoable)
    /// - **note 左右端 drag** = resize (release で `NotesEditRequest::Resize` 発行、Undoable)
    /// - **note click** (drag<16px) = selection 1 個 (`NotesEditRequest::Select` 発行)
    /// - **空白 click** = selection clear (同上)
    /// - **Shift+drag** = rect multi-select、**加算** (release で `NotesEditRequest::Select` 発行、
    ///   既存 `selected` ∪ rect 内の note ids)。排他にしたい場合は空白 click で clear してから drag
    /// - **Insert** shortcut = pointer 位置に新規 note 追加 (`NotesEditRequest::Add`)。
    ///   `id` は user 側で `next_note_id` 等で割り当て、`make_edit` callback 内で参照する
    ///   ため、widget は **id=0 placeholder で `Add(vec![note_with_id_0])` を渡す**。
    ///   user 側で id を上書きしてから push (= user 側で `m.next_note_id` を bump)。
    /// - **Delete** shortcut = selected を一括削除 (`NotesEditRequest::Delete`)
    /// - **note hover** で cursor を `Move` / `EwResize` に切替
    ///
    /// # pan/zoom について
    ///
    /// widget は pan/zoom を自前で扱わない (view ownership は app 側)。
    /// user は widget の **外** で `if resp.dragging.is_none() { /* pan logic */ }` のように
    /// drag 中でないとき pan を実装する。または `note_hit(...)` を呼んで note 上でない
    /// press のみ pan を始める (= 現 example の semantics)。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn piano_roll<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        notes: &[Note],
        view: PianoRollView,
        selected: &[NoteId],
        style: &PianoRollStyle,
        make_edit: F,
    ) -> PianoRollResponse
    where
        F: Fn(NotesEditRequest) -> Edit<M> + Send + Sync + 'static,
    {
        let wid = WidgetId::ROOT.child((b"piano_roll_widget", &id));
        let pointer = self.pointer;

        // grid / keyboard / velocity lane レイアウト (M9 Phase 45c で vel_area 追加)。
        // velocity_lane_h > 0 のとき rect の下端 vel_h px を vel_area として確保し、
        // 残り main_h を keyboard + grid に配分する (vel_area は keyboard に被せない)。
        let kbd_w = view.keyboard_w.max(0.0);
        let vel_h = view.velocity_lane_h.max(0.0).min(rect.h * 0.5);
        let main_h = (rect.h - vel_h).max(1.0);
        let grid = Rect {
            x: rect.x + kbd_w,
            y: rect.y,
            w: (rect.w - kbd_w).max(1.0),
            h: main_h,
        };
        let kbd = Rect { x: rect.x, y: rect.y, w: kbd_w, h: main_h };
        let vel_area = Rect {
            x: rect.x + kbd_w,
            y: rect.y + main_h,
            w: (rect.w - kbd_w).max(1.0),
            h: vel_h,
        };

        // visible filter (二分探索)
        let view_end_beat = view.start_beat + view.len_beats;
        let s_idx =
            notes.partition_point(|n| n.start_beat + n.len_beats < view.start_beat);
        let e_idx = s_idx
            + notes[s_idx..].partition_point(|n| n.start_beat <= view_end_beat);
        let visible: &[Note] = &notes[s_idx..e_idx];

        // ----- press 振り分け (state 更新) -----
        // Shift+drag は take_drag_rect_in_rect が drag state を握るので、ここでは
        // 「Shift なし note hit」だけ widget が drag を始める。
        let just_pressed_on_note = pointer.primary_just_pressed
            && !pointer.modifiers.shift
            && pointer.pos.is_some_and(|(px, py)| grid.contains(px, py));

        if just_pressed_on_note
            && let Some((px, py)) = pointer.pos
            && let Some((hit_id, kind)) =
                note_hit(notes, view, grid, px, py, style.resize_handle_px)
        {
            let drag_ids: Vec<NoteId> = if selected.contains(&hit_id) {
                selected.to_vec()
            } else {
                vec![hit_id]
            };
            let anchors: Vec<NoteDragAnchor> = drag_ids
                .iter()
                .filter_map(|id_target| {
                    notes.iter().find(|n| n.id == *id_target).map(|n| NoteDragAnchor {
                        id: n.id,
                        start_beat: n.start_beat,
                        pitch: n.pitch,
                        len_beats: n.len_beats,
                    })
                })
                .collect();
            if !anchors.is_empty() {
                let state: &mut PianoRollState = self.widget_state(wid);
                state.note_drag =
                    Some(NoteDragSession { kind, anchor_mouse: (px, py), anchors });
            }
        }

        // ----- drag continue (描画用 delta を計算) + release 検出 -----
        // 拍は f64、pixel は f32 なので変換を 1 箇所で吸収。
        let beat_per_px: f64 = view.len_beats / f64::from(grid.w.max(1.0));
        let pitch_per_px = view.pitch_visible / grid.h.max(1.0);
        let drag_session: Option<NoteDragSession> = {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.note_drag.clone()
        };
        // drag release で取り出すが、drag 距離が 16px 未満なら **click に格下げ** する
        // (= 短い「press → release」は note 中央上の click として selection 切替に振り向ける)。
        let drag_release_raw: Option<NoteDragSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.note_drag.take()
        } else {
            None
        };
        let (drag_release, drag_short_click_pos): (Option<NoteDragSession>, Option<(f32, f32)>) =
            if let Some(nd) = drag_release_raw {
                let dist = pointer.pos.map_or(0.0, |(px, py)| {
                    (px - nd.anchor_mouse.0).abs() + (py - nd.anchor_mouse.1).abs()
                });
                if dist < 16.0 {
                    (None, pointer.pos)
                } else {
                    (Some(nd), None)
                }
            } else {
                (None, None)
            };

        // drag 中の delta (pointer から計算)。beat_delta は f64、pitch_delta は i32 (整数 pitch 単位)。
        let drag_overlay: Option<(NoteDragSession, f64, i32)> = drag_session
            .as_ref()
            .and_then(|nd| pointer.pos.map(|p| (nd.clone(), p)))
            .map(|(nd, (px, py))| {
                let dx = px - nd.anchor_mouse.0;
                let dy = py - nd.anchor_mouse.1;
                let beat_delta = f64::from(dx) * beat_per_px;
                let pitch_delta = (-(dy * pitch_per_px)).round() as i32;
                (nd, beat_delta, pitch_delta)
            });

        // ----- Response 初期値 + hover 計算 -----
        let mut response = PianoRollResponse {
            hovered: pointer
                .pos
                .is_some_and(|(px, py)| grid.contains(px, py)),
            ..Default::default()
        };
        if let Some((cx, cy)) = pointer.pos
            && grid.contains(cx, cy)
            && let Some((hover_id, hover_kind)) =
                note_hit(notes, view, grid, cx, cy, style.resize_handle_px)
        {
            response.hovered_note_id = Some(hover_id);
            response.hovered_zone = Some(hover_kind);
        }
        response.dragging = drag_session.as_ref().map(|nd| nd.kind);

        // hover 中の cursor 形状要求 (note 上のみ、drag 中は drag kind 対応)
        if response.dragging.is_some() {
            let cursor = match response.dragging {
                Some(NoteDragKind::Move) => CursorIcon::Move,
                Some(NoteDragKind::ResizeLeft | NoteDragKind::ResizeRight) => {
                    CursorIcon::EwResize
                }
                None => CursorIcon::Default,
            };
            self.set_cursor(cursor);
        } else if let Some((cx, cy)) = pointer.pos
            && let Some(cursor) =
                note_hover_cursor(visible, view, grid, cx, cy, style.resize_handle_px)
        {
            self.set_cursor(cursor);
        }

        // ----- pending click 判定 -----
        // 2 通り: (a) drag が起こらなかった pure release、(b) drag は始まったが <16px で
        // click に格下げされた release。どちらも grid 上の click として selection 切替の
        // trigger に使う。
        let pending_click: Option<(f32, f32)> = if drag_release.is_some() {
            None
        } else if let Some(p) = drag_short_click_pos {
            Some(p)
        } else if pointer.primary_just_released
            && !pointer.modifiers.shift
            && let Some((px, py)) = pointer.pos
        {
            Some((px, py))
        } else {
            None
        };

        // ----- 描画 (heavy ブロック + cached + 動的 overlay) -----
        // M9 Phase 45c: viewport_key に vel_h を追加 (velocity lane 高さ変化で cache 無効化)。
        let viewport_key = (
            b"piano_roll_widget_v1" as &[u8],
            view.start_beat.to_bits(),
            view.len_beats.to_bits(),
            view.pitch_top.to_bits(),
            view.pitch_visible.to_bits(),
            grid.w.to_bits(),
            grid.h.to_bits(),
            kbd.w.to_bits(),
            view.notes_generation,
            vel_h.to_bits(),
        );

        let visible_owned: Vec<Note> = visible.to_vec();
        let style_copy = *style;
        let view_copy = view;
        // selected は heavy 内 borrow 不可なので Vec を所有権渡しで closure に取り込む
        let selected_set: HashSet<NoteId> = selected.iter().copied().collect();
        let drag_overlay_clone = drag_overlay.clone();

        self.heavy(("piano_roll_inner", &id), move |hctx| {
            // === cached(): viewport_key 一致時に skip される背景レイヤ ===
            hctx.cached(viewport_key, |hctx| {
                draw_grid_background(hctx, grid, kbd, view_copy, &style_copy);
                draw_notes(
                    hctx,
                    &visible_owned,
                    view_copy,
                    grid,
                    style_copy.note_fill_fn,
                    style_copy.note_border_radius_px,
                    style_copy.lyric_color,
                    style_copy.lyric_font_px,
                );
                // M9 Phase 45c: velocity lane (vel_h > 0 のとき内蔵描画)
                if vel_h > 0.0 {
                    draw_velocity_lane(hctx, &visible_owned, view_copy, vel_area, &style_copy);
                }
            });

            // === cached の外: 動的 overlay (selection / drag preview / cursor / playhead) ===
            // selection overlay
            if !selected_set.is_empty() {
                draw_selection_overlay(
                    hctx,
                    &visible_owned,
                    &selected_set,
                    view_copy,
                    grid,
                    &style_copy,
                );
            }
            // drag preview (drag 中の shifted rect)
            if let Some((nd, bd, pd)) = drag_overlay_clone {
                draw_drag_preview(hctx, &nd, view_copy, grid, &style_copy, bd, pd);
            }
            // M9 Phase 45c: playhead 線 (time で動くので cache 対象外、毎フレーム描画)。
            // 範囲外なら描画スキップ。grid と vel_area を縦断する 1 本。
            if let Some(b) = view_copy.playhead_beat
                && b >= view_copy.start_beat
                && b <= view_copy.start_beat + view_copy.len_beats
            {
                let beat_to_px = f64::from(grid.w) / view_copy.len_beats.max(1e-6);
                let x = grid.x + ((b - view_copy.start_beat) * beat_to_px) as f32;
                let y_top = grid.y;
                let y_bottom = vel_area.y + vel_area.h;
                draw_playhead_line(
                    hctx,
                    x,
                    y_top,
                    y_bottom,
                    style_copy.playhead_color,
                    style_copy.playhead_width_px,
                );
            }
        });

        // ----- shortcut: Insert (note 追加) / Delete (selected 削除) -----
        if self.take_shortcut("add_note")
            && let Some((cx, cy)) = pointer.pos
            && grid.contains(cx, cy)
        {
            let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
            let pitch_to_px = grid.h / view.pitch_visible.max(1e-6);
            let start_beat = (view.start_beat + f64::from(cx - grid.x) / beat_to_px).max(0.0);
            let pitch_f = view.pitch_top - (cy - grid.y) / pitch_to_px;
            let pitch = (pitch_f.round() as i32).clamp(0, 127) as u8;
            // note id は user 側で next_note_id を bump して上書き。
            // ここでは placeholder id=0 で渡す (user は make_edit closure 内で bump 済 id を使う)。
            let new_note = Note {
                id: 0,
                start_beat,
                len_beats: 0.5,
                pitch,
                velocity: 96,
                lyric: None,
            };
            self.push_edit(make_edit(NotesEditRequest::Add(vec![new_note])));
        }

        if self.take_shortcut("delete") && !selected.is_empty() {
            let sel_set: HashSet<NoteId> = selected.iter().copied().collect();
            let to_delete: Vec<Note> =
                notes.iter().filter(|n| sel_set.contains(&n.id)).cloned().collect();
            if !to_delete.is_empty() {
                self.push_edit(make_edit(NotesEditRequest::Delete(to_delete)));
            }
        }

        // ----- pending click → selection 切替 (Edit 発行のみ、外部 selected は frame 末で apply 後に反映) -----
        if let Some((cx, cy)) = pending_click {
            let prev: Vec<NoteId> = selected.to_vec();
            let new_sel: Vec<NoteId> = if grid.contains(cx, cy) {
                if let Some((hit_id, _)) =
                    note_hit(notes, view, grid, cx, cy, style.resize_handle_px)
                {
                    vec![hit_id]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            if prev != new_sel {
                self.push_edit(make_edit(NotesEditRequest::Select {
                    prev,
                    next: new_sel,
                }));
                response.selection_changed = true;
            }
            // grid 内の short click なら beat/pitch も Response に載せる
            if grid.contains(cx, cy) {
                let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
                let pitch_to_px = grid.h / view.pitch_visible.max(1e-6);
                let beat = view.start_beat + f64::from(cx - grid.x) / beat_to_px;
                let pitch = view.pitch_top - (cy - grid.y) / pitch_to_px;
                response.clicked_at_beat_pitch = Some((beat, pitch));
            }
        }

        // ----- drag release → Move / Resize Edit 発行 -----
        if let Some(nd) = drag_release {
            let (beat_delta, pitch_delta): (f64, i32) = pointer.pos.map_or((0.0, 0), |(px, py)| {
                let dx = px - nd.anchor_mouse.0;
                let dy = py - nd.anchor_mouse.1;
                (f64::from(dx) * beat_per_px, (-(dy * pitch_per_px)).round() as i32)
            });

            match nd.kind {
                NoteDragKind::Move => {
                    let mut deltas: Vec<MoveDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_start = (a.start_beat + beat_delta).max(0.0);
                        let new_pitch =
                            (i32::from(a.pitch) + pitch_delta).clamp(0, 127) as u8;
                        if (new_start - a.start_beat).abs() > 1e-6 || new_pitch != a.pitch
                        {
                            deltas.push((a.id, a.start_beat, a.pitch, new_start, new_pitch));
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(NotesEditRequest::Move(deltas)));
                    }
                }
                NoteDragKind::ResizeRight => {
                    let mut deltas: Vec<ResizeDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_len = (a.len_beats + beat_delta).max(0.05);
                        if (new_len - a.len_beats).abs() > 1e-6 {
                            deltas.push((
                                a.id,
                                a.start_beat,
                                a.len_beats,
                                a.start_beat,
                                new_len,
                            ));
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(NotesEditRequest::Resize(deltas)));
                    }
                }
                NoteDragKind::ResizeLeft => {
                    let mut deltas: Vec<ResizeDelta> = Vec::new();
                    for a in &nd.anchors {
                        let max_start = a.start_beat + a.len_beats - 0.05;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual_delta = new_start - a.start_beat;
                        let new_len = (a.len_beats - actual_delta).max(0.05);
                        if (new_start - a.start_beat).abs() > 1e-6
                            || (new_len - a.len_beats).abs() > 1e-6
                        {
                            deltas.push((
                                a.id,
                                a.start_beat,
                                a.len_beats,
                                new_start,
                                new_len,
                            ));
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(NotesEditRequest::Resize(deltas)));
                    }
                }
            }
        }

        // ----- Shift+drag rect multi-select (加算) -----
        // `take_drag_rect_in_rect` は呼ぶだけで cyan 半透明 overlay を自動描画するため、
        // pan / note drag と紛らわしくないよう **「Shift 押下時の press」または「既に rect-select
        // drag が active」のときだけ呼ぶ**。drag 開始は press 時に Shift を見て gate、
        // drag 中は state.drag_start.is_some() で active 判定して呼び続ける (release で finished)。
        //
        // 加算の意味: release frame で `next = prev ∪ rect_inside`。daw_01 旧自前実装と
        // DAW 業界慣習 (Cubase / Logic / Bitwig) に合わせる。排他 (現選択を捨てて新規 rect)
        // は「空白 click で clear → Shift+drag」の 2 ステップで実現可能 (新規 API 不要)。
        let drag_rect_wid = wid.child(b"rect_select");
        let shift_rect_active = {
            let state: &mut crate::widgets::drag_rect::DragRectState =
                self.widget_state(drag_rect_wid);
            state.drag_start.is_some()
        };
        let shift_press = pointer.primary_just_pressed && pointer.modifiers.shift;
        if (shift_press || shift_rect_active)
            && let Some(drag) = self.take_drag_rect_in_rect(drag_rect_wid, grid)
        {
            response.rect_select_active = true;
            if drag.modifiers.shift && drag.finished {
                let drag_rect = drag.rect();
                let mut set: HashSet<NoteId> = selected.iter().copied().collect();
                for n in visible {
                    let r = note_to_rect(n, view, grid);
                    if rects_intersect(r, drag_rect) {
                        set.insert(n.id);
                    }
                }
                let mut new_ids: Vec<NoteId> = set.into_iter().collect();
                new_ids.sort_unstable();
                let prev: Vec<NoteId> = selected.to_vec();
                let mut prev_sorted = prev.clone();
                prev_sorted.sort_unstable();
                if prev_sorted != new_ids {
                    self.push_edit(make_edit(NotesEditRequest::Select {
                        prev,
                        next: new_ids,
                    }));
                    response.selection_changed = true;
                }
            }
        }

        response
    }
}

// ============================================================
// Internal drawing helpers
// ============================================================

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn draw_grid_background<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    grid: Rect,
    kbd: Rect,
    view: PianoRollView,
    style: &PianoRollStyle,
) {
    // (a) 主領域背景
    hctx.push_rect(RectCommand {
        rect: grid,
        fill: style.bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    // (b) 黒鍵 row 帯
    let pitch_to_px = grid.h / view.pitch_visible.max(1e-6);
    let pitch_top_int = view.pitch_top.floor() as i32;
    let pitch_visible_int = view.pitch_visible.ceil() as i32;
    for i in 0..=pitch_visible_int {
        let pitch = pitch_top_int - i;
        if !(0..=127).contains(&pitch) {
            continue;
        }
        if is_black_key(pitch as u8) {
            let y = grid.y + (view.pitch_top - pitch as f32) * pitch_to_px;
            hctx.push_rect(RectCommand {
                rect: Rect { x: grid.x, y, w: grid.w, h: pitch_to_px },
                fill: style.black_row_overlay,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
    }

    // (c) 拍縦線 (1 拍ごと細線、4 拍ごと太線)
    let view_end_beat = view.start_beat + view.len_beats;
    let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
    let first_beat = view.start_beat.floor() as i32;
    let last_beat = view_end_beat.ceil() as i32;
    for b in first_beat..=last_beat {
        let x = grid.x + ((f64::from(b) - view.start_beat) * beat_to_px) as f32;
        if x < grid.x - 1.0 || x > grid.x + grid.w + 1.0 {
            continue;
        }
        let is_bar = b.rem_euclid(4) == 0;
        let (line_w, color) = if is_bar {
            (style.bar_line_width_px, style.bar_line)
        } else {
            (style.beat_line_width_px, style.beat_line)
        };
        hctx.push_rect(RectCommand {
            rect: Rect { x: x - line_w * 0.5, y: grid.y, w: line_w, h: grid.h },
            fill: color,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }

    // (d) keyboard widget (左端、kbd.w > 0 のみ)
    if kbd.w > 0.0 {
        // 背景
        hctx.push_rect(RectCommand {
            rect: kbd,
            fill: style.keyboard_bg,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
        // 各 pitch を rect で描画
        for i in 0..=pitch_visible_int {
            let pitch = pitch_top_int - i;
            if !(0..=127).contains(&pitch) {
                continue;
            }
            let y = grid.y + (view.pitch_top - pitch as f32) * pitch_to_px;
            let key_rect = Rect {
                x: kbd.x,
                y,
                w: (kbd.w - 1.0).max(0.0),
                h: (pitch_to_px - 1.0).max(0.0),
            };
            let fill = if is_black_key(pitch as u8) {
                style.black_key
            } else {
                style.white_key
            };
            hctx.push_rect(RectCommand {
                rect: key_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
            // C のオクターブのみラベル
            if (pitch as u8).is_multiple_of(12) && pitch_to_px >= 8.0 {
                let octave = (pitch / 12) - 1;
                hctx.push_text(GlyphArea {
                    text: format!("C{octave}"),
                    left: kbd.x + 4.0,
                    top: y,
                    font_size: style.c_label_font_px,
                    line_height: style.c_label_font_px * 1.2,
                    color: style.c_label_color,
                    clip_rect: None,
                });
            }
        }
    }
}

/// visible note を grid 内に clip して描画。M9 Phase 44c 以降 `lyric: Some(...)` が
/// あれば note rect の左端に重ねて描画する (note 高さが lyric font 1 行分以上あるときのみ)。
#[allow(clippy::too_many_arguments)]
fn draw_notes<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    note_fill_fn: NoteFillFn,
    radius_px: f32,
    lyric_color: Color,
    lyric_font_px: f32,
) {
    for note in visible {
        let r = note_to_rect(note, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        let clipped = Rect {
            x: x_left,
            y: y_top,
            w: x_right - x_left,
            h: y_bot - y_top,
        };
        hctx.push_rect(note_rect_command(clipped, note_fill_fn(note.velocity), radius_px));
        // M9 Phase 44c: lyric を note rect 内左端に描画 (note の高さが font 1 行に届くとき)。
        if let Some(lyric) = note.lyric.as_ref()
            && clipped.h >= lyric_font_px + 1.0
            && clipped.w >= lyric_font_px
        {
            hctx.push_text(GlyphArea {
                text: lyric.to_string(),
                left: clipped.x + 1.0,
                top: clipped.y,
                font_size: lyric_font_px,
                line_height: lyric_font_px * 1.1,
                color: lyric_color,
                clip_rect: Some(clipped),
            });
        }
    }
}

/// selected note に黄色ハイライト + 白枠 overlay を描画 (cached の外、毎フレーム)。
fn draw_selection_overlay<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    selected_set: &HashSet<NoteId>,
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
) {
    for note in visible {
        if !selected_set.contains(&note.id) {
            continue;
        }
        let r = note_to_rect(note, view, grid);
        let pad = style.note_selected_pad_px;
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: r.x - pad,
                y: r.y - pad,
                w: r.w + pad * 2.0,
                h: r.h + pad * 2.0,
            },
            fill: style.note_selected_fill,
            border: style.note_selected_border,
            border_width: style.note_selected_border_w,
            radius: [3.0; 4],
            clip_rect: None,
        });
    }
}

/// drag 中の shifted note rect (drag preview) を描画。
fn draw_drag_preview<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    nd: &NoteDragSession,
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
    beat_delta: f64,
    pitch_delta: i32,
) {
    for a in &nd.anchors {
        let (start_beat, len_beats, pitch) =
            drag_preview_geometry(*a, nd.kind, beat_delta, pitch_delta);
        let r = note_geometry_to_rect(start_beat, len_beats, pitch, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: x_left,
                y: y_top,
                w: x_right - x_left,
                h: y_bot - y_top,
            },
            fill: style.note_selected_fill,
            border: style.note_selected_border,
            border_width: style.note_selected_border_w,
            radius: [style.note_border_radius_px; 4],
            clip_rect: None,
        });
    }
}

/// (M9 Phase 45c) velocity lane の描画。`vel_area` は keyboard を除いた grid と同じ x 範囲。
/// 各 visible note の start_beat 位置に幅 `style.velocity_bar_width_px` の縦 bar を、
/// `velocity / 127` の比率で高さを決めて bottom-aligned で描画する。
fn draw_velocity_lane<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    vel_area: Rect,
    style: &PianoRollStyle,
) {
    // 背景塗り
    hctx.push_rect(RectCommand {
        rect: vel_area,
        fill: style.velocity_lane_bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    let beat_to_px = f64::from(vel_area.w) / view.len_beats.max(1e-6);
    let half_w = style.velocity_bar_width_px * 0.5;
    for n in visible {
        let bar_h = vel_area.h * (f32::from(n.velocity) / 127.0);
        if bar_h <= 0.0 {
            continue;
        }
        let cx = vel_area.x + ((n.start_beat - view.start_beat) * beat_to_px) as f32;
        // grid 範囲外は skip (visible は端に半分はみ出る note も含み得る)
        if cx + half_w < vel_area.x || cx - half_w > vel_area.x + vel_area.w {
            continue;
        }
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: cx - half_w,
                y: vel_area.y + vel_area.h - bar_h,
                w: style.velocity_bar_width_px,
                h: bar_h,
            },
            fill: style.velocity_bar_color,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }
}

// (M9 Phase 45e) `draw_playhead_line` は `crate::widgets::playhead` に切り出し済み。
// piano_roll / arrangement 両方が `pub(crate) fn` を呼ぶ形にリファクタ。

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;
    use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey, PhysicalSize};
    use daw_ui_renderer::Scene;

    fn note(id: NoteId, start: f64, len: f64, pitch: u8) -> Note {
        Note { id, start_beat: start, len_beats: len, pitch, velocity: 96, lyric: None }
    }

    fn test_view() -> PianoRollView {
        PianoRollView {
            start_beat: 0.0,
            len_beats: 4.0,
            pitch_top: 72.0,
            pitch_visible: 24.0,
            keyboard_w: 0.0,
            notes_generation: 0,
            velocity_lane_h: 0.0,
            playhead_beat: None,
        }
    }

    // -------- Pure function tests (note_hit / note_hover_cursor / rects_intersect) --------

    #[test]
    fn rects_intersect_overlapping_returns_true() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 5.0, y: 5.0, w: 10.0, h: 10.0 };
        assert!(rects_intersect(a, b));
    }

    #[test]
    fn rects_intersect_disjoint_returns_false() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 20.0, y: 20.0, w: 10.0, h: 10.0 };
        assert!(!rects_intersect(a, b));
    }

    #[test]
    fn rects_intersect_touching_returns_false() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 10.0, y: 0.0, w: 10.0, h: 10.0 };
        assert!(!rects_intersect(a, b));
    }

    /// note (id 0) start=1, len=1, pitch=60 → grid (50,0)-(450,200) で x∈[150,250]、y≈100。
    fn make_test_setup() -> (Vec<Note>, PianoRollView, Rect) {
        let notes = vec![note(0, 1.0, 1.0, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        (notes, view, grid)
    }

    #[test]
    fn note_hit_returns_move_in_center() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 200.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::Move)));
    }

    #[test]
    fn note_hit_returns_resize_left_at_left_edge() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 151.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeLeft)));
    }

    #[test]
    fn note_hit_returns_resize_right_at_right_edge() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 249.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeRight)));
    }

    #[test]
    fn note_hit_returns_none_outside_grid() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 10.0, 10.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn note_hit_returns_none_on_empty_grid_area() {
        let (notes, view, grid) = make_test_setup();
        // grid 内だが note の上ではない
        let hit = note_hit(&notes, view, grid, 200.0, 10.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn note_hover_cursor_returns_move_in_center() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 200.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::Move));
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_left_edge() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 151.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_right_edge() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 249.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
    }

    #[test]
    fn note_hover_cursor_returns_none_outside_grid() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 10.0, 10.0, 4.0);
        assert_eq!(cursor, None);
    }

    #[test]
    fn note_hover_cursor_returns_none_on_empty_area() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 200.0, 10.0, 4.0);
        assert_eq!(cursor, None);
    }

    #[test]
    fn note_to_rect_basic_position() {
        let (_, view, grid) = make_test_setup();
        let r = note_to_rect(&note(0, 1.0, 1.0, 60), view, grid);
        // beat_to_px = 400/4 = 100, pitch_to_px = 200/24 ≈ 8.33
        // x = 50 + 1*100 = 150, w = 1*100 = 100
        assert!((r.x - 150.0).abs() < 1e-3);
        assert!((r.w - 100.0).abs() < 1e-3);
    }

    #[test]
    fn is_black_key_basic() {
        assert!(is_black_key(1)); // C#
        assert!(is_black_key(3)); // D#
        assert!(!is_black_key(0)); // C
        assert!(!is_black_key(4)); // E
    }

    // -------- Widget integration tests --------

    /// 簡易テスト Model (no-Clone 不変条件: Clone/Default/Hash 不要)。
    struct TestModel {
        notes: Vec<Note>,
        selected: Vec<NoteId>,
        last_request: Option<RequestKind>,
        last_select_prev: Option<Vec<NoteId>>,
        last_select_next: Option<Vec<NoteId>>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RequestKind {
        Add,
        Delete,
        Move,
        Resize,
        Select,
    }

    impl TestModel {
        fn new(notes: Vec<Note>) -> Self {
            Self {
                notes,
                selected: Vec::new(),
                last_request: None,
                last_select_prev: None,
                last_select_next: None,
            }
        }
    }

    fn make_dispatch(
    ) -> impl Fn(NotesEditRequest) -> Edit<TestModel> + Send + Sync + 'static + Clone {
        |req: NotesEditRequest| -> Edit<TestModel> {
            match req {
                NotesEditRequest::Add(_) => {
                    Edit::mutate(|m: &mut TestModel| m.last_request = Some(RequestKind::Add))
                }
                NotesEditRequest::Delete(notes) => Edit::mutate(move |m: &mut TestModel| {
                    m.last_request = Some(RequestKind::Delete);
                    let ids: HashSet<NoteId> = notes.iter().map(|n| n.id).collect();
                    m.notes.retain(|x| !ids.contains(&x.id));
                }),
                NotesEditRequest::Move(_) => {
                    Edit::mutate(|m: &mut TestModel| m.last_request = Some(RequestKind::Move))
                }
                NotesEditRequest::Resize(_) => {
                    Edit::mutate(|m: &mut TestModel| m.last_request = Some(RequestKind::Resize))
                }
                NotesEditRequest::Select { prev, next } => {
                    let prev_clone = prev.clone();
                    let next_clone = next.clone();
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::Select);
                        m.last_select_prev = Some(prev_clone.clone());
                        m.last_select_next = Some(next_clone.clone());
                        m.selected.clone_from(&next_clone);
                    })
                }
            }
        }
    }

    /// rect (0,0)-(800,400) の grid 内 (kbd_w=0, view 4 拍 × 24 pitch)。
    fn run_frame<F: FnOnce(&mut Ui<'_, TestModel>)>(
        host: &mut UiHost<TestModel>,
        model: &mut TestModel,
        input: FrameInput,
        f: F,
    ) {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        let edits = host.frame_to_edits(model, &mut scene, screen, input, |_, ui| {
            f(ui);
        });
        for e in edits {
            e.apply(model);
        }
    }

    /// Insert shortcut で Add request が発行される (id=0 placeholder)。
    #[test]
    fn piano_roll_pushes_add_edit_on_insert_shortcut() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.shortcut_map_mut().bind("add_note", "Insert");
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let key = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Insert,
        };
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((100.0, 100.0)),
                ..PointerFrame::default()
            },
            keyboard: vec![key],
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input, |ui| {
            let sel: Vec<NoteId> = vec![];
            let dispatch = make_dispatch();
            let _resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Add));
    }

    /// Delete shortcut + selected あり → Delete request 発行。
    #[test]
    fn piano_roll_pushes_delete_edit_on_delete_shortcut() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 0.0, 0.5, 60), note(2, 1.0, 0.5, 64)]);
        model.selected = vec![1, 2];
        let view = test_view();
        let style = PianoRollStyle::default();
        let key = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Delete,
        };
        let input = FrameInput {
            keyboard: vec![key],
            ..Default::default()
        };
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Delete));
        // dispatch が Delete を実行すると notes が retain される
        assert!(model.notes.is_empty(), "id 1, 2 が削除された");
    }

    /// short click (drag<16px) on note → Select request、Response.selection_changed = true。
    #[test]
    fn piano_roll_response_emits_selection_changed_on_note_click() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(7, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();

        // press → release を 1 frame で同時に流す (= short click)
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((300.0, 200.0)),
                primary_just_pressed: true,
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel: Vec<NoteId> = vec![];
        let resp_changed = std::cell::Cell::new(false);
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
            resp_changed.set(resp.selection_changed);
        });
        assert_eq!(model.last_request, Some(RequestKind::Select));
        // grid の幅 800、4 拍なので beat_to_px = 200。x=300 = beat 1.5 → note (id 7, start=1, len=1) 上
        assert_eq!(model.last_select_next, Some(vec![7]));
        assert!(resp_changed.get(), "selection_changed = true");
    }

    /// short click on empty area → Select { next: vec![] } (selection clear)。
    #[test]
    fn piano_roll_response_clears_selection_on_empty_click() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 1.0, 1.0, 60)]);
        model.selected = vec![1];
        let view = test_view();
        let style = PianoRollStyle::default();
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)), // 空白の上 (y=50 は note y≈100 から離れている)
                primary_just_pressed: true,
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_select_next, Some(vec![]));
    }

    /// 同 frame に 2 個の piano_roll を `(label, idx)` で並べ、片方の selected が他方に漏れない。
    #[test]
    fn piano_roll_two_instances_independent_state() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let sel0: Vec<NoteId> = vec![1];
        let sel1: Vec<NoteId> = vec![2];

        run_frame(&mut host, &mut model, FrameInput::default(), |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                ("pr", 0),
                Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 },
                &[],
                view,
                &sel0,
                &style,
                dispatch.clone(),
            );
            let _ = ui.piano_roll(
                ("pr", 1),
                Rect { x: 400.0, y: 0.0, w: 400.0, h: 400.0 },
                &[],
                view,
                &sel1,
                &style,
                dispatch,
            );
        });
        // 何も操作しないので selected はそのまま (片方が他方に漏れない)
        assert_eq!(sel0, vec![1]);
        assert_eq!(sel1, vec![2]);
    }

    /// hover で Response.hovered_zone が ResizeLeft (note 左端 1px 内側に pointer)。
    #[test]
    fn piano_roll_response_hovered_zone_resize_left_at_edge() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        // note (id 0) start=1, len=1, pitch=60 → grid (0,0)-(800,400) で x∈[200,400]、y≈200
        // x=201 が左端 4px 内 (= ResizeLeft)
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((201.0, 200.0)),
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let zone = std::cell::Cell::new(None);
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
            zone.set(resp.hovered_zone);
        });
        assert_eq!(zone.get(), Some(NoteDragKind::ResizeLeft));
    }

    /// drag 中 (press → continue) で Response.dragging が Move、まだ Edit は発行されない。
    #[test]
    fn piano_roll_no_edit_during_drag_only_at_release() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: press at note 中央
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((300.0, 200.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let dragging1 = std::cell::Cell::new(None);
        let notes_clone = model.notes.clone();
        let view_owned = view;
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view_owned,
                &sel,
                &style,
                dispatch,
            );
            dragging1.set(resp.dragging);
        });
        assert_eq!(dragging1.get(), Some(NoteDragKind::Move));
        assert_eq!(model.last_request, None, "drag 中は Edit 発行せず");

        // Frame 2: release at 移動先
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 200.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view_owned,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Move), "release で Move 発行");
    }

    /// Modifiers が default (= 修飾なし) で、modifier の Default 実装がエラーで失敗しないことの確認。
    #[test]
    fn modifiers_default_is_no_alt() {
        let m = Modifiers::default();
        assert!(!m.alt);
        assert!(!m.shift);
    }

    /// Shift+drag rect select が **加算**: 既選択 [3] + rect 内 {1, 2} → next = [1, 2, 3] (sorted)。
    /// daw_01 旧自前実装の慣習に合致。
    #[test]
    fn piano_roll_shift_drag_is_additive() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        // note 1: x ∈ [100, 200], y ≈ [116.67, 133.33]
        // note 2: x ∈ [200, 300], y ≈ [133.33, 150.00]
        // note 3: x ∈ [600, 700], y ≈ [200.00, 216.67] (rect 外、prev に保持)
        let mut model = TestModel::new(vec![
            note(1, 0.5, 0.5, 65),
            note(2, 1.0, 0.5, 64),
            note(3, 3.0, 0.5, 60),
        ]);
        model.selected = vec![3];
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();

        // Frame 1: Shift+press at (50, 50) — drag 開始 (空白なので note hit せず、shift 経路へ)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { shift: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });
        // drag 中はまだ Edit 発行せず
        assert_eq!(model.last_request, None, "drag 中は Select 発行せず");

        // Frame 2: release at (350, 200) — finished で rect (50,50)-(350,200) が確定
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((350.0, 200.0)),
                primary_just_released: true,
                modifiers: Modifiers { shift: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Select), "release で Select 発行");
        assert_eq!(
            model.last_select_next,
            Some(vec![1, 2, 3]),
            "加算: prev [3] + rect 内 {{1, 2}} = sorted [1, 2, 3]"
        );
        assert_eq!(model.last_select_prev, Some(vec![3]));
    }

    /// 旧仕様 (Alt+drag rect select) は廃止された: Alt+drag で press → release しても
    /// Select request は発行されない。
    #[test]
    fn piano_roll_alt_drag_no_longer_selects() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 0.5, 0.5, 65), note(2, 1.0, 0.5, 64)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();

        // Frame 1: Alt+press at (50, 50)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { alt: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });

        // Frame 2: Alt+release at (350, 200) — 旧仕様なら Select 発行、新仕様では発行されない
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((350.0, 200.0)),
                primary_just_released: true,
                modifiers: Modifiers { alt: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, None, "Alt+drag は rect select を起動しない");
    }

    // ============================================================
    // M9 Phase 45c: velocity lane + playhead 内蔵描画のテスト
    // ============================================================

    /// 同じ widget 呼び出しで velocity_lane_h を変えたときの scene.rects 差分を測る helper。
    /// 各テストは別々の `host` で cache 状態を分離する (heavy() の cache key が再構築される)。
    fn count_rects_with_view(view: PianoRollView, notes: &[Note]) -> (usize, usize) {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let model = TestModel::new(notes.to_vec());
        let style = PianoRollStyle::default();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &model.notes,
                view,
                &[],
                &style,
                dispatch,
            );
        });
        (scene.rect_count(), scene.line_count())
    }

    #[test]
    fn velocity_lane_disabled_by_default() {
        let v_off = test_view(); // velocity_lane_h: 0.0
        let mut v_on = test_view();
        v_on.velocity_lane_h = 60.0;

        let notes = vec![note(1, 0.5, 0.5, 60), note(2, 1.5, 0.5, 64)];
        let (rects_off, _) = count_rects_with_view(v_off, &notes);
        let (rects_on, _) = count_rects_with_view(v_on, &notes);

        // velocity_lane_h > 0 で bg 1 + bar 2 = 3 個多い rect が積まれる。
        assert_eq!(rects_on, rects_off + 3, "velocity lane bg + 2 bars が追加される");
    }

    #[test]
    fn velocity_lane_skips_zero_velocity_bars() {
        // velocity 0 のみの notes で vel_h: 0 vs 60 の差分を測る。
        // 期待: bg 1 個のみ追加 (bar は skip)、合計差分 = 1。
        let v_off = test_view();
        let mut v_on = test_view();
        v_on.velocity_lane_h = 60.0;
        let n_zero = vec![Note {
            id: 1,
            start_beat: 0.5,
            len_beats: 0.5,
            pitch: 60,
            velocity: 0,
            lyric: None,
        }];
        let (rects_off, _) = count_rects_with_view(v_off, &n_zero);
        let (rects_on, _) = count_rects_with_view(v_on, &n_zero);

        assert_eq!(rects_on, rects_off + 1, "velocity 0 のみなら bar は出ず bg のみ追加");
    }

    #[test]
    fn playhead_renders_line_batch_when_in_range() {
        let v_off = test_view(); // playhead_beat: None
        let mut v_on = test_view();
        v_on.playhead_beat = Some(2.0); // len_beats=4.0、範囲内

        let notes = vec![note(1, 0.5, 0.5, 60)];
        let (_, lines_off) = count_rects_with_view(v_off, &notes);
        let (_, lines_on) = count_rects_with_view(v_on, &notes);

        assert_eq!(lines_on, lines_off + 1, "playhead 1 LineBatch が追加される");
    }

    #[test]
    fn playhead_skipped_when_out_of_range() {
        let v_off = test_view();
        let mut v_out = test_view();
        v_out.playhead_beat = Some(100.0); // start=0, len=4 の範囲外

        let notes = vec![note(1, 0.5, 0.5, 60)];
        let (_, lines_off) = count_rects_with_view(v_off, &notes);
        let (_, lines_out) = count_rects_with_view(v_out, &notes);

        assert_eq!(lines_off, lines_out, "範囲外 playhead は描画されない");
    }

    #[test]
    fn playhead_and_velocity_lane_combine() {
        let v_off = test_view();
        let mut v_both = test_view();
        v_both.velocity_lane_h = 60.0;
        v_both.playhead_beat = Some(2.0);

        let notes = vec![note(1, 0.5, 0.5, 60), note(2, 1.5, 0.5, 64)];
        let (rects_off, lines_off) = count_rects_with_view(v_off, &notes);
        let (rects_both, lines_both) = count_rects_with_view(v_both, &notes);

        // velocity lane: bg 1 + 2 bars = 3 個 rect 増 / playhead: 1 LineBatch 増
        assert_eq!(rects_both, rects_off + 3);
        assert_eq!(lines_both, lines_off + 1);
    }
}
