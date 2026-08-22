//! Renoise 風 mixer strip。draw(...) を呼ぶと指定 area 内に N 本のチャンネル
//! ストリップが横並びで描画される。
//!
//! 各 strip:
//!   - トラック名
//!   - M (mute) / S (solo) toggle
//!   - Pan knob + その右の数値欄 (drag / 打ち込みで編集可)
//!   - Volume fader (縦) + L/R peak meter

use common::model::{AutomationTarget, SendMode, TrackBuiltinParam};
use daw_ui_core::{
    Edit, KnobStyle, LevelMeterStyle, MeterBallistic, MeterScale, ScrubableNumberStyle,
    ToggleButtonStyle, Ui,
};

use common::automation::{norm_to_plain, plain_to_norm};

use crate::automation_value::automation_value_display;
use crate::view::modulation::{build_mod, push_mod_drag_resync};
use crate::view::param_gesture::push_param_gesture_edges;
use crate::view::track_color;
use daw_ui_renderer::{Color, Rect, RectCommand};
use crate::theme::Theme;

use crate::app::{AppData, AppEvent, ModControlDomain};
use crate::widgets::select_modifier::SelectModifier;

const STRIP_WIDTH: f32 = 80.0;
const STRIP_GAP: f32 = 4.0;
const TOP_LABEL_H: f32 = 18.0;
/// r.md #13: strip 上端の「トラック名バンド」の高さ (px)。 この帯を押すと
/// トラックを選択する (M/S トグルや fader/knob より上なので操作と干渉しない)。
/// top pad(6) + 名前(TOP_LABEL_H=18) = 次の M/S トグル行の直前まで。
const NAME_BAND_H: f32 = 24.0;
/// group strip の名前バンド左端にある折り畳み disclosure (▶/▼) が占める幅
/// (= draw_strip の `pad(6) + disc_w(14) + gap(2)`)。 選択の press 帯はこの分
/// だけ右にずらして、 disclosure クリック (= 折り畳みトグル) が選択を巻き込まない
/// ようにする (code review: group strip で disclosure が NAME_BAND 内に重なる)。
const DISCLOSURE_ZONE_W: f32 = 22.0;
const TOGGLE_H: f32 = 22.0;
const KNOB_SIZE: f32 = 32.0;
/// Pan ノブ **右** の数値欄 (`"L50"` / `"C"` / `"R100"`) の font size (px)。
/// send 行のラベル (10px) と同格の副次情報サイズ。
const PAN_READOUT_FONT: f32 = 10.0;
/// Pan 数値欄の高さ (px)。 ノブ (32px) と縦センタで揃える。 font 10 の行 (12px) に
/// 上下 2px の余白を足した最小の入力欄。
const PAN_READOUT_H: f32 = 16.0;
/// Pan ノブと数値欄の間隔 (px)。
const PAN_READOUT_GAP: f32 = 4.0;
/// Pan 数値欄の幅 (px)。 最長表記 `"L100"` が `ScrubableNumberStyle::pad_x` の左右余白
/// 込みで省略なしに収まる幅 (回帰テスト `pan_readout_fits_field_width` で固定)。
/// **値によって幅を変えない**: 幅を実測に追随させるとノブが値ごとに左右へ動く。
const PAN_READOUT_W: f32 = 30.0;
/// pan 行 (= `[ノブ][gap][数値欄]`) の合計幅 (px)。 この 1 行を strip 中央に寄せる。
const PAN_ROW_W: f32 = KNOB_SIZE + PAN_READOUT_GAP + PAN_READOUT_W;
/// Pan 数値欄の内側左右余白 (px)。 `ScrubableNumberStyle::pad_x` の既定 (広い欄向け) より
/// 詰める理由は無いが、 `PAN_READOUT_W` の根拠になるのでここで名前を付けて共有する。
const PAN_READOUT_PAD_X: f32 = 4.0;
/// Pan 数値欄の scrub 感度 (units_per_pixel、 pan は plain -1..=1)。 inspector の
/// Pan 欄 (`track_inspector::scrub_style`) と同値 = 同じ param は同じ手応え。
const PAN_READOUT_SENSITIVITY: f32 = 0.004;
const FADER_W: f32 = 18.0;
const METER_GAP: f32 = 2.0;
/// scale 付きステレオメーターの box 幅 (px)。 widget が内部で
/// `[tick ~6 | L バー | R バー | 数字 ~18]` に配分する。 全 ch に dB 目盛りを
/// 付けつつ現 80px ストリップ (fader 18 と並べて) に収まる幅。 数字 "-60" が
/// 読める最小幅で、 バーは ~5px ずつ残る。
const METER_SCALE_W: f32 = 35.0;
/// Sends セクション 1 行の高さ (= 宛先名 + × の header 行 + knob / Pre-Post / M の
/// controls 行)。 2 行構成にして Pre/Post トグルに "Post" が省略されない幅を確保する。
const SEND_ROW_H: f32 = 33.0;
/// Sends セクション内の send 用ミニ knob のサイズ。
const SEND_KNOB_SIZE: f32 = 18.0;
/// Sends セクションの内側左右パディング。
const SEND_PAD: f32 = 6.0;
/// send 行内の小ボタン同士の隙間。
const SEND_BTN_GAP: f32 = 3.0;
/// per-send mute (M) ボタンの幅 (1 文字なので固定の狭幅)。
const SEND_MUTE_BTN_W: f32 = 14.0;
/// × (remove send) ボタンの幅 (header 行右上)。 controls 行の M の真上に揃う。
const SEND_CLOSE_BTN_W: f32 = 14.0;
/// Pre/Post トグルの font_size (controls 行)。 "Post" (最長ラベル) が
/// `send_prepost_width` に省略なしで収まる前提 (回帰テストで固定)。
const SEND_PREPOST_FONT: f32 = 10.0;
/// 「＋ Send」 ボタンの高さ。
const ADD_SEND_H: f32 = 16.0;
/// returns 帯と通常 strip 帯を分ける divider の幅。
const RETURN_DIVIDER_W: f32 = 2.0;
/// track 色ストライプの幅 (px)。 strip 左端に縦に描く。 arrangement header の
/// `ArrangementStyle.track_color_strip_w` (gui_01 default 4.0) と揃える。
const COLOR_STRIP_W: f32 = 4.0;
/// 「＋ Return」 ボタンの高さ (returns 帯の上端に置く)。
const ADD_RETURN_H: f32 = 22.0;

/// mixer のトグル (M / S / send の Pre-Post / per-send mute) の共通ベース。
/// ui-core 既定 (`ToggleButtonStyle::from_palette`) から、 mixer の詰まった
/// 80px ストリップ向けに radius / font を詰め、 ON を非意味的な
/// `control_active` に戻す (意味色を持つトグルは各 style 関数が上書きする)。
fn toggle_button_base(theme: &Theme) -> ToggleButtonStyle {
    let p = &theme.core;
    ToggleButtonStyle {
        on_color: p.control_active,
        radius: 4.0,
        font_size: 12.0,
        ..ToggleButtonStyle::from_palette(p)
    }
}

