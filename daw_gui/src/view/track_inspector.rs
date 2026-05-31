//! トラック inspector (左サイドバー):
//! - 選択トラック名
//! - 「Chain」見出し
//! - MIDI FX → Instrument → FX のリスト (各行に GUI / × ボタン、drag&drop で reorder)
//! - + Instrument / + Effect / + MIDI FX ボタン

use daw_ui_core::{
    Edit, ReorderableListEditRequest, ReorderableListStyle, ScrubableNumberFormat,
    ScrubableNumberStyle, ToggleButtonStyle, Ui,
};
use daw_ui_renderer::{Color, Rect};

use crate::app::{
    text_num_to_builtin, AppData, AppEvent, ColorPickerTarget, PickerTarget, TextNumField,
};
use crate::view::track_color;
use common::model::{FadeCurve, ImageBuiltinParam, StretchMode, TextAlign};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const TEXT_DIM: Color = Color { r: 0.62, g: 0.65, b: 0.70, a: 1.0 };
const ROW_BG: Color = Color { r: 0.20, g: 0.20, b: 0.24, a: 1.0 };
const ROW_BG_HOVER: Color = Color { r: 0.24, g: 0.24, b: 0.30, a: 1.0 };
const ROW_BG_DRAGGING: Color = Color { r: 0.30, g: 0.40, b: 0.55, a: 0.85 };
const SECTION_TEXT: Color = Color { r: 0.55, g: 0.62, b: 0.78, a: 1.0 };
const DROP_INDICATOR: Color = Color { r: 0.55, g: 0.78, b: 0.95, a: 1.0 };

// Audio event toggle (Reverse / Muted) 用 style。 mixer_strips の
// STYLE_MUTE / STYLE_SOLO とほぼ同じだが、 inspector 側に独立して定義
// (mixer の private const を import するより、 同 widget 並びの一覧性を
// 優先)。 hint band は無し (= 単純トグル) にして、 文字 + ON/OFF 色だけで
// 状態を伝える。
const TOGGLE_AUDIO_BASE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.42, g: 0.55, b: 0.78, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

// Image PiP の automate toggle 用 style (= lane を作る / 削除する 1 個
// 1 個のボタン)。 ON 状態は arrangement lane 行のヘッダ色 (薄い藤色) と
// 揃えて「この field は lane 駆動中」 を視覚化。
const TOGGLE_IMAGE_AUTOMATE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.78, g: 0.55, b: 0.85, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 11.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

