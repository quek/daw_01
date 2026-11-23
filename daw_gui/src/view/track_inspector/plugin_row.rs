//! chain list の plugin 行 (`名前 [オフ] … [Out▾] [SC▾] [⌨] [Par|GUI|再読込] [Mix] [x]`) と、 その直下の展開
//! (読み込み失敗の理由 / SC パネル / 出力 (パラアウト) のパネル / param パネル)。 chain list 本体
//! (`chain_list.rs`) から切り出したもの (サイズ budget、 不変条件 9)。
//!
//! r.md #158 仕様 2: トリガから離れて出ていたもの (チェーンの下の「読み込み失敗」「Parallel Out」の
//! セクション) は行へ戻した — 失敗は行の `[再読込]` と直下の理由 1 行、出力は行の `Out▾` で直下に開く。
//! 展開の中身は行の名前と同じ左端から (`layout::expansion_inset`)、欄はラベル左・値右 (`form`)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ChainEntry};
use crate::event_device::DeviceEvent;
use common::model::{RackPanelKey, TapPoint, TapSource};

use super::chain_list::{ROW_H, RowCtx, draw_row_name, panel_height};
use super::layout::{
    FAIL_REASON_H, FORM_GAP, FORM_ROW_PITCH, LABEL_FONT, PAR_PAD, ROW_NAME_X, expansion_inset, expansion_rect, form_block_h,
};
use super::mix_knob::MIX_COL_W;
use super::native_row::claim_expansion_press;
use super::{aux_out_panel, device_panel, form, vocab};

/// SC パネルの高さ (port ごとに欄 1 行)。
pub(super) fn sc_panel_height(ports: usize) -> f32 {
    form_block_h(ports)
}

/// 行幅いっぱいの展開 `full` (SC / 出力) の欄の列: ラベルは行の名前の左端から、値の欄は帯の欄と同じ x。
pub(super) fn expansion_cols(ctx: &RowCtx<'_>, full: Rect) -> form::FormCols {
    let inner = expansion_inset(full);
    form::cols_nested(inner.x, inner.w, ctx.form_left)
}

/// plugin 行の直下の展開 (上から 読み込み失敗の理由 / SC / 出力 / Par。行高は
/// `chain_list::build_list_rows` が同じ順で足す)。
pub(super) fn draw_plugin_expansions(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &RowCtx<'_>, entry: &ChainEntry, content: Rect) {
    let device_id = entry.device_id;
    // 行の直下に先に積んだ展開の高さ (y は `layout::expansion_rect` の 1 本で決める)。
    let mut above = 0.0;
    let full = |above: f32, h: f32| expansion_rect(content, above, h);
    if let Some(reason) = entry.load_error.as_deref() {
        // 理由は 1 行で切る (長い FFI エラーでも行が伸びない)。全文は status bar / ログに出ている。
        let h = LABEL_FONT * 1.2;
        let line = expansion_inset(full(above, FAIL_REASON_H));
        let rect = Rect { y: line.y + (FAIL_REASON_H - h) * 0.5 - 2.0, h, ..line };
        ui.label_at_clipped(("inspector_row_fail_reason", device_id), reason, rect, LABEL_FONT, app.theme.core.text_dim);
        above += FAIL_REASON_H;
    }
    // 展開を 2 つ以上開いたら、2 つ目からは上端に細い罫線 (出力のパネルの下に続く Par の口パク先が、出力の
    // 欄の続きに見えない)。
    let mut stacked = false;
    if ctx.sc_open == Some(device_id) && ctx.sc_panel_h > 0.0 {
        let rect = full(above, ctx.sc_panel_h);
        claim_expansion_press(ui, rect, ("sc", device_id), ctx.popup_open);
        draw_sidechain_panel(app, ui, device_id, ctx.sc_ports, &expansion_cols(ctx, rect), rect.y);
        above += ctx.sc_panel_h;
        stacked = true;
    }
    if let Some(aux) = ctx.aux_entry.filter(|a| a.device_id == device_id) {
        let rect = full(above, ctx.aux_panel_h);
        claim_expansion_press(ui, rect, ("aux_out", device_id), ctx.popup_open);
        if stacked {
            expansion_rule(app, ui, ("aux_out", device_id), rect);
        }
        aux_out_panel::draw(app, ui, aux, &expansion_cols(ctx, rect), rect.y);
        above += ctx.aux_panel_h;
        stacked = true;
    }
    let key = RackPanelKey::Device(device_id);
    if app.rack_panel_open(key) {
        let rect = full(above, panel_height(app, device_id));
        claim_expansion_press(ui, rect, ("par", key), ctx.popup_open);
        if stacked {
            expansion_rule(app, ui, ("par", key), rect);
        }
        let inner = expansion_inset(rect);
        let panel = device_panel::PanelCtx { device_id, x: inner.x, w: inner.w, y: inner.y, form_left: ctx.form_left };
        let measured = (device_panel::draw_device_panel(app, ui, panel) - inner.y).max(0.0);
        // 展開部の実消費高を device ごとに測って次フレームの行高に使う (lag-by-one)。
        if app.cur.peph.rack_panel_heights.get(&key).is_none_or(|h| (h - measured).abs() > 0.5) {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.cur.peph.rack_panel_heights.insert(key, measured);
            }));
        }
    }
}

