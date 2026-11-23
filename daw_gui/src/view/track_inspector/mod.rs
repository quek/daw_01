//! トラックインスペクタ (画面左の縦長のカラム、r.md #158 `docs/plan_rmd_158_inspector.md`):
//! - タイトル (スクロールしない 2 行): 選んでいるもの (クリップ → 列 → トラック) と色見本 (`title.rs`)
//! - その下は 1 本の縦スクロールに **帯** を積む (`band.rs`)。上から 選択 (クリップ / セル) → 列 →
//!   入力 → Rack → 口パク → モジュレーション。見出しのクリックで開閉 (種類ごとに共通、ViewState に保存)
//! - 右端の 5px で幅を変える (360〜640)。`I` / View メニューで丸ごと隠す (app_config に保存)
//! - 帯の中の欄は「ラベル左・値右」(`form.rs`、寸法は `layout.rs`)。Rack の行の直下に開く Par / SC / 出力の
//!   欄も、値の列は帯の欄と同じ x (`form::cols_nested`)。見出し・欄名の文言は `vocab.rs`

/// r.md #158: plugin 行の `Out▾` で直下に開く出力 (パラアウト) のパネル。
mod aux_out_panel;
/// r.md #158: 帯 (見出し / 詳細 / 小見出し)。
mod band;
mod chain_list;
/// chain list の chain 行 / 操作行と、 Parallel ヘッダ行と共有する disclosure / 改名欄。
mod chain_row;
/// r.md #158: 選択の帯 (クリップ / セル) と列の帯の中身 (音声 / 画像 / 歌声 / 読み上げ / 字幕 / ローンチ)。
mod clip_band;
mod device_panel;
/// r.md #158: 欄 (ラベル左・値右)。
mod form;
/// r.md #156: トラックの「入力」と「モニター」。
mod input_section;
/// r.md #158: インスペクタの寸法 (テストも読む)。
pub mod layout;
/// r.md #151: Rack の行の Mix ノブ (Alt+クリックで Δ)。
mod mix_knob;
/// 口パク出力先トラックの「口形状 → 画像」 セクション。
mod mouth_map_section;
/// r.md #129: 内蔵 device の Par パネル (格子の寸法 `layout` はテストが座標を求めるのに使う)。
pub mod native_panel;
/// r.md #129: chain list の内蔵 device の行と展開、 master の末尾 (フェーダー後 / Limiter)。
mod native_row;
mod parallel_header;
/// chain list の plugin 行と、 その直下の展開 (SC パネル / param パネル)。
mod plugin_row;
/// chain list の行の右クリックメニュー (項目は型で持つ)。
mod row_menu;
mod modulation_rack;
/// r.md #158: タイトル (選んでいるもの + 色見本)。
mod title;
/// r.md #158: 見出し・欄名の文言 (テストも読む)。
pub mod vocab;

use daw_ui_core::{Edit, Orientation, ScrubableNumberFormat, ScrubableNumberStyle, ToggleButtonStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ClipKey, InspectorScrubField, ScrubGesture};
use crate::event_inspector::InspectorEvent;
use crate::handler::bypass_target::BypassTarget;
use crate::state::ModRackHover;
use crate::state::inspector_view::{InspectorBand, InspectorScrollFloor};
use crate::view::modulation::{self as mod_widget, build_mod, scrub_field_mod, ModBuild};
use common::model::AutomationTarget;

use band::BandHead;
use layout::{BAND_ADD_W, BAND_GAP, PAD, RESIZE_HANDLE_W, TITLE_H};

// r.md #48: style は const にできない (const はランタイムのパレットを読めず、テーマを
// 切り替えても古い色のまま残る)。 いずれも `Theme` を受け取る fn にして、 ui-core の
// `from_palette` を base にした差分だけをここで宣言する。

/// インスペクタのトグル (クリップの帯の ミュート / 逆再生 / 反転 / ループ…、Rack の行の ⌨ / SC▾ / Out▾、
/// chain の M / S、MIDI 割り当て) 共通の style。 mixer_strips の STYLE_MUTE / STYLE_SOLO とほぼ同じだが、
/// inspector 側に独立して定義 (mixer の private style を import するより、 同 widget 並びの一覧性を
/// 優先)。 hint band は無し (= 単純トグル) にして、 文字 + ON/OFF 色だけで 状態を伝える。
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

/// インスペクタが占める幅 (隠しているときは 0)。`root.rs` のレイアウト計算が使う
/// (`master_panel::panel_width` と同じ形)。
#[must_use]
pub fn panel_width(app: &AppData) -> f32 {
    if app.ui_prefs.inspector_open {
        crate::handler::inspector_view::clamp_inspector_w(app.ui_prefs.inspector_w)
    } else {
        0.0
    }
}

