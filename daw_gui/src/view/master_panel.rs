//! r.md #50: 画面右端に常駐するマスターパネル。
//!
//! Mixer 右端にあった MASTER ストリップをここへ移設し、アレンジでも Mixer でも
//! MIDI エディタでも**常に**同じものが見えるようにする。加えて曲の音を視覚で
//! 追える 4 セクションを縦に積む:
//!
//! ```text
//! MASTER      フェーダー + L/R メーター (塗り=VU / 細線=ピーク / 数値=最大ピーク)
//!             + ラウドネス (LU バー + M/S/I/LRA/TP の数値 + Reset)
//! スペクトラム  20Hz-20kHz 対数軸
//! オシロ       トリガ付き波形 (L/R 2 色)
//! ステレオ     ゴニオ + 位相相関 + 幅 / 左右バランス
//! ```
//!
//! 表示値はすべて `app.transport.master_meter` (テレメトリスレッドの
//! `MasterAnalyzer` が作ったスナップショット) から読むだけで、ここでは弾道も
//! 平滑も一切かけない (規格準拠の測定は解析器が SSoT)。
//!
//! 設定は各メーターの**右クリック**で変える (`AppEvent::SetMeterSettings`)。

use daw_ui_core::{
    CorrelationStyle, Edit, GoniometerStyle, LevelMeterStyle, LoudnessMeterStyle, MeterBallistic,
    MeterScale, OscilloscopeStyle, SpectrumStyle, ToggleButtonStyle, Ui,
};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::handler::master_panel::{
    MASTER_PANEL_MAX_W, MASTER_PANEL_MIN_W, MASTER_SECTION_MIN_H, section_heights, section_ratios,
};
use crate::master_meter::settings::{
    LoudnessScale, LoudnessUnits, MeterSettings, ScopeTrigger, SpectrumFft, SpectrumWindow,
};
use crate::master_meter::spectrum::{F_MAX, F_MIN};

/// パネル左端のリサイズ帯 (px)。
const RESIZE_HANDLE_W: f32 = 5.0;
/// セクション間の境界 (ドラッグで高さ配分を変える) の高さ (px)。
const SECTION_HANDLE_H: f32 = 5.0;
/// パネル内側の余白 (px)。
const PAD: f32 = 6.0;
/// セクション見出しの高さ (px)。
const HEADER_H: f32 = 14.0;
const HEADER_FONT: f32 = 10.0;
/// 数値読み出しの font / 行高。
const READ_FONT: f32 = 10.0;
const READ_LINE_H: f32 = 13.0;
/// フェーダー列の幅 (fader 18 + gap 2 + meter 35 = mixer strip と同じ内訳)。
const FADER_W: f32 = 18.0;
const FADER_GROUP_W: f32 = 55.0;
/// ラウドネス LU バーの幅 (バー + 目盛り数字)。
const LOUDNESS_BAR_W: f32 = 46.0;
/// 数値読み出し列に最低限必要な幅。
const READOUT_MIN_W: f32 = 84.0;
/// Reset ボタンの高さ。
const RESET_BTN_H: f32 = 16.0;
/// 相関バーの高さ。
const CORRELATION_H: f32 = 12.0;

/// セクションの並び (見出しラベルは描画順と 1:1)。
const SECTION_TITLES: [&str; 4] = ["MASTER", "スペクトラム", "オシロスコープ", "ステレオ"];

/// パネルが占める幅 (閉じているときは 0)。`root.rs` のレイアウト計算が使う。
#[must_use]
pub fn panel_width(app: &AppData) -> f32 {
    if app.ui_prefs.master_panel_open {
        app.ui_prefs
            .master_panel_w
            .clamp(MASTER_PANEL_MIN_W, MASTER_PANEL_MAX_W)
    } else {
        0.0
    }
}

