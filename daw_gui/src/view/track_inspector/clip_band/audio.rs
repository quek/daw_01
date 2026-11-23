//! 音声クリップの帯の欄 (旧「Audio Event」、`docs/plan_audio_clip.md` §3.6〜§3.9 / r.md #40)。
//!
//! 編集が効く片 (Audio Editor の選択、無ければ窓に見えている最初のひと続き) の値を出し、編集は選んでいる
//! クリップ全部へ送る (variant-safe broadcast)。ミュート / 逆再生は帯の先頭の「再生」の行 (`mod.rs`)。

use daw_ui_core::{ScrubableNumberFormat, ScrubableNumberStyle, Ui};

use common::model::StretchMode;

use super::super::form::{self, FadeRowSpec, FormCols};
use super::super::layout::{FORM_ROW_PITCH, HINT_FONT, HINT_PITCH};
use super::super::{scrub_style, vocab};
use crate::app::{AppData, AppEvent, DiscreteClipEdit, FadeEdgeKind, InspectorAudioEventSummary, InspectorScrubField};

const STRETCH_MODE_LABELS: &[&str] = &["Raw", "Repitch", "Stretch", "Slice"];

fn stretch_mode_to_index(m: StretchMode) -> usize {
    match m {
        StretchMode::Raw => 0,
        StretchMode::Repitch => 1,
        StretchMode::Stretch => 2,
        StretchMode::Slice => 3,
    }
}

fn stretch_mode_from_index(i: usize) -> StretchMode {
    match i {
        1 => StretchMode::Repitch,
        2 => StretchMode::Stretch,
        3 => StretchMode::Slice,
        _ => StretchMode::Raw,
    }
}

/// 伸縮 / ゲイン / パン / ピッチ / フォルマント / フェード。
pub(super) fn draw_audio_fields(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    c: &FormCols,
    summary: &InspectorAudioEventSummary,
    mut y: f32,
) -> f32 {
    // ---- 伸縮 (Raw / Repitch / Stretch / Slice) ----
    form::label(ui, app, "inspector_audio_stretch_label", vocab::FIELD_STRETCH, c, y);
    if let Some(picked) = ui.dropdown(
        "inspector_audio_stretch_dropdown",
        c.value(y),
        STRETCH_MODE_LABELS,
        stretch_mode_to_index(summary.stretch_mode),
    ) {
        ui.push_edit(form::broadcast(DiscreteClipEdit::StretchMode(stretch_mode_from_index(picked))));
    }
    y += FORM_ROW_PITCH;

    let st = |sensitivity: f32, lo: f64, hi: f64| ScrubableNumberStyle {
        sensitivity,
        range: Some((lo, hi)),
        ..scrub_style(&app.theme)
    };
    y = form::scrub_row(
        ui,
        app,
        c,
        y,
        "inspector_audio_gain",
        vocab::FIELD_GAIN,
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.gain_db))),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &st(0.1, -80.0, 24.0),
        InspectorScrubField::Gain,
        |t, v| AppEvent::SetClipGainDb { target: t, gain_db: v as f32 },
    );
    y = form::scrub_row(
        ui,
        app,
        c,
        y,
        "inspector_audio_pan",
        vocab::FIELD_PAN,
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.pan))),
        0.0,
        // pan の表記は mixer / automation と同一 (`"L50"` / `"C"` / `"R100"`、r.md #47)。
        crate::automation_value::PAN_FORMAT,
        &st(0.004, -1.0, 1.0),
        InspectorScrubField::Pan,
        |t, v| AppEvent::SetClipPan { target: t, pan: v as f32 },
    );
    let pitch_limit = f64::from(common::model::PITCH_SEMITONES_LIMIT);
    y = form::scrub_row(
        ui,
        app,
        c,
        y,
        "inspector_audio_pitch",
        vocab::FIELD_PITCH,
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.pitch_semitones))),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &st(0.05, -pitch_limit, pitch_limit),
        InspectorScrubField::Pitch,
        |t, v| AppEvent::SetClipPitchSemitones { target: t, semitones: v as f32 },
    );
    // フォルマント (r.md #40): スペクトル包絡 (= 声質) を音程とは独立に動かす。Stretch では `0` が
    // 「原音のフォルマントを保持」、テープ系では `0` が「素通し」なので、値の下にヒントを添える
    // (グレーアウトはしない — 全 mode で効く、r.md #40 の仕様分岐 2)。
    let formant_limit = f64::from(common::model::FORMANT_SEMITONES_LIMIT);
    y = form::scrub_row(
        ui,
        app,
        c,
        y,
        "inspector_audio_formant",
        vocab::FIELD_FORMANT,
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.formant_semitones))),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &st(0.05, -formant_limit, formant_limit),
        InspectorScrubField::Formant,
        |t, v| AppEvent::SetClipFormantSemitones { target: t, semitones: v as f32 },
    );
    let hint = if summary.stretch_mode == StretchMode::Stretch {
        "0 = 移調しても声質を保つ"
    } else {
        "0 = 素通し (テープ結果からのずらし量)"
    };
    ui.label_at("inspector_audio_formant_hint", hint, c.value_x + 2.0, y - 3.0, HINT_FONT, app.theme.core.text_dim);
    y += HINT_PITCH - 3.0;

    // ---- フェード (長さ + カーブ) ----
    let max_beats = summary.fade_max_beats;
    y = form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_audio_fade_in",
        label: vocab::FIELD_FADE_IN,
        beats: app.inspector_fold(|a, t| a.audio_first_fade(t).map(|f| f.visible_fade_in_beats())),
        max_beats,
        curve: summary.fade_in_curve,
        key: InspectorScrubField::FadeIn,
        set_beats: |t, beats| AppEvent::SetClipFadeInBeats { target: t, beats },
        set_curve: |c| DiscreteClipEdit::FadeCurve(FadeEdgeKind::In, c),
    });
    form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_audio_fade_out",
        label: vocab::FIELD_FADE_OUT,
        beats: app.inspector_fold(|a, t| a.audio_first_fade(t).map(|f| f.visible_fade_out_beats())),
        max_beats,
        curve: summary.fade_out_curve,
        key: InspectorScrubField::FadeOut,
        set_beats: |t, beats| AppEvent::SetClipFadeOutBeats { target: t, beats },
        set_curve: |c| DiscreteClipEdit::FadeCurve(FadeEdgeKind::Out, c),
    })
}

/// 「再生」の行の [逆再生] (音声クリップだけ)。押すと選んでいるクリップ全部へ。
pub(super) fn reverse_toggle(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    rect: daw_ui_renderer::Rect,
    summary: &InspectorAudioEventSummary,
) {
    let next = !summary.reversed;
    ui.toggle_button_at(
        "inspector_audio_reverse",
        vocab::TOGGLE_REVERSE,
        rect,
        summary.reversed,
        &super::super::toggle_audio_style(&app.theme),
        move |_| form::broadcast(DiscreteClipEdit::Reversed(next)),
    );
}
