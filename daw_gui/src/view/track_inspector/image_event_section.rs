//! インスペクタの「Image Event」 セクション (`docs/plan_image_overlay.md` §4 P4)。
//!
//! contract は `chain_sections.rs` / `launch_section.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。 `track_inspector/mod.rs::draw` から
//! 切り出したもの (サイズ budget、 不変条件 9)。 1 関数に収めると関数 budget (300 行) を
//! 超えるので、 PiP 行 ([`draw_pip_rows`]) と Fade 行 ([`draw_fade_rows`]) を分けてある。

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{
    AppData, AppEvent, DiscreteClipEdit, FadeEdgeKind, InspectorImageEventSummary,
    InspectorScrubField,
};
use common::model::ImageBuiltinParam;

use super::{
    FADE_CURVE_LABELS, fade_curve_from_index, fade_curve_to_index, scrub_field, scrub_style,
    toggle_audio_style, toggle_automate_style,
};

/// セクション内の行が揃える列 (PiP 行と Fade 行で共有)。
struct Cols {
    row_w: f32,
    input_h: f32,
    label_w: f32,
    input_x: f32,
    input_w: f32,
    auto_btn_w: f32,
    auto_btn_x: f32,
}

/// selected_clip が `ClipContent::Image` のとき、 first event の
/// 数値入力 (x/y/w/h/opacity) と fade / mute toggle を表示。 編集
/// AppEvent は全 ImageEvent に broadcast。
pub(super) fn draw_image_event_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let Some(summary) = app.inspector_image_event_summary() else {
        return y;
    };
    // edit buffer の target が現選択と違ければ resync を発火 (audio
    // section と同 idiom)。 image clip 切替後に 1 frame だけ古い
    // buffer が表示されるが、 直後の frame で formatted な現値に
    // 書き戻る。
    if app.cur.peph.clip_edit_buffer_target != Some(summary.target) {
        let target = summary.target;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ResyncClipEditBuffers(target));
        }));
    }

    ui.label_at(
        "inspector_image_event_label",
        "Image Event",
        area.x + pad,
        y,
        12.0,
        p.text,
    );
    y += 18.0;

    let row_w = area.w - pad * 2.0;
    let input_h = 22.0;
    let label_w = 50.0;
    // Image PiP 行は automate ボタンを追加 (= label + input + 「A」 btn)。
    // 「A」 btn は click で同 track に lane を追加 (`docs/plan_image_
    // automation.md` §4.1)。 既に lane があれば visible 復活のみ。
    let auto_btn_w = 22.0;
    let auto_btn_gap = 4.0;
    let input_x = area.x + pad + label_w;
    let input_w = row_w - label_w - auto_btn_w - auto_btn_gap;
    let auto_btn_x = input_x + input_w + auto_btn_gap;

    // Mute toggle (image 用 1 個だけ。 audio の Reverse は無い)。
    let toggle_h = 24.0;
    let new_mute = !summary.muted;
    ui.toggle_button_at(
        "inspector_image_mute",
        "Mute",
        Rect { x: area.x + pad, y, w: row_w, h: toggle_h },
        summary.muted,
        &toggle_audio_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::Muted(new_mute),
                })
            })
        },
    );
    y += toggle_h + 8.0;

    let cols = Cols { row_w, input_h, label_w, input_x, input_w, auto_btn_w, auto_btn_x };
    y = draw_pip_rows(app, ui, area, pad, &summary, &cols, y);
    y = draw_fade_rows(app, ui, area, pad, &summary, &cols, y);

    // r.md #98: Flip H / Flip V (節の末尾、 Mute と同じ toggle 部品 + 同じ broadcast
    // 経路)。 2 つ横並びで 1 行。
    let flip_w = (row_w - 4.0) * 0.5;
    let new_flip_h = !summary.flip_h;
    ui.toggle_button_at(
        "inspector_image_flip_h",
        "Flip H",
        Rect { x: area.x + pad, y, w: flip_w, h: toggle_h },
        summary.flip_h,
        &toggle_audio_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::ImageFlipH(new_flip_h),
                })
            })
        },
    );
    let new_flip_v = !summary.flip_v;
    ui.toggle_button_at(
        "inspector_image_flip_v",
        "Flip V",
        Rect { x: area.x + pad + flip_w + 4.0, y, w: flip_w, h: toggle_h },
        summary.flip_v,
        &toggle_audio_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::ImageFlipV(new_flip_v),
                })
            })
        },
    );
    y + toggle_h + 12.0
}

