//! オートメーションの点の値を人間可読単位で見せる / 打つ部品。アレンジのレーンと
//! Automation Editor が共有する。値の表示 / 解釈は `automation_value` の 1 経路。

use common::model::AutomationTarget;
use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Rect, RectCommand};

use crate::app::{AppData, AppEvent, AutomationPointKeyRef};

/// 数値入力欄の画面位置 (点の少し上に中央揃え)。点 dot (8px) より広い。`top` は欄を上へ
/// はみ出させない上限 y。
pub(crate) fn input_rect(point_rect: Rect, top: f32) -> Rect {
    let field_w = 60.0_f32;
    Rect {
        x: point_rect.x + point_rect.w * 0.5 - field_w * 0.5,
        y: (point_rect.y - 22.0).max(top),
        w: field_w,
        h: 18.0,
    }
}

/// 編集中の点の inline 数値入力欄 (点をダブルクリックで開始)。`point_rect` は点の画面位置で、
/// `None` (画面外 / 削除済) なら編集状態を破棄する。`top` は欄を上へはみ出させない上限 y。
pub(crate) fn draw_value_input(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    edit_key: AutomationPointKeyRef,
    point_rect: Option<Rect>,
    top: f32,
) {
    let lane_target = app
        .cur.song_doc.song()
        .automation_lane_by_key(edit_key.track_id, edit_key.lane_id)
        .map(|l| l.target.clone());
    let (Some(rect), Some(target)) = (point_rect, lane_target) else {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.cur.peph.editing_automation_point = None;
        }));
        return;
    };
    let plugin_range = app.plugin_param_range(&target);
    let desc = crate::automation_value::automation_value_display(&target, plugin_range);
    let cur = app.automation_point_value(&edit_key).unwrap_or(0.0);
    let prefill = desc.format_number(cur);
    let edit_resp = ui.text_input_at_focused(
        (
            "automation_point_value",
            edit_key.track_id,
            edit_key.lane_id,
            edit_key.clip_id,
            edit_key.point_idx,
        ),
        input_rect(rect, top),
        &prefill,
        &ui.text_input_style(),
        |_new| Edit::mutate(|_| {}),
    );
    if edit_resp.committed || edit_resp.blurred {
        // Enter / 外クリックで確定。 数値が読めれば plain で上書き、 読めなければ
        // 値は変えずに編集終了。
        let text = edit_resp.committed_text.unwrap_or_default();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            if let Some(plain) = desc.parse_to_plain(&text) {
                app.handle_event(AppEvent::SetAutomationPointValue {
                    key: edit_key,
                    value: plain,
                });
            } else {
                app.cur.peph.editing_automation_point = None;
            }
        }));
    } else if !edit_resp.focused {
        // Esc / focus 喪失 (commit でない) → キャンセル。
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.cur.peph.editing_automation_point = None;
        }));
    }
}

/// 点をドラッグしている間の現値表示 (人間可読単位、 カーソル近傍)。
pub(crate) fn draw_drag_readout(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    target: &AutomationTarget,
    value_norm: f32,
    cursor: (f32, f32),
    top: f32,
) {
    let plugin_range = app.plugin_param_range(target);
    let desc = crate::automation_value::automation_value_display(target, plugin_range);
    let plain = common::automation::norm_to_plain_ranged(target, value_norm, plugin_range);
    let text = desc.format_with_unit(plain);
    let (cx, cy) = cursor;
    let pad = 4.0_f32;
    #[allow(clippy::cast_precision_loss)]
    let w = (text.chars().count() as f32) * 7.0 + pad * 2.0;
    let label_rect = Rect {
        x: cx + 10.0,
        y: (cy - 20.0).max(top),
        w,
        h: 16.0,
    };
    // 読み値は不透明に近いクローム面 (`panel`) のチップに載せる = 背景は
    // パレット自身の面なので、 文字は極性固定インクでなく `text` でよい。
    let p = &app.theme.core;
    ui.push_rect(RectCommand {
        rect: label_rect,
        fill: p.panel.with_alpha(0.92),
        border: p.border,
        border_width: 1.0,
        radius: [3.0; 4],
        clip_rect: None,
    });
    ui.label_at(
        "automation_point_drag_readout",
        &text,
        label_rect.x + pad,
        label_rect.y + 2.0,
        11.0,
        p.text,
    );
}
