//! ランチャー帯の幾何 **SSoT**。矩形分割・列 x 写像・セル rect・当たり判定を 1 か所に置く。
//!
//! **行の縦位置はここで作らない。** `ArrangementFrame::rows` (= `arrangement_row_layout`) と
//! `tops[0]` から写すだけで、ヘッダ / アレンジのレーンと同じ 1 本を共有する。
//! 別の式で組み直すと、per-track 行高 override / レーン展開のたびに行がズレる。

use super::*;

// ============================================================
// 矩形分割
// ============================================================

/// ランチャー帯の矩形群。`ArrangementFrame` が 1 フィールドで持つ。
#[derive(Clone, Copy, Debug)]
pub(crate) struct LauncherRects {
    /// 帯全体 (停止列 + セル格子 + 返す列)。arrangement 全高。
    pub pane: Rect,
    /// 上端の見出し行 (ルーラー + Arranger 帯と同じ y 範囲)。
    pub head: Rect,
    /// 停止列 (全高)。上端の `head` ぶんがグローバル停止。
    pub stop_col: Rect,
    /// 「アレンジへ返す」列 (全高)。上端の `head` ぶんがグローバル版。
    pub return_col: Rect,
    /// セル格子の本体 (アレンジのレーンと同じ y 範囲)。
    pub grid: Rect,
    /// シーン見出し (`grid` と同じ x 範囲、`head` と同じ y 範囲)。
    pub scene_head: Rect,
    /// 列 1 本の幅 (px、全列共通)。
    pub col_w: f32,
    /// 横スクロール位置 (列数、小数可)。
    pub scroll_scene: f32,
    /// 帯が畳まれていて格子を描けない (= つかみ代だけ)。
    pub collapsed: bool,
}

impl Default for LauncherRects {
    fn default() -> Self {
        Self {
            pane: ZERO_RECT,
            head: ZERO_RECT,
            stop_col: ZERO_RECT,
            return_col: ZERO_RECT,
            grid: ZERO_RECT,
            scene_head: ZERO_RECT,
            col_w: DEFAULT_COL_W,
            scroll_scene: 0.0,
            collapsed: true,
        }
    }
}

/// 帯幅を `LauncherLayout` と `ui_prefs` から解く。
///
/// **どちらの端でも [`GRAB_W`] を残す** — 左端 (アレンジのみ) でスプリッタが
/// ヘッダ境界に重なって掴めなくなる / 右端 (ランチャーのみ) で画面外へ出る、を
/// 構造的に防ぐ (計画書 Q5)。
#[must_use]
pub(crate) fn resolve_pane_w(view: &LauncherView, avail_w: f32) -> f32 {
    resolve_pane_w_raw(view.layout, view.width, avail_w)
}

/// [`resolve_pane_w`] の素の入力版。
///
/// **`view_build` もここを通す。** アレンジのレーンの幅 (= `view.len_beats` を決める
/// 分母) は帯のぶんを引いた残りなので、帯幅の式が 2 か所にあると「ルーラーと
/// クリップは揃っているのに `arrange_zoom_x` の意味だけ静かにズレる」状態になる。
#[must_use]
pub(crate) fn resolve_pane_w_raw(layout: LauncherLayout, width: f32, avail_w: f32) -> f32 {
    let (lo, hi) = pane_w_bounds(avail_w);
    match layout {
        LauncherLayout::ArrangerOnly => lo,
        LauncherLayout::LauncherOnly => hi,
        LauncherLayout::Both => {
            let w = if width > 0.0 { width } else { DEFAULT_PANE_W };
            // **「両方」は両側が実用幅のときだけ成立する** ([`MIN_BOTH_PANE_W`])。
            // 素の `lo`/`hi` で clamp すると、記憶が潰れた値 (端まで引く途中の 13px 等) を
            // そのまま採用して格子が 1 列も描けない「両方」になる。
            let (blo, bhi) = both_pane_w_bounds(avail_w);
            w.clamp(blo, bhi)
        }
    }
}

/// 帯幅の絶対的な下限 / 上限。**どちらの端でも [`GRAB_W`] を残す** (計画書 Q5)。
///
/// 端に吸着したレイアウト (`ArrangerOnly` / `LauncherOnly`) の帯幅そのものでもある。
#[must_use]
pub(crate) fn pane_w_bounds(avail_w: f32) -> (f32, f32) {
    let lo = GRAB_W.min(avail_w * 0.5).max(0.0);
    (lo, (avail_w - GRAB_W).max(lo))
}

