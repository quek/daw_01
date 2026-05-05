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
use crate::scenegraph::hash_inputs;
use crate::snap::SnapConfig;
use crate::time::{TimeDisplay, TimeMapping};
use crate::ui::Ui;
use crate::viewport::ViewportState1D;
use crate::widgets::playhead::draw_playhead_line;
use crate::widgets::time_grid::{BarBeatGridStyle, TimeRulerStyle};

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
    /// (M13 Phase 55) ruler 領域の高さ (px、`0.0` で ruler 無し → 旧 piano_roll 互換)。
    /// `> 0` のとき rect の top から `ruler_h` px を ruler として確保し、その下に keyboard /
    /// grid を配置 (keyboard と grid の `y` がともに `rect.y + ruler_h` から開始)。
    /// ruler は keyboard 領域には被せず、grid と同じ x 範囲のみ。
    pub ruler_h: f32,
    /// (M13 Phase 55) テンポ (BPM)。`time_ruler` / `bar_beat_grid` に渡す
    /// `TimeMapping::tempo_bpm` に使う。BarBeat 表示の bar 線位置計算では `time_sig` だけで
    /// 足りるが、将来 Seconds/SMPTE 切替で必要になるため field として保持。
    pub bpm: f32,
    /// (M13 Phase 55) 拍子 (numerator, denominator)。`(4, 4)` で 4/4、`(3, 4)` で 3/4、
    /// `(6, 8)` で 6/8。内部で `numerator * 4 / denominator` (= beats_per_bar) に変換。
    pub time_sig: (u8, u8),
    /// (M9 Phase 45f) drag overlay と commit 値の grid 吸着設定 (#010 [Replied])。
    /// `Default::default()` は `Adaptive` ON。 raw 動作を保ちたい caller は `SnapConfig::OFF` を渡す。
    /// drag 中 `pointer.modifiers.alt` で一時無効化。
    pub snap: SnapConfig,
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
    /// (M14 Phase 59 / daw_01 #017) note 群の lyric を一括更新。
    /// 1 commit = 1 Edit = 1 undo 単位 (歌詞 inline 編集 + モーラ単位での次 note 自動分配を
    /// 1 つの undo にまとめる)。`lyric == None` で歌詞削除 (空文字列 commit は widget 内で
    /// `None` に正規化済)。`Vec` の順序は分配順 (start_beat asc → 同 beat なら pitch desc)。
    SetLyrics(Vec<(NoteId, Option<String>)>),
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
    /// (M14 Phase 59 / daw_01 #017) 歌詞編集 mode 中の note id。`Some(_)` のとき
    /// piano_roll 内に text_input overlay が出ており、drag/resize/wheel/click は全て
    /// 短絡 (`dragging` 等は同時に None になる)。app 側で「他 UI grey-out」「Ctrl+Z 抑制」
    /// 等の判断に使える (typing_focus による global shortcut 抑制は `text_input_at` が自動)。
    pub lyric_editing: Option<NoteId>,
    /// (M14 Phase 59 / daw_01 #017) 直近 commit frame で「note 数より入力モーラが多くて
    /// 捨てた数」。0 なら通常、`>0` なら daw_01 で status bar / toast 表示等に使える。
    pub lyric_overflow_morae: usize,
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
    /// resize handle の幅 (px)。note rect 左右 edge から **内外** この px = resize、
    /// それ以外 (rect 中央) = move。短 note (`r.w <= resize_handle_px * 2`) は rect 内
    /// すべて Move、rect 外側のみ resize 判定。
    pub resize_handle_px: f32,
    pub c_label_color: Color,
    pub c_label_font_px: f32,
    /// M9 Phase 44c: note 上に重ねて描画する歌詞 (lyric) の色とフォントサイズ。
    /// `Note.lyric == Some(...)` のとき note rect 内に label 描画 (vertical center)。
    /// **(M14 Phase 59)** `lyric_font_px` は **MAX cap** として解釈される。 実 font_size は
    /// `(note_h * 0.75).clamp(7.0, lyric_font_px)` で note の高さに連動する (zoom in / out で
    /// 自動スケール、 daw_01 #017 動作確認で「9px 固定だと小さすぎる」 指摘から)。
    pub lyric_color: Color,
    pub lyric_font_px: f32,
    /// (M14 Phase 59 / daw_01 #017) 歌詞編集モード起動 shortcut name。
    /// `Some(name)` のとき `take_shortcut(name)` を 1 frame 1 度監視し、起動条件
    /// (`selected.len() == 1` かつ `lyric_editing == None`) を満たせば編集モードに入る。
    /// `None` で機能完全無効 (text_input overlay も出ない)。
    ///
    /// 既定値 `Some("piano_roll.edit_lyric")`。caller 側で
    /// `host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L")` を 1 度呼ぶ。
    /// `with_default_bindings()` には**含めない** (修飾なし `L` は他文脈で別意味になりうる
    /// ため caller opt-in 方針)。
    pub lyric_edit_shortcut: Option<&'static str>,
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
    /// (M13 Phase 55) ruler 領域の背景色 (`view.ruler_h > 0` のとき `time_ruler` の `bg` に渡す)。
    pub ruler_bg: Color,
    /// (M13 Phase 55) ruler の小節番号テキスト色 (`time_ruler` の `label_color` に渡す)。
    pub ruler_label_color: Color,
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
            // M14 Phase 59: MAX cap (実 font_size = note_h * 0.75 で note 高さスケール)。
            // 旧 9.0 固定 → 24.0 max にして zoom in 時の readable 化。
            lyric_font_px: 24.0,
            // M14 Phase 59 / daw_01 #017: 歌詞編集 (L キー) shortcut。caller が `bind("L")` する想定。
            lyric_edit_shortcut: Some("piano_roll.edit_lyric"),
            // M13 Phase 55: ruler 領域 (`view.ruler_h > 0` のときのみ描画)
            ruler_bg: Color::rgb(0.13, 0.14, 0.17),
            ruler_label_color: Color::rgb(0.85, 0.88, 0.92),
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

/// 内部 helper: cursor 位置がこの note のどの zone (Move / ResizeLeft / ResizeRight)
/// に該当するかを返す。`note_hit` / `note_hover_cursor` から共通で呼ばれる。
///
/// 判定範囲 (x 方向): note rect の左右 edge から **内外** ±`edge` px (= 8px 幅のハンドル帯)。
/// y 方向は note rect 内のみ (拡張なし、隣接 pitch との衝突回避)。
///
/// 短 note (`r.w <= edge * 2.0`) は rect 内では Move 強制 (左右 edge 領域が重なって
/// 判別不能なため)、rect 外側のみ ResizeLeft / ResizeRight として扱う。
fn note_zone_at(
    note: &Note,
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<NoteDragKind> {
    let r = note_to_rect(note, view, grid);
    // y は note rect 内のみ (Rect::contains の半開区間と整合)
    if cy < r.y || cy >= r.y + r.h {
        return None;
    }
    // x の拡張範囲 [r.x - edge, r.x + r.w + edge) 外は不参加
    if cx < r.x - edge || cx >= r.x + r.w + edge {
        return None;
    }
    let in_rect = cx >= r.x && cx < r.x + r.w;
    let near_left = cx < r.x + edge;
    let near_right = cx >= r.x + r.w - edge;
    let short_note = r.w <= edge * 2.0;

    Some(if short_note && in_rect {
        NoteDragKind::Move
    } else if near_left && (!in_rect || cx - r.x < edge) {
        NoteDragKind::ResizeLeft
    } else if near_right && (!in_rect || (r.x + r.w) - cx < edge) {
        NoteDragKind::ResizeRight
    } else {
        NoteDragKind::Move
    })
}

/// note hit-test (visible filtering 後)。grid 内の cursor 位置で hit する note の id と
/// hit zone (Move / ResizeLeft / ResizeRight) を返す。後勝ち (描画順で前面)。
///
/// resize handle は note rect の左右 edge から **内外** ±`resize_handle_px` の範囲
/// (= 8px 幅のハンドル帯)。短 note (`r.w <= resize_handle_px * 2`) は rect 内は Move 強制、
/// rect 外側のみ resize 判定。
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
        if let Some(kind) = note_zone_at(note, view, grid, cx, cy, resize_handle_px) {
            hit = Some((note.id, kind));
        }
    }
    hit
}

/// hover 中の note hit zone から cursor 形状を決める。
/// note rect 左右 edge の内外 ±`resize_handle_px` = `EwResize`、中央 = `Move`、
/// grid 外やどの note の判定範囲外も None。
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
        if let Some(kind) = note_zone_at(note, view, grid, cx, cy, resize_handle_px) {
            hit_cursor = Some(match kind {
                NoteDragKind::Move => CursorIcon::Move,
                NoteDragKind::ResizeLeft | NoteDragKind::ResizeRight => CursorIcon::EwResize,
            });
        }
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