/// Mute トグル。 ON 背景 = 業界標準の赤 (gui_01 #052 で hint_band 廃止 →
/// ON は背景色のみで表現する idiom に統一)。
fn style_mute(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle { on_color: theme.daw.record, ..toggle_button_base(theme) }
}

/// Solo トグル。 ON 背景 = 業界標準の黄。 黄は明るいので文字は極性固定インクを
/// `ink_for` で選ぶ (テーマ従属の `text` のままだと、 ライトテーマで暗い黄の上に
/// 暗い文字が乗って読めなくなる)。
fn style_solo(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        on_color: theme.daw.solo,
        on_text_color: Some(theme.core.ink_for(theme.daw.solo)),
        ..toggle_button_base(theme)
    }
}

/// send 内 per-send mute (`enabled`)。 mute と同じ赤系 on_color、 ただし
/// `enabled == true` (= 鳴っている) が「OFF 表示」、 `enabled == false`
/// (= ミュート) が「ON 表示 (赤)」 になるよう、 描画側で `!enabled` を渡す。
fn style_send_mute(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle { on_color: theme.daw.record, font_size: 10.0, ..toggle_button_base(theme) }
}

/// Pre/Post 切替トグル。 PreFader のとき on_color (accent) で強調する。
fn style_send_prepost(theme: &Theme) -> ToggleButtonStyle {
    ToggleButtonStyle {
        on_color: theme.core.accent,
        font_size: SEND_PREPOST_FONT,
        ..toggle_button_base(theme)
    }
}

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    let p = &app.theme.core;
    // mixer ビューの最下層 backdrop (= strip が浮く床)。strip 本体 = panel より
    // 一段沈める必要があるので view base の window_bg を使う。
    ui.panel("mixer_bg", area, p.window_bg, 0.0);

    // S キーで「マウス直下のストリップ」を solo するため、 各 strip の
    // rect にポインタ当たり判定をして hover track を求める (arrangement の
    // `arrange_hovered_track` と同 idiom)。 layout を持つこの draw が唯一の算出点
    // (SSoT)。 master strip は solo を持たないので対象外 (= None のまま)。
    let pointer = ui.pointer();
    let ptr = pointer.pos;
    let mut hovered_strip: Option<u32> = None;

    let inner_pad = 8.0;
    let strip_y = area.y + inner_pad;
    let strip_h = area.h - inner_pad * 2.0;
    let pitch = STRIP_WIDTH + STRIP_GAP;

    // 派生集合に基づいて mix を「通常 track」 と「リターン (= 他 track の
    // send 宛先)」 に分割する。 リターンは右側に固めて divider + 緑 tint で
    // 別物として見せる (Ableton の return track 列メタファ)。 normal / return
    // 両方とも `track_mix` から来た `is_return` フラグで判定 (派生値、 SSOT)。
    let mix = app.track_mix();
    // A track can be both a group (has children) and a return (has incoming
    // sends) — the unified model allows it and the audio graph handles it.
    // When it is both, keep it in the *normal* band so it renders with its
    // children (group hierarchy intact); only a **pure** return (no
    // children) goes to the returns band. Otherwise the parent strip would
    // be yanked right while its children stayed left, severing the tree.
    let (returns, normals): (Vec<_>, Vec<_>) =
        mix.iter().partition(|e| e.is_return && !e.is_group);

    // 折り畳まれた group の配下 strip は隠す (arrangement と同じ
    // `collapsed_groups` を参照 = SSoT 共有)。x レイアウト / content_w が
    // filter 後の index に揃うよう、 並べる前に除外する。group strip 自身は
    // (自分の祖先に collapsed が無い限り) 残り、 disclosure ▶/▼ を出す。
    let normals: Vec<_> = normals
        .into_iter()
        .filter(|e| !app.is_hidden_under_collapsed_group(e.track_id))
        .collect();

    // r.md #13: strip の名前バンドを押すとトラックを選択する。 arrangement と同じ
    // `selection.selected_track_ids` (SSoT) を読み書きするので、 mixer ↔ arrangement
    // の選択は自動で双方向連動する。 range-select (Shift) の並びは mixer の可視 strip
    // 順 (normals 左→右 → returns 左→右、 master は除く)。 press 時に modifier を読む
    // ので release-frame の modifier race が無い (arrangement の press_modifiers と同狙い)。
    let visible_order: Vec<u32> = normals.iter().chain(returns.iter()).map(|e| e.track_id).collect();
    let select_press = std::cell::Cell::new(None::<u32>);

    // ----- 右端から固定配置: returns 帯 → 「＋ Return」 -----
    // r.md #50: MASTER ストリップは画面右端の常駐マスターパネル
    // (`view::master_panel`) へ移設したので、ここには居ない。同じフェーダーを
    // 2 か所で編集できる状態を作らないため、Mixer 側からは完全に消す。
    let returns_right = area.x + area.w - inner_pad;

    // returns 帯: 右端に returns.len() 本 + その左に「＋ Return」 ボタン。
    // returns 0 本でも「＋ Return」 ボタンは出す。
    let returns_w = (returns.len() as f32) * pitch;
    // 「＋ Return」 ボタン用に固定列を 1 本分確保する。
    let add_return_col_w = STRIP_WIDTH;
    let returns_band_x = returns_right - returns_w;
    let add_return_x = returns_band_x - STRIP_GAP - add_return_col_w;

    // 通常 track strips: 左端 inner_pad から returns 帯 / Add Return 列の手前まで
    // scroll_area で横スクロール。
    let scroll_x = area.x + inner_pad;
    let scroll_right = add_return_x - inner_pad;
    let scroll_w = (scroll_right - scroll_x).max(0.0);
    let scroll_rect = Rect { x: scroll_x, y: strip_y, w: scroll_w, h: strip_h };
    let content_w = (normals.len() as f32) * pitch;
    ui.scroll_area("mixer_strips", scroll_rect, (content_w, strip_h), |ui, offset| {
        for (i, entry) in normals.iter().enumerate() {
            let x = scroll_x - offset.0 + (i as f32) * pitch;
            if x + STRIP_WIDTH < scroll_x || x > scroll_x + scroll_w {
                continue;
            }
            let strip_rect = Rect { x, y: strip_y, w: STRIP_WIDTH, h: strip_h };
            // scroll_rect で clip して当たり判定 (はみ出した strip の不可視部分を除外)。
            if let Some((px, py)) = ptr
                && scroll_rect.contains(px, py)
                && strip_rect.contains(px, py)
            {
                hovered_strip = Some(entry.track_id);
            }
            // 名前バンド上の press でトラック選択 (r.md #13)。 group strip は左端の
            // 折り畳み disclosure を避けて帯を右にずらす (disclosure クリックが選択を
            // 巻き込まないように)。
            let band_x0 = if entry.is_group { x + DISCLOSURE_ZONE_W } else { x };
            let name_band = Rect { x: band_x0, y: strip_y, w: x + STRIP_WIDTH - band_x0, h: NAME_BAND_H };
            if pointer.primary_just_pressed
                && let Some((px, py)) = ptr
                && scroll_rect.contains(px, py)
                && name_band.contains(px, py)
            {
                select_press.set(Some(entry.track_id));
            }
            draw_track_strip(app, ui, entry, strip_rect);
        }
    });

    // ----- 「＋ Return」 ボタン (Add Return 列) -----
    ui.button_at(
        "mixer_add_return",
        "+ Return",
        Rect { x: add_return_x, y: strip_y, w: add_return_col_w, h: ADD_RETURN_H },
        || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::AddReturnTrack)),
    );

    // ----- returns 帯の divider + return strips -----
    if !returns.is_empty() {
        // 帯の左端に縦 divider を引いて「ここから右はリターン」 を示す。
        ui.panel(
            "mixer_return_divider",
            Rect {
                x: returns_band_x - STRIP_GAP - RETURN_DIVIDER_W,
                y: strip_y,
                w: RETURN_DIVIDER_W,
                h: strip_h,
            },
            app.theme.daw.strip_return_divider,
            0.0,
        );
        for (i, entry) in returns.iter().enumerate() {
            let x = returns_band_x + (i as f32) * pitch;
            // リターンは通常の fader / pan / mute / solo を持つ。 send 元には
            // ならない想定だが、 リターンから別リターンへ送るのも閉路防止
            // 込みで許容されている (本タスクでは Sends セクションはリターン
            // strip には出さない = 簡潔さ優先、 normal strip 経由で繋ぐ)。
            let strip_rect = Rect { x, y: strip_y, w: STRIP_WIDTH, h: strip_h };
            if let Some((px, py)) = ptr
                && strip_rect.contains(px, py)
            {
                hovered_strip = Some(entry.track_id);
            }
            // 名前バンド上の press でトラック選択 (r.md #13、 returns も実トラック)。
            let name_band = Rect { x, y: strip_y, w: STRIP_WIDTH, h: NAME_BAND_H };
            if pointer.primary_just_pressed
                && let Some((px, py)) = ptr
                && name_band.contains(px, py)
            {
                select_press.set(Some(entry.track_id));
            }
            draw_return_strip(app, ui, entry, strip_rect);
        }
    }

    // press したトラックがあれば modifier-aware に選択を更新する (r.md #13)。
    // arrangement のヘッダ選択と同じ意味論: Single / Ctrl=Toggle / Shift=Range。
    // r.md #43: 解決ロジックは `AppData::apply_select_tracks` の 1 実装に集約した
    // (旧実装はここと arrangement/run.rs に別々の範囲計算を持ち、 anchor の SSoT も
    // 割れていた — mixer は cursor = 選択末尾を基点にしていたので、 Shift+click を
    // 繰り返すと基点が歩いて範囲を伸縮できなかった)。 view が渡すのは mixer の
    // 可視 strip 順 (`visible_order`) だけ。
    if let Some(tid) = select_press.get() {
        let modifier =
            SelectModifier::from_modifiers(pointer.modifiers.shift, pointer.modifiers.ctrl);
        let order = visible_order.clone();
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.apply_select_tracks(tid, modifier, &order);
        }));
    }

    // 算出した hover track を AppData に反映 (変化時のみ Edit、
    // arrange_hovered_track と同じ diff-guard)。 dispatch_shortcuts が S キーで読む。
    if app.ui_ephemeral.mixer_hovered_track != hovered_strip {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.ui_ephemeral.mixer_hovered_track = hovered_strip;
        }));
    }
}