/// インスペクタを描き、Q の宛先 (Rack の行 / モジュレーター) を **ここ 1 か所で** 公開する。
///
/// 宛先を描画の途中で書くと、帯を畳んだ / インスペクタを隠したフレームで誰も消さず、見えない行が
/// `Q` で切り替わる (マスターパネルの §18-AB と同じ)。描画は宛先を返すだけにして、どの経路で抜けても
/// 毎フレーム必ずここを通す。
pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let hover = draw_panel(app, ui, area);
    if app.cur.peph.inspector_hovered_row != hover.row {
        let row = hover.row;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.cur.peph.inspector_hovered_row = row));
    }
    if app.cur.peph.inspector_hovered_mod != hover.module {
        let module = hover.module;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| app.cur.peph.inspector_hovered_mod = module));
    }
}

/// 帯の中身が返す Q の宛先。描かれなかった帯は `None` のまま。
#[derive(Default)]
struct InspectorHover {
    row: Option<BypassTarget>,
    module: Option<ModRackHover>,
}

fn draw_panel(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) -> InspectorHover {
    if !app.ui_prefs.inspector_open || area.w < 1.0 {
        // 隠したら、開いていた dropdown の一覧は持ち主が描かれないフレームの末尾で daw-ui が閉じる
        // (`Ui::close_popups_without_owner`)。右クリックメニューも同じ。
        return InspectorHover::default();
    }
    let p = &app.theme.core;
    ui.panel("inspector_bg", area, p.panel, 0.0);

    // ---- 右端の幅のドラッグ帯 (scroll viewport の外の列。縦スクロールバーと掴み合わない) ----
    let handle = Rect { x: area.x + area.w - RESIZE_HANDLE_W, y: area.y, w: RESIZE_HANDLE_W, h: area.h };
    if let Some(drag) = ui.resize_handle("inspector_resize", handle, Orientation::Horizontal) {
        // 左端固定なので、右へ動かすほど広い。縁 = 帯の右端 (帯の中の掴んだ位置を保つので跳ばない)。
        let (w, commit) = (drag.handle_origin.0 + RESIZE_HANDLE_W - area.x, drag.released);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Inspector(InspectorEvent::SetWidth { w, commit }));
        }));
    }
    ui.panel("inspector_edge", Rect { x: area.x + area.w - 1.0, y: area.y, w: 1.0, h: area.h }, p.border, 0.0);
    let content = Rect { w: (area.w - RESIZE_HANDLE_W).max(1.0), ..area };

    let title = app.inspector_title();
    title::draw_title(app, ui, Rect { h: TITLE_H, ..content }, &title);

    // ---- 帯を縦スクロール領域に積む ------------------------------------------
    // r.md #37: title 下〜area 下端の **全部** が scroll viewport。 縦位置は「1 本の y カーソル」 だけが
    // 決め、帯の並び順がそのまま画面の上下順 (daw_gui/CLAUDE.md「UI の配置・操作性」)。
    // 本文の実高さは前フレーム測定値 (`inspector_body_h`、 lag-by-one)。帯 / Par を畳んで本文が縮んでも、
    // 見えている下端は床 (`InspectorScrollFloor`) で保つ — 末尾までスクロールした状態で畳んでも、
    // scroll_area が offset を詰めて押した見出しが下へずれることが無い。
    let body_top = content.y + TITLE_H;
    let vp = Rect { x: content.x, y: body_top, w: content.w, h: (area.y + area.h - body_top).max(0.0) };
    let subject = app.inspector_subject();
    let prev_floor = app.cur.peph.inspector_scroll_floor.as_ref();
    let floor = InspectorScrollFloor::floor_for(prev_floor, &subject).unwrap_or(0.0);
    let content_h = app.cur.peph.inspector_body_h.max(floor).max(1.0);
    let measured_body_h = std::cell::Cell::new(0.0_f32);
    let mut hover = InspectorHover::default();
    let offset = ui.scroll_area("inspector_body", vp, (vp.w, content_h), |ui, scroll_off| {
        let top = body_top - scroll_off.1;
        let y = draw_bands(app, ui, content, top + BAND_GAP, &title.main, &mut hover);
        measured_body_h.set(y - top);
    });
    // 測定した本文の実高さと床を次フレーム用に保存 (変化時のみ edit を積む)。
    let measured = measured_body_h.get();
    if (app.cur.peph.inspector_body_h - measured).abs() > 0.5 {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cur.peph.inspector_body_h = measured;
        }));
    }
    let next_floor = InspectorScrollFloor::next(prev_floor, subject, measured, offset.1 + vp.h);
    if prev_floor != Some(&next_floor) {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.cur.peph.inspector_scroll_floor = Some(next_floor);
        }));
    }
    hover
}

