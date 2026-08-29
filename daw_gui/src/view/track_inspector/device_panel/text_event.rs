//! 字幕 (`builtin.video.subtitle`) デバイスの Text Event 編集欄。
//!
//! `device_panel/mod.rs` が順に呼ぶセクションの 1 つ。 contract は
//! `chain_sections.rs` / `modulation_rack.rs` と同じ
//! 「`(app, ui, area, pad, 起点 y) -> 次の y`」。
use super::super::*;

pub(super) fn draw_text_event(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    pad: f32,
    mut y: f32,
) -> f32 {
    let p = &app.theme.core;
    // ---- Text Event section (`docs/plan_text_overlay.md` §4 P5 + P5.B) --
    // selected_clip が `ClipContent::Text` のとき、 first event の全 field
    // (text / font / align / 23 numeric + 2 fade beats / fade curves / mute)
    // を expose。 numeric field は scrubable_number 化され、
    // on_change が `TextNumField` discriminator 付き `SetClipTextNumField` を
    // 直接 dispatch する (drag / type 両対応、 undo は Begin/EndInspectorScrub)。
    //
    // (talk/v26) これは「字幕の見た目」編集 UI。字幕 (`builtin.video.subtitle`) device が
    // 挿さっているトラック (= 画面表示が有効) のときだけ出す。device 無しで字幕パラメータが
    // 出るのは無意味なので gate する (`docs/plan_voicevox_talk.md`)。本文 (セリフ) 自体は
    // talk 節 (VOICEVOX device 時) でも編集できるので、ここで隠しても talk-only トラックの
    // テキスト編集は失われない。
    let text_track_has_subtitle = app
        .selected_clip_ref()
        .and_then(|r| app.song_doc.song().track_by_id(r.track_id))
        .is_some_and(common::model::Track::has_subtitle_device);
    // 字幕 device の「Par」を押したときだけ Text Event 欄を出す
    // (= 専用欄を常時表示せず Par パネルに集約)。
    if text_track_has_subtitle
        && app.subtitle_param_panel_open()
        && let Some(summary) = app.inspector_text_event_summary()
    {
        if app.ui_ephemeral.clip_edit_buffer_target != Some(summary.target) {
            let target = summary.target;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ResyncClipTextEditBuffers(target));
            }));
        }

        ui.label_at(
            "inspector_text_event_label",
            "Text Event",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;

        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 60.0;
        let auto_btn_w = 22.0;
        let auto_btn_gap = 4.0;
        let input_x = area.x + pad + label_w;
        let numeric_input_w = row_w - label_w - auto_btn_w - auto_btn_gap;
        let string_input_w = row_w - label_w;
        let auto_btn_x = input_x + numeric_input_w + auto_btn_gap;

        // Mute toggle
        let toggle_h = 24.0;
        let new_mute = !summary.muted;
        ui.toggle_button_at(
            "inspector_text_mute",
            "Mute",
            Rect { x: area.x + pad, y, w: row_w, h: toggle_h },
            summary.muted,
            &toggle_audio_style(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                        targets: app.inspector_target_refs(),
                        edit: DiscreteClipEdit::TextMuted(new_mute),
                    })
                })
            },
        );
        y += toggle_h + 8.0;

        // Text content (single-line, Enter で commit)
        ui.label_at(
            "inspector_text_content_label",
            "Text",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        let text_resp = ui.text_input_at(
            "inspector_text_content_input",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            &app.ui_ephemeral.clip_text_content_edit_text,
            &TextInputStyle::default(),
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipTextContentEditChanged(s))
                })
            },
        );
        // Enter でも外クリック (blurred = focus loss) でも確定する (daw_01 #112)。
        if text_resp.committed || text_resp.blurred {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipTextContentEdit)
            }));
        }
        y += input_h + 4.0;

        // Font family: ボタンで font picker を開く。ラベルは現在の
        // フォント (空 = デフォルト)。検索付きモーダルで選び、 ↑↓ / ホバーで
        // キャンバスにライブプレビューされる。
        ui.label_at(
            "inspector_text_font_label",
            "Font",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        let font_btn_label = if app.ui_ephemeral.clip_text_font_family_edit_text.is_empty() {
            "(default)".to_string()
        } else {
            app.ui_ephemeral.clip_text_font_family_edit_text.clone()
        };
        if ui.button_at_clicked(
            "inspector_text_font_button",
            &font_btn_label,
            Rect { x: input_x, y, w: string_input_w, h: input_h },
        ) {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenFontPicker)
            }));
        }
        y += input_h + 4.0;

        // Align dropdown (Left / Center / Right)
        ui.label_at(
            "inspector_text_align_label",
            "Align",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        const ALIGN_LABELS: &[&str] = &["Left", "Center", "Right"];
        let align_idx = match summary.align {
            TextAlign::Left => 0,
            TextAlign::Center => 1,
            TextAlign::Right => 2,
        };
        if let Some(picked) = ui.dropdown(
            "inspector_text_align_dropdown",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            ALIGN_LABELS,
            align_idx,
        ) {
            let new_align = match picked {
                0 => TextAlign::Left,
                2 => TextAlign::Right,
                _ => TextAlign::Center,
            };
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::TextAlign(new_align),
                })
            }));
        }
        y += input_h + 8.0;

        // 23 numeric rows + 2 fade beats。 1 行を描く責務は `emit_num_row` が持つ
        // (= 1 箇所変更で 25 行全部が drag 対応)。 寸法は `NumRowLayout` に束ねて渡す。
        let lay = NumRowLayout {
            label_w,
            input_x,
            numeric_input_w,
            auto_btn_x,
            auto_btn_w,
            input_h,
            fade_max: summary.fade_max_beats.max(0.0),
            area_x: area.x + pad,
        };
        emit_num_row(ui, app, &summary, &lay, "X", TextNumField::X, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Y", TextNumField::Y, &mut y);
        emit_num_row(ui, app, &summary, &lay, "W", TextNumField::W, &mut y);
        emit_num_row(ui, app, &summary, &lay, "H", TextNumField::H, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Rot (°)", TextNumField::Rotation, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Size (px)", TextNumField::FontSize, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Opacity", TextNumField::Opacity, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Fill R", TextNumField::FillR, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Fill G", TextNumField::FillG, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Fill B", TextNumField::FillB, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Fill A", TextNumField::FillA, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Out R", TextNumField::OutlineR, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Out G", TextNumField::OutlineG, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Out B", TextNumField::OutlineB, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Out A", TextNumField::OutlineA, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Out W (px)", TextNumField::OutlineWidth, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Sh R", TextNumField::ShadowR, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Sh G", TextNumField::ShadowG, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Sh B", TextNumField::ShadowB, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Sh A", TextNumField::ShadowA, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Sh X (px)", TextNumField::ShadowOffsetX, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Sh Y (px)", TextNumField::ShadowOffsetY, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Sh Blur (px)", TextNumField::ShadowBlur, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Fade In", TextNumField::FadeInBeats, &mut y);
        emit_num_row(ui, app, &summary, &lay, "Fade Out", TextNumField::FadeOutBeats, &mut y);

        // Fade curve dropdowns (2 個)。 FADE_CURVE_LABELS / curve <-> index
        // は image fade と同 idiom。
        const FADE_CURVE_LABELS: &[&str] = &["Linear", "Exp", "S-Curve"];
        let curve_to_idx = |c: FadeCurve| match c {
            FadeCurve::Linear => 0,
            FadeCurve::Exponential => 1,
            FadeCurve::SCurve => 2,
        };
        let idx_to_curve = |i: usize| match i {
            1 => FadeCurve::Exponential,
            2 => FadeCurve::SCurve,
            _ => FadeCurve::Linear,
        };
        ui.label_at(
            "inspector_text_fade_in_curve_label",
            "In Curve",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        if let Some(picked) = ui.dropdown(
            "inspector_text_fade_in_curve",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            FADE_CURVE_LABELS,
            curve_to_idx(summary.fade_in_curve),
        ) {
            let new_curve = idx_to_curve(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::TextFadeCurve(FadeEdgeKind::In, new_curve),
                })
            }));
        }
        y += input_h + 4.0;
        ui.label_at(
            "inspector_text_fade_out_curve_label",
            "Out Curve",
            area.x + pad,
            y + 5.0,
            11.0,
            p.text,
        );
        if let Some(picked) = ui.dropdown(
            "inspector_text_fade_out_curve",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            FADE_CURVE_LABELS,
            curve_to_idx(summary.fade_out_curve),
        ) {
            let new_curve = idx_to_curve(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::TextFadeCurve(FadeEdgeKind::Out, new_curve),
                })
            }));
        }
        y += input_h + 12.0;
        // suppress unused warning when section emits nothing further
        let _ = (label_w, summary);
    }
    y
}