/// 通常 (= 非リターン) track strip。 fader / pan / mute / solo + Sends
/// セクションを描画する。
fn draw_track_strip(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    entry: &crate::app::TrackMixEntry,
    rect: Rect,
) {
    // グループ強調の色ハイライト (旧 COLOR_GROUP_BG 青 tint) は撤去。
    // グループ識別は構造手掛かり ("↳" depth prefix + 折り畳み) だけで担い、
    // 背景は通常 strip と同じ neutral (elevation-1 = strip 本体) に統一する。
    let bg = app.theme.core.panel;
    let display_name = if entry.depth > 0 {
        let arrows = "↳".repeat(entry.depth.min(4) as usize);
        format!("{arrows} {}", entry.name)
    } else {
        entry.name.clone()
    };
    let track_id = entry.track_id;
    // group strip は折り畳み disclosure を出す。collapsed 状態は
    // arrangement と共通の collapsed_groups を引く。
    let group_collapsed = if entry.is_group {
        Some(app.ui_prefs.collapsed_groups.contains(&track_id))
    } else {
        None
    };
    let (was_dragging_vol, was_dragging_pan) = drag_flags(app, track_id);
    let n_sends = app.song_doc.song().track_by_id(track_id).map_or(0, |t| t.sends.len());
    // strip 高さが足りないときは band 側を縮めてフェーダーの最低高を守る
    // (縮めた分の send 行は band 内の縦スクロールで到達できる)。 旧実装は
    // band を要求どおり確保して fader_h を `.max(20.0)` で誤魔化していたため、
    // send 2 本以上でフェーダー / メーターと Sends セクションが重なって描かれ、
    // 重なり領域では先に描かれるフェーダーが press を consume して × / send knob が
    // クリック不能になっていた。
    let sends_band_h = sends_band_height_fitted(n_sends, rect.h);
    draw_strip(
        app,
        ui,
        track_id as usize,
        &display_name,
        entry.volume,
        entry.pan,
        entry.muted,
        entry.solo,
        entry.peak_l_raw,
        entry.peak_r_raw,
        rect,
        bg,
        Some(track_color::to_renderer(entry.color)),
        track_id,
        false,
        group_collapsed,
        sends_band_h,
        was_dragging_vol,
        was_dragging_pan,
    );
    // Sends セクションは draw_strip の fader 下端より下の band に描画する。
    draw_sends_section(app, ui, track_id, rect, bg, sends_band_h);
}

