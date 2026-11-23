//! 音声クリップの帯の欄 (旧「Audio Event」、`docs/plan_audio_clip.md` §3.6〜§3.9 / r.md #40)。
//!
//! 値は選んでいる音声クリップ全部の、編集が効く片 (Audio Editor の選択、無ければ窓に見えている最初のひと続き)
//! を畳んだもの (割れていれば「—」/ 空 / 消灯)。編集は選んでいるクリップ全部へ送る (variant-safe broadcast)。
//! ミュート / 逆再生は帯の先頭の「再生」の行 (`mod.rs`。種類が混ざっていれば逆再生はこの欄の先頭)。

use daw_ui_core::{ScrubableNumberFormat, ScrubableNumberStyle, Ui};

use common::model::{AudioEvent, StretchMode};

use super::super::form::{self, FadeRowSpec};
use super::super::layout::{FORM_ROW_PITCH, HINT_FONT, HINT_PITCH};
use super::super::{scrub_style, vocab};
use super::ClipFields;
use crate::app::{AppData, AppEvent, DiscreteClipEdit, FadeEdgeKind, InspectorScrubField};

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

/// 選んでいる音声クリップ全部の片の値を畳む。
fn fold_event<T: PartialEq>(app: &AppData, f: &ClipFields<'_, '_>, get: impl Fn(&AudioEvent) -> T) -> Option<T> {
    f.sel.fold_eq(|c| app.audio_first_event_of(c.key, c.clip, &get))
}

fn fold_num(app: &AppData, f: &ClipFields<'_, '_>, get: impl Fn(&AudioEvent) -> f32) -> Option<f64> {
    f.sel.fold_num(|c| app.audio_first_event_of(c.key, c.clip, |e| f64::from(get(e))))
}

/// 伸縮 / ゲイン / パン / ピッチ / フォルマント / フェード。`max_beats` = 代表の片のひと続きの長さ (フェードの上限)。
pub(super) fn draw_audio_fields(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, max_beats: f64, mut y: f32) -> f32 {
    let c = &f.c;
    if f.sel.band.is_some_and(|b| b.mixed()) {
        form::label(ui, app, "inspector_audio_playback_label", vocab::FIELD_PLAYBACK, c, y);
        reverse_toggle(app, ui, f, c.value_part(y, 2, 0));
        y += FORM_ROW_PITCH;
    }
    // ---- 伸縮 (Raw / Repitch / Stretch / Slice)。割れていれば空 ----
    let stretch = fold_event(app, f, |e| e.stretch_mode);
    form::label(ui, app, "inspector_audio_stretch_label", vocab::FIELD_STRETCH, c, y);
    if let Some(picked) = ui.dropdown(
        "inspector_audio_stretch_dropdown",
        c.value(y),
        STRETCH_MODE_LABELS,
        stretch.map_or(usize::MAX, stretch_mode_to_index),
    ) {
        ui.push_edit(form::broadcast(DiscreteClipEdit::StretchMode(stretch_mode_from_index(picked))));
    }
    y += FORM_ROW_PITCH;

    let st = |sensitivity: f32, lo: f64, hi: f64| ScrubableNumberStyle {
        sensitivity,
        range: Some((lo, hi)),
        ..scrub_style(&app.theme)
    };
    let owner = f.mod_owner();
    y = form::scrub_row(
        ui,
        app,
        c,
        y,
        "inspector_audio_gain",
        vocab::FIELD_GAIN,
        fold_num(app, f, |e| e.gain_db),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &st(0.1, -80.0, 24.0),
        InspectorScrubField::Gain,
        owner,
        |t, v| AppEvent::SetClipGainDb { target: t, gain_db: v as f32 },
    );
    y = form::scrub_row(
        ui,
        app,
        c,
        y,
        "inspector_audio_pan",
        vocab::FIELD_PAN,
        fold_num(app, f, |e| e.pan),
        0.0,
        // pan の表記は mixer / automation と同一 (`"L50"` / `"C"` / `"R100"`、r.md #47)。
        crate::automation_value::PAN_FORMAT,
        &st(0.004, -1.0, 1.0),
        InspectorScrubField::Pan,
        owner,
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
        fold_num(app, f, |e| e.pitch_semitones),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &st(0.05, -pitch_limit, pitch_limit),
        InspectorScrubField::Pitch,
        owner,
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
        fold_num(app, f, |e| e.formant_semitones),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &st(0.05, -formant_limit, formant_limit),
        InspectorScrubField::Formant,
        owner,
        |t, v| AppEvent::SetClipFormantSemitones { target: t, semitones: v as f32 },
    );
    let hint = if stretch == Some(StretchMode::Stretch) {
        "0 = 移調しても声質を保つ"
    } else {
        "0 = 素通し (テープ結果からのずらし量)"
    };
    ui.label_at("inspector_audio_formant_hint", hint, c.value_x + 2.0, y - 3.0, HINT_FONT, app.theme.core.text_dim);
    y += HINT_PITCH - 3.0;

    // ---- フェード (長さ + カーブ) ----
    let fade = |get: fn(&common::model::EventFade) -> f64| f.sel.fold_num(|c| app.audio_first_fade_of(c.key, c.clip).map(|x| get(&x)));
    let curve = |get: fn(&common::model::EventFade) -> common::model::FadeCurve| {
        f.sel.fold_eq(|c| app.audio_first_fade_of(c.key, c.clip).map(|x| get(&x)))
    };
    y = form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_audio_fade_in",
        label: vocab::FIELD_FADE_IN,
        beats: fade(common::model::EventFade::visible_fade_in_beats),
        max_beats,
        curve: curve(|x| x.fade_in_curve),
        key: InspectorScrubField::FadeIn,
        owner,
        set_beats: |t, beats| AppEvent::SetClipFadeInBeats { target: t, beats },
        set_curve: |c| DiscreteClipEdit::FadeCurve(FadeEdgeKind::In, c),
    });
    form::fade_row(ui, app, c, y, &FadeRowSpec {
        id: "inspector_audio_fade_out",
        label: vocab::FIELD_FADE_OUT,
        beats: fade(common::model::EventFade::visible_fade_out_beats),
        max_beats,
        curve: curve(|x| x.fade_out_curve),
        key: InspectorScrubField::FadeOut,
        owner,
        set_beats: |t, beats| AppEvent::SetClipFadeOutBeats { target: t, beats },
        set_curve: |c| DiscreteClipEdit::FadeCurve(FadeEdgeKind::Out, c),
    })
}

/// [逆再生] (音声クリップだけ)。選んでいる音声クリップ全部が逆再生なら点灯、割れていれば消灯で、押すと
/// 全部を点灯の反対に揃える (ミュートと同じ規則)。
pub(super) fn reverse_toggle(app: &AppData, ui: &mut Ui<'_, AppData>, f: &ClipFields<'_, '_>, rect: daw_ui_renderer::Rect) {
    let on = fold_event(app, f, |e| e.reversed) == Some(true);
    ui.toggle_button_at(
        "inspector_audio_reverse",
        vocab::TOGGLE_REVERSE,
        rect,
        on,
        &super::super::toggle_audio_style(&app.theme),
        move |_| form::broadcast(DiscreteClipEdit::Reversed(!on)),
    );
}
