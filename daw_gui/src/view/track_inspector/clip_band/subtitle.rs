//! 字幕 (`builtin.video.subtitle` が映す Text クリップの見た目、`docs/plan_text_overlay.md` §4 P5)。r.md #158 で
//! 字幕デバイスの Par からクリップの帯へ移した (旧「Text Event」)。
//!
//! 字幕デバイスを持つトラックの Text クリップを選んでいるとき、小見出し「字幕」の下にフォント / 揃え /
//! 位置 / 大きさ / 不透明度 / フェードを出し、めったに触らない塗り・縁・影・回転は詳細「字幕の細かい設定」
//! (最初は畳む) に入れる。文字 (本文) とミュートは帯の先頭の行 (`mod.rs`)。数値欄の値は選んでいるクリップ
//! 全部を畳んだ値 (割れていれば「—」)、編集は全部へ送る。

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};

use common::model::{TextAlign, TextBuiltinParam};

use super::super::form::{self, FadeRowSpec, FormCols};
use super::super::layout::FORM_ROW_PITCH;
use super::super::{band, scrub_style, vocab};
use crate::app::{
    AppData, AppEvent, DiscreteClipEdit, FadeEdgeKind, InspectorScrubField, InspectorTextEventSummary, TextNumField,
    text_num_to_builtin,
};
use crate::state::inspector_view::InspectorDetail;

const ALIGN_LABELS: &[&str] = &["左", "中央", "右"];

/// 小見出し「字幕」以下。字幕デバイスを持つトラックの Text クリップでなければ何も描かない。
pub(super) fn draw_subtitle(app: &AppData, ui: &mut Ui<'_, AppData>, c: &FormCols, w: f32, y: f32) -> f32 {
    let has_subtitle = app
        .selected_clip_ref()
        .and_then(|r| app.cur.song_doc.song().track_by_id(r.track_id))
        .is_some_and(common::model::Track::has_subtitle_device);
    let Some(summary) = app.inspector_text_event_summary().filter(|_| has_subtitle) else {
        return y;
    };
    let mut y = band::sub_heading(app, ui, "inspector_subtitle_head", c.label_x, w, y + 2.0, vocab::SUB_SUBTITLE, None);

    // フォント: ボタンで font picker を開く (検索付き、↑↓ / ホバーでキャンバスにライブプレビュー)。
    form::label(ui, app, "inspector_text_font_label", vocab::FIELD_FONT, c, y);
    let family = &app.cur.peph.clip_text_font_family_edit_text;
    let font_label = if family.is_empty() { "(既定)" } else { family.as_str() };
    if ui.button_at_clicked("inspector_text_font_button", font_label, c.value(y)) {
        ui.push_edit(Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::OpenFontPicker)));
    }
    y += FORM_ROW_PITCH;

    // 揃え。
    form::label(ui, app, "inspector_text_align_label", vocab::FIELD_ALIGN, c, y);
    let align_idx = match summary.align {
        TextAlign::Left => 0,
        TextAlign::Center => 1,
        TextAlign::Right => 2,
    };
    if let Some(picked) = ui.dropdown("inspector_text_align_dropdown", c.value(y), ALIGN_LABELS, align_idx) {
        let align = match picked {
            0 => TextAlign::Left,
            2 => TextAlign::Right,
            _ => TextAlign::Center,
        };
        ui.push_edit(form::broadcast(DiscreteClipEdit::TextAlign(align)));
    }
    y += FORM_ROW_PITCH;

    for (label, field) in [
        (vocab::FIELD_X, TextNumField::X),
        (vocab::FIELD_Y, TextNumField::Y),
        (vocab::FIELD_W, TextNumField::W),
        (vocab::FIELD_H, TextNumField::H),
        (vocab::FIELD_FONT_SIZE, TextNumField::FontSize),
        (vocab::FIELD_OPACITY, TextNumField::Opacity),
    ] {
        y = num_row(app, ui, c, &summary, y, label, field);
    }

    let max_beats = summary.fade_max_beats;
    y = form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_text_fade_in",
        label: vocab::FIELD_FADE_IN,
        beats: app.inspector_text_num_folded(TextNumField::FadeInBeats),
        max_beats,
        curve: summary.fade_in_curve,
        key: InspectorScrubField::Text { field: TextNumField::FadeInBeats },
        set_beats: |t, v| AppEvent::SetClipTextNumField { target: t, field: TextNumField::FadeInBeats, value: v as f32 },
        set_curve: |c| DiscreteClipEdit::TextFadeCurve(FadeEdgeKind::In, c),
    });
    y = form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_text_fade_out",
        label: vocab::FIELD_FADE_OUT,
        beats: app.inspector_text_num_folded(TextNumField::FadeOutBeats),
        max_beats,
        curve: summary.fade_out_curve,
        key: InspectorScrubField::Text { field: TextNumField::FadeOutBeats },
        set_beats: |t, v| AppEvent::SetClipTextNumField { target: t, field: TextNumField::FadeOutBeats, value: v as f32 },
        set_curve: |c| DiscreteClipEdit::TextFadeCurve(FadeEdgeKind::Out, c),
    });

    // めったに触らない見た目 (r.md #158 仕様 7): 最初は畳む。
    band::details(app, ui, c.label_x, w, y, InspectorDetail::SubtitleStyle, vocab::DETAIL_SUBTITLE_STYLE, |ui, y| {
        draw_style_details(app, ui, c, w, &summary, y)
    })
}

