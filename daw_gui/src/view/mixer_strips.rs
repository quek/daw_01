//! Renoise 風 mixer strip。draw(...) を呼ぶと指定 area 内に N 本のチャンネル
//! ストリップが横並びで描画される。
//!
//! 各 strip:
//!   - トラック名
//!   - M (mute) / S (solo) toggle
//!   - Pan knob
//!   - Volume fader (縦) + L/R peak meter

use common::model::{AutomationTarget, SendMode, TrackBuiltinParam};
use daw_ui_core::{Edit, LevelMeterStyle, MeterBallistic, MeterScale, ToggleButtonStyle, Ui};

use crate::view::modulation::{build_mod, push_mod_drag_resync};
use crate::view::param_gesture::push_param_gesture_edges;
use crate::view::track_color;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent, ModControlDomain};

const STRIP_WIDTH: f32 = 80.0;
const STRIP_GAP: f32 = 4.0;
const TOP_LABEL_H: f32 = 18.0;
const TOGGLE_H: f32 = 22.0;
const KNOB_SIZE: f32 = 32.0;
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

const COLOR_BG: Color = Color { r: 0.13, g: 0.13, b: 0.15, a: 1.0 };
const COLOR_STRIP_BG: Color = Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 };
/// Return strip — 緑寄りの tint で、 通常 track / group bus とも別物だと
/// 一目で分かるようにする (Ableton の return track 列のメタファ)。
const COLOR_RETURN_BG: Color = Color { r: 0.18, g: 0.28, b: 0.22, a: 1.0 };
/// returns 帯と通常帯を分ける縦 divider の色。
const COLOR_RETURN_DIVIDER: Color = Color { r: 0.30, g: 0.40, b: 0.32, a: 1.0 };
const COLOR_MASTER_BG: Color = Color { r: 0.22, g: 0.22, b: 0.28, a: 1.0 };
const COLOR_TEXT: Color = Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 };
/// Mute active 時の背景色 (= 業界標準の赤)。 旧 hint band 色を on_color に昇格
/// (gui_01 #052 で hint_band 廃止 → ON は背景色のみで表現する idiom に統一)。
const COLOR_MUTE_ACTIVE: Color = Color { r: 0.86, g: 0.27, b: 0.27, a: 1.0 };
/// Solo active 時の背景色 (= 業界標準の黄)。 同様に旧 hint band 色を on_color に昇格。
const COLOR_SOLO_ACTIVE: Color = Color { r: 0.90, g: 0.78, b: 0.31, a: 1.0 };
/// 黄背景 (Solo) と組み合わせる黒文字 (= 白文字では視認性低い、 STYLE_CLICK と同 idiom)。
const COLOR_TEXT_BLACK: Color = Color { r: 0.10, g: 0.10, b: 0.12, a: 1.0 };

const TOGGLE_BUTTON_BASE: ToggleButtonStyle = ToggleButtonStyle {
    off_color: Color { r: 0.22, g: 0.22, b: 0.26, a: 1.0 },
    on_color: Color { r: 0.30, g: 0.30, b: 0.36, a: 1.0 },
    border: Color { r: 0.35, g: 0.38, b: 0.45, a: 1.0 },
    border_width: 1.0,
    radius: 4.0,
    font_size: 12.0,
    text_color: Color { r: 0.92, g: 0.93, b: 0.96, a: 1.0 },
    on_text_color: None,
};

const STYLE_MUTE: ToggleButtonStyle = ToggleButtonStyle {
    on_color: COLOR_MUTE_ACTIVE,
    ..TOGGLE_BUTTON_BASE
};

const STYLE_SOLO: ToggleButtonStyle = ToggleButtonStyle {
    on_color: COLOR_SOLO_ACTIVE,
    on_text_color: Some(COLOR_TEXT_BLACK),
    ..TOGGLE_BUTTON_BASE
};