/// 行の直下に重ねて開いた展開の境目 (2 つ目以降の展開の上端、中身と同じ左端から行の右端まで)。
pub(super) fn expansion_rule(app: &AppData, ui: &mut Ui<'_, AppData>, key: impl std::hash::Hash, full: Rect) {
    let inner = expansion_inset(full);
    ui.panel(("inspector_expansion_rule", key), Rect { h: 1.0, ..inner }, app.theme.core.border, 0.0);
}

/// 右端からボタンを詰めて置く列 (行の上端 `y` から 2px、高さは行 - 4)。
struct ButtonColumn {
    right: f32,
    y: f32,
}

impl ButtonColumn {
    /// 幅 `w` のボタンの矩形を取り、列を左へ進める。
    fn take(&mut self, w: f32) -> Rect {
        self.right -= w + 2.0;
        Rect { x: self.right, y: self.y, w, h: ROW_H - 4.0 }
    }
}

const BTN_GUI_W: f32 = 44.0;
const BTN_X_W: f32 = 26.0;
const BTN_KEYS_W: f32 = 26.0;
const BTN_SC_W: f32 = 34.0;

/// plugin 行: 名前 (+ オフの札) + [Out▾] [SC▾] [⌨] [Par | GUI | 再読込] [Mix] [x]。
pub(super) fn draw_plugin_row(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &RowCtx<'_>, i: usize, entry: &ChainEntry, row: Rect) {
    let p = &app.theme.core;
    let device_id = entry.device_id;
    let popup_open = ctx.popup_open;
    // [x]
    let x_rect = Rect { x: row.x + row.w - BTN_X_W, y: row.y + 2.0, w: BTN_X_W, h: ROW_H - 4.0 };
    ui.button_at(("inspector_row_remove", i), "x", x_rect, move || {
        Edit::mutate(move |app: &mut AppData| {
            if !popup_open {
                app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![device_id] }));
            }
        })
    });
    // Mix の列 (ノブは chain list が描く。音声を通さない device でも列は空ける)。
    let mut col = ButtonColumn { right: x_rect.x - MIX_COL_W, y: row.y + 2.0 };
    let failed = entry.load_error.is_some();
    if failed {
        // 読み込みに失敗した device は host に instance が無い (= 無音、窓も param も無い)。Par / GUI の位置に
        // 再読込を置く (自動リトライはしない: 恒常的な失敗で無限ループになるため、再試行はユーザーの意思で)。
        // 幅は文言に合わせる (Par / GUI の幅では全角 3 文字が「…」で切れる)。
        let font = ui.control_font_size();
        let w = BTN_GUI_W.max(ui.measure_text(vocab::RELOAD, font) + 12.0);
        ui.button_at(("inspector_row_reload", i), vocab::RELOAD, col.take(w), move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::Device(DeviceEvent::ReloadDevice { device_id }));
                }
            })
        });
    } else if entry.shows_button() && !(ctx.disabled && !entry.is_video && !entry.is_voicevox) {
        // [Par | GUI]。r.md #131: 無効トラックでは host の instance を要る窓 / param パネルは出さない
        // (Song だけで完結する映像 FX / 声のパネルは開ける)。
        let label = if entry.shows_param_panel() { "Par" } else { "GUI" };
        ui.button_at(("inspector_row_gui", i), label, col.take(BTN_GUI_W), move || {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::Device(DeviceEvent::ToggleSlotGui { device_id }));
                }
            })
        });
    }
    // [⌨] (埋め込みエディタ窓を開く device だけ)
    if !failed && entry.has_embedded_gui && !entry.shows_param_panel() {
        let next = !entry.send_all_keys;
        ui.toggle_button_at(("inspector_row_keys", i), "\u{2328}", col.take(BTN_KEYS_W), entry.send_all_keys, &ctx.keys_style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    app.handle_event(AppEvent::Device(DeviceEvent::SetPluginSendAllKeys { device_id, enabled: next }));
                }
            })
        });
    }
    // [SC] (aux 入力 port を持つ plugin だけ)。 配線済みは ON 色。
    if entry.aux_input_count > 0 {
        let open = app.cur.peph.open_sidechain_panel == Some(device_id);
        let label = if open { "SC\u{25B4}" } else { "SC\u{25BE}" };
        ui.toggle_button_at(("inspector_row_sc", i), label, col.take(BTN_SC_W), entry.sc_wired || open, &ctx.keys_style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    let cur = &mut app.cur.peph.open_sidechain_panel;
                    *cur = if *cur == Some(device_id) { None } else { Some(device_id) };
                }
            })
        });
    }
    // [Out] (aux 出力 port を持つ plugin だけ。マスターは子トラックへ展開できないので出さない)。出すかどうかは
    // 開いたパネル (`AppData::aux_out_entry`) と同じ `aux_out_allowed_on` の 1 本、配線済み (展開済み、ON 色) も
    // パネルと同じ `wired_aux_outputs` の 1 本 (規則を 2 か所に書かない)。行は model を引き直さない。
    if entry.aux_output_count > 0 && ctx.aux_out_allowed {
        let open = app.cur.peph.open_aux_out_panel == Some(device_id);
        let label = if open { "Out\u{25B4}" } else { "Out\u{25BE}" };
        ui.toggle_button_at(("inspector_row_aux_out", i), label, col.take(BTN_SC_W + 6.0), entry.aux_out_wired || open, &ctx.keys_style, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                if !popup_open {
                    let cur = &mut app.cur.peph.open_aux_out_panel;
                    *cur = if *cur == Some(device_id) { None } else { Some(device_id) };
                }
            })
        });
    }
    // 名前 (ボタンの手前で打ち切る)。失敗は警告色、無効トラック (r.md #131) は使えない色、バイパスは札。
    let display_name: std::borrow::Cow<'_, str> = if failed {
        format!("{} {}", vocab::NOT_LOADED, entry.plugin_name).into()
    } else {
        entry.plugin_name.as_str().into()
    };
    let name_x = row.x + ROW_NAME_X;
    let color = if failed {
        p.text_error
    } else if ctx.disabled {
        p.text_faint
    } else {
        p.text
    };
    let off = entry.bypassed && !failed && !ctx.disabled;
    draw_row_name(app, ui, i, &display_name, name_x, row.y, (col.right - 6.0 - name_x).max(1.0), color, off);
}