/// 日本語テキストをモーラ単位で分割する (歌唱合成用)。
///
/// `Ui::piano_roll` の歌詞 inline 編集 (M14 Phase 59 / daw_01 #017) が「`Enter` で
/// commit text を分割して次 note へ自動分配」するために使う公開 helper。daw_01 以外
/// (将来のボーカルエディタ等) でも再利用可。
///
/// # 分割ルール (REAPER VOICEVOX script に準拠)
///
/// - 基本: 1 char = 1 モーラ
/// - **小書きかな** (`ぁぃぅぇぉ ゃゅょ っ ゎ ァィゥェォ ャュョ ッ ヮ`) は **直前の char と結合**。
///   - 例: `"きゃ"` → 1 モーラ (`["きゃ"]`)
///   - 例: `"しゅんかん"` → 4 モーラ (`["しゅ", "ん", "か", "ん"]`)
/// - 連続小書きは結合先 char に積まれる (`"きゃっ"` → `["きゃっ"]`)
/// - 先頭 char が小書きかなの場合は単独 1 モーラ (defensive、通常入力では発生しない)
/// - ASCII / 漢字 / その他はそのまま 1 char = 1 モーラ
///
/// # 例
///
/// ```
/// use daw_ui_core::split_into_morae;
/// assert_eq!(split_into_morae("あいうえ"), vec!["あ", "い", "う", "え"]);
/// assert_eq!(split_into_morae("しゅんかん"), vec!["しゅ", "ん", "か", "ん"]);
/// assert_eq!(split_into_morae(""), Vec::<String>::new());
/// ```
#[must_use]
pub fn split_into_morae(text: &str) -> Vec<String> {
    const SMALL_KANA: &[char] = &[
        'ぁ', 'ぃ', 'ぅ', 'ぇ', 'ぉ', 'ゃ', 'ゅ', 'ょ', 'っ', 'ゎ', 'ァ', 'ィ', 'ゥ', 'ェ', 'ォ',
        'ャ', 'ュ', 'ョ', 'ッ', 'ヮ',
    ];
    let mut out: Vec<String> = Vec::new();
    for c in text.chars() {
        if SMALL_KANA.contains(&c)
            && let Some(last) = out.last_mut()
        {
            last.push(c);
        } else {
            out.push(c.to_string());
        }
    }
    out
}

/// `from_id` を起点に、(start_beat asc → 同 beat なら pitch desc) 順で `count` 個の
/// 後続 note id を返す (起点 note 自身を `out[0]` に含む)。
///
/// `Ui::piano_roll` の歌詞 inline 編集 (M14 Phase 59 / daw_01 #017) が「Enter で commit
/// したモーラを起点 note + 後続 note に順次分配」するために使う内部 helper。
///
/// - `from_id` が見つからなければ空 Vec
/// - 後続 note が `count - 1` 個に満たなければ Vec の長さは `count` より小さくなる
/// - 順序は (start_beat asc, pitch desc): 同拍なら **高 pitch が先** (歌唱メロディと整合、
///   高い音を先に拾う)
///
/// `notes` が start_beat 昇順ソート済前提でも、同 beat 内の pitch 順までは保証されないため
/// 関数内で安定 sort をかける。
fn collect_next_notes_for_lyric(notes: &[Note], from_id: NoteId, count: usize) -> Vec<NoteId> {
    if count == 0 {
        return Vec::new();
    }
    let mut sorted: Vec<&Note> = notes.iter().collect();
    sorted.sort_by(|a, b| {
        a.start_beat
            .partial_cmp(&b.start_beat)
            .unwrap_or(std::cmp::Ordering::Equal)
            // pitch desc (高 pitch を先に)
            .then(b.pitch.cmp(&a.pitch))
    });
    let Some(pos) = sorted.iter().position(|n| n.id == from_id) else {
        return Vec::new();
    };
    sorted[pos..].iter().take(count).map(|n| n.id).collect()
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
    /// drag 中の最終 pointer 位置 (M9 Phase 60、 arrangement.rs と同パターン)。
    /// winit は release frame で `pointer.pos` を press 位置に戻すことがあり、 そのまま delta
    /// 計算すると beat_delta = 0 で commit されない / 「元に戻る」 ように見える bug への対策。
    /// release では `last_mouse` を delta 計算に使う = drag preview と一致する位置で確定。
    last_mouse: (f32, f32),
    /// drag 中の最終 alt 状態。 drag overlay と release commit の **両方** がこれを真値とする
    /// (`pointer.modifiers.alt` を直接見ない)。 continuation frame で毎 frame update し、
    /// release frame では `allow_update = false` で skip することで release 直前の値を保持する。
    /// これにより OS event 順序 (ModifiersChanged が MouseInput(Released) より先に来るケース)
    /// に依存せず、 overlay と commit が必ず同一値で確定する。
    last_alt: bool,
}