/// 帯を上から積む (r.md #158 仕様 2): 選択 (クリップ / セル) → 列 → 入力 → Rack → 口パク →
/// モジュレーション。中身は各セクションの `(app, ui, area, pad, y) -> y` 契約の関数 (Rack とモジュレーションは
/// Q の宛先も返す)。帯の対象がタイトルの 1 行目 (`title_main`) と同じ語なら省く — 1 本のトラックを選んだだけで
/// トラック名が 4 回並ばない (複数選択の「選択中の N トラック」/「… のみ」は 1 行目と違うので残る)。
fn draw_bands(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    area: Rect,
    mut y: f32,
    title_main: &str,
    hover: &mut InspectorHover,
) -> f32 {
    let shown = |target: &str| -> String { if target == title_main { String::new() } else { target.to_string() } };
    // ---- 選んでいるクリップ / セル ----
    if let Some((kind, target)) = app.inspector_clip_band() {
        let target = shown(&target);
        let head = BandHead::new(InspectorBand::Clip(kind), vocab::clip_band_title(kind), &target);
        y = band::band(app, ui, area, y, &head, |ui, y| clip_band::draw_clip_band_body(app, ui, area, PAD, y))
            + BAND_GAP;
    }
    // ---- 選んでいる列 (シーン) ----
    if let Some(target) = app.inspector_scene_band_target() {
        let head = BandHead::new(InspectorBand::Scene, vocab::BAND_SCENE, &target);
        y = band::band(app, ui, area, y, &head, |ui, y| clip_band::draw_scene_launch(app, ui, area, PAD, y))
            + BAND_GAP;
    }
    // ---- 入力 (選択中の全トラックに効く。録れる入力が 1 つも無いトラック (master / グループのバス /
    // 画像・映像のトラック) だけなら出さない — 仕様 14) ----
    if input_section::input_band_shown(app) {
        let target = shown(&app.inspector_track_band_target(true));
        let head = BandHead::new(InspectorBand::Input, vocab::BAND_INPUT, &target);
        y = band::band(app, ui, area, y, &head, |ui, y| input_section::draw_input_section(app, ui, area, PAD, y))
            + BAND_GAP;
    }
    let Some(cursor) = app.cursor_track_id() else {
        return y;
    };
    // ---- Rack (カーソルのトラック 1 本) ----
    let cursor_only = shown(&app.inspector_track_band_target(false));
    let target = if app.cur.song_doc.song().track_effectively_enabled(cursor) {
        cursor_only.clone()
    } else {
        let name = app.inspector_track_band_target(false);
        format!("{name}{}{}", vocab::SEP, vocab::DISABLED)
    };
    // 読み込みに失敗した plugin の数は見出しに出す (畳んだ Rack / Parallel / 選んでいない chain の中でも
    // 気付ける。理由と [再読込] は各 plugin 行の中、仕様 2: トリガの近く)。
    let failed = app.failed_loads_in_track(cursor);
    let warn = if failed > 0 { vocab::failed_loads(failed) } else { String::new() };
    let head = BandHead { warn: &warn, ..BandHead::new(InspectorBand::Rack, vocab::BAND_RACK, &target) };
    y = band::band(app, ui, area, y, &head, |ui, y| {
        let (y, row) = chain_list::draw_chain_list(app, ui, area, PAD, y);
        hover.row = row;
        y
    }) + BAND_GAP;
    // ---- 口パク (このトラックを口パク出力先にしている歌声トラックがあるとき) ----
    if mouth_map_section::mouth_map_shown(app) {
        let head = BandHead::new(InspectorBand::MouthMap, vocab::BAND_MOUTH, &cursor_only);
        y = band::band(app, ui, area, y, &head, |ui, y| mouth_map_section::draw_mouth_map_section(app, ui, area, PAD, y))
            + BAND_GAP;
    }
    // ---- モジュレーション (見出しの右端に追加メニュー。畳んでいても見える) ----
    let head = BandHead {
        actions_w: BAND_ADD_W,
        ..BandHead::new(InspectorBand::Modulation, vocab::BAND_MODULATION, &cursor_only)
    };
    let out = band::head(app, ui, area, y, &head);
    modulation_rack::draw_add_source_menu(ui, out.actions);
    let body_end = if out.open {
        let (y, module) = modulation_rack::draw_modulation_rack(app, ui, area, PAD, out.body_y);
        hover.module = module;
        y
    } else {
        out.body_y
    };
    band::end(&out, body_end) + BAND_GAP
}
