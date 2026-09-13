//! Par の格子の 1 マス: つまみ + 常時表示の数値欄 / 列見出し / 切り替えボタン。
//! ここは位置決めだけで、つまみの中身は `view::native_device::native_knob_with_value`。

use common::model::{NativeParamId, RackPanelKey};
use daw_ui_core::{Edit, ToggleButtonStyle, Ui};
use daw_ui_renderer::Rect;

use super::PanelCtx;
use super::layout::{HEAD_FONT, KNOB, KNOB_GAP, NUM_H, NUM_W, SWITCH_H, SWITCH_W, column_x};
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
