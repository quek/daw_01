//! `reorderable_list` ウィジェット — `scroll_area` 上に drag&drop reorder を内蔵した list (M11 Phase 51)。
//!
//! daw_01 conversation `#012 [Replied]` で確定した API。`list_view` と完全平行な API
//! (scroll_area + row callback) に drag-reorder セマンティクスを足したもの。
//!
//! 設計:
//! - `make_edit: Fn(ReorderableListEditRequest) -> Edit<M> + Clone + Send + Sync + 'static`
//!   (M10 Phase 49 の `Ui::arrangement` と同じ trait bound、Undoable Edit の forward/inverse 2
//!   closure に分配するため `Clone` 必要)
//! - `Reorder(Vec<usize>)`: release frame で 1 度だけ発行。`order` は新順での元 index 列で
//!   `new_items[i] = items[order[i]]`、または `apply_reorder<T: Clone>(items, anchor, target)`
//!   helper でも apply 可能。stable id ベースが必要なら caller 側で `key_of(items[order[i]])`
//!   等にマッピングする。
//! - `drag_handle_w`: `0.0` で row 全体 drag (Bitwig 風)、`> 0.0` で row 左端 N px だけ
//!   drag 起点 (Logic / Cubase 風グリップ)。残り領域は row callback の button_at 等で消費可能。
//! - **commit-by-release**: drag 中は library が overlay 描画 (元 row 半透明 + drop indicator)、
//!   release frame で初めて `Reorder` を発行する。dy < 16px は短 click → `clicked` に格下げ。
//! - **release frame optimistic preview**: arrangement Phase 50 と同パターン。release frame で
//!   先に新順序を計算 → state に保存 → 同フレーム + 次フレームの 1 度ずつ新順序で描画して
//!   Edit 適用 1 frame 遅延の visual 揺れを抑える。
//! - reorder logic は arrangement の `compute_reorder_target_index` / `apply_reorder` を再利用。
//!
//! ## 行内アコーディオン展開 (daw_01)
//!
//! [`Ui::reorderable_list_expandable`] は各 row の **直下に可変高の展開領域** を持てる。
//! `row_extra_h(i)` が row `i` の展開高 (`0.0` = 折りたたみ) を返し、`expansion(ui, i, rect)` が
//! その領域を描く。これで「チェーン行の Par を押すと、その行の真下に params が開いて以降の行が
//! 下にずれる」 アコーディオン UI を、 drag 並べ替えを保ったまま実現する。
//!
//! **drag 中は全展開を畳む** (uniform 行高に戻す) ことで、 既存の uniform な hit-test /
//! `compute_reorder_target_index` / drop indicator ロジックを丸ごと再利用する (= 可変高 hit-test
//! の作り直しを避けつつ、 reorder の正しさを構造的に保証)。展開は drag していないフレームだけ描く。
//! press → session 開始の anchor 判定は「前フレームに表示されていた (= 可変高の) layout」 で行い、
//! 次フレームから畳む (1 frame 遅延、 視覚的に滑らか)。

use std::cell::{Cell, RefCell};
use std::hash::Hash;

use daw_ui_platform::Modifiers;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::theme::Palette;
use crate::ui::Ui;

/// anchor を抜き取って target に挿入した新順 `Vec<T>` を返す。`anchor_index >= items.len()`
/// または `target_index > items.len()-1` (after remove) でも安全に clamp。
///
/// `<T: Clone>` で generic。`u32` 用途 (track_id 列) でも `usize` 用途 (`reorderable_list` の
/// 元 index 列) でも単相化で動く。arrangement / reorderable_list 双方が使う純 Model 非依存ロジック
/// (旧 arrangement.rs 所有だったが S4b で arrangement が daw_gui に移設されたため、共有元として
/// 汎用 reorder widget 側に移動)。
#[must_use]
pub fn apply_reorder<T: Clone>(items: &[T], anchor_index: usize, target_index: usize) -> Vec<T> {
    if items.is_empty() || anchor_index >= items.len() {
        return items.to_vec();
    }
    let mut v: Vec<T> = items.to_vec();
    let it = v.remove(anchor_index);
    let insert_at = target_index.min(v.len());
    v.insert(insert_at, it);
    v
}

/// 縦リストの reorder drop 先 index を `mouse_y` から計算する。row 中央線 (0.5) で前後判定し、
/// anchor 抜き取り semantics (`Vec::remove(anchor)` → `Vec::insert(target-1)`) に合わせて 1 詰める。
/// `apply_reorder` と対で使う (arrangement / reorderable_list 共有)。
#[must_use]
pub fn compute_reorder_target_index(
    anchor_index: usize,
    mouse_y: f32,
    header_top: f32,
    track_top: f32,
    row_h: f32,
    n_tracks: usize,
) -> usize {
    if n_tracks == 0 || row_h <= 0.0 {
        return 0;
    }
    let local = mouse_y - header_top + track_top;
    if local <= 0.0 {
        return 0;
    }
    // local / row_h を「row 内 fractional 位置」付きで取り、中央 (0.5) より上下で挿入位置を判定。
    let raw = local / row_h;
    let idx = raw as usize;
    let frac = raw - raw.floor();
    // 中央線より下 → 次の row の前に挿入
    let target_unbounded = if frac >= 0.5 { idx + 1 } else { idx };
    let target_u = target_unbounded.min(n_tracks);
    // anchor 抜き取り後の semantics: anchor 自身またはその直後 (= anchor_index, anchor_index+1) は no-op。
    if target_u == anchor_index || target_u == anchor_index + 1 {
        return anchor_index;
    }
    // anchor より後の挿入は 1 詰めて semantics を合わせる (Vec::remove(anchor) → Vec::insert(target-1))。
    if target_u > anchor_index + 1 {
        target_u - 1
    } else {
        target_u
    }
}

