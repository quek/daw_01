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
    text_num_to_builtin, AppData, AppEvent, ClipRef, ColorPickerTarget, InspectorScrubField,
    TextNumField,
};
use crate::view::track_color;
use common::model::{FadeCurve, ImageBuiltinParam, StretchMode, TextAlign};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
const TEXT_DIM: Color = Color { r: 0.62, g: 0.65, b: 0.70, a: 1.0 };
const ROW_BG: Color = Color { r: 0.20, g: 0.20, b: 0.24, a: 1.0 };
const ROW_BG_HOVER: Color = Color { r: 0.24, g: 0.24, b: 0.30, a: 1.0 };
const ROW_BG_DRAGGING: Color = Color { r: 0.30, g: 0.40, b: 0.55, a: 0.85 };
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

/// FIXME #15 (`docs/plan_inspector_scrub.md`): audio / image / text inspector
/// の数値 field を 1 行ぶん描く共通 helper。 `ui.scrubable_number_at` を呼び、
/// on_change で `make_event(v)` が返す `AppEvent` を全 event に broadcast、
/// drag / text 編集の開始・終了 edge で `BeginInspectorScrub` /
/// `EndInspectorScrub` を発火して一連の操作を undo 1 step に bracket する
/// (= Group Transform セクションと同 idiom)。 `scrub_key` は
/// `app.inspector_scrub_active` の識別に使う。
#[allow(clippy::too_many_arguments)]
fn scrub_field(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    id: impl std::hash::Hash,
    rect: Rect,
    value: Option<f64>,
    default: f64,
    fmt: ScrubableNumberFormat,
    style: &ScrubableNumberStyle,
    scrub_key: InspectorScrubField,
    make_event: impl Fn(ClipRef, f64) -> AppEvent + Clone + Send + Sync + 'static,
) {
    // FIXME #46: 複数選択時は inspector_target_refs 全体へ broadcast する。 値が
    // 割れている field は `value == None` で渡され、 placeholder「—」を表示 (編集
    // 開始で base = default に戻る)。 `mutate_*_events_in_clip` は variant-safe なので、
    // broadcast 先に種別違いのクリップが混ざっても no-op で安全 (= その field を
    // 持つクリップにだけ適用される)。
    let base = value.unwrap_or(default);
    let placeholder = if value.is_none() { Some("\u{2014}") } else { None };
    let resp = ui.scrubable_number_at(
        id,
        rect,
        base,
        default,
        fmt,
        style,
        "Inspector",
        move |v| {
            let make_event = make_event.clone();
            Edit::mutate(move |app: &mut AppData| {
                // 編集 (drag/text) 発火時のみ対象を解決する。 selection は 1 ストローク中
                // 変わらないので edit 時点で十分で、 毎フレームの Vec alloc を避けられる。
                for t in app.inspector_target_refs() {
                    app.handle_event(make_event(t, v));
                }
            })
        },
        placeholder,
    );
    // drag / text 編集の開始・終了 edge で undo を 1 step に bracket。
    let active = resp.dragging || resp.editing_text;
    let was_active = app.inspector_scrub_active == Some(scrub_key);
    if active && !was_active {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.inspector_scrub_active = Some(scrub_key);
            app.handle_event(AppEvent::BeginInspectorScrub);
        }));
    } else if !active && was_active {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.inspector_scrub_active = None;
            app.handle_event(AppEvent::EndInspectorScrub);
        }));
    }
}

/// FIXME #15: audio / image / text inspector の scrubable_number base style。
/// sensitivity / range は field 別に上書きする (= Group Transform と同 idiom、
/// `SCRUB_STYLE_GROUP` を共有)。
const SCRUB_STYLE_INSPECTOR: ScrubableNumberStyle = SCRUB_STYLE_GROUP;

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