/// 詳細の中身: 回転と、塗り / 縁 / 影。
fn draw_style_details(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    w: f32,
    summary: &InspectorTextEventSummary,
    y: f32,
) -> f32 {
    let mut y = num_row(app, ui, c, summary, y, vocab::FIELD_ROTATION, TextNumField::Rotation);
    // アルファの欄名は「不透明度」(「A」だと右の自動化の「A」と並んで紛れる)。
    let (r, g, b, a) = (vocab::FIELD_R, vocab::FIELD_G, vocab::FIELD_B, vocab::FIELD_OPACITY);
    let groups: [(&str, &[(&str, TextNumField)]); 3] = [
        (
            vocab::SUB_FILL,
            &[(r, TextNumField::FillR), (g, TextNumField::FillG), (b, TextNumField::FillB), (a, TextNumField::FillA)],
        ),
        (
            vocab::SUB_OUTLINE,
            &[
                (r, TextNumField::OutlineR),
                (g, TextNumField::OutlineG),
                (b, TextNumField::OutlineB),
                (a, TextNumField::OutlineA),
                (vocab::FIELD_OUTLINE_W, TextNumField::OutlineWidth),
            ],
        ),
        (
            vocab::SUB_SHADOW,
            &[
                (r, TextNumField::ShadowR),
                (g, TextNumField::ShadowG),
                (b, TextNumField::ShadowB),
                (a, TextNumField::ShadowA),
                (vocab::FIELD_OFFSET_X, TextNumField::ShadowOffsetX),
                (vocab::FIELD_OFFSET_Y, TextNumField::ShadowOffsetY),
                (vocab::FIELD_BLUR, TextNumField::ShadowBlur),
            ],
        ),
    ];
    for (heading, fields) in groups {
        y = band::sub_heading(app, ui, ("inspector_text_group", heading), c.label_x, w, y + 2.0, heading, None);
        for &(label, field) in fields {
            y = num_row(app, ui, c, summary, y, label, field);
        }
    }
    y
}

