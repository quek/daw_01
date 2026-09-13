//! トラック inspector (左サイドバー):
//! - 選択トラック名
//! - 「Chain」見出し
//! - MIDI FX → Instrument → FX のリスト (各行に GUI / × ボタン、drag&drop で reorder)
//! - + Instrument / + Effect / + MIDI FX ボタン

/// 選択クリップの「Audio Event」 セクション。
mod audio_event_section;
mod chain_list;
/// chain list の chain 行 / 操作行と、 Parallel ヘッダ行と共有する disclosure / 改名欄。
mod chain_row;
mod chain_sections;
mod device_panel;
/// 選択クリップの「Image Event」 セクション。
mod image_event_section;
/// r.md #87: 選択中のランチャーセルのローンチ設定 (Q7 / 計画書 §3.4)。
mod launch_section;
/// 口パク出力先トラックの「口形状 → 画像」 セクション。
mod mouth_map_section;
/// r.md #129: 内蔵 device の Par パネル (格子の寸法 `layout` はテストが座標を求めるのに使う)。
pub mod native_panel;
/// r.md #129: chain list の内蔵 device の行と展開、 master の末尾 (Post-Fader / Limiter)。
mod native_row;
mod parallel_header;
/// chain list の plugin 行と、 その直下の展開 (SC パネル / param パネル)。
mod plugin_row;
/// chain list の行の右クリックメニュー (項目は型で持つ)。
mod row_menu;
mod modulation_rack;