/// リターン strip。 通常の fader / pan / mute / solo を持つが、 緑 tint
/// (`daw.strip_return_bg`) で別物として見せ、 Sends セクションは描画しない
/// (= 簡潔さ優先)。
fn draw_return_strip(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    entry: &crate::app::TrackMixEntry,
    rect: Rect,
) {
    let track_id = entry.track_id;
    let (was_dragging_vol, was_dragging_pan) = drag_flags(app, track_id);
    draw_strip(
        app,
        ui,
        track_id as usize,
        &entry.name,
        entry.volume,
        entry.pan,
        entry.muted,
        entry.solo,
        entry.peak_l_raw,
        entry.peak_r_raw,
        rect,
        app.theme.daw.strip_return_bg,
        Some(track_color::to_renderer(entry.color)),
        track_id,
        false,
        None, // group_collapsed: return strip は disclosure 無し
        0.0, // sends_band_h = 0 (リターンは send 元 UI を出さない)
        was_dragging_vol,
        was_dragging_pan,
    );
}

/// この track の Volume / Pan が前フレーム時点で active gesture かを
/// `AppData.active_param_gestures` から引く (= gesture edge 検知用)。
fn drag_flags(app: &AppData, track_id: u32) -> (bool, bool) {
    let vol = app
        .recording.active_param_gestures
        .contains(&(track_id, AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume)));
    let pan = app
        .recording.active_param_gestures
        .contains(&(track_id, AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan)));
    (vol, pan)
}

