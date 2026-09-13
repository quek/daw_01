//! r.md #110 (`docs/plan_parallel.md` §6.1): インスペクタの chain list — **縦回転 Live 型**。
//!
//! Live の Device View は「Chain List (縦) | 選択 chain の device (横)」 で、 Parallel の中の
//! Parallel は括弧の中の括弧。 280px のインスペクタでは「隣」を「下」にする: Parallel は
//! 開始行 `「` / chain 行 × N / `+ chain` / 選択 chain の device (再帰) / `+ Plugin` /
//! 終了行 `L`。 括弧は chain の色帯と同じ x / 幅の Parallel 色の帯で、 開始行から終了行まで 1 本に
//! 繋がる (chain の帯はその上に乗る)。 非選択 chain は 1 行だけなので縦にも横にも爆発しない。 展開中 chain の
//! device 区間は左端の細い色帯 (chain 色) で示し、 インデントは帯の幅 (4px / 深さ) だけ。
//!
//! 行の flatten は view-model ([`AppData::chain_rows`]) が持ち、 ここは描画と入力だけ。
//! drag&drop は daw-ui の [`Ui::drag_list`] (行の種類を知らない generic widget) に、
//! 「掴める行 / 運ぶブロック長 / 落とせるスロット」 を渡して結果を `RelocateDevices` へ
//! 写像する。 Sidechain は旧 独立セクションを撤去し、 aux 入力 port を持つ plugin 行の
//! `SC` で行直下に展開する (source は他 track + 同 track の Parallel 内 chain)。
//!
//! 行の種類ごとの中身は別ファイル (サイズ budget、 不変条件 9): plugin 行と展開は
//! `plugin_row.rs`、 chain 行と操作行は `chain_row.rs`、 Parallel のヘッダ行は
//! `parallel_header.rs`、 右クリックメニューは `row_menu.rs`。 ここに残るのは list 全体
//! (行高 / slot / drop / hover / click) と、 行の背景・色帯。

use daw_ui_core::{DragListRow, DragListSlot, DragListStyle, Edit, ToggleButtonStyle, Ui, WidgetId};
use daw_ui_renderer::{Color, Rect};

use crate::app::{
    AppData, AppEvent, ChainRow, ChainRowKind, ColorPickerTarget, DeviceDragPayload, InsertAt,
    RelocateDevices,
};
use crate::event_device::DeviceEvent;
use crate::widgets::select_modifier::SelectModifier;
use common::model::ChainRef;

use super::chain_row::{draw_add_chain_row, draw_add_plugin_row, draw_chain_row};
use super::plugin_row::{SC_PAD, SC_PORT_H, draw_plugin_expansions, draw_plugin_row};
use super::row_menu::{carried_device_ids, draw_context_menus};
use super::toggle_audio_style;

/// 行高 (plugin / Parallel 開始 / chain 行)。
pub(super) const ROW_H: f32 = 26.0;
/// 操作行 (`+ chain` / `+ Plugin`) と終了行の高さ。
const OP_ROW_H: f32 = 22.0;
const END_ROW_H: f32 = 10.0;
pub(super) const ROW_GAP: f32 = 3.0;
/// 深さ 1 段ぶんの色帯の幅 (= インデント)。
pub(super) const BAR_W: f32 = 4.0;
/// Parallel の括弧 (`「` / `L`) の横棒の長さ (開閉 disclosure の手前まで)。
const BRACKET_STUB_W: f32 = 10.0;

/// 展開状態と行の種類から「この行の高さ」を決める。
pub(super) fn base_row_h(kind: &ChainRowKind) -> f32 {
    match kind {
        ChainRowKind::Plugin(_)
        | ChainRowKind::ParallelBegin { .. }
        | ChainRowKind::SplitParams { .. }
        | ChainRowKind::Chain { .. } => ROW_H,
        ChainRowKind::AddChain { .. } | ChainRowKind::AddPlugin { .. } => OP_ROW_H,
        ChainRowKind::ParallelEnd { .. } => END_ROW_H,
    }
}