use daw_ui_core::{Edit, ScrubableNumberFormat, ScrubableNumberStyle, ToggleButtonStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{
    text_num_to_builtin, AppData, AppEvent, ClipKey, ColorPickerTarget, DiscreteClipEdit,
    FadeEdgeKind, InspectorScrubField, ScrubGesture, TalkParamKind, TextNumField,
};
use crate::view::modulation::{self as mod_widget, build_mod, scrub_field_mod, ModBuild};
use crate::view::track_color;
use common::model::{AutomationTarget, FadeCurve, TextAlign};

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

/// `scrubable_number` の drag / text 編集 stroke を **undo 1 step に bracket** する
/// **唯一の実装**。実体は [`crate::view::scrub_gesture::push`] 1 本で、ここは
/// [`InspectorScrubField`] を所有者へ包むだけ。
///
/// これが無いと `Song` を per-frame に書く数値欄は **1 ドラッグで数十 undo step** を
/// 積み、`UNDO_LIMIT` (200) を溢れさせて**それ以前の実編集履歴を捨てる**。
/// inspector / 変調ラック / 「ローンチ」 セクションが同じ 1 本を通るので、
/// 欄を足すたびに bracket を書き写して 1 か所だけ忘れる、が起きない。
///
/// `key` は **同時に描かれる別の欄と必ず違う値**にすること — 同じ key を 2 つの欄が
/// 使うと、片方の drag 中にもう片方が「非 active」として閉じ、bracket が
/// 1 フレームで切れる。
///
/// **`active` が false のフレームも呼ぶこと** — 呼ばないと「欄が消えた」と
/// 判定されて gesture が閉じる ([`crate::view::scrub_gesture`] の寿命規約)。
pub(crate) fn push_scrub_bracket(
    ui: &mut Ui<'_, AppData>,
    app: &AppData,
    key: InspectorScrubField,
    active: bool,
) {
    crate::view::scrub_gesture::push(ui, app, ScrubGesture::Inspector(key), active);
}

/// audio / image / text inspector
/// の数値 field を 1 行ぶん描く共通 helper。 `ui.scrubable_number_at` を呼び、
/// on_change で `make_event(v)` が返す `AppEvent` を全 event に broadcast、
/// drag / text 編集の一連を [`push_scrub_bracket`] で undo 1 step に bracket する。
/// `scrub_key` はその bracket の**所有者を名指しする鍵**
/// ([`ScrubGesture::Inspector`](crate::state::ScrubGesture) の中身)。
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
    make_event: impl Fn(ClipKey, f64) -> AppEvent + Clone + Send + Sync + 'static,
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
    let mod_build = mod_spec.as_ref().and_then(|(target, domain)| {
        let owner = crate::view::native_device::ParamOwner::resolve(app.cur.song_doc.song(), cursor_track)?;
        Some(build_mod(app, target.clone(), base, *domain, owner))
    });
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
    push_scrub_bracket(ui, app, scrub_key, resp.dragging || resp.editing_text);
    // modulation depth ドラッグの falling edge で host 再同期 (自コントロールの
    // target を key に、他コントロールと干渉せず drag-end で 1 回だけ recompile)。
    if let Some((target, _)) = &mod_spec {
        mod_widget::push_mod_depth_bracket(
            ui,
            app,
            crate::app::ParamSurface::Rack,
            cursor_track,
            target,
            resp.mod_dragging,
        );
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
    if app.cur.selection.selected_track_ids.len() <= 1
        && let Some(idx) = app.cursor_track_index()
        && let Some(track) = app.cur.song_doc.song().tracks.get(idx)
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
    // (gui_01 popup.rs)。 各セクションは `(app, ui, area, pad, y) -> f32` 契約の関数で、
    // この並び順がそのまま画面の上下順。
    let body_top = y;
    let param_h = (area.y + area.h - body_top).max(0.0);
    let content_h = app.cur.peph.inspector_body_h.max(1.0);
    let param_vp = Rect { x: area.x, y: body_top, w: area.w, h: param_h };
    let measured_body_h = std::cell::Cell::new(0.0_f32);
    ui.scroll_area("inspector_body", param_vp, (param_vp.w, content_h), |ui, scroll_off| {
        let mut y = body_top - scroll_off.1;

        // ---- r.md #87: ランチャーのセルのローンチ設定 (Q7) -------------------
        // 選択にセルが 1 つも無ければ何も描かない (= 通常のクリップ編集時は
        // 従来と 1px も変わらない)。 先頭に置くのは「セルを選んだらまずここを見る」
        // 導線のため (`chain_sections` と同じ `(app, ui, area, pad, y) -> f32` 契約)。
        y = launch_section::draw_launch_section(app, ui, area, pad, y);

        // ---- Audio Event section (Phase 2 PR1 + PR2 + PR3) ------------------
        y = audio_event_section::draw_audio_event_section(app, ui, area, pad, y);

        // ---- Image Event section (`docs/plan_image_overlay.md` §4 P4) ------
        y = image_event_section::draw_image_event_section(app, ui, area, pad, y);

        // ---- Plugin chain (r.md #110: Parallel 対応の縦回転 Live 型 chain list) ----
        y = chain_list::draw_chain_list(app, ui, area, pad, y);

        // r.md #37: チェーン直下に 「+ Plugin」 → Parallel Out → Sidechain を top-down で
        // 並べる (旧: inspector 下端に pinned)。 「このチェーンの末尾に足す」 「このチェーンの
        // デバイスの配線」 が読み順で自明になる。 各 fn は modulation_rack と同じ
        // `(app, ui, area, pad, y) -> f32` contract。
        // ロード失敗は他の配線セクションより上 (= チェーンに最も近い位置)。
        y = chain_sections::draw_failed_load_section(app, ui, area, pad, y);
        y = chain_sections::draw_parallel_out_section(app, ui, area, pad, y);

        // ---- 口パク mapping (口形状 → 画像) -------------------------------
        y = mouth_map_section::draw_mouth_map_section(app, ui, area, pad, y);

        // Modulation rack を scroll viewport 末尾に置く (top-down フロー、
        // 展開で 1 個を大きなグラフィカルエディタに、ソース数無制限・スクロール)。
        y += 10.0;
        y = modulation_rack::draw_modulation_rack(app, ui, area, pad, y);

        measured_body_h.set(y - (body_top - scroll_off.1));
    });
    // 測定した param 実高さを次フレーム用に保存 (変化時のみ edit を積む)。
    let measured = measured_body_h.get();
    if (app.cur.peph.inspector_body_h - measured).abs() > 0.5 {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cur.peph.inspector_body_h = measured;
        }));
    }
}
