//! Par の格子の 1 マス: つまみ + 常時表示の数値欄 / 列見出し / 切り替えボタン。
//! ここは位置決めだけで、つまみの中身は `view::native_device::native_knob_with_value`。

use common::model::{NativeParamId, RackPanelKey};
use daw_ui_core::{Edit, ToggleButtonStyle, Ui};
use daw_ui_renderer::Rect;

use super::PanelCtx;
use super::layout::{
    HEAD_FONT, KNOB, KNOB_GAP, NUM_H, NUM_W, SELECTOR_LABEL_W, SELECTOR_W, SWITCH_H, SWITCH_W, column_x,
};
use crate::app::{AppData, AppEvent, ParamSurface};
use crate::event_device::DeviceEvent;
use crate::view::native_device::{NativeKnobResponse, NativeKnobSpec, native_knob_with_value, wid};

/// 列 `col` の段 `y` にパラメーター `param` のつまみと数値欄を置く。`external_drag` はカーブ点 /
/// ホイールで同じ param を動かしている状態 (ジェスチャーは部品の中で 1 本に OR される)。
#[allow(clippy::too_many_arguments)]
pub(super) fn param_cell(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    ctx: &PanelCtx<'_>,
    param: NativeParamId,
    col: usize,
    y: f32,
    dimmed: bool,
    external_drag: bool,
) -> NativeKnobResponse {
    let knob = Rect { x: column_x(ctx.rect, col, KNOB), y, w: KNOB, h: KNOB };
    let value = Rect { x: column_x(ctx.rect, col, NUM_W), y: y + KNOB + KNOB_GAP, w: NUM_W, h: NUM_H };
    let spec = NativeKnobSpec {
        surface: ParamSurface::Rack,
        owner: ctx.owner,
        device: ctx.dev,
        param,
        rect: knob,
        surface_bg: ctx.bg,
        dimmed,
        external_drag,
        scope: ctx.scope,
    };
    native_knob_with_value(app, ui, &spec, value)
}

/// 段 `y` に列見出しを左の列から並べる (各列の中央)。
pub(super) fn head_labels(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>, key: RackPanelKey, y: f32, labels: &[&str]) {
    let color = app.theme.core.text_dim;
    for (col, label) in labels.iter().enumerate() {
        let w = ui.measure_text(label, HEAD_FONT);
        let x = column_x(ctx.rect, col, w);
        ui.label_at(wid(ParamSurface::Rack, key, "head", col), label, x, y, HEAD_FONT, color);
    }
}

/// 列 `col` の段 `y` (高さ `cell_h` の中央) に切り替えボタンを置く。押すと `event`。
#[allow(clippy::too_many_arguments)]
pub(super) fn switch(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    key: RackPanelKey,
    part: &'static str,
    col: usize,
    y: f32,
    cell_h: f32,
    label: &str,
    lit: bool,
    event: DeviceEvent,
) {
    let style = switch_style(app);
    let r = Rect { x: column_x(rect, col, SWITCH_W), y: y + (cell_h - SWITCH_H) * 0.5, w: SWITCH_W, h: SWITCH_H };
    ui.toggle_button_at(wid(ParamSurface::Rack, key, part, col), label, r, lit, &style, move |_| {
        Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::Device(event)))
    });
}

/// Par の中の小さな切り替えボタンの見た目 (font 10、角丸小)。
pub(super) fn switch_style(app: &AppData) -> ToggleButtonStyle {
    ToggleButtonStyle { radius: 3.0, font_size: 10.0, ..ToggleButtonStyle::from_palette(&app.theme.core) }
}

/// セクションの小見出し (`TIME ────`)。`right` は右端に右寄せで出す補助表示
/// (Delay の実効 ms など)。ラベルの右から右端まで細い罫線を引く。
#[allow(clippy::too_many_arguments)]
pub(super) fn section_label(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    ctx: &PanelCtx<'_>,
    key: RackPanelKey,
    part: &'static str,
    y: f32,
    label: &str,
    right: Option<&str>,
) {
    let color = app.theme.core.text_dim;
    let (x0, right_edge) = (ctx.rect.x, ctx.rect.x + ctx.rect.w);
    let label_w = ui.measure_text(label, HEAD_FONT);
    ui.label_at(wid(ParamSurface::Rack, key, part, 0), label, x0, y, HEAD_FONT, color);
    let rule_end = match right {
        Some(text) => {
            let w = ui.measure_text(text, HEAD_FONT);
            let x = (right_edge - w).max(x0 + label_w + 8.0);
            ui.label_at(wid(ParamSurface::Rack, key, part, 1), text, x, y, HEAD_FONT, color);
            x - 6.0
        }
        None => right_edge,
    };
    let rule_x = x0 + label_w + 6.0;
    if rule_end > rule_x {
        let r = Rect { x: rule_x, y: y + HEAD_FONT * 0.5, w: rule_end - rule_x, h: 1.0 };
        ui.panel(wid(ParamSurface::Rack, key, part, 2), r, color, 0.0);
    }
}

/// 段階式パラメータの小さなセレクタ (`Pat [Stereo]`)。押すと次の段へ (末尾で先頭に戻る)。
///
/// 値は [`NativeParamId`] のまま (`ParamRange::Stepped`) なので、オートメーション・変調・
/// MIDI Learn の的としては連続パラメータと同じ。ここは**見せ方だけ**が違う。
#[allow(clippy::too_many_arguments)]
pub(super) fn selector(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    ctx: &PanelCtx<'_>,
    key: RackPanelKey,
    part: &'static str,
    x: f32,
    y: f32,
    label: &str,
    param: NativeParamId,
    steps: &'static [&'static str],
) {
    let color = app.theme.core.text_dim;
    ui.label_at(wid(ParamSurface::Rack, key, part, 0), label, x, y + 3.0, HEAD_FONT, color);
    let cur = ctx.live.param(param).unwrap_or(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let idx = (cur.round().max(0.0) as usize).min(steps.len().saturating_sub(1));
    let next = (idx + 1) % steps.len().max(1);
    let id = ctx.dev.id;
    let r = Rect { x: x + SELECTOR_LABEL_W, y, w: SELECTOR_W, h: SWITCH_H };
    let style = switch_style(app);
    ui.toggle_button_at(wid(ParamSurface::Rack, key, part, 1), steps[idx], r, false, &style, move |_| {
        Edit::mutate(move |app: &mut AppData| {
            #[allow(clippy::cast_precision_loss)]
            let v = next as f32;
            app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit {
                device_id: id,
                edit: crate::event_native::NativeEdit::param(param, v),
            }));
        })
    });
}

/// ON/OFF パラメータの切り替えボタン (`[Sync]`)。押すと反転する。
#[allow(clippy::too_many_arguments)]
pub(super) fn param_switch(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    ctx: &PanelCtx<'_>,
    key: RackPanelKey,
    part: &'static str,
    x: f32,
    y: f32,
    label: &str,
    param: NativeParamId,
) {
    let lit = ctx.live.param(param).is_some_and(|v| v >= 0.5);
    let id = ctx.dev.id;
    let r = Rect { x, y, w: SWITCH_W, h: SWITCH_H };
    let style = switch_style(app);
    ui.toggle_button_at(wid(ParamSurface::Rack, key, part, 0), label, r, lit, &style, move |_| {
        Edit::mutate(move |app: &mut AppData| {
            let v = if lit { 0.0 } else { 1.0 };
            app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit {
                device_id: id,
                edit: crate::event_native::NativeEdit::param(param, v),
            }));
        })
    });
}