#[allow(clippy::too_many_arguments)]
fn draw_strip(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    layout_idx: usize,
    name: &str,
    volume: f32,
    pan: f32,
    muted: bool,
    solo: bool,
    peak_l_raw: f32,
    peak_r_raw: f32,
    rect: Rect,
    bg: Color,
    // track の effective 色。 `Some(c)` で strip 左端に縦カラーストライプを
    // 描く (arrangement header と同 idiom)。 master strip は neutral なので None。
    color: Option<Color>,
    track_idx: u32,
    is_master: bool,
    // group strip のとき `Some(collapsed)` を渡すと、 名前左に折り畳み
    // disclosure ▶/▼ を描き、 click で `collapsed_groups` を toggle する
    // (arrangement と同じ SSoT)。 非 group (通常 track / return / master) は `None`。
    group_collapsed: Option<bool>,
    // この strip 下部に確保する Sends セクション band の高さ (px)。 通常
    // track は caller が `sends_band_height` で算出した値、 リターン / master
    // は 0。 fader 下端をこの分だけ持ち上げて領域を空ける。 Sends セクション
    // 本体の描画は caller (`draw_track_strip`) が `draw_sends_section` で行う。
    sends_band_h: f32,
    // Phase 4 Step B: 前フレーム時点での「この track の Volume / Pan が
    // active gesture か」 を caller (= draw) が AppData から読んだ値。 widget
    // の dragging 結果と diff して ParamGestureBegin / End を発火する。
    // master strip は automation target を持たないので常に false を渡す。
    was_dragging_vol: bool,
    was_dragging_pan: bool,
) {
    let p = &app.theme.core;
    ui.panel(("mixer_strip_bg", layout_idx), rect, bg, 4.0);

    // track 色ストライプ: strip 左端に縦 COLOR_STRIP_W px。 panel と同じ角丸
    // (radius 4) に揃えるため左 2 隅 (tl, bl) のみ丸める。 bg の上に重ねるので
    // group (青) / return (緑) tint と色衝突せず常にトラック色が視認できる。
    if let Some(c) = color {
        ui.push_rect(RectCommand {
            rect: Rect { x: rect.x, y: rect.y, w: COLOR_STRIP_W, h: rect.h },
            fill: c,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [4.0, 0.0, 0.0, 4.0],
            clip_rect: None,
        });
    }

    // r.md #13: 選択中トラックの strip をアクセント枠でハイライトする。
    // arrangement と同じ `selection.selected_track_ids` (SSoT) を参照するので
    // 両ビューが連動して光る。 master は実トラックではないので対象外。 色ストライプ
    // の**後**に描いて枠が左辺で途切れないようにする (最前面に完全な枠)。
    if !is_master && app.selection.selected_track_ids.contains(&track_idx) {
        ui.push_rect(RectCommand {
            rect,
            fill: Color::TRANSPARENT,
            border: p.accent,
            border_width: 2.0,
            radius: [4.0; 4],
            clip_rect: None,
        });
    }

    let pad = 6.0;
    let mut y = rect.y + pad;

    // 名前 (group strip は左に折り畳み disclosure ▶/▼ を置く)
    let name_x = if let Some(collapsed) = group_collapsed {
        let tri = if collapsed { "\u{25b6}" } else { "\u{25bc}" }; // ▶ 折り畳み / ▼ 展開
        let disc_w = 14.0;
        ui.button_at(
            ("mixer_strip_disclosure", layout_idx),
            tri,
            Rect { x: rect.x + pad, y, w: disc_w, h: 14.0 },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    // arrangement の ToggleGroupCollapsed と同じ toggle
                    // (collapsed_groups が両 view 共通の SSoT)。
                    if app.ui_prefs.collapsed_groups.contains(&track_idx) {
                        app.ui_prefs.collapsed_groups.remove(&track_idx);
                    } else {
                        app.ui_prefs.collapsed_groups.insert(track_idx);
                    }
                })
            },
        );
        rect.x + pad + disc_w + 2.0
    } else {
        rect.x + pad
    };
    // 80px ストリップに収まらない名前は末尾 ellipsis + clip。 素の label_at だと
    // 半角 13 字以上 (group strip は 10 字以上) で strip 境界を越え、 STRIP_GAP の
    // 隙間にグリフの破片が残ったまま隣の strip 背景に飲まれて「末尾が黙って消える」
    // (例: multi-out 子トラックの "Surge XT Out 1" / "Out 2" が区別できない)。
    ui.label_at_clipped(
        ("mixer_strip_name", layout_idx),
        name,
        Rect {
            x: name_x,
            y,
            w: (rect.x + rect.w - pad - name_x).max(1.0),
            h: TOP_LABEL_H,
        },
        11.0,
        // 全トラック名を本文色で描画。 旧 dim はクローム面 (strip 本体) に対し
        // コントラスト不足で読みにくかった。 乗る背景は strip の面 (panel /
        // return tint / master) というパレット自身のクロームなので、 極性固定
        // インクではなくテーマ従属の `text` でよい。
        p.text,
    );
    y += TOP_LABEL_H;

    if !is_master {
        let btn_w = (rect.w - pad * 2.0 - 4.0) * 0.5;
        ui.toggle_button_at(
            ("mixer_strip_mute", layout_idx),
            "M",
            Rect { x: rect.x + pad, y, w: btn_w, h: TOGGLE_H },
            muted,
            &style_mute(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackMute(track_idx))
                })
            },
        );
        ui.toggle_button_at(
            ("mixer_strip_solo", layout_idx),
            "S",
            Rect { x: rect.x + pad + btn_w + 4.0, y, w: btn_w, h: TOGGLE_H },
            solo,
            &style_solo(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackSolo(track_idx))
                })
            },
        );
        y += TOGGLE_H + 6.0;

        // Pan 行 = `[ノブ 32][gap 4][数値欄 30]` を **1 行のまとまり** として strip
        // 中央に寄せる (r.md #62)。 旧レイアウトはノブの真下に数値行 (12 + 間隔 2px) を
        // 積んでいて、 strip 1 本あたり縦 14px を数値だけに費やしていた。 横に並べれば
        // 行高 = ノブ径のままなので、 その 14px はそのまま fader / メーター高に回る。
        // 参照 DAW (Ardour / Bitwig Mix view / REAPER MCP) はいずれも pan に数値専用の
        // 行を割かない。 左が「つまみ」・右が「その値」 の並びは、 本プロジェクトの
        // インスペクタ (ラベル左・値右) とも一致する。
        //
        // ノブ本体は plain -1..1 ⇔ 正規化 0..1 の写像を手書きせず、
        // `common::automation` の plain⇔norm SSoT を使う (同じ式を 3 本書かない)。
        let pan_row_x = rect.x + (rect.w - PAN_ROW_W) * 0.5;
        let knob_x = pan_row_x;
        let track_idx_for_pan = track_idx;
        // per-control modulation (docs/plan_modulation_routing_redesign.md §6, gui_01
        // #109): Pan を音でドラッグ変調する Bitwig 流。knob は値が 0..=1 正規化なので
        // `ModControlDomain::Norm`、routing 帰属はこの strip のトラック (track_idx)。
        let pan_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan);
        let knob_value = plain_to_norm(&pan_target, f64::from(pan));
        let pan_mod =
            build_mod(app, pan_target.clone(), f64::from(knob_value), ModControlDomain::Norm, track_idx);
        let pan_resp = ui.knob_at(
            ("mixer_strip_pan", layout_idx),
            Rect { x: knob_x, y, w: KNOB_SIZE, h: KNOB_SIZE },
            knob_value,
            0.5,
            // Pan は bipolar param: 見かけの零点はセンタ (12 時)。 弧はセンタから
            // L/R 方向へ伸び、 センタでは塗りが消えてセンタ notch だけが残る (r.md #47)。
            //
            // `surface` には **この strip の実際の背景** を渡す。 通常 / group strip は
            // `panel` だが return strip は緑 tint (`daw.strip_return_bg`) なので、 palette の
            // 既定 (`panel`) 任せにすると return strip だけ可動範囲外の切り欠きが
            // 「暗い帯」 として浮く。 caller は bg を持っているので迷わず渡せる。
            &KnobStyle { surface: Some(bg), ..KnobStyle::BIPOLAR },
            {
                let target_for_change = pan_target.clone();
                move |v| {
                    #[allow(clippy::cast_possible_truncation)]
                    let pan = norm_to_plain(&target_for_change, v) as f32;
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::SetTrackPan {
                            track: track_idx_for_pan,
                            pan,
                        })
                    })
                }
            },
            Some(pan_mod.modulation()),
        );
        push_mod_drag_resync(ui, app, track_idx, &pan_target, pan_resp.mod_dragging);

        // Pan の数値欄 (`"L50"` / `"C"` / `"R100"`)。 参照 DAW は全社が pan の数値を出す
        // (REAPER `100%L..100%R` / Ardour `L:50 R:50` / Live `50L`)。 表記は
        // `automation_value` の PAN_FORMAT が SSoT で、 inspector / automation lane と同一。
        //
        // **読むだけでなく編集できる**: 従来 daw_01 には pan を数値で正確に指定する手段が
        // どこにも無く、 ノブのドラッグだけだった。 Ardour はミキサーの数値欄について
        // 「its precise value is shown in a text field ... that doubles as a way to type in a
        // numeric value」 と明記している。 ここは `scrubable_number_at` (drag で微調整 /
        // click で打ち込み / dblclick でセンタへリセット) をそのまま使う = inspector /
        // automation lane header / export range と同じ idiom (bespoke な編集バッファを作らない)。
        //
        // 値は **knob の `displayed_value`** から取る: knob を drag / dblclick reset している
        // 間は model より widget の preview が先行するので、 app 側の pan を読むと数値だけ
        // 1 frame 遅れる (逆に数値欄を drag している間は widget 自身の preview が優先される)。
        let pan_desc = automation_value_display(&pan_target, None);
        let pan_plain = norm_to_plain(&pan_target, pan_resp.displayed_value);
        let readout_style = ScrubableNumberStyle {
            // hover は窪みの既定ではなく 1 段持ち上げた `control`。 80px strip では
            // 「いまどの欄を掴んでいるか」 が面から離れて見えないと読めない (inspector と同じ判断)。
            bg_color_hovered: p.control,
            // scrub 中の帯は accent の electric azure ではなく控えめな `scrub_drag_bg`
            // (transport のテンポだけが暖色版を使う)。
            bg_color_dragging: p.scrub_drag_bg,
            font_size: PAN_READOUT_FONT,
            pad_x: PAN_READOUT_PAD_X,
            sensitivity: PAN_READOUT_SENSITIVITY,
            range: Some(pan_desc.range),
            ..ScrubableNumberStyle::from_palette(p)
        };
        let readout_resp = ui.scrubable_number_at(
            ("mixer_strip_pan_value", layout_idx),
            Rect {
                x: pan_row_x + KNOB_SIZE + PAN_READOUT_GAP,
                y: y + (KNOB_SIZE - PAN_READOUT_H) * 0.5,
                w: PAN_READOUT_W,
                h: PAN_READOUT_H,
            },
            pan_plain,
            // dblclick reset = センタ (`"C"`)。 ノブの dblclick (正規化 0.5) と同じ着地点。
            0.0,
            pan_desc.format,
            &readout_style,
            move |v| {
                // 範囲は widget が `style.range` で clamp 済だが、 表示レンジの SSoT
                // (`AutomationValueDisplay`) を通してから model 単位へ落とす。
                #[allow(clippy::cast_possible_truncation)]
                let pan = pan_desc.clamp_plain(v) as f32;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetTrackPan { track: track_idx_for_pan, pan })
                })
            },
            None,
            // modulation の表示・depth ドラッグ面は **ノブ 1 つに集約** する
            // (同じ param の変調を 2 箇所で編集できる状態を作らない)。
            None,
        );
        // gesture (= undo 1 step + オートメーション記録) は **ノブと数値欄で 1 本**。
        // 同じ `(track, Pan)` を key にするので、 どちらの drag でも Begin / End は
        // 1 回ずつになるよう OR を取ってから edge 検知に渡す。 text 打ち込みは 1 回の
        // `SetTrackPan` で完結する (= それ自体が 1 undo step) ので gesture にしない。
        push_param_gesture_edges(
            ui,
            track_idx,
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan),
            "Pan",
            was_dragging_pan,
            pan_resp.dragging || readout_resp.dragging,
        );
        y += KNOB_SIZE + 2.0;
    }

    // 縦 fader + L/R peak meter。 Sends セクションを持つ strip では、 その
    // band の高さ分だけ fader 下端を持ち上げて領域を空ける (= caller が
    // `draw_sends_section` で同じ band geometry を使って描く)。
    let fader_top = y + 4.0;
    // `sends_band_height_fitted` はこの積み上げを定数化した値で band 高を決める。
    // 片方だけ変えると band とフェーダーが重なるので、 非 master 経路で一致を固定する。
    debug_assert!(
        is_master || (fader_top - (rect.y + STRIP_FADER_TOP_OFFSET)).abs() < 0.01,
        "STRIP_FADER_TOP_OFFSET が draw_strip の y 積み上げとずれている"
    );
    let fader_bottom = rect.y + rect.h - pad - 12.0 - sends_band_h;
    let fader_h = (fader_bottom - fader_top).max(20.0);

    let group_w = FADER_W + METER_GAP + METER_SCALE_W;
    let group_x = rect.x + (rect.w - group_w) * 0.5;

    let fader_db = if volume <= 0.0 { f32::NEG_INFINITY } else { 20.0 * volume.log10() };
    let track_idx_for_vol = track_idx;
    let is_master_for_vol = is_master;
    // fader ハンドル・L/R メーター・dB 目盛り・0dB 線・
    // peak を「ただ一つの dB→ピクセル y 写像」から配置する単一 widget に統一。
    // group rect (group_w = FADER_W + METER_GAP + METER_SCALE_W = 55) を渡すと
    // widget が内部で fader 列 (fader_w) と meter 列に分割し、両者の高さ写像が
    // 構造的に一致する (旧 fader_at + level_meter_stereo 別置きの ~13px ズレ解消)。
    let vol_scale = MeterScale::default();
    let style = LevelMeterStyle {
        scale: Some(vol_scale),
        peak_readout: true,
        ..LevelMeterStyle::from_palette(p)
    };
    // per-control modulation (docs/plan_modulation_routing_redesign.md §6, gui_01
    // #110): 音量フェーダーを音でドラッグ変調。表示ドメインは「フェーダーの正規化
    // トラック位置」(dB taper) なので base も `MeterScale::db_to_frac(dB)` の frac で
    // 渡し、`ModControlDomain::FaderDb` が volume(amp) ↔ frac を解決する。master は
    // 変調対象外 (cursor_modulatable_targets と同様)。
    let vol_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);
    let vol_mod = if is_master {
        None
    } else {
        let base_frac = f64::from(vol_scale.db_to_frac(fader_db));
        Some(build_mod(app, vol_target.clone(), base_frac, ModControlDomain::FaderDb(vol_scale), track_idx))
    };
    let resp = ui.channel_fader_meter(
        ("mixer_strip_fader", layout_idx),
        Rect { x: group_x, y: fader_top, w: group_w, h: fader_h },
        FADER_W,
        fader_db,
        0.0,
        peak_l_raw,
        peak_r_raw,
        MeterBallistic::Peak,
        style,
        move |new_db| {
            let amp = if new_db.is_finite() { 10f32.powf(new_db / 20.0) } else { 0.0 };
            Edit::mutate(move |app: &mut AppData| {
                if is_master_for_vol {
                    app.handle_event(AppEvent::SetMasterGain(amp));
                } else {
                    app.handle_event(AppEvent::SetTrackVolume {
                        track: track_idx_for_vol,
                        amp,
                    });
                }
            })
        },
        vol_mod.as_ref().map(|m| m.modulation()),
    );
    // Phase 4 Step B: master strip は automation target を持たないので skip。
    if !is_master {
        push_param_gesture_edges(
            ui,
            track_idx,
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
            "Volume",
            was_dragging_vol,
            resp.fader.dragging,
        );
        push_mod_drag_resync(ui, app, track_idx, &vol_target, resp.mod_dragging);
    }
}