/// 右クリックメニューの安定 id (パネル内 5 か所)。
///
/// `context_menu_for` は popup id を **rect 座標から** 作るので、パネル幅や
/// セクション高が変わると id が変わり、開いていた popup が `open_popups` に
/// 取り残される (見えないのに click / drag を食う領域が残り、
/// `has_open_popups()` が永久に true になって他 view のガードまで壊す)。
/// 座標に依らない id を自分で与えて `context_menu_at` を使う。
const MENU_IDS: [&str; 5] = [
    "master_panel_menu_level",
    "master_panel_menu_loudness",
    "master_panel_menu_spectrum",
    "master_panel_menu_scope",
    "master_panel_menu_gonio",
];

pub fn draw<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect) {
    if !app.ui_prefs.master_panel_open || rect.w < 1.0 {
        // パネルを閉じたら、開いたままのメニューも一緒に畳む
        // (描かれない popup が残ると不可視の入力デッドゾーンになる)。
        for id in MENU_IDS {
            ui.close_popup(id);
        }
        return;
    }
    let p = &app.theme.core;
    ui.panel("master_panel_bg", rect, p.panel_raised, 0.0);

    // ----- 左端のリサイズ帯 -----
    // popup が開いている間は掴まない。掴むと press を消費してしまい、
    // 「メニュー外クリックで閉じる」が効かなくなる (= 見えない popup が残る)。
    let handle = Rect { x: rect.x, y: rect.y, w: RESIZE_HANDLE_W, h: rect.h };
    if !ui.has_open_popups()
        && let Some(drag) = ui.take_drag_in_rect("master_panel_resize", handle)
    {
        // 左へ動かすほど広くなる (右端固定パネル)。
        let next = rect.x + rect.w - drag.current.0;
        let commit = matches!(drag.kind, daw_ui_core::DragKind::Released);
        // ドラッグ中は帯から離れても矢印のまま (掴んでいる間の形を保つ)。
        ui.set_cursor(daw_ui_core::CursorIcon::EwResize);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::SetMasterPanelWidth { w: next, commit });
        }));
    }
    if ui
        .pointer()
        .pos
        .is_some_and(|(x, y)| handle.contains(x, y))
    {
        ui.set_cursor(daw_ui_core::CursorIcon::EwResize);
    }
    ui.push_rect(RectCommand {
        rect: Rect { x: rect.x, y: rect.y, w: 1.0, h: rect.h },
        fill: p.border,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    let content = Rect {
        x: rect.x + RESIZE_HANDLE_W,
        y: rect.y,
        w: (rect.w - RESIZE_HANDLE_W).max(1.0),
        h: rect.h,
    };
    // セクション 4 つ + 境界 3 本。
    let avail = (content.h - SECTION_HANDLE_H * 3.0).max(1.0);
    let heights = section_heights(app.ui_prefs.master_panel_sections, avail);
    let total: f32 = heights.iter().sum::<f32>() + SECTION_HANDLE_H * 3.0;
    if total > content.h + 0.5 {
        // 画面が低すぎて最低高が入らない: 縦スクロールで全部見せる。
        ui.scroll_area(
            "master_panel_scroll",
            content,
            (content.w, total),
            |ui, offset| {
                let inner = Rect { y: content.y - offset.1, h: total, ..content };
                draw_sections(app, ui, inner, heights, avail);
            },
        );
    } else {
        draw_sections(app, ui, content, heights, avail);
    }
}

fn draw_sections<'a>(
    app: &'a AppData,
    ui: &mut Ui<'a, AppData>,
    content: Rect,
    heights: [f32; 4],
    avail: f32,
) {
    let mut y = content.y;
    for i in 0..4 {
        let sect = Rect { x: content.x, y, w: content.w, h: heights[i] };
        draw_section_header(app, ui, sect, i);
        let body = Rect {
            x: sect.x + PAD,
            y: sect.y + HEADER_H,
            w: (sect.w - PAD * 2.0).max(1.0),
            h: (sect.h - HEADER_H - 2.0).max(1.0),
        };
        match i {
            0 => draw_master_section(app, ui, body),
            1 => draw_spectrum_section(app, ui, body),
            2 => draw_scope_section(app, ui, body),
            _ => draw_stereo_section(app, ui, body),
        }
        y += heights[i];
        if i < 3 {
            draw_section_handle(app, ui, content, y, i, heights, avail);
            y += SECTION_HANDLE_H;
        }
    }
}

