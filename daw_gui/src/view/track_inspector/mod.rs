//! トラック inspector (左サイドバー):
//! - 選択トラック名
//! - 「Chain」見出し
//! - MIDI FX → Instrument → FX のリスト (各行に GUI / × ボタン、drag&drop で reorder)
//! - + Instrument / + Effect / + MIDI FX ボタン

mod chain_sections;
mod device_panel;
/// r.md #87: 選択中のランチャーセルのローンチ設定 (Q7 / 計画書 §3.4)。
mod launch_section;
mod modulation_rack;

use daw_ui_core::{
    Edit, ReorderableListEditRequest, ReorderableListStyle, ScrubableNumberFormat,
    ScrubableNumberStyle, TextInputStyle, ToggleButtonStyle, Ui,
};
use daw_ui_renderer::Rect;

use crate::app::{
    text_num_to_builtin, AppData, AppEvent, ChainEntry, ClipRef, ColorPickerTarget,
    DeviceDragPayload, DiscreteClipEdit, FadeEdgeKind, InspectorScrubField, RelocateDevices,
    TalkParamKind, TextNumField,
};
use crate::widgets::select_modifier::SelectModifier;
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

/// r.md #71 (プラグインのコピー / 移動): チェーン操作 (運搬 / 右クリックメニュー) が
/// 対象にする device 集合。 **掴んだ / 右クリックした行が選択に含まれていれば選択全体、
/// 含まれていなければその行だけ** (トラックヘッダの右クリックメニューと同じ規則、
/// `arrangement_view.rs`)。 順序は表示チェーン順。
fn carried_device_ids(app: &AppData, chain: &[ChainEntry], device_id: u64) -> Vec<u64> {
    if app.selection.selected_device_ids.contains(&device_id) {
        chain
            .iter()
            .filter(|c| app.selection.selected_device_ids.contains(&c.device_id))
            .map(|c| c.device_id)
            .collect()
    } else {
        vec![device_id]
    }
}

/// チェーン行の右端に並ぶボタン。
enum ChainRowButton {
    /// `GUI` / `Par` — プラグイン窓 (またはパラメータ欄) の開閉。
    ToggleGui,
    /// `x` — この device を外す。
    Remove,
}

/// チェーン行のボタンが返す `Edit`。
///
/// `[[feedback_popup_click_leaks_to_background]]`: 右クリックメニューが開いている frame は
/// **描くが発行しない** (項目 click が下の行のボタンを誤発火する)。判定は `Edit` の中で行う
/// — メニューはこの行より後に描かれるので、ボタンを積む時点ではまだ確定していない。
fn chain_row_edit(button: ChainRowButton, device_id: u64, popup_open: bool) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        if popup_open {
            return;
        }
        match button {
            ChainRowButton::ToggleGui => app.handle_event(AppEvent::ToggleSlotGui { device_id }),
            ChainRowButton::Remove => app.handle_event(AppEvent::RemoveDevices {
                device_ids: vec![device_id],
            }),
        }
    })
}

/// r.md #71 (プラグインのコピー / 移動): チェーン行の右クリックメニュー
/// (`コピー / 切り取り / 貼り付け / 複製 / 削除`) 1 項目分の実行。
///
/// 対象集合は **項目を選んだときだけ**組む (毎フレーム全行分作ると無駄な確保になる。
/// arrangement のトラックヘッダメニューが `target_ids()` を遅延させているのと同じ形)。
/// `idx` は上のラベル配列と同順。
fn apply_chain_menu_action(
    app: &mut AppData,
    idx: usize,
    device_id: u64,
    dest_track: Option<u32>,
) {
    let chain = app.inspector_chain();
    let ids = carried_device_ids(app, &chain, device_id);
    match idx {
        0 => app.copy_devices(ids),
        1 => app.cut_devices(ids),
        // 貼り付け位置は「この device の直前」。 選択をこの device 1 本にしてから
        // **Ctrl+V と同じ経路** を起こす (挿入位置の決定は `paste_devices` が選択から
        // 引くので、規則も経路も 1 本のまま。 OS クリップボードの読み出しは shortcut
        // layer が担うので、ここで二重に読まない)。
        2 => {
            app.set_device_selection(vec![device_id]);
            app.ui_ephemeral.pending_shortcut_injections.push("paste");
        }
        // 複製 = 選んだ device の直後にコピーを挿す。
        3 => duplicate_devices_after(app, ids, device_id, dest_track),
        _ => app.handle_event(AppEvent::RemoveDevices { device_ids: ids }),
    }
}