// Group Transform の scrubable_number base style。 sensitivity / range は param
// 別に上書きする。 ドラッグで連続変化 / click で text 入力 / dblclick で reset。
const SCRUB_STYLE_GROUP: ScrubableNumberStyle = ScrubableNumberStyle {
    bg_color: Color { r: 0.16, g: 0.17, b: 0.21, a: 1.0 },
    bg_color_hovered: Color { r: 0.20, g: 0.21, b: 0.26, a: 1.0 },
    bg_color_dragging: Color { r: 0.20, g: 0.32, b: 0.42, a: 1.0 },
    text_color: TEXT,
    border: Color { r: 0.32, g: 0.35, b: 0.42, a: 1.0 },
    border_width: 1.0,
    radius: 3.0,
    font_size: 11.0,
    sensitivity: 0.004,
    range: None,
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

const FADE_CURVE_LABELS: &[&str] = &["Linear", "Exp", "SCurve"];

fn fade_curve_to_index(c: FadeCurve) -> usize {
    match c {
        FadeCurve::Linear => 0,
        FadeCurve::Exponential => 1,
        FadeCurve::SCurve => 2,
    }
}

fn fade_curve_from_index(i: usize) -> FadeCurve {
    match i {
        1 => FadeCurve::Exponential,
        2 => FadeCurve::SCurve,
        _ => FadeCurve::Linear,
    }
}

const CHAIN_LIST_STYLE: ReorderableListStyle = ReorderableListStyle {
    row_height: 26.0,
    row_gap: 3.0,
    row_bg: ROW_BG,
    row_bg_hover: ROW_BG_HOVER,
    row_bg_selected: ROW_BG,
    row_bg_dragging: ROW_BG_DRAGGING,
    drop_indicator_color: DROP_INDICATOR,
    drop_indicator_h: 2.0,
    radius: 3.0,
    drag_handle_w: 0.0,
};

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("inspector_bg", area, BG, 0.0);

    let pad = 12.0;
    let mut y = area.y + pad;

    // 選択トラック名
    ui.label_at(
        "inspector_title",
        &app.selected_track_label(),
        area.x + pad,
        y,
        16.0,
        TEXT,
    );

    // v18 (`docs/plan_track_clip_color.md`): タイトル行右端に track 色スウォッチ。
    // 単一トラック選択時のみ表示し、クリックで color_picker を開く (anchor =
    // スウォッチ rect)。effective 色 (上書き or id 由来の導出色) を塗る。
    if app.selected_track_ids.len() <= 1
        && let Some(idx) = app.cursor_track_index()
        && let Some(track) = app.song.tracks.get(idx)
    {
        let track_id = track.id;
        let swatch = Rect { x: area.x + area.w - pad - 20.0, y: y - 2.0, w: 20.0, h: 20.0 };
        // hit-test (click 検出) を先に行い、 その上に色を塗る (button の既定
        // 描画は隠れる)。
        let clicked = ui.button_at_clicked("inspector_color_swatch", "", swatch);
        let fill = track_color::to_renderer(track_color::effective_track_color(track));
        ui.panel_with_border(
            "inspector_color_swatch_fill",
            swatch,
            fill,
            Color { r: 0.45, g: 0.48, b: 0.55, a: 1.0 },
            1.0,
            4.0,
        );
        if clicked {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.open_color_picker(ColorPickerTarget::Track(track_id), swatch);
            }));
        }
    }

    y += 28.0;

    // ---- Audio Event section (Phase 2 PR1 + PR2) -----------------------
    // selected_clip が `ClipContent::Audio` のとき、 first event の field
    // を編集できる UI を表示。 PR1 で Reverse / Mute toggle + Stretch Mode
    // dropdown、 PR2 で Gain (dB) / Pan / Pitch (semitones) text_input を
    // 追加。 編集 AppEvent は全 event に broadcast (Phase 1 で 1 clip
    // 1 event 前提なので first event = clip 全体)。 `docs/plan_audio_clip
    // .md` §3.6 / §3.7 / §3.8 / §3.9 (AudioEvent 選択時)。
    if let Some(summary) = app.inspector_audio_event_summary() {
        // text_input edit buffer の target が現選択と違ければ buffer
        // 再生成を発火する。 1 frame だけ古い buffer を表示するが、
        // 次 frame で正しい formatted 値に書き戻る (= 体感的にちらつかない)。
        // 同じ Clip を選択し直しただけでは target は変わらない (=
        // ResyncClipEditBuffers が無駄に走らない)。
        if app.clip_edit_buffer_target != Some(summary.target) {
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
            TEXT_DIM,
        );
        y += 18.0;

        // Reverse / Mute toggle 横並び (mixer_strips の M/S と同じ感覚)。
        let toggle_h = 24.0;
        let row_w = area.w - pad * 2.0;
        let toggle_w = (row_w - 6.0) * 0.5;
        let target_rev = summary.target;
        let new_rev = !summary.reversed;
        ui.toggle_button_at(
            "inspector_audio_reverse",
            "Reverse",
            Rect { x: area.x + pad, y, w: toggle_w, h: toggle_h },
            summary.reversed,
            &TOGGLE_AUDIO_BASE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipReversed {
                        target: target_rev,
                        reversed: new_rev,
                    })
                })
            },
        );
        let target_mute = summary.target;
        let new_mute = !summary.muted;
        ui.toggle_button_at(
            "inspector_audio_mute",
            "Mute",
            Rect {
                x: area.x + pad + toggle_w + 6.0,
                y,
                w: toggle_w,
                h: toggle_h,
            },
            summary.muted,
            &TOGGLE_AUDIO_BASE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipMuted {
                        target: target_mute,
                        muted: new_mute,
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
            TEXT_DIM,
        );
        y += 16.0;
        let dropdown_rect = Rect {
            x: area.x + pad,
            y,
            w: row_w,
            h: 24.0,
        };
        let cur_idx = stretch_mode_to_index(summary.stretch_mode);
        if let Some(picked) = ui.dropdown(
            "inspector_audio_stretch_dropdown",
            dropdown_rect,
            STRETCH_MODE_LABELS,
            cur_idx,
        ) {
            let target_sm = summary.target;
            let new_mode = stretch_mode_from_index(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipStretchMode {
                    target: target_sm,
                    mode: new_mode,
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

        // Gain
        ui.label_at(
            "inspector_audio_gain_label",
            "Gain dB",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let gain_resp = ui.text_input_at(
            "inspector_audio_gain_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_gain_db_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipGainEditChanged(s))
                })
            },
        );
        if gain_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipGainEdit)
            }));
        }
        y += input_h + 4.0;

        // Pan
        ui.label_at(
            "inspector_audio_pan_label",
            "Pan",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let pan_resp = ui.text_input_at(
            "inspector_audio_pan_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_pan_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipPanEditChanged(s))
                })
            },
        );
        if pan_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipPanEdit)
            }));
        }
        y += input_h + 4.0;

        // Pitch (semitones)
        ui.label_at(
            "inspector_audio_pitch_label",
            "Pitch st",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let pitch_resp = ui.text_input_at(
            "inspector_audio_pitch_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_pitch_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipPitchEditChanged(s))
                })
            },
        );
        if pitch_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipPitchEdit)
            }));
        }
        y += input_h + 8.0;

        // ---- Phase 2 PR3: Fade In / Fade Out (length + curve) -------
        // length は text_input (beats、 0..clip_length で clamp)、 curve
        // は dropdown (Linear / Exponential / SCurve、 spec §3.5)。
        // length と curve を同 1 行に並べる: label 60 + length 80 + curve
        // 残りの 3 区分。
        let fade_curve_w = 80.0;
        let fade_len_w = (row_w - label_w - fade_curve_w - 4.0).max(40.0);
        let fade_len_x = area.x + pad + label_w;
        let fade_curve_x = fade_len_x + fade_len_w + 4.0;

        // Fade In length + curve
        ui.label_at(
            "inspector_audio_fade_in_label",
            "Fade In",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let fade_in_resp = ui.text_input_at(
            "inspector_audio_fade_in_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            &app.clip_fade_in_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipFadeInEditChanged(s))
                })
            },
        );
        if fade_in_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipFadeInEdit)
            }));
        }
        let fade_in_idx = fade_curve_to_index(summary.fade_in_curve);
        if let Some(picked) = ui.dropdown(
            "inspector_audio_fade_in_curve",
            Rect { x: fade_curve_x, y, w: fade_curve_w, h: input_h },
            FADE_CURVE_LABELS,
            fade_in_idx,
        ) {
            let target = summary.target;
            let new_curve = fade_curve_from_index(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipFadeInCurve {
                    target,
                    curve: new_curve,
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
            TEXT_DIM,
        );
        let fade_out_resp = ui.text_input_at(
            "inspector_audio_fade_out_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            &app.clip_fade_out_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipFadeOutEditChanged(s))
                })
            },
        );
        if fade_out_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipFadeOutEdit)
            }));
        }
        let fade_out_idx = fade_curve_to_index(summary.fade_out_curve);
        if let Some(picked) = ui.dropdown(
            "inspector_audio_fade_out_curve",
            Rect { x: fade_curve_x, y, w: fade_curve_w, h: input_h },
            FADE_CURVE_LABELS,
            fade_out_idx,
        ) {
            let target = summary.target;
            let new_curve = fade_curve_from_index(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipFadeOutCurve {
                    target,
                    curve: new_curve,
                })
            }));
        }
        y += input_h + 12.0;
    }

    // ---- Image Event section (`docs/plan_image_overlay.md` §4 P4) ------
    // selected_clip が `ClipContent::Image` のとき、 first event の
    // 数値入力 (x/y/w/h/opacity) と fade / mute toggle を表示。 編集
    // AppEvent は全 ImageEvent に broadcast。
    if let Some(summary) = app.inspector_image_event_summary() {
        // edit buffer の target が現選択と違ければ resync を発火 (audio
        // section と同 idiom)。 image clip 切替後に 1 frame だけ古い
        // buffer が表示されるが、 直後の frame で formatted な現値に
        // 書き戻る。
        if app.clip_edit_buffer_target != Some(summary.target) {
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
            TEXT_DIM,
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
        let target_mute = summary.target;
        let new_mute = !summary.muted;
        ui.toggle_button_at(
            "inspector_image_mute",
            "Mute",
            Rect { x: area.x + pad, y, w: row_w, h: toggle_h },
            summary.muted,
            &TOGGLE_AUDIO_BASE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipMuted {
                        target: target_mute,
                        muted: new_mute,
                    })
                })
            },
        );
        y += toggle_h + 8.0;

        // X
        ui.label_at(
            "inspector_image_x_label",
            "X",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let x_resp = ui.text_input_at(
            "inspector_image_x_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_image_x_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipImageXEditChanged(s))
                })
            },
        );
        if x_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipImageXEdit)
            }));
        }
        let x_auto_on = summary.x_automated;
        ui.toggle_button_at(
            "inspector_image_x_automate",
            "A",
            Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
            x_auto_on,
            &TOGGLE_IMAGE_AUTOMATE,
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
            TEXT_DIM,
        );
        let y_resp = ui.text_input_at(
            "inspector_image_y_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_image_y_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipImageYEditChanged(s))
                })
            },
        );
        if y_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipImageYEdit)
            }));
        }
        let y_auto_on = summary.y_automated;
        ui.toggle_button_at(
            "inspector_image_y_automate",
            "A",
            Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
            y_auto_on,
            &TOGGLE_IMAGE_AUTOMATE,
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
            TEXT_DIM,
        );
        let w_resp = ui.text_input_at(
            "inspector_image_w_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_image_w_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipImageWEditChanged(s))
                })
            },
        );
        if w_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipImageWEdit)
            }));
        }
        let w_auto_on = summary.w_automated;
        ui.toggle_button_at(
            "inspector_image_w_automate",
            "A",
            Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
            w_auto_on,
            &TOGGLE_IMAGE_AUTOMATE,
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
            TEXT_DIM,
        );
        let h_resp = ui.text_input_at(
            "inspector_image_h_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_image_h_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipImageHEditChanged(s))
                })
            },
        );
        if h_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipImageHEdit)
            }));
        }
        let h_auto_on = summary.h_automated;
        ui.toggle_button_at(
            "inspector_image_h_automate",
            "A",
            Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
            h_auto_on,
            &TOGGLE_IMAGE_AUTOMATE,
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
            TEXT_DIM,
        );
        let opacity_resp = ui.text_input_at(
            "inspector_image_opacity_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_image_opacity_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipImageOpacityEditChanged(s))
                })
            },
        );
        if opacity_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipImageOpacityEdit)
            }));
        }
        let opacity_auto_on = summary.opacity_automated;
        ui.toggle_button_at(
            "inspector_image_opacity_automate",
            "A",
            Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
            opacity_auto_on,
            &TOGGLE_IMAGE_AUTOMATE,
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
            TEXT_DIM,
        );
        let rotation_resp = ui.text_input_at(
            "inspector_image_rotation_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            &app.clip_image_rotation_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipImageRotationEditChanged(s))
                })
            },
        );
        if rotation_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipImageRotationEdit)
            }));
        }
        let rotation_auto_on = summary.rotation_automated;
        ui.toggle_button_at(
            "inspector_image_rotation_automate",
            "A",
            Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
            rotation_auto_on,
            &TOGGLE_IMAGE_AUTOMATE,
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
        y += input_h + 8.0;

        // Fade In / Out (length + curve)。 audio section と同じ idiom
        // で 1 行に length + curve dropdown を並べる。
        let fade_curve_w = 80.0;
        let fade_len_w = (row_w - label_w - fade_curve_w - 4.0).max(40.0);
        let fade_len_x = area.x + pad + label_w;
        let fade_curve_x = fade_len_x + fade_len_w + 4.0;

        // Fade In
        ui.label_at(
            "inspector_image_fade_in_label",
            "Fade In",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let fade_in_resp = ui.text_input_at(
            "inspector_image_fade_in_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            &app.clip_fade_in_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipFadeInEditChanged(s))
                })
            },
        );
        if fade_in_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipFadeInEdit)
            }));
        }
        let fade_in_idx = fade_curve_to_index(summary.fade_in_curve);
        if let Some(picked) = ui.dropdown(
            "inspector_image_fade_in_curve",
            Rect { x: fade_curve_x, y, w: fade_curve_w, h: input_h },
            FADE_CURVE_LABELS,
            fade_in_idx,
        ) {
            let target = summary.target;
            let new_curve = fade_curve_from_index(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipFadeInCurve {
                    target,
                    curve: new_curve,
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
            TEXT_DIM,
        );
        let fade_out_resp = ui.text_input_at(
            "inspector_image_fade_out_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            &app.clip_fade_out_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipFadeOutEditChanged(s))
                })
            },
        );
        if fade_out_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipFadeOutEdit)
            }));
        }
        let fade_out_idx = fade_curve_to_index(summary.fade_out_curve);
        if let Some(picked) = ui.dropdown(
            "inspector_image_fade_out_curve",
            Rect { x: fade_curve_x, y, w: fade_curve_w, h: input_h },
            FADE_CURVE_LABELS,
            fade_out_idx,
        ) {
            let target = summary.target;
            let new_curve = fade_curve_from_index(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipFadeOutCurve {
                    target,
                    curve: new_curve,
                })
            }));
        }
        y += input_h + 12.0;
    }

    // ---- Group Transform section (`docs/plan_tachie_group_transform.md` §5.5) --
    // cursor track が visual group のとき、立ち絵全体の 2D affine + opacity を
    // 数値編集 + per-param「A」automate トグルで expose。image inspector と同
    // idiom。group は表示 clip を持たないので clip 選択ではなく track 選択が
    // トリガ（§5.5）。純 audio バスには出ない（§5.6 group_has_visual_content）。
    if let Some(summary) = app.inspector_group_transform_summary() {
        ui.label_at(
            "inspector_group_transform_label",
            "Group Transform",
            area.x + pad,
            y,
            12.0,
            TEXT_DIM,
        );
        y += 18.0;

        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 64.0;
        let auto_btn_w = 22.0;
        let auto_btn_gap = 4.0;
        let input_x = area.x + pad + label_w;
        let input_w = row_w - label_w - auto_btn_w - auto_btn_gap;
        let auto_btn_x = input_x + input_w + auto_btn_gap;
        let track_id = summary.track_id;
        let gt = summary.transform;

        for param in crate::app::GROUP_PARAMS {
            use common::model::GroupTransformParam as G;
            let idx = crate::app::group_param_index(param);
            // param 別の (現値, default, 書式, range, sensitivity[units/px], label)。
            // Rotation は degree 表示 / 入力（model は radians）。
            #[allow(clippy::type_complexity)]
            let (value, default, fmt, range, sens, label): (
                f64,
                f64,
                ScrubableNumberFormat,
                Option<(f64, f64)>,
                f32,
                &str,
            ) = match param {
                G::X => (gt.x.into(), 0.0, ScrubableNumberFormat::Decimal(3), None, 0.004, "X"),
                G::Y => (gt.y.into(), 0.0, ScrubableNumberFormat::Decimal(3), None, 0.004, "Y"),
                G::Rotation => (
                    gt.rotation_radians.to_degrees().into(),
                    0.0,
                    ScrubableNumberFormat::Decimal(1),
                    None,
                    1.0,
                    "Rot (°)",
                ),
                G::ScaleX => (
                    gt.scale_x.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.1, 10.0)),
                    0.01,
                    "ScaleX",
                ),
                G::ScaleY => (
                    gt.scale_y.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.1, 10.0)),
                    0.01,
                    "ScaleY",
                ),
                G::AnchorX => (
                    gt.anchor_x.into(),
                    0.5,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    "AnchorX",
                ),
                G::AnchorY => (
                    gt.anchor_y.into(),
                    0.5,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    "AnchorY",
                ),
                G::Opacity => (
                    gt.opacity.into(),
                    1.0,
                    ScrubableNumberFormat::Decimal(3),
                    Some((0.0, 1.0)),
                    0.004,
                    "Opacity",
                ),
            };
            ui.label_at((param, "group_label"), label, area.x + pad, y + 5.0, 11.0, TEXT_DIM);
            let style =
                ScrubableNumberStyle { sensitivity: sens, range, ..SCRUB_STYLE_GROUP };
            let resp = ui.scrubable_number_at(
                (param, "group_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                value,
                default,
                fmt,
                &style,
                "Group Transform",
                move |v| {
                    // Rotation は degree 入力 → radians に変換して設定。
                    let value = if matches!(param, G::Rotation) {
                        (v as f32).to_radians()
                    } else {
                        v as f32
                    };
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetGroupTransformField {
                            track_id,
                            param,
                            value,
                        })
                    })
                },
            );
            // drag / text 編集の開始・終了 edge で undo を 1 step に bracket。
            let active = resp.dragging || resp.editing_text;
            let was_active = app.group_scrub_active == Some(param);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.group_scrub_active = Some(param);
                    app.handle_event(AppEvent::BeginGroupTransformDrag);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.group_scrub_active = None;
                    app.handle_event(AppEvent::EndGroupTransformDrag);
                }));
            }
            let auto_on = summary.automated[idx];
            ui.toggle_button_at(
                (param, "group_auto"),
                "A",
                Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
                auto_on,
                &TOGGLE_IMAGE_AUTOMATE,
                move |_| {
                    Edit::mutate(move |app: &mut AppData| {
                        let ev = if auto_on {
                            AppEvent::RemoveGroupAutomationLane { param }
                        } else {
                            AppEvent::AddGroupAutomationLane { param }
                        };
                        app.handle_event(ev);
                    })
                },
            );
            y += input_h + 4.0;
        }
        y += 8.0;
    }

    // ---- Text Event section (`docs/plan_text_overlay.md` §4 P5 + P5.B) --
    // selected_clip が `ClipContent::Text` のとき、 first event の全 field
    // (text / font / align / 23 numeric + 2 fade beats / fade curves / mute)
    // を expose。 numeric field は `TextNumField` discriminator で
    // `ClipTextNumEditChanged` / `CommitClipTextNumEdit` に dispatch。
    if let Some(summary) = app.inspector_text_event_summary() {
        if app.clip_edit_buffer_target != Some(summary.target) {
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
            TEXT_DIM,
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
        let target_mute = summary.target;
        let new_mute = !summary.muted;
        ui.toggle_button_at(
            "inspector_text_mute",
            "Mute",
            Rect { x: area.x + pad, y, w: row_w, h: toggle_h },
            summary.muted,
            &TOGGLE_AUDIO_BASE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipTextMuted {
                        target: target_mute,
                        muted: new_mute,
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
            TEXT_DIM,
        );
        let text_resp = ui.text_input_at(
            "inspector_text_content_input",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            &app.clip_text_content_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipTextContentEditChanged(s))
                })
            },
        );
        if text_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipTextContentEdit)
            }));
        }
        y += input_h + 4.0;

        // Font family (system font name; "" = renderer default)
        ui.label_at(
            "inspector_text_font_label",
            "Font",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let font_resp = ui.text_input_at(
            "inspector_text_font_input",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            &app.clip_text_font_family_edit_text,
            |s| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ClipTextFontFamilyEditChanged(s))
                })
            },
        );
        if font_resp.committed {
            ui.push_edit(Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::CommitClipTextFontFamilyEdit)
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
            TEXT_DIM,
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
            let target_align = summary.target;
            let new_align = match picked {
                0 => TextAlign::Left,
                2 => TextAlign::Right,
                _ => TextAlign::Center,
            };
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipTextAlign {
                    target: target_align,
                    value: new_align,
                })
            }));
        }
        y += input_h + 8.0;

        // 23 numeric rows + 2 fade beats を 1 closure 化した helper で
        // 順次 emit。 Hash-able な `(field, &'static str)` を id にし、
        // 各 field 用の widget 識別子を per-field ユニークにする。
        let emit_num_row = |ui: &mut Ui<'_, AppData>,
                            label: &str,
                            field: TextNumField,
                            row_y: &mut f32| {
            let buffer = app
                .clip_text_num_edits
                .get(&field)
                .cloned()
                .unwrap_or_default();
            ui.label_at(
                (field, "label"),
                label,
                area.x + pad,
                *row_y + 5.0,
                11.0,
                TEXT_DIM,
            );
            let resp = ui.text_input_at(
                (field, "input"),
                Rect { x: input_x, y: *row_y, w: numeric_input_w, h: input_h },
                &buffer,
                move |s| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::ClipTextNumEditChanged {
                            field,
                            value: s,
                        })
                    })
                },
            );
            if resp.committed {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::CommitClipTextNumEdit { field })
                }));
            }
            if let Some(builtin) = text_num_to_builtin(field) {
                let auto_on = summary.automated.contains(&builtin);
                ui.toggle_button_at(
                    (field, "auto"),
                    "A",
                    Rect { x: auto_btn_x, y: *row_y, w: auto_btn_w, h: input_h },
                    auto_on,
                    &TOGGLE_IMAGE_AUTOMATE,
                    move |_| {
                        Edit::mutate(move |app: &mut AppData| {
                            let ev = if auto_on {
                                AppEvent::RemoveTextAutomationLane { field: builtin }
                            } else {
                                AppEvent::AddTextAutomationLane { field: builtin }
                            };
                            app.handle_event(ev);
                        })
                    },
                );
            }
            *row_y += input_h + 4.0;
        };

        emit_num_row(ui, "X", TextNumField::X, &mut y);
        emit_num_row(ui, "Y", TextNumField::Y, &mut y);
        emit_num_row(ui, "W", TextNumField::W, &mut y);
        emit_num_row(ui, "H", TextNumField::H, &mut y);
        emit_num_row(ui, "Rot (°)", TextNumField::Rotation, &mut y);
        emit_num_row(ui, "Size (px)", TextNumField::FontSize, &mut y);
        emit_num_row(ui, "Opacity", TextNumField::Opacity, &mut y);
        emit_num_row(ui, "Fill R", TextNumField::FillR, &mut y);
        emit_num_row(ui, "Fill G", TextNumField::FillG, &mut y);
        emit_num_row(ui, "Fill B", TextNumField::FillB, &mut y);
        emit_num_row(ui, "Fill A", TextNumField::FillA, &mut y);
        emit_num_row(ui, "Out R", TextNumField::OutlineR, &mut y);
        emit_num_row(ui, "Out G", TextNumField::OutlineG, &mut y);
        emit_num_row(ui, "Out B", TextNumField::OutlineB, &mut y);
        emit_num_row(ui, "Out A", TextNumField::OutlineA, &mut y);
        emit_num_row(ui, "Out W (px)", TextNumField::OutlineWidth, &mut y);
        emit_num_row(ui, "Sh R", TextNumField::ShadowR, &mut y);
        emit_num_row(ui, "Sh G", TextNumField::ShadowG, &mut y);
        emit_num_row(ui, "Sh B", TextNumField::ShadowB, &mut y);
        emit_num_row(ui, "Sh A", TextNumField::ShadowA, &mut y);
        emit_num_row(ui, "Sh X (px)", TextNumField::ShadowOffsetX, &mut y);
        emit_num_row(ui, "Sh Y (px)", TextNumField::ShadowOffsetY, &mut y);
        emit_num_row(ui, "Sh Blur (px)", TextNumField::ShadowBlur, &mut y);
        emit_num_row(ui, "Fade In", TextNumField::FadeInBeats, &mut y);
        emit_num_row(ui, "Fade Out", TextNumField::FadeOutBeats, &mut y);

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
            TEXT_DIM,
        );
        if let Some(picked) = ui.dropdown(
            "inspector_text_fade_in_curve",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            FADE_CURVE_LABELS,
            curve_to_idx(summary.fade_in_curve),
        ) {
            let target_curve = summary.target;
            let new_curve = idx_to_curve(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipTextFadeInCurve {
                    target: target_curve,
                    curve: new_curve,
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
            TEXT_DIM,
        );
        if let Some(picked) = ui.dropdown(
            "inspector_text_fade_out_curve",
            Rect { x: input_x, y, w: string_input_w, h: input_h },
            FADE_CURVE_LABELS,
            curve_to_idx(summary.fade_out_curve),
        ) {
            let target_curve = summary.target;
            let new_curve = idx_to_curve(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetClipTextFadeOutCurve {
                    target: target_curve,
                    curve: new_curve,
                })
            }));
        }
        y += input_h + 12.0;
        // suppress unused warning when section emits nothing further
        let _ = (label_w, summary);
    }

    // Vocal source 編集 (Vocal track のときのみ)
    if let Some(track) = app.song.tracks.get(app.cursor_track_index().unwrap_or(0))
        && let common::model::InstrumentSource::Vocal { speaker_id, .. } = &track.source
    {
        ui.label_at(
            "inspector_vocal_label",
            "Vocal Speaker",
            area.x + pad,
            y,
            12.0,
            TEXT_DIM,
        );
        y += 18.0;
        let dropdown_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 24.0,
        };
        // singers が空 (engine 未起動 / fetch 失敗) なら placeholder ラベルだけ
        if app.singers.is_empty() {
            ui.label_at(
                "inspector_vocal_placeholder",
                "(VOICEVOX engine 未起動 — speaker 一覧取得待ち)",
                dropdown_rect.x + 4.0,
                dropdown_rect.y + 6.0,
                11.0,
                TEXT_DIM,
            );
        } else {
            // 各 singer の各 style を 1 entry に flatten。
            // 「<キャラ名> - <スタイル名>」 を表示、 selected_idx は speaker_id 一致で決定
            let entries: Vec<(u32, String, String)> = app
                .singers
                .iter()
                .flat_map(|s| {
                    s.styles.iter().map(move |st| {
                        (
                            st.id,
                            s.name.clone(),
                            st.name.clone(),
                        )
                    })
                })
                .collect();
            let labels: Vec<String> = entries
                .iter()
                .map(|(_, n, sn)| format!("{n} - {sn}"))
                .collect();
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let selected_idx = entries
                .iter()
                .position(|(id, _, _)| *id == *speaker_id)
                .unwrap_or(0);
            if let Some(picked) = ui.dropdown(
                "inspector_vocal_dropdown",
                dropdown_rect,
                &label_refs,
                selected_idx,
            ) && let Some((id, _, style_name)) = entries.get(picked)
            {
                let track_idx = app.cursor_track_index().unwrap_or(0) as u32;
                let new_id = *id;
                let new_style = style_name.clone();
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetTrackSpeaker {
                        track: track_idx,
                        speaker_id: new_id,
                        style_name: new_style.clone(),
                    });
                }));
            }
        }
        y += 30.0;
    }

    // ---- Parent group dropdown ---------------------------------------
    // Reaper folder / Live group equivalent. The selected track can
    // optionally be reparented under any other track that already has
    // children (or any track really — the cycle check in
    // `action_set_track_parent` rejects bad picks). Master bus =
    // "(top-level)" sentinel.
    if let Some(track) = app.song.tracks.get(app.cursor_track_index().unwrap_or(0)) {
        // Candidates: tracks that already have at least one child (= are
        // groups in the Reaper-folder sense), excluding the selected
        // track itself and any of its descendants. Picking a non-group
        // track as parent is also valid, but the dropdown only surfaces
        // existing groups — to convert a regular track into a group,
        // the user picks it as parent here and the act of pointing at
        // it makes it one. PR2 phase 1 keeps the simpler "groups only"
        // candidate list; expand later if it surfaces as a friction.
        let groups: Vec<(u32, String)> = app
            .song
            .tracks
            .iter()
            .filter(|t| app.is_group_track(t.id) && t.id != track.id)
            .map(|t| (t.id, if t.name.is_empty() { format!("Group {}", t.id) } else { t.name.clone() }))
            .collect();

        ui.label_at(
            "inspector_parent_label",
            "Parent",
            area.x + pad,
            y,
            12.0,
            TEXT_DIM,
        );
        y += 18.0;

        let dropdown_rect = Rect {
            x: area.x + pad,
            y,
            w: area.w - pad * 2.0,
            h: 24.0,
        };

        // Build option list: "(top-level)" then every other group track.
        let mut labels: Vec<String> = Vec::with_capacity(groups.len() + 1);
        labels.push("(top-level)".into());
        labels.extend(groups.iter().map(|(_, n)| n.clone()));
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();

        let selected_idx = match track.parent_group_id {
            None => 0,
            Some(pid) => groups
                .iter()
                .position(|(id, _)| *id == pid)
                .map(|i| i + 1)
                .unwrap_or(0),
        };

        if let Some(picked) = ui.dropdown(
            "inspector_parent_dropdown",
            dropdown_rect,
            &label_refs,
            selected_idx,
        ) {
            let new_parent = if picked == 0 {
                None
            } else {
                groups.get(picked - 1).map(|(id, _)| *id)
            };
            let track_id = track.id;
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetTrackParent {
                    track_id,
                    parent_id: new_parent,
                });
            }));
        }
        y += 30.0;
    }

    // 「Chain」見出し
    ui.label_at(
        "inspector_chain_label",
        "Chain",
        area.x + pad,
        y,
        12.0,
        TEXT_DIM,
    );
    y += 18.0;

    let chain = app.inspector_chain();
    let btns_h = 26.0;
    let btns_y = area.y + area.h - btns_h - pad;

    // Sidechain section: only render if there's at least one plugin in the
    // chain. Vertical budget: 18 px header + 4 px gap + (24 + 4) px per row,
    // capped at 4 rows; if more entries exist they overflow off-screen
    // (vertical scrolling not yet implemented in inspector). Without this
    // dynamic budget the chain list would always shrink even when no
    // sidechain wiring is meaningful.
    let sc_entries = app.sidechain_entries();
    let sc_section_h = if sc_entries.is_empty() {
        0.0
    } else {
        let row_h = 24.0;
        let row_gap = 4.0;
        let visible_rows = sc_entries.len().min(4);
        18.0 + 4.0 + visible_rows as f32 * (row_h + row_gap) + 6.0
    };

    let list_x = area.x + pad;
    let list_y = y;
    let list_w = area.w - pad * 2.0;
    let list_h = (btns_y - 6.0 - list_y - sc_section_h).max(0.0);
    let list_rect = Rect { x: list_x, y: list_y, w: list_w, h: list_h };

    let btn_gui_w = 44.0;
    let btn_x_w = 30.0;

    ui.reorderable_list(
        "inspector_chain",
        list_rect,
        &chain,
        None,
        &CHAIN_LIST_STYLE,
        |req| match req {
            ReorderableListEditRequest::Reorder(order) => Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::ReorderInspectorChain(order.clone()));
            }),
        },
        |ui, entry, idx, row_rect, _selected, _dragging| {
            ui.label_at(
                ("inspector_row_section", idx),
                &entry.section_label,
                row_rect.x + 6.0,
                row_rect.y + 8.0,
                10.0,
                SECTION_TEXT,
            );
            ui.label_at(
                ("inspector_row_name", idx),
                &entry.plugin_name,
                row_rect.x + 60.0,
                row_rect.y + 8.0,
                11.0,
                TEXT,
            );
            let kind = entry.slot_kind;
            let index = entry.slot_index;
            let gui_x = row_rect.x + row_rect.w - btn_gui_w - btn_x_w - 4.0;
            ui.button_at(
                ("inspector_row_gui", idx),
                "GUI",
                Rect {
                    x: gui_x,
                    y: row_rect.y + 2.0,
                    w: btn_gui_w,
                    h: row_rect.h - 4.0,
                },
                move || {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::ToggleSlotGui {
                            slot_kind: kind,
                            slot_index: index,
                        })
                    })
                },
            );
            let xb_x = row_rect.x + row_rect.w - btn_x_w;
            ui.button_at(
                ("inspector_row_remove", idx),
                "x",
                Rect {
                    x: xb_x,
                    y: row_rect.y + 2.0,
                    w: btn_x_w,
                    h: row_rect.h - 4.0,
                },
                move || {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::RemoveSlot {
                            slot_kind: kind,
                            slot_index: index,
                        })
                    })
                },
            );
        },
    );

    // ---- Sidechain section ------------------------------------------
    // PR4.5 sidechain UI: per-plugin source picker. ECS-flat dropdown per
    // chain row so the user can wire any track's output into the plugin's
    // first aux input port (sidechain_sources[0]). Self-track is filtered
    // out (would create a feedback cycle which `compile_schedule` rejects).
    // Only the first aux port is exposed; multi-port plugins (rare) still
    // need editing via .daw file or follow-up UI.
    if !sc_entries.is_empty() {
        let sc_header_y = btns_y - sc_section_h;
        ui.label_at(
            "inspector_sc_label",
            "Sidechain",
            area.x + pad,
            sc_header_y,
            12.0,
            TEXT_DIM,
        );
        let row_h = 24.0;
        let row_gap = 4.0;
        let mut row_y = sc_header_y + 18.0 + 4.0;
        let visible_rows = sc_entries.len().min(4);
        let choices = app.sidechain_source_choices();
        let labels: Vec<String> = choices.iter().map(|c| c.label.clone()).collect();
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let dropdown_w = 140.0;
        let name_x = area.x + pad;
        let dropdown_x = area.x + area.w - pad - dropdown_w;
        for (i, entry) in sc_entries.iter().take(visible_rows).enumerate() {
            // Truncate plugin name visually if it's too long for the
            // available left half — gui_01 doesn't auto-clip, so we
            // budget by char count (rough; mono font assumption). Use
            // `chars().take()` for UTF-8 safety: byte-slicing would panic
            // on multi-byte boundaries (Japanese / fancy plugin names).
            let max_name_chars = ((dropdown_x - name_x - 8.0) / 7.0) as usize;
            let n_chars = entry.plugin_name.chars().count();
            let display_name = if n_chars > max_name_chars && max_name_chars > 3 {
                let truncated: String =
                    entry.plugin_name.chars().take(max_name_chars - 1).collect();
                format!("{truncated}…")
            } else {
                entry.plugin_name.clone()
            };
            ui.label_at(
                ("inspector_sc_name", i),
                &display_name,
                name_x,
                row_y + 6.0,
                11.0,
                TEXT,
            );
            let dropdown_rect = Rect {
                x: dropdown_x,
                y: row_y,
                w: dropdown_w,
                h: row_h,
            };
            let selected_idx = match entry.current_source {
                None => 0,
                Some(src_id) => choices
                    .iter()
                    .position(|c| c.track_id == Some(src_id))
                    .unwrap_or(0),
            };
            if let Some(picked) = ui.dropdown(
                ("inspector_sc_dropdown", i),
                dropdown_rect,
                &label_refs,
                selected_idx,
            ) && let Some(choice) = choices.get(picked)
            {
                let track_id = entry.track_id;
                let slot_kind = entry.slot_kind;
                let slot_index = entry.slot_index;
                let new_source = choice.track_id;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetSidechainSource {
                        track_id,
                        slot_kind,
                        slot_index,
                        port: 0,
                        source: new_source,
                    });
                }));
            }
            row_y += row_h + row_gap;
        }
    }

    // 下端: + Inst / + FX / + MIDI FX。Reaper folder 流で group track
    // も全機能を持てる仕様 (plan_group_track.md §1)、よって group も
    // 普通 track と同じ 3 ボタン表示。
    // master bus は audio fx のみ持つので「+ FX」 1 本だけを全幅で出す。
    if app.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
        ui.button_at(
            "inspector_add_fx",
            "+ FX",
            Rect { x: area.x + pad, y: btns_y, w: area.w - pad * 2.0, h: btns_h },
            || {
                Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx))
                })
            },
        );
        return;
    }
    let btn_w = (area.w - pad * 2.0 - 12.0) / 3.0;
    ui.button_at(
        "inspector_add_inst",
        "+ Inst",
        Rect { x: area.x + pad, y: btns_y, w: btn_w, h: btns_h },
        || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Instrument))
            })
        },
    );
    ui.button_at(
        "inspector_add_fx",
        "+ FX",
        Rect { x: area.x + pad + btn_w + 6.0, y: btns_y, w: btn_w, h: btns_h },
        || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::Fx))
            })
        },
    );
    ui.button_at(
        "inspector_add_midi_fx",
        "+ MIDI",
        Rect {
            x: area.x + pad + (btn_w + 6.0) * 2.0,
            y: btns_y,
            w: btn_w,
            h: btns_h,
        },
        || {
            Edit::mutate(|app: &mut AppData| {
                app.handle_event(AppEvent::OpenPluginPickerFor(PickerTarget::MidiFx))
            })
        },
    );
}