/// chain list を `y` から描き、 消費後の `y` を返す。
pub(super) fn draw_chain_list(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let rows = app.chain_rows();
    let cursor_tid = app.cursor_track_id();
    // 右クリックメニューが開いている frame は行の click / button を評価しない
    // (`[[feedback_popup_click_leaks_to_background]]`)。
    let popup_open = ui.has_open_popups();

    // 展開中の param パネル / SC パネル (表示中の chain に居るものだけ)。
    let plugin_ids: Vec<u64> = rows
        .iter()
        .filter_map(|r| match &r.kind {
            ChainRowKind::Plugin(e) => Some(e.device_id),
            _ => None,
        })
        .collect();
    let open_dev: Option<u64> = app
        .cur.peph
        .open_plugin_params
        .or(app.cur.peph.open_video_fx_params)
        .filter(|id| plugin_ids.contains(id));
    let sc_open: Option<u64> = app
        .cur.peph
        .open_sidechain_panel
        .filter(|id| plugin_ids.contains(id));
    let panel_h = if app.cur.peph.inspector_device_panel_h > 1.0 {
        app.cur.peph.inspector_device_panel_h
    } else {
        280.0 // 初回 bootstrap: expansion を 1 度描かせて実測させる
    };
    let sc_ports = sc_open.map(|id| app.sidechain_ports(id)).unwrap_or_default();
    let sc_panel_h = if sc_ports.is_empty() {
        0.0
    } else {
        sc_ports.len() as f32 * SC_PORT_H + SC_PAD
    };

    let (list_rows, slots, slot_targets) =
        build_list_rows(&rows, open_dev, panel_h, sc_open, sc_panel_h);
    let content_h: f32 = list_rows.iter().map(|r| r.height + ROW_GAP).sum::<f32>() + 4.0;

    ui.label_at("inspector_rack_label", "Rack", area.x + pad, y, 12.0, p.text);
    y += 18.0;
    let list_rect = Rect { x: area.x + pad, y, w: area.w - pad * 2.0, h: content_h };
    let style = DragListStyle {
        row_gap: ROW_GAP,
        drop_indicator_color: p.loop_band,
        drop_indicator_h: 2.0,
    };

    // 落とせるか: 掴んだ device (Parallel) の中の chain へは落とせない (循環)。
    let song = app.cur.song_doc.song();
    let valid_drop = |from: Option<usize>, slot: usize| -> bool {
        let Some(from) = from else { return true };
        let Some(dev_id) = rows.get(from).and_then(ChainRow::drag_id) else {
            return false;
        };
        let (dest, _) = slot_targets[slot];
        let ChainRef::Chain(cid) = dest else { return true };
        !common::model::chain_is_inside_device(song.fx_chain_by_track_id(cursor_tid.unwrap_or(0)).unwrap_or(&[]), dev_id, cid)
    };

    let ctx = RowCtx {
        rows: &rows,
        popup_open,
        cursor_tid,
        open_dev,
        panel_h,
        sc_open,
        sc_ports: &sc_ports,
        sc_panel_h,
        area,
        pad,
        keys_style: toggle_audio_style(&app.theme),
    };
    let resp = ui.drag_list(
        "inspector_chain",
        list_rect,
        &list_rows,
        &slots,
        Some(crate::app_types::DEVICE_DRAG_KIND),
        &style,
        valid_drop,
        |ui, i, row_rect, hovered, dragging| {
            draw_row(app, ui, &ctx, i, row_rect, hovered, dragging);
        },
    );

    // ---- 応答 ----
    // hover 行 (Q / ショートカットの対象)。 plugin / Parallel のみ。
    let hovered_device = resp
        .hovered
        .and_then(|i| rows.get(i))
        .and_then(ChainRow::drag_id);
    if app.cur.peph.inspector_hovered_device != hovered_device {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cur.peph.inspector_hovered_device = hovered_device;
        }));
    }
    // click = 選択 (無修飾 / Ctrl / Shift)。 Parallel / chain の開閉は行左端の disclosure。
    if !popup_open
        && let Some(i) = resp.clicked
        && let Some(r) = rows.get(i)
        && let Some(id) = r.select_id()
    {
        let m = resp.clicked_modifiers;
        let modifier = SelectModifier::from_modifiers(m.shift, m.ctrl);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::SelectDevice { device_id: id, modifier }));
        }));
    }
    // 内部 drop = 移動 (Ctrl でコピー)。
    if let Some((from, slot)) = resp.dropped
        && let Some(r) = rows.get(from)
        && let Some(id) = r.drag_id()
    {
        let device_ids = carried_device_ids(app, &rows, id);
        let (dest, dest_index) = slot_targets[slot];
        let copy = ui.pointer().modifiers.ctrl;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::RelocateDevices(RelocateDevices {
                device_ids: device_ids.clone(),
                dest,
                dest_index: InsertAt::Index(dest_index),
                copy,
            })));
        }));
    }
    // 横へ出た = トラック跨ぎの運搬を始める。
    if let Some(i) = resp.dragged_out
        && let Some(r) = rows.get(i)
        && let Some(id) = r.drag_id()
    {
        let device_ids = carried_device_ids(app, &rows, id);
        ui.begin_drag(
            crate::app_types::DEVICE_DRAG_KIND,
            DeviceDragPayload {
                device_ids,
                source_track: cursor_tid.unwrap_or(common::model::MASTER_TRACK_ID),
            },
        );
    }
    // 外部 drop (別 track の chain から運ばれてきた)。
    if let Some(slot) = resp.external_dropped_slot
        && let Some(copy) = ui.drag_modifiers().map(|m| m.ctrl)
        && let Some(pl) = ui.take_drag_payload::<DeviceDragPayload>(crate::app_types::DEVICE_DRAG_KIND)
    {
        let (dest, dest_index) = slot_targets[slot];
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::RelocateDevices(RelocateDevices {
                device_ids: pl.device_ids.clone(),
                dest,
                dest_index: InsertAt::Index(dest_index),
                copy,
            })));
        }));
    }
    draw_context_menus(app, ui, &rows, &resp.row_rects);

    list_rect.y + list_rect.h + 8.0
}