/// strip 下部に確保する Sends セクション band の高さ (px)。 send 行数 +
/// 「＋ Send」 ボタン + 区切り余白で決まる。 `draw_strip` (fader 短縮量) と
/// `draw_sends_section` (実描画) の両方がこれを使って geometry を揃える。
fn sends_band_height(n_sends: usize) -> f32 {
    // 上端の区切り線 + 各 send 行 + 「＋ Send」 ボタン + 下端余白。
    4.0 + (n_sends as f32) * SEND_ROW_H + ADD_SEND_H + 4.0
}

/// フェーダー / メーターに残す最小高 (px)。 これを割り込むと掴めなくなるので、
/// Sends band 側を縮めて (= band 内を縦スクロールさせて) 守る。
const MIN_FADER_H: f32 = 28.0;

/// strip 上部 (pad + 名前 + M/S + pan 行 + fader 上マージン) が固定で食う高さ。
/// `draw_strip` の y 積み上げと一致させること (`debug_assert` で固定)。
/// r.md #62 で pan 数値をノブの右へ移したので、 pan 行の高さ = ノブ径のみ (旧実装は
/// ここに数値行 `PAN_READOUT_H + 2.0` が積まれていて strip 1 本あたり 14px 高かった)。
const STRIP_FADER_TOP_OFFSET: f32 = 6.0
    + TOP_LABEL_H
    + TOGGLE_H
    + 6.0
    + KNOB_SIZE
    + 2.0
    + 4.0;
/// fader 下端から strip 下端までの固定余白 (`draw_strip` の `pad + 12.0`)。
const STRIP_FADER_BOTTOM_PAD: f32 = 6.0 + 12.0;

/// `sends_band_height` を strip の実高さに収まるよう clamp した値。
/// 収まらないぶんは `draw_sends_section` 内の縦スクロールで到達する。
fn sends_band_height_fitted(n_sends: usize, strip_h: f32) -> f32 {
    let want = sends_band_height(n_sends);
    let room = (strip_h - STRIP_FADER_TOP_OFFSET - MIN_FADER_H - STRIP_FADER_BOTTOM_PAD).max(0.0);
    want.min(room)
}

/// controls 行で Pre/Post トグルに割り当てる幅。 knob 右の小ボタン帯から per-send mute
/// (M) を固定幅で引いた残り全部を Pre/Post に与え、 "Post" (最長ラベル) が省略
/// (P…) されないようにする。 `inner_w` は strip の内側幅 (= `rect.w - SEND_PAD*2`)。
/// `draw_sends_section` と回帰テストの SSoT。
fn send_prepost_width(inner_w: f32) -> f32 {
    let btns_w = inner_w - (SEND_KNOB_SIZE + 4.0);
    (btns_w - SEND_MUTE_BTN_W - SEND_BTN_GAP).max(0.0)
}