/// 「両方」レイアウトで帯幅に許す範囲。
///
/// 帯とアレンジの**両側**が [`MIN_BOTH_PANE_W`] を満たす範囲。窓がそれを 2 つ取れない
/// ほど狭いときは [`pane_w_bounds`] に落ちる (少なくともスプリッタは掴めるので戻せる)。
///
/// **「表示に使う幅」ではなく「『両方』として記憶してよい幅」の SSoT。**
/// `drag::emit_pane_width` の吸着判定と [`resolve_pane_w_raw`] が同じこの 1 本を通るので、
/// 「覚えた幅で復元したら畳まれていた」が構造的に起きない。
#[must_use]
pub(crate) fn both_pane_w_bounds(avail_w: f32) -> (f32, f32) {
    let (lo, hi) = pane_w_bounds(avail_w);
    if avail_w < MIN_BOTH_PANE_W * 2.0 {
        return (lo, hi);
    }
    (MIN_BOTH_PANE_W.max(lo), (avail_w - MIN_BOTH_PANE_W).min(hi))
}

/// 列幅を `ui_prefs` から解く (未設定 / 壊れた値は既定幅)。
#[must_use]
pub(crate) fn resolve_col_w(view: &LauncherView) -> f32 {
    if view.col_w.is_finite() && view.col_w > 0.0 {
        view.col_w.clamp(MIN_COL_W, MAX_COL_W)
    } else {
        DEFAULT_COL_W
    }
}

/// `frame::build` が呼ぶ矩形分割。`pane_w` は [`resolve_pane_w`] の結果。
///
/// `head_h` は ルーラー + Arranger 帯の高さ (シーン見出し行の高さ)、`body_y` /
/// `body_h` は アレンジのレーンと同じ y 範囲 (= `header_pane.y` / `lanes_h`)。
#[must_use]
pub(crate) fn split(
    rect: Rect,
    pane_x: f32,
    pane_w: f32,
    head_h: f32,
    body_y: f32,
    body_h: f32,
    view: &LauncherView,
) -> LauncherRects {
    let pane = Rect { x: pane_x, y: rect.y, w: pane_w.max(0.0), h: rect.h };
    let head = Rect { x: pane.x, y: rect.y, w: pane.w, h: head_h.max(0.0) };
    // 帯の右端 `PANE_SPLITTER_HANDLE` px は **スプリッタ専用**。ここに列を置くと
    // `zone_at` がスプリッタを先に返すので、「アレンジへ返す」ボタンが押せない
    // (幅を変えるドラッグとボタンが同じ場所に居る、が実機で出た)。
    let grab_w = PANE_SPLITTER_HANDLE.min(pane.w);
    let cols_w = (pane.w - grab_w).max(0.0);
    let stop_w = STOP_COL_W.min(cols_w);
    let return_w = RETURN_COL_W.min((cols_w - stop_w).max(0.0));
    let grid_w = (cols_w - stop_w - return_w).max(0.0);
    let stop_col = Rect { x: pane.x, y: rect.y, w: stop_w, h: rect.h };
    let return_col = Rect {
        x: pane.x + pane.w - grab_w - return_w,
        y: rect.y,
        w: return_w,
        h: rect.h,
    };
    let grid = Rect { x: pane.x + stop_w, y: body_y, w: grid_w, h: body_h.max(0.0) };
    let scene_head = Rect { x: grid.x, y: rect.y, w: grid_w, h: head_h.max(0.0) };
    LauncherRects {
        pane,
        head,
        stop_col,
        return_col,
        grid,
        scene_head,
        col_w: resolve_col_w(view),
        scroll_scene: if view.scroll_scene.is_finite() { view.scroll_scene.max(0.0) } else { 0.0 },
        // 格子が実用にならない幅は「畳まれている」= つかみ代だけを描く。
        collapsed: grid_w < MIN_COL_W * 0.5,
    }
}

// ============================================================
// 列 (シーン) の写像
// ============================================================

impl LauncherRects {
    /// 表示 index `i` の列の左 x。
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn col_x(&self, index: usize) -> f32 {
        self.grid.x + (index as f32 - self.scroll_scene) * self.col_w
    }