/// `scroll_area` 内部の scrollbar 幅 (`scroll_area::SCROLLBAR_W` のミラー、row 幅から差し引くため)。
const SCROLLBAR_W: f32 = 10.0;

/// drag commit 判定の最小移動量 (px)。これ未満では click 扱いに格下げ。
/// arrangement の `TrackReorderSession` (Phase 46) と同値。
const DRAG_THRESHOLD_PX: f32 = 16.0;

#[derive(Clone, Copy, Debug)]
pub struct ReorderableListStyle {
    pub row_height: f32,
    pub row_gap: f32,
    pub row_bg: Color,
    pub row_bg_hover: Color,
    pub row_bg_selected: Color,
    /// drag 中の anchor row 背景 (半透明風)。
    pub row_bg_dragging: Color,
    /// drop 位置の横 line 色。
    pub drop_indicator_color: Color,
    pub drop_indicator_h: f32,
    pub radius: f32,
    /// `0.0` で row 全体 drag、`> 0.0` で row 左端 N px だけ drag 起点
    /// (残り領域は row callback の button_at 等が click を消費可能)。
    pub drag_handle_w: f32,
}

impl ReorderableListStyle {
    /// パレットから既定の reorder list スタイルを組む。row 面は [`crate::widgets::list_view`]
    /// と同じ (`panel_raised` / `control_hover` / `accent`)、drag 中の anchor row はその accent を
    /// 半透明にしたもの、drop 標的の線は `loop_band` (= 帯 / ドロップ標的の共通色)。
    ///
    /// `Default` は持たない (r.md #48): テーマ色を読む `Default::default()` は隠れた
    /// グローバル依存になり、ライトテーマに追従しないため。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            row_height: 26.0,
            row_gap: 2.0,
            row_bg: p.panel_raised,
            row_bg_hover: p.control_hover,
            row_bg_selected: p.accent,
            row_bg_dragging: p.accent.with_alpha(0.85),
            drop_indicator_color: p.loop_band,
            drop_indicator_h: 2.0,
            radius: 2.0,
            drag_handle_w: 0.0,
        }
    }
}

/// r.md #71 (プラグインのコピー / 移動): 掴んだ行をリスト矩形の **横へ** 出したと
/// 判定する余白 (px)。
///
/// **縦のはみ出しでは運び出さない**: reorder は `compute_reorder_target_index` が
/// y だけを見る **1 次元の gesture** で、リストより上/下にポインタがあることは
/// 「先頭へ / 末尾へ動かす」の途中経過として意味を持つ。 一方 x はリスト内で
/// 何の意味も持たないので、横に出たことが「別の場所へ運ぶ」の曖昧さのない合図になる。
///
/// 余白ゼロだと事故る: inspector の chain は幅 260px 前後 (`area.w - pad*2`) しか
/// 無く、普通に並べ替えているだけで数 px 横に揺れる。 その瞬間に reorder が失われて
/// トラック跨ぎの運搬に化けたら、並べ替えが「たまに効かない」機能になる。
const CARRY_OUT_MARGIN_PX: f32 = 24.0;

#[derive(Clone, Debug, Default)]
pub struct ReorderableListResponse {
    /// このフレームで click された row index (drag 距離 < 16px の release で trigger)。
    pub clicked: Option<usize>,
    /// `clicked` を起こした **press フレーム**の修飾キー。 選択遷移
    /// (Ctrl / Shift) は必ずこれで決める。 release フレームの生読みは
    /// `ModifiersChanged` 先行 race で修飾が落ちて見え、Ctrl+click が
    /// Single に化ける (arrangement が `press_modifiers` で同じ罠を回避している)。
    pub clicked_modifiers: Modifiers,
    /// hover 中の row index (任意フレーム)。
    pub hovered: Option<usize>,
    /// drag 中の anchor row index。drag 開始フレーム以降、release まで保持。
    pub dragging: Option<usize>,
    /// 掴んだ行がリスト矩形の **横へ出た最初のフレーム**だけ `Some(index)`。
    /// widget 内部の reorder session はこの時点で破棄され、以後 `Reorder` は
    /// 発行されない。caller はここで [`Ui::begin_drag`] して運搬を引き継ぐ。
    pub dragged_out: Option<usize>,
    /// `accept_drag_kind` と一致する外部 drag が **`rect` の上にある**ときの挿入位置
    /// (`0..=items.len()`)。 リストの外にポインタがあるフレームは `None`
    /// (= indicator を出さない / drop も受けない)。drop indicator は widget が描く。
    pub external_insert_at: Option<usize>,
    /// 上の位置で **このフレームに release された** (= drop 確定)。caller は
    /// [`Ui::take_drag_payload`] して commit する。
    pub external_dropped_at: Option<usize>,
    /// このフレームに描いた行の `(index, 画面座標 rect)`。 caller が
    /// `context_menu_for` / overlay を重ねるため (arrangement の
    /// `track_header_rects` / `clip_rects` と同じ contract)。 可視行のみ。
    pub row_rects: Vec<(usize, Rect)>,
}

