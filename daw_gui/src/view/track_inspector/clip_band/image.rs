//! 画像クリップの帯の欄 (旧「Image Event」、`docs/plan_image_overlay.md` §4 P4 / `docs/plan_image_automation.md`)。
//!
//! 位置 / 大きさ / 不透明度 / 回転 (自動化の「A」つき)、フェード、反転。値は窓に見えている最初の
//! ひと続きから読み、編集は選んでいるクリップ全部へ送る。ミュートは帯の先頭の「再生」の行 (`mod.rs`)。

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};

use common::model::{ImageBuiltinParam, ImageEvent};

use super::super::form::{self, FadeRowSpec, FormCols};
use super::super::layout::FORM_ROW_PITCH;
use super::super::{scrub_style, toggle_audio_style, vocab};
use crate::app::{AppData, AppEvent, ClipKey, DiscreteClipEdit, FadeEdgeKind, InspectorImageEventSummary, InspectorScrubField};

/// 位置・大きさ・不透明度・回転の 6 欄 (上から この順)。
const PIP_FIELDS: [ImageBuiltinParam; 6] = [
    ImageBuiltinParam::X,
    ImageBuiltinParam::Y,
    ImageBuiltinParam::W,
    ImageBuiltinParam::H,
    ImageBuiltinParam::Opacity,
    ImageBuiltinParam::Rotation,
];

fn pip_label(p: ImageBuiltinParam) -> &'static str {
    match p {
        ImageBuiltinParam::X => vocab::FIELD_X,
        ImageBuiltinParam::Y => vocab::FIELD_Y,
        ImageBuiltinParam::W => vocab::FIELD_W,
        ImageBuiltinParam::H => vocab::FIELD_H,
        ImageBuiltinParam::Opacity => vocab::FIELD_OPACITY,
        ImageBuiltinParam::Rotation => vocab::FIELD_ROTATION,
    }
}

/// 表示の値 (回転は度、model は radians)。
fn pip_value(e: &ImageEvent, p: ImageBuiltinParam) -> f64 {
    f64::from(match p {
        ImageBuiltinParam::X => e.x,
        ImageBuiltinParam::Y => e.y,
        ImageBuiltinParam::W => e.w,
        ImageBuiltinParam::H => e.h,
        ImageBuiltinParam::Opacity => e.opacity,
        ImageBuiltinParam::Rotation => e.rotation_radians.to_degrees(),
    })
}

fn pip_key(p: ImageBuiltinParam) -> InspectorScrubField {
    match p {
        ImageBuiltinParam::X => InspectorScrubField::ImageX,
        ImageBuiltinParam::Y => InspectorScrubField::ImageY,
        ImageBuiltinParam::W => InspectorScrubField::ImageW,
        ImageBuiltinParam::H => InspectorScrubField::ImageH,
        ImageBuiltinParam::Opacity => InspectorScrubField::ImageOpacity,
        ImageBuiltinParam::Rotation => InspectorScrubField::ImageRotation,
    }
}

/// 編集 (回転は度 → radians、handler が -π..π に wrap)。
fn pip_event(p: ImageBuiltinParam, target: ClipKey, v: f64) -> AppEvent {
    let value = v as f32;
    match p {
        ImageBuiltinParam::X => AppEvent::SetClipImageX { target, value },
        ImageBuiltinParam::Y => AppEvent::SetClipImageY { target, value },
        ImageBuiltinParam::W => AppEvent::SetClipImageW { target, value },
        ImageBuiltinParam::H => AppEvent::SetClipImageH { target, value },
        ImageBuiltinParam::Opacity => AppEvent::SetClipImageOpacity { target, value },
        ImageBuiltinParam::Rotation => AppEvent::SetClipImageRotation { target, value: value.to_radians() },
    }
}

fn pip_automated(s: &InspectorImageEventSummary, p: ImageBuiltinParam) -> bool {
    match p {
        ImageBuiltinParam::X => s.x_automated,
        ImageBuiltinParam::Y => s.y_automated,
        ImageBuiltinParam::W => s.w_automated,
        ImageBuiltinParam::H => s.h_automated,
        ImageBuiltinParam::Opacity => s.opacity_automated,
        ImageBuiltinParam::Rotation => s.rotation_automated,
    }
}