/// `device_id` の直後に `ids` のコピーを挿す (チェーン行メニューの「複製」)。
fn duplicate_devices_after(
    app: &mut AppData,
    ids: Vec<u64>,
    device_id: u64,
    dest_track: Option<u32>,
) {
    let Some(dest_track) = dest_track else { return };
    let Some(dest_index) = app
        .song_doc
        .song()
        .fx_chain_by_track_id(dest_track)
        .and_then(|c| c.iter().position(|d| d.id == device_id))
        .map(|i| i as u32 + 1)
    else {
        return;
    };
    app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
        device_ids: ids,
        dest_track,
        dest_index,
        copy: true,
    }));
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

    // ---- r.md #87: ランチャーのセルのローンチ設定 (Q7) -------------------
    // 選択にセルが 1 つも無ければ何も描かない (= 通常のクリップ編集時は
    // 従来と 1px も変わらない)。 先頭に置くのは「セルを選んだらまずここを見る」
    // 導線のため (`chain_sections` と同じ `(app, ui, area, pad, y) -> f32` 契約)。
    y = launch_section::draw_launch_section(app, ui, area, pad, y);

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
        // 右クリックメニューが開いている frame は行の click / button を評価しない
        // (`[[feedback_popup_click_leaks_to_background]]`: capture_input=false の popup は
        // 背景の pointer を mask しないので、項目 click が下の行まで届く)。
        let popup_open = ui.has_open_popups();
        // 開いているデバイス (open_plugin_params / open_video_fx_params)。 表示中の
        // チェーンに居るものだけ展開する (r.md #71: パネルは device_id で開いたまま
        // にして、 描画側で gate する)。
        let open_dev: Option<u64> = app
            .ui_ephemeral.open_plugin_params
            .or(app.ui_ephemeral.open_video_fx_params)
            .filter(|id| chain.iter().any(|e| e.device_id == *id));
        // r.md #71: 選択中の行 (表示チェーンとの交差なので、異トラックの id は出ない
        // = `live_device_ids()` と同じ正規化になる)。
        let selected_rows: Vec<usize> = chain
            .iter()
            .enumerate()
            .filter(|(_, e)| app.selection.selected_device_ids.contains(&e.device_id))
            .map(|(i, _)| i)
            .collect();
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
        let resp = ui.reorderable_list_expandable(
            "inspector_chain",
            chain_rect,
            &chain,
            &selected_rows,
            Some(crate::app_types::DEVICE_DRAG_KIND),
            &chain_style,
            |req| match req {
                ReorderableListEditRequest::Reorder(order) => {
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::ReorderInspectorChain(order.clone()));
                    })
                }
            },
            |ui, entry, idx, row_rect, _selected, _dragging| {
                let device_id = entry.device_id;
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
                // 右クリックメニューが開いている frame の抑止は `chain_row_edit` が担う。
                if entry.shows_button() {
                    let label = if entry.shows_param_panel() { "Par" } else { "GUI" };
                    ui.button_at(
                        ("inspector_row_gui", idx),
                        label,
                        Rect { x: gui_x, y: row_rect.y + 2.0, w: btn_gui_w, h: row_rect.h - 4.0 },
                        move || chain_row_edit(ChainRowButton::ToggleGui, device_id, popup_open),
                    );
                }
                let xb_x = row_rect.x + row_rect.w - btn_x_w;
                ui.button_at(
                    ("inspector_row_remove", idx),
                    "x",
                    Rect { x: xb_x, y: row_rect.y + 2.0, w: btn_x_w, h: row_rect.h - 4.0 },
                    move || chain_row_edit(ChainRowButton::Remove, device_id, popup_open),
                );
            },
            |i| {
                if chain.get(i).map(|e| e.device_id) == open_dev {
                    panel_h
                } else {
                    0.0
                }
            },
            |ui, _exp_i, exp_rect| {
                let measured =
                    (device_panel::draw_device_panel(app, ui, area, pad, exp_rect)
                        - exp_rect.y)
                        .max(0.0);
                // 展開部の実消費高を測って次フレームの row_extra_h に使う (lag-by-one)。
                if (app.ui_ephemeral.inspector_device_panel_h - measured).abs() > 0.5 {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        app.ui_ephemeral.inspector_device_panel_h = measured;
                    }));
                }
            },
        );

        // r.md #71 (プラグインのコピー / 移動): 行 click = 選択 (無修飾 / Ctrl / Shift)。
        // 修飾キーは widget が **press フレームで捕まえた値** (`clicked_modifiers`) を
        // 使う — release フレームの生読みは ModifiersChanged 先行 race で Ctrl+click が
        // Single に化ける。
        if !popup_open
            && let Some(i) = resp.clicked
            && let Some(e) = chain.get(i)
        {
            let device_id = e.device_id;
            let m = resp.clicked_modifiers;
            let modifier = SelectModifier::from_modifiers(m.shift, m.ctrl);
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::SelectDevice { device_id, modifier });
            }));
        }
        // リスト外へ出た = トラック跨ぎの運搬を始める。運ぶ対象は「掴んだ行が選択に
        // 含まれていれば選択全体、含まれていなければその行だけ」(トラックヘッダの
        // 右クリックメニューと同じ規則)。
        if let Some(i) = resp.dragged_out
            && let Some(e) = chain.get(i)
        {
            let device_ids = carried_device_ids(app, &chain, e.device_id);
            ui.begin_drag(
                crate::app_types::DEVICE_DRAG_KIND,
                DeviceDragPayload {
                    device_ids,
                    source_track: cursor_tid.unwrap_or(common::model::MASTER_TRACK_ID),
                },
            );
        }
        // 落とした = 挿入位置を確定して移動 / コピー。既定は移動、Ctrl でコピー。
        // 修飾キーは payload が持っている「押されていた最後のフレーム」の値。
        if let Some(at) = resp.external_dropped_at
            && let Some(copy) = ui.drag_modifiers().map(|m| m.ctrl)
            && let Some(dest_track) = cursor_tid
            && let Some(pl) =
                ui.take_drag_payload::<DeviceDragPayload>(crate::app_types::DEVICE_DRAG_KIND)
        {
            ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::RelocateDevices(RelocateDevices {
                    device_ids: pl.device_ids.clone(),
                    dest_track,
                    dest_index: at as u32,
                    copy,
                }));
            }));
        }
        // 右クリックメニューは **widget の外 (caller 側)** で重ねる (arrangement の
        // `track_header_rects` / `clip_rects` と同じ idiom)。
        for (i, row_rect) in &resp.row_rects {
            let Some(e) = chain.get(*i) else { continue };
            let device_id = e.device_id;
            let dest_track = cursor_tid;
            ui.context_menu_for(
                *row_rect,
                &["コピー", "切り取り", "貼り付け", "複製", "削除"],
                move |idx, ui| {
                    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
                        apply_chain_menu_action(app, idx, device_id, dest_track);
                    }));
                },
            );
        }

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
