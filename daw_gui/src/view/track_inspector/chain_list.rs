//! r.md #110 (`docs/plan_parallel.md` §6.1): インスペクタの chain list — **縦回転 Live 型**。
//!
//! Live の Device View は「Chain List (縦) | 選択 chain の device (横)」 で、 Parallel の中の
//! Parallel は括弧の中の括弧。 280px のインスペクタでは「隣」を「下」にする: Parallel は
//! 開始行 `╭` / chain 行 × N / `+ chain` / 選択 chain の device (再帰) / `+ Plugin` /
//! 終了行 `╰`。 非選択 chain は 1 行だけなので縦にも横にも爆発しない。 展開中 chain の
//! device 区間は左端の細い色帯 (chain 色) で示し、 インデントは帯の幅 (4px / 深さ) だけ。
//!
//! 行の flatten は view-model ([`AppData::chain_rows`]) が持ち、 ここは描画と入力だけ。
//! drag&drop は daw-ui の [`Ui::drag_list`] (行の種類を知らない generic widget) に、
//! 「掴める行 / 運ぶブロック長 / 落とせるスロット」 を渡して結果を `RelocateDevices` へ
//! 写像する。 Sidechain は旧 独立セクションを撤去し、 aux 入力 port を持つ plugin 行の
//! `SC` で行直下に展開する (source は他 track + 同 track の Parallel 内 chain)。

use daw_ui_core::{
    DragListRow, DragListSlot, DragListStyle, Edit, KnobStyle, ToggleButtonStyle, Ui,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{
    AppData, AppEvent, ChainEntry, ChainRow, ChainRowKind, ColorPickerTarget, DeviceDragPayload,
    RelocateDevices,
};
use crate::handler::parallel::ChainMixerEdit;
use crate::view::disclosure::{RevealAxis, disclosure_glyph};
use crate::view::param_gesture::push_param_gesture_edges;
use crate::widgets::select_modifier::SelectModifier;
use common::model::{AutomationTarget, ChainRef, TapPoint, TapSource, TrackBuiltinParam};

use super::{device_panel, toggle_audio_style};

/// 行高 (plugin / Parallel 開始 / chain 行)。
const ROW_H: f32 = 26.0;
/// 操作行 (`+ chain` / `+ Plugin`) と終了行の高さ。
const OP_ROW_H: f32 = 22.0;
const END_ROW_H: f32 = 10.0;
const ROW_GAP: f32 = 3.0;
/// 深さ 1 段ぶんの色帯の幅 (= インデント)。
const BAR_W: f32 = 4.0;
/// chain 行の mixer: ミニ knob と M / S。
const CHAIN_KNOB: f32 = 18.0;
const CHAIN_BTN_W: f32 = 18.0;
/// SC パネル 1 port 行の高さ。
const SC_PORT_H: f32 = 24.0;
const SC_PAD: f32 = 6.0;

/// 展開状態と行の種類から「この行の高さ」を決める。
fn base_row_h(kind: &ChainRowKind) -> f32 {
    match kind {
        ChainRowKind::Plugin(_) | ChainRowKind::ParallelBegin { .. } | ChainRowKind::Chain { .. } => ROW_H,
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
        .ui_ephemeral
        .open_plugin_params
        .or(app.ui_ephemeral.open_video_fx_params)
        .filter(|id| plugin_ids.contains(id));
    let sc_open: Option<u64> = app
        .ui_ephemeral
        .open_sidechain_panel
        .filter(|id| plugin_ids.contains(id));
    let panel_h = if app.ui_ephemeral.inspector_device_panel_h > 1.0 {
        app.ui_ephemeral.inspector_device_panel_h
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
    let song = app.song_doc.song();
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
    if app.ui_ephemeral.inspector_hovered_device != hovered_device {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.inspector_hovered_device = hovered_device;
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
            app.handle_event(AppEvent::SelectDevice { device_id: id, modifier });
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
            app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
                device_ids: device_ids.clone(),
                dest,
                dest_index,
                copy,
            }));
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
            app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
                device_ids: pl.device_ids.clone(),
                dest,
                dest_index,
                copy,
            }));
        }));
    }
    draw_context_menus(app, ui, &rows, &resp.row_rects);

    list_rect.y + list_rect.h + 8.0
}

