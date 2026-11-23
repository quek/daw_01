//! chain list の Parallel 内 chain の行 (`[色] ▶ 名前 / preview / gain / pan / M / S / x`) と
//! 操作行 (`+ チェーン` / `+ プラグイン`)、 Parallel ヘッダ行 (`parallel_header.rs`) と共有する開閉
//! disclosure / 改名欄。 chain list 本体 (`chain_list.rs`) から切り出したもの (サイズ budget、
//! 不変条件 9)。

use daw_ui_core::{Edit, KnobStyle, Ui, WidgetId};
use daw_ui_renderer::{Color, Rect};

use crate::app::{AppData, AppEvent, ChainRowKind, ColorPickerTarget, ParamSurface};
use crate::event_device::DeviceEvent;
use crate::handler::parallel::ChainMixerEdit;
use crate::view::disclosure::{RevealAxis, disclosure_glyph};
use crate::view::param_gesture::push_param_gesture;
use common::model::{AutomationTarget, ChainRef, TrackBuiltinParam};

use super::chain_list::{BAR_W, ROW_GAP, ROW_H, draw_hidden_failures};
use super::layout::ROW_NAME_X;
use super::{toggle_audio_style, vocab};

/// chain 行の mixer: ミニ knob と M / S。
pub(super) const CHAIN_KNOB: f32 = 18.0;
pub(super) const CHAIN_BTN_W: f32 = 18.0;
/// chain 行の開閉マークと名前の左端 (行の中身の左端から。色見本の右)。`+ チェーン` の文字も名前の左端に揃える。
const CHAIN_DISCLOSURE_X: f32 = 14.0;
const CHAIN_NAME_X: f32 = CHAIN_DISCLOSURE_X + 11.0;

