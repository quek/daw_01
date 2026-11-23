//! 出力 (パラアウト、`docs/plan_paraout.md`) のパネル — aux 出力を持つ plugin の行の `Out▾` で、行の直下に開く
//! (r.md #158 仕様 2: 旧「Parallel Out」セクションはチェーンの下にあってトリガから離れていた)。
//!
//! 1 行目が「子トラック [展開]」(子トラックを自動で作って出力を割り当てる)、展開済みなら port ごとに
//! 「出力 N [宛先 ▾]」。行数の上限は無い (高さは [`panel_height`] が port 数から決め、chain list の行高に足す)。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ParallelOutputEntry};
use crate::event_device::DeviceEvent;

use super::layout::{FORM_ROW_PITCH, PAR_PAD, form_block_h};
use super::{form, vocab};

/// 展開ボタンの幅。
const EXPLODE_W: f32 = 64.0;

/// パネルの高さ (展開の行 + 展開済みなら port ごとに 1 行)。
pub(super) fn panel_height(entry: &ParallelOutputEntry) -> f32 {
    let ports = if entry.exploded { entry.aux_output_count as usize } else { 0 };
    form_block_h(1 + ports)
}

/// パネルの中身を描く (`c` は欄の列 = `plugin_row::expansion_cols`、`top` はパネルの上端)。
pub(super) fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, entry: &ParallelOutputEntry, c: &form::FormCols, top: f32) {
    let device_id = entry.device_id;
    let c = *c;
    let mut y = top + PAR_PAD;
    form::label(ui, app, ("inspector_po_explode_label", device_id), vocab::FIELD_CHILD_TRACKS, &c, y);
    ui.button_at(("inspector_po_explode", device_id), vocab::EXPLODE_OUTPUTS, Rect { w: EXPLODE_W, ..c.value(y) }, move || {
        Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::ExplodeParallelOut { device_id }));
        })
    });
    y += FORM_ROW_PITCH;
    // port ごとの宛先 (展開済みのときだけ)。
    if !entry.exploded {
        return;
    }
    let choices = app.paraout_dest_choices();
    let labels: Vec<&str> = choices.iter().map(|c| c.0.as_str()).collect();
    for port in 0..entry.aux_output_count as usize {
        form::label(ui, app, ("inspector_po_outlabel", device_id, port), &vocab::aux_out_port(port + 1), &c, y);
        let selected = match entry.routes.get(port).and_then(|o| *o) {
            None => 0,
            Some(dest) => choices.iter().position(|c| c.1 == Some(dest)).unwrap_or(0),
        };
        if let Some(picked) = ui.dropdown(("inspector_po_dropdown", device_id, port), c.value(y), &labels, selected)
            && let Some(choice) = choices.get(picked)
        {
            let dest = choice.1;
            #[allow(clippy::cast_possible_truncation)]
            let port = port as u8;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::Device(DeviceEvent::SetParallelOutputRoute { device_id, port, dest }));
            }));
        }
        y += FORM_ROW_PITCH;
    }
}