/// 右クリックメニュー (widget の外で重ねる idiom)。 plugin / Parallel / chain 行だけ。
fn draw_context_menus(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rows: &[ChainRow],
    row_rects: &[(usize, Rect)],
) {
    for (i, row_rect) in row_rects {
        let Some(r) = rows.get(*i) else { continue };
        let base = Rect { x: row_rect.x, y: row_rect.y, w: row_rect.w, h: base_row_h(&r.kind) };
        match &r.kind {
            ChainRowKind::Plugin(e) => {
                let device_id = e.device_id;
                let bypass_label = if app.all_devices_bypassed(&carried_device_ids(app, rows, device_id)) {
                    "有効化"
                } else {
                    "無効化"
                };
                let labels = [bypass_label, "Parallel にまとめる", "コピー", "切り取り", "貼り付け", "複製", "削除"];
                context_menu(ui, base, &labels, move |app, idx| apply_device_menu(app, idx, device_id));
            }
            ChainRowKind::ParallelBegin { parallel_id, bypassed, .. } => {
                let parallel_id = *parallel_id;
                let bypass_label = if *bypassed { "有効化" } else { "無効化" };
                let labels = [bypass_label, "Parallel を解除", "名前変更", "コピー", "切り取り", "複製", "削除"];
                context_menu(ui, base, &labels, move |app, idx| apply_parallel_menu(app, idx, parallel_id));
            }
            ChainRowKind::Chain { parallel_id, chain_id, .. } => {
                let (parallel_id, chain_id) = (*parallel_id, *chain_id);
                let labels = ["名前変更", "色...", "複製", "chain 追加", "削除"];
                context_menu(ui, base, &labels, move |app, idx| apply_chain_menu(app, idx, parallel_id, chain_id, base));
            }
            _ => {}
        }
    }
}

/// 行 `base` の右クリックメニュー。 選ばれた項目 `idx` を `apply` で model に反映する。
fn context_menu(
    ui: &mut Ui<'_, AppData>,
    base: Rect,
    labels: &[&str],
    apply: impl Fn(&mut AppData, usize) + Copy + Send + 'static,
) {
    ui.context_menu_for(base, labels, move |idx, ui| {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| apply(app, idx)));
    });
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

/// plugin 行の直下の展開 (SC パネル / param パネル)。
fn draw_plugin_expansions(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    ctx: &RowCtx<'_>,
    device_id: u64,
    content: Rect,
) {
    let mut ey = content.y + ROW_H;
    if ctx.sc_open == Some(device_id) && ctx.sc_panel_h > 0.0 {
        let rect = Rect { x: content.x, y: ey, w: content.w, h: ctx.sc_panel_h };
        draw_sidechain_panel(app, ui, device_id, ctx.sc_ports, rect);
        ey += ctx.sc_panel_h;
    }
    if ctx.open_dev == Some(device_id) {
        let exp_rect = Rect { x: content.x, y: ey, w: content.w, h: ctx.panel_h };
        let measured =
            (device_panel::draw_device_panel(app, ui, ctx.area, ctx.pad, exp_rect) - exp_rect.y).max(0.0);
        // 展開部の実消費高を測って次フレームの行高に使う (lag-by-one)。
        if (app.ui_ephemeral.inspector_device_panel_h - measured).abs() > 0.5 {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.ui_ephemeral.inspector_device_panel_h = measured;
            }));
        }
    }
}