/// 絶対位置 snap で計算した note drag の beat delta (overlay と release commit で共有)。
/// anchor 0 の編集対象端 (Move=start / ResizeRight=end / ResizeLeft=start) の絶対位置を
/// snap → その差分を全 anchor に適用 (相対関係維持 + anchor 0 が grid に着地)。 anchors が
/// 空のときは raw を返す (defensive)。
fn compute_note_drag_beat_delta(
    nd: &NoteDragSession,
    raw_beat_delta: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> f64 {
    let Some(a0) = nd.anchors.first() else {
        return raw_beat_delta;
    };
    let pivot = match nd.kind {
        NoteDragKind::Move | NoteDragKind::ResizeLeft => a0.start_beat,
        NoteDragKind::ResizeRight => a0.start_beat + a0.len_beats,
    };
    let snapped_pivot =
        snap.snap_beat(pivot + raw_beat_delta, nd.last_alt, zoom_x_px_per_beat);
    snapped_pivot - pivot
}

/// M14 Phase 61b (#011): arrangement と同根。 caller の `notes_generation` は note 数や編集
/// epoch のみで bump しがちで、 個別 note の `(id, start_beat, len_beats, pitch, velocity,
/// lyric)` 変化が漏れると drag 残像が発生する。 widget 内部で全 visible note を fold して
/// viewport_key に追加する (caller boilerplate 強要回避、 `feedback_pursue_best_practice`)。
///
/// `lyric: Option<Arc<str>>` は **identity hash** (`Arc::as_ptr`) で扱う。 daw_01 VOICEVOX
/// 歌詞編集の `SetLyrics` は `Arc::from(...)` で新規作成するので pointer が変われば cache
/// 無効化が走る。 「同 string を別 Arc で持つ」 caller には不正確だが、 daw_01 は `Arc::clone`
/// で共有するので問題なし (実需要が出たら中身 hash に切替、 follow-up)。
///
/// 5000 note × 6 fold step = ~5μs @ 4GHz、 16ms 予算の 0.03%。
fn fold_piano_roll_note_hash(notes: &[Note]) -> u64 {
    const PRIME: u64 = 0x100_0000_01B3; // FNV-1a 64bit prime
    let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a 64bit offset basis
    for n in notes {
        h ^= u64::from(n.id);
        h = h.wrapping_mul(PRIME);
        h ^= n.start_beat.to_bits();
        h = h.wrapping_mul(PRIME);
        h ^= n.len_beats.to_bits();
        h = h.wrapping_mul(PRIME);
        h ^= u64::from(n.pitch);
        h = h.wrapping_mul(PRIME);
        h ^= u64::from(n.velocity);
        h = h.wrapping_mul(PRIME);
        if let Some(s) = &n.lyric {
            h ^= Arc::as_ptr(s).cast::<()>() as usize as u64;
            h = h.wrapping_mul(PRIME);
        }
    }
    h
}

/// piano_roll widget の永続状態 (`UiHost.state` に置かれる)。
#[derive(Debug, Default)]
pub(crate) struct PianoRollState {
    /// note drag (Move / ResizeLeft / ResizeRight) の anchor。drag release で None に戻す。
    note_drag: Option<NoteDragSession>,
    /// (M14 Phase 59 / daw_01 #017) 歌詞編集中の note id (text_input overlay 表示中)。
    /// L キー検知で `Some(selected[0])` に遷移 (`selected.len() == 1` のときのみ)、
    /// Enter で 1) commit + 分配 → 2) 次 note へ移動 or `None` 復帰、Esc で `None`、
    /// 編集対象 note が消失したら defensive で `None`。`Some(_)` のとき drag/resize/wheel/click
    /// は全て短絡 (typing_focus が立つので global shortcut も自動抑制)。
    lyric_editing: Option<NoteId>,
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
    min_len: f64,
) -> (f64, f64, u8) {
    match kind {
        NoteDragKind::Move => (
            (anchor.start_beat + beat_delta).max(0.0),
            anchor.len_beats,
            (i32::from(anchor.pitch) + pitch_delta).clamp(0, 127) as u8,
        ),
        NoteDragKind::ResizeRight => (
            anchor.start_beat,
            (anchor.len_beats + beat_delta).max(min_len),
            anchor.pitch,
        ),
        NoteDragKind::ResizeLeft => {
            let max_start = anchor.start_beat + anchor.len_beats - min_len;
            let new_start = (anchor.start_beat + beat_delta).clamp(0.0, max_start);
            let actual_delta = new_start - anchor.start_beat;
            (new_start, (anchor.len_beats - actual_delta).max(min_len), anchor.pitch)
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

        // ===== M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 mode =====
        // Frame 開始時、lyric_editing が selected と sync しているか defensive check。
        // 編集対象 note が消失したら自動で None に戻す (note 削除等のため)。
        let mut lyric_editing: Option<NoteId> = {
            let state: &mut PianoRollState = self.widget_state(wid);
            if let Some(eid) = state.lyric_editing
                && !notes.iter().any(|n| n.id == eid)
            {
                state.lyric_editing = None;
            }
            state.lyric_editing
        };
        // L キー検知: lyric_editing == None かつ selected.len() == 1 のときのみ起動。
        // `"piano_roll.edit_lyric"` は `is_typing_only_shortcut` に追加済 (M14 Phase 59)。
        // 編集中 (typing_focus = true) は shortcut layer を素通りして text_input に届く
        // (= `'l'` 文字としてタイプ可能)。take_shortcut は frame 頭の typing_lock 判定後
        // pending_shortcuts に積まれた name を引くので、編集中は false を返す。
        if lyric_editing.is_none()
            && let Some(name) = style.lyric_edit_shortcut
            && self.take_shortcut(name)
            && selected.len() == 1
        {
            lyric_editing = Some(selected[0]);
            // 編集モードに入る瞬間、stale な note_drag セッションを clear (drag 中に L
            // を押した稀なケース対策)。
            let state: &mut PianoRollState = self.widget_state(wid);
            state.lyric_editing = lyric_editing;
            state.note_drag = None;
        }
        // Esc 検知: 編集モード中の Esc は "escape" shortcut が global で consume するため
        // text_input の自前 Esc ハンドラ経由ではなく piano_roll が明示的に handle する
        // (= take_shortcut("escape") で消費 → lyric_editing = None で即時 cancel)。
        // これで「編集中の Esc → 1 frame で完全 cancel」を保証 (text_input の blur 検出
        // 経路 (resp.focused = false) は外 click 等の defensive fallback として残す)。
        if let Some(edit_id_for_esc) = lyric_editing
            && self.take_shortcut("escape")
        {
            // text_input の focus を明示的に clear (text_input id は ("piano_roll_lyric", edit_id))
            let ti_wid =
                WidgetId::ROOT.child((b"text_input", &("piano_roll_lyric", edit_id_for_esc)));
            self.clear_focus_if_focused(ti_wid);
            lyric_editing = None;
            self.widget_state::<PianoRollState>(wid).lyric_editing = None;
        }
        let editing_mode = lyric_editing.is_some();

        // grid / keyboard / velocity lane / ruler レイアウト
        // M13 Phase 55: rect の top から `ruler_h` 分を ruler 領域、その下に main_h
        // (= keyboard + grid)、最下段に vel_h (velocity lane)。`ruler_h = 0.0` で旧互換。
        let kbd_w = view.keyboard_w.max(0.0);
        let ruler_h = view.ruler_h.max(0.0).min(rect.h * 0.5);
        let vel_h = view.velocity_lane_h.max(0.0).min((rect.h - ruler_h) * 0.5);
        let main_h = (rect.h - ruler_h - vel_h).max(1.0);
        let ruler = Rect {
            x: rect.x + kbd_w,
            y: rect.y,
            w: (rect.w - kbd_w).max(1.0),
            h: ruler_h,
        };
        let grid = Rect {
            x: rect.x + kbd_w,
            y: rect.y + ruler_h,
            w: (rect.w - kbd_w).max(1.0),
            h: main_h,
        };
        let kbd = Rect { x: rect.x, y: rect.y + ruler_h, w: kbd_w, h: main_h };
        let vel_area = Rect {
            x: rect.x + kbd_w,
            y: rect.y + ruler_h + main_h,
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
        // M14 Phase 59: editing_mode 中は drag/click を全短絡。
        let just_pressed_on_note = !editing_mode
            && pointer.primary_just_pressed
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
                let press_alt = pointer.modifiers.alt;
                let state: &mut PianoRollState = self.widget_state(wid);
                state.note_drag = Some(NoteDragSession {
                    kind,
                    anchor_mouse: (px, py),
                    anchors,
                    last_mouse: (px, py),
                    last_alt: press_alt,
                });
            }
        }

        // ----- drag continue (描画用 delta を計算) + release 検出 -----
        // 拍は f64、pixel は f32 なので変換を 1 箇所で吸収。
        let beat_per_px: f64 = view.len_beats / f64::from(grid.w.max(1.0));
        // SnapConfig::Adaptive 用 zoom = grid.w / view.len_beats。 0 除算は beat_per_px 側で対処済み。
        let zoom_x_px_per_beat: f32 = (1.0 / beat_per_px) as f32;
        let pitch_per_px = view.pitch_visible / grid.h.max(1.0);
        // drag 継続中は毎 continuation frame で `last_mouse` / `last_alt` を update。
        // **release frame の `last_alt` は update しない** — 同 frame に ModifiersChanged(alt=false)
        // が先行する現象 (alt が一瞬 false に化ける) を回避するため、 release 直前 frame の値を保持する。
        // **release frame の `last_mouse` は pointer.pos が anchor と異なる場合のみ update** —
        // winit は release frame で `pointer.pos` を press 位置に戻すことがあり、 そのまま上書きすると
        // delta=0 で commit not pushed (drag が「元に戻る」 ように見える)。 pointer.pos == anchor のときは
        // continuation 由来の last_mouse を保持し、 そうでないときは pointer.pos が真値 (= 通常 release
        // pos、 OR press → 1 frame で release した short drag の release pos) として update する。
        if let Some((px, py)) = pointer.pos {
            let alt_now = pointer.modifiers.alt;
            let state: &mut PianoRollState = self.widget_state(wid);
            if let Some(ref mut nd) = state.note_drag {
                if !pointer.primary_just_released {
                    nd.last_mouse = (px, py);
                    nd.last_alt = alt_now;
                } else if (px, py) != nd.anchor_mouse {
                    nd.last_mouse = (px, py);
                }
            }
        }
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
                // dist 判定 / delta 計算は両者とも `nd.last_mouse` を真値とする (pointer.pos の
                // winit-bug 化を上の continuation block で吸収済み)。 click 短縮は pointer.pos
                // ではなく last_mouse 基準。
                // 短 click 化 (drag → click 格下げ) の閾値は **mouse jitter を ignore する程度** (4px)。
                //   - Resize (Left/Right) は常に commit (resize handle 上 click は意味なし)
                //   - Move + Alt なしのみ jitter 閾値で短 click 化 (click=selection / drag=移動 の区別)
                //   - Alt 押下中は Move でも閾値 skip (raw 微調整の明示意図)
                let dist = (nd.last_mouse.0 - nd.anchor_mouse.0).abs()
                    + (nd.last_mouse.1 - nd.anchor_mouse.1).abs();
                let is_move = matches!(nd.kind, NoteDragKind::Move);
                let demote = is_move && !nd.last_alt && dist < 4.0;
                if demote {
                    (None, pointer.pos)
                } else {
                    (Some(nd), None)
                }
            } else {
                (None, None)
            };

        // drag 中の delta (pointer から計算)。beat_delta は f64、pitch_delta は i32 (整数 pitch 単位)。
        // M9 Phase 45f: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用 (相対関係維持)。 alt 押下で snap 一時無効化。
        // alt は drag state の `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない
        // (release frame の commit と必ず同一値で確定するため)。
        let drag_overlay: Option<(NoteDragSession, f64, i32)> = drag_session
            .as_ref()
            .and_then(|nd| pointer.pos.map(|p| (nd.clone(), p)))
            .map(|(nd, (px, py))| {
                let dx = px - nd.anchor_mouse.0;
                let dy = py - nd.anchor_mouse.1;
                let raw = f64::from(dx) * beat_per_px;
                // 絶対位置 snap (詳細は `compute_note_drag_beat_delta` 参照、 arrangement と同パターン)。
                let beat_delta = compute_note_drag_beat_delta(
                    &nd,
                    raw,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
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

        // hover 中の cursor 形状要求 (drag 中は drag kind、note hover (拡張範囲含む) は
        // hover_cursor、その他 widget 内は Default に明示 reset で stale cursor を防ぐ)。
        // winit は state-full なので set_cursor を呼ばないと前フレームの形状が残る (ui.rs:999)。
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
        } else if pointer.pos.is_some_and(|(px, py)| rect.contains(px, py)) {
            self.set_cursor(CursorIcon::Default);
        }

        // ----- pending click 判定 -----
        // 2 通り: (a) drag が起こらなかった pure release、(b) drag は始まったが <16px で
        // click に格下げされた release。どちらも grid 上の click として selection 切替の
        // trigger に使う。M14 Phase 59: editing_mode 中は click を発火しない。
        let pending_click: Option<(f32, f32)> = if editing_mode || drag_release.is_some() {
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
        // M13 Phase 55: ruler_h / bpm / time_sig を追加 + v2 に bump (cache 構造変化)。
        // tuple Hash impl は 12 要素まで → bpm + time_sig を 1 つの組に纏めて 12 要素に収める。
        // M14 Phase 61b (#011): note 個別の (id, start_beat, len_beats, pitch, velocity, lyric)
        // 変化を widget 側で hash して 2 要素 outer tuple に wrap + v3 に bump (arrangement の
        // clip drag 残像と同根の予防、 caller の notes_generation は note 数や編集 epoch のみで
        // 不十分なケースを吸収)。
        let internal_note_hash = fold_piano_roll_note_hash(visible);
        let viewport_key = (
            (
                b"piano_roll_widget_v3" as &[u8],
                view.start_beat.to_bits(),
                view.len_beats.to_bits(),
                view.pitch_top.to_bits(),
                view.pitch_visible.to_bits(),
                grid.w.to_bits(),
                grid.h.to_bits(),
                kbd.w.to_bits(),
                view.notes_generation,
                vel_h.to_bits(),
                ruler_h.to_bits(),
                (view.bpm.to_bits(), u32::from(view.time_sig.0), u32::from(view.time_sig.1)),
            ),
            internal_note_hash,
        );

        // M13 Phase 55: library `time_ruler` / `bar_beat_grid` を呼ぶための共通 mapping。
        // beat 単位 view を sample 単位 ViewportState1D に変換 (sample_rate = 48k は BarBeat
        // 表示で比例定数として打ち消されるダミー)。
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: f64::from(view.bpm.max(1.0)),
            time_sig: (view.time_sig.0.max(1), view.time_sig.1.max(1)),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let sample_viewport =
            ViewportState1D::new(view.start_beat * spb, view.len_beats.max(1e-6) * spb);
        let grid_style_pr = BarBeatGridStyle {
            bar_color: style.bar_line,
            beat_color: style.beat_line,
            bar_line_width: style.bar_line_width_px,
            beat_line_width: style.beat_line_width_px,
        };
        let ruler_style_pr = TimeRulerStyle {
            bg: style.ruler_bg,
            tick_color: style.bar_line,
            label_color: style.ruler_label_color,
            bar_tick_height: 12.0,
            beat_tick_height: 5.0,
        };
        let id_for_inner: u64 = hash_inputs(&id);

        let visible_owned: Vec<Note> = visible.to_vec();
        let style_copy = *style;
        let view_copy = view;
        // selected は heavy 内 borrow 不可なので Vec を所有権渡しで closure に取り込む
        let selected_set: HashSet<NoteId> = selected.iter().copied().collect();
        let drag_overlay_clone = drag_overlay.clone();
        let lyric_editing_for_draw = lyric_editing;
        // M9 Phase 45f: drag overlay の Resize min_len は snap unit に合わせる
        // (snap_unit < 0.05 なら 0.05)。 release 側 min_len と同じ計算で一貫性確保。 alt 真値は
        // drag session の `last_alt` (overlay と release commit が必ず同一 unit で確定する)。
        // overlay 不在時 (drag していない) は min_len 自体使われないので alt = false で適当に初期化。
        let drag_overlay_alt = drag_overlay.as_ref().is_some_and(|(nd, _, _)| nd.last_alt);
        let drag_overlay_min_len: f64 = if view.snap.is_active(drag_overlay_alt) {
            view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
        } else {
            0.05
        };

        self.heavy(("piano_roll_inner", &id), move |hctx| {
            // === cached(): viewport_key 一致時に skip される背景レイヤ ===
            hctx.cached(viewport_key, |hctx| {
                draw_grid_background(hctx, grid, kbd, view_copy, &style_copy);
                hctx.bar_beat_grid(
                    ("pr_grid", id_for_inner),
                    grid,
                    mapping,
                    sample_viewport,
                    grid_style_pr,
                );
                if ruler_h > 0.0 {
                    hctx.time_ruler(
                        ("pr_ruler", id_for_inner),
                        ruler,
                        mapping,
                        sample_viewport,
                        ruler_style_pr,
                    );
                }
                draw_notes(
                    hctx,
                    &visible_owned,
                    view_copy,
                    grid,
                    style_copy.note_fill_fn,
                    style_copy.note_border_radius_px,
                );
                // M9 Phase 45c: velocity lane (vel_h > 0 のとき内蔵描画)
                if vel_h > 0.0 {
                    draw_velocity_lane(hctx, &visible_owned, view_copy, vel_area, &style_copy);
                }
            });

            // === cached の外: 動的 overlay (selection / drag preview / lyric / cursor / playhead) ===
            // selection overlay (note の上、lyric の下)
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
                draw_drag_preview(
                    hctx,
                    &nd,
                    view_copy,
                    grid,
                    &style_copy,
                    bd,
                    pd,
                    drag_overlay_min_len,
                );
            }
            // M14 Phase 59: lyric 描画 (selection overlay より後 = 黄色 fill に隠れない、
            // 編集中 note は text_input overlay に譲る)。 font_size は note 高さスケール。
            draw_lyrics(
                hctx,
                &visible_owned,
                view_copy,
                grid,
                style_copy.lyric_color,
                style_copy.lyric_font_px,
                lyric_editing_for_draw,
            );
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
        // M14 Phase 59: editing_mode 中は global shortcut が typing_focus で抑制される
        // ため take_shortcut は false を返すはずだが、defensive で明示 guard。
        if !editing_mode
            && self.take_shortcut("add_note")
            && let Some((cx, cy)) = pointer.pos
            && grid.contains(cx, cy)
        {
            let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
            let pitch_to_px = grid.h / view.pitch_visible.max(1e-6);
            let raw_start = (view.start_beat + f64::from(cx - grid.x) / beat_to_px).max(0.0);
            // M9 Phase 45f: Insert は widget 内発火、grid 吸着が UX 自然 (#010 [Replied])。
            // single frame の click なので drag state は関与せず、 直接 `pointer.modifiers.alt` を読む。
            let start_beat = view
                .snap
                .snap_beat(raw_start, pointer.modifiers.alt, zoom_x_px_per_beat)
                .max(0.0);
            let pitch_f = view.pitch_top - (cy - grid.y) / pitch_to_px;
            // M14 Phase 61d (#012): 描画式 `y = grid.y + (pitch_top - pitch) * pitch_to_px` の
            // 逆関数として ceil() を使う (pitch P の視覚行 y ∈ [(top-P)*pt, (top-P+1)*pt) なので
            // 逆引きは pitch_f ∈ (P-1, P] のとき P を返す = ceil)。 round() だと判定領域が視覚行
            // に対して半行ぶん上にずれて、 行の下半分にカーソルがあると 1 つ下のピッチに化ける。
            let pitch = (pitch_f.ceil() as i32).clamp(0, 127) as u8;
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

        if !editing_mode && self.take_shortcut("delete") && !selected.is_empty() {
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
        // M9 Phase 60: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用。 Resize の min_len は snap unit に合わせる (snap_unit < 0.05 なら 0.05)。
        // **alt は drag 中の最終 `nd.last_alt` を真値とする** — release frame の `pointer.modifiers.alt`
        // は OS event 順序 (ModifiersChanged が MouseInput(Released) より先に届く) によって false に
        // 化けることがあるため信用しない。 `last_alt` は continuation frame で更新され release frame
        // では `allow_update = false` で保持されるので OS event 順序に依存しない。 overlay の snap
        // 判定とも同一値で確定し、 「release で grid に飛ぶ」 不整合が起きない。
        if let Some(nd) = drag_release {
            let release_alt = nd.last_alt;
            // pointer.pos に頼らず `nd.last_mouse` を使う (winit release frame で pointer.pos が
            // press 位置に戻る既存問題、 arrangement と同パターン)。
            let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
            let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
            let raw = f64::from(dx) * beat_per_px;
            // 絶対位置 snap (overlay と一貫)。
            let beat_delta =
                compute_note_drag_beat_delta(&nd, raw, &view.snap, zoom_x_px_per_beat);
            #[allow(clippy::cast_possible_truncation)]
            let pitch_delta = (-(dy * pitch_per_px)).round() as i32;
            let min_len = if view.snap.is_active(release_alt) {
                view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
            } else {
                0.05
            };

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
                        let new_len = (a.len_beats + beat_delta).max(min_len);
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
                        let max_start = a.start_beat + a.len_beats - min_len;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual_delta = new_start - a.start_beat;
                        let new_len = (a.len_beats - actual_delta).max(min_len);
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
        // M14 Phase 59: editing_mode 中は Shift+drag rect select も短絡。
        if !editing_mode
            && (shift_press || shift_rect_active)
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

        // ===== M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 overlay (text_input + commit dispatch) =====
        // lyric_editing が Some なら、編集対象 note の rect 内に text_input を重ね描きし、
        // Enter / NumpadEnter で commit text を `split_into_morae` で分割 → 後続 note へ
        // 1 SetLyrics Edit (1 undo) で分配。Esc は text_input が focus clear → 次 frame で
        // resp.focused == false 検出 → lyric_editing = None (2 frame で UX 完了)。
        if let Some(edit_id) = lyric_editing {
            // borrow conflict 回避: 必要なデータを先にコピーしてから self.text_input を呼ぶ。
            let edit_data = notes.iter().find(|n| n.id == edit_id).map(|n| {
                let raw_rect = note_to_rect(n, view, grid);
                let prefill = n.lyric.as_deref().unwrap_or("").to_string();
                (raw_rect, prefill)
            });
            if let Some((raw_rect, prefill)) = edit_data {
                // grid 内に clip (note rect が grid 外にはみ出している場合)
                let clipped_x = raw_rect.x.max(grid.x);
                let clipped_y = raw_rect.y.max(grid.y);
                let clipped_w = (raw_rect.x + raw_rect.w).min(grid.x + grid.w) - clipped_x;
                let clipped_h = (raw_rect.y + raw_rect.h).min(grid.y + grid.h) - clipped_y;
                // M14 Phase 59: text_input overlay の最小表示サイズ (8 px)。 旧 `style.lyric_font_px`
                // を threshold にしていたが、 lyric_font_px が MAX cap になったため固定値に変更
                // (text_input は font_size 14 px 既定で 8 px 高あれば最低限読める)。
                if clipped_w < 8.0 || clipped_h < 8.0 {
                    // 表示できないほど小さい (zoom out 過多 etc) → 編集モード解除
                    self.widget_state::<PianoRollState>(wid).lyric_editing = None;
                    lyric_editing = None;
                } else {
                    let clipped = Rect {
                        x: clipped_x,
                        y: clipped_y,
                        w: clipped_w,
                        h: clipped_h,
                    };
                    // text_input_at_focused: id に edit_id を含めることで note 切替時に
                    // widget id が変化 → was_widget_visible_last_frame == false → 自動 focus +
                    // 全選択 (gained_focus 検知経由)。
                    let resp = self.text_input_at_focused(
                        ("piano_roll_lyric", edit_id),
                        clipped,
                        &prefill,
                        // on_change は per-keystroke で呼ばれるが、ここでは何もしない
                        // (commit 検出で 1 度だけ SetLyrics 発行 = 1 undo)。
                        |_new_text| Edit::mutate(|_: &mut M| {}),
                    );

                    if resp.committed {
                        let committed_text = resp.committed_text.unwrap_or_default();
                        let morae: Vec<String> = if committed_text.is_empty() {
                            // 空文字 commit → 起点 note の歌詞を None に (= 削除)
                            Vec::new()
                        } else {
                            split_into_morae(&committed_text)
                        };
                        // 起点 note の歌詞 update count: 空入力は 1 (起点を None に)、
                        // それ以外は morae.len() 個分の連続 note を取る。
                        let target_count = morae.len().max(1);
                        let target_ids =
                            collect_next_notes_for_lyric(notes, edit_id, target_count);
                        let mut updates: Vec<(NoteId, Option<String>)> =
                            Vec::with_capacity(target_ids.len());
                        for (i, nid) in target_ids.iter().enumerate() {
                            let lyric = morae.get(i).cloned().filter(|s| !s.is_empty());
                            updates.push((*nid, lyric));
                        }
                        // 余り (overflow) を Response に載せる (note 数 < 入力モーラ数の場合)。
                        response.lyric_overflow_morae =
                            morae.len().saturating_sub(target_ids.len());
                        if !updates.is_empty() {
                            self.push_edit(make_edit(NotesEditRequest::SetLyrics(updates)));
                        }
                        // 次 note へ移動 (= 分配し終わった先の note id、無ければ None)
                        let all_sorted =
                            collect_next_notes_for_lyric(notes, edit_id, usize::MAX);
                        let next_id = all_sorted.get(target_ids.len()).copied();
                        self.widget_state::<PianoRollState>(wid).lyric_editing = next_id;
                        lyric_editing = next_id;
                        // selection も自動追従 (daw_01 UI が同期、note 強調が次 note へ)
                        if let Some(nid) = next_id
                            && selected != [nid].as_slice()
                        {
                            let prev = selected.to_vec();
                            self.push_edit(make_edit(NotesEditRequest::Select {
                                prev,
                                next: vec![nid],
                            }));
                            response.selection_changed = true;
                        }
                    } else if !resp.focused {
                        // Esc 検出 (or 外 click による blur): text_input が
                        // clear_focus_if_focused → 次 frame で resp.focused = false。
                        self.widget_state::<PianoRollState>(wid).lyric_editing = None;
                        lyric_editing = None;
                    }
                }
            } else {
                // defensive: notes に edit_id が無い (フレーム頭の sync check で本来 None
                // にしているので通常起こらない)
                self.widget_state::<PianoRollState>(wid).lyric_editing = None;
                lyric_editing = None;
            }
        }
        response.lyric_editing = lyric_editing;

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

    // (c) 拍縦線 (1 拍ごと細線、bar 縦線) — M13 Phase 55 で library `Ui::bar_beat_grid` に統合。
    // この関数の caller (piano_roll cached layer) で `hctx.bar_beat_grid` を呼ぶ。

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
                    text: format!("C{octave}").into(),
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
    }
}

/// (M14 Phase 59) note 上に歌詞を描画する独立 pass。 selection overlay / drag preview の
/// **後** に呼んで lyric を最前面に置く (旧設計では cached 内 draw_notes 内で描いていたため
/// selection の黄色 fill に覆われていた、 daw_01 #017 動作確認で発覚)。
///
/// font_size は note rect の高さに連動 (`note_h * 0.7` を `lyric_font_px_max` で cap)。
/// 縦方向中央寄せで note 内に収める。 lyric_editing 中の note は text_input overlay が
/// 出ているので skip (= 編集中歌詞は text_input 内で表示される)。
fn draw_lyrics<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    lyric_color: Color,
    lyric_font_px_max: f32,
    skip_note_id: Option<NoteId>,
) {
    for note in visible {
        if Some(note.id) == skip_note_id {
            continue;
        }
        let Some(lyric) = note.lyric.as_ref() else {
            continue;
        };
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
        // note 高さに比例した font (cap = lyric_font_px_max)、 最低 7px 以上で描画。
        let font_size = (clipped.h * 0.75).clamp(7.0, lyric_font_px_max);
        if clipped.h < font_size + 1.0 || clipped.w < font_size {
            continue;
        }
        // 縦方向中央寄せ: top = clipped.y + (clipped.h - font_size) / 2
        let top = clipped.y + ((clipped.h - font_size) * 0.5).max(0.0);
        hctx.push_text(GlyphArea {
            text: lyric.clone(),
            left: clipped.x + 2.0,
            top,
            font_size,
            line_height: font_size * 1.1,
            color: lyric_color,
            clip_rect: Some(clipped),
        });
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
#[allow(clippy::too_many_arguments)]
fn draw_drag_preview<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    nd: &NoteDragSession,
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
    beat_delta: f64,
    pitch_delta: i32,
    min_len: f64,
) {
    for a in &nd.anchors {
        let (start_beat, len_beats, pitch) =
            drag_preview_geometry(*a, nd.kind, beat_delta, pitch_delta, min_len);
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
            ruler_h: 0.0,
            bpm: 120.0,
            time_sig: (4, 4),
            // 数値検証 test は raw beat 値を期待するので明示 OFF。
            snap: SnapConfig::OFF,
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

    // -------- Hit-test extension tests (rect 外側 ±resize_handle_px) --------
    // note (id 0) start=1, len=1 → rect x∈[150,250]、edge=4 で拡張範囲 x∈[146,254)。

    #[test]
    fn note_hit_returns_resize_left_at_outer_left_handle() {
        let (notes, view, grid) = make_test_setup();
        // x=147 = r.x(150) - 3 → 拡張範囲内、左端 handle
        let hit = note_hit(&notes, view, grid, 147.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeLeft)));
    }

    #[test]
    fn note_hit_returns_resize_right_at_outer_right_handle() {
        let (notes, view, grid) = make_test_setup();
        // x=252 = r.x+r.w(250) + 2 → 拡張範囲内、右端 handle
        let hit = note_hit(&notes, view, grid, 252.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeRight)));
    }

    #[test]
    fn note_hit_returns_none_just_past_outer_handle() {
        let (notes, view, grid) = make_test_setup();
        // x=145 = r.x(150) - 5 → 拡張範囲 [146, 254) の外
        let hit = note_hit(&notes, view, grid, 145.0, 102.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn note_hit_short_note_inside_returns_move() {
        // 短 note (len=0.05 → w=5px、< edge*2=8) の rect 内中央は Move 強制
        let notes = vec![note(0, 1.0, 0.05, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        // r.x = 150, r.w = 5。中央 x=152 で内側
        let hit = note_hit(&notes, view, grid, 152.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::Move)));
    }

    #[test]
    fn note_hit_short_note_outer_left_returns_resize_left() {
        // 短 note でも rect 外側左は ResizeLeft (内外あいまい性なし)
        let notes = vec![note(0, 1.0, 0.05, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        // r.x = 150。x=148 = r.x - 2 → 外側左
        let hit = note_hit(&notes, view, grid, 148.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeLeft)));
    }

    #[test]
    fn note_hit_adjacent_notes_back_wins_at_shared_handle() {
        // note A (id 0, start=1, len=1) → rect x∈[150,250]、右端拡張 [246,254)
        // note B (id 1, start=2, len=1) → rect x∈[250,350]、左端拡張 [246,254)
        // x=251 は両方の拡張ハンドル領域に入る → 後勝ちで B
        let notes = vec![note(0, 1.0, 1.0, 60), note(1, 2.0, 1.0, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        let hit = note_hit(&notes, view, grid, 251.0, 102.0, 4.0);
        assert_eq!(hit, Some((1, NoteDragKind::ResizeLeft)));
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_outer_left_handle() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 147.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_outer_right_handle() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 252.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
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

    // -------- M14 Phase 59 / daw_01 #017: split_into_morae unit tests --------

    #[test]
    fn split_into_morae_single_char() {
        assert_eq!(split_into_morae("あ"), vec!["あ"]);
    }

    #[test]
    fn split_into_morae_basic_distribution() {
        assert_eq!(split_into_morae("あいうえ"), vec!["あ", "い", "う", "え"]);
    }

    #[test]
    fn split_into_morae_combines_yo() {
        assert_eq!(split_into_morae("きゃ"), vec!["きゃ"]);
    }

    #[test]
    fn split_into_morae_combines_tsu() {
        // "ぱっと" = ぱ + っ (結合) + と → 2 モーラ
        assert_eq!(split_into_morae("ぱっと"), vec!["ぱっ", "と"]);
    }

    #[test]
    fn split_into_morae_consecutive_small_kana() {
        // "きゃっ" = き + ゃ (結合) + っ (続けて結合) → 1 モーラ
        assert_eq!(split_into_morae("きゃっ"), vec!["きゃっ"]);
    }

    #[test]
    fn split_into_morae_leading_small_kana_defensive() {
        // 先頭小書きは defensive で単独 1 モーラ (通常入力では発生しない)
        assert_eq!(split_into_morae("ぁい"), vec!["ぁ", "い"]);
    }

    #[test]
    fn split_into_morae_empty() {
        assert_eq!(split_into_morae(""), Vec::<String>::new());
    }

    #[test]
    fn split_into_morae_long_kana() {
        // "しゅんかんいどう" = しゅ / ん / か / ん / い / ど / う → 7 モーラ
        assert_eq!(
            split_into_morae("しゅんかんいどう"),
            vec!["しゅ", "ん", "か", "ん", "い", "ど", "う"]
        );
    }

    #[test]
    fn split_into_morae_ascii_one_per_char() {
        assert_eq!(split_into_morae("abc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_into_morae_katakana_yo() {
        // カタカナの拗音も同様に結合
        assert_eq!(split_into_morae("シュン"), vec!["シュ", "ン"]);
    }

    // -------- M14 Phase 59 / daw_01 #017: collect_next_notes_for_lyric unit tests --------

    #[test]
    fn collect_next_notes_returns_self_first() {
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60), note(30, 2.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 2);
        assert_eq!(result, vec![10, 20]);
    }

    #[test]
    fn collect_next_notes_sorted_by_start_beat() {
        // notes が start_beat ソート前提だが、関数内で sort を保証
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60), note(30, 2.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 3);
        assert_eq!(result, vec![10, 20, 30]);
    }

    #[test]
    fn collect_next_notes_same_beat_pitch_desc() {
        // 同 start_beat なら高 pitch が先 (歌詞編集モードで「高音先取り」が歌唱メロディと整合)
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 0.0, 1.0, 72), note(30, 1.0, 1.0, 60)];
        // 起点 = id 20 (pitch 72、start_beat 0.0)
        // sorted: 20 (0.0, 72) → 10 (0.0, 60) → 30 (1.0, 60)
        let result = collect_next_notes_for_lyric(&notes, 20, 3);
        assert_eq!(result, vec![20, 10, 30]);
    }

    #[test]
    fn collect_next_notes_truncates_when_count_exceeds() {
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 5);
        assert_eq!(result, vec![10, 20], "残数が count より少ないとき truncate");
    }

    #[test]
    fn collect_next_notes_empty_when_id_not_found() {
        let notes = vec![note(10, 0.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 999, 3);
        assert_eq!(result, Vec::<NoteId>::new());
    }

    #[test]
    fn collect_next_notes_zero_count() {
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 0);
        assert_eq!(result, Vec::<NoteId>::new());
    }

    // -------- Widget integration tests --------

    /// 簡易テスト Model (no-Clone 不変条件: Clone/Default/Hash 不要)。
    struct TestModel {
        notes: Vec<Note>,
        selected: Vec<NoteId>,
        last_request: Option<RequestKind>,
        last_select_prev: Option<Vec<NoteId>>,
        last_select_next: Option<Vec<NoteId>>,
        /// (M14 Phase 59) 最後に発行された `SetLyrics` の内容。
        last_set_lyrics: Option<Vec<(NoteId, Option<String>)>>,
        /// (M14 Phase 61d / daw_01 #012) 最後に Add request で渡された note の pitch。
        last_added_pitch: Option<u8>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RequestKind {
        Add,
        Delete,
        Move,
        Resize,
        Select,
        SetLyrics,
    }

    impl TestModel {
        fn new(notes: Vec<Note>) -> Self {
            Self {
                notes,
                selected: Vec::new(),
                last_request: None,
                last_select_prev: None,
                last_select_next: None,
                last_set_lyrics: None,
                last_added_pitch: None,
            }
        }
    }

    fn make_dispatch(
    ) -> impl Fn(NotesEditRequest) -> Edit<TestModel> + Send + Sync + 'static + Clone {
        |req: NotesEditRequest| -> Edit<TestModel> {
            match req {
                NotesEditRequest::Add(notes) => {
                    let pitch = notes.first().map(|n| n.pitch);
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::Add);
                        m.last_added_pitch = pitch;
                    })
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
                NotesEditRequest::SetLyrics(updates) => {
                    let updates_clone = updates.clone();
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::SetLyrics);
                        // 実際の Model 反映 (歌詞を Note.lyric に書き戻す)
                        for (id, lyric) in &updates_clone {
                            if let Some(n) = m.notes.iter_mut().find(|n| n.id == *id) {
                                n.lyric = lyric.as_deref().map(Arc::from);
                            }
                        }
                        m.last_set_lyrics = Some(updates_clone.clone());
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

    /// (M14 Phase 61d / daw_01 #012) Insert shortcut で視覚行の **下半分** にカーソルがあっても
    /// その行の pitch (= ceil) で Add される。 旧 `pitch_f.round()` は半行ぶん上にずれて 1 pitch
    /// 下のノートが追加される bug があった。 test_view: pitch_top=72, pitch_visible=24, grid h=400
    /// → pitch_to_px = 16.667。 cy=215 は pitch 60 の行 (y ∈ [200, 216.667)) の下半分、
    /// pitch_f = 72 - 215/16.667 = 59.1 → ceil = 60 (正)、 round = 59 (旧 bug)。
    #[test]
    fn piano_roll_insert_shortcut_uses_ceil_for_pitch() {
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
                pos: Some((100.0, 215.0)), // pitch 60 の visual 行の下半分
                ..PointerFrame::default()
            },
            keyboard: vec![key],
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input, |ui| {
            let sel: Vec<NoteId> = vec![];
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
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
        assert_eq!(
            model.last_added_pitch,
            Some(60),
            "cy=215 (pitch 60 行の下半分) は ceil で pitch=60、 round だと 59 にずれる (#012)"
        );
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

    // -------- M13 Phase 55: ruler / time_sig 対応 grid の確認 --------

    /// 1 frame 描画して `Scene` を返す helper。
    fn render_piano_roll_once(view: PianoRollView, notes: &[Note]) -> daw_ui_renderer::Scene {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let style = PianoRollStyle::default();
            let _ = ui.piano_roll(
                "pr_test",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                notes,
                view,
                &[],
                &style,
                |_| Edit::mutate(|()| {}),
            );
        });
        scene
    }

    /// `ruler_h: 0.0` で bar 番号 label が出ない (旧 piano_roll 互換)。
    #[test]
    fn ruler_h_zero_disables_bar_labels() {
        let mut view = test_view();
        view.ruler_h = 0.0;
        view.bpm = 120.0;
        view.time_sig = (4, 4);
        let scene = render_piano_roll_once(view, &[]);
        let labels: Vec<String> =
            scene.iter_glyphs().map(|g| g.text.as_ref().to_string()).collect();
        for blocked in ["1", "2", "3"] {
            assert!(
                !labels.iter().any(|s| s == blocked),
                "ruler_h=0.0 で bar label {blocked:?} は出ない: labels={labels:?}",
            );
        }
    }

    /// `ruler_h: 20.0` で bar label "1", "2" が出る (small bars only)。
    #[test]
    fn ruler_h_positive_emits_bar_labels() {
        let mut view = test_view();
        view.start_beat = 0.0;
        view.len_beats = 8.0; // 2 小節分
        view.ruler_h = 20.0;
        view.bpm = 120.0;
        view.time_sig = (4, 4);
        let scene = render_piano_roll_once(view, &[]);
        let labels: Vec<String> =
            scene.iter_glyphs().map(|g| g.text.as_ref().to_string()).collect();
        for expected in ["1", "2"] {
            assert!(
                labels.iter().any(|s| s == expected),
                "ruler_h=20.0 で bar label {expected:?} が出る: labels={expected:?}, found={labels:?}",
            );
        }
    }

    /// `ruler_h: 20.0` のとき grid 内 bar 線の y が `rect.y + 20.0` 以降から始まる
    /// (= grid 領域が ruler 分シフトしている)。
    /// ruler 内の tick 線 (短い) は除外し、grid 全幅の bar 線 (長い) のみ判定する。
    #[test]
    fn grid_y_offset_with_ruler() {
        let style = PianoRollStyle::default();
        let mut view = test_view();
        view.start_beat = 0.0;
        view.len_beats = 8.0;
        view.ruler_h = 20.0;
        view.bpm = 120.0;
        view.time_sig = (4, 4);
        let scene = render_piano_roll_once(view, &[]);

        // bar_color で seg が long (高さ > ruler_h) のものを grid 内 bar 線として抽出。
        let grid_bar_y_tops: Vec<f32> = scene
            .iter_lines()
            .flat_map(|b| b.segments.iter().copied())
            .filter(|seg| seg.color == style.bar_line)
            .filter(|seg| (seg.b[1] - seg.a[1]).abs() > 20.0)
            .map(|seg| seg.a[1].min(seg.b[1]))
            .collect();
        assert!(
            !grid_bar_y_tops.is_empty(),
            "grid 内 bar 線が出ている (sanity)",
        );
        for y in &grid_bar_y_tops {
            assert!(
                *y >= 20.0 - 0.1,
                "ruler_h=20 で grid bar 線の y_top は >=20.0: actual={y}",
            );
        }
    }

    // -------- Stale cursor reset tests --------
    // piano_roll widget が pointer 位置に応じて cursor を明示的に reset するか検証。
    // winit は state-full なので、note 外で set_cursor を呼ばないと前フレームの形状が残る
    // (ui.rs:999 で明記)。修正後は pointer が widget rect 内かつ note hover 圏外で
    // CursorIcon::Default を明示 set する。

    fn run_frame_with_cursor_capture<F: FnOnce(&mut Ui<'_, TestModel>)>(
        captured: Arc<std::sync::Mutex<Vec<CursorIcon>>>,
        model: &mut TestModel,
        input: FrameInput,
        f: F,
    ) {
        let captured_clone = Arc::clone(&captured);
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.set_cursor_request = Some(Box::new(move |c| {
            captured_clone.lock().unwrap().push(c);
        }));
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame(model, &mut scene, screen, input, |_, ui| {
            f(ui);
        });
    }

    #[test]
    fn piano_roll_resets_cursor_to_default_when_pointer_in_widget_but_not_on_note() {
        let captured: Arc<std::sync::Mutex<Vec<CursorIcon>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        // grid 内だが note 外 (note rect は x∈[200,400] y≈[200,215]、(50,50) は note 外)
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)),
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        run_frame_with_cursor_capture(Arc::clone(&captured), &mut model, input, |ui| {
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
        assert_eq!(captured.lock().unwrap().as_slice(), &[CursorIcon::Default]);
    }

    #[test]
    fn piano_roll_does_not_set_cursor_when_pointer_outside_widget() {
        let captured: Arc<std::sync::Mutex<Vec<CursorIcon>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        // pointer が widget rect (0,0)-(800,400) の外 → 何もしない (他 widget の責務)
        // screen は (1000,600) で widget 外も合法
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((900.0, 500.0)),
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        let captured_clone = Arc::clone(&captured);
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.set_cursor_request = Some(Box::new(move |c| {
            captured_clone.lock().unwrap().push(c);
        }));
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 1000, height: 600 };
        host.frame(&mut model, &mut scene, screen, input, |_, ui| {
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
        assert!(
            captured.lock().unwrap().is_empty(),
            "widget 外では set_cursor を呼ばない: actual={:?}",
            *captured.lock().unwrap()
        );
    }

    // ============================================================
    // M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 widget integration tests
    // ============================================================

    fn key_l() -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Char('L'),
        }
    }

    /// 文字 `c` (ASCII alphabet) と挿入 text を持つ KeyEvent を作る。
    /// `text` は IME 経由でも user 直接 type でも text_input.rs が `ev.text` を見るので
    /// 多バイト文字 (例: "あ") も渡せる。
    fn key_typing(c: char, text: &str) -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: Some(text.to_string()),
            physical_key: PhysicalKey::Char(c.to_ascii_uppercase()),
        }
    }

    fn key_enter() -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Enter,
        }
    }

    fn key_esc() -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape,
        }
    }

    fn setup_lyric_test_host() -> UiHost<TestModel> {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L");
        host
    }

    /// 共通の piano_roll 呼び出し (rect 800x400 / kbd_w=0)。
    fn run_lyric_frame(
        host: &mut UiHost<TestModel>,
        model: &mut TestModel,
        input: FrameInput,
        view: PianoRollView,
        style: &PianoRollStyle,
        on_resp: impl FnOnce(&PianoRollResponse),
    ) {
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        let resp_cell: std::cell::RefCell<Option<PianoRollResponse>> =
            std::cell::RefCell::new(None);
        run_frame(host, model, input, |ui| {
            let dispatch = make_dispatch();
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                style,
                dispatch,
            );
            *resp_cell.borrow_mut() = Some(resp);
        });
        on_resp(resp_cell.borrow().as_ref().expect("resp captured"));
    }

    /// L キー + selected.len() == 1 → lyric_editing = Some(selected[0])、Edit 発行なし。
    #[test]
    fn lyric_edit_l_key_enters_mode_when_single_selected() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = None;
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, Some(0));
        assert_eq!(model.last_request, None, "L で mode 入っただけ、Edit 発行なし");
    }

    #[test]
    fn lyric_edit_l_key_noop_when_zero_selected() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![]; // 選択なし
        let view = test_view();
        let style = PianoRollStyle::default();

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, None);
    }

    #[test]
    fn lyric_edit_l_key_noop_when_multi_selected() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(0, 1.0, 1.0, 60),
            note(1, 2.0, 1.0, 60),
        ]);
        model.selected = vec![0, 1]; // 複数選択
        let view = test_view();
        let style = PianoRollStyle::default();

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, None);
    }

    #[test]
    fn lyric_edit_disabled_via_style_none() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle {
            lyric_edit_shortcut: None, // 無効化
            ..PianoRollStyle::default()
        };

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, None, "style.lyric_edit_shortcut=None なら L で起動しない");
    }

    /// 1 note のみ → "a" + Enter → SetLyrics 発行 + lyric_editing = None。
    /// Frame 1: L → enter mode
    /// Frame 2: type "a" + Enter → commit + clear (no next note)
    #[test]
    fn lyric_edit_enter_commits_single_note_and_clears() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: 'a' + Enter
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(model.last_request, Some(RequestKind::SetLyrics));
        assert_eq!(model.last_set_lyrics, Some(vec![(0u32, Some("a".to_string()))]));
        assert_eq!(got_lyric_editing, None, "次 note 無し → mode 解除");
    }

    /// 4 notes 同 pitch → "abcd" + Enter → 各 note に 1 char ずつ分配 (start_beat 順)。
    #[test]
    fn lyric_edit_enter_distributes_morae_to_next_notes() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
            note(30, 1.0, 0.5, 60),
            note(40, 1.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: "abcd" + Enter (4 chars + commit)
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![
                    key_typing('a', "a"),
                    key_typing('b', "b"),
                    key_typing('c', "c"),
                    key_typing('d', "d"),
                    key_enter(),
                ],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![
                (10u32, Some("a".to_string())),
                (20u32, Some("b".to_string())),
                (30u32, Some("c".to_string())),
                (40u32, Some("d".to_string())),
            ])
        );
    }

    /// 4 notes、入力 2 mora → 2 note に分配 + lyric_editing = Some(notes[2].id) (= 次へ移動)。
    #[test]
    fn lyric_edit_enter_advances_to_next_when_more_notes_remain() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
            note(30, 1.0, 0.5, 60),
            note(40, 1.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        let mut got_lyric_editing = None;
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_typing('b', "b"), key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(10u32, Some("a".to_string())), (20u32, Some("b".to_string()))])
        );
        assert_eq!(got_lyric_editing, Some(30), "次 note (id 30) へ移動");
        // selection も追従していること (同じ frame で Select 発行された)
        assert_eq!(model.selected, vec![30u32]);
    }

    /// 拗音結合: "しゅんかん" → 4 mora ([しゅ] [ん] [か] [ん])。
    #[test]
    fn lyric_edit_enter_combines_kana_correctly() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
            note(30, 1.0, 0.5, 60),
            note(40, 1.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // "しゅんかん" を IME 経由ではなく KeyEvent.text 直接挿入で simulate
        // (実際の IME 入力は manual test: cargo run --bin piano_roll で確認)
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![
                    key_typing('a', "し"),
                    key_typing('b', "ゅ"),
                    key_typing('c', "ん"),
                    key_typing('d', "か"),
                    key_typing('e', "ん"),
                    key_enter(),
                ],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![
                (10u32, Some("しゅ".to_string())),
                (20u32, Some("ん".to_string())),
                (30u32, Some("か".to_string())),
                (40u32, Some("ん".to_string())),
            ])
        );
    }

    /// 2 notes に 3 mora 入力 → SetLyrics 2 件 + lyric_overflow_morae == 1。
    #[test]
    fn lyric_edit_overflow_morae_count_in_response() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        let mut overflow = 0;
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![
                    key_typing('a', "a"),
                    key_typing('b', "b"),
                    key_typing('c', "c"),
                    key_enter(),
                ],
                ..Default::default()
            },
            view,
            &style,
            |resp| overflow = resp.lyric_overflow_morae,
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(10u32, Some("a".to_string())), (20u32, Some("b".to_string()))])
        );
        assert_eq!(overflow, 1, "余りモーラ 1 個分 (= 'c') が捨てられた");
    }

    /// Frame 1: L → mode 入る。Frame 2: "a" + Esc → SetLyrics 発行されず + lyric_editing = None。
    #[test]
    fn lyric_edit_esc_cancels_without_setlyrics() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        let mut got_lyric_editing = Some(0);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_esc()],
                ..Default::default()
            },
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(model.last_request, None, "Esc で cancel → SetLyrics 発行なし");
        assert_eq!(got_lyric_editing, None, "Esc 1 frame で完全 cancel");
    }

    /// 編集中 (frame 2) の primary press on note → drag が始まらない。
    #[test]
    fn lyric_edit_short_circuits_drag() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L → enter mode
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: primary press on note (lyric_editing = Some(0))
        let mut dragging = Some(NoteDragKind::Move);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((300.0, 200.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            view,
            &style,
            |resp| dragging = resp.dragging,
        );
        assert_eq!(dragging, None, "編集中は drag 開始しない");
    }

    /// 既存 lyric "x" を持つ note → 全選択 (auto on focus) + Backspace 相当の入力なしで Enter
    /// → 「変更なしで Enter」となる。 ※ 「全選択を replace せずに Enter」と
    /// 「全選択 → Backspace → Enter」は別。後者は次 test。
    /// 当 test は「prefill が text_input に入っている → 何もしないで Enter → committed_text =
    /// 既存の "x" → SetLyrics(0, Some("x"))」 (no-op だが Edit は発行される) を確認。
    #[test]
    fn lyric_edit_prefill_then_enter_commits_existing_lyric() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![Note {
            id: 0,
            start_beat: 1.0,
            len_beats: 1.0,
            pitch: 60,
            velocity: 96,
            lyric: Some(Arc::from("x")),
        }]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_enter()], ..Default::default() },
            view,
            &style,
            |_| {},
        );
        // prefill "x" がそのまま commit text として渡る → SetLyrics(0, Some("x"))
        assert_eq!(model.last_set_lyrics, Some(vec![(0u32, Some("x".to_string()))]));
    }

    /// `"x"` lyric を持つ note → 全選択 → typing で空 (※ 全選択中の typing は replace)
    /// → Enter → SetLyrics(0, None) (空文字列正規化)。
    /// 簡易化: 直接 select_all (Ctrl+A) → Backspace → Enter の代わりに
    /// "全選択中に何かを type して Enter" だと replace になる。 ここでは
    /// **既存 lyric を Backspace で削除 → Enter** で空文字列 commit を再現する。
    #[test]
    fn lyric_edit_empty_string_normalized_to_none() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![Note {
            id: 0,
            start_beat: 1.0,
            len_beats: 1.0,
            pitch: 60,
            velocity: 96,
            lyric: Some(Arc::from("x")),
        }]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L → enter mode (prefill = "x"、全選択済)
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: Backspace で全選択 "x" を削除 (text_input は空に) → Enter
        let key_backspace = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Backspace,
        };
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_backspace, key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(0u32, None)]),
            "空文字 commit は None に正規化"
        );
    }

    /// lyric_editing = Some(id) 状態で notes から id を消す → 次 frame で
    /// frame 頭 sync check が lyric_editing = None に reset。
    #[test]
    fn lyric_edit_auto_clears_when_target_note_deleted() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L → enter mode
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // notes から id 0 を消す (= 外部要因で note 削除)
        model.notes.clear();

        // Frame 2: lyric_editing が Some(0) のまま render → 頭 sync で None に reset
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput::default(),
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(got_lyric_editing, None, "編集対象 note 消失で auto-clear");
    }

    /// 同 start_beat、pitch 60/72 の 2 note → pitch 72 起点で "ab" + Enter →
    /// SetLyrics(72→"a", 60→"b") (高 pitch 先取り順序)。
    #[test]
    fn lyric_edit_same_beat_pitch_desc_order() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(60, 0.0, 0.5, 60),  // pitch 60、id 60 (id=pitch でわかりやすく)
            note(72, 0.0, 0.5, 72),  // pitch 72、id 72 (高 pitch)
        ]);
        model.selected = vec![72]; // 高 pitch を起点に
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_typing('b', "b"), key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(72u32, Some("a".to_string())), (60u32, Some("b".to_string()))]),
            "同 beat なら高 pitch (72) が先、低 pitch (60) が次"
        );
    }

    // M14 Phase 61b (#011): fold_piano_roll_note_hash の cache invalidation 性質を verify。
    // (1) 同一データ stable、 (2-5) start_beat / len_beats / pitch / velocity 各変化で hash 変
    // 化、 (6) lyric Arc identity の振る舞い (同 Arc clone は同 hash、 別 Arc::from は別 hash)。

    fn note_with_velocity(id: NoteId, start: f64, len: f64, pitch: u8, vel: u8) -> Note {
        Note { id, start_beat: start, len_beats: len, pitch, velocity: vel, lyric: None }
    }

    #[test]
    fn fold_piano_roll_note_hash_stable() {
        let notes = vec![note(0, 0.0, 1.0, 60), note(1, 1.0, 0.5, 64)];
        assert_eq!(
            fold_piano_roll_note_hash(&notes),
            fold_piano_roll_note_hash(&notes),
            "同じ notes の fold は冪等"
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_move() {
        let before = vec![note(0, 0.0, 1.0, 60)];
        let after = vec![note(0, 0.5, 1.0, 60)]; // start 0 → 0.5
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_resize() {
        let before = vec![note(0, 0.0, 1.0, 60)];
        let after = vec![note(0, 0.0, 2.0, 60)]; // len 1 → 2
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_pitch() {
        let before = vec![note(0, 0.0, 1.0, 60)];
        let after = vec![note(0, 0.0, 1.0, 64)]; // pitch 60 → 64
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_velocity() {
        let before = vec![note_with_velocity(0, 0.0, 1.0, 60, 96)];
        let after = vec![note_with_velocity(0, 0.0, 1.0, 60, 127)]; // vel 96 → 127
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_lyric_arc_identity() {
        let shared: Arc<str> = Arc::from("a");
        let n1 = Note {
            id: 0,
            start_beat: 0.0,
            len_beats: 1.0,
            pitch: 60,
            velocity: 96,
            lyric: Some(Arc::clone(&shared)),
        };
        let n1_dup = Note { lyric: Some(Arc::clone(&shared)), ..n1.clone() };
        // 同 Arc::clone → 同 pointer → 同 hash
        assert_eq!(
            fold_piano_roll_note_hash(std::slice::from_ref(&n1)),
            fold_piano_roll_note_hash(std::slice::from_ref(&n1_dup)),
        );
        // 別 Arc::from(同 string) → 別 pointer → 別 hash (daw_01 SetLyrics は新規 Arc::from で
        // この振る舞いに依存して cache invalidate する)。
        let n2 = Note { lyric: Some(Arc::<str>::from("a")), ..n1.clone() };
        assert_ne!(
            fold_piano_roll_note_hash(std::slice::from_ref(&n1)),
            fold_piano_roll_note_hash(std::slice::from_ref(&n2)),
        );
    }
}
