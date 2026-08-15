//! トラック inspector (左サイドバー):
//! - 選択トラック名
//! - 「Chain」見出し
//! - MIDI FX → Instrument → FX のリスト (各行に GUI / × ボタン、drag&drop で reorder)
//! - + Instrument / + Effect / + MIDI FX ボタン

mod chain_sections;
mod modulation_rack;

use daw_ui_core::{
    Edit, ReorderableListEditRequest, ReorderableListStyle, ScrubableNumberFormat,
    ScrubableNumberStyle, ToggleButtonStyle, Ui,
};
use daw_ui_renderer::Rect;

use crate::app::{
    text_num_to_builtin, AppData, AppEvent, ClipRef, ColorPickerTarget, DiscreteClipEdit,
    FadeEdgeKind, InspectorScrubField, TalkParamKind, TextNumField,
};
use crate::view::modulation::{self as mod_widget, build_mod, scrub_field_mod, ModBuild};
use crate::view::track_color;
use common::model::{AutomationTarget, FadeCurve, ImageBuiltinParam, StretchMode, TextAlign};

// r.md #48: style は const にできない (const はランタイムのパレットを読めず、テーマを
// 切り替えても古い色のまま残る)。 いずれも `Theme` を受け取る fn にして、 ui-core の
// `from_palette` を base にした差分だけをここで宣言する。

/// Audio event toggle (Reverse / Muted) 用 style。 mixer_strips の
/// STYLE_MUTE / STYLE_SOLO とほぼ同じだが、 inspector 側に独立して定義
/// (mixer の private style を import するより、 同 widget 並びの一覧性を
/// 優先)。 hint band は無し (= 単純トグル) にして、 文字 + ON/OFF 色だけで
/// 状態を伝える。
pub(super) fn toggle_audio_style(theme: &crate::theme::Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        radius: 4.0,
        font_size: 12.0,
        ..ToggleButtonStyle::from_palette(&theme.core)
    }
}

/// Image PiP / Group Transform / Text の automate toggle 用 style (= lane を作る /
/// 削除する 1 個 1 個のボタン)。 ON 色は arrangement automation lane ヘッダと同じ
/// `daw.automation_lane` (薄い藤色) で、「この field は lane 駆動中」 を視覚化する。
fn toggle_automate_style(theme: &crate::theme::Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        on_color: theme.daw.automation_lane,
        radius: 4.0,
        font_size: 11.0,
        ..ToggleButtonStyle::from_palette(&theme.core)
    }
}

/// inspector (audio / image / text / plugin param) と Group Transform が共有する
/// scrubable_number の base style。 sensitivity / range は param 別に上書きする。
/// ドラッグで連続変化 / click で text 入力 / dblclick で reset。
pub(super) fn scrub_style(theme: &crate::theme::Theme) -> ScrubableNumberStyle {
    ScrubableNumberStyle {
        // hover は窪みの既定 (`inset_bg_hover`) ではなく 1 段持ち上げた `control`。
        // inspector は同幅の数値欄が縦に何本も並ぶので、 hover 中の 1 本が面から
        // はっきり離れないと「いまどの行を掴んでいるか」 が読めない。
        bg_color_hovered: theme.core.control,
        font_size: 11.0,
        sensitivity: 0.004,
        ..ScrubableNumberStyle::from_palette(&theme.core)
    }
}

/// audio / image / text inspector
/// の数値 field を 1 行ぶん描く共通 helper。 `ui.scrubable_number_at` を呼び、
/// on_change で `make_event(v)` が返す `AppEvent` を全 event に broadcast、
/// drag / text 編集の開始・終了 edge で `BeginInspectorScrub` /
/// `EndInspectorScrub` を発火して一連の操作を undo 1 step に bracket する
/// (= Group Transform セクションと同 idiom)。 `scrub_key` は
/// `app.ui_ephemeral.inspector_scrub_active` の識別に使う。
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
    // 複数選択時は inspector_target_refs 全体へ broadcast する。 値が
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
    let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
    if active && !was_active {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
            app.handle_event(AppEvent::BeginInspectorScrub);
        }));
    } else if !active && was_active {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.inspector_scrub_active = None;
            app.handle_event(AppEvent::EndInspectorScrub);
        }));
    }
    // modulation depth ドラッグの falling edge で host 再同期 (自コントロールの
    // target を key に、他コントロールと干渉せず drag-end で 1 回だけ recompile)。
    if let Some((target, _)) = &mod_spec {
        mod_widget::push_mod_drag_resync(ui, app, cursor_track, target, resp.mod_dragging);
    }
}

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

/// チェーン (device 一覧) の reorder list style。
/// 「選択」 は param パネルの開閉で示すので、 選択行は accent で塗らず静止行と同じ面のまま。
fn chain_list_style(theme: &crate::theme::Theme) -> ReorderableListStyle {
    ReorderableListStyle {
        row_gap: 3.0,
        row_bg_selected: theme.core.panel_raised,
        radius: 3.0,
        ..ReorderableListStyle::from_palette(&theme.core)
    }
}