/// plugin 行: 名前 + [SC▾] [⌨] [Par|GUI] [x]。
#[allow(clippy::too_many_arguments)]
fn draw_plugin_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    i: usize,
    entry: &ChainEntry,
    row: Rect,
    popup_open: bool,
    keys_style: &ToggleButtonStyle,
) {
    let p = &app.theme.core;
    let device_id = entry.device_id;
    let btn_gui_w = 44.0;
    let btn_x_w = 26.0;
    let btn_keys_w = 26.0;
    let btn_sc_w = 34.0;
    let btn_h = ROW_H - 4.0;
    let by = row.y + 2.0;
    let mut right = row.x + row.w - btn_x_w;
    // [x]
    ui.button_at(
        ("inspector_row_remove", i),
        "x",
        Rect { x: right, y: by, w: btn_x_w, h: btn_h },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![device_id] });
                }
            })
        },
    );
    // [Par | GUI]
    if entry.shows_button() {
        right -= btn_gui_w + 2.0;
        let label = if entry.shows_param_panel() { "Par" } else { "GUI" };
        ui.button_at(
            ("inspector_row_gui", i),
            label,
            Rect { x: right, y: by, w: btn_gui_w, h: btn_h },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    if !popup_open {
                        app.handle_event(AppEvent::ToggleSlotGui { device_id });
                    }
                })
            },
        );
    }
    // [⌨] (埋め込みエディタ窓を開く device だけ)
    if entry.has_embedded_gui && !entry.shows_param_panel() {
        right -= btn_keys_w + 2.0;
        let next = !entry.send_all_keys;
        ui.toggle_button_at(
            ("inspector_row_keys", i),
            "\u{2328}",
            Rect { x: right, y: by, w: btn_keys_w, h: btn_h },
            entry.send_all_keys,
            keys_style,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    if !popup_open {
                        app.handle_event(AppEvent::SetPluginSendAllKeys { device_id, enabled: next });
                    }
                })
            },
        );
    }
    // [SC] (aux 入力 port を持つ plugin だけ)。 配線済みは ON 色。
    if entry.aux_input_count > 0 {
        right -= btn_sc_w + 2.0;
        let open = app.ui_ephemeral.open_sidechain_panel == Some(device_id);
        ui.toggle_button_at(
            ("inspector_row_sc", i),
            if open { "SC\u{25B4}" } else { "SC\u{25BE}" },
            Rect { x: right, y: by, w: btn_sc_w, h: btn_h },
            entry.sc_wired || open,
            keys_style,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    if popup_open {
                        return;
                    }
                    app.ui_ephemeral.open_sidechain_panel =
                        if app.ui_ephemeral.open_sidechain_panel == Some(device_id) { None } else { Some(device_id) };
                })
            },
        );
    }
    // 名前 (ボタンの手前で打ち切る)。
    let failed = entry.load_error.is_some();
    let display_name: std::borrow::Cow<'_, str> = if failed {
        format!("[未ロード] {}", entry.plugin_name).into()
    } else {
        entry.plugin_name.as_str().into()
    };
    let name_x = row.x + 8.0;
    ui.label_at_clipped(
        ("inspector_row_name", i),
        &display_name,
        Rect { x: name_x, y: row.y + 8.0, w: (right - 6.0 - name_x).max(1.0), h: 11.0 * 1.2 },
        11.0,
        if failed {
            p.text_error
        } else if entry.bypassed {
            p.text_faint
        } else {
            p.text
        },
    );
}