fn draw_section_header<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, sect: Rect, i: usize) {
    ui.label_at(
        ("master_panel_header", i),
        SECTION_TITLES[i],
        sect.x + PAD,
        sect.y + 1.0,
        HEADER_FONT,
        app.theme.core.text_dim,
    );
}

/// セクション境界。上下ドラッグで隣り合う 2 セクションの配分を移す。
#[allow(clippy::too_many_arguments)]
fn draw_section_handle<'a>(
    app: &'a AppData,
    ui: &mut Ui<'a, AppData>,
    content: Rect,
    y: f32,
    i: usize,
    heights: [f32; 4],
    avail: f32,
) {
    let handle = Rect { x: content.x, y, w: content.w, h: SECTION_HANDLE_H };
    let p = &app.theme.core;
    ui.push_rect(RectCommand {
        rect: Rect { x: handle.x + PAD, y: y + 2.0, w: (handle.w - PAD * 2.0).max(1.0), h: 1.0 },
        fill: p.border,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    if ui.pointer().pos.is_some_and(|(px, py)| handle.contains(px, py)) {
        ui.set_cursor(daw_ui_core::CursorIcon::NsResize);
    }
    if ui.has_open_popups() {
        return;
    }
    let Some(drag) = ui.take_drag_in_rect(("master_panel_section", i), handle) else {
        return;
    };
    // ドラッグ中は境界から離れても形を保つ。
    ui.set_cursor(daw_ui_core::CursorIcon::NsResize);
    let commit = matches!(drag.kind, daw_ui_core::DragKind::Released);
    // ドラッグ量ぶんを i と i+1 の間で移す。実高で計算してから
    // `section_ratios` (= `section_heights` の逆写像) で比率へ戻す。
    // 単純な正規化で戻すと最低高の下駄が二重に効いて、掴んだ境界がカーソルに
    // 追従せず、触っていないセクションまで毎フレーム動く。
    let dy = drag.current.1 - (y + SECTION_HANDLE_H * 0.5);
    if dy.abs() < 0.5 && !commit {
        return;
    }
    let mut next = heights;
    next[i] = (next[i] + dy).max(MASTER_SECTION_MIN_H[i]);
    next[i + 1] = (next[i + 1] - dy).max(MASTER_SECTION_MIN_H[i + 1]);
    let ratios = section_ratios(next, avail);
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::SetMasterPanelSectionRatios { ratios, commit });
    }));
}

// =====================================================================
// MASTER (フェーダー + メーター + ラウドネス)
// =====================================================================