/// numeric 行の描画に要る寸法一式。 **引数を 1 個にまとめるためだけの束**
/// (`emit_num_row` は 25 回呼ばれるので、寸法を毎回並べると呼び出しが読めなくなる)。
struct NumRowLayout {
    /// ラベル欄の幅。
    label_w: f32,
    /// 数値ボックスの左端 x。
    input_x: f32,
    /// 数値ボックスの幅。
    numeric_input_w: f32,
    /// automate 「A」 トグルの左端 x と幅。
    auto_btn_x: f32,
    auto_btn_w: f32,
    /// 行の高さ。
    input_h: f32,
    /// fade beats 行の上限 (clip 長。 `summary.fade_max_beats`)。
    fade_max: f64,
    /// 行の左端 (`area.x + pad`)。
    area_x: f32,
}
/// numeric 1 行分 (label + scrubable + automate 「A」 トグル)。
///
/// 25 行ぶんの見た目と undo の bracket をここ 1 か所が決める
/// (= 1 箇所直せば 25 行全部に効く)。 行の値源は `summary` の first event
/// snapshot、 on_change は `SetClipTextNumField` (Rotation は deg→rad)。
fn emit_num_row(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    summary: &crate::app::InspectorTextEventSummary,
    lay: &NumRowLayout,
    label: &str,
    field: TextNumField,
    row_y: &mut f32,
) {
    let p = &app.theme.core;
        // ラベル欄は lay.input_x までの lay.label_w(60) しかない。 "Sh Blur (px)" は
        // 実 advance 69.6px でここを溢れ、 後から描かれる数値ボックスの
        // 不透明背景に末尾が食われていた。
        ui.label_at_clipped(
            (field, "label"),
            label,
            Rect {
                x: lay.area_x,
                y: *row_y + 5.0,
                w: (lay.label_w - 2.0).max(1.0),
                h: 11.0 * 1.2,
            },
            11.0,
            p.text,
        );
        // field 別の (書式, range, sensitivity[units/px])。 clamp は handler
        // (`set_clip_text_num_field`) と一致させる。
        let (fmt, range, sens): (ScrubableNumberFormat, Option<(f64, f64)>, f32) = match field {
            // PiP rect (0..1 normalized)。
            TextNumField::X | TextNumField::Y | TextNumField::W | TextNumField::H => {
                (ScrubableNumberFormat::Decimal(3), Some((0.0, 1.0)), 0.004)
            }
            // Rotation: degree 表示、 handler が -π..π wrap (range なし)。
            // 度域 range で modulation overlay を描けるように (handler は -π..π wrap)。
            TextNumField::Rotation => {
                (ScrubableNumberFormat::Decimal(1), Some((-180.0, 180.0)), 1.0)
            }
            // Font size (px, >= 1.0)。
            TextNumField::FontSize => {
                (ScrubableNumberFormat::Decimal(1), Some((1.0, 4096.0)), 0.5)
            }
            // 0..1 の opacity / RGBA。
            TextNumField::Opacity
            | TextNumField::FillR
            | TextNumField::FillG
            | TextNumField::FillB
            | TextNumField::FillA
            | TextNumField::OutlineR
            | TextNumField::OutlineG
            | TextNumField::OutlineB
            | TextNumField::OutlineA
            | TextNumField::ShadowR
            | TextNumField::ShadowG
            | TextNumField::ShadowB
            | TextNumField::ShadowA => {
                (ScrubableNumberFormat::Decimal(3), Some((0.0, 1.0)), 0.004)
            }
            // px 値 (>= 0)。 outline width / shadow blur。
            TextNumField::OutlineWidth | TextNumField::ShadowBlur => {
                (ScrubableNumberFormat::Decimal(1), Some((0.0, 1024.0)), 0.5)
            }
            // shadow offset px (handler 側 clamp 無し、 上下とも自由)。
            TextNumField::ShadowOffsetX | TextNumField::ShadowOffsetY => {
                (ScrubableNumberFormat::Decimal(1), None, 0.5)
            }
            // fade beats (0..clip 長)。
            TextNumField::FadeInBeats | TextNumField::FadeOutBeats => {
                (ScrubableNumberFormat::Decimal(3), Some((0.0, lay.fade_max)), 0.01)
            }
        };
        let default = match field {
            // dblclick reset の妥当値。
            TextNumField::Opacity
            | TextNumField::FillA
            | TextNumField::W
            | TextNumField::H => summary.text_num_field_value(field),
            TextNumField::FontSize => 48.0,
            _ => 0.0,
        };
        let style =
            ScrubableNumberStyle { sensitivity: sens, range, ..scrub_style(&app.theme) };
        scrub_field(
            ui,
            app,
            (field, "input"),
            Rect { x: lay.input_x, y: *row_y, w: lay.numeric_input_w, h: lay.input_h },
            app.inspector_text_num_folded(field),
            default,
            fmt,
            &style,
            InspectorScrubField::Text(field),
            move |t, v| {
                // Rotation のみ degree 入力 → radians に変換 (handler が wrap)。
                let value = if matches!(field, TextNumField::Rotation) {
                    (v as f32).to_radians()
                } else {
                    v as f32
                };
                AppEvent::SetClipTextNumField { target: t, field, value }
            },
        );
        if let Some(builtin) = text_num_to_builtin(field) {
            let auto_on = summary.automated.contains(&builtin);
            ui.toggle_button_at(
                (field, "auto"),
                "A",
                Rect { x: lay.auto_btn_x, y: *row_y, w: lay.auto_btn_w, h: lay.input_h },
                auto_on,
                &toggle_automate_style(&app.theme),
                move |_| toggle_text_automation_edit(builtin, auto_on),
            );
        }
        *row_y += lay.input_h + 4.0;
}

/// 数値欄の「A」ボタン: この field の text automation lane を足す / 外す。
fn toggle_text_automation_edit(
    field: common::model::TextBuiltinParam,
    auto_on: bool,
) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(if auto_on {
            AppEvent::RemoveTextAutomationLane { field }
        } else {
            AppEvent::AddTextAutomationLane { field }
        });
    })
}