/// release frame で 1 度発行される reorder Edit リクエスト。
#[derive(Debug)]
pub enum ReorderableListEditRequest {
    /// `order` は新順での元 index 列。`new_items[i] = items[order[i]]` で並び替え可能、
    /// または `daw_ui_core::widgets::reorderable_list::apply_reorder(items, anchor, target)` で
    /// 直接 `Vec<T>` を得られる (内部で apply_reorder を使ってこの値を計算している)。
    Reorder(Vec<usize>),
}

#[derive(Clone, Copy, Debug)]
struct ReorderSession {
    anchor_index: usize,
    anchor_mouse_y: f32,
    /// drag 中の最終 mouse y (release frame の `pointer.pos` が press 位置のままになる
    /// winit ケースに備えて、widget state 側で確実に保持)。
    last_mouse_y: f32,
    /// press した **そのフレーム**の修飾キー。 release フレームの生読みは
    /// `ModifiersChanged` 先行 race で落ちるので、 選択遷移はこの値で決める
    /// (`ArrangementState.press_modifiers` と同じ理由)。
    press_modifiers: Modifiers,
}

#[derive(Debug, Default)]
pub(crate) struct ReorderableListState {
    session: Option<ReorderSession>,
    /// release frame で計算した「次フレーム描画用」の新順 index 列。
    /// Some の間は同フレーム + 次フレーム描画でこの順序を反映する (1 frame の visual 遅延を
    /// 解消)。次フレームに enter したら自動的に消費 (= None)。
    pending_order: Option<Vec<usize>>,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// `scroll_area` 上の drag&drop reorder list。各 row は `row` callback で描画する。
    ///
    /// drag による並び替えは release frame で `make_edit(ReorderableListEditRequest::Reorder(order))` を
    /// 1 度だけ発行する (commit-by-release)。dy < 16px の release は click 扱い (`Response.clicked`)。
    ///
    /// `selected: &[usize]` は描画用ハイライトのみ (本 widget は selection を管理しない、
    /// caller が `clicked` / `clicked_modifiers` を見て更新する)。
    ///
    /// `accept_drag_kind` は **外部 drag** ([`Ui::begin_drag`]) を受け入れる札。
    /// `None` = 受け付けない。
    #[allow(clippy::too_many_arguments)]
    pub fn reorderable_list<T, F, R>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        items: &[T],
        selected: &[usize],
        accept_drag_kind: Option<&'static str>,
        style: &ReorderableListStyle,
        make_edit: F,
        row: R,
    ) -> ReorderableListResponse
    where
        F: Fn(ReorderableListEditRequest) -> Edit<M> + Clone + Send + Sync + 'static,
        R: FnMut(&mut Ui<'a, M>, &T, usize, Rect, /*selected*/ bool, /*dragging*/ bool),
    {
        self.reorderable_list_core(
            id,
            rect,
            items,
            selected,
            accept_drag_kind,
            style,
            make_edit,
            row,
            |_| 0.0,
            |_, _, _| {},
        )
    }

    /// 各 row の **直下に可変高の展開領域** を持てる reorderable list (行内アコーディオン)。
    ///
    /// - `row_extra_h(i)`: row `i` の展開高 (px)。`0.0` で折りたたみ。
    /// - `expansion(ui, i, rect)`: row `i` の展開領域 (`rect` は base row の直下、高さ
    ///   `row_extra_h(i)`) を描く。
    ///
    /// drag 中は全展開を畳んで uniform 行高で扱うので、 reorder のセマンティクスは
    /// [`Self::reorderable_list`] と完全に同じ。
    #[allow(clippy::too_many_arguments)]
    pub fn reorderable_list_expandable<T, F, R, H, E>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        items: &[T],
        selected: &[usize],
        accept_drag_kind: Option<&'static str>,
        style: &ReorderableListStyle,
        make_edit: F,
        row: R,
        row_extra_h: H,
        expansion: E,
    ) -> ReorderableListResponse
    where
        F: Fn(ReorderableListEditRequest) -> Edit<M> + Clone + Send + Sync + 'static,
        R: FnMut(&mut Ui<'a, M>, &T, usize, Rect, bool, bool),
        H: Fn(usize) -> f32,
        E: FnMut(&mut Ui<'a, M>, usize, Rect),
    {
        self.reorderable_list_core(
            id,
            rect,
            items,
            selected,
            accept_drag_kind,
            style,
            make_edit,
            row,
            row_extra_h,
            expansion,
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn reorderable_list_core<T, F, R, H, E>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        items: &[T],
        selected: &[usize],
        accept_drag_kind: Option<&'static str>,
        style: &ReorderableListStyle,
        make_edit: F,
        mut row: R,
        row_extra_h: H,
        mut expansion: E,
    ) -> ReorderableListResponse
    where
        F: Fn(ReorderableListEditRequest) -> Edit<M> + Clone + Send + Sync + 'static,
        R: FnMut(&mut Ui<'a, M>, &T, usize, Rect, bool, bool),
        H: Fn(usize) -> f32,
        E: FnMut(&mut Ui<'a, M>, usize, Rect),
    {
        let wid = WidgetId::ROOT.child((b"reorderable_list", &id));
        let pointer = self.pointer;
        let row_total_h = style.row_height + style.row_gap;
        let item_count = items.len();

        // ---- frame 開始時の session / pending_order 状態 ----
        // drag 中 (session あり) or reorder 直後の settle (pending あり) は **全展開を畳む** =
        // uniform 行高。 これで press / release / drop-indicator は既存の uniform ロジックを
        // そのまま使え、 可変高は「静止時の表示」 だけに閉じ込められる。
        let (collapsed, pending_at_start) = {
            let state: &mut ReorderableListState = self.widget_state(wid);
            (state.session.is_some() || state.pending_order.is_some(), state.pending_order.is_some())
        };
        let _ = pending_at_start;

        // ---- 各 row の base-top (content 空間) の累積 + content_h ----
        // row i は [tops[i], tops[i]+row_height) が base、 続く extra_h(i) が展開、 末尾 row_gap。
        let extra_of = |i: usize| -> f32 {
            if collapsed {
                0.0
            } else {
                row_extra_h(i).max(0.0)
            }
        };
        let mut tops: Vec<f32> = Vec::with_capacity(item_count);
        let mut acc = 0.0;
        for i in 0..item_count {
            tops.push(acc);
            acc += style.row_height + extra_of(i) + style.row_gap;
        }
        let content_h = acc;

        let needs_scrollbar = content_h > rect.h;
        let row_visible_w = if needs_scrollbar {
            (rect.w - SCROLLBAR_W).max(0.0)
        } else {
            rect.w
        };

        // ---- press 検出 ----
        // drag_handle_w 範囲内 (or row 全体) の **base row 部分** で primary_just_pressed →
        // reorder session 開始。 展開領域 (expansion) を押しても drag は始めない (中の widget が
        // 入力を消費する)。
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && rect.contains(px, py)
            // scrollbar 帯 (右端 SCROLLBAR_W) は reorder press の対象外 —
            // thumb ドラッグが Reorder Edit を併発する (list_view と同基準、 review)。
            && px < rect.x + row_visible_w
            && row_total_h > 0.0
        {
            let in_handle = if style.drag_handle_w <= 0.0 {
                true
            } else {
                (px - rect.x) <= style.drag_handle_w
            };
            if in_handle {
                let scroll_y = self.scroll_offset(("reorderable_list_scroll", &id)).1;
                let local = py - rect.y + scroll_y;
                if local >= 0.0 {
                    let hit = (0..item_count).find(|&i| {
                        local >= tops[i] && local < tops[i] + style.row_height
                    });
                    if let Some(idx) = hit {
                        let press_modifiers = pointer.modifiers;
                        let state: &mut ReorderableListState = self.widget_state(wid);
                        state.session = Some(ReorderSession {
                            anchor_index: idx,
                            anchor_mouse_y: py,
                            last_mouse_y: py,
                            press_modifiers,
                        });
                    }
                }
            }
        }

        // ---- drag continue: 毎フレーム last_mouse_y を更新 ----
        // r.md #71 (プラグインのコピー / 移動): 掴んだまま **横へ** 出たら内部 reorder を
        // 打ち切り、 caller に運搬 (`begin_drag`) を引き継がせる。 判定は x だけ
        // (`CARRY_OUT_MARGIN_PX` の doc 参照)。
        let mut dragged_out: Option<usize> = None;
        if let Some((px, py)) = pointer.pos {
            let out_of_x = px < rect.x - CARRY_OUT_MARGIN_PX
                || px > rect.x + rect.w + CARRY_OUT_MARGIN_PX;
            let state: &mut ReorderableListState = self.widget_state(wid);
            if let Some(ref mut s) = state.session {
                s.last_mouse_y = py;
                if out_of_x {
                    dragged_out = Some(s.anchor_index);
                    state.session = None;
                }
            }
        }

        // ---- release: session 取り出し → Reorder 発行 or click 格下げ ----
        // release 時 (drag 中) は collapsed = true なので layout は uniform。 target も uniform な
        // `compute_reorder_target_index` (row_total_h) で計算する。
        let release_session: Option<ReorderSession> = if pointer.primary_just_released {
            let state: &mut ReorderableListState = self.widget_state(wid);
            state.session.take()
        } else {
            None
        };

        // ---- 外部 drag (widget を跨いだ運搬) の受け入れ ----
        // **ポインタがこのリストの矩形の中にあるフレームだけ** 挿入位置を出す
        // (外にあるフレームは None = indicator も drop も無し)。 gate が無いと画面の
        // どこで drag していてもチェーンに indicator が出て、 しかも release でその
        // 位置に落ちてしまう (トラックヘッダへ落とす経路と二重に発火する)。
        // gate を入れれば 2 つの drop 経路は幾何的に排他になる。
        // 行の中点より上なら手前、下なら後ろ。 tops は展開高込みの実表示レイアウトなので、
        // アコーディオンが開いていても indicator が行とずれない。
        let external_insert_at: Option<usize> = accept_drag_kind
            .filter(|k| self.dragging_kind() == Some(*k))
            .and_then(|_| pointer.pos)
            .filter(|&(px, py)| rect.contains(px, py))
            .map(|(_px, py)| {
                let local_y = py - rect.y + self.scroll_offset(("reorderable_list_scroll", &id)).1;
                (0..item_count)
                    .filter(|&i| tops[i] + style.row_height * 0.5 < local_y)
                    .count()
            });
        let external_dropped_at = if pointer.primary_just_released {
            external_insert_at
        } else {
            None
        };

        let mut clicked: Option<usize> = None;
        let mut clicked_modifiers = Modifiers::default();
        if let Some(s) = release_session {
            let dy = (s.last_mouse_y - s.anchor_mouse_y).abs();
            if dy >= DRAG_THRESHOLD_PX {
                let scroll_y = self.scroll_offset(("reorderable_list_scroll", &id)).1;
                let target = compute_reorder_target_index(
                    s.anchor_index,
                    s.last_mouse_y,
                    rect.y,
                    scroll_y,
                    row_total_h,
                    item_count,
                );
                if target != s.anchor_index {
                    let cur: Vec<usize> = (0..item_count).collect();
                    let new_order = apply_reorder(&cur, s.anchor_index, target);
                    let edit = make_edit(ReorderableListEditRequest::Reorder(new_order.clone()));
                    self.push_edit(edit);
                    let state: &mut ReorderableListState = self.widget_state(wid);
                    state.pending_order = Some(new_order);
                }
            } else {
                // 短 click: row 全体 drag mode のときだけ clicked を発火 (drag handle mode では
                // row 残り領域の button_at 等が click を消費する想定なので、widget は出さない)。
                if style.drag_handle_w <= 0.0 {
                    clicked = Some(s.anchor_index);
                    clicked_modifiers = s.press_modifiers;
                }
            }
        }

        // ---- 描画用 session snapshot + pending_order 取得 ----
        let session_for_overlay: Option<ReorderSession> = {
            let state: &mut ReorderableListState = self.widget_state(wid);
            state.session
        };
        let pending_order_for_draw: Option<Vec<usize>> = {
            let state: &mut ReorderableListState = self.widget_state(wid);
            // pending_order は 1 frame だけ保持して次フレームで消費 (= take して使い回し)。
            state.pending_order.take()
        };

        // ---- 描画 (scroll_area + per-row push_rect、list_view と同パターン) ----
        let style_copy = *style;
        let item_count_copy = item_count;
        let hovered = Cell::new(None::<usize>);
        // 描いた行の rect (caller が context_menu / overlay を重ねるため)。
        // 描画クロージャの中から書くので `RefCell` (`hovered` が `Cell` なのと同じ理由)。
        let row_rects: RefCell<Vec<(usize, Rect)>> = RefCell::new(Vec::new());
        // 展開を描くのは「静止時」 のみ (= collapsed=false かつ drag overlay 無し)。
        let draw_expanded = !collapsed && session_for_overlay.is_none();
        let tops_for_draw = tops;

        self.scroll_area(
            ("reorderable_list_scroll", &id),
            rect,
            (rect.w, content_h),
            |ui, offset| {
                if item_count_copy == 0 || row_total_h <= 0.0 {
                    return;
                }
                let visible_top = offset.1;
                let visible_bottom = offset.1 + rect.h;

                if draw_expanded {
                    // ---- 可変高 (静止) 描画: tops_for_draw に従って base row + 展開を描く ----
                    for i in 0..item_count_copy {
                        let base_top = tops_for_draw[i];
                        let extra = (row_extra_h(i)).max(0.0);
                        let row_bottom = base_top + style_copy.row_height + extra;
                        if row_bottom < visible_top || base_top > visible_bottom {
                            continue; // 画面外 row は skip
                        }
                        let row_y = rect.y - offset.1 + base_top;
                        let row_rect = Rect {
                            x: rect.x,
                            y: row_y,
                            w: row_visible_w,
                            h: style_copy.row_height,
                        };
                        let inside = pointer
                            .pos
                            .is_some_and(|(px, py)| row_rect.contains(px, py));
                        let is_selected = selected.contains(&i);
                        let bg = if is_selected {
                            style_copy.row_bg_selected
                        } else if inside {
                            style_copy.row_bg_hover
                        } else {
                            style_copy.row_bg
                        };
                        row_rects.borrow_mut().push((i, row_rect));
                        ui.push_rect(RectCommand {
                            rect: row_rect,
                            fill: bg,
                            border: Color::TRANSPARENT,
                            border_width: 0.0,
                            radius: [style_copy.radius; 4],
                            clip_rect: None,
                        });
                        row(ui, &items[i], i, row_rect, is_selected, false);
                        if extra > 0.0 {
                            let exp_rect = Rect {
                                x: rect.x,
                                y: row_y + style_copy.row_height,
                                w: row_visible_w,
                                h: extra,
                            };
                            expansion(ui, i, exp_rect);
                        }
                        if inside {
                            hovered.set(Some(i));
                        }
                    }
                    return;
                }

                // ---- uniform (drag / settle) 描画: 既存ロジック (pending_order の表示順) ----
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let i_start = (visible_top / row_total_h).floor().max(0.0) as usize;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let i_end = ((visible_bottom / row_total_h).ceil() as usize).min(item_count_copy);

                // 描画順は **表示順** (= pending_order があれば preview 順、無ければ元順)。
                // i は表示位置 (0..item_count)、src は実 items index (= pending_order[i] or i)。
                for i in i_start..i_end {
                    let src = pending_order_for_draw
                        .as_ref()
                        .and_then(|o| o.get(i).copied())
                        .unwrap_or(i);
                    if src >= item_count_copy {
                        continue;
                    }
                    #[allow(clippy::cast_precision_loss)]
                    let row_y = rect.y - offset.1 + (i as f32) * row_total_h;
                    let row_rect = Rect {
                        x: rect.x,
                        y: row_y,
                        w: row_visible_w,
                        h: style_copy.row_height,
                    };
                    let inside = pointer
                        .pos
                        .is_some_and(|(px, py)| row_rect.contains(px, py));
                    let is_selected = selected.contains(&src);
                    let is_dragging = session_for_overlay.is_some_and(|s| s.anchor_index == src);
                    row_rects.borrow_mut().push((src, row_rect));
                    let bg = if is_dragging {
                        style_copy.row_bg_dragging
                    } else if is_selected {
                        style_copy.row_bg_selected
                    } else if inside {
                        style_copy.row_bg_hover
                    } else {
                        style_copy.row_bg
                    };
                    ui.push_rect(RectCommand {
                        rect: row_rect,
                        fill: bg,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [style_copy.radius; 4],
                        clip_rect: None,
                    });
                    row(ui, &items[src], src, row_rect, is_selected, is_dragging);

                    if inside {
                        hovered.set(Some(src));
                    }
                }

                // ---- 外部 drag の drop indicator ----
                // 内部 session と外部 drag は同時に成立しない (session がある間は
                // `dragging_kind()` が None) ので分岐は排他。 位置は展開高込みの
                // 実表示レイアウト (`tops_for_draw`) 基準。
                if let Some(at) = external_insert_at {
                    let top = tops_for_draw.get(at).copied().unwrap_or(content_h);
                    let indicator_y =
                        rect.y - offset.1 + top - style_copy.drop_indicator_h * 0.5;
                    let y_clamped = indicator_y
                        .max(rect.y)
                        .min(rect.y + rect.h - style_copy.drop_indicator_h);
                    ui.push_rect(RectCommand {
                        rect: Rect {
                            x: rect.x,
                            y: y_clamped,
                            w: row_visible_w,
                            h: style_copy.drop_indicator_h,
                        },
                        fill: style_copy.drop_indicator_color,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                }

                // ---- drop indicator: drag 中で dy >= threshold なら描画 ----
                if let Some(s) = session_for_overlay {
                    let dy = (s.last_mouse_y - s.anchor_mouse_y).abs();
                    if dy >= DRAG_THRESHOLD_PX && item_count_copy > 0 {
                        let target = compute_reorder_target_index(
                            s.anchor_index,
                            s.last_mouse_y,
                            rect.y,
                            offset.1,
                            row_total_h,
                            item_count_copy,
                        );
                        if target != s.anchor_index {
                            // anchor 抜き取り後の挿入位置 → 表示上の line 位置:
                            //   target <= anchor → row[target] の上端
                            //   target >  anchor → row[target+1] の上端 (= row[target] 抜き取り後の次行の上)
                            let target_visual = if target > s.anchor_index { target + 1 } else { target };
                            #[allow(clippy::cast_precision_loss)]
                            let indicator_y = rect.y - offset.1
                                + (target_visual as f32) * row_total_h
                                - style_copy.drop_indicator_h * 0.5;
                            // viewport 内 clamp (上下端で indicator がはみ出さないように)
                            let y_clamped = indicator_y
                                .max(rect.y)
                                .min(rect.y + rect.h - style_copy.drop_indicator_h);
                            ui.push_rect(RectCommand {
                                rect: Rect {
                                    x: rect.x,
                                    y: y_clamped,
                                    w: row_visible_w,
                                    h: style_copy.drop_indicator_h,
                                },
                                fill: style_copy.drop_indicator_color,
                                border: Color::TRANSPARENT,
                                border_width: 0.0,
                                radius: [0.0; 4],
                                clip_rect: None,
                            });
                        }
                    }
                }
            },
        );

        ReorderableListResponse {
            clicked,
            clicked_modifiers,
            hovered: hovered.get(),
            dragging: session_for_overlay.map(|s| s.anchor_index),
            dragged_out,
            external_insert_at,
            external_dropped_at,
            row_rects: row_rects.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::{Arc, Mutex};

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use super::{ReorderableListEditRequest, ReorderableListStyle};
    use crate::edit::Edit;
    use crate::input::{FrameInput, PointerFrame};
    use crate::theme::Palette;
    use crate::ui::UiHost;

    /// 元 index 列 `0..n` から anchor → target reorder 適用後の Vec<usize>。
    /// `apply_reorder<usize>` の単純 wrapper、test 側で期待値計算に使う。
    fn order_after(n: usize, anchor: usize, target: usize) -> Vec<usize> {
        let cur: Vec<usize> = (0..n).collect();
        super::apply_reorder(&cur, anchor, target)
    }

    #[test]
    fn reorderable_list_calls_row_for_each_visible_item() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle::from_palette(&Palette::dark());
        let items: Vec<u32> = (0..5).collect();
        let calls = Cell::new(0u32);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.reorderable_list(
                "rl",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &items,
                &[],
                None,
                &style,
                |_| Edit::mutate(|()| {}),
                |_ui, _item, _i, _row_rect, _sel, _drag| {
                    calls.set(calls.get() + 1);
                },
            );
        });

        assert_eq!(calls.get(), 5, "全 5 row 呼ばれる (画面に収まる)");
    }

    #[test]
    fn reorderable_list_skips_offscreen_rows() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle::from_palette(&Palette::dark());
        let items: Vec<u32> = (0..1000).collect();
        let calls = Cell::new(0u32);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.reorderable_list(
                "rl",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &items,
                &[],
                None,
                &style,
                |_| Edit::mutate(|()| {}),
                |_ui, _item, _i, _row_rect, _sel, _drag| {
                    calls.set(calls.get() + 1);
                },
            );
        });

        assert!(calls.get() < 20, "1000 row のうち画面外は skip ({})", calls.get());
        assert!(calls.get() > 0);
    }

    #[test]
    fn short_release_triggers_clicked_not_reorder() {
        // press → release 同フレーム (dy = 0) → click 扱い、Reorder Edit は発行されない
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle::from_palette(&Palette::dark());
        let items: Vec<u32> = (0..5).collect();

        let edit_log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let edit_log_for_clo = edit_log.clone();
        let make_edit = move |_req: ReorderableListEditRequest| -> Edit<()> {
            let log = edit_log_for_clo.clone();
            Edit::mutate(move |()| {
                log.lock().unwrap().push("reorder");
            })
        };

        // row index 1 で press + release
        let click = PointerFrame {
            pos: Some((100.0, 40.0)),
            primary_just_pressed: true,
            primary_just_released: true,
            ..PointerFrame::default()
        };

        let resp_clicked = Cell::new(None::<usize>);
        let resp_drag = Cell::new(None::<usize>);

        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..Default::default() },
            |(), ui| {
                let r = ui.reorderable_list(
                    "rl",
                    Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                    &items,
                    &[],
                    None,
                    &style,
                    make_edit.clone(),
                    |_, _, _, _, _, _| {},
                );
                resp_clicked.set(r.clicked);
                resp_drag.set(r.dragging);
            },
        );

        assert_eq!(resp_clicked.get(), Some(1), "短 release は clicked に格下げ");
        assert!(edit_log.lock().unwrap().is_empty(), "Reorder Edit は発行されない");
    }

    #[test]
    fn long_drag_release_emits_reorder_edit() {
        // press row 0 → drag down 100px → release で Reorder 発行
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle::from_palette(&Palette::dark());
        let items: Vec<u32> = (0..5).collect();

        let captured: Arc<Mutex<Option<Vec<usize>>>> = Arc::new(Mutex::new(None));
        let captured_clo = captured.clone();
        let make_edit = move |req: ReorderableListEditRequest| -> Edit<()> {
            let cap = captured_clo.clone();
            match req {
                ReorderableListEditRequest::Reorder(order) => {
                    *cap.lock().unwrap() = Some(order);
                    Edit::mutate(|()| {})
                }
            }
        };

        // frame 1: press row 0 (y = 12 → row index 0)
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((100.0, 12.0)),
                    primary_just_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| {
                ui.reorderable_list(
                    "rl",
                    Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                    &items,
                    &[],
                    None,
                    &style,
                    make_edit.clone(),
                    |_, _, _, _, _, _| {},
                );
            },
        );
        scene.clear();

        // frame 2: drag down to y=130 + release (dy = 118px > 16、row_total_h=28、
        // local = 130 / 28 = 4.64 → idx 4 frac > 0.5 → target_unbounded = 5、anchor=0 で
        // target_u(5) > anchor+1(1) → target = 5-1 = 4 → apply_reorder = [1,2,3,4,0])
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((100.0, 130.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| {
                ui.reorderable_list(
                    "rl",
                    Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                    &items,
                    &[],
                    None,
                    &style,
                    make_edit.clone(),
                    |_, _, _, _, _, _| {},
                );
            },
        );

        let order = captured.lock().unwrap().clone();
        assert!(order.is_some(), "Reorder Edit が発行された");
        let order = order.unwrap();
        // row_total_h = 28、y=120 → local = 120 / 28 = 4.28 → idx 4 frac > 0.5 → target_unbounded = 5 → clamp 5
        // anchor 0、target_unbounded 5、target 5 - 1 = 4 (anchor より後挿入は -1 詰め)
        // → apply_reorder(0..5, 0, 4) = [1, 2, 3, 4, 0]
        assert_eq!(order, order_after(5, 0, 4), "row 0 を末尾に reorder");
    }

    #[test]
    fn drag_handle_w_restricts_press_zone() {
        // drag_handle_w = 12.0、press at x=20 (handle 範囲外) → session 開始しない → click 扱い
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle {
            drag_handle_w: 12.0,
            ..ReorderableListStyle::from_palette(&Palette::dark())
        };
        let items: Vec<u32> = (0..5).collect();

        let resp_drag = Cell::new(None::<usize>);

        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((20.0, 12.0)),
                    primary_just_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| {
                let r = ui.reorderable_list(
                    "rl",
                    Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                    &items,
                    &[],
                    None,
                    &style,
                    |_| Edit::mutate(|()| {}),
                    |_, _, _, _, _, _| {},
                );
                resp_drag.set(r.dragging);
            },
        );

        assert_eq!(resp_drag.get(), None, "handle 外 press は session 開始しない");
    }

    #[test]
    fn drag_handle_w_inside_starts_session() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle {
            drag_handle_w: 12.0,
            ..ReorderableListStyle::from_palette(&Palette::dark())
        };
        let items: Vec<u32> = (0..5).collect();

        let resp_drag = Cell::new(None::<usize>);

        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((6.0, 12.0)),
                    primary_just_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |(), ui| {
                let r = ui.reorderable_list(
                    "rl",
                    Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                    &items,
                    &[],
                    None,
                    &style,
                    |_| Edit::mutate(|()| {}),
                    |_, _, _, _, _, _| {},
                );
                resp_drag.set(r.dragging);
            },
        );

        assert_eq!(resp_drag.get(), Some(0), "handle 内 press で session 開始");
    }

    #[test]
    fn empty_items_renders_nothing() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle::from_palette(&Palette::dark());
        let items: Vec<u32> = vec![];
        let calls = Cell::new(0u32);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.reorderable_list(
                "rl",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 200.0 },
                &items,
                &[],
                None,
                &style,
                |_| Edit::mutate(|()| {}),
                |_, _, _, _, _, _| {
                    calls.set(calls.get() + 1);
                },
            );
        });

        assert_eq!(calls.get(), 0, "空 list で row callback は 0 回");
    }

    /// 行内アコーディオン。 1 行を展開すると、 その行の直下に展開領域が描かれ、
    /// 後続行が `row_extra_h` 分だけ下にずれる (= 可変行高 layout)。
    #[test]
    fn expandable_row_pushes_later_rows_down_and_calls_expansion() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ReorderableListStyle::from_palette(&Palette::dark()); // row_height 26, gap 2 → row_total 28
        let items: Vec<u32> = (0..4).collect();

        // row 1 を 100px 展開。
        let expanded = 1usize;
        let extra_h = 100.0_f32;

        // expansion callback が呼ばれた (idx, rect.y) を記録。
        let expansion_calls: Cell<u32> = Cell::new(0);
        let expansion_idx: Cell<i64> = Cell::new(-1);
        let expansion_y: Cell<f32> = Cell::new(-1.0);
        // row 2 (展開行の次) の base row.y を記録 → 展開分ずれているか検証。
        let row2_y: Cell<f32> = Cell::new(-1.0);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.reorderable_list_expandable(
                "rl",
                Rect { x: 0.0, y: 0.0, w: 200.0, h: 600.0 },
                &items,
                &[],
                None,
                &style,
                |_| Edit::mutate(|()| {}),
                |_ui, _item, i, row_rect, _sel, _drag| {
                    if i == 2 {
                        row2_y.set(row_rect.y);
                    }
                },
                |i| if i == expanded { extra_h } else { 0.0 },
                |_ui, i, rect| {
                    expansion_calls.set(expansion_calls.get() + 1);
                    expansion_idx.set(i64::try_from(i).unwrap());
                    expansion_y.set(rect.y);
                },
            );
        });

        // 展開 callback は展開行 (1) で 1 度だけ。
        assert_eq!(expansion_calls.get(), 1, "展開行で expansion が 1 度呼ばれる");
        assert_eq!(expansion_idx.get(), i64::try_from(expanded).unwrap());
        // 展開領域は row1 の base (top = 1*28 = 28) の直下 (= 28 + row_height 26 = 54)。
        assert!((expansion_y.get() - 54.0).abs() < 0.01, "expansion rect.y = row1底 (got {})", expansion_y.get());
        // row 2 の base top = row0(28) + row1(26+100+2=128) = ... tops[2] = 28 + 128 = 156。
        // (tops[0]=0, tops[1]=28, tops[2]=28+(26+100+2)=156)
        assert!((row2_y.get() - 156.0).abs() < 0.01, "row2 は展開分ずれる (got {})", row2_y.get());
    }
}