/// import 済み image source の表示名 (ファイル名)。口パク mapping dropdown 用。
fn image_source_label(src: &common::model::ImageSource) -> String {
    // import 時に保持した元ファイル名を優先 (on-disk path は content addressing
    // で sanitize / hash 済みなので日本語名が潰れて区別できない)。 v21 以前の
    // project は `name` 未保持 (空文字) なので path の file_name に fallback。
    if !src.name.is_empty() {
        return src.name.clone();
    }
    let path = match &src.path {
        common::model::ImageSourcePath::ProjectRelative(p)
        | common::model::ImageSourcePath::Absolute(p) => p,
    };
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string()
}

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

    // ---- FIXME #23: param セクションを縦スクロール領域に収める --------------
    // title 下〜area 下端を param viewport (上、 scroll) と chain band (下、 pinned)
    // に分割する。 param の実高さは前フレーム測定値 (`inspector_body_h`、
    // immediate-mode の lag-by-one) を content_size に使い、 viewport = content と
    // max_param_h の小さい方。 content <= viewport なら scrollbar 無しで chain が
    // すぐ下に続く。 dropdown popup は deferred buffer 描画なので clip_rect の外に
    // 出て切れない (gui_01 popup.rs)。 closure body の param セクションは既存コード
    // のまま (再インデントしない)。
    const CHAIN_MIN_H: f32 = 160.0;
    let body_top = y;
    let max_param_h = (area.y + area.h - body_top - CHAIN_MIN_H).max(0.0);
    let content_h = app.inspector_body_h.max(1.0);
    let param_h = content_h.min(max_param_h);
    let param_vp = Rect { x: area.x, y: body_top, w: area.w, h: param_h };
    let boundary_y = body_top + param_h;
    let measured_body_h = std::cell::Cell::new(0.0_f32);
    ui.scroll_area("inspector_body", param_vp, (param_vp.w, content_h), |ui, scroll_off| {
    let mut y = body_top - scroll_off.1;

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

        // Gain dB (-80..24)
        ui.label_at(
            "inspector_audio_gain_label",
            "Gain dB",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
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
                ..SCRUB_STYLE_INSPECTOR
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
            TEXT_DIM,
        );
        scrub_field(
            ui,
            app,
            "inspector_audio_pan_input",
            Rect { x: input_x, y, w: input_w, h: input_h },
            app.inspector_fold(|a, t| a.audio_first_event(t, |e| f64::from(e.pan))),
            0.0,
            ScrubableNumberFormat::Decimal(2),
            &ScrubableNumberStyle {
                sensitivity: 0.004,
                range: Some((-1.0, 1.0)),
                ..SCRUB_STYLE_INSPECTOR
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
            TEXT_DIM,
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
                range: Some((-96.0, 96.0)),
                ..SCRUB_STYLE_INSPECTOR
            },
            InspectorScrubField::Pitch,
            move |t, v| AppEvent::SetClipPitchSemitones { target: t, semitones: v as f32 },
        );
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

        let fade_max = summary.clip_length_beats.max(0.0);

        // Fade In length + curve
        ui.label_at(
            "inspector_audio_fade_in_label",
            "Fade In",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        scrub_field(
            ui,
            app,
            "inspector_audio_fade_in_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            app.inspector_fold(|a, t| a.audio_first_event(t, |e| e.fade_in_beats)),
            0.0,
            ScrubableNumberFormat::Decimal(3),
            &ScrubableNumberStyle {
                sensitivity: 0.01,
                range: Some((0.0, fade_max)),
                ..SCRUB_STYLE_INSPECTOR
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
        scrub_field(
            ui,
            app,
            "inspector_audio_fade_out_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            app.inspector_fold(|a, t| a.audio_first_event(t, |e| e.fade_out_beats)),
            0.0,
            ScrubableNumberFormat::Decimal(3),
            &ScrubableNumberStyle {
                sensitivity: 0.01,
                range: Some((0.0, fade_max)),
                ..SCRUB_STYLE_INSPECTOR
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

        // PiP rect / opacity の scrubable は 0..1 normalized、 細かい step。
        let style_unit = ScrubableNumberStyle {
            sensitivity: 0.004,
            range: Some((0.0, 1.0)),
            ..SCRUB_STYLE_INSPECTOR
        };

        // X
        ui.label_at(
            "inspector_image_x_label",
            "X",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
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
                range: None,
                ..SCRUB_STYLE_INSPECTOR
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

        let fade_max = summary.clip_length_beats.max(0.0);

        // Fade In
        ui.label_at(
            "inspector_image_fade_in_label",
            "Fade In",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        scrub_field(
            ui,
            app,
            "inspector_image_fade_in_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            app.inspector_fold(|a, t| a.image_first_event(t, |e| e.fade_in_beats)),
            0.0,
            ScrubableNumberFormat::Decimal(3),
            &ScrubableNumberStyle {
                sensitivity: 0.01,
                range: Some((0.0, fade_max)),
                ..SCRUB_STYLE_INSPECTOR
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
        scrub_field(
            ui,
            app,
            "inspector_image_fade_out_input",
            Rect { x: fade_len_x, y, w: fade_len_w, h: input_h },
            app.inspector_fold(|a, t| a.image_first_event(t, |e| e.fade_out_beats)),
            0.0,
            ScrubableNumberFormat::Decimal(3),
            &ScrubableNumberStyle {
                sensitivity: 0.01,
                range: Some((0.0, fade_max)),
                ..SCRUB_STYLE_INSPECTOR
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
                None,
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
    // を expose。 FIXME #15: numeric field は scrubable_number 化され、
    // on_change が `TextNumField` discriminator 付き `SetClipTextNumField` を
    // 直接 dispatch する (drag / type 両対応、 undo は Begin/EndInspectorScrub)。
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

        // Font family: ボタンで font picker を開く (FIXME #25)。ラベルは現在の
        // フォント (空 = デフォルト)。検索付きモーダルで選び、 ↑↓ / ホバーで
        // キャンバスにライブプレビューされる。
        ui.label_at(
            "inspector_text_font_label",
            "Font",
            area.x + pad,
            y + 5.0,
            11.0,
            TEXT_DIM,
        );
        let font_btn_label = if app.clip_text_font_family_edit_text.is_empty() {
            "(default)".to_string()
        } else {
            app.clip_text_font_family_edit_text.clone()
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

        // FIXME #15: 23 numeric rows + 2 fade beats を 1 closure 化した helper。
        // 各行を scrubable_number 化し (= 1 箇所変更で 25 行全部が drag 対応)、
        // `scrub_field` で drag / text 編集を undo 1 step に bracket する。
        // automate 「A」 トグルは従来どおり共存。 値源は summary の first event
        // snapshot、 on_change は `SetClipTextNumField` (Rotation は deg→rad)。
        let text_fade_max = summary.clip_length_beats.max(0.0);
        let emit_num_row = |ui: &mut Ui<'_, AppData>,
                            app: &AppData,
                            label: &str,
                            field: TextNumField,
                            row_y: &mut f32| {
            ui.label_at(
                (field, "label"),
                label,
                area.x + pad,
                *row_y + 5.0,
                11.0,
                TEXT_DIM,
            );
            // field 別の (書式, range, sensitivity[units/px])。 clamp は handler
            // (`set_clip_text_num_field`) と一致させる。
            let (fmt, range, sens): (ScrubableNumberFormat, Option<(f64, f64)>, f32) = match field {
                // PiP rect (0..1 normalized)。
                TextNumField::X | TextNumField::Y | TextNumField::W | TextNumField::H => {
                    (ScrubableNumberFormat::Decimal(3), Some((0.0, 1.0)), 0.004)
                }
                // Rotation: degree 表示、 handler が -π..π wrap (range なし)。
                TextNumField::Rotation => (ScrubableNumberFormat::Decimal(1), None, 1.0),
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
                    (ScrubableNumberFormat::Decimal(3), Some((0.0, text_fade_max)), 0.01)
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
            let style = ScrubableNumberStyle { sensitivity: sens, range, ..SCRUB_STYLE_INSPECTOR };
            scrub_field(
                ui,
                app,
                (field, "input"),
                Rect { x: input_x, y: *row_y, w: numeric_input_w, h: input_h },
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

        emit_num_row(ui, app, "X", TextNumField::X, &mut y);
        emit_num_row(ui, app, "Y", TextNumField::Y, &mut y);
        emit_num_row(ui, app, "W", TextNumField::W, &mut y);
        emit_num_row(ui, app, "H", TextNumField::H, &mut y);
        emit_num_row(ui, app, "Rot (°)", TextNumField::Rotation, &mut y);
        emit_num_row(ui, app, "Size (px)", TextNumField::FontSize, &mut y);
        emit_num_row(ui, app, "Opacity", TextNumField::Opacity, &mut y);
        emit_num_row(ui, app, "Fill R", TextNumField::FillR, &mut y);
        emit_num_row(ui, app, "Fill G", TextNumField::FillG, &mut y);
        emit_num_row(ui, app, "Fill B", TextNumField::FillB, &mut y);
        emit_num_row(ui, app, "Fill A", TextNumField::FillA, &mut y);
        emit_num_row(ui, app, "Out R", TextNumField::OutlineR, &mut y);
        emit_num_row(ui, app, "Out G", TextNumField::OutlineG, &mut y);
        emit_num_row(ui, app, "Out B", TextNumField::OutlineB, &mut y);
        emit_num_row(ui, app, "Out A", TextNumField::OutlineA, &mut y);
        emit_num_row(ui, app, "Out W (px)", TextNumField::OutlineWidth, &mut y);
        emit_num_row(ui, app, "Sh R", TextNumField::ShadowR, &mut y);
        emit_num_row(ui, app, "Sh G", TextNumField::ShadowG, &mut y);
        emit_num_row(ui, app, "Sh B", TextNumField::ShadowB, &mut y);
        emit_num_row(ui, app, "Sh A", TextNumField::ShadowA, &mut y);
        emit_num_row(ui, app, "Sh X (px)", TextNumField::ShadowOffsetX, &mut y);
        emit_num_row(ui, app, "Sh Y (px)", TextNumField::ShadowOffsetY, &mut y);
        emit_num_row(ui, app, "Sh Blur (px)", TextNumField::ShadowBlur, &mut y);
        emit_num_row(ui, app, "Fade In", TextNumField::FadeInBeats, &mut y);
        emit_num_row(ui, app, "Fade Out", TextNumField::FadeOutBeats, &mut y);

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

    // カーソルトラックの index。None のとき以降の per-track セクションは
    // 描画しない (track 0 を誤対象にしない)。
    let cursor_idx = app.cursor_track_index();

    // (FIXME #36) Clip Voice 編集: 選択中の clip が vocal track 上の MIDI clip の
    // とき、 キャラ ▼ → スタイル ▼ の 2 段 dropdown で per-clip 声を選ぶ。
    // 声は per-clip (`Clip::speaker_id`) が SSoT、 SetClipVoice で焼き込む。
    if let Some(r) = app.selected_clip_ref()
        && let Some(track) = app.song.tracks.get(r.track as usize)
        && track.is_voicevox_vocal()
        && let Some(clip) = track.clips.get(r.clip as usize)
        && app
            .song
            .clip_contents
            .get(&clip.content_id)
            .is_none_or(|c| matches!(c, common::model::ClipContent::Midi(_)))
    {
        let clip_key = common::model::ClipKey {
            track_id: track.id,
            clip_id: clip.id,
        };
        let cur_speaker = clip.speaker_id;
        // 現在の声の表示名: clip 焼き込み名 → speaker_id 逆引き → アプリ既定。
        let (cur_singer, cur_style) = if !clip.singer_name.is_empty() {
            (clip.singer_name.clone(), clip.style_name.clone())
        } else if let Some(found) = app.singers.iter().find_map(|s| {
            s.styles
                .iter()
                .find(|st| st.id == cur_speaker)
                .map(|st| (s.name.clone(), st.name.clone()))
        }) {
            found
        } else {
            (
                common::voicevox::DEFAULT_SINGER_NAME.to_string(),
                common::voicevox::DEFAULT_STYLE_NAME.to_string(),
            )
        };

        ui.label_at(
            "inspector_clip_voice_label",
            "Clip Voice",
            area.x + pad,
            y,
            12.0,
            TEXT_DIM,
        );
        y += 18.0;

        if app.singers.is_empty() {
            // engine 未起動 / 一覧未取得: 焼き込み声名 + 取得中。 声名は常に出せる。
            let txt = format!("{cur_singer} - {cur_style}  (一覧取得中…)");
            ui.label_at(
                "inspector_clip_voice_current",
                &txt,
                area.x + pad + 4.0,
                y + 6.0,
                11.0,
                TEXT_DIM,
            );
            y += 26.0;
        } else {
            // 上段: キャラ dropdown。
            let char_labels: Vec<&str> =
                app.singers.iter().map(|s| s.name.as_str()).collect();
            let cur_char_idx = app
                .singers
                .iter()
                .position(|s| s.name == cur_singer)
                .unwrap_or(0);
            let char_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_char = ui.dropdown(
                "inspector_clip_voice_char",
                char_rect,
                &char_labels,
                cur_char_idx,
            );
            y += 28.0;

            // 下段: スタイル dropdown (= 上段で選んだ or 現在のキャラの styles)。
            let char_idx = picked_char.unwrap_or(cur_char_idx).min(app.singers.len() - 1);
            let singer = &app.singers[char_idx];
            let style_labels: Vec<&str> =
                singer.styles.iter().map(|st| st.name.as_str()).collect();
            let cur_style_idx = singer
                .styles
                .iter()
                .position(|st| st.id == cur_speaker)
                .unwrap_or(0);
            let style_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_style = ui.dropdown(
                "inspector_clip_voice_style",
                style_rect,
                &style_labels,
                cur_style_idx,
            );
            y += 28.0;

            // 確定値: キャラを変えたらそのキャラの先頭 style、 style を変えたら
            // その style を採用 (= (speaker_id, singer_name, style_name))。
            let chosen: Option<(u32, String, String)> = if let Some(pc) = picked_char {
                app.singers.get(pc).and_then(|s| {
                    s.styles
                        .first()
                        .map(|st| (st.id, s.name.clone(), st.name.clone()))
                })
            } else if let Some(ps) = picked_style {
                singer
                    .styles
                    .get(ps)
                    .map(|st| (st.id, singer.name.clone(), st.name.clone()))
            } else {
                None
            };
            if let Some((sid, sn, stn)) = chosen {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetClipVoice {
                        clip: clip_key,
                        speaker_id: sid,
                        singer_name: sn.clone(),
                        style_name: stn.clone(),
                    });
                }));
            }

            // 再取得ボタン (新規キャラ導入時に押す)。
            let refetch_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 22.0,
            };
            if ui.button_at_clicked(
                "inspector_clip_voice_refetch",
                "声一覧を再取得",
                refetch_rect,
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::RefetchSingers);
                }));
            }
            y += 28.0;
        }
    }

    // ---- 親グループ (Parent) の編集 UI はインスペクタから撤去した (FIXME #39) ----
    // 親子 (グループ階層) の編集はアレンジビューでのトラックドラッグ
    // (`ArrangementEditRequest::SetTrackParent`) 一本に統一する。階層は
    // アレンジの入れ子インデントで可視化されるので、同じ概念をインスペクタの
    // ドロップダウンでも編集できると Single Source of Truth が崩れる。
    // `AppEvent::SetTrackParent` / `action_set_track_parent` 自体はアレンジ
    // ドラッグが使うため残す。

    // ---- 口パク (lip-sync) 出力先 binding ----------------------------
    // Vocal track のみ。生成した口画像 ImageEvent を焼き込む先の口 track
    // (立ち絵 group の子 image track) を選ぶ。設定で再生成が走る。
    if let Some(track) = cursor_idx.and_then(|i| app.song.tracks.get(i))
        && track.is_voicevox_vocal()
    {
        let self_id = track.id;
        // 候補: 自分以外の全 track (= 口 track はどれでも選べる)。
        // candidate_ids[k] と labels[k+1] が対応 (labels[0] = "(なし)" sentinel)。
        // 表示名はここで 1 度だけ format/clone する (別 Vec への再 clone を避ける)。
        let mut candidate_ids: Vec<u32> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        labels.push("(なし)".into());
        for t in app.song.tracks.iter().filter(|t| t.id != self_id) {
            candidate_ids.push(t.id);
            labels.push(if t.name.is_empty() {
                format!("Track {}", t.id)
            } else {
                t.name.clone()
            });
        }
        ui.label_at(
            "inspector_lipsync_target_label",
            "口パク出力先",
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
        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let selected_idx = match track.lipsync_target_track {
            None => 0,
            Some(tid) => candidate_ids
                .iter()
                .position(|id| *id == tid)
                .map(|i| i + 1)
                .unwrap_or(0),
        };
        if let Some(picked) = ui.dropdown(
            "inspector_lipsync_target_dropdown",
            dropdown_rect,
            &label_refs,
            selected_idx,
        ) {
            let target = if picked == 0 {
                None
            } else {
                candidate_ids.get(picked - 1).copied()
            };
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetLipsyncTarget {
                    track: self_id,
                    target,
                });
            }));
        }
        y += 30.0;
    }

    // ---- 口パク mapping (口形状 → 画像) -------------------------------
    // この track を口パク出力先に指定している vocal track があるとき、7 形状
    // (a/i/u/e/o/N/閉口) の画像割当を表示する。各 slot は import 済み image を選ぶ。
    if let Some(track) = cursor_idx.and_then(|i| app.song.tracks.get(i)) {
        let this_id = track.id;
        let is_target = app
            .song
            .tracks
            .iter()
            .any(|t| t.lipsync_target_track == Some(this_id));
        if is_target {
            ui.label_at(
                "inspector_mouthmap_label",
                "口パク (口形状 → 画像)",
                area.x + pad,
                y,
                12.0,
                TEXT_DIM,
            );
            y += 18.0;
            // import 済み image source の (id, ファイル名) 一覧 (id 昇順)。
            // image_ids[k] と labels[k+1] が対応 (labels[0] = "(なし)" sentinel)。
            // ラベル文字列を別 Vec へ再 clone せず、 ソート後そのまま labels へ move する。
            let mut images: Vec<(common::model::ImageSourceId, String)> = app
                .song
                .image_sources
                .iter()
                .map(|(id, src)| (*id, image_source_label(src)))
                .collect();
            images.sort_by_key(|(id, _)| *id);
            let map = track.mouth_map.as_ref();
            const SHAPES: [(common::model::MouthShape, &str); 7] = [
                (common::model::MouthShape::A, "あ"),
                (common::model::MouthShape::I, "い"),
                (common::model::MouthShape::U, "う"),
                (common::model::MouthShape::E, "え"),
                (common::model::MouthShape::O, "お"),
                (common::model::MouthShape::N, "ん"),
                (common::model::MouthShape::Closed, "閉"),
            ];
            let mut image_ids: Vec<common::model::ImageSourceId> =
                Vec::with_capacity(images.len());
            let mut labels: Vec<String> = Vec::with_capacity(images.len() + 1);
            labels.push("(なし)".into());
            for (id, name) in images {
                image_ids.push(id);
                labels.push(name);
            }
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            for (slot_i, (shape, shape_label)) in SHAPES.iter().enumerate() {
                ui.label_at(
                    format!("inspector_mouthmap_slot_label_{slot_i}"),
                    shape_label,
                    area.x + pad,
                    y + 5.0,
                    11.0,
                    TEXT_DIM,
                );
                let dropdown_rect = Rect {
                    x: area.x + pad + 40.0,
                    y,
                    w: area.w - pad * 2.0 - 40.0,
                    h: 22.0,
                };
                let cur = map.map_or(0, |m| m.get(*shape));
                let selected_idx = if cur == 0 {
                    0
                } else {
                    image_ids
                        .iter()
                        .position(|id| *id == cur)
                        .map(|i| i + 1)
                        .unwrap_or(0)
                };
                if let Some(picked) = ui.dropdown(
                    format!("inspector_mouthmap_dropdown_{slot_i}"),
                    dropdown_rect,
                    &label_refs,
                    selected_idx,
                ) {
                    let source_id = if picked == 0 {
                        0
                    } else {
                        image_ids.get(picked - 1).copied().unwrap_or(0)
                    };
                    let shape = *shape;
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetMouthMapSlot {
                            track: this_id,
                            shape,
                            source_id,
                        });
                    }));
                }
                y += 26.0;
            }
            y += 4.0;
        }
    }

    measured_body_h.set(y - (body_top - scroll_off.1));
    });
    // FIXME #23: 測定した param 実高さを次フレーム用に保存 (変化時のみ edit を積む)。
    let measured = measured_body_h.get();
    if (app.inspector_body_h - measured).abs() > 0.5 {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.inspector_body_h = measured;
        }));
    }
    // chain band は area 下端 pinned。 param viewport 直下 (boundary_y) から描く。
    let mut y = boundary_y;

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
                ("inspector_row_name", idx),
                &entry.plugin_name,
                row_rect.x + 8.0,
                row_rect.y + 8.0,
                11.0,
                TEXT,
            );
            let device_index = entry.device_index;
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
                        app.handle_event(AppEvent::ToggleSlotGui { index: device_index })
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
                        app.handle_event(AppEvent::RemoveDevice { index: device_index })
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
                let device_index = entry.device_index;
                let new_source = choice.track_id;
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetSidechainSource {
                        track_id,
                        device_index,
                        port: 0,
                        source: new_source,
                    });
                }));
            }
            row_y += row_h + row_gap;
        }
    }

    // 下端: 「+ Plugin」 1 ボタン (統合ピッカー)。 plan_unified_plugin_picker.md:
    // 旧 +Inst / +FX / +MIDI の 3 ボタンを 1 つに統合し、 選んだプラグインの種別で
    // 行き先 (Instrument / FX / MIDI FX) を自動振り分けする。 master bus は audio fx
    // のみなのでリスト側 (refresh_picker_visible) が FX のみに絞る。 ラベルだけ master
    // は「+ FX」で期待値を示す。
    let is_master = app.cursor_track_id() == Some(common::model::MASTER_TRACK_ID);
    ui.button_at(
        "inspector_add_plugin",
        if is_master { "+ FX" } else { "+ Plugin" },
        Rect { x: area.x + pad, y: btns_y, w: area.w - pad * 2.0, h: btns_h },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::OpenPluginPicker)),
    );
}