/// send 内 per-send mute (`enabled`)。 mute と同じ赤系 on_color、 ただし
/// `enabled == true` (= 鳴っている) が「OFF 表示」、 `enabled == false`
/// (= ミュート) が「ON 表示 (赤)」 になるよう、 描画側で `!enabled` を渡す。
const STYLE_SEND_MUTE: ToggleButtonStyle = ToggleButtonStyle {
    on_color: COLOR_MUTE_ACTIVE,
    font_size: 10.0,
    ..TOGGLE_BUTTON_BASE
};

/// Pre/Post 切替トグル。 PreFader のとき on_color (青系) で強調する。
const STYLE_SEND_PREPOST: ToggleButtonStyle = ToggleButtonStyle {
    on_color: Color { r: 0.32, g: 0.55, b: 0.85, a: 1.0 },
    font_size: SEND_PREPOST_FONT,
    ..TOGGLE_BUTTON_BASE
};


pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    ui.panel("mixer_bg", area, COLOR_BG, 0.0);

    // FIXME #68: S キーで「マウス直下のストリップ」を solo するため、 各 strip の
    // rect にポインタ当たり判定をして hover track を求める (arrangement の
    // `arrange_hovered_track` と同 idiom)。 layout を持つこの draw が唯一の算出点
    // (SSoT)。 master strip は solo を持たないので対象外 (= None のまま)。
    let ptr = ui.pointer().pos;
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

    // FIXME #7: 折り畳まれた group の配下 strip は隠す (arrangement と同じ
    // `collapsed_groups` を参照 = SSoT 共有)。x レイアウト / content_w が
    // filter 後の index に揃うよう、 並べる前に除外する。group strip 自身は
    // (自分の祖先に collapsed が無い限り) 残り、 disclosure ▶/▼ を出す。
    let normals: Vec<_> = normals
        .into_iter()
        .filter(|e| !app.is_hidden_under_collapsed_group(e.track_id))
        .collect();

    // ----- 右端から固定配置: MASTER → returns 帯 → 「＋ Return」 -----
    let master_x = area.x + area.w - inner_pad - STRIP_WIDTH;

    // returns 帯: master の左に returns.len() 本 + 帯上端に「＋ Return」 ボタン。
    // returns 0 本でも「＋ Return」 ボタンは出す (= master のすぐ左)。
    let returns_w = (returns.len() as f32) * pitch;
    // 「＋ Return」 ボタン用に固定列を 1 本分確保する。
    let add_return_col_w = STRIP_WIDTH;
    let returns_band_x = master_x - inner_pad - returns_w;
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
            COLOR_RETURN_DIVIDER,
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
            draw_return_strip(app, ui, entry, strip_rect);
        }
    }

    // ----- MASTER strip (右端固定) -----
    // Master strip は track ではないので gesture 対象外 (= 渡す flag は false)。
    draw_strip(
        app,
        ui,
        usize::MAX,
        "MASTER",
        app.master_gain,
        0.0,
        false,
        false,
        app.peak_l_display,
        app.peak_r_display,
        Rect { x: master_x, y: strip_y, w: STRIP_WIDTH, h: strip_h },
        COLOR_MASTER_BG,
        None, // master は track 色を持たない (neutral 背景)
        u32::MAX,
        true,
        None, // group_collapsed: master は group disclosure 無し
        0.0, // sends_band_h (master は Sends セクション無し)
        false,
        false,
    );

    // FIXME #68: 算出した hover track を AppData に反映 (変化時のみ Edit、
    // arrange_hovered_track と同じ diff-guard)。 dispatch_shortcuts が S キーで読む。
    if app.mixer_hovered_track != hovered_strip {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.mixer_hovered_track = hovered_strip;
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
    // FIXME #5: グループ強調の色ハイライト (旧 COLOR_GROUP_BG 青 tint) は撤去。
    // グループ識別は構造手掛かり ("↳" depth prefix + 折り畳み) だけで担い、
    // 背景は通常 strip と同じ neutral に統一する。
    let bg = COLOR_STRIP_BG;
    let display_name = if entry.depth > 0 {
        let arrows = "↳".repeat(entry.depth.min(4) as usize);
        format!("{arrows} {}", entry.name)
    } else {
        entry.name.clone()
    };
    let track_id = entry.track_id;
    // FIXME #7: group strip は折り畳み disclosure を出す。collapsed 状態は
    // arrangement と共通の collapsed_groups を引く。
    let group_collapsed = if entry.is_group {
        Some(app.collapsed_groups.contains(&track_id))
    } else {
        None
    };
    let (was_dragging_vol, was_dragging_pan) = drag_flags(app, track_id);
    let n_sends = app.song.track_by_id(track_id).map_or(0, |t| t.sends.len());
    let sends_band_h = sends_band_height(n_sends);
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
    draw_sends_section(app, ui, track_id, rect, sends_band_h);
}