/// PiP rect / opacity / rotation の行 (数値欄 + automate `A` ボタン)。
fn draw_pip_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    summary: &InspectorImageEventSummary,
    cols: &Cols,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let Cols { input_h, input_x, input_w, auto_btn_w, auto_btn_x, .. } = *cols;
    // PiP rect / opacity の scrubable は 0..1 normalized、 細かい step。
    let style_unit = ScrubableNumberStyle {
        sensitivity: 0.004,
        range: Some((0.0, 1.0)),
        ..scrub_style(&app.theme)
    };

    // X
    ui.label_at(
        "inspector_image_x_label",
        "X",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_image_x_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.image_first_event(t, |e| f64::from(e.x))),
        0.0,
        ScrubableNumberFormat::Decimal(3),
        &style_unit,
        InspectorScrubField::ImageX,
        move |t, v| AppEvent::SetClipImageX { target: t, value: v as f32 },
    );
    let x_auto_on = summary.x_automated;
    ui.toggle_button_at(
        "inspector_image_x_automate",
        "A",
        Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
        x_auto_on,
        &toggle_automate_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let ev = if x_auto_on {
                    AppEvent::RemoveImageAutomationLane { field: ImageBuiltinParam::X }
                } else {
                    AppEvent::AddImageAutomationLane { field: ImageBuiltinParam::X }
                };
                app.handle_event(ev);
            })
        },
    );
    y += input_h + 4.0;

    // Y
    ui.label_at(
        "inspector_image_y_label",
        "Y",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_image_y_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.image_first_event(t, |e| f64::from(e.y))),
        0.0,
        ScrubableNumberFormat::Decimal(3),
        &style_unit,
        InspectorScrubField::ImageY,
        move |t, v| AppEvent::SetClipImageY { target: t, value: v as f32 },
    );
    let y_auto_on = summary.y_automated;
    ui.toggle_button_at(
        "inspector_image_y_automate",
        "A",
        Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
        y_auto_on,
        &toggle_automate_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let ev = if y_auto_on {
                    AppEvent::RemoveImageAutomationLane { field: ImageBuiltinParam::Y }
                } else {
                    AppEvent::AddImageAutomationLane { field: ImageBuiltinParam::Y }
                };
                app.handle_event(ev);
            })
        },
    );
    y += input_h + 4.0;

    // W
    ui.label_at(
        "inspector_image_w_label",
        "W",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_image_w_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.image_first_event(t, |e| f64::from(e.w))),
        f64::from(summary.w),
        ScrubableNumberFormat::Decimal(3),
        &style_unit,
        InspectorScrubField::ImageW,
        move |t, v| AppEvent::SetClipImageW { target: t, value: v as f32 },
    );
    let w_auto_on = summary.w_automated;
    ui.toggle_button_at(
        "inspector_image_w_automate",
        "A",
        Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
        w_auto_on,
        &toggle_automate_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let ev = if w_auto_on {
                    AppEvent::RemoveImageAutomationLane { field: ImageBuiltinParam::W }
                } else {
                    AppEvent::AddImageAutomationLane { field: ImageBuiltinParam::W }
                };
                app.handle_event(ev);
            })
        },
    );
    y += input_h + 4.0;

    // H
    ui.label_at(
        "inspector_image_h_label",
        "H",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_image_h_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.image_first_event(t, |e| f64::from(e.h))),
        f64::from(summary.h),
        ScrubableNumberFormat::Decimal(3),
        &style_unit,
        InspectorScrubField::ImageH,
        move |t, v| AppEvent::SetClipImageH { target: t, value: v as f32 },
    );
    let h_auto_on = summary.h_automated;
    ui.toggle_button_at(
        "inspector_image_h_automate",
        "A",
        Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
        h_auto_on,
        &toggle_automate_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let ev = if h_auto_on {
                    AppEvent::RemoveImageAutomationLane { field: ImageBuiltinParam::H }
                } else {
                    AppEvent::AddImageAutomationLane { field: ImageBuiltinParam::H }
                };
                app.handle_event(ev);
            })
        },
    );
    y += input_h + 4.0;

    // Opacity
    ui.label_at(
        "inspector_image_opacity_label",
        "Opacity",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_image_opacity_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.image_first_event(t, |e| f64::from(e.opacity))),
        1.0,
        ScrubableNumberFormat::Decimal(3),
        &style_unit,
        InspectorScrubField::ImageOpacity,
        move |t, v| AppEvent::SetClipImageOpacity { target: t, value: v as f32 },
    );
    let opacity_auto_on = summary.opacity_automated;
    ui.toggle_button_at(
        "inspector_image_opacity_automate",
        "A",
        Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
        opacity_auto_on,
        &toggle_automate_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let ev = if opacity_auto_on {
                    AppEvent::RemoveImageAutomationLane {
                        field: ImageBuiltinParam::Opacity,
                    }
                } else {
                    AppEvent::AddImageAutomationLane {
                        field: ImageBuiltinParam::Opacity,
                    }
                };
                app.handle_event(ev);
            })
        },
    );
    y += input_h + 4.0;

    // Rotation (degree 表示、 内部 radians) — `docs/plan_image
    // _automation.md` rotation。
    ui.label_at(
        "inspector_image_rotation_label",
        "Rot (°)",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    // Rotation は degree 表示 / 入力（model は radians）。 on_change で
    // degree→radians 変換、 handler 側が -π..π に wrap するので range なし。
    scrub_field(
        ui,
        app,
        "inspector_image_rotation_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| {
            a.image_first_event(t, |e| f64::from(e.rotation_radians.to_degrees()))
        }),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &ScrubableNumberStyle {
            sensitivity: 1.0,
            // 度域 range で modulation の色帯/live tick を描けるように (gui_01
            // overlay は range 必須)。handler は -π..π wrap のまま。
            range: Some((-180.0, 180.0)),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::ImageRotation,
        move |t, v| AppEvent::SetClipImageRotation {
            target: t,
            value: (v as f32).to_radians(),
        },
    );
    let rotation_auto_on = summary.rotation_automated;
    ui.toggle_button_at(
        "inspector_image_rotation_automate",
        "A",
        Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
        rotation_auto_on,
        &toggle_automate_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                let ev = if rotation_auto_on {
                    AppEvent::RemoveImageAutomationLane {
                        field: ImageBuiltinParam::Rotation,
                    }
                } else {
                    AppEvent::AddImageAutomationLane {
                        field: ImageBuiltinParam::Rotation,
                    }
                };
                app.handle_event(ev);
            })
        },
    );
    y + input_h + 8.0
}