fn draw_master_section<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, body: Rect) {
    let m = &app.transport.master_meter;
    let p = &app.theme.core;
    let settings = app.ui_prefs.meter_settings;

    // ---- フェーダー + L/R メーター ----
    let scale = MeterScale::default();
    let style = LevelMeterStyle {
        scale: Some(scale),
        peak_readout: true,
        peak_hold_ms: u128::from(settings.peak_hold_ms),
        // 0 VU のアライメント線 (EBU R68 = -18 dBFS / SMPTE RP155 = -20 dBFS)。
        // バーは dBFS 目盛りのまま = ピークと VU が同じ写像を共有する、という
        // 二重表示の前提を崩さずに「0 VU がどこか」を示す。
        reference_db: Some(settings.vu_reference_dbfs),
        ..LevelMeterStyle::from_palette(p)
    };
    let master_gain = app.song_doc.song().master_gain;
    let fader_db = if master_gain <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * master_gain.log10()
    };
    let fader_rect = Rect { x: body.x, y: body.y, w: FADER_GROUP_W, h: body.h };
    let long_peak = if m.peak_max_db.is_finite() {
        10f32.powf(m.peak_max_db / 20.0)
    } else {
        0.0
    };
    let resp = ui.channel_fader_meter(
        "master_panel_fader",
        fader_rect,
        FADER_W,
        fader_db,
        0.0,
        // 塗り = VU。細線 = ピーク。保持線と数値も解析器の値。
        m.vu[0],
        m.vu[1],
        MeterBallistic::Direct {
            overlay: (m.peak[0], m.peak[1]),
            hold: (m.peak_hold[0], m.peak_hold[1]),
            long_peak,
        },
        style,
        |new_db| {
            let amp = if new_db.is_finite() { 10f32.powf(new_db / 20.0) } else { 0.0 };
            Edit::mutate(move |app: &mut AppData| app.handle_event(AppEvent::SetMasterGain(amp)))
        },
        None,
    );
    // drag の立ち上がり / 立ち下がりで undo gesture を開閉する。`master_gain` は
    // `Song` に入って undo 対象になったので、これが無いと 1 回のドラッグで
    // per-frame の編集が undo 履歴を埋める。was_dragging は 1 frame 遅れで
    // 追従する (param_gesture と同じ edge 検出の連鎖)。
    if resp.fader.dragging != app.ui_ephemeral.master_gain_dragging {
        let started = resp.fader.dragging;
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(if started {
                AppEvent::BeginMasterGainDrag
            } else {
                AppEvent::EndMasterGainDrag
            });
        }));
    }
    if resp.peak_reset {
        ui.push_edit(Edit::mutate(|app: &mut AppData| {
            app.handle_event(AppEvent::ResetMasterPeakHold);
        }));
    }
    level_context_menu(ui, fader_rect, settings);

    // ---- ラウドネス ----
    let rest_x = body.x + FADER_GROUP_W + 8.0;
    let rest_w = (body.x + body.w - rest_x).max(0.0);
    if rest_w < READOUT_MIN_W {
        return;
    }
    let (bar_w, read_x, read_w) = if rest_w >= LOUDNESS_BAR_W + 6.0 + READOUT_MIN_W {
        (
            LOUDNESS_BAR_W,
            rest_x + LOUDNESS_BAR_W + 6.0,
            rest_w - LOUDNESS_BAR_W - 6.0,
        )
    } else {
        (0.0, rest_x, rest_w)
    };
    let loudness_rect = Rect { x: rest_x, y: body.y, w: rest_w, h: body.h };
    if bar_w > 0.0 {
        let target = settings.loudness_target_lufs;
        let lu = |v: f32| if v.is_finite() { v - target } else { f32::NEG_INFINITY };
        let style = LoudnessMeterStyle {
            range_lu: settings.loudness_scale.range_lu(),
            ..LoudnessMeterStyle::from_palette(p)
        };
        ui.loudness_meter(
            "master_panel_loudness",
            Rect { x: rest_x, y: body.y, w: bar_w, h: body.h },
            lu(m.loudness.short_term_lufs),
            lu(m.loudness.momentary_lufs),
            &style,
        );
    }
    draw_loudness_readout(app, ui, Rect { x: read_x, y: body.y, w: read_w, h: body.h });
    loudness_context_menu(ui, loudness_rect, settings);
}