    /// `grid` に少しでも掛かる列の表示 index 範囲 `[first, last)`。
    ///
    /// **`Song.scenes` の数を超えて返す** — 超えたぶんが「空きプレースホルダ列」で、
    /// そこにセルを置いた瞬間に `Song::ensure_scene_at` が列を実体化する。load 時に
    /// 列を補わないので「開いただけで `*` が立つ」は起きない (r.md #9)。
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn visible_cols(&self) -> (usize, usize) {
        if self.collapsed || self.col_w <= 0.0 {
            return (0, 0);
        }
        let first = self.scroll_scene.floor().max(0.0) as usize;
        let span = (self.grid.w / self.col_w).ceil().max(0.0) as usize;
        (first, first + span + 1)
    }

    /// x にある列の表示 index (`grid` の外は `None`)。
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub(crate) fn col_index_at_x(&self, x: f32) -> Option<usize> {
        if self.collapsed || self.col_w <= 0.0 || x < self.grid.x || x >= self.grid.x + self.grid.w
        {
            return None;
        }
        let rel = (x - self.grid.x) / self.col_w + self.scroll_scene;
        (rel >= 0.0).then(|| rel.floor() as usize)
    }
}

// ============================================================
// 行 (arrangement と共有)
// ============================================================

/// 画面 y に直した行の上端。`content_top` は content 空間 (先頭行の上端 = 0)。
#[must_use]
pub(crate) fn row_screen_top(f: &ArrangementFrame<'_>, row: &ArrangementRow) -> f32 {
    f.tops.first().copied().unwrap_or(f.lanes.y) + row.content_top
}

/// 行が `grid` の縦範囲に少しでも掛かるか (culling)。
#[must_use]
pub(crate) fn row_visible(f: &ArrangementFrame<'_>, row: &ArrangementRow) -> bool {
    let top = row_screen_top(f, row);
    top + row.height >= f.launcher.grid.y && top <= f.launcher.grid.y + f.launcher.grid.h
}

/// y にある行 (`grid` の縦範囲内のみ)。
#[must_use]
pub(crate) fn row_at_y(f: &ArrangementFrame<'_>, y: f32) -> Option<ArrangementRow> {
    if y < f.launcher.grid.y || y >= f.launcher.grid.y + f.launcher.grid.h {
        return None;
    }
    f.rows.iter().copied().find(|r| {
        let top = row_screen_top(f, r);
        y >= top && y < top + r.height
    })
}

// ============================================================
// セル
// ============================================================

/// この行が **自分のセルを所有できる**か (= 落とし先 / 作成 / 編集 / メニューの対象)。
///
/// **「押せるか」とは別の問い。** グループ行のまとめセルは押せる (子行へ展開して一斉発火)
/// が、グループトラックは自分のクリップを鳴らさない (`process_track_owned` が
/// `track_has_children` で pass 1 を抜ける) ので、そこに作られたセルは
/// **見えない・撃てない・鳴らない**。マスター行はそもそもクリップを持たない。
///
/// 落とし先 ([`drop_cell_at`]) / caller へ返す rect (`draw::row_cells` の `cell_rects`) /
/// ダブルクリックの作成 (`release::double_click`) が **すべてこの 1 本**を通る。
/// 判定を口ごとに書くと「押下側は弾くのに rect 経由の口だけ素通りする」食い違いが
/// 生まれ、右クリックメニューやファイル drop からだけ不可視のセルを作れてしまう。
#[must_use]
pub(crate) fn row_takes_cells(f: &ArrangementFrame<'_>, row_key: ArrangementRowKey) -> bool {
    f.launcher_view.rows.get(&row_key).is_some_and(|r| r.takes_cells)
}

/// 行 × 列のセル矩形。クリップと同じ上下インセット (`clip_to_rect` の `+2 / -4`) を
/// 使うので、帯とアレンジでクリップの高さが揃う。
#[must_use]
pub(crate) fn cell_rect(rects: &LauncherRects, row_top: f32, row_h: f32, col: usize) -> Rect {
    let x = rects.col_x(col);
    Rect {
        x: x + 1.0,
        y: row_top + 2.0,
        w: (rects.col_w - 2.0).max(2.0),
        h: (row_h - 4.0).max(2.0),
    }
}

/// セル左端の ▶ (発火ボタン) の矩形。行が低いときはセル高で頭打ち。
#[must_use]
pub(crate) fn launch_button_rect(cell: Rect) -> Rect {
    let s = LAUNCH_BTN_W.min(cell.h - 2.0).min(cell.w - 2.0).max(4.0);
    Rect { x: cell.x + 1.0, y: cell.y + (cell.h - s) * 0.5, w: s, h: s }
}