/// drag_list の入力: 行ごとの高さ (展開込み) / 掴めるか / ブロック長と、落とせるスロット
/// (`slots[i]` の落とし先が `slot_targets[i] = (chain, index)`)。
fn build_list_rows(
    rows: &[ChainRow],
    open_dev: Option<u64>,
    panel_h: f32,
    sc_open: Option<u64>,
    sc_panel_h: f32,
) -> (Vec<DragListRow>, Vec<DragListSlot>, Vec<(ChainRef, u32)>) {
    let mut list_rows: Vec<DragListRow> = Vec::with_capacity(rows.len());
    let mut slots: Vec<DragListSlot> = Vec::new();
    let mut slot_targets: Vec<(ChainRef, u32)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let mut h = base_row_h(&r.kind);
        let mut draggable = false;
        let mut block_len = 1;
        let mut slot: Option<(ChainRef, u32)> = None;
        match &r.kind {
            ChainRowKind::Plugin(e) => {
                draggable = true;
                if open_dev == Some(e.device_id) {
                    h += panel_h;
                }
                if sc_open == Some(e.device_id) {
                    h += sc_panel_h;
                }
                slot = Some((r.chain, r.index));
            }
            ChainRowKind::ParallelBegin { parallel_id, .. } => {
                draggable = true;
                // 対応する終了行までがブロック。
                block_len = rows[i..]
                    .iter()
                    .position(|x| matches!(&x.kind, ChainRowKind::ParallelEnd { parallel_id: r2, .. } if r2 == parallel_id))
                    .map_or(1, |k| k + 1);
                slot = Some((r.chain, r.index));
            }
            ChainRowKind::AddPlugin { chain } => slot = Some((*chain, r.index)),
            _ => {}
        }
        if let Some(t) = slot {
            slots.push(DragListSlot { after_row: i, indent: indent_of(r) });
            slot_targets.push(t);
        }
        list_rows.push(DragListRow { height: h, draggable, block_len });
    }
    (list_rows, slots, slot_targets)
}

fn indent_of(r: &ChainRow) -> f32 {
    r.bars.len() as f32 * BAR_W
}