/// strip 下部の Sends セクションを描画する。 各 send は 2 行の slot:
///   header 行 : 宛先名ラベル (省略付き) + × (remove、 右上)
///   controls 行: [ミニ level knob | Pre/Post | M(per-send mute)]
/// 80px ストリップ幅に 4 要素を 1 行で詰めると Pre/Post が ~13px しか取れず "Post"/"Pre"
/// が "P…" に省略される (daw_01 UI/UX 修正)。 × を header 右上に逃がし、 controls 行の
/// 残り幅を Pre/Post に寄せて省略を無くす。 末尾に「＋ Send」 ボタン (= track picker を
/// 開く)。 `band_h` は `sends_band_height` と一致する (= caller が両方に同値を渡す)。
fn draw_sends_section(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    track_id: u32,
    rect: Rect,
    // この strip の背景色。 send のミニ knob が可動範囲外をくり抜くのに使う
    // (`KnobStyle::surface`)。 strip 本体と同じ面の上に描かれるので同値。
    bg: Color,
    band_h: f32,
) {
    let pad = SEND_PAD;
    let band_top = rect.y + rect.h - pad - band_h;
    // 上端に区切り線。 strip の面の上に引くクロームなので本文色 (`text`) で、
    // fader/メーター帯と Sends 帯の境目をはっきり分ける。
    ui.panel(
        ("mixer_sends_div", track_id as usize),
        Rect { x: rect.x + pad, y: band_top, w: rect.w - pad * 2.0, h: 1.0 },
        app.theme.core.text,
        0.0,
    );

    // 各 send の宛先名は派生 (= track_by_id で都度解決)。 send 本体は
    // `app.song_doc.song().track_by_id(track_id).sends` を読む。 track が無ければ
    // (race) 何も描かない。
    let Some(src_track) = app.song_doc.song().track_by_id(track_id) else {
        return;
    };
    // band が必要高より低い (= strip が短い) ときは band 内を縦スクロールさせる。
    // scrollbar が出る分だけ行の内側幅を詰めて × / M ボタンと重ならないようにする。
    let content_h = sends_band_height(src_track.sends.len());
    let band_rect = Rect { x: rect.x, y: band_top, w: rect.w, h: band_h };
    let scrolling = content_h > band_h + 0.5;
    let scrollbar_w = if scrolling { 10.0 } else { 0.0 };
    ui.scroll_area(
        ("mixer_sends_scroll", track_id as usize),
        band_rect,
        (band_rect.w, content_h),
        |ui, scroll_off| {
            draw_sends_rows(
                app,
                ui,
                track_id,
                src_track,
                rect,
                bg,
                band_top - scroll_off.1,
                scrollbar_w,
            );
        },
    );
}

/// Sends band の中身 (send 行 + 「＋ Send」)。 `top` は band 上端 (スクロール
/// オフセット適用済み)、 `scrollbar_w` は band にスクロールバーが出ているときの
/// 予約幅。 `draw_sends_section` の scroll_area 内からのみ呼ぶ。
#[allow(clippy::too_many_arguments)]
fn draw_sends_rows(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    track_id: u32,
    src_track: &common::model::Track,
    rect: Rect,
    bg: Color,
    top: f32,
    scrollbar_w: f32,
) {
    let pad = SEND_PAD;
    let mut y = top + 4.0;
    let inner_x = rect.x + pad;
    let inner_w = rect.w - pad * 2.0 - scrollbar_w;

    for (send_idx, send) in src_track.sends.iter().enumerate() {
        let dest_name = app
            .song_doc.song()
            .track_by_id(send.dest_track_id)
            .map(|t| {
                if t.name.is_empty() {
                    format!("\u{2192}{}", send.dest_track_id)
                } else {
                    format!("\u{2192}{}", t.name)
                }
            })
            .unwrap_or_else(|| format!("\u{2192}?{}", send.dest_track_id));
        // 矢印の後ろに空白を置かない: 名前欄は 51px しかなく (80px strip − pad − ×)、
        // 空白 1 文字 (font 10 で 5.3px) を足すと既定リターン名 "Return 1" (42.2px)
        // まで ellipsis されて "→ Return…" になり、 どのリターン宛てか判別できなく
        // なっていた。 空白なしなら "→Return 10" まで収まる。

        // header 行: 宛先名 (左、 × にかぶらないよう省略付き) + × (右上)。
        let close_x = inner_x + inner_w - SEND_CLOSE_BTN_W;
        let name_max_w = (close_x - SEND_BTN_GAP - inner_x).max(0.0);
        ui.label_at_clipped(
            ("mixer_send_name", track_id as usize, send_idx),
            &dest_name,
            Rect { x: inner_x, y, w: name_max_w, h: 12.0 },
            10.0,
            app.theme.core.text,
        );
        // × (remove send) — slot 右上 (= 一般的な「閉じる / 削除」位置)。
        let send_idx_for_remove = send_idx;
        ui.button_at_sized(
            ("mixer_send_remove", track_id as usize, send_idx),
            "x",
            Rect { x: close_x, y, w: SEND_CLOSE_BTN_W, h: 13.0 },
            11.0,
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::RemoveSend {
                        track_id,
                        send_idx: send_idx_for_remove,
                    })
                })
            },
        );
        // controls 行は header の下。
        let row_y = y + 14.0;

        // ミニ level knob (0..2 → 0..1 normalized、 volume / pan と同 idiom)。
        // gain は linear 0..2、 knob は 0..1 表示なので gain/2 を渡す。
        let knob_rect = Rect { x: inner_x, y: row_y, w: SEND_KNOB_SIZE, h: SEND_KNOB_SIZE };
        let send_idx_for_knob = send_idx;
        // 前フレーム時点で SendGain gesture か (= edge 検知)。
        // v29: SendGain target は安定 send id でアドレスする。
        let send_gain_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
            send_id: send.id,
            legacy_send_idx: None,
        });
        let was_dragging_send = app
            .recording.active_param_gestures
            .contains(&(track_id, send_gain_target.clone()));
        // 再生中は SendGain オートメーションの playhead 値に追従させる
        // (volume / pan と同 idiom)。 停止中・非 automation・書き込み中は send.gain。
        let live_gain = app.live_param_value(src_track, &send_gain_target, send.gain);
        let knob_resp = ui.knob_at(
            ("mixer_send_knob", track_id as usize, send_idx),
            knob_rect,
            (live_gain * 0.5).clamp(0.0, 1.0),
            // double-click reset = unity (= 1.0 linear → 0.5 normalized)。
            0.5,
            // 送り量は unipolar param: 零点は最小値 (7 時) で、 dblclick の戻り先が
            // unity (0.5) であることとは無関係 (弧の起点 ≠ default_value)。
            // `surface` = strip の背景 (pan knob と同じ理由)。
            &KnobStyle { surface: Some(bg), ..KnobStyle::UNIPOLAR },
            move |v| {
                let gain = v * 2.0;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetSendGain {
                        track_id,
                        send_idx: send_idx_for_knob,
                        gain,
                    })
                })
            },
            // SendGain は per-control modulation 非対象 (cursor_modulatable_targets
            // = Volume/Pan のみ)。ラックからもルーティングしない。
            None,
        );
        // send-gain automation の gesture edge を volume / pan と同様に発火。
        push_param_gesture_edges(
            ui,
            track_id,
            send_gain_target,
            "Send",
            was_dragging_send,
            knob_resp.dragging,
        );

        // knob 右に Pre/Post (広め) と M (1 文字・狭め) を横並び。 × は header に
        // 逃がしたので、 残り幅を Pre/Post に寄せて "Post" の省略を無くす。
        let btns_x = inner_x + SEND_KNOB_SIZE + 4.0;
        let prepost_w = send_prepost_width(inner_w);
        let btn_h = SEND_KNOB_SIZE.min(16.0);
        let btn_y = row_y;

        // Pre/Post トグル: PreFader のとき on 表示 (青)。 クリックで cycle。
        let is_pre = send.mode == SendMode::PreFader;
        let mode_label = if is_pre { "Pre" } else { "Post" };
        let send_idx_for_mode = send_idx;
        ui.toggle_button_at(
            ("mixer_send_prepost", track_id as usize, send_idx),
            mode_label,
            Rect { x: btns_x, y: btn_y, w: prepost_w, h: btn_h },
            is_pre,
            &style_send_prepost(&app.theme),
            move |_| {
                // クリックで Pre ↔ Post を反転。
                let next = if is_pre { SendMode::PostFader } else { SendMode::PreFader };
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetSendMode {
                        track_id,
                        send_idx: send_idx_for_mode,
                        mode: next,
                    })
                })
            },
        );

        // per-send mute (`enabled`)。 `enabled == false` = ミュート = 赤 ON
        // 表示にするため `!enabled` を value に渡し、 クリックで enabled を反転。
        let send_idx_for_mute = send_idx;
        let enabled = send.enabled;
        ui.toggle_button_at(
            ("mixer_send_mute", track_id as usize, send_idx),
            "M",
            Rect { x: btns_x + prepost_w + SEND_BTN_GAP, y: btn_y, w: SEND_MUTE_BTN_W, h: btn_h },
            !enabled,
            &style_send_mute(&app.theme),
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetSendEnabled {
                        track_id,
                        send_idx: send_idx_for_mute,
                        enabled: !enabled,
                    })
                })
            },
        );

        y += SEND_ROW_H;
    }

    // 「＋ Send」 ボタン: track picker を開いて宛先を選ぶ。
    ui.button_at(
        ("mixer_add_send", track_id as usize),
        "+ Send",
        Rect { x: inner_x, y, w: inner_w, h: ADD_SEND_H },
        move || {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::OpenSendPicker {
                    src_track_id: track_id,
                })
            })
        },
    );
}