/// chain 行: [色] ▶ 名前 (+ 畳んだ中の読み込み失敗) / preview 四角 / gain knob / pan knob / M / S / x。
/// `kind` は `ChainRowKind::Chain` (それ以外は何も描かない)。
pub(super) fn draw_chain_row(app: &AppData, ui: &mut Ui<'_, AppData>, i: usize, kind: &ChainRowKind, row: Rect, popup_open: bool) {
    let &ChainRowKind::Chain {
        chain_id, ref name, color, gain, pan, muted, solo, open, n_devices, inactive, hidden_failures, ..
    } = kind
    else {
        return;
    };
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
                    app.handle_event(AppEvent::Device(DeviceEvent::RemoveDevices { device_ids: vec![chain_id] }));
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
                    app.handle_event(AppEvent::Device(DeviceEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Solo(v) }));
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
                    app.handle_event(AppEvent::Device(DeviceEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Muted(v) }));
                }
            })
        },
    );
    // knob は automation gesture idiom (mixer の send knob と同じ)。 レーンの置き場は chain の持ち主
    // (master の Parallel なら song 側、 master 行も再生中に追従する)。
    let Some(track_id) = app.cur.song_doc.chain_owner_track(chain_id) else { return };
    let pan_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainPan { chain_id });
    let gain_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::ChainGain { chain_id });
    let scope = app.live_param_scope();
    let live_pan = app.live_param_value_on(&scope, track_id, &pan_target, pan);
    let live_gain = app.live_param_value_on(&scope, track_id, &gain_target, gain);
    right -= CHAIN_KNOB + 4.0;
    let pan_resp = ui.knob_at(
        ("inspector_chain_pan", i),
        Rect { x: right, y: row.y + (ROW_H - CHAIN_KNOB) * 0.5, w: CHAIN_KNOB, h: CHAIN_KNOB },
        ((live_pan + 1.0) * 0.5).clamp(0.0, 1.0),
        0.5,
        &KnobStyle { surface: Some(p.panel_raised), ..KnobStyle::BIPOLAR },
        move |v| {
            let pan = v * 2.0 - 1.0;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Pan(pan) }));
            })
        },
        None,
    );
    push_param_gesture(ui, app, ParamSurface::Rack, track_id, pan_target, pan_resp.dragging);
    right -= CHAIN_KNOB + 2.0;
    let gain_resp = ui.knob_at(
        ("inspector_chain_gain", i),
        Rect { x: right, y: row.y + (ROW_H - CHAIN_KNOB) * 0.5, w: CHAIN_KNOB, h: CHAIN_KNOB },
        (live_gain * 0.5).clamp(0.0, 1.0),
        0.5,
        &KnobStyle { surface: Some(p.panel_raised), ..KnobStyle::UNIPOLAR },
        move |v| {
            let gain = v * 2.0;
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::SetChainMixer { chain_id, edit: ChainMixerEdit::Gain(gain) }));
            })
        },
        None,
    );
    push_param_gesture(ui, app, ParamSurface::Rack, track_id, gain_target, gain_resp.dragging);
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
    let swatch_inside = pointer.pos.is_some_and(|(px, py)| swatch_hit.contains(px, py));
    let swatch_clicked = ui
        .primary_click(WidgetId::ROOT.child((b"inspector_chain_swatch", chain_id)), swatch_inside)
        .clicked;
    let fill = color
        .map(|rgb| Color { r: rgb[0], g: rgb[1], b: rgb[2], a: 1.0 })
        .unwrap_or(p.text_dim);
    ui.panel(("inspector_chain_swatch_fill", i), swatch, fill, 0.0);
    if swatch_clicked && !popup_open {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.open_color_picker(ColorPickerTarget::ParallelChain(chain_id), swatch);
        }));
    }
    let disclosure_x = row.x + CHAIN_DISCLOSURE_X;
    draw_disclosure(ui, ("inspector_chain_disclosure", i), chain_id, open, disclosure_x, row, popup_open, p);
    // 閉じた chain の中で読み込みに失敗した plugin の数 (名前の欄の右端に警告色)。
    let fail_w = draw_hidden_failures(app, ui, ("chain", i), right - 6.0, row.y, hidden_failures);
    let name_rect =
        Rect { x: row.x + CHAIN_NAME_X, y: row.y + 3.0, w: (right - 6.0 - fail_w - row.x - CHAIN_NAME_X).max(1.0), h: ROW_H - 6.0 };
    if let Some((id, buf)) = &app.cur.peph.renaming_chain
        && *id == chain_id
    {
        draw_rename_input(app, ui, ("inspector_chain_rename", i), name_rect, buf, move |app, text| {
            app.handle_event(AppEvent::Device(DeviceEvent::RenameParallelChain { chain_id, name: text }));
        });
    } else {
        // r.md #114: Selector の非アクティブ chain は薄く (アクティブなら展開に依らず明色 =
        // 「今鳴っている chain」 が一目で分かる)。
        let color = match (inactive, open) {
            (true, _) => p.text_faint,
            (false, true) => p.text,
            (false, false) => p.text_dim,
        };
        ui.label_at_clipped(
            ("inspector_chain_name", i),
            name,
            Rect { x: name_rect.x, y: row.y + 8.0, w: name_rect.w, h: 11.0 * 1.2 },
            11.0,
            color,
        );
    }
}

/// Parallel / chain 行の開閉 disclosure (▶ / ▼、`view::disclosure` の規則)。 click で
/// `ToggleParallelNodeCollapsed { id }`。
#[allow(clippy::too_many_arguments)]
pub(super) fn draw_disclosure(
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
    let inside = !popup_open && pointer.pos.is_some_and(|(px, py)| hit.contains(px, py));
    if ui.primary_click(WidgetId::ROOT.child((b"inspector_disclosure", id)), inside).clicked {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::ToggleParallelNodeCollapsed { id }));
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

/// 改名 text_input (初回 show で focus + 全選択)。 Enter / blur で確定、 Esc で取消。
pub(super) fn draw_rename_input(
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
            if let Some((_, b)) = app.cur.peph.renaming_chain.as_mut() {
                *b = text;
            }
        })
    });
    let _ = app;
    if resp.committed || resp.blurred {
        let text = resp.committed_text.clone().unwrap_or_else(|| buf.to_string());
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cur.peph.renaming_chain = None;
            if !text.trim().is_empty() {
                commit(app, text);
            }
        }));
    } else if !resp.focused {
        // Esc 等で focus が外れた = 取消。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.cur.peph.renaming_chain = None;
        }));
    }
}