/// 展開中 chain の区間を示す左端の色帯 (深さぶん)。
fn draw_bars(ui: &mut Ui<'_, AppData>, r: &ChainRow, row_rect: Rect, p: &daw_ui_core::Palette) {
    for (k, c) in r.bars.iter().enumerate() {
        let col = c
            .map(|rgb| Color { r: rgb[0], g: rgb[1], b: rgb[2], a: 1.0 })
            .unwrap_or(p.accent);
        ui.panel(
            ("inspector_chain_bar", r.chain, r.index, k),
            Rect { x: row_rect.x + k as f32 * BAR_W, y: row_rect.y - ROW_GAP, w: BAR_W - 1.0, h: row_rect.h + ROW_GAP },
            col,
            0.0,
        );
    }
}

/// Parallel の直接の行に、 その Parallel の色の帯を chain 行の色見本と同じ x / 幅で通す。
/// 開始行は行の中央の `「` から始まり (折り畳み中は横棒だけ = 終了行が無い)、 終了行は `L` で
/// 閉じる。 途中の行は上下の行と隙間なく繋ぐ。 chain 行の色見本と展開中 chain の帯はこの上に
/// 乗るので、 chain 同士の切れ目に Parallel の色が覗いて 1 本の括弧に見える。
/// 開始行の帯 click は Parallel の color picker (chain 行の色見本と同じ操作)。
fn draw_parallel_band(
    ui: &mut Ui<'_, AppData>,
    i: usize,
    r: &ChainRow,
    content: Rect,
    popup_open: bool,
    p: &daw_ui_core::Palette,
) {
    let Some(color) = r.parallel_band else {
        return;
    };
    let col = rgb_or(color, p.text_dim);
    let w = BAR_W - 1.0;
    let stub = |ui: &mut Ui<'_, AppData>, y: f32| {
        ui.panel(("inspector_parallel_stub", i), Rect { x: content.x, y, w: BRACKET_STUB_W, h: w }, col, 0.0);
    };
    let mid = content.y + (content.h - w) * 0.5;
    if let ChainRowKind::ParallelBegin { parallel_id, .. } = &r.kind {
        // 帯 (と括弧の横棒) の click で picker。 hit は chain の色見本と同じく帯より少し広く。
        let hit = Rect { x: content.x, y: content.y, w: BRACKET_STUB_W, h: content.h };
        let pointer = ui.pointer();
        let inside = !popup_open && pointer.pos.is_some_and(|(px, py)| hit.contains(px, py));
        if ui.primary_click(WidgetId::ROOT.child((b"inspector_parallel_band", *parallel_id)), inside).clicked {
            let parallel_id = *parallel_id;
            let anchor = Rect { x: content.x, y: content.y, w, h: content.h };
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.open_color_picker(ColorPickerTarget::Parallel(parallel_id), anchor);
            }));
        }
    }
    let (top, bottom) = match &r.kind {
        ChainRowKind::ParallelBegin { open: false, .. } => {
            stub(ui, mid);
            return;
        }
        ChainRowKind::ParallelBegin { .. } => {
            stub(ui, mid);
            (mid, content.y + content.h + ROW_GAP)
        }
        ChainRowKind::ParallelEnd { .. } => {
            stub(ui, mid);
            (content.y - ROW_GAP, mid + w)
        }
        _ => (content.y - ROW_GAP, content.y + content.h + ROW_GAP),
    };
    ui.panel(("inspector_parallel_band", i), Rect { x: content.x, y: top, w, h: bottom - top }, col, 0.0);
}

fn draw_row_bg(
    ui: &mut Ui<'_, AppData>,
    i: usize,
    rect: Rect,
    selected: bool,
    hovered: bool,
    dragging: bool,
    p: &daw_ui_core::Palette,
) {
    // 「選択」 は param パネルの開閉で示すので、 選択行は accent で塗らず薄い枠だけ。
    let fill = if dragging {
        p.accent.with_alpha(0.5)
    } else if hovered {
        p.control_hover
    } else {
        p.panel_raised
    };
    let border = if selected { p.accent } else { Color::TRANSPARENT };
    ui.panel_with_border(("inspector_chain_row_bg", i), rect, fill, border, if selected { 1.0 } else { 0.0 }, 3.0);
}