/// M / S / I / LRA / TP の数値 + クリップ表示 + Reset。
fn draw_loudness_readout<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect) {
    let m = &app.transport.master_meter;
    let p = &app.theme.core;
    let s = app.ui_prefs.meter_settings;
    let l = &m.loudness;
    // EBU Tech 3341 §2.2 は M / S の**最大値**の表示も要求する (Integrated の
    // リセットと同時に畳まれる)。dim で並べて現在値と見分けられるようにする。
    let rows: [(&str, String, Color); 7] = [
        ("M", fmt_loudness(l.momentary_lufs, s), p.text),
        ("M max", fmt_loudness(l.max_momentary_lufs, s), p.text_dim),
        ("S", fmt_loudness(l.short_term_lufs, s), p.text),
        ("S max", fmt_loudness(l.max_short_term_lufs, s), p.text_dim),
        ("I", fmt_loudness(l.integrated_lufs, s), p.text),
        (
            "LRA",
            if l.lra_provisional && l.lra_lu > 0.0 {
                format!("{:.1} LU*", l.lra_lu)
            } else {
                format!("{:.1} LU", l.lra_lu)
            },
            if l.lra_provisional { p.text_dim } else { p.text },
        ),
        (
            "TP",
            if m.max_true_peak_dbtp.is_finite() {
                format!("{:.1} dBTP", m.max_true_peak_dbtp)
            } else {
                "-inf".to_string()
            },
            // EBU R128 の上限 -1 dBTP を超えたら警告色。
            if m.max_true_peak_dbtp > -1.0 { app.theme.daw.record } else { p.text },
        ),
    ];
    let mut y = rect.y;
    for (i, (label, value, color)) in rows.iter().enumerate() {
        if y + READ_LINE_H > rect.y + rect.h {
            break;
        }
        ui.label_at(("mp_lbl", i), label, rect.x, y, READ_FONT, p.text_dim);
        let vw = ui.measure_text(value, READ_FONT);
        ui.label_at(
            ("mp_val", i),
            value,
            rect.x + rect.w - vw,
            y,
            READ_FONT,
            *color,
        );
        y += READ_LINE_H;
    }

    // クリップ表示 (0 dBFS 到達)。クリックでリセット。
    if y + RESET_BTN_H <= rect.y + rect.h {
        let clip_w = (rect.w * 0.45).min(60.0);
        let clip_rect = Rect { x: rect.x, y: y + 2.0, w: clip_w, h: RESET_BTN_H };
        let clipped = m.clip_count > 0;
        let text = if clipped { format!("CLIP {}", m.clip_count) } else { "CLIP".into() };
        // 点灯はユーザが決める state ではなく音の観測結果、 click は反転でなく常に
        // クリアなので toggle ではなく `indicator_button_at`。 手書きの rect + label +
        // press 判定だと hover も押下も出ず、 隣の Reset と作法が揃わなかった。
        ui.indicator_button_at(
            "mp_clip",
            &text,
            clip_rect,
            clipped,
            &clip_indicator_style(&app.theme),
            || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ResetMasterPeakHold)),
        );
        // Reset (ラウドネス積算を同時リセット)。
        let btn_x = clip_rect.x + clip_rect.w + 4.0;
        let btn_w = (rect.x + rect.w - btn_x).max(0.0);
        if btn_w >= 40.0 {
            // `button_at` は 16px 固定で、隣の CLIP ラベルや上の読み値 (10px) に対して
            // 大きすぎる。同じ帯に並ぶものは同じ font に揃える。
            ui.button_at_sized(
                "mp_reset_loudness",
                "Reset",
                Rect { x: btn_x, y: clip_rect.y, w: btn_w, h: RESET_BTN_H },
                READ_FONT,
                || Edit::mutate(|app: &mut AppData| app.handle_event(AppEvent::ResetLoudness)),
            );
        }
    }
}

/// CLIP 表示の見た目 (Mixer の Rec = `style_rec` と同じ流儀)。
/// 点灯 = クリップ検出で赤背景、 消灯は通常のボタン面。
fn clip_indicator_style(theme: &crate::theme::Theme) -> ToggleButtonStyle {
    // `on_text_color` は None のまま = 点灯塗りの輝度から auto-contrast (r.md #48 の契約)。
    // 手で `ink_for` を書くと同じ計算の重複になる。
    ToggleButtonStyle {
        on_color: theme.daw.record,
        radius: 3.0,
        font_size: READ_FONT,
        ..ToggleButtonStyle::from_palette(&theme.core)
    }
}

/// ラウドネス値を設定中の単位で整形する。
fn fmt_loudness(v: f32, s: MeterSettings) -> String {
    if !v.is_finite() {
        return "-inf".to_string();
    }
    match s.loudness_units {
        LoudnessUnits::Lufs => format!("{v:.1} LUFS"),
        LoudnessUnits::Lu => format!("{:+.1} LU", v - s.loudness_target_lufs),
    }
}

// =====================================================================
// スペクトラム / オシロ / ステレオ
// =====================================================================