/// リターン strip。 通常の fader / pan / mute / solo を持つが、 緑 tint で
/// 別物として見せ、 Sends セクションは描画しない (= 簡潔さ優先)。
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
        COLOR_RETURN_BG,
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
        .active_param_gestures
        .contains(&(track_id, AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume)));
    let pan = app
        .active_param_gestures
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
    // FIXME #7: group strip のとき `Some(collapsed)` を渡すと、 名前左に折り畳み
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

    let pad = 6.0;
    let mut y = rect.y + pad;

    // 名前 (FIXME #7: group strip は左に折り畳み disclosure ▶/▼ を置く)
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
                    if app.collapsed_groups.contains(&track_idx) {
                        app.collapsed_groups.remove(&track_idx);
                    } else {
                        app.collapsed_groups.insert(track_idx);
                    }
                })
            },
        );
        rect.x + pad + disc_w + 2.0
    } else {
        rect.x + pad
    };
    ui.label_at(
        ("mixer_strip_name", layout_idx),
        name,
        name_x,
        y,
        11.0,
        // FIXME #14 (plan_mixer_name_contrast): 全トラック名を明色で描画。 旧 dim
        // (COLOR_TEXT) は暗 strip 背景に対しコントラスト不足で読みにくかった。
        COLOR_TEXT,
    );
    y += TOP_LABEL_H;

    if !is_master {
        let btn_w = (rect.w - pad * 2.0 - 4.0) * 0.5;
        ui.toggle_button_at(
            ("mixer_strip_mute", layout_idx),
            "M",
            Rect { x: rect.x + pad, y, w: btn_w, h: TOGGLE_H },
            muted,
            &STYLE_MUTE,
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
            &STYLE_SOLO,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::ToggleTrackSolo(track_idx))
                })
            },
        );
        y += TOGGLE_H + 6.0;

        // Pan knob (-1..1 → 0..1)
        let knob_x = rect.x + (rect.w - KNOB_SIZE) * 0.5;
        let knob_value = (pan + 1.0) * 0.5;
        let track_idx_for_pan = track_idx;
        // per-control modulation (docs/plan_modulation_routing_redesign.md §6, gui_01
        // #109): Pan を音でドラッグ変調する Bitwig 流。knob は値が 0..=1 正規化なので
        // `ModControlDomain::Norm`、routing 帰属はこの strip のトラック (track_idx)。
        let pan_target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan);
        let pan_mod =
            build_mod(app, pan_target.clone(), f64::from(knob_value), ModControlDomain::Norm, track_idx);
        let pan_resp = ui.knob_at(
            ("mixer_strip_pan", layout_idx),
            Rect { x: knob_x, y, w: KNOB_SIZE, h: KNOB_SIZE },
            knob_value,
            0.5,
            "Pan",
            move |v| {
                let pan = v * 2.0 - 1.0;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SetTrackPan {
                        track: track_idx_for_pan,
                        pan,
                    })
                })
            },
            Some(pan_mod.modulation()),
        );
        push_param_gesture_edges(
            ui,
            track_idx,
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan),
            "Pan",
            was_dragging_pan,
            pan_resp.dragging,
        );
        push_mod_drag_resync(ui, app, track_idx, &pan_target, pan_resp.mod_dragging);
        y += KNOB_SIZE + 4.0;
    }

    // 縦 fader + L/R peak meter。 Sends セクションを持つ strip では、 その
    // band の高さ分だけ fader 下端を持ち上げて領域を空ける (= caller が
    // `draw_sends_section` で同じ band geometry を使って描く)。
    let fader_top = y + 4.0;
    let fader_bottom = rect.y + rect.h - pad - 12.0 - sends_band_h;
    let fader_h = (fader_bottom - fader_top).max(20.0);

    let group_w = FADER_W + METER_GAP + METER_SCALE_W;
    let group_x = rect.x + (rect.w - group_w) * 0.5;

    let fader_db = if volume <= 0.0 { f32::NEG_INFINITY } else { 20.0 * volume.log10() };
    let track_idx_for_vol = track_idx;
    let is_master_for_vol = is_master;
    let fader_label: &'static str = if is_master_for_vol { "Master Volume" } else { "Track Volume" };
    // FIXME #1 (gui_01 #083): fader ハンドル・L/R メーター・dB 目盛り・0dB 線・
    // peak を「ただ一つの dB→ピクセル y 写像」から配置する単一 widget に統一。
    // group rect (group_w = FADER_W + METER_GAP + METER_SCALE_W = 55) を渡すと
    // widget が内部で fader 列 (fader_w) と meter 列に分割し、両者の高さ写像が
    // 構造的に一致する (旧 fader_at + level_meter_stereo 別置きの ~13px ズレ解消)。
    let vol_scale = MeterScale::default();
    let style = LevelMeterStyle {
        scale: Some(vol_scale),
        peak_readout: true,
        ..LevelMeterStyle::default()
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
        fader_label,
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
    band_h: f32,
) {
    let pad = SEND_PAD;
    let band_top = rect.y + rect.h - pad - band_h;
    // 上端に薄い区切り線。
    ui.panel(
        ("mixer_sends_div", track_id as usize),
        Rect { x: rect.x + pad, y: band_top, w: rect.w - pad * 2.0, h: 1.0 },
        COLOR_TEXT,
        0.0,
    );

    let mut y = band_top + 4.0;
    let inner_x = rect.x + pad;
    let inner_w = rect.w - pad * 2.0;

    // 各 send の宛先名は派生 (= track_by_id で都度解決)。 send 本体は
    // `app.song.track_by_id(track_id).sends` を読む。 track が無ければ
    // (race) 何も描かない。
    let Some(src_track) = app.song.track_by_id(track_id) else {
        return;
    };
    for (send_idx, send) in src_track.sends.iter().enumerate() {
        let dest_name = app
            .song
            .track_by_id(send.dest_track_id)
            .map(|t| {
                if t.name.is_empty() {
                    format!("→ {}", send.dest_track_id)
                } else {
                    format!("→ {}", t.name)
                }
            })
            .unwrap_or_else(|| format!("→ ?{}", send.dest_track_id));

        // header 行: 宛先名 (左、 × にかぶらないよう省略付き) + × (右上)。
        let close_x = inner_x + inner_w - SEND_CLOSE_BTN_W;
        let name_max_w = (close_x - SEND_BTN_GAP - inner_x).max(0.0);
        ui.label_at_clipped(
            ("mixer_send_name", track_id as usize, send_idx),
            &dest_name,
            Rect { x: inner_x, y, w: name_max_w, h: 12.0 },
            10.0,
            COLOR_TEXT,
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
        let was_dragging_send = app.active_param_gestures.contains(&(
            track_id,
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                send_idx: send_idx as u8,
            }),
        ));
        // FIXME #72: 再生中は SendGain オートメーションの playhead 値に追従させる
        // (volume / pan と同 idiom)。 停止中・非 automation・書き込み中は send.gain。
        let live_gain = app.live_param_value(
            src_track,
            &AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                send_idx: send_idx as u8,
            }),
            send.gain,
        );
        let knob_resp = ui.knob_at(
            ("mixer_send_knob", track_id as usize, send_idx),
            knob_rect,
            (live_gain * 0.5).clamp(0.0, 1.0),
            // double-click reset = unity (= 1.0 linear → 0.5 normalized)。
            0.5,
            "Send",
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
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                send_idx: send_idx as u8,
            }),
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
            &STYLE_SEND_PREPOST,
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
            &STYLE_SEND_MUTE,
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
}