// ---- Sidechain セクションの行レイアウト (高さ予約と描画の SSoT) ----------
/// 1 行目 = プラグイン名 (行幅いっぱい)。
const SC_NAME_H: f32 = 14.0;
/// 2 行目 = [tap point | source] のコントロール行。
const SC_CTL_H: f32 = 24.0;
/// tap point dropdown の幅。 最長ラベル "Post-Fdr" = 8 字 * 14 * 0.527 = 59.1px、
/// dropdown の文字領域は w - PAD_X(8) - ARROW_W(16) なので 84px 以上必要。
const SC_TAP_W: f32 = 88.0;
const SC_ROW_H: f32 = SC_NAME_H + SC_CTL_H;
const SC_ROW_GAP: f32 = 6.0;

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
    let p = &app.theme.core;
    ui.panel("inspector_bg", area, p.panel, 0.0);

    let pad = 12.0;
    let mut y = area.y + pad;

    // 選択トラック名
    ui.label_at(
        "inspector_title",
        &app.selected_track_label(),
        area.x + pad,
        y,
        16.0,
        p.text,
    );

    // v18 (`docs/plan_track_clip_color.md`): タイトル行右端に track 色スウォッチ。
    // 単一トラック選択時のみ表示し、クリックで color_picker を開く (anchor =
    // スウォッチ rect)。effective 色 (上書き or id 由来の導出色) を塗る。
    if app.selection.selected_track_ids.len() <= 1
        && let Some(idx) = app.cursor_track_index()
        && let Some(track) = app.song_doc.song().tracks.get(idx)
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
            p.border,
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

    // ---- param セクションを縦スクロール領域に収める --------------
    // r.md #37: title 下〜area 下端の **全部** が scroll viewport。 inspector の縦位置は
    // 「1 本の y カーソル」 だけが決める (= 縦位置の SSoT が 1 つ)。
    //
    // 旧実装は viewport の下に 「chain band」 (Parallel Out / Sidechain / 「+ Plugin」) を
    // pinned で置き、 `btns_y = area.y + area.h - btns_h - pad` から **上へ逆算** して
    // 積んでいた。 その帰結が 3 つとも実害だった:
    //   (a) band 予約高の下限 `CHAIN_MIN_H = 160` が実コンテンツ (device 0 個なら
    //       ボタン 26 + pad 12 = 38px) を上回るので、 viewport 下端とボタンの間に
    //       誰も描かない空白が最大 122px 残る (= ユーザーの言う 「下寄せ」)。
    //   (b) 逆算配置は描画前に各セクションの高さを知る必要があるので、 高さ式が
    //       描画ループと二重管理になる。
    //   (c) 予約高に収まらない行を無言で捨てる cap が要る。 パラアウト 5 行 /
    //       sidechain 4 行を超えた分は **描画も操作もできなかった**。
    // 逆算を全廃すると (a)(b)(c) が同時に消える。 3 セクションは
    // `chain_sections::draw_*` として scroll フロー内へ移した。
    //
    // param の実高さは前フレーム測定値 (`inspector_body_h`、 immediate-mode の
    // lag-by-one) を content_size に使う。 content <= viewport なら scrollbar は出ない。
    // dropdown popup は deferred buffer 描画なので clip_rect の外に出て切れない
    // (gui_01 popup.rs)。 closure body の param セクションは既存コードのまま
    // (再インデントしない)。
    let body_top = y;
    let param_h = (area.y + area.h - body_top).max(0.0);
    let content_h = app.ui_ephemeral.inspector_body_h.max(1.0);
    let param_vp = Rect { x: area.x, y: body_top, w: area.w, h: param_h };
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
        if app.ui_ephemeral.clip_edit_buffer_target != Some(summary.target) {
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
            Rect {
                x: area.x + pad + toggle_w + 6.0,
                y,
                w: toggle_w,
                h: toggle_h,
            },
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
            ScrubableNumberFormat::Decimal(2),
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
            app.inspector_fold(|a, t| a.audio_first_event(t, |e| e.fade_in_beats)),
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
            app.inspector_fold(|a, t| a.audio_first_event(t, |e| e.fade_out_beats)),
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
        if app.ui_ephemeral.clip_edit_buffer_target != Some(summary.target) {
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
        y += input_h + 8.0;

        // Fade In / Out (length + curve)。 audio section と同じ idiom
        // で 1 行に length + curve dropdown を並べる。
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
            app.inspector_fold(|a, t| a.image_first_event(t, |e| e.fade_in_beats)),
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
            app.inspector_fold(|a, t| a.image_first_event(t, |e| e.fade_out_beats)),
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
        y += input_h + 12.0;
    }

    // ---- Plugin chain + 行内アコーディオン -----------------
    // チェーン (プラグイン一覧) を viewport 内に出し、 各行の Par で開いたデバイスの
    // param パネルを **その行の直下** に展開する (`reorderable_list_expandable`)。 開いた
    // 行だけ `row_extra_h > 0` → expansion クロージャが呼ばれ、 中の各 section gate が
    // その開いたデバイスの params を描く。 展開高は前フレーム測定値
    // (`inspector_device_panel_h`、 未測定は default で bootstrap して 1 度描かせる)。
    {
        let chain = app.inspector_chain();
        let cursor_tid = app.cursor_track_id();
        let chain_style = chain_list_style(&app.theme);
        let row_total_h = chain_style.row_height + chain_style.row_gap;
        // 開いているデバイスの chain device_index (open_plugin_params / open_video_fx_params、
        // cursor track 上のものだけ)。
        let open_dev: Option<u32> = app
            .ui_ephemeral.open_plugin_params
            .or(app.ui_ephemeral.open_video_fx_params)
            .filter(|(t, _)| Some(*t) == cursor_tid)
            .map(|(_, idx)| idx);
        let panel_h = if app.ui_ephemeral.inspector_device_panel_h > 1.0 {
            app.ui_ephemeral.inspector_device_panel_h
        } else {
            280.0 // 初回 bootstrap: expansion を 1 度描かせて実測させる
        };
        let chain_h = chain.len() as f32 * row_total_h
            + if open_dev.is_some() { panel_h } else { 0.0 }
            + 4.0;
        let btn_gui_w = 44.0;
        let btn_x_w = 30.0;
        ui.label_at("inspector_chain_label", "Chain", area.x + pad, y, 12.0, p.text);
        y += 18.0;
        // 他セクションと同じ左右 pad を取る。 旧実装は area 幅いっぱい (280px) だった
        // ため、 inspector 本体が縦スクロールすると右端 10px が scrollbar に隠れ、
        // その帯 (描画されないのに hit-test は生きている) の click が chain 行の
        // 「x」 (device 削除) を誤発火しえた。
        let chain_rect = Rect { x: area.x + pad, y, w: area.w - pad * 2.0, h: chain_h };
        ui.reorderable_list_expandable(
            "inspector_chain",
            chain_rect,
            &chain,
            None,
            &chain_style,
            |req| match req {
                ReorderableListEditRequest::Reorder(order) => {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::ReorderInspectorChain(order.clone()));
                    })
                }
            },
            |ui, entry, idx, row_rect, _selected, _dragging| {
                let device_index = entry.device_index;
                let gui_x = row_rect.x + row_rect.w - btn_gui_w - btn_x_w - 4.0;
                // 名前は右の [Par|GUI] / [x] ボタンの手前で打ち切る。 素の label_at だと
                // 長いプラグイン名 (例: "BBC Symphony Orchestra Professional") が
                // ボタンの上に重なって読めなくなる。
                let name_x = row_rect.x + 8.0;
                let buttons_left = if entry.shows_button() {
                    gui_x
                } else {
                    row_rect.x + row_rect.w - btn_x_w
                };
                // load に失敗した device は host に instance が無い = 無音。
                // どの行が死んでいるかを行そのもので示し、 理由と復旧手段
                // (再読込) は下の 「読み込み失敗」 セクションが出す。
                let failed = entry.load_error.is_some();
                // 正常時は借用のまま (毎フレーム全 device 分の String を作らない)。
                let display_name: std::borrow::Cow<'_, str> = if failed {
                    format!("[未ロード] {}", entry.plugin_name).into()
                } else {
                    entry.plugin_name.as_str().into()
                };
                ui.label_at_clipped(
                    ("inspector_row_name", idx),
                    &display_name,
                    Rect {
                        x: name_x,
                        y: row_rect.y + 8.0,
                        w: (buttons_left - 6.0 - name_x).max(1.0),
                        h: 11.0 * 1.2,
                    },
                    11.0,
                    if failed { p.text_error } else { p.text },
                );
                if entry.shows_button() {
                    let label = if entry.shows_param_panel() { "Par" } else { "GUI" };
                    ui.button_at(
                        ("inspector_row_gui", idx),
                        label,
                        Rect { x: gui_x, y: row_rect.y + 2.0, w: btn_gui_w, h: row_rect.h - 4.0 },
                        move || {
                            Edit::mutate(move |app: &mut AppData| {
                                app.handle_event(AppEvent::ToggleSlotGui { index: device_index })
                            })
                        },
                    );
                }
                let xb_x = row_rect.x + row_rect.w - btn_x_w;
                ui.button_at(
                    ("inspector_row_remove", idx),
                    "x",
                    Rect { x: xb_x, y: row_rect.y + 2.0, w: btn_x_w, h: row_rect.h - 4.0 },
                    move || {
                        Edit::mutate(move |app: &mut AppData| {
                            app.handle_event(AppEvent::RemoveDevice { index: device_index })
                        })
                    },
                );
            },
            |i| {
                if chain.get(i).map(|e| e.device_index) == open_dev {
                    panel_h
                } else {
                    0.0
                }
            },
            |ui, _exp_i, exp_rect| {
                let panel_top = exp_rect.y;
                #[allow(unused_mut)]
                let mut y = exp_rect.y;
                // ====== 開いたデバイスの param パネル本体 (各 section gate が選別) ======

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
            p.text,
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
            ui.label_at((param, "group_label"), label, area.x + pad, y + 5.0, 11.0, p.text);
            let style =
                ScrubableNumberStyle { sensitivity: sens, range, ..scrub_style(&app.theme) };
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
            let was_active = app.ui_ephemeral.group_scrub_active == Some(param);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.group_scrub_active = Some(param);
                    app.handle_event(AppEvent::BeginGroupTransformDrag);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.group_scrub_active = None;
                    app.handle_event(AppEvent::EndGroupTransformDrag);
                }));
            }
            let auto_on = summary.automated[idx];
            ui.toggle_button_at(
                (param, "group_auto"),
                "A",
                Rect { x: auto_btn_x, y, w: auto_btn_w, h: input_h },
                auto_on,
                &toggle_automate_style(&app.theme),
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

    // 内蔵映像 FX のパラメータ調整パネル（チェーン行の "GUI" ボタンで開閉）。
    // 各 param を scrubable_number で実レンジ表示 + per-control 変調（Ranged domain で kick→効果）。
    // 値の SSoT は PluginParam lane の default_value（`SetVideoFxParam` が格納）。
    if let Some(view) = app.inspector_video_fx_params() {
        ui.label_at("inspector_vfx_label", view.def.name, area.x + pad, y, 12.0, p.text);
        y += 18.0;
        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 88.0;
        let input_x = area.x + pad + label_w;
        let input_w = (row_w - label_w).max(40.0);
        let track_id = view.track_id;
        let device_index = view.device_index;
        // v29: lane target は安定 device_id。 0 (未解決 — panel が開いている限り
        // 起きないはず) は lane に一致しない sentinel なので表示 fallback として無害。
        let device_id =
            crate::app::device_id_at(app.song_doc.song(), track_id, device_index).unwrap_or(0);
        for (i, param) in view.def.params.iter().enumerate() {
            let value = f64::from(view.values[i]);
            let (min, max) = param.kind.range();
            let (min, max) = (f64::from(min), f64::from(max));
            let default_real = f64::from(param.kind.norm_to_real(param.kind.default_norm()));
            #[allow(clippy::cast_possible_truncation)]
            let sens = (((max - min) / 220.0).max(0.0001)) as f32;
            ui.label_at_clipped(
                (i, "vfx_label"),
                param.name,
                Rect {
                    x: area.x + pad,
                    y: y + 5.0,
                    w: (label_w - 4.0).max(1.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                p.text,
            );
            let style = ScrubableNumberStyle {
                sensitivity: sens,
                range: Some((min, max)),
                ..scrub_style(&app.theme)
            };
            let target = AutomationTarget::PluginParam {
                device_id,
                param_id: param.id,
                legacy_device_index: None,
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
            // drag / text 編集の開始・終了 edge で undo を 1 step に bracket（毎フレームの
            // SetVideoFxParam は非 undoable、stroke 先頭で 1 snapshot）。
            let active = resp.dragging || resp.editing_text;
            let scrub_key = crate::app::InspectorScrubField::VideoFx { device_index, param_id };
            let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
                    app.handle_event(AppEvent::BeginInspectorScrub);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = None;
                    app.handle_event(AppEvent::EndInspectorScrub);
                }));
            }
            // 変調 depth ドラッグの falling edge で host 再同期（音声 target の depth 反映用）。
            mod_widget::push_mod_drag_resync(ui, app, track_id, &target, resp.mod_dragging);
            y += input_h + 4.0;
        }
        y += 8.0;
    }

    // ---- Plugin param panel -----------------------------------
    // 埋め込み GUI を持たない plugin (VOICEVOX builtin / GUI 無し CLAP・VST3) の
    // チェーン行「Par」ボタンで開閉。 VOICEVOX は device 既定の声 (キャラ→スタイル)
    // を、 汎用 plugin は param を scrubable_number で実レンジ編集する。 値の SSoT は
    // PluginParam lane の default_value (= `set_plugin_param`、 映像 FX と同 idiom)。
    if let Some(view) = app.inspector_plugin_params() {
        let device_index = view.device_index;
        let track_id = view.track_id;
        // v29: lane target は安定 device_id (上の video FX パネルと同じ fallback)。
        let device_id =
            crate::app::device_id_at(app.song_doc.song(), track_id, device_index).unwrap_or(0);
        ui.label_at_clipped(
            "inspector_pp_label",
            &view.plugin_name,
            Rect { x: area.x + pad, y, w: (area.w - pad * 2.0).max(1.0), h: 12.0 * 1.2 },
            12.0,
            p.text,
        );
        y += 18.0;

        // 汎用 param 行 (scrubable_number で実レンジ + per-control 変調)。
        let row_w = area.w - pad * 2.0;
        let input_h = 22.0;
        let label_w = 96.0;
        let input_x = area.x + pad + label_w;
        let input_w = (row_w - label_w).max(40.0);
        for (i, row) in view.params.iter().enumerate() {
            // param 名はプラグイン由来で長さ上限が無い。 label_w(96) を超えると
            // 後続の値ボックス (不透明 bg) に覆われて途中で消えるので rect で切る。
            ui.label_at_clipped(
                (i, "pp_name"),
                &row.name,
                Rect {
                    x: area.x + pad,
                    y: y + 5.0,
                    w: (label_w - 4.0).max(1.0),
                    h: 11.0 * 1.2,
                },
                11.0,
                p.text,
            );
            if row.readonly {
                // 編集不可 param は現値をラベル表示するだけ。
                let txt = format!("{:.3}", row.value_real);
                ui.label_at_clipped(
                    (i, "pp_ro"),
                    &txt,
                    Rect { x: input_x, y: y + 5.0, w: input_w, h: 11.0 * 1.2 },
                    11.0,
                    p.text,
                );
                y += input_h + 4.0;
                continue;
            }
            let (min, max) = (row.min, row.max);
            #[allow(clippy::cast_possible_truncation)]
            let sens = (((max - min) / 220.0).max(0.0001)) as f32;
            let style = ScrubableNumberStyle {
                sensitivity: sens,
                range: Some((min, max)),
                ..scrub_style(&app.theme)
            };
            let target = AutomationTarget::PluginParam {
                device_id,
                param_id: row.id,
                legacy_device_index: None,
            };
            let domain = crate::app::ModControlDomain::Ranged { min, max, log: false };
            let mod_build = build_mod(app, target.clone(), row.value_real, domain, track_id);
            let modulation = Some(mod_build.modulation());
            let param_id = row.id;
            let fmt = if row.stepped {
                ScrubableNumberFormat::Decimal(0)
            } else {
                ScrubableNumberFormat::Decimal(3)
            };
            let resp = ui.scrubable_number_at(
                (i, "pp_scrub"),
                Rect { x: input_x, y, w: input_w, h: input_h },
                row.value_real,
                row.default_real,
                fmt,
                &style,
                move |v| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetPluginParam {
                            device_index,
                            param_id,
                            value_real: v,
                        });
                    })
                },
                None,
                modulation,
            );
            // drag / text 編集 stroke を undo 1 step に bracket (`SetPluginParam` は
            // per-frame 非 undoable)。 終端で host へ 1 回 resync して音に反映する
            // (映像 FX と違い audio plugin は daw_audio が lane を読むため要 push)。
            let active = resp.dragging || resp.editing_text;
            let scrub_key = crate::app::InspectorScrubField::PluginParam { device_index, param_id };
            let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
                    app.handle_event(AppEvent::BeginInspectorScrub);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = None;
                    app.handle_event(AppEvent::EndInspectorScrub);
                    // 確定値の host push は不要: scrub 中の edit_song が bump した epoch を
                    // runner の frame flush が per-frame で LoadSong する (sync 一本化)。
                }));
            }
            // 変調 depth ドラッグの falling edge で host 再同期。
            mod_widget::push_mod_drag_resync(ui, app, track_id, &target, resp.mod_dragging);
            y += input_h + 4.0;
        }
        y += 8.0;
    }

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
        .and_then(|r| app.song_doc.song().tracks.get(r.track as usize))
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

        // 23 numeric rows + 2 fade beats を 1 closure 化した helper。
        // 各行を scrubable_number 化し (= 1 箇所変更で 25 行全部が drag 対応)、
        // `scrub_field` で drag / text 編集を undo 1 step に bracket する。
        // automate 「A」 トグルは従来どおり共存。 値源は summary の first event
        // snapshot、 on_change は `SetClipTextNumField` (Rotation は deg→rad)。
        let text_fade_max = summary.fade_max_beats.max(0.0);
        let emit_num_row = |ui: &mut Ui<'_, AppData>,
                            app: &AppData,
                            label: &str,
                            field: TextNumField,
                            row_y: &mut f32| {
            // ラベル欄は input_x までの label_w(60) しかない。 "Sh Blur (px)" は
            // 実 advance 69.6px でここを溢れ、 後から描かれる数値ボックスの
            // 不透明背景に末尾が食われていた。
            ui.label_at_clipped(
                (field, "label"),
                label,
                Rect {
                    x: area.x + pad,
                    y: *row_y + 5.0,
                    w: (label_w - 2.0).max(1.0),
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
            let style =
                ScrubableNumberStyle { sensitivity: sens, range, ..scrub_style(&app.theme) };
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
                    &toggle_automate_style(&app.theme),
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

    // カーソルトラックの index。None のとき以降の per-track セクションは
    // 描画しない (track 0 を誤対象にしない)。
    let cursor_idx = app.cursor_track_index();

    // Clip Voice 編集: 選択中の clip が vocal track 上の MIDI clip の
    // とき、 キャラ ▼ → スタイル ▼ の 2 段 dropdown で per-clip 声を選ぶ。
    // 声は per-clip (`Clip::speaker_id`) が SSoT、 SetClipVoice で焼き込む。
    if app.voicevox_param_panel_open()
        && let Some(r) = app.selected_clip_ref()
        && let Some(track) = app.song_doc.song().tracks.get(r.track as usize)
        && track.is_voicevox_vocal()
        && let Some(clip) = track.clips.get(r.clip as usize)
        && app
            .song_doc.song()
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
        } else if let Some(found) = app.voicevox.singers.iter().find_map(|s| {
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
            p.text,
        );
        y += 18.0;

        if app.voicevox.singers.is_empty() {
            // engine 未起動 / 一覧未取得: 焼き込み声名 + 取得中。 声名は常に出せる。
            let txt = format!("{cur_singer} - {cur_style}  (一覧取得中…)");
            ui.label_at(
                "inspector_clip_voice_current",
                &txt,
                area.x + pad + 4.0,
                y + 6.0,
                11.0,
                p.text,
            );
            y += 26.0;
        } else {
            // 上段: キャラ dropdown。
            let char_labels: Vec<&str> =
                app.voicevox.singers.iter().map(|s| s.name.as_str()).collect();
            let cur_char_idx = app
                .voicevox.singers
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
            let char_idx = picked_char.unwrap_or(cur_char_idx).min(app.voicevox.singers.len() - 1);
            let singer = &app.voicevox.singers[char_idx];
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
                app.voicevox.singers.get(pc).and_then(|s| {
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

    // (talk) Text Clip 読み上げ編集 (`docs/plan_voicevox_talk.md` §4)。選択中 clip が
    // VOICEVOX デバイス付きトラック上の Text clip のとき、talk 話者 (キャラ→talk style)
    // + 読み上げスケール 4 つ (話速/音高/抑揚/音量) を編集する。声は `Clip::speaker_id`
    // を talk style として流用 (SetClipVoice で焼き込み)。スケールは `Clip::talk`。
    if app.voicevox_param_panel_open()
        && let Some(r) = app.selected_clip_ref()
        && let Some(track) = app.song_doc.song().tracks.get(r.track as usize)
        && track.is_voicevox_vocal()
        && let Some(clip) = track.clips.get(r.clip as usize)
        && app
            .song_doc.song()
            .clip_contents
            .get(&clip.content_id)
            .is_some_and(|c| matches!(c, common::model::ClipContent::Text(_)))
    {
        let clip_key = common::model::ClipKey {
            track_id: track.id,
            clip_id: clip.id,
        };
        let cur_speaker = clip.speaker_id;
        let has_subtitle = track.has_subtitle_device();
        let talk = clip.talk.unwrap_or_default();
        // 現在の talk 声名: clip 焼き込み名 → speaker_id 逆引き → 空 (取得中表示)。
        let (cur_char, cur_style) = if !clip.singer_name.is_empty() {
            (clip.singer_name.clone(), clip.style_name.clone())
        } else {
            app.voicevox.talk_speakers
                .iter()
                .find_map(|s| {
                    s.styles
                        .iter()
                        .find(|st| st.id == cur_speaker)
                        .map(|st| (s.name.clone(), st.name.clone()))
                })
                .unwrap_or_default()
        };

        ui.label_at(
            "inspector_talk_label",
            "読み上げ (Talk)",
            area.x + pad,
            y,
            12.0,
            p.text,
        );
        y += 18.0;

        // 字幕デバイス未挿入 = 画面非表示。ワンクリック追加ヘルパ (Q10)。
        if !has_subtitle {
            let warn_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 22.0,
            };
            if ui.button_at_clicked(
                "inspector_talk_add_subtitle",
                "+ 字幕デバイス (画面に表示)",
                warn_rect,
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::SelectPluginFromDb {
                        id: common::plugin_db::SUBTITLE_ID.to_string(),
                        keep_open: false,
                        open_gui: false,
                    });
                }));
            }
            y += 26.0;
        }

        // (talk) 本文 (セリフ) 入力。字幕 device 時は overlay「Text Event」節が本文入力を
        // 持つので、ここは字幕 device 無し (= 喋るが映さない talk-only) のときだけ出し、
        // 二重入力を避ける。編集 buffer / events は overlay と共用 (同時表示しないので競合せず)。
        if !has_subtitle {
            if app.ui_ephemeral.clip_edit_buffer_target != Some(r) {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ResyncClipTextEditBuffers(r));
                }));
            }
            ui.label_at(
                "inspector_talk_text_label",
                "セリフ",
                area.x + pad,
                y + 5.0,
                11.0,
                p.text,
            );
            let resp = ui.text_input_at(
                "inspector_talk_text_input",
                Rect {
                    x: area.x + pad + 48.0,
                    y,
                    w: area.w - pad * 2.0 - 48.0,
                    h: 22.0,
                },
                &app.ui_ephemeral.clip_text_content_edit_text,
                |s| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::ClipTextContentEditChanged(s))
                    })
                },
            );
            // Enter でも外クリック (blurred = focus loss) でも確定する (daw_01 #112)。
            if resp.committed || resp.blurred {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::CommitClipTextContentEdit)
                }));
            }
            y += 26.0;
        }

        // talk 話者 picker (キャラ → talk style)。
        if app.voicevox.talk_speakers.is_empty() {
            let txt = if cur_char.is_empty() {
                "(talk 声一覧 取得中…)".to_string()
            } else {
                format!("{cur_char} - {cur_style}  (一覧取得中…)")
            };
            ui.label_at(
                "inspector_talk_voice_current",
                &txt,
                area.x + pad + 4.0,
                y + 6.0,
                11.0,
                p.text,
            );
            y += 26.0;
        } else {
            let char_labels: Vec<&str> =
                app.voicevox.talk_speakers.iter().map(|s| s.name.as_str()).collect();
            let cur_char_idx = app
                .voicevox.talk_speakers
                .iter()
                .position(|s| s.name == cur_char)
                .or_else(|| {
                    app.voicevox.talk_speakers
                        .iter()
                        .position(|s| s.styles.iter().any(|st| st.id == cur_speaker))
                })
                .unwrap_or(0);
            let char_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 24.0,
            };
            let picked_char =
                ui.dropdown("inspector_talk_char", char_rect, &char_labels, cur_char_idx);
            y += 28.0;

            let char_idx = picked_char
                .unwrap_or(cur_char_idx)
                .min(app.voicevox.talk_speakers.len() - 1);
            let speaker = &app.voicevox.talk_speakers[char_idx];
            let style_labels: Vec<&str> =
                speaker.styles.iter().map(|st| st.name.as_str()).collect();
            let cur_style_idx = speaker
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
                "inspector_talk_style",
                style_rect,
                &style_labels,
                cur_style_idx,
            );
            y += 28.0;

            let chosen: Option<(u32, String, String)> = if let Some(pc) = picked_char {
                app.voicevox.talk_speakers.get(pc).and_then(|s| {
                    s.styles
                        .first()
                        .map(|st| (st.id, s.name.clone(), st.name.clone()))
                })
            } else if let Some(ps) = picked_style {
                speaker
                    .styles
                    .get(ps)
                    .map(|st| (st.id, speaker.name.clone(), st.name.clone()))
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

            let refetch_rect = Rect {
                x: area.x + pad,
                y,
                w: area.w - pad * 2.0,
                h: 22.0,
            };
            if ui.button_at_clicked(
                "inspector_talk_refetch",
                "talk 声一覧を再取得",
                refetch_rect,
            ) {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::RefetchSpeakers);
                }));
            }
            y += 28.0;
        }

        // 読み上げスケール 4 つ。VOICEVOX talk の話速/音高/抑揚/音量。
        let scales = [
            ("話速", TalkParamKind::Speed, f64::from(talk.speed_scale), 1.0),
            ("音高", TalkParamKind::Pitch, f64::from(talk.pitch_scale), 0.0),
            (
                "抑揚",
                TalkParamKind::Intonation,
                f64::from(talk.intonation_scale),
                1.0,
            ),
            ("音量", TalkParamKind::Volume, f64::from(talk.volume_scale), 1.0),
        ];
        for (label, kind, val, default) in scales {
            ui.label_at(
                ("inspector_talk_scale_label", label),
                label,
                area.x + pad,
                y + 4.0,
                11.0,
                p.text,
            );
            let input_rect = Rect {
                x: area.x + pad + 48.0,
                y,
                w: area.w - pad * 2.0 - 48.0,
                h: 20.0,
            };
            let resp = ui.scrubable_number_at(
                ("inspector_talk_scale", label),
                input_rect,
                val,
                default,
                ScrubableNumberFormat::Decimal(2),
                &scrub_style(&app.theme),
                move |v| {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetClipTalkParam {
                            clip: clip_key,
                            param: kind,
                            value: v as f32,
                        });
                    })
                },
                None,
                None,
            );
            // 他の inspector 数値 field (`scrub_field`) と同じ Begin/End bracket
            // で 1 drag = 1 undo step にする — これが無いと talk 4 項目だけ
            // Ctrl+Z で戻せない (review)。
            let scrub_key = crate::app::InspectorScrubField::Talk(kind);
            let active = resp.dragging || resp.editing_text;
            let was_active = app.ui_ephemeral.inspector_scrub_active == Some(scrub_key);
            if active && !was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = Some(scrub_key);
                    app.handle_event(AppEvent::BeginInspectorScrub);
                }));
            } else if !active && was_active {
                ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                    app.ui_ephemeral.inspector_scrub_active = None;
                    app.handle_event(AppEvent::EndInspectorScrub);
                }));
            }
            y += 24.0;
        }
    }

    // ---- 親グループ (Parent) の編集 UI はインスペクタから撤去した ----
    // 親子 (グループ階層) の編集はアレンジビューでのトラックドラッグ
    // (drag reparent SetTrackParent) 一本に統一する。階層は
    // アレンジの入れ子インデントで可視化されるので、同じ概念をインスペクタの
    // ドロップダウンでも編集できると Single Source of Truth が崩れる。
    // `AppEvent::SetTrackParent` / `action_set_track_parent` 自体はアレンジ
    // ドラッグが使うため残す。

    // ---- 口パク (lip-sync) 出力先 binding ----------------------------
    // Vocal track のみ。生成した口画像 ImageEvent を焼き込む先の口 track
    // (立ち絵 group の子 image track) を選ぶ。設定で再生成が走る。
    // VOICEVOX device の「Par」を押したときだけ出す (= 専用欄を常時
    // 表示せず Par パネルに集約。声 / 話速 / 口パク先をまとめて 1 箇所で編集)。
    if app.voicevox_param_panel_open()
        && let Some(track) = cursor_idx.and_then(|i| app.song_doc.song().tracks.get(i))
        && track.is_voicevox_vocal()
    {
        let self_id = track.id;
        // 候補: 自分以外の全 track (= 口 track はどれでも選べる)。
        // candidate_ids[k] と labels[k+1] が対応 (labels[0] = "(なし)" sentinel)。
        // 表示名はここで 1 度だけ format/clone する (別 Vec への再 clone を避ける)。
        let mut candidate_ids: Vec<u32> = Vec::new();
        let mut labels: Vec<String> = Vec::new();
        labels.push("(なし)".into());
        for t in app.song_doc.song().tracks.iter().filter(|t| t.id != self_id) {
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
            p.text,
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
                // ====== /device param panel ======
                // 展開部の実消費高を測って次フレームの row_extra_h に使う (lag-by-one)。
                let measured = (y - panel_top).max(0.0);
                if (app.ui_ephemeral.inspector_device_panel_h - measured).abs() > 0.5 {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.ui_ephemeral.inspector_device_panel_h = measured;
                    }));
                }
            },
        );
        // チェーン (rows + 開いた展開) ぶん viewport y を進める。
        y = chain_rect.y + chain_rect.h + 8.0;
    }

    // r.md #37: チェーン直下に 「+ Plugin」 → Parallel Out → Sidechain を top-down で
    // 並べる (旧: inspector 下端に pinned)。 「このチェーンの末尾に足す」 「このチェーンの
    // デバイスの配線」 が読み順で自明になる。 各 fn は modulation_rack と同じ
    // `(app, ui, area, pad, y) -> f32` contract。
    y = chain_sections::draw_add_plugin_button(app, ui, area, pad, y);
    // ロード失敗は他の配線セクションより上 (= チェーンに最も近い位置)。
    // 「+ Plugin」 だけはチェーン末尾に接していることが座標で意味を持つので
    // その直後に置く。
    y = chain_sections::draw_failed_load_section(app, ui, area, pad, y);
    y = chain_sections::draw_parallel_out_section(app, ui, area, pad, y);
    y = chain_sections::draw_sidechain_section(app, ui, area, pad, y);
    y = chain_sections::draw_editor_key_section(app, ui, area, pad, y);

    let cursor_idx = app.cursor_track_index();

    // ---- 口パク mapping (口形状 → 画像) -------------------------------
    // この track を口パク出力先に指定している vocal track があるとき、7 形状
    // (a/i/u/e/o/N/閉口) の画像割当を表示する。各 slot は import 済み image を選ぶ。
    if let Some(track) = cursor_idx.and_then(|i| app.song_doc.song().tracks.get(i)) {
        let this_id = track.id;
        let is_target = app
            .song_doc.song()
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
                p.text,
            );
            y += 18.0;
            // import 済み image source の (id, ファイル名) 一覧 (id 昇順)。
            // image_ids[k] と labels[k+1] が対応 (labels[0] = "(なし)" sentinel)。
            // ラベル文字列を別 Vec へ再 clone せず、 ソート後そのまま labels へ move する。
            let mut images: Vec<(common::model::ImageSourceId, String)> = app
                .song_doc.song()
                .media.image_sources
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
                    p.text,
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

    // Modulation rack を scroll viewport 末尾に置く (top-down フロー、
    // 展開で 1 個を大きなグラフィカルエディタに、ソース数無制限・スクロール)。
    y += 10.0;
    y = modulation_rack::draw_modulation_rack(app, ui, area, pad, y);

    measured_body_h.set(y - (body_top - scroll_off.1));
    });
    // 測定した param 実高さを次フレーム用に保存 (変化時のみ edit を積む)。
    let measured = measured_body_h.get();
    if (app.ui_ephemeral.inspector_body_h - measured).abs() > 0.5 {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.inspector_body_h = measured;
        }));
    }
}