fn draw_spectrum_section<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, body: Rect) {
    let m = &app.transport.master_meter;
    let s = app.ui_prefs.meter_settings;
    let style = SpectrumStyle {
        floor_db: -s.spectrum_range_db,
        f_min: F_MIN,
        f_max: F_MAX,
        show_labels: body.h >= 60.0,
        ..SpectrumStyle::from_palette(&app.theme.core)
    };
    let hold: &[f32] = if s.spectrum_peak_hold { &m.spectrum_hold_db } else { &[] };
    ui.spectrum_analyzer("master_panel_spectrum", body, &m.spectrum_db, hold, &style);
    spectrum_context_menu(ui, body, s);
}

fn draw_scope_section<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, body: Rect) {
    let m = &app.transport.master_meter;
    let style = OscilloscopeStyle::from_palette(&app.theme.core);
    ui.oscilloscope("master_panel_scope", body, &m.scope, &style);
    scope_context_menu(ui, body, app.ui_prefs.meter_settings);
}

fn draw_stereo_section<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, body: Rect) {
    let m = &app.transport.master_meter;
    let p = &app.theme.core;
    let s = app.ui_prefs.meter_settings;
    // 下に相関バー + 幅/バランスの数値、残りをゴニオ (正方形)。
    let bottom_h = CORRELATION_H + READ_LINE_H + 4.0;
    let gonio_h = (body.h - bottom_h).max(20.0);
    let gonio_side = gonio_h.min(body.w);
    let gonio = Rect {
        x: body.x + (body.w - gonio_side) * 0.5,
        y: body.y,
        w: gonio_side,
        h: gonio_h,
    };
    let gstyle = GoniometerStyle {
        persistence: s.gonio_persistence,
        ..GoniometerStyle::from_palette(p)
    };
    ui.goniometer("master_panel_gonio", gonio, &m.gonio, m.seq, &gstyle);
    gonio_context_menu(ui, gonio, s);

    let bar = Rect {
        x: body.x,
        y: body.y + gonio_h + 2.0,
        w: body.w,
        h: CORRELATION_H,
    };
    ui.correlation_meter(
        "master_panel_correlation",
        bar,
        m.stereo.correlation,
        m.stereo.correlation_min,
        m.stereo.correlation_max,
        &CorrelationStyle::from_palette(p),
    );
    // 相関は小数 3 桁。モノ寄りの素材は +0.998 と +1.000 のどちらにもなるが、
    // 2 桁だと両方 "+1.00" に丸まって「完全モノか、ほぼモノか」が消える
    // (この 2 つは Side/Mid で 20dB 以上違う)。幅も 0.0% と 2.9% を潰さないよう
    // 小数 1 桁にする。
    let text = format!(
        "相関 {:+.3}   幅 {:.1}%   バランス {:+.1} dB",
        m.stereo.correlation,
        m.stereo.width * 100.0,
        m.stereo.balance_db
    );
    let ty = bar.y + bar.h + 2.0;
    if ty + READ_LINE_H <= body.y + body.h {
        ui.label_at("mp_stereo_read", &text, body.x, ty, READ_FONT, p.text_dim);
    }
}

// =====================================================================
// 右クリックメニュー (設定はここでしか変えない)
// =====================================================================

/// 選択中の項目に ✓ を付けたラベルを作る。
fn checked(label: &str, on: bool) -> String {
    if on { format!("\u{2713} {label}") } else { format!("  {label}") }
}

/// `rect` 内の右クリックで `MENU_IDS[slot]` のメニューを開く。
///
/// `context_menu_for` (rect 由来 id) ではなく安定 id の `context_menu_at` を
/// 使うのが要点 ([`MENU_IDS`] の説明を参照)。
fn menu_at<F>(ui: &mut Ui<'_, AppData>, slot: usize, rect: Rect, items: &[&str], on_select: F)
where
    F: for<'ui> FnOnce(usize, &mut Ui<'ui, AppData>),
{
    let open_at = ui.take_secondary_click_in_rect(rect);
    ui.context_menu_at(MENU_IDS[slot], open_at, items, on_select);
}

fn push_settings(ui: &mut Ui<'_, AppData>, next: MeterSettings) {
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::SetMeterSettings(Box::new(next)));
    }));
}

