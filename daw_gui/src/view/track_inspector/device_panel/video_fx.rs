//! 内蔵映像 FX のパラメータ調整パネル (チェーン行の "Par" ボタンで開閉)。
//!
//! `device_panel/mod.rs` が順に呼ぶセクションの 1 つ (contract は同 mod の doc)。
use super::super::*;
use super::PanelCtx;
use crate::event_device::DeviceEvent;

pub(super) fn draw_video_fx_params(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: PanelCtx) -> f32 {
    let p = &app.theme.core;
    let mut y = ctx.y;
    // 内蔵映像 FX のパラメータ調整パネル（チェーン行の "Par" ボタンで開閉）。
    // 各 param を scrubable_number で実レンジ表示 + per-control 変調（Ranged domain で kick→効果）。
    // 値の SSoT は PluginParam lane の default_value（`SetVideoFxParam` が格納）。
    if let Some(view) = app.inspector_video_fx_params(ctx.device_id) {
        let device_id = view.device_id;
        ui.label_at(("inspector_vfx_label", device_id), view.def.name, ctx.x, y, 12.0, p.text);
        y += 18.0;
        let row_w = ctx.w;
        let input_h = 22.0;
        let label_w = 88.0;
        let input_x = ctx.x + label_w;
        let input_w = (row_w - label_w).max(40.0);
        let track_id = view.track_id;
        // 変調の持ち主 (device を持つトラック) はパネルにつき 1 回だけ解決し、全行で使い回す。
        let owner = crate::view::native_device::ParamOwner::resolve(app.cur.song_doc.song(), track_id);
        // 見えている行だけ描き、高さは全行ぶん消費する (plugin の Par と同じ、`Ui::visible_rows`)。
        let pitch = input_h + 4.0;
        let rows_top = y;
        let visible = ui.visible_rows(rows_top, pitch, view.def.params.len());
        #[allow(clippy::cast_precision_loss)]
        let rows_end = rows_top + view.def.params.len() as f32 * pitch;
        for (i, param) in view.def.params.iter().enumerate().take(visible.end).skip(visible.start) {
            #[allow(clippy::cast_precision_loss)]
            let y = rows_top + i as f32 * pitch;
            let value = f64::from(view.values[i]);
            let (min, max) = param.kind.range();
            let (min, max) = (f64::from(min), f64::from(max));
            let default_real = f64::from(param.kind.norm_to_real(param.kind.default_norm()));
            #[allow(clippy::cast_possible_truncation)]
            let sens = (((max - min) / 220.0).max(0.0001)) as f32;
            ui.label_at_clipped(
                (device_id, i, "vfx_label"),
                param.name,
                Rect {
                    x: ctx.x,
                    y: y + 5.0,
                    w: (label_w - 4.0).max(1.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                p.text,
            );
            let style = ScrubableNumberStyle {
                sensitivity: sens,
                range: Some((min, max)),
                ..scrub_style(&app.theme)
            };
            let target = AutomationTarget::PluginParam {
                device_id,
                param_id: param.id,
                legacy_device_index: None,
            };
            let domain = crate::app::ModControlDomain::Ranged { min, max, log: param.kind.is_log() };
            let mod_build = owner.map(|o| build_mod(app, target.clone(), value, domain, o));
            let modulation = mod_build.as_ref().map(|m| m.modulation());
            let param_id = param.id;
            let resp = ui.scrubable_number_at(
                (device_id, i, "vfx_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                value,
                default_real,
                ScrubableNumberFormat::Decimal(3),
                &style,
                move |v| set_video_fx_param_edit(device_id, param_id, v),
                None,
                modulation,
            );
            // drag / text 編集の開始・終了 edge で undo を 1 step に bracket（毎フレームの
            // SetVideoFxParam は非 undoable、stroke 先頭で 1 snapshot）。
            let scrub_key = crate::app::InspectorScrubField::VideoFx { device_id, param_id };
            super::super::push_scrub_bracket(
                ui,
                app,
                scrub_key,
                resp.dragging || resp.editing_text,
            );
            // 変調 depth ドラッグの falling edge で host 再同期（音声 target の depth 反映用）。
            mod_widget::push_mod_depth_bracket(
                ui,
                app,
                crate::app::ParamSurface::Rack,
                track_id,
                &target,
                resp.mod_dragging,
            );
        }
        y = rows_end + 8.0;
    }
    y
}

/// 映像 FX パラメータ 1 本の値変更 `Edit` (スクラブ / 数値入力の途中経過)。
/// stroke の undo bracket は呼び出し側が持つ (`plugin_params.rs` と同 idiom)。
#[allow(clippy::cast_possible_truncation)]
fn set_video_fx_param_edit(device_id: u64, param_id: u32, v: f64) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::Device(DeviceEvent::SetVideoFxParam { device_id, param_id, value_real: v as f32 }));
    })
}