/// SC パネル: port ごとに `入力 N [source ▾] [tap ▾]` (plugin と内蔵 Comp / Bus Comp 共通)。`c` は欄の列
/// ([`expansion_cols`])、`top` はパネルの上端。
pub(super) fn draw_sidechain_panel(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    device_id: u64,
    ports: &[crate::app_types::SidechainPort],
    c: &form::FormCols,
    top: f32,
) {
    let choices = app.sidechain_source_choices(device_id);
    let labels: Vec<&str> = choices.iter().map(|c| c.label.as_str()).collect();
    const TAP_POINTS: [TapPoint; 3] = [TapPoint::PreFx, TapPoint::PostFx, TapPoint::PostFader];
    let tap_labels = ["Pre-FX", "Post-FX", "Post-Fdr"];
    let tap_w = 84.0;
    let src_w = (c.value_w - tap_w - FORM_GAP).max(40.0);
    for (k, port) in ports.iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let y = top + PAR_PAD + k as f32 * FORM_ROW_PITCH;
        form::label(ui, app, ("inspector_sc_in", device_id, k), &vocab::sc_port(port.port + 1), c, y);
        let sel = choices.iter().position(|c| c.source == port.source).unwrap_or(0);
        if let Some(picked) = ui.dropdown(("inspector_sc_src", device_id, k), Rect { w: src_w, ..c.value(y) }, &labels, sel)
            && let Some(choice) = choices.get(picked)
        {
            let source: Option<TapSource> = choice.source;
            let port_no = port.port;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::SetSidechainSource { device_id, port: port_no, source }));
            }));
        }
        let tap_rect = Rect { x: c.value_x + src_w + FORM_GAP, w: tap_w, ..c.value(y) };
        // 自 track の入力を key にする port は Pre-FX 固定 (dropdown を出さない)。
        if port.source == app.cursor_track_id().map(TapSource::Track) {
            let h = LABEL_FONT * 1.2;
            let r = Rect { x: tap_rect.x + 6.0, y: y + (tap_rect.h - h) * 0.5, h, ..tap_rect };
            ui.label_at_clipped(("inspector_sc_tap_fixed", device_id, k), "Pre-FX", r, LABEL_FONT, app.theme.core.text_dim);
            continue;
        }
        let tap_sel = TAP_POINTS.iter().position(|t| *t == port.tap_point).unwrap_or(2);
        if let Some(picked) = ui.dropdown(("inspector_sc_tap", device_id, k), tap_rect, &tap_labels, tap_sel)
            && let Some(&tp) = TAP_POINTS.get(picked)
        {
            let port_no = port.port;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::SetAuxInputTapPoint { device_id, port: port_no, tap_point: tp }));
            }));
        }
    }
}