fn level_context_menu(ui: &mut Ui<'_, AppData>, rect: Rect, s: MeterSettings) {
    let labels = vec![
        checked("0 VU = -18 dBFS (EBU)", (s.vu_reference_dbfs + 18.0).abs() < 0.01),
        checked("0 VU = -20 dBFS (SMPTE)", (s.vu_reference_dbfs + 20.0).abs() < 0.01),
        checked("落下 8.6 dB/s (BBC)", (s.peak_fall_db_per_s - 8.6).abs() < 0.01),
        checked("落下 13.3 dB/s", (s.peak_fall_db_per_s - 13.3).abs() < 0.01),
        checked("落下 20 dB/s", (s.peak_fall_db_per_s - 20.0).abs() < 0.01),
        checked("ピーク保持 1.5 秒", s.peak_hold_ms == 1500),
        checked("ピーク保持 5 秒", s.peak_hold_ms == 5000),
        checked("ピーク保持 なし", s.peak_hold_ms == 0),
        "ピーク / クリップをリセット".to_string(),
    ];
    let items: Vec<&str> = labels.iter().map(String::as_str).collect();
    menu_at(ui, 0, rect, &items, move |idx, ui| {
        let mut next = s;
        match idx {
            0 => next.vu_reference_dbfs = -18.0,
            1 => next.vu_reference_dbfs = -20.0,
            2 => next.peak_fall_db_per_s = 8.6,
            3 => next.peak_fall_db_per_s = 13.3,
            4 => next.peak_fall_db_per_s = 20.0,
            5 => next.peak_hold_ms = 1500,
            6 => next.peak_hold_ms = 5000,
            7 => next.peak_hold_ms = 0,
            _ => {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ResetMasterPeakHold);
                }));
                return;
            }
        }
        push_settings(ui, next);
    });
}

fn loudness_context_menu(ui: &mut Ui<'_, AppData>, rect: Rect, s: MeterSettings) {
    let mut labels = vec![
        checked("目標 -14 LUFS (配信)", (s.loudness_target_lufs + 14.0).abs() < 0.01),
        checked("目標 -16 LUFS (Apple Music)", (s.loudness_target_lufs + 16.0).abs() < 0.01),
        checked("目標 -23 LUFS (EBU R128)", (s.loudness_target_lufs + 23.0).abs() < 0.01),
    ];
    for sc in LoudnessScale::ALL {
        labels.push(checked(sc.label(), s.loudness_scale == sc));
    }
    for u in LoudnessUnits::ALL {
        labels.push(checked(u.label(), s.loudness_units == u));
    }
    labels.push("積算をリセット".to_string());
    let items: Vec<&str> = labels.iter().map(String::as_str).collect();
    menu_at(ui, 1, rect, &items, move |idx, ui| {
        let mut next = s;
        match idx {
            0 => next.loudness_target_lufs = -14.0,
            1 => next.loudness_target_lufs = -16.0,
            2 => next.loudness_target_lufs = -23.0,
            3 | 4 => next.loudness_scale = LoudnessScale::ALL[idx - 3],
            5 | 6 => next.loudness_units = LoudnessUnits::ALL[idx - 5],
            _ => {
                ui.push_edit(Edit::mutate(|app: &mut AppData| {
                    app.handle_event(AppEvent::ResetLoudness);
                }));
                return;
            }
        }
        push_settings(ui, next);
    });
}

/// スペクトラムメニューの傾き候補 [dB/oct]。
const SLOPES: [f32; 4] = [0.0, 3.0, 4.5, 6.0];
/// 表示レンジ候補 [dB]。
const RANGES: [f32; 4] = [60.0, 90.0, 100.0, 120.0];
/// リリース候補 [ms] (20 dB 落ちる時間)。
const RELEASES: [u32; 4] = [100, 300, 600, 1500];