/// `╭ Parallel名` 行: 名前 (改名中は text_input) + [x]。
#[allow(clippy::too_many_arguments)]
fn draw_parallel_begin_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    i: usize,
    parallel_id: u64,
    name: &str,
    bypassed: bool,
    color: Option<[f32; 3]>,
    open: bool,
    row: Rect,
    popup_open: bool,
) {
    let p = &app.theme.core;
    let btn_x_w = 26.0;
    let by = row.y + 2.0;
    let right = row.x + row.w - btn_x_w;
    ui.button_at(
        ("inspector_parallel_remove", i),
        "x",
        Rect { x: right, y: by, w: btn_x_w, h: ROW_H - 4.0 },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![parallel_id] });
                }
            })
        },
    );
    // 括弧 (Parallel の色) + 開閉 disclosure + 名前。 折り畳み中は括弧を `╴` にして終了行が無いことを示す。
    let bracket = if open { "\u{256D}" } else { "\u{2574}" };
    ui.label_at(("inspector_parallel_bracket", i), bracket, row.x + 2.0, row.y + 7.0, 12.0, rgb_or(color, p.text_dim));
    draw_disclosure(ui, ("inspector_parallel_disclosure", i), parallel_id, open, row.x + 14.0, row, popup_open, p);
    let name_rect = Rect { x: row.x + 28.0, y: row.y + 3.0, w: (right - 6.0 - row.x - 28.0).max(1.0), h: ROW_H - 6.0 };
    if let Some((id, buf)) = &app.ui_ephemeral.renaming_chain
        && *id == parallel_id
    {
        draw_rename_input(app, ui, ("inspector_parallel_rename", i), name_rect, buf, move |app, text| {
            app.handle_event(AppEvent::RenameParallel { parallel_id, name: text });
        });
    } else {
        ui.label_at_clipped(
            ("inspector_parallel_name", i),
            name,
            Rect { x: name_rect.x, y: row.y + 8.0, w: name_rect.w, h: 11.0 * 1.2 },
            11.0,
            if bypassed { p.text_faint } else { p.text },
        );
    }
}