/// 小さい操作ボタン (`+ チェーン` / chain の中の `+ プラグイン`) の文字。
const SMALL_OP_FONT: f32 = 11.0;

/// 文字幅 + 16px の小さい操作ボタンの矩形 (左端 `x`、操作行の高さ)。
fn small_op_rect(ui: &mut Ui<'_, AppData>, text: &str, x: f32, content: Rect) -> Rect {
    let w = ui.measure_text(text, SMALL_OP_FONT) + 16.0;
    Rect { x, y: content.y + 1.0, w, h: content.h - 2.0 }
}

/// `+ チェーン` 行 (Parallel の chain 列の末尾)。r.md #158 仕様 15: 「もう 1 本の chain 行が入る場所」の形 —
/// chain 行と同じ幅 (色見本の右から行の右端まで、Parallel の括弧の帯は切らない) の枠だけの行に、chain の名前と
/// 同じ左端で「+ チェーン」。chain の中の `+ プラグイン` (device の名前の左端に小さい塗りのボタン) とは、段も
/// 形も違う — 主 (Parallel に chain を足す) と従 (chain に device を足す) が見分けられる。
pub(super) fn draw_add_chain_row(app: &AppData, ui: &mut Ui<'_, AppData>, i: usize, parallel_id: u64, content: Rect, popup_open: bool) {
    let p = &app.theme.core;
    let rect = Rect { x: content.x + BAR_W, y: content.y + 1.0, w: (content.w - BAR_W).max(1.0), h: content.h - 2.0 };
    let inside = !popup_open && ui.pointer().pos.is_some_and(|(px, py)| rect.contains(px, py));
    if ui.primary_click(WidgetId::ROOT.child(("inspector_add_chain", parallel_id)), inside).clicked {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::AddParallelChain { parallel_id }));
        }));
    }
    let hovered = inside && ui.hovers(rect);
    let fill = if hovered { p.control_hover } else { Color::TRANSPARENT };
    ui.panel_with_border(("inspector_add_chain_bg", i), rect, fill, p.border, 1.0, 3.0);
    let line_h = SMALL_OP_FONT * 1.2;
    let text_rect = Rect { x: content.x + CHAIN_NAME_X, y: rect.y + (rect.h - line_h) * 0.5, w: (rect.w - CHAIN_NAME_X).max(1.0), h: line_h };
    ui.label_at_clipped(("inspector_add_chain_text", i), vocab::ADD_CHAIN, text_rect, SMALL_OP_FONT, if hovered { p.text } else { p.text_dim });
}

/// `+ プラグイン` (master では `+ エフェクト`) 行 — その chain の末尾に picker を開く。トラックの chain の末尾
/// (`nested == false`) は全幅 (主)、Parallel の chain の中は device の名前の左端から小さく (従、仕様 15)。
pub(super) fn draw_add_plugin_row(
    ui: &mut Ui<'_, AppData>,
    i: usize,
    chain: ChainRef,
    is_master: bool,
    nested: bool,
    content: Rect,
    popup_open: bool,
) {
    let text = if is_master { vocab::ADD_EFFECT } else { vocab::ADD_PLUGIN };
    let on_click = move || {
        Edit::mutate(move |app: &mut AppData| {
            if !popup_open {
                app.handle_event(AppEvent::OpenPluginPicker { chain: Some(chain) });
            }
        })
    };
    if nested {
        let rect = small_op_rect(ui, text, content.x + ROW_NAME_X, content);
        ui.button_at_sized(("inspector_add_plugin", i), text, rect, SMALL_OP_FONT, on_click);
    } else {
        let rect = Rect { x: content.x, y: content.y + 1.0, w: content.w, h: content.h - 2.0 };
        ui.button_at(("inspector_add_plugin", i), text, rect, on_click);
    }
}