/// 位置 / 大きさ / 不透明度 / 回転、フェード、反転。
pub(super) fn draw_image_fields(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    summary: &InspectorImageEventSummary,
    mut y: f32,
) -> f32 {
    // 0..1 の欄 (位置 / 大きさ / 不透明度) は細かい送り。回転は度 (range は modulation の色帯用、handler は wrap)。
    let unit = ScrubableNumberStyle { sensitivity: 0.004, range: Some((0.0, 1.0)), ..scrub_style(&app.theme) };
    let degrees = ScrubableNumberStyle { sensitivity: 1.0, range: Some((-180.0, 180.0)), ..scrub_style(&app.theme) };
    for p in PIP_FIELDS {
        let (style, fmt, default) = match p {
            ImageBuiltinParam::Rotation => (&degrees, ScrubableNumberFormat::Decimal(1), 0.0),
            // dblclick で戻す値: 大きさは今の値 (0 に潰さない)、不透明度は 1。
            ImageBuiltinParam::W => (&unit, ScrubableNumberFormat::Decimal(3), f64::from(summary.w)),
            ImageBuiltinParam::H => (&unit, ScrubableNumberFormat::Decimal(3), f64::from(summary.h)),
            ImageBuiltinParam::Opacity => (&unit, ScrubableNumberFormat::Decimal(3), 1.0),
            _ => (&unit, ScrubableNumberFormat::Decimal(3), 0.0),
        };
        let row_y = y;
        y = form::scrub_row(
            ui,
            app,
            c,
            row_y,
            pip_id(p),
            pip_label(p),
            app.inspector_fold(|a, t| a.image_first_event(t, |e| pip_value(e, p))),
            default,
            fmt,
            style,
            pip_key(p),
            move |t, v| pip_event(p, t, v),
        );
        // 「A」: この欄のレーンを足す / 外す (`docs/plan_image_automation.md` §4.1)。
        let on = pip_automated(summary, p);
        form::auto_toggle(ui, app, (pip_id(p), "automate"), c, row_y, on, move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(if on {
                    AppEvent::RemoveImageAutomationLane { field: p }
                } else {
                    AppEvent::AddImageAutomationLane { field: p }
                });
            })
        });
    }

    let max_beats = summary.fade_max_beats;
    y = form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_image_fade_in",
        label: vocab::FIELD_FADE_IN,
        beats: app.inspector_fold(|a, t| a.image_first_fade(t).map(|f| f.visible_fade_in_beats())),
        max_beats,
        curve: summary.fade_in_curve,
        key: InspectorScrubField::ImageFadeIn,
        set_beats: |t, beats| AppEvent::SetClipFadeInBeats { target: t, beats },
        set_curve: |c| DiscreteClipEdit::FadeCurve(FadeEdgeKind::In, c),
    });
    y = form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_image_fade_out",
        label: vocab::FIELD_FADE_OUT,
        beats: app.inspector_fold(|a, t| a.image_first_fade(t).map(|f| f.visible_fade_out_beats())),
        max_beats,
        curve: summary.fade_out_curve,
        key: InspectorScrubField::ImageFadeOut,
        set_beats: |t, beats| AppEvent::SetClipFadeOutBeats { target: t, beats },
        set_curve: |c| DiscreteClipEdit::FadeCurve(FadeEdgeKind::Out, c),
    });

    // ---- 反転 (r.md #98): [左右] [上下] ----
    form::label(ui, app, "inspector_image_flip_label", vocab::FIELD_FLIP, c, y);
    let style = toggle_audio_style(&app.theme);
    let (flip_h, flip_v) = (summary.flip_h, summary.flip_v);
    ui.toggle_button_at("inspector_image_flip_h", vocab::TOGGLE_FLIP_H, c.value_part(y, 2, 0), flip_h, &style, move |_| {
        form::broadcast(DiscreteClipEdit::ImageFlipH(!flip_h))
    });
    ui.toggle_button_at("inspector_image_flip_v", vocab::TOGGLE_FLIP_V, c.value_part(y, 2, 1), flip_v, &style, move |_| {
        form::broadcast(DiscreteClipEdit::ImageFlipV(!flip_v))
    });
    y + FORM_ROW_PITCH
}

fn pip_id(p: ImageBuiltinParam) -> &'static str {
    match p {
        ImageBuiltinParam::X => "inspector_image_x",
        ImageBuiltinParam::Y => "inspector_image_y",
        ImageBuiltinParam::W => "inspector_image_w",
        ImageBuiltinParam::H => "inspector_image_h",
        ImageBuiltinParam::Opacity => "inspector_image_opacity",
        ImageBuiltinParam::Rotation => "inspector_image_rotation",
    }
}