/// chain 行: [色] ▶ 名前 / preview 四角 / gain knob / pan knob / M / S / x。
#[allow(clippy::too_many_arguments)]
fn draw_chain_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    i: usize,
    parallel_id: u64,
    chain_id: u64,
    name: &str,
    color: Option<[f32; 3]>,
    gain: f32,
    pan: f32,
    muted: bool,
    solo: bool,
    open: bool,
    n_devices: usize,
    row: Rect,
    popup_open: bool,
) {
    let p = &app.theme.core;
    let by = row.y + (ROW_H - CHAIN_BTN_W) * 0.5;
    // 右から: x, S, M, pan, gain。
    let mut right = row.x + row.w - 4.0;
    let toggle_style = toggle_audio_style(&app.theme);
    right -= CHAIN_BTN_W;
    ui.button_at(
        ("inspector_chain_remove", i),
        "x",
        Rect { x: right, y: by, w: CHAIN_BTN_W, h: CHAIN_BTN_W },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::RemoveDevices { device_ids: vec![chain_id] });
                }
            })
        },
    );
    right -= CHAIN_BTN_W + 2.0;
    ui.toggle_button_at(
        ("inspector_chain_solo", i),
        "S",
        Rect { x: right, y: by, w: CHAIN_BTN_W, h: CHAIN_BTN_W },
        solo,
        &toggle_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Solo(v) });
                }
            })
        },
    );
    right -= CHAIN_BTN_W + 2.0;
    ui.toggle_button_at(
        ("inspector_chain_mute", i),
        "M",
        Rect { x: right, y: by, w: CHAIN_BTN_W, h: CHAIN_BTN_W },
        muted,
        &toggle_style,
        move |v| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Muted(v) });
                }
            })
        },
    );
    // knob は automation gesture idiom (mixer の send knob と同じ)。
    let Some(track_id) = app.cursor_track_id() else { return };
    let track = app.song_doc.song().track_by_id(track_id);
    let pan_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainPan { chain_id });
    let gain_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id });
    let live_pan = track.map_or(pan, |t| app.live_param_value(t, &pan_target, pan));
    let live_gain = track.map_or(gain, |t| app.live_param_value(t, &gain_target, gain));
    right -= CHAIN_KNOB + 4.0;
    let was_pan = app.recording.active_param_gestures.contains(&(track_id, pan_target.clone()));
    let pan_resp = ui.knob_at(
        ("inspector_chain_pan", i),
        Rect { x: right, y: row.y + (ROW_H - CHAIN_KNOB) * 0.5, w: CHAIN_KNOB, h: CHAIN_KNOB },
        ((live_pan + 1.0) * 0.5).clamp(0.0, 1.0),
        0.5,
        &KnobStyle { surface: Some(p.panel_raised), ..KnobStyle::BIPOLAR },
        move |v| {
            let pan = v * 2.0 - 1.0;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Pan(pan) });
            })
        },
        None,
    );
    push_param_gesture_edges(ui, track_id, pan_target, "Chain Pan", was_pan, pan_resp.dragging);
    right -= CHAIN_KNOB + 2.0;
    let was_gain = app.recording.active_param_gestures.contains(&(track_id, gain_target.clone()));
    let gain_resp = ui.knob_at(
        ("inspector_chain_gain", i),
        Rect { x: right, y: row.y + (ROW_H - CHAIN_KNOB) * 0.5, w: CHAIN_KNOB, h: CHAIN_KNOB },
        (live_gain * 0.5).clamp(0.0, 1.0),
        0.5,
        &KnobStyle { surface: Some(p.panel_raised), ..KnobStyle::UNIPOLAR },
        move |v| {
            let gain = v * 2.0;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Gain(gain) });
            })
        },
        None,
    );
    push_param_gesture_edges(ui, track_id, gain_target, "Chain Gain", was_gain, gain_resp.dragging);
    // preview 四角 (Bitwig の chain preview: device 数ぶんの小さい四角)。
    let sq = 6.0;
    let n_sq = n_devices.min(6);
    right -= n_sq as f32 * (sq + 2.0) + 4.0;
    for k in 0..n_sq {
        ui.panel(
            ("inspector_chain_preview", i, k),
            Rect { x: right + k as f32 * (sq + 2.0), y: row.y + (ROW_H - sq) * 0.5, w: sq, h: sq },
            p.text_dim,
            1.0,
        );
    }
    // 左: 色帯 (この chain の device 行の帯と同じ x / 幅で、 展開中は下の帯へ繋がる。
    // click で picker、 hit は帯より少し広く) + 開閉 disclosure + 名前。
    let swatch = Rect {
        x: row.x,
        y: row.y,
        w: BAR_W - 1.0,
        h: if open { row.h + ROW_GAP } else { row.h },
    };
    let swatch_hit = Rect { x: row.x, y: row.y, w: 10.0, h: row.h };
    let pointer = ui.pointer();
    let swatch_clicked = pointer.primary_just_released
        && pointer.pos.is_some_and(|(px, py)| swatch_hit.contains(px, py));
    let fill = color
        .map(|rgb| Color { r: rgb[0], g: rgb[1], b: rgb[2], a: 1.0 })
        .unwrap_or(p.text_dim);
    ui.panel(("inspector_chain_swatch_fill", i), swatch, fill, 0.0);
    if swatch_clicked && !popup_open {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.open_color_picker(ColorPickerTarget::ParallelChain(chain_id), swatch);
        }));
    }
    let name_x = row.x + 14.0;
    draw_disclosure(ui, ("inspector_chain_disclosure", i), chain_id, open, name_x, row, popup_open, p);
    let name_rect = Rect { x: name_x + 11.0, y: row.y + 3.0, w: (right - 6.0 - name_x - 11.0).max(1.0), h: ROW_H - 6.0 };
    if let Some((id, buf)) = &app.ui_ephemeral.renaming_chain
        && *id == chain_id
    {
        draw_rename_input(app, ui, ("inspector_chain_rename", i), name_rect, buf, move |app, text| {
            app.handle_event(AppEvent::RenameParallelChain { chain_id, name: text });
        });
    } else {
        ui.label_at_clipped(
            ("inspector_chain_name", i),
            name,
            Rect { x: name_rect.x, y: row.y + 8.0, w: name_rect.w, h: 11.0 * 1.2 },
            11.0,
            if open { p.text } else { p.text_dim },
        );
    }
    let _ = parallel_id;
}

