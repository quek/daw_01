//! 埋め込み GUI を持たない plugin の「Par」インライン param パネル。
//!
//! `device_panel/mod.rs` が順に呼ぶセクションの 1 つ。 contract は
//! `chain_sections.rs` / `modulation_rack.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。
use super::super::*;

pub(super) fn draw_plugin_params(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    // ---- Plugin param panel -----------------------------------
    // 埋め込み GUI を持たない plugin (VOICEVOX builtin / GUI 無し CLAP・VST3) の
    // チェーン行「Par」ボタンで開閉。 VOICEVOX は device 既定の声 (キャラ→スタイル)
    // を、 汎用 plugin は param を scrubable_number で実レンジ編集する。 値の SSoT は
    // PluginParam lane の default_value (= `set_plugin_param`、 映像 FX と同 idiom)。
    if let Some(view) = app.inspector_plugin_params() {
        let device_id = view.device_id;
        let track_id = view.track_id;
        ui.label_at_clipped(
            "inspector_pp_label",
            &view.plugin_name,
            Rect { x: area.x + pad, y, w: (area.w - pad * 2.0).max(1.0), h: 12.0 * 1.2 },
            12.0,
            p.text,
        );
        y += 18.0;

        // 汎用 param 行 (scrubable_number で実レンジ + per-control 変調)。
        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 96.0;
        let input_x = area.x + pad + label_w;
        let input_w = (row_w - label_w).max(40.0);
        for (i, row) in view.params.iter().enumerate() {
            // param 名はプラグイン由来で長さ上限が無い。 label_w(96) を超えると
            // 後続の値ボックス (不透明 bg) に覆われて途中で消えるので rect で切る。
            ui.label_at_clipped(
                (i, "pp_name"),
                &row.name,
                Rect {
                    x: area.x + pad,
                    y: y + 5.0,
                    w: (label_w - 4.0).max(1.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                p.text,
            );
            if row.readonly {
                // 編集不可 param は現値をラベル表示するだけ。
                let txt = format!("{:.3}", row.value_real);
                ui.label_at_clipped(
                    (i, "pp_ro"),
                    &txt,
                    Rect { x: input_x, y: y + 5.0, w: input_w, h: 11.0 * 1.2 },
                    11.0,
                    p.text,
                );
                y += input_h + 4.0;
                continue;
            }
            let (min, max) = (row.min, row.max);
            #[allow(clippy::cast_possible_truncation)]
            let sens = (((max - min) / 220.0).max(0.0001)) as f32;
            let style = ScrubableNumberStyle {
                sensitivity: sens,
                range: Some((min, max)),
                ..scrub_style(&app.theme)
            };
            let target = AutomationTarget::PluginParam {
                device_id,
                param_id: row.id,
                legacy_device_index: None,
            };
            let domain = crate::app::ModControlDomain::Ranged { min, max, log: false };
            let mod_build = build_mod(app, target.clone(), row.value_real, domain, track_id);
            let modulation = Some(mod_build.modulation());
            let param_id = row.id;
            let fmt = if row.stepped {
                ScrubableNumberFormat::Decimal(0)
            } else {
                ScrubableNumberFormat::Decimal(3)
            };
            let resp = ui.scrubable_number_at(
                (i, "pp_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                row.value_real,
                row.default_real,
                fmt,
                &style,
                move |v| set_plugin_param_edit(device_id, param_id, v),
                None,
                modulation,
            );
            // drag / text 編集 stroke を undo 1 step に bracket (`SetPluginParam` は
            // per-frame 非 undoable)。 確定値の host push は不要: scrub 中の edit_song が
            // bump した epoch を runner の frame flush が per-frame で LoadSong する
            // (sync 一本化)。 bracket の実装は inspector 共通の 1 本を通す。
            let scrub_key = crate::app::InspectorScrubField::PluginParam { device_id, param_id };
            super::super::push_scrub_bracket(
                ui,
                app,
                scrub_key,
                resp.dragging || resp.editing_text,
            );
            // 変調 depth ドラッグの falling edge で host 再同期。
            mod_widget::push_mod_depth_bracket(ui, app, track_id, &target, resp.mod_dragging);
            y += input_h + 4.0;
        }
        y += 8.0;
    }
    y
}

/// パラメータ 1 本の値変更 `Edit` (スクラブ / 数値入力の途中経過)。
/// `SetPluginParam` は per-frame 非 undoable で、 stroke の bracket は呼び出し側が持つ。
fn set_plugin_param_edit(device_id: u64, param_id: u32, value_real: f64) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::SetPluginParam { device_id, param_id, value_real });
    })
}