/// 行 × 列のセル key (中身の有無まで解決済み)。
#[must_use]
pub(crate) fn cell_key(
    view: &LauncherView,
    row_key: ArrangementRowKey,
    col: usize,
) -> LauncherCellKey {
    let scene_id = view.scenes.get(col).map_or(0, |s| s.id);
    let clip_id = if scene_id == 0 {
        0
    } else {
        view.rows
            .get(&row_key)
            .and_then(|r| r.cells.get(&scene_id))
            .map_or(0, |c| c.clip_id)
    };
    #[allow(clippy::cast_possible_truncation)]
    LauncherCellKey { row: row_key, scene_index: col as u32, scene_id, clip_id }
}

/// ポインタ下のセル (`grid` 内のみ)。
#[must_use]
pub(crate) fn cell_at(
    f: &ArrangementFrame<'_>,
    x: f32,
    y: f32,
) -> Option<(LauncherCellKey, Rect)> {
    let rects = &f.launcher;
    if rects.collapsed || !rects.grid.contains(x, y) {
        return None;
    }
    let col = rects.col_index_at_x(x)?;
    let row = row_at_y(f, y)?;
    // マスター行はクリップを持たない (`song_lanes` = オートメーションのみ) ので、
    // セルという概念が無い。描画 / press / hover が同じここで揃う。
    if row.key == ArrangementRowKey::Track(MASTER_TRACK_ID) {
        return None;
    }
    let top = row_screen_top(f, &row);
    let r = cell_rect(rects, top, row.height, col);
    r.contains(x, y).then(|| (cell_key(f.launcher_view, row.key, col), r))
}

/// **落とし先**としてのセル解決。`cell_at` と違い、セルの見た目のインセット
/// (左右 1px / 上下 2px) を当たり判定に使わない — 格子の中なら列と行から必ず
/// 1 つに決まる。
///
/// 見た目の隙間に落としただけで drop が黙って消える (ドラッグしたクリップが
/// 元の位置へ戻る / ファイルが無反応になる) のを防ぐための別口。
#[must_use]
pub(crate) fn drop_cell_at(
    f: &ArrangementFrame<'_>,
    x: f32,
    y: f32,
) -> Option<LauncherCellKey> {
    let rects = &f.launcher;
    if rects.collapsed || !rects.grid.contains(x, y) {
        return None;
    }
    let col = rects.col_index_at_x(x)?;
    let row = row_at_y(f, y)?;
    // マスター行 / グループ行はセルを所有できない ([`row_takes_cells`] が唯一の判定)。
    if !row_takes_cells(f, row.key) {
        return None;
    }
    Some(cell_key(f.launcher_view, row.key, col))
}

// ============================================================
// スプリッタ
// ============================================================

/// 帯の右端スプリッタ (帯幅を変える) の当たり判定。
#[must_use]
pub(crate) fn pane_splitter_at(f: &ArrangementFrame<'_>, x: f32, y: f32) -> bool {
    pane_splitter_hit(f.rect, f.launcher.pane.x + f.launcher.pane.w, x, y)
}

/// [`pane_splitter_at`] の純関数版 (テストと caller が同じ 1 本を通る)。
///
/// ホットゾーンは **境界の左側 [`PANE_SPLITTER_HANDLE`] px だけ** (= 帯の中)。
/// 中心対称に張るとアレンジのレーンの左端 (拍 0) を食う ([`PANE_SPLITTER_HANDLE`] の doc)。
/// 畳みきった帯の幅 ([`GRAB_W`]) と同じなので、畳んだ帯 = 掴み代 が成り立つ。
#[must_use]
pub(crate) fn pane_splitter_hit(rect: Rect, boundary_x: f32, x: f32, y: f32) -> bool {
    x >= boundary_x - PANE_SPLITTER_HANDLE
        && x < boundary_x
        && y >= rect.y
        && y < rect.y + rect.h
}

/// シーン見出しの列境界スプリッタ (列幅を変える) の当たり判定。
/// 当たった列の表示 index (= 右端を掴んだ列) を返す。
#[must_use]
pub(crate) fn col_splitter_at(f: &ArrangementFrame<'_>, x: f32, y: f32) -> Option<usize> {
    let rects = &f.launcher;
    let handle = f.style.header_resize_handle_px;
    if rects.collapsed || handle <= 0.0 || !rects.scene_head.contains(x, y) {
        return None;
    }
    let half = handle * 0.5;
    let (first, last) = rects.visible_cols();
    (first..last).find(|&i| {
        let right = rects.col_x(i) + rects.col_w;
        x >= right - half && x < right + half
    })
}