/// Fade In / Out (length + curve)。 audio section と同じ idiom
/// で 1 行に length + curve dropdown を並べる。
fn draw_fade_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    summary: &InspectorImageEventSummary,
    cols: &Cols,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let Cols { row_w, input_h, label_w, .. } = *cols;
    let fade_curve_w = 80.0;
    let fade_len_w = (row_w - label_w - fade_curve_w - 4.0).max(40.0);
    let fade_len_x = area.x + pad + label_w;
    let fade_curve_x = fade_len_x + fade_len_w + 4.0;

    let fade_max = summary.fade_max_beats.max(0.0);

    // Fade In
    ui.label_at(
        "inspector_image_fade_in_label",
        "Fade In",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_image_fade_in_input",
        Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
        app.inspector_fold(|a, t| a.image_first_fade(t).map(|f| f.visible_fade_in_beats())),
        0.0,
        ScrubableNumberFormat::Decimal(3),
        &ScrubableNumberStyle {
            sensitivity: 0.01,
            range: Some((0.0, fade_max)),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::ImageFadeIn,
        move |t, v| AppEvent::SetClipFadeInBeats { target: t, beats: v },
    );
    let fade_in_idx = fade_curve_to_index(summary.fade_in_curve);
    if let Some(picked) = ui.dropdown(
        "inspector_image_fade_in_curve",
        Rect { x: fade_curve_x, y, w: fade_curve_w, h: input_h },
        FADE_CURVE_LABELS,
        fade_in_idx,
    ) {
        let new_curve = fade_curve_from_index(picked);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                targets: app.inspector_target_refs(),
                edit: DiscreteClipEdit::FadeCurve(FadeEdgeKind::In, new_curve),
            })
        }));
    }
    y += input_h + 4.0;

    // Fade Out
    ui.label_at(
        "inspector_image_fade_out_label",
        "Fade Out",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_image_fade_out_input",
        Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
        app.inspector_fold(|a, t| a.image_first_fade(t).map(|f| f.visible_fade_out_beats())),
        0.0,
        ScrubableNumberFormat::Decimal(3),
        &ScrubableNumberStyle {
            sensitivity: 0.01,
            range: Some((0.0, fade_max)),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::ImageFadeOut,
        move |t, v| AppEvent::SetClipFadeOutBeats { target: t, beats: v },
    );
    let fade_out_idx = fade_curve_to_index(summary.fade_out_curve);
    if let Some(picked) = ui.dropdown(
        "inspector_image_fade_out_curve",
        Rect { x: fade_curve_x, y, w: fade_curve_w, h: input_h },
        FADE_CURVE_LABELS,
        fade_out_idx,
    ) {
        let new_curve = fade_curve_from_index(picked);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                targets: app.inspector_target_refs(),
                edit: DiscreteClipEdit::FadeCurve(FadeEdgeKind::Out, new_curve),
            })
        }));
    }
    y + input_h + 4.0
}