/// Parallel / chain 行の開閉 disclosure (▶ / ▼、`view::disclosure` の規則)。 click で
/// `ToggleParallelNodeCollapsed { id }`。
#[allow(clippy::too_many_arguments)]
fn draw_disclosure(
    ui: &mut Ui<'_, AppData>,
    key: (&'static str, usize),
    id: u64,
    open: bool,
    x: f32,
    row: Rect,
    popup_open: bool,
    p: &daw_ui_core::Palette,
) {
    // 枠も背景も無い glyph だけ (arrangement の group disclosure と同じ): release の hit test。
    let hit = Rect { x: x - 2.0, y: row.y + 4.0, w: 13.0, h: ROW_H - 8.0 };
    let pointer = ui.pointer();
    if !popup_open
        && pointer.primary_just_released
        && let Some((px, py)) = pointer.pos
        && hit.contains(px, py)
    {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ToggleParallelNodeCollapsed { id });
        }));
    }
    ui.label_at(
        key,
        disclosure_glyph(!open, RevealAxis::Block),
        x,
        row.y + 8.0,
        9.0,
        if open { p.accent } else { p.text_dim },
    );
}

fn rgb_or(color: Option<[f32; 3]>, fallback: Color) -> Color {
    color.map_or(fallback, |rgb| Color { r: rgb[0], g: rgb[1], b: rgb[2], a: 1.0 })
}

/// 改名 text_input (初回 show で focus + 全選択)。 Enter / blur で確定、 Esc で取消。
fn draw_rename_input(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: (&'static str, usize),
    rect: Rect,
    buf: &str,
    commit: impl Fn(&mut AppData, String) + Send + Sync + 'static,
) {
    let style = ui.text_input_style();
    let resp = ui.text_input_at_focused(id, rect, buf, &style, |text| {
        Edit::mutate(move |app: &mut AppData| {
            if let Some((_, b)) = app.ui_ephemeral.renaming_chain.as_mut() {
                *b = text;
            }
        })
    });
    let _ = app;
    if resp.committed || resp.blurred {
        let text = resp.committed_text.clone().unwrap_or_else(|| buf.to_string());
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.renaming_chain = None;
            if !text.trim().is_empty() {
                commit(app, text);
            }
        }));
    } else if !resp.focused {
        // Esc 等で focus が外れた = 取消。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.ui_ephemeral.renaming_chain = None;
        }));
    }
}

/// SC パネル: port ごとに `In N: [source ▾] [tap ▾]`。
fn draw_sidechain_panel(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    device_id: u64,
    ports: &[crate::app_types::SidechainPort],
    rect: Rect,
) {
    let p = &app.theme.core;
    let choices = app.sidechain_source_choices();
    let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
    const TAP_POINTS: [TapPoint; 3] = [TapPoint::PreFx, TapPoint::PostFx, TapPoint::PostFader];
    let tap_labels = ["Pre-FX", "Post-FX", "Post-Fdr"];
    let label_w = 34.0;
    let tap_w = 84.0;
    let x0 = rect.x + 8.0;
    let src_x = x0 + label_w;
    let src_w = (rect.w - 8.0 - label_w - tap_w - 10.0).max(40.0);
    let tap_x = src_x + src_w + 4.0;
    for (k, port) in ports.iter().enumerate() {
        let y = rect.y + SC_PAD * 0.5 + k as f32 * SC_PORT_H;
        ui.label_at(("inspector_sc_in", device_id as usize, k), &format!("In {}", port.port + 1), x0, y + 6.0, 11.0, p.text_dim);
        let sel = choices
            .iter()
            .position(|c| c.source == port.source)
            .unwrap_or(0);
        if let Some(picked) = ui.dropdown(
            ("inspector_sc_src", device_id as usize, k),
            Rect { x: src_x, y, w: src_w, h: SC_PORT_H - 2.0 },
            &labels,
            sel,
        ) && let Some(choice) = choices.get(picked)
        {
            let source: Option<TapSource> = choice.source;
            let port_no = port.port;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetSidechainSource { device_id, port: port_no, source });
            }));
        }
        // 自 track の入力を key にする port は Pre-FX 固定 (dropdown を出さない)。
        if port.source == app.cursor_track_id().map(TapSource::Track) {
            ui.label_at(("inspector_sc_tap_fixed", device_id as usize, k), "Pre-FX", tap_x + 6.0, y + 6.0, 11.0, p.text_dim);
            continue;
        }
        let tap_sel = TAP_POINTS.iter().position(|t| *t == port.tap_point).unwrap_or(2);
        if let Some(picked) = ui.dropdown(
            ("inspector_sc_tap", device_id as usize, k),
            Rect { x: tap_x, y, w: tap_w, h: SC_PORT_H - 2.0 },
            &tap_labels,
            tap_sel,
        ) && let Some(&tp) = TAP_POINTS.get(picked)
        {
            let port_no = port.port;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetAuxInputTapPoint { device_id, port: port_no, tap_point: tp });
            }));
        }
    }
}