// Phase 4 Step B の `push_param_gesture_edges` は共通 helper として
// `view::param_gesture` に抽出 (Phase 5 follow-up review、 transport.rs と
// 重複していたため)。

#[cfg(test)]
mod tests {
    use super::*;
    use daw_ui_core::{FrameInput, UiHost};
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::Scene;

    /// daw_01 UI/UX 修正の回帰固定: 旧レイアウトは Pre/Post トグルが ~13px しか
    /// 取れず "Pre"/"Post" が "P…" に省略されていた。 再設計後の `send_prepost_width`
    /// に最長ラベル "Post" が実 measure で省略なしに収まることを保証する。
    #[test]
    fn send_prepost_label_fits_without_ellipsis() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };

        let mut post_w = 0.0_f32;
        let mut pre_w = 0.0_f32;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            post_w = ui.measure_text("Post", SEND_PREPOST_FONT);
            pre_w = ui.measure_text("Pre", SEND_PREPOST_FONT);
        });

        let avail = send_prepost_width(STRIP_WIDTH - SEND_PAD * 2.0);
        assert!(
            post_w <= avail,
            "'Post' ({post_w}px @ {SEND_PREPOST_FONT}pt) は Pre/Post 幅 {avail}px に収まる"
        );
        assert!(
            pre_w <= avail,
            "'Pre' ({pre_w}px @ {SEND_PREPOST_FONT}pt) は Pre/Post 幅 {avail}px に収まる"
        );
    }

    /// Pan の数値欄 (r.md #62 でノブの右へ移設) は、 最長表記 `"L100"` が欄の内側
    /// (左右 `pad_x`) に省略なく収まり、 かつ `[ノブ][gap][欄]` の 1 行が strip の内側幅に
    /// 収まる。 欄幅は値によらず固定なので、 ここが破れるとノブ位置ごと崩れる。
    #[test]
    fn pan_readout_fits_field_width() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };

        // 最長は 3 桁 + ラベル。 PAN_FORMAT の実表記から生成して定数のズレを防ぐ。
        let widest = crate::automation_value::PAN_FORMAT.format_value(-1.0);
        let mut w = 0.0_f32;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            w = ui.measure_text(&widest, PAN_READOUT_FONT);
        });

        // 欄の文字領域 = 幅 − 左右 pad (= 表示と text input 双方の内側余白)。
        let text_avail = PAN_READOUT_W - PAN_READOUT_PAD_X * 2.0;
        assert_eq!(widest, "L100", "最長 pan 表記");
        assert!(
            w <= text_avail,
            "'{widest}' ({w}px @ {PAN_READOUT_FONT}pt) は数値欄の文字領域 {text_avail}px に収まる"
        );

        let inner_w = STRIP_WIDTH - 6.0 * 2.0; // draw_strip の pad = 6.0
        assert!(
            PAN_ROW_W <= inner_w,
            "pan 行 [ノブ+gap+数値欄] {PAN_ROW_W}px は strip 内側幅 {inner_w}px に収まる"
        );
    }

    /// strip の y 積み上げと `STRIP_FADER_TOP_OFFSET` の一致。 r.md #62 で pan 数値を
    /// ノブの右へ移したので、 pan 行は **ノブ径のみ** を消費する (旧実装比 14px 短縮)。
    /// `draw_strip` 側は debug_assert でしか守られていないため、 定数側をここで固定する。
    #[test]
    fn strip_fader_top_offset_matches_stack() {
        let stack = 6.0 // 上 pad
            + TOP_LABEL_H
            + TOGGLE_H
            + 6.0 // M/S 行の下マージン
            + KNOB_SIZE // pan 行 = ノブ径 (数値欄は横並びなので行高を増やさない)
            + 2.0 // pan 行 → fader 上マージン
            + 4.0; // fader_top の +4.0
        assert!(
            (STRIP_FADER_TOP_OFFSET - stack).abs() < 1e-6,
            "STRIP_FADER_TOP_OFFSET ({STRIP_FADER_TOP_OFFSET}) が積み上げ ({stack}) と一致"
        );
    }
}