pub(super) fn rgb_or(color: Option<[f32; 3]>, fallback: Color) -> Color {
    color.map_or(fallback, |rgb| Color { r: rgb[0], g: rgb[1], b: rgb[2], a: 1.0 })
}

/// 1 行ぶんの描画に要る、buffer 全体で共通の文脈 (drag_list の row closure から呼ぶ)。
pub(super) struct RowCtx<'a> {
    pub(super) rows: &'a [ChainRow],
    pub(super) popup_open: bool,
    pub(super) cursor_tid: Option<u32>,
    pub(super) open_dev: Option<u64>,
    pub(super) panel_h: f32,
    pub(super) sc_open: Option<u64>,
    pub(super) sc_ports: &'a [crate::app_types::SidechainPort],
    pub(super) sc_panel_h: f32,
    pub(super) area: Rect,
    pub(super) pad: f32,
    pub(super) keys_style: ToggleButtonStyle,
}

/// 行 `i` を描く (背景 / 色帯 / 種類ごとの中身 / 展開)。
fn draw_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    ctx: &RowCtx<'_>,
    i: usize,
    row_rect: Rect,
    hovered: bool,
    dragging: bool,
) {
    let p = &app.theme.core;
    let RowCtx { rows, popup_open, cursor_tid, keys_style, .. } = ctx;
    let (popup_open, cursor_tid) = (*popup_open, *cursor_tid);
    let r = &rows[i];
    draw_bars(ui, r, row_rect, p);
    let content = Rect {
        x: row_rect.x + indent_of(r),
        y: row_rect.y,
        w: (row_rect.w - indent_of(r)).max(1.0),
        h: row_rect.h,
    };
    // 背景 → Parallel の帯 → 中身、 の順 (chain 行の色見本は中身側で帯の上に描く)。
    let selected = r.select_id().is_some_and(|id| app.cur.selection.selected_device_ids.contains(&id));
    match &r.kind {
        ChainRowKind::Plugin(_) | ChainRowKind::ParallelBegin { .. } => {
            draw_row_bg(ui, i, content, selected, hovered, dragging, p);
        }
        ChainRowKind::Chain { .. } => draw_row_bg(ui, i, content, selected, hovered, false, p),
        _ => {}
    }
    draw_parallel_band(ui, i, r, content, popup_open, p);
    match &r.kind {
        ChainRowKind::Plugin(e) => {
            draw_plugin_row(app, ui, i, e, content, popup_open, keys_style);
            draw_plugin_expansions(app, ui, ctx, e.device_id, content);
        }
        ChainRowKind::ParallelBegin { parallel_id, name, bypassed, open, out_gain, gain_match, split, .. } => {
            let head = super::parallel_header::ParallelHead {
                parallel_id: *parallel_id,
                name,
                bypassed: *bypassed,
                open: *open,
                out_gain: *out_gain,
                gain_match: *gain_match,
                split: *split,
            };
            super::parallel_header::draw_parallel_begin_row(app, ui, i, &head, content, popup_open);
        }
        ChainRowKind::SplitParams { parallel_id, split } => {
            super::parallel_header::draw_split_row(app, ui, i, *parallel_id, *split, content, popup_open);
        }
        ChainRowKind::Chain { parallel_id, chain_id, name, color, gain, pan, muted, solo, open, n_devices, inactive } => {
            draw_chain_row(
                app, ui, i, *parallel_id, *chain_id, name, *color, *gain, *pan, *muted, *solo, *open, *n_devices, *inactive, content,
                popup_open,
            );
        }
        ChainRowKind::AddChain { parallel_id } => draw_add_chain_row(ui, i, *parallel_id, content, popup_open),
        ChainRowKind::AddPlugin { chain } => {
            let is_master = cursor_tid == Some(common::model::MASTER_TRACK_ID);
            draw_add_plugin_row(ui, i, *chain, is_master, content, popup_open);
        }
        // 終了行は `draw_parallel_band` の `L` だけ。
        ChainRowKind::ParallelEnd { .. } => {}
    }
}
