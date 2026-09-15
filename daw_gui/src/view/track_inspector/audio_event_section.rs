//! インスペクタの「Audio Event」 セクション (Phase 2 PR1 / PR2 / PR3)。
//!
//! contract は `chain_sections.rs` / `launch_section.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。 `track_inspector/mod.rs::draw` から
//! 切り出したもの (サイズ budget、 不変条件 9)。

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, DiscreteClipEdit, FadeEdgeKind, InspectorScrubField};
use common::model::StretchMode;

use super::{
    FADE_CURVE_LABELS, fade_curve_from_index, fade_curve_to_index, scrub_field, scrub_style,
    toggle_audio_style,
};

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

/// selected_clip が `ClipContent::Audio` のとき、 first event の field
/// を編集できる UI を表示。 PR1 で Reverse / Mute toggle + Stretch Mode
/// dropdown、 PR2 で Gain (dB) / Pan / Pitch (semitones) text_input を
/// 追加。 編集 AppEvent は全 event に broadcast (Phase 1 で 1 clip
/// 1 event 前提なので first event = clip 全体)。 `docs/plan_audio_clip
/// .md` §3.6 / §3.7 / §3.8 / §3.9 (AudioEvent 選択時)。
pub(super) fn draw_audio_event_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    let Some(summary) = app.inspector_audio_event_summary() else {
        return y;
    };
    // text_input edit buffer の target が現選択と違ければ buffer
    // 再生成を発火する。 1 frame だけ古い buffer を表示するが、
    // 次 frame で正しい formatted 値に書き戻る (= 体感的にちらつかない)。
    // 同じ Clip を選択し直しただけでは target は変わらない (=
    // ResyncClipEditBuffers が無駄に走らない)。
    if app.cur.peph.clip_edit_buffer_target != Some(summary.target) {
        let target = summary.target;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ResyncClipEditBuffers(target));
        }));
    }

    ui.label_at(
        "inspector_audio_event_label",
        "Audio Event",
        area.x + pad,
        y,
        12.0,
        p.text,
    );
    y += 18.0;

    // Reverse / Mute toggle 横並び (mixer_strips の M/S と同じ感覚)。
    let toggle_h = 24.0;
    let row_w = area.w - pad * 2.0;
    let toggle_w = (row_w - 6.0) * 0.5;
    let new_rev = !summary.reversed;
    ui.toggle_button_at(
        "inspector_audio_reverse",
        "Reverse",
        Rect { x: area.x + pad, y, w: toggle_w, h: toggle_h },
        summary.reversed,
        &toggle_audio_style(&app.theme),
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                // 選択全クリップへ一括 (variant-safe broadcast)。
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::Reversed(new_rev),
                })
            })
        },
    );
    let new_mute = !summary.muted;
    ui.toggle_button_at(
        "inspector_audio_mute",
        "Mute",
        Rect { x: area.x + pad + toggle_w + 6.0, y, w: toggle_w, h: toggle_h },
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
    y += toggle_h + 6.0;

    // Stretch Mode dropdown (Raw / Repitch / Stretch / Slice)。
    ui.label_at(
        "inspector_audio_stretch_label",
        "Stretch",
        area.x + pad,
        y,
        11.0,
        p.text,
    );
    y += 16.0;
    let dropdown_rect = Rect { x: area.x + pad, y, w: row_w, h: 24.0 };
    let cur_idx = stretch_mode_to_index(summary.stretch_mode);
    if let Some(picked) = ui.dropdown(
        "inspector_audio_stretch_dropdown",
        dropdown_rect,
        STRETCH_MODE_LABELS,
        cur_idx,
    ) {
        let new_mode = stretch_mode_from_index(picked);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                targets: app.inspector_target_refs(),
                edit: DiscreteClipEdit::StretchMode(new_mode),
            })
        }));
    }
    y += 24.0 + 8.0;

    // ---- Phase 2 PR2: numeric field text_input ------------------
    // Gain (dB) / Pan / Pitch (semitones) を 1 行ずつ。 既存の
    // `bpm_edit_text` と同じ「buffer に逐次書き込み + Enter で
    // commit」 pattern。 buffer は target が現選択と整合するときのみ
    // 表示用、 そうでなければ commit でも無視される。
    let input_h = 22.0;
    let label_w = 60.0;
    let input_x = area.x + pad + label_w;
    let input_w = row_w - label_w;

    // Gain dB (-80..24)
    ui.label_at(
        "inspector_audio_gain_label",
        "Gain dB",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_audio_gain_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.gain_db))),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &ScrubableNumberStyle {
            sensitivity: 0.1,
            range: Some((-80.0, 24.0)),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::Gain,
        move |t, v| AppEvent::SetClipGainDb { target: t, gain_db: v as f32 },
    );
    y += input_h + 4.0;

    // Pan (-1..1)
    ui.label_at(
        "inspector_audio_pan_label",
        "Pan",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_audio_pan_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.pan))),
        0.0,
        // pan の表記は mixer / automation と同一 (`"L50"` / `"C"` / `"R100"`、r.md #47)。
        crate::automation_value::PAN_FORMAT,
        &ScrubableNumberStyle {
            sensitivity: 0.004,
            range: Some((-1.0, 1.0)),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::Pan,
        move |t, v| AppEvent::SetClipPan { target: t, pan: v as f32 },
    );
    y += input_h + 4.0;

    // Pitch (semitones, -96..96)
    ui.label_at(
        "inspector_audio_pitch_label",
        "Pitch st",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_audio_pitch_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.pitch_semitones))),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &ScrubableNumberStyle {
            sensitivity: 0.05,
            range: Some((
                f64::from(-common::model::PITCH_SEMITONES_LIMIT),
                f64::from(common::model::PITCH_SEMITONES_LIMIT),
            )),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::Pitch,
        move |t, v| AppEvent::SetClipPitchSemitones { target: t, semitones: v as f32 },
    );
    y += input_h + 4.0;

    // Formant (semitones) — r.md #40。 スペクトル包絡 (= 声質) を音程とは
    // 独立に動かす。 Stretch では `0` が「原音のフォルマントを保持」 (=
    // ピッチを動かしても声質が変わらない)、 テープ系では `0` が「素通し」。
    ui.label_at(
        "inspector_audio_formant_label",
        "Formant st",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_audio_formant_input",
        Rect { x: input_x, y, w: input_w, h: input_h },
        app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.formant_semitones))),
        0.0,
        ScrubableNumberFormat::Decimal(1),
        &ScrubableNumberStyle {
            sensitivity: 0.05,
            range: Some((
                f64::from(-common::model::FORMANT_SEMITONES_LIMIT),
                f64::from(common::model::FORMANT_SEMITONES_LIMIT),
            )),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::Formant,
        move |t, v| AppEvent::SetClipFormantSemitones { target: t, semitones: v as f32 },
    );
    // 他の数値行と同じ送り。 ラベル用の `+= 16.0` を 22px フィールドの後に
    // 使うと、続くヒント行 (label_at は y を行ボックス上端として扱う) が
    // フィールド矩形の下端 6px に食い込む。
    y += input_h + 4.0;
    // mode で `0` の意味が変わるのでヒントを添える (グレーアウトはしない —
    // 全 mode で効く設計、 r.md #40 の仕様分岐 2)。
    ui.label_at(
        "inspector_audio_formant_hint",
        if summary.stretch_mode == StretchMode::Stretch {
            "0 = 移調しても声質を保つ"
        } else {
            "0 = 素通し (テープ結果からのずらし量)"
        },
        area.x + pad,
        y,
        10.0,
        p.text_dim,
    );
    y += 14.0;

    // ---- Phase 2 PR3: Fade In / Fade Out (length + curve) -------
    // length は text_input (beats、 0..clip_length で clamp)、 curve
    // は dropdown (Linear / Exponential / SCurve、 spec §3.5)。
    // length と curve を同 1 行に並べる: label 60 + length 80 + curve
    // 残りの 3 区分。
    let fade_curve_w = 80.0;
    let fade_len_w = (row_w - label_w - fade_curve_w - 4.0).max(40.0);
    let fade_len_x = area.x + pad + label_w;
    let fade_curve_x = fade_len_x + fade_len_w + 4.0;

    let fade_max = summary.fade_max_beats.max(0.0);

    // Fade In length + curve
    ui.label_at(
        "inspector_audio_fade_in_label",
        "Fade In",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_audio_fade_in_input",
        Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
        app.inspector_fold(|a, t| a.audio_first_fade(t).map(|f| f.visible_fade_in_beats())),
        0.0,
        ScrubableNumberFormat::Decimal(3),
        &ScrubableNumberStyle {
            sensitivity: 0.01,
            range: Some((0.0, fade_max)),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::FadeIn,
        move |t, v| AppEvent::SetClipFadeInBeats { target: t, beats: v },
    );
    let fade_in_idx = fade_curve_to_index(summary.fade_in_curve);
    if let Some(picked) = ui.dropdown(
        "inspector_audio_fade_in_curve",
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

    // Fade Out length + curve
    ui.label_at(
        "inspector_audio_fade_out_label",
        "Fade Out",
        area.x + pad,
        y + 5.0,
        11.0,
        p.text,
    );
    scrub_field(
        ui,
        app,
        "inspector_audio_fade_out_input",
        Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
        app.inspector_fold(|a, t| a.audio_first_fade(t).map(|f| f.visible_fade_out_beats())),
        0.0,
        ScrubableNumberFormat::Decimal(3),
        &ScrubableNumberStyle {
            sensitivity: 0.01,
            range: Some((0.0, fade_max)),
            ..scrub_style(&app.theme)
        },
        InspectorScrubField::FadeOut,
        move |t, v| AppEvent::SetClipFadeOutBeats { target: t, beats: v },
    );
    let fade_out_idx = fade_curve_to_index(summary.fade_out_curve);
    if let Some(picked) = ui.dropdown(
        "inspector_audio_fade_out_curve",
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
    y + input_h + 12.0
}
