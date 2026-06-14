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
    text_num_to_builtin, AppData, AppEvent, ClipRef, ColorPickerTarget, DiscreteClipEdit,
    FadeEdgeKind, InspectorScrubField, TextNumField,
};
use crate::view::modulation::{self as mod_widget, build_mod, scrub_field_mod, ModBuild};
use crate::view::track_color;
use common::model::{AutomationTarget, FadeCurve, ImageBuiltinParam, StretchMode, TextAlign};

const BG: Color = Color { r: 0.16, g: 0.16, b: 0.20, a: 1.0 };
const TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
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

    // --- per-control modulation (docs/plan_modulation_routing_redesign.md §6) ---
    // scrub_key から target + 表示↔model 変換 (回転 deg↔rad 等) を引き、Bitwig 風の
    // modulation overlay + arm 中の depth-drag を組む。`build_mod` が image / text /
    // (回転含む) を 1 経路で扱う。clip-level field (gain / pan / pitch / fades) は
    // `scrub_field_mod` が `None` を返すので従来どおり overlay なし。
    // inspector の image/text field は cursor track の clip に属する。
    let cursor_track = app.cursor_track_id().unwrap_or(common::model::MASTER_TRACK_ID);
    let mod_spec = scrub_field_mod(scrub_key);
    let mod_build = mod_spec
        .as_ref()
        .map(|(target, domain)| build_mod(app, target.clone(), base, *domain, cursor_track));
    let modulation = mod_build.as_ref().map(ModBuild::modulation);

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
        modulation,
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
    // modulation depth ドラッグの falling edge で host 再同期 (自コントロールの
    // target を key に、他コントロールと干渉せず drag-end で 1 回だけ recompile)。
    if let Some((target, _)) = &mod_spec {
        mod_widget::push_mod_drag_resync(ui, app, cursor_track, target, resp.mod_dragging);
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

// FIXME #56: generator の rate (tempo 同期 division か Free Hz) 選択肢。
const MOD_RATE_DIVS: [(&str, u32, u32); 9] = [
    ("1/16", 1, 16),
    ("1/8T", 1, 12),
    ("1/8", 1, 8),
    ("1/4T", 1, 6),
    ("1/4", 1, 4),
    ("1/2", 1, 2),
    ("1bar", 1, 1),
    ("2bar", 2, 1),
    ("4bar", 4, 1),
];

/// 種別ごとの inspector 行数 (各行 22px)。
fn mod_src_rows(kind: &common::model::ModSourceKind) -> usize {
    use common::model::ModSourceKind as K;
    match kind {
        K::EnvelopeFollower { .. } | K::Lfo(_) | K::Random(_) => 2,
        K::Mseg(_) | K::Steps(_) => 3,
    }
}

/// rate ドロップダウン (tempo 同期 division + Free)。pick で `EditModSource::Rate` を emit。
fn mod_rate_control(
    ui: &mut Ui<'_, AppData>,
    id_seed: u32,
    rect: Rect,
    rate: &common::model::ModRate,
    sid: u32,
) {
    use common::model::ModRate;
    let mut labels: Vec<&str> = MOD_RATE_DIVS.iter().map(|(l, _, _)| *l).collect();
    labels.push("Free");
    let sel = match rate {
        ModRate::Sync {
            numerator,
            denominator,
        } => MOD_RATE_DIVS
            .iter()
            .position(|(_, n, d)| n == numerator && d == denominator)
            .unwrap_or(4),
        ModRate::Free { .. } => MOD_RATE_DIVS.len(),
    };
    if let Some(picked) = ui.dropdown(("inspector_mod_rate", id_seed), rect, &labels, sel) {
        let new_rate = if picked < MOD_RATE_DIVS.len() {
            let (_, n, d) = MOD_RATE_DIVS[picked];
            ModRate::Sync {
                numerator: n,
                denominator: d,
            }
        } else {
            ModRate::Free { hz: 1.0 }
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::EditModSource {
                id: sid,
                edit: crate::app::ModSourceEdit::Rate(new_rate),
            });
        }));
    }
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
            TEXT,
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
            &TOGGLE_AUDIO_BASE,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    // FIXME #46: 選択全クリップへ一括 (variant-safe broadcast)。
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            let new_curve = fade_curve_from_index(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::FadeCurve(FadeEdgeKind::Out, new_curve),
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
            TEXT,
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
            &TOGGLE_AUDIO_BASE,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
            let new_curve = fade_curve_from_index(picked);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::BroadcastDiscreteClipEdit {
                    targets: app.inspector_target_refs(),
                    edit: DiscreteClipEdit::FadeCurve(FadeEdgeKind::Out, new_curve),
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
            TEXT,
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
                    // 度域 range。modulation の色帯/live tick は range が要る (gui_01 overlay
                    // は range なしだと枠強調のみ)。handler は従来どおり -π..π wrap。
                    Some((-180.0, 180.0)),
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
            ui.label_at((param, "group_label"), label, area.x + pad, y + 5.0, 11.0, TEXT);
            let style =
                ScrubableNumberStyle { sensitivity: sens, range, ..SCRUB_STYLE_GROUP };
            // per-control modulation (docs/plan_modulation_routing_redesign.md §6):
            // 立ち絵を音でドラッグ変調する Bitwig 流。全 8 param を対象 (Rotation は
            // deg↔rad、ScaleX/Y は log space を `build_mod` が到達値ベースで吸収)。
            let g_target = AutomationTarget::GroupTransform(param);
            let g_domain = if matches!(param, G::Rotation) {
                mod_widget::PLAIN_ROTATION
            } else {
                mod_widget::PLAIN_IDENT
            };
            let g_mod_build = build_mod(app, g_target.clone(), value, g_domain, track_id);
            let g_modulation = Some(g_mod_build.modulation());
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
                g_modulation,
            );
            // modulation depth ドラッグの falling edge で host 再同期 (audio target の
            // depth 反映用。visual group transform は compose が即読みするので視覚は即時)。
            mod_widget::push_mod_drag_resync(ui, app, track_id, &g_target, resp.mod_dragging);
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

    // FIXME #54 Wave4: 内蔵映像 FX のパラメータ調整パネル（チェーン行の "GUI" ボタンで開閉）。
    // 各 param を scrubable_number で実レンジ表示 + per-control 変調（Ranged domain で kick→効果）。
    // 値の SSoT は PluginParam lane の default_value（`SetVideoFxParam` が格納）。
    if let Some(view) = app.inspector_video_fx_params() {
        ui.label_at("inspector_vfx_label", view.def.name, area.x + pad, y, 12.0, TEXT);
        y += 18.0;
        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 88.0;
        let input_x = area.x + pad + label_w;
        let input_w = (row_w - label_w).max(40.0);
        let track_id = view.track_id;
        let device_index = view.device_index;
        for (i, param) in view.def.params.iter().enumerate() {
            let value = f64::from(view.values[i]);
            let (min, max) = param.kind.range();
            let (min, max) = (f64::from(min), f64::from(max));
            let default_real = f64::from(param.kind.norm_to_real(param.kind.default_norm()));
            #[allow(clippy::cast_possible_truncation)]
            let sens = (((max - min) / 220.0).max(0.0001)) as f32;
            ui.label_at((i, "vfx_label"), param.name, area.x + pad, y + 5.0, 11.0, TEXT);
            let style =
                ScrubableNumberStyle { sensitivity: sens, range: Some((min, max)), ..SCRUB_STYLE_GROUP };
            let target = AutomationTarget::PluginParam {
                device_index,
                param_id: param.id,
                legacy_slot: None,
            };
            let domain = crate::app::ModControlDomain::Ranged { min, max, log: param.kind.is_log() };
            let mod_build = build_mod(app, target.clone(), value, domain, track_id);
            let modulation = Some(mod_build.modulation());
            let param_id = param.id;
            let resp = ui.scrubable_number_at(
                (i, "vfx_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                value,
                default_real,
                ScrubableNumberFormat::Decimal(3),
                &style,
                "Video FX",
                move |v| {
                    #[allow(clippy::cast_possible_truncation)]
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetVideoFxParam {
                            device_index,
                            param_id,
                            value_real: v as f32,
                        });
                    })
                },
                None,
                modulation,
            );
            // 変調 depth ドラッグの falling edge で host 再同期（音声 target の depth 反映用）。
            mod_widget::push_mod_drag_resync(ui, app, track_id, &target, resp.mod_dragging);
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
            TEXT,
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
            &TOGGLE_AUDIO_BASE,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
                TEXT,
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
            TEXT,
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
            TEXT,
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
            TEXT,
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
                TEXT,
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
            TEXT,
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
                TEXT,
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
                    TEXT,
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
        TEXT,
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

    // docs/plan_modulation.md §9: modulation rack — sources (create / list /
    // live meter / remove) + per-lane follower routings (depth / polarity).
    // Always shown so a source can be created; vertical budget caps the
    // source / routing lists at 3 visible rows each (overflow off-screen,
    // matching the sidechain section's no-scroll convention).
    let mod_sources = app.mod_source_display();
    let mod_routings = app.cursor_mod_routings();
    let mod_routing_count: usize = mod_routings.iter().map(|(_, _, _, rs)| rs.len()).sum();
    // Each source is 2 rows (track/meter/tap/remove, then attack/release).
    // Cap visible sources at 2 and routings at 2 so the section stays bounded
    // in the no-scroll inspector.
    let mod_vis_route = mod_routing_count.min(2);
    // FIXME #56: 種別ごとに行数が違う (follower/LFO/Random=2、 MSEG/Steps=3)。
    // 可視 source を 2 個に cap し、 高さは種別別に合算 (no-scroll で bounded)。
    let mod_src_h: f32 = mod_sources
        .iter()
        .take(2)
        .map(|r| mod_src_rows(&r.kind) as f32 * 22.0)
        .sum();
    // header+add(22) + src rows(種別別) + [routing rows(22) + add row(24) +
    // gap] when a source exists + 6 pad.
    let mod_section_h = 22.0
        + mod_src_h
        + if mod_sources.is_empty() {
            0.0
        } else {
            4.0 + mod_vis_route as f32 * 22.0 + 24.0
        }
        + 6.0;

    let list_x = area.x + pad;
    let list_y = y;
    let list_w = area.w - pad * 2.0;
    let list_h = (btns_y - 6.0 - list_y - sc_section_h - mod_section_h).max(0.0);
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
    // first aux input port (aux_inputs[0]). Self-track is filtered
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
            TEXT,
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

    // ---- Modulation rack (docs/plan_modulation.md §9) ----------------
    // Sources: create from cursor track / live follower meter / remove.
    // Routings: per cursor-track automation lane, follower modulation with
    // depth + polarity. Sits above the Sidechain section.
    {
        let mut my = btns_y - sc_section_h - mod_section_h;
        ui.label_at(
            "inspector_mod_label",
            "Modulation",
            area.x + pad,
            my,
            12.0,
            TEXT,
        );
        // FIXME #56: [+ ▾] add-menu — 種別 (Follow/LFO/Random/MSEG/Steps) を選んで作成。
        {
            let add_w = 64.0;
            let add_rect = Rect {
                x: area.x + area.w - pad - add_w,
                y: my - 2.0,
                w: add_w,
                h: 20.0,
            };
            let add_labels = ["+ \u{25be}", "Follow", "LFO", "Random", "MSEG", "Steps"];
            if let Some(picked) = ui.dropdown("inspector_mod_add_src", add_rect, &add_labels, 0)
                && picked > 0
            {
                let tag = match picked {
                    1 => crate::app::ModSourceKindTag::Follower,
                    2 => crate::app::ModSourceKindTag::Lfo,
                    3 => crate::app::ModSourceKindTag::Random,
                    4 => crate::app::ModSourceKindTag::Mseg,
                    _ => crate::app::ModSourceKindTag::Steps,
                };
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::AddModSource { kind: tag });
                }));
            }
        }
        my += 22.0;

        // Source rows (FIXME #56): row1 共通 = [name/track] [meter] [arm] [×]、
        // row2+ は種別別エディタ (follower=tap/A/R, LFO=shape/rate/phase,
        // Random=mode/rate/reroll, Steps=count/dir/slew + step bars,
        // MSEG=play/rate/±pt + point values)。
        let mod_track_choices = app.mod_source_track_choices();
        let mod_track_labels: Vec<&str> =
            mod_track_choices.iter().map(|(_, l)| l.as_str()).collect();
        let mut any_mod_drag = false;
        for (i, src) in mod_sources.iter().take(2).enumerate() {
            use common::model::ModSourceKind as K;
            use crate::app::ModSourceEdit as E;
            let sid = src.id;
            let i_u = i as u32;
            let lx = area.x + pad;
            let row_w = area.w - pad * 2.0;
            // 0..=1 用の共通 scrub スタイル。
            let unit_style = ScrubableNumberStyle {
                range: Some((0.0, 1.0)),
                sensitivity: 0.006,
                ..SCRUB_STYLE_INSPECTOR
            };

            // --- row 1 (共通): [name/track dropdown] [meter] [arm] [×] ---
            let rm_rect = Rect {
                x: area.x + area.w - pad - 20.0,
                y: my,
                w: 20.0,
                h: 20.0,
            };
            // FIXME #56: follower の 3 段タップセレクタは row2 (kind 別エディタ) へ移動。
            let meter_w = 50.0;
            let meter_x = rm_rect.x - 4.0 - meter_w;
            let filled = ((src.scalar.clamp(0.0, 1.0) * 6.0).round() as usize).min(6);
            let meter: String = "\u{25ae}".repeat(filled) + &"\u{25af}".repeat(6 - filled);
            ui.label_at(
                ("inspector_mod_src_meter", i),
                &meter,
                meter_x,
                my + 4.0,
                11.0,
                TEXT,
            );
            // arm toggle (Bitwig 流): armed 中は各 param control をドラッグで depth 編集。
            let arm_w = 24.0;
            let arm_x = meter_x - 4.0 - arm_w;
            let armed = app.armed_mod_source == Some(sid);
            ui.button_at(
                ("inspector_mod_src_arm", i),
                if armed { "\u{25c9}" } else { "\u{25cb}" },
                Rect { x: arm_x, y: my, w: arm_w, h: 20.0 },
                move || {
                    Edit::mutate(move |app: &mut AppData| {
                        let next =
                            if app.armed_mod_source == Some(sid) { None } else { Some(sid) };
                        app.handle_event(AppEvent::SetArmedModSource(next));
                    })
                },
            );
            let name_rect = Rect {
                x: lx,
                y: my,
                w: (arm_x - 4.0 - lx).max(40.0),
                h: 20.0,
            };
            if let K::EnvelopeFollower { tap, .. } = &src.kind {
                let sel = mod_track_choices
                    .iter()
                    .position(|(tid, _)| *tid == tap.source_track)
                    .unwrap_or(0);
                if let Some(picked) = ui.dropdown(
                    ("inspector_mod_src_track", i),
                    name_rect,
                    &mod_track_labels,
                    sel,
                ) && let Some(&(tid, _)) = mod_track_choices.get(picked)
                {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetModSourceTrack {
                            id: sid,
                            source_track: tid,
                        });
                    }));
                }
            } else {
                ui.label_at(
                    ("inspector_mod_src_kind", i),
                    src.kind.short_label(),
                    name_rect.x,
                    my + 4.0,
                    12.0,
                    TEXT,
                );
            }
            ui.button_at(("inspector_mod_src_rm", i), "\u{00d7}", rm_rect, move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::RemoveModSource { id: sid });
                })
            });
            my += 22.0;

            // --- 種別別エディタ行 ---
            match &src.kind {
                K::EnvelopeFollower { tap, follower } => {
                    // row2: [tap 3 段セレクタ][A][R]。drag-end edge で recompile。
                    // docs/plan_modulation_followups.md §1: Pre-FX (素の音) / Post-FX / Post-Fader。
                    let tap_w = 64.0;
                    const TAP_POINTS: [common::model::TapPoint; 3] = [
                        common::model::TapPoint::PreFx,
                        common::model::TapPoint::PostFx,
                        common::model::TapPoint::PostFader,
                    ];
                    let tap_labels = ["Pre-FX", "Post-FX", "Post-Fdr"];
                    let tap_sel =
                        TAP_POINTS.iter().position(|t| *t == tap.tap_point).unwrap_or(2);
                    if let Some(picked) = ui.dropdown(
                        ("inspector_mod_src_tap", i),
                        Rect { x: lx, y: my, w: tap_w, h: 20.0 },
                        &tap_labels,
                        tap_sel,
                    ) && let Some(&tp) = TAP_POINTS.get(picked)
                    {
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::SetModSourceTapPoint {
                                id: sid,
                                tap_point: tp,
                            });
                        }));
                    }
                    let rest_x = lx + tap_w + 6.0;
                    let half = (area.x + area.w - pad - rest_x - 6.0) / 2.0;
                    let ar_style = ScrubableNumberStyle {
                        range: Some((0.0, 60_000.0)),
                        sensitivity: 0.04,
                        ..SCRUB_STYLE_INSPECTOR
                    };
                    ui.label_at(("inspector_mod_a_lbl", i), "A", rest_x, my + 4.0, 10.0, TEXT);
                    let a_resp = ui.scrubable_number_at(
                        ("inspector_mod_attack", i),
                        Rect { x: rest_x + 12.0, y: my, w: (half - 12.0).max(20.0), h: 20.0 },
                        f64::from(follower.attack_ms),
                        1.0,
                        ScrubableNumberFormat::Decimal(1),
                        &ar_style,
                        "Inspector",
                        move |v| {
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetModSourceAttack { id: sid, ms: v as f32 });
                            })
                        },
                        None,
                        None,
                    );
                    let r_x = rest_x + half + 6.0;
                    ui.label_at(("inspector_mod_r_lbl", i), "R", r_x, my + 4.0, 10.0, TEXT);
                    let r_resp = ui.scrubable_number_at(
                        ("inspector_mod_release", i),
                        Rect { x: r_x + 12.0, y: my, w: (half - 12.0).max(20.0), h: 20.0 },
                        f64::from(follower.release_ms),
                        100.0,
                        ScrubableNumberFormat::Decimal(1),
                        &ar_style,
                        "Inspector",
                        move |v| {
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetModSourceRelease { id: sid, ms: v as f32 });
                            })
                        },
                        None,
                        None,
                    );
                    any_mod_drag |= a_resp.dragging
                        || a_resp.editing_text
                        || r_resp.dragging
                        || r_resp.editing_text;
                    my += 22.0;
                }
                K::Lfo(c) => {
                    // row2: [shape dd][rate dd][phase scrub]
                    let shape_w = 56.0;
                    let shapes = ["Sin", "Tri", "SawU", "SawD", "Sqr", "Pulse"];
                    let ssel = match c.shape {
                        common::model::LfoShape::Sine => 0,
                        common::model::LfoShape::Triangle => 1,
                        common::model::LfoShape::SawUp => 2,
                        common::model::LfoShape::SawDown => 3,
                        common::model::LfoShape::Square => 4,
                        common::model::LfoShape::Pulse { .. } => 5,
                    };
                    if let Some(p) = ui.dropdown(
                        ("inspector_lfo_shape", i),
                        Rect { x: lx, y: my, w: shape_w, h: 20.0 },
                        &shapes,
                        ssel,
                    ) {
                        let shape = match p {
                            0 => common::model::LfoShape::Sine,
                            1 => common::model::LfoShape::Triangle,
                            2 => common::model::LfoShape::SawUp,
                            3 => common::model::LfoShape::SawDown,
                            4 => common::model::LfoShape::Square,
                            _ => common::model::LfoShape::Pulse { width: 0.5 },
                        };
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::LfoShape(shape) });
                        }));
                    }
                    let rate_w = 56.0;
                    mod_rate_control(ui, i_u, Rect { x: lx + shape_w + 6.0, y: my, w: rate_w, h: 20.0 }, &c.rate, sid);
                    let ph_x = lx + shape_w + rate_w + 12.0;
                    ui.label_at(("inspector_lfo_ph_lbl", i), "\u{03c6}", ph_x, my + 4.0, 10.0, TEXT);
                    let ph_resp = ui.scrubable_number_at(
                        ("inspector_lfo_phase", i),
                        Rect { x: ph_x + 12.0, y: my, w: (area.x + area.w - pad - ph_x - 12.0).max(20.0), h: 20.0 },
                        f64::from(c.phase),
                        0.0,
                        ScrubableNumberFormat::Decimal(2),
                        &unit_style,
                        "Inspector",
                        move |v| {
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::LfoPhase(v as f32) });
                            })
                        },
                        None,
                        None,
                    );
                    any_mod_drag |= ph_resp.dragging || ph_resp.editing_text;
                    my += 22.0;
                }
                K::Random(c) => {
                    // row2: [mode toggle][rate dd][reroll]
                    let mode_w = 64.0;
                    let is_smooth = matches!(c.mode, common::model::RandomMode::Smooth);
                    ui.button_at(
                        ("inspector_rand_mode", i),
                        if is_smooth { "Smooth" } else { "S&H" },
                        Rect { x: lx, y: my, w: mode_w, h: 20.0 },
                        move || {
                            Edit::mutate(move |app: &mut AppData| {
                                let mode = if is_smooth {
                                    common::model::RandomMode::SampleHold
                                } else {
                                    common::model::RandomMode::Smooth
                                };
                                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::RandomMode(mode) });
                            })
                        },
                    );
                    let rate_w = 56.0;
                    mod_rate_control(ui, i_u, Rect { x: lx + mode_w + 6.0, y: my, w: rate_w, h: 20.0 }, &c.rate, sid);
                    let rr_x = lx + mode_w + rate_w + 12.0;
                    ui.button_at(
                        ("inspector_rand_reroll", i),
                        "\u{21bb} seed",
                        Rect { x: rr_x, y: my, w: (area.x + area.w - pad - rr_x).max(40.0), h: 20.0 },
                        move || {
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::RerollSeed });
                            })
                        },
                    );
                    my += 22.0;
                }
                K::Steps(c) => {
                    // row2: [-][+][dir][rate][slew]; row3: per-step bars。
                    let n = c.values.len();
                    ui.button_at(("inspector_steps_dec", i), "\u{2212}", Rect { x: lx, y: my, w: 22.0, h: 20.0 }, move || {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsCount(n.saturating_sub(1)) });
                        })
                    });
                    ui.button_at(("inspector_steps_inc", i), "+", Rect { x: lx + 26.0, y: my, w: 22.0, h: 20.0 }, move || {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsCount(n + 1) });
                        })
                    });
                    let dirs = ["Fwd", "Bwd", "Ping"];
                    let dsel = match c.direction {
                        common::model::StepsDirection::Forward => 0,
                        common::model::StepsDirection::Backward => 1,
                        common::model::StepsDirection::PingPong => 2,
                    };
                    if let Some(p) = ui.dropdown(("inspector_steps_dir", i), Rect { x: lx + 52.0, y: my, w: 50.0, h: 20.0 }, &dirs, dsel) {
                        let dir = match p {
                            0 => common::model::StepsDirection::Forward,
                            1 => common::model::StepsDirection::Backward,
                            _ => common::model::StepsDirection::PingPong,
                        };
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsDirection(dir) });
                        }));
                    }
                    mod_rate_control(ui, i_u, Rect { x: lx + 106.0, y: my, w: 50.0, h: 20.0 }, &c.rate, sid);
                    let sl_x = lx + 160.0;
                    ui.label_at(("inspector_steps_sl_lbl", i), "sl", sl_x, my + 4.0, 9.0, TEXT);
                    let sl_resp = ui.scrubable_number_at(
                        ("inspector_steps_slew", i),
                        Rect { x: sl_x + 16.0, y: my, w: (area.x + area.w - pad - sl_x - 16.0).max(20.0), h: 20.0 },
                        f64::from(c.slew),
                        0.0,
                        ScrubableNumberFormat::Decimal(2),
                        &unit_style,
                        "Inspector",
                        move |v| {
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepsSlew(v as f32) });
                            })
                        },
                        None,
                        None,
                    );
                    any_mod_drag |= sl_resp.dragging || sl_resp.editing_text;
                    my += 22.0;
                    // row3: per-step bars (上下ドラッグで 0..1)、 横幅に収まる数だけ。
                    let shown = n.min(16);
                    if shown > 0 {
                        let cell = (row_w / shown as f32).max(8.0);
                        for j in 0..shown {
                            let resp = ui.scrubable_number_at(
                                ("inspector_step_val", i * 100 + j),
                                Rect { x: lx + j as f32 * cell, y: my, w: (cell - 2.0).max(6.0), h: 20.0 },
                                f64::from(c.values[j]),
                                0.0,
                                ScrubableNumberFormat::Decimal(2),
                                &unit_style,
                                "Inspector",
                                move |v| {
                                    Edit::mutate(move |app: &mut AppData| {
                                        app.handle_event(AppEvent::EditModSource { id: sid, edit: E::StepValue { index: j, value: v as f32 } });
                                    })
                                },
                                None,
                                None,
                            );
                            any_mod_drag |= resp.dragging || resp.editing_text;
                        }
                    }
                    my += 22.0;
                }
                K::Mseg(c) => {
                    // row2: [play_mode][rate][+pt][-pt]; row3: per-point value scrubs。
                    let pmodes = ["1shot", "Loop", "Ping"];
                    let psel = match c.play_mode {
                        common::model::MsegPlayMode::OneShot => 0,
                        common::model::MsegPlayMode::Loop => 1,
                        common::model::MsegPlayMode::PingPong => 2,
                    };
                    if let Some(p) = ui.dropdown(("inspector_mseg_play", i), Rect { x: lx, y: my, w: 56.0, h: 20.0 }, &pmodes, psel) {
                        let pm = match p {
                            0 => common::model::MsegPlayMode::OneShot,
                            1 => common::model::MsegPlayMode::Loop,
                            _ => common::model::MsegPlayMode::PingPong,
                        };
                        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::MsegPlayMode(pm) });
                        }));
                    }
                    mod_rate_control(ui, i_u, Rect { x: lx + 62.0, y: my, w: 56.0, h: 20.0 }, &c.rate, sid);
                    let np = c.points.len();
                    ui.button_at(("inspector_mseg_addpt", i), "+pt", Rect { x: lx + 124.0, y: my, w: 34.0, h: 20.0 }, move || {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::MsegAddPoint { time: 0.5, value: 0.5 } });
                        })
                    });
                    ui.button_at(("inspector_mseg_rmpt", i), "\u{2212}pt", Rect { x: lx + 162.0, y: my, w: 34.0, h: 20.0 }, move || {
                        Edit::mutate(move |app: &mut AppData| {
                            if np > 2 {
                                app.handle_event(AppEvent::EditModSource { id: sid, edit: E::MsegRemovePoint(np - 2) });
                            }
                        })
                    });
                    my += 22.0;
                    // row3: per-point の高さ (value) を編集。 time は既存値を保持。
                    let shown = np.min(8);
                    if shown > 0 {
                        let cell = (row_w / shown as f32).max(8.0);
                        for j in 0..shown {
                            let t = c.points[j].time;
                            let resp = ui.scrubable_number_at(
                                ("inspector_mseg_val", i * 100 + j),
                                Rect { x: lx + j as f32 * cell, y: my, w: (cell - 2.0).max(6.0), h: 20.0 },
                                f64::from(c.points[j].value),
                                0.0,
                                ScrubableNumberFormat::Decimal(2),
                                &unit_style,
                                "Inspector",
                                move |v| {
                                    Edit::mutate(move |app: &mut AppData| {
                                        app.handle_event(AppEvent::EditModSource { id: sid, edit: E::MsegMovePoint { index: j, time: t, value: v as f32 } });
                                    })
                                },
                                None,
                                None,
                            );
                            any_mod_drag |= resp.dragging || resp.editing_text;
                        }
                    }
                    my += 22.0;
                }
            }
        }
        // Sync once on the scrub drag-end edge (follower coeffs の recompile +
        // generator config の schedule `mod_kinds` 反映)。
        if any_mod_drag != app.mod_follower_scrub_active {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SetModFollowerScrubbing(any_mod_drag));
            }));
        }

        // Routings only make sense once a source exists.
        if !mod_sources.is_empty() {
            my += 4.0;
            // FIXME #56: follower は follow 先トラック名、 generator は種別ラベル。
            let src_name = |sid: u32| -> String {
                mod_sources
                    .iter()
                    .find(|r| r.id == sid)
                    .map(|r| match &r.kind {
                        common::model::ModSourceKind::EnvelopeFollower { tap, .. } => app
                            .song
                            .track_by_id(tap.source_track)
                            .map(|t| t.name.clone())
                            .unwrap_or_else(|| format!("src {sid}")),
                        other => other.short_label().to_string(),
                    })
                    .unwrap_or_else(|| format!("src {sid}"))
            };
            // docs/plan_modulation_routing_redesign.md §6: existing lane 非依存
            // routings grouped by target. depth は polarity/× の隣で編集 (scrubable、
            // ドラッグ中は handler が dirty のみ)。capped at 3 rows.
            let mut route_i = 0usize;
            'routes: for (tid, target, label, routings) in &mod_routings {
                for (sid, depth, bipolar) in routings {
                    if route_i >= 3 {
                        break 'routes;
                    }
                    let row_y = my;
                    let lbl = format!("{label} \u{2190} {}", src_name(*sid));
                    ui.label_at(
                        ("inspector_mod_rt_lbl", route_i),
                        &lbl,
                        area.x + pad,
                        row_y + 4.0,
                        11.0,
                        TEXT,
                    );
                    let depth_w = 46.0;
                    let pol_w = 22.0;
                    let rm_w = 20.0;
                    let rm_x = area.x + area.w - pad - rm_w;
                    let pol_x = rm_x - 4.0 - pol_w;
                    let depth_x = pol_x - 4.0 - depth_w;
                    let (t, s) = (*tid, *sid);
                    let tgt = target.clone();
                    ui.scrubable_number_at(
                        ("inspector_mod_rt_depth", route_i),
                        Rect {
                            x: depth_x,
                            y: row_y,
                            w: depth_w,
                            h: 20.0,
                        },
                        f64::from(*depth),
                        1.0,
                        ScrubableNumberFormat::Decimal(2),
                        &SCRUB_STYLE_INSPECTOR,
                        "Inspector",
                        move |v| {
                            let tgt = tgt.clone();
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetModRoutingDepth {
                                    track_id: t,
                                    target: tgt,
                                    source_id: s,
                                    depth: v as f32,
                                });
                            })
                        },
                        None,
                        None, // ラックの depth は数値直編集 (control 側 ring と別経路)
                    );
                    // polarity toggle (± = Bipolar, + = Unipolar)。
                    let bip = *bipolar;
                    let tgt_pol = target.clone();
                    ui.button_at(
                        ("inspector_mod_rt_pol", route_i),
                        if bip { "\u{00b1}" } else { "+" },
                        Rect {
                            x: pol_x,
                            y: row_y,
                            w: pol_w,
                            h: 20.0,
                        },
                        move || {
                            let tgt_pol = tgt_pol.clone();
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::SetModRoutingPolarity {
                                    track_id: t,
                                    target: tgt_pol,
                                    source_id: s,
                                    bipolar: !bip,
                                });
                            })
                        },
                    );
                    let tgt_rm = target.clone();
                    ui.button_at(
                        ("inspector_mod_rt_rm", route_i),
                        "\u{00d7}",
                        Rect {
                            x: rm_x,
                            y: row_y,
                            w: rm_w,
                            h: 20.0,
                        },
                        move || {
                            let tgt_rm = tgt_rm.clone();
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::RemoveModRouting {
                                    track_id: t,
                                    target: tgt_rm,
                                    source_id: s,
                                });
                            })
                        },
                    );
                    my += 22.0;
                    route_i += 1;
                }
            }
            // Add-routing dropdown: "<target> <- <source>" over the cursor track's
            // modulatable targets × sources, excluding already-routed pairs.
            let track_id = if app.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
                common::model::MASTER_TRACK_ID
            } else {
                app.cursor_track_id().unwrap_or(0)
            };
            let mut add_labels: Vec<String> = vec!["+ route\u{2026}".into()];
            let mut add_payload: Vec<(common::model::AutomationTarget, u32)> =
                vec![(common::model::AutomationTarget::SongTempo, 0)];
            for target in app.cursor_modulatable_targets() {
                let routed: &[(u32, f32, bool)] = mod_routings
                    .iter()
                    .find(|(_, t, _, _)| *t == target)
                    .map_or(&[], |(_, _, _, rs)| rs.as_slice());
                let tlabel = crate::app::automation_target_display_name(&target);
                for r in &mod_sources {
                    if routed.iter().any(|(rs, _, _)| *rs == r.id) {
                        continue;
                    }
                    add_labels.push(format!("{tlabel} \u{2190} {}", src_name(r.id)));
                    add_payload.push((target.clone(), r.id));
                }
            }
            if add_labels.len() > 1 {
                let label_refs: Vec<&str> = add_labels.iter().map(String::as_str).collect();
                let dd_rect = Rect {
                    x: area.x + pad,
                    y: my,
                    w: area.w - pad * 2.0,
                    h: 22.0,
                };
                if let Some(picked) =
                    ui.dropdown("inspector_mod_add_route", dd_rect, &label_refs, 0)
                    && picked > 0
                    && let Some((tgt, s)) = add_payload.get(picked).cloned()
                {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::AddModRouting {
                            track_id,
                            target: tgt,
                            source_id: s,
                        });
                    }));
                }
            }
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