fn spectrum_context_menu(ui: &mut Ui<'_, AppData>, rect: Rect, s: MeterSettings) {
    let mut labels = Vec::new();
    for f in SpectrumFft::ALL {
        labels.push(checked(&format!("FFT {}", f.label()), s.spectrum_fft == f));
    }
    for w in SpectrumWindow::ALL {
        labels.push(checked(w.label(), s.spectrum_window == w));
    }
    for sl in SLOPES {
        labels.push(checked(
            &format!("傾き {sl:.1} dB/oct"),
            (s.spectrum_slope_db_oct - sl).abs() < 0.01,
        ));
    }
    for r in RANGES {
        labels.push(checked(
            &format!("レンジ {r:.0} dB"),
            (s.spectrum_range_db - r).abs() < 0.01,
        ));
    }
    for ms in RELEASES {
        labels.push(checked(
            &format!("落ち {ms} ms / 20 dB"),
            s.spectrum_release_ms == ms,
        ));
    }
    labels.push(checked("ピーク保持線", s.spectrum_peak_hold));
    let items: Vec<&str> = labels.iter().map(String::as_str).collect();
    // 群ごとの開始 index (ラベルを積んだ順と 1:1)。
    let fft0 = 0;
    let win0 = fft0 + SpectrumFft::ALL.len();
    let slope0 = win0 + SpectrumWindow::ALL.len();
    let range0 = slope0 + SLOPES.len();
    let rel0 = range0 + RANGES.len();
    let hold0 = rel0 + RELEASES.len();
    menu_at(ui, 2, rect, &items, move |idx, ui| {
        let mut next = s;
        if idx < win0 {
            next.spectrum_fft = SpectrumFft::ALL[idx - fft0];
        } else if idx < slope0 {
            next.spectrum_window = SpectrumWindow::ALL[idx - win0];
        } else if idx < range0 {
            next.spectrum_slope_db_oct = SLOPES[idx - slope0];
        } else if idx < rel0 {
            next.spectrum_range_db = RANGES[idx - range0];
        } else if idx < hold0 {
            next.spectrum_release_ms = RELEASES[idx - rel0];
        } else {
            next.spectrum_peak_hold = !next.spectrum_peak_hold;
        }
        push_settings(ui, next);
    });
}

/// オシロの表示窓候補 [ms]。
const SCOPE_WINDOWS: [f32; 5] = [5.0, 10.0, 20.0, 50.0, 100.0];

fn scope_context_menu(ui: &mut Ui<'_, AppData>, rect: Rect, s: MeterSettings) {
    let mut labels = Vec::new();
    for w in SCOPE_WINDOWS {
        labels.push(checked(
            &format!("表示幅 {w:.0} ms"),
            (s.scope_window_ms - w).abs() < 0.01,
        ));
    }
    for t in ScopeTrigger::ALL {
        labels.push(checked(t.label(), s.scope_trigger == t));
    }
    let items: Vec<&str> = labels.iter().map(String::as_str).collect();
    menu_at(ui, 3, rect, &items, move |idx, ui| {
        let mut next = s;
        if idx < SCOPE_WINDOWS.len() {
            next.scope_window_ms = SCOPE_WINDOWS[idx];
        } else {
            next.scope_trigger = ScopeTrigger::ALL[idx - SCOPE_WINDOWS.len()];
        }
        push_settings(ui, next);
    });
}

/// ゴニオの残光候補。
const PERSISTENCE: [(f32, &str); 4] =
    [(0.70, "残光 短"), (0.85, "残光 中"), (0.90, "残光 長"), (0.96, "残光 とても長い")];

fn gonio_context_menu(ui: &mut Ui<'_, AppData>, rect: Rect, s: MeterSettings) {
    let labels: Vec<String> = PERSISTENCE
        .iter()
        .map(|(v, l)| checked(l, (s.gonio_persistence - v).abs() < 0.01))
        .collect();
    let items: Vec<&str> = labels.iter().map(String::as_str).collect();
    menu_at(ui, 4, rect, &items, move |idx, ui| {
        let mut next = s;
        next.gonio_persistence = PERSISTENCE[idx.min(PERSISTENCE.len() - 1)].0;
        push_settings(ui, next);
    });
}