/// 掴んだ / 右クリックした行が選択に含まれていれば選択全体、 含まれていなければその行だけ
/// (トラックヘッダの右クリックメニューと同じ規則)。 順序は表示順。 chain id は運べないので
/// 除く。
fn carried_device_ids(app: &AppData, rows: &[ChainRow], device_id: u64) -> Vec<u64> {
    if app.selection.selected_device_ids.contains(&device_id) {
        rows.iter()
            .filter_map(ChainRow::drag_id)
            .filter(|id| app.selection.selected_device_ids.contains(id))
            .collect()
    } else {
        vec![device_id]
    }
}

fn apply_device_menu(app: &mut AppData, idx: usize, device_id: u64) {
    let rows = app.chain_rows();
    let ids = carried_device_ids(app, &rows, device_id);
    match idx {
        0 => {
            let bypassed = !app.all_devices_bypassed(&ids);
            app.handle_event(AppEvent::SetDevicesBypassed { device_ids: ids, bypassed });
        }
        1 => app.handle_event(AppEvent::GroupDevices { device_ids: ids }),
        2 => app.copy_devices(ids),
        3 => app.cut_devices(ids),
        // 貼り付け位置は「この device の直前」。 選択をこの device 1 本にしてから
        // **Ctrl+V と同じ経路** を起こす。
        4 => {
            app.set_device_selection(vec![device_id]);
            app.ui_ephemeral.pending_shortcut_injections.push("paste");
        }
        5 => duplicate_after(app, ids, device_id),
        _ => app.handle_event(AppEvent::RemoveDevices { device_ids: ids }),
    }
}

fn apply_parallel_menu(app: &mut AppData, idx: usize, parallel_id: u64) {
    let rows = app.chain_rows();
    let ids = carried_device_ids(app, &rows, parallel_id);
    match idx {
        0 => {
            let bypassed = !app.all_devices_bypassed(&ids);
            app.handle_event(AppEvent::SetDevicesBypassed { device_ids: ids, bypassed });
        }
        1 => app.handle_event(AppEvent::UngroupParallel { parallel_id }),
        2 => {
            let name = app.song_doc.song().parallel_by_id(parallel_id).map(|r| r.name.clone()).unwrap_or_default();
            app.ui_ephemeral.renaming_chain = Some((parallel_id, name));
        }
        3 => app.copy_devices(ids),
        4 => app.cut_devices(ids),
        5 => duplicate_after(app, ids, parallel_id),
        _ => app.handle_event(AppEvent::RemoveDevices { device_ids: ids }),
    }
}

fn apply_chain_menu(app: &mut AppData, idx: usize, parallel_id: u64, chain_id: u64, anchor: Rect) {
    match idx {
        0 => {
            let name = app
                .song_doc
                .song()
                .chain_by_id(chain_id)
                .map(|(_, c)| c.name.clone())
                .unwrap_or_default();
            app.ui_ephemeral.renaming_chain = Some((chain_id, name));
        }
        1 => app.open_color_picker(ColorPickerTarget::ParallelChain(chain_id), anchor),
        2 => app.handle_event(AppEvent::DuplicateParallelChain { chain_id }),
        3 => app.handle_event(AppEvent::AddParallelChain { parallel_id }),
        _ => app.handle_event(AppEvent::RemoveDevices { device_ids: vec![chain_id] }),
    }
}