/// 数値欄 1 行 (+ 自動化できる欄は「A」)。書式 / 範囲 / 送りは handler (`set_clip_text_num_field`) の clamp と一致させる。
fn num_row(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    summary: &InspectorTextEventSummary,
    y: f32,
    label: &str,
    field: TextNumField,
) -> f32 {
    use TextNumField as F;
    let (fmt, range, sensitivity): (ScrubableNumberFormat, Option<(f64, f64)>, f32) = match field {
        // 位置と大きさ (0..1)。
        F::X | F::Y | F::W | F::H => (ScrubableNumberFormat::Decimal(3), Some((0.0, 1.0)), 0.004),
        // 回転: 度 (handler が -π..π に wrap。range は modulation の色帯用)。
        F::Rotation => (ScrubableNumberFormat::Decimal(1), Some((-180.0, 180.0)), 1.0),
        // 文字の大きさ (px, >= 1)。
        F::FontSize => (ScrubableNumberFormat::Decimal(1), Some((1.0, 4096.0)), 0.5),
        // 不透明度 / RGBA (0..1)。
        F::Opacity
        | F::FillR
        | F::FillG
        | F::FillB
        | F::FillA
        | F::OutlineR
        | F::OutlineG
        | F::OutlineB
        | F::OutlineA
        | F::ShadowR
        | F::ShadowG
        | F::ShadowB
        | F::ShadowA => (ScrubableNumberFormat::Decimal(3), Some((0.0, 1.0)), 0.004),
        // 縁の太さ / 影のぼかし (px, >= 0)。
        F::OutlineWidth | F::ShadowBlur => (ScrubableNumberFormat::Decimal(1), Some((0.0, 1024.0)), 0.5),
        // 影のずれ (px、上下とも自由)。
        F::ShadowOffsetX | F::ShadowOffsetY => (ScrubableNumberFormat::Decimal(1), None, 0.5),
        // フェード (拍) は `fade_row` が描く。
        F::FadeInBeats | F::FadeOutBeats => (ScrubableNumberFormat::Decimal(3), Some((0.0, summary.fade_max_beats)), 0.01),
    };
    // dblclick で戻す値。
    let default = match field {
        F::Opacity | F::FillA | F::W | F::H => summary.text_num_field_value(field),
        F::FontSize => 48.0,
        _ => 0.0,
    };
    let style = ScrubableNumberStyle { sensitivity, range, ..scrub_style(&app.theme) };
    let next = form::scrub_row(
        ui,
        app,
        c,
        y,
        text_field_id(field),
        label,
        app.inspector_text_num_folded(field),
        default,
        fmt,
        &style,
        InspectorScrubField::Text { field },
        move |t, v| {
            // 回転だけ度 → radians (handler が wrap)。
            let value = if matches!(field, F::Rotation) { (v as f32).to_radians() } else { v as f32 };
            AppEvent::SetClipTextNumField { target: t, field, value }
        },
    );
    if let Some(builtin) = text_num_to_builtin(field) {
        let on = summary.automated.contains(&builtin);
        form::auto_toggle(ui, app, (text_field_id(field), "auto"), c, y, on, move |_| toggle_text_automation(builtin, on));
    }
    next
}

/// 数値欄の widget id (欄ごとに 1 つ。インスペクタに字幕の欄は 1 組しかない)。
fn text_field_id(field: TextNumField) -> &'static str {
    use TextNumField as F;
    match field {
        F::X => "inspector_text_x",
        F::Y => "inspector_text_y",
        F::W => "inspector_text_w",
        F::H => "inspector_text_h",
        F::Rotation => "inspector_text_rotation",
        F::FontSize => "inspector_text_font_size",
        F::Opacity => "inspector_text_opacity",
        F::FillR => "inspector_text_fill_r",
        F::FillG => "inspector_text_fill_g",
        F::FillB => "inspector_text_fill_b",
        F::FillA => "inspector_text_fill_a",
        F::OutlineR => "inspector_text_outline_r",
        F::OutlineG => "inspector_text_outline_g",
        F::OutlineB => "inspector_text_outline_b",
        F::OutlineA => "inspector_text_outline_a",
        F::OutlineWidth => "inspector_text_outline_w",
        F::ShadowR => "inspector_text_shadow_r",
        F::ShadowG => "inspector_text_shadow_g",
        F::ShadowB => "inspector_text_shadow_b",
        F::ShadowA => "inspector_text_shadow_a",
        F::ShadowOffsetX => "inspector_text_shadow_x",
        F::ShadowOffsetY => "inspector_text_shadow_y",
        F::ShadowBlur => "inspector_text_shadow_blur",
        F::FadeInBeats => "inspector_text_fade_in",
        F::FadeOutBeats => "inspector_text_fade_out",
    }
}

/// 数値欄の「A」: この欄の text automation lane を足す / 外す。
fn toggle_text_automation(field: TextBuiltinParam, on: bool) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(if on {
            AppEvent::RemoveTextAutomationLane { field }
        } else {
            AppEvent::AddTextAutomationLane { field }
        });
    })
}