/// `device_id` の直後に `ids` のコピーを挿す (メニューの「複製」)。
fn duplicate_after(app: &mut AppData, ids: Vec<u64>, device_id: u64) {
    let Some((dest, index)) = app.song_doc.song().find_device(device_id) else {
        return;
    };
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: ids,
        dest,
        dest_index: index as u32 + 1,
        copy: true,
    }));
}

/// 1 行ぶんの描画に要る、buffer 全体で共通の文脈 (drag_list の row closure から呼ぶ)。
struct RowCtx<'a> {
    rows: &'a [ChainRow],
    popup_open: bool,
    cursor_tid: Option<u32>,
    open_dev: Option<u64>,
    panel_h: f32,
    sc_open: Option<u64>,
    sc_ports: &'a [crate::app_types::SidechainPort],
    sc_panel_h: f32,
    area: Rect,
    pad: f32,
    keys_style: ToggleButtonStyle,
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
    match &r.kind {
        ChainRowKind::Plugin(e) => {
            let selected = app.selection.selected_device_ids.contains(&e.device_id);
            draw_row_bg(ui, i, content, selected, hovered, dragging, p);
            draw_plugin_row(app, ui, i, e, content, popup_open, keys_style);
            draw_plugin_expansions(app, ui, ctx, e.device_id, content);
        }
        ChainRowKind::ParallelBegin { parallel_id, name, bypassed, color, open } => {
            let selected = app.selection.selected_device_ids.contains(parallel_id);
            draw_row_bg(ui, i, content, selected, hovered, dragging, p);
            draw_parallel_begin_row(app, ui, i, *parallel_id, name, *bypassed, *color, *open, content, popup_open);
        }
        ChainRowKind::Chain { parallel_id, chain_id, name, color, gain, pan, muted, solo, open, n_devices } => {
            let is_sel = app.selection.selected_device_ids.contains(chain_id);
            draw_row_bg(ui, i, content, is_sel, hovered, false, p);
            draw_chain_row(
                app, ui, i, *parallel_id, *chain_id, name, *color, *gain, *pan, *muted, *solo, *open, *n_devices, content, popup_open,
            );
        }
        ChainRowKind::AddChain { parallel_id } => draw_add_chain_row(ui, i, *parallel_id, content, popup_open),
        ChainRowKind::AddPlugin { chain } => {
            let is_master = cursor_tid == Some(common::model::MASTER_TRACK_ID);
            draw_add_plugin_row(ui, i, *chain, is_master, content, popup_open);
        }
        ChainRowKind::ParallelEnd { color, .. } => {
            // `╰` — Parallel の終端。 左端の丸角の線で括弧を閉じる (Parallel の色)。
            ui.label_at(("inspector_parallel_end", i), "\u{2570}", content.x + 2.0, content.y - 6.0, 12.0, rgb_or(*color, p.text_dim));
        }
    }
}

/// `+ chain` 行 (Parallel の chain 列の末尾)。
fn draw_add_chain_row(ui: &mut Ui<'_, AppData>, i: usize, parallel_id: u64, content: Rect, popup_open: bool) {
    ui.button_at_sized(
        ("inspector_add_chain", i),
        "+ chain",
        Rect { x: content.x + 8.0, y: content.y + 1.0, w: 80.0, h: content.h - 2.0 },
        11.0,
        move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::AddParallelChain { parallel_id });
                }
            })
        },
    );
}

/// `+ Plugin` (master では `+ FX`) 行 — その chain の末尾に picker を開く。
fn draw_add_plugin_row(
    ui: &mut Ui<'_, AppData>,
    i: usize,
    chain: ChainRef,
    is_master: bool,
    content: Rect,
    popup_open: bool,
) {
    ui.button_at(
        ("inspector_add_plugin", i),
        if is_master { "+ FX" } else { "+ Plugin" },
        Rect { x: content.x, y: content.y + 1.0, w: content.w, h: content.h - 2.0 },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::OpenPluginPicker { chain: Some(chain) });
                }
            })
        },
    );
}
