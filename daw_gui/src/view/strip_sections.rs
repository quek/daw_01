//! mixer strip の上に積む内蔵チャンネルストリップ帯 (コンプ + EQ)。
//!
//! 設計正本は [docs/plan_channel_strip.md](../../../docs/plan_channel_strip.md)。
//! 既存 strip (名前 / M・S / Pan / Fader / Sends) には一切触らず、**その上に**
//! 3 つの帯を積む:
//!
//! ```text
//! +----------+  Comp セクション (開いているときだけ)
//! +----------+  EQ セクション   (開いているときだけ)
//! +----------+  常設サムネイル帯 (GR バー + EQ カーブ、常に見える)
//! | Name ... |  ← ここから下が既存 strip
//! ```
//!
//! 上から下が信号順 (`inserts → Comp → EQ → Pan → Fader`)。開閉は **全 ch 一括**
//! (`UiPrefs::strip_comp_open` / `strip_eq_open`) で、サムネイル帯の GR バー /
//! カーブのクリックがそのトグルを兼ねる。
//!
//! EQ カーブは [`common::channel_strip_dsp`] の振幅応答をそのまま描く — 音を出す
//! daw_audio と同じ関数なので、画面の線と実際の音が食い違わない。

use std::sync::Arc;

use common::automation::{norm_to_plain, plain_to_norm};
use common::channel_strip_dsp::{eq_magnitude_db, eq_stages};
use common::model::{
    AutomationTarget, ChannelStrip, CompMode, CompParam, EqBand, EqParam, GR_METER_RANGE_DB,
    TrackBuiltinParam,
};
use daw_ui_core::{Edit, KnobStyle, ToggleButtonStyle, Ui};
use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::app::{AppData, AppEvent, ModControlDomain};
use crate::automation_value::automation_value_display;
use crate::event::{StripEdit, StripSection, StripSwitch};
use crate::theme::Theme;
use crate::view::modulation::{build_mod, push_mod_depth_bracket};
use crate::view::param_gesture::push_param_gesture_edges;

/// 常設サムネイル帯の高さ (px)。折り畳んでいてもここだけは必ず出る。
pub const THUMB_H: f32 = 28.0;
/// ノブ 1 個の直径 (px)。3 個並べて strip 内側 68px に収まる最大。
const KNOB: f32 = 20.0;
/// ノブ同士の間隔。
const KNOB_GAP: f32 = 4.0;
/// 各行のラベル / hover 読み出し行の高さ (= [`LABEL_FONT`] の行高)。
const LABEL_H: f32 = 12.0;
/// ラベル / hover 読み出しの font size。80px strip でも値 (`Freq 2500Hz`) が
/// 読める下限。8px では読めないという指摘で 10px に上げた。
const LABEL_FONT: f32 = 10.0;
/// ノブ行の高さ = ラベル行 + ノブ + 隙間。
const ROW_H: f32 = LABEL_H + KNOB + 2.0;
/// 行内の小ボタン (モード切替 / スイッチ) の font size。
const SWITCH_FONT: f32 = 9.0;
/// コンプのモード切替行の高さ。
const MODE_ROW_H: f32 = 18.0;
/// コンプの GR メーター行の高さ。
const GR_ROW_H: f32 = 12.0;
/// セクション内の上下余白。
const SECTION_PAD: f32 = 2.0;
/// 行内の小スイッチ (ON / BELL / Listen) の一辺 (px)。
/// **どの行でも同じ正方形**にする — 大きさが揃っていないと「ボタンなのか
/// ただの表示なのか」が読めない。文字は 1 文字だけ乗せる。
const SWITCH_W: f32 = 14.0;

/// Comp セクションの高さ = モード行 + 3 ノブ行 + GR 行。
const COMP_H: f32 = MODE_ROW_H + 2.0 + ROW_H * 3.0 + GR_ROW_H + SECTION_PAD * 2.0;
/// EQ セクションの高さ = HP 行 + LP 行 + 4 バンド行。
/// フィルタを 1 行ずつに割ったのは、ON スイッチを他の行と同じ正方形で置くため
/// (2 段重ねだと行高 20px に 14px の正方形が 2 つ入らない)。
const EQ_H: f32 = ROW_H * 6.0 + SECTION_PAD * 2.0;

/// EQ カーブの縦軸レンジ (±dB)。
const CURVE_DB_RANGE: f32 = 18.0;
/// EQ カーブの横軸下端 (Hz)。
const CURVE_F_MIN: f32 = 20.0;
/// EQ カーブの横軸上端 (Hz)。
const CURVE_F_MAX: f32 = 20_000.0;
/// カーブのサンプル点数 (帯の幅 32px に対して 1px あたり 1 点強)。
const CURVE_POINTS: usize = 40;
/// カーブ描画に使うサンプリング周波数。**音の実 SR ではない** — 描くのは
/// 20Hz〜20kHz の応答なので、48kHz 固定で描いても可聴域の形は変わらない
/// (係数の bilinear warping の差が出るのは Nyquist 付近だけ)。
const CURVE_SR: f32 = 48_000.0;

/// 1 本の strip を描く間ずっと変わらない引数の束。
///
/// セクション / 行 / ノブへ同じものを配り歩くので、束ねて渡す。
struct StripCtx<'a> {
    app: &'a AppData,
    /// 住所であり、widget id の分離キーでもある (安定 id、不変条件 1)。
    track_id: u32,
    /// 描画時点の設定値 (`Song` から 1 度だけ読む)。
    strip: ChannelStrip,
    /// この strip の背景色 (= ノブが「載っている面」の色)。
    bg: Color,
    /// 正の減衰量 dB (0 = 掛かっていない)。
    gain_reduction_db: f32,
}

/// この strip 上端に積む帯の総高 (px)。`mixer_strips` が既存 strip の開始 y を
/// 決めるのに使い、`root` が下ペインの必要高を見積もるのにも使う (SSoT)。
#[must_use]
pub fn head_height(app: &AppData) -> f32 {
    THUMB_H
        + if app.ui_prefs.strip_comp_open { COMP_H } else { 0.0 }
        + if app.ui_prefs.strip_eq_open { EQ_H } else { 0.0 }
}

/// 帯を描く。`rect` は strip 全体の矩形で、上端から [`head_height`] 分を使う。
pub fn draw_head(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    track_id: u32,
    rect: Rect,
    pad: f32,
    bg: Color,
    gain_reduction_db: f32,
) {
    let Some(track) = app.song_doc.song().track_by_id(track_id) else {
        return;
    };
    let ctx = StripCtx {
        app,
        track_id,
        strip: track.strip,
        bg,
        gain_reduction_db,
    };
    let inner = Rect {
        x: rect.x + pad,
        y: rect.y,
        w: (rect.w - pad * 2.0).max(1.0),
        h: rect.h,
    };
    let mut y = rect.y;

    // Q キー (= 「カーソル直下のものを無効化」) の対象面。セクション本体と
    // 常設帯の両方が対象で、算出はここ 1 か所 (`mixer_hovered_track` と同 idiom)。
    let ptr = ui.pointer().pos;
    let mut hovered: Option<StripSection> = None;
    let hit = |r: Rect, sec: StripSection, hovered: &mut Option<StripSection>| {
        if ptr.is_some_and(|(px, py)| r.contains(px, py)) {
            *hovered = Some(sec);
        }
    };

    if app.ui_prefs.strip_comp_open {
        let sect = Rect { y, h: COMP_H, ..inner };
        draw_comp_section(&ctx, ui, sect);
        hit(sect, StripSection::Comp, &mut hovered);
        y += COMP_H;
        separator(ui, app, rect, y);
    }
    if app.ui_prefs.strip_eq_open {
        let sect = Rect { y, h: EQ_H, ..inner };
        draw_eq_section(&ctx, ui, sect);
        hit(sect, StripSection::Eq, &mut hovered);
        y += EQ_H;
        separator(ui, app, rect, y);
    }
    let thumb = Rect { y, h: THUMB_H, ..inner };
    let (gr_hit, eq_hit) = draw_thumbnail(&ctx, ui, thumb);
    hit(gr_hit, StripSection::Comp, &mut hovered);
    hit(eq_hit, StripSection::Eq, &mut hovered);

    publish_hover(app, ui, track_id, hovered);
}

/// カーソル直下のセクションを `AppData` へ反映する (変化時のみ)。
///
/// 自分の strip から外れたときは、**自分が最後に立てた値だったときだけ** 消す
/// (他の strip が立てた値を横から消さない)。
fn publish_hover(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    track_id: u32,
    hovered: Option<StripSection>,
) {
    let current = app.ui_ephemeral.mixer_hovered_strip_section;
    let next = hovered.map(|s| (track_id, s));
    let mine = current.is_some_and(|(t, _)| t == track_id);
    if next == current || (next.is_none() && !mine) {
        return;
    }
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.ui_ephemeral.mixer_hovered_strip_section = next;
    }));
}

/// セクション同士を分ける 1px の区切り線。
fn separator(ui: &mut Ui<'_, AppData>, app: &AppData, rect: Rect, y: f32) {
    ui.push_rect(RectCommand {
        rect: Rect { x: rect.x, y, w: rect.w, h: 1.0 },
        fill: app.theme.core.border,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
}

// ---------------------------------------------------------------------------
// 常設サムネイル帯
// ---------------------------------------------------------------------------

/// 常設帯: **EQ カーブを帯の全幅**に描き、その左端に GR バーを重ねる。
///
/// クリックでそのセクションを開閉する (全 ch 一括)。**バイパスはここでは切らない** —
/// カーソルを乗せて `Q` (= 「直下のものを無効化」の既存キー) が担当する。
/// ダブルクリックに割り当てると、1 回目のクリックで開いて 2 回目で閉じる動きが
/// 必ず先に見えてしまう。
///
/// 戻り値は `(GR バーの当たり判定, EQ カーブの当たり判定)`。
fn draw_thumbnail(ctx: &StripCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) -> (Rect, Rect) {
    const GR_W: f32 = 8.0;
    let body = Rect { x: rect.x, y: rect.y + 2.0, w: rect.w, h: rect.h - 4.0 };

    // ---- EQ カーブ (帯の全幅) ----
    ui.push_rect(RectCommand {
        rect: body,
        fill: band_bg(ctx, ctx.strip.eq.on),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [2.0; 4],
        clip_rect: None,
    });
    draw_eq_curve(ctx, ui, body);

    // ---- GR バー (カーブの上に重ねる) ----
    let gr_rect = Rect { w: GR_W, ..body };
    draw_gr_bar(ctx, ui, gr_rect);

    // 当たり判定は左 8px = コンプ、残り = EQ (描画の重なりと同じ切り分け)。
    let eq_rect = Rect { x: body.x + GR_W, w: (body.w - GR_W).max(1.0), ..body };
    section_toggle_click(ui, gr_rect, StripSection::Comp);
    section_toggle_click(ui, eq_rect, StripSection::Eq);
    (gr_rect, eq_rect)
}

/// 常設帯の面の色。**ON は窪んだ井戸 / OFF は strip と同じ面**にして、
/// 「効いているかどうか」を線の色だけでなく面でも読ませる。
fn band_bg(ctx: &StripCtx<'_>, on: bool) -> Color {
    if on { ctx.app.theme.core.window_bg } else { ctx.bg }
}

/// 常設帯の 1 面をクリックしたらそのセクションを開閉する (全 ch 一括)。
fn section_toggle_click(ui: &mut Ui<'_, AppData>, rect: Rect, section: StripSection) {
    if ui.take_primary_press_in_rect(rect).is_some() {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ToggleStripSection(section))
        }));
    }
}

/// GR メーター (縦)。上から下へ、減衰量ぶん伸びる。
fn draw_gr_bar(ctx: &StripCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) {
    // カーブの上に重なるので、面は必ず自分で塗る (下のカーブを透かさない)。
    ui.panel(("strip_gr_bg", ctx.track_id), rect, band_bg(ctx, ctx.strip.comp.on), 2.0);
    if !ctx.strip.comp.on {
        return;
    }
    let frac = (ctx.gain_reduction_db / GR_METER_RANGE_DB).clamp(0.0, 1.0);
    if frac <= 0.0 {
        return;
    }
    ui.push_rect(RectCommand {
        rect: Rect { h: rect.h * frac, ..rect },
        fill: ctx.app.theme.daw.strip_gr,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [2.0, 2.0, 0.0, 0.0],
        clip_rect: None,
    });
}

/// EQ の合成レスポンスを 1 本の折れ線で描く (HP/LP 含む)。
///
/// 応答値は daw_audio と同じ [`eq_magnitude_db`] から取る。バイパス中も
/// 「フラットな線」を沈んだ色で描く (何も描かないと帯が壊れて見える)。
fn draw_eq_curve(ctx: &StripCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &ctx.app.theme.core;
    let stages = eq_stages(&ctx.strip.eq, CURVE_SR);
    let color = if ctx.strip.eq.on { ctx.app.theme.daw.strip_eq_curve } else { p.text_dim };

    // 0dB の基準線 (カーブが上下どちらへ振れているかを読む物差し)。
    let mid_y = rect.y + rect.h * 0.5;
    ui.push_rect(RectCommand {
        rect: Rect { x: rect.x, y: mid_y, w: rect.w, h: 1.0 },
        fill: p.border,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    let ratio = CURVE_F_MAX / CURVE_F_MIN;
    let mut prev: Option<(f32, f32)> = None;
    let mut segs: Vec<LineSegment> = Vec::with_capacity(CURVE_POINTS);
    for i in 0..=CURVE_POINTS {
        #[allow(clippy::cast_precision_loss)]
        let t = i as f32 / CURVE_POINTS as f32;
        let f = CURVE_F_MIN * ratio.powf(t);
        let db = eq_magnitude_db(&stages, CURVE_SR, f).clamp(-CURVE_DB_RANGE, CURVE_DB_RANGE);
        let x = rect.x + rect.w * t;
        let y = mid_y - (db / CURVE_DB_RANGE) * (rect.h * 0.5 - 1.0);
        if let Some((px, py)) = prev {
            segs.push(LineSegment { a: [px, py], b: [x, y], color });
        }
        prev = Some((x, y));
    }
    ui.push_lines(LineBatch {
        segments: Arc::from(segs),
        line_width_px: 1.0,
        clip_rect: Some(rect),
    });
}

// ---------------------------------------------------------------------------
// Comp セクション
// ---------------------------------------------------------------------------

fn draw_comp_section(ctx: &StripCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let p = &ctx.app.theme.core;
    let track_id = ctx.track_id;
    let mut y = rect.y + SECTION_PAD;

    // ---- モード切替 (3 択) ----
    let gap = 2.0;
    let w = (rect.w - gap * 2.0) / 3.0;
    for (i, mode) in CompMode::ALL.into_iter().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        let x = rect.x + (w + gap) * i as f32;
        let style = ToggleButtonStyle {
            on_color: p.control_active,
            radius: 2.0,
            font_size: SWITCH_FONT,
            ..ToggleButtonStyle::from_palette(p)
        };
        ui.toggle_button_at(
            ("strip_comp_mode", ctx.track_id, i),
            mode.label(),
            Rect { x, y, w, h: MODE_ROW_H },
            ctx.strip.comp.mode == mode,
            &style,
            move |_| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::StripEdit {
                        track: track_id,
                        edit: StripEdit::CompMode(mode),
                    })
                })
            },
        );
    }
    y += MODE_ROW_H + 2.0;

    // ---- ノブ行 ----
    // 行の見出しは静的文字列で持つ (毎フレーム join すると strip の本数だけ
    // String を作ることになる)。
    let rows: [(&[CompParam], &'static str, bool); 3] = [
        (&[CompParam::Threshold, CompParam::Ratio], "Thr Rat", false),
        (&[CompParam::Attack, CompParam::Release], "Atk Rel", false),
        // 検出フィルタの行にだけ SC Listen を置く (聴く対象を決めるツマミの隣)。
        (&[CompParam::ScFreq, CompParam::Makeup], "SC Gain", true),
    ];
    for (row_idx, (params, row_name, with_listen)) in rows.into_iter().enumerate() {
        let row = Rect { y, h: ROW_H, ..rect };
        let switch_w = if with_listen { SWITCH_W + 2.0 } else { 0.0 };
        let start_x = row_start_x(row, params.len(), switch_w);
        let mut hover: Option<String> = None;
        for (i, param) in params.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let x = start_x + (KNOB + KNOB_GAP) * i as f32;
            let readout = strip_knob(
                ctx,
                ui,
                ("strip_comp_knob", ctx.track_id, row_idx, i),
                Rect { x, y: row.y + LABEL_H, w: KNOB, h: KNOB },
                TrackBuiltinParam::StripComp { param: *param },
                param.label(),
                ctx.strip.comp.mode.overrides(*param),
            );
            if readout.is_some() {
                hover = readout;
            }
        }
        if with_listen {
            draw_sc_listen(ctx, ui, row);
        }
        row_label(ctx, ui, ("strip_comp_row_label", ctx.track_id, row_idx), row, row_name, hover);
        y += ROW_H;
    }

    // ---- GR メーター (横) ----
    draw_gr_readout(ctx, ui, Rect { x: rect.x, y, w: rect.w, h: GR_ROW_H });
}

/// 検出信号の試聴トグル。**同時に 1 トラックだけ** (排他は handler が担保)。
fn draw_sc_listen(ctx: &StripCtx<'_>, ui: &mut Ui<'_, AppData>, row: Rect) {
    let p = &ctx.app.theme.core;
    let style = ToggleButtonStyle {
        on_color: p.accent,
        on_text_color: Some(p.ink_for(p.accent)),
        radius: 2.0,
        font_size: SWITCH_FONT,
        ..ToggleButtonStyle::from_palette(p)
    };
    let on = ctx.strip.comp.sc_listen;
    let track_id = ctx.track_id;
    ui.toggle_button_at(
        ("strip_sc_listen", ctx.track_id),
        "\u{25b6}",
        switch_rect(row),
        on,
        &style,
        move |_| {
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::StripEdit {
                    track: track_id,
                    edit: StripEdit::Switch { switch: StripSwitch::ScListen, on: !on },
                })
            })
        },
    );
}

/// 横向きの GR メーター + 数値 (Comp セクションを開いているときの詳細表示)。
fn draw_gr_readout(ctx: &StripCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) {
    const VALUE_W: f32 = 20.0;
    let p = &ctx.app.theme.core;
    let bar = Rect { w: (rect.w - VALUE_W - 2.0).max(1.0), h: 6.0, y: rect.y + 3.0, ..rect };
    ui.panel(("strip_gr_row_bg", ctx.track_id), bar, p.window_bg, 2.0);
    let frac = if ctx.strip.comp.on {
        (ctx.gain_reduction_db / GR_METER_RANGE_DB).clamp(0.0, 1.0)
    } else {
        0.0
    };
    if frac > 0.0 {
        ui.push_rect(RectCommand {
            rect: Rect { w: bar.w * frac, ..bar },
            fill: ctx.app.theme.daw.strip_gr,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [2.0; 4],
            clip_rect: None,
        });
    }
    // 減衰量は常に負方向なので符号は書かない (`-` を出しても情報が増えない)。
    // 代わりに小数第 1 位まで出す — 数 dB の掛かり具合はここで読む。
    let text = if ctx.strip.comp.on {
        format!("{:.1}", ctx.gain_reduction_db)
    } else {
        "0.0".to_string()
    };
    ui.label_at(
        ("strip_gr_value", ctx.track_id),
        &text,
        rect.x + rect.w - VALUE_W,
        rect.y,
        LABEL_FONT,
        p.text_dim,
    );
}

// ---------------------------------------------------------------------------
// EQ セクション
// ---------------------------------------------------------------------------

fn draw_eq_section(ctx: &StripCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let mut y = rect.y + SECTION_PAD;

    // ---- フィルタ行: HP と LP を 1 行ずつ ----
    // 1 行にまとめると ON スイッチが 2 段重ねになり、他の行のボタンと大きさが
    // 揃わない (= ボタンに見えない)。行を割って正方形のまま置く。
    for (i, band) in [EqBand::Hp, EqBand::Lp].into_iter().enumerate() {
        let row = Rect { y, h: ROW_H, ..rect };
        let start_x = row_start_x(row, 1, SWITCH_W + 2.0);
        let hover = strip_knob(
            ctx,
            ui,
            ("strip_eq_filter_knob", ctx.track_id, i),
            Rect { x: start_x, y: row.y + LABEL_H, w: KNOB, h: KNOB },
            TrackBuiltinParam::StripEq { band, param: EqParam::Freq },
            band.label(),
            !ctx.strip.eq.band(band).on,
        );
        band_switch(
            ctx,
            ui,
            ("strip_eq_filter_on", i),
            switch_rect(row),
            // 14px 角に入る 1 文字。ON/OFF は背景色が示すので、字は
            // 「これは点いたり消えたりする物」の目印で足りる。
            "\u{25cf}",
            StripSwitch::BandOn(band),
            ctx.strip.eq.band(band).on,
        );
        row_label(ctx, ui, ("strip_eq_filter_label", ctx.track_id, i), row, band.label(), hover);
        y += ROW_H;
    }

    // ---- ゲインバンド行 (高い順: HF / HMF / LMF / LF) ----
    for (row_idx, band) in EqBand::GAIN_BANDS.into_iter().enumerate() {
        let row = Rect { y, h: ROW_H, ..rect };
        let params: &[EqParam] = if band.has_q_knob() {
            &[EqParam::Freq, EqParam::Gain, EqParam::Q]
        } else {
            &[EqParam::Freq, EqParam::Gain]
        };
        let switch_w = if band.has_bell_switch() { SWITCH_W + 2.0 } else { 0.0 };
        let start_x = row_start_x(row, params.len(), switch_w);
        let mut hover: Option<String> = None;
        for (i, param) in params.iter().enumerate() {
            #[allow(clippy::cast_precision_loss)]
            let x = start_x + (KNOB + KNOB_GAP) * i as f32;
            let readout = strip_knob(
                ctx,
                ui,
                ("strip_eq_knob", ctx.track_id, row_idx, i),
                Rect { x, y: row.y + LABEL_H, w: KNOB, h: KNOB },
                TrackBuiltinParam::StripEq { band, param: *param },
                eq_param_label(*param),
                false,
            );
            if readout.is_some() {
                hover = readout;
            }
        }
        if band.has_bell_switch() {
            band_switch(
                ctx,
                ui,
                ("strip_eq_bell", row_idx),
                switch_rect(row),
                // ベル (山) ⇄ シェルフ (棚) の切替。ON = ベル。
                "B",
                StripSwitch::Bell(band),
                ctx.strip.eq.band(band).bell,
            );
        }
        row_label(
            ctx,
            ui,
            ("strip_eq_row_label", ctx.track_id, row_idx),
            row,
            band.label(),
            hover,
        );
        y += ROW_H;
    }
}

fn eq_param_label(param: EqParam) -> &'static str {
    match param {
        EqParam::Freq => "Freq",
        EqParam::Gain => "Gain",
        EqParam::Q => "Q",
    }
}

/// バンドの ON / ベル切替のような、オートメーションに載せない小スイッチ。
fn band_switch(
    ctx: &StripCtx<'_>,
    ui: &mut Ui<'_, AppData>,
    id: (&'static str, usize),
    rect: Rect,
    text: &str,
    switch: StripSwitch,
    on: bool,
) {
    let style = eq_switch_style(&ctx.app.theme);
    let track_id = ctx.track_id;
    ui.toggle_button_at((id, ctx.track_id), text, rect, on, &style, move |_| {
        Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::StripEdit {
                track: track_id,
                edit: StripEdit::Switch { switch, on: !on },
            })
        })
    });
}

/// 行右端の小スイッチ共通 style。ON は accent で「点いた」と分かる強さにする
/// (`control_active` だと 14px 角では面の色と見分けが付かない)。
fn eq_switch_style(theme: &Theme) -> ToggleButtonStyle {
    let p = &theme.core;
    ToggleButtonStyle {
        on_color: p.accent,
        on_text_color: Some(p.ink_for(p.accent)),
        radius: 2.0,
        font_size: 9.0,
        ..ToggleButtonStyle::from_palette(p)
    }
}

// ---------------------------------------------------------------------------
// 共通部品
// ---------------------------------------------------------------------------

/// 行の右端に置く小スイッチの矩形。**全行で同じ正方形**、ノブと縦センタ揃え。
fn switch_rect(row: Rect) -> Rect {
    Rect {
        x: row.x + row.w - SWITCH_W,
        y: row.y + LABEL_H + (KNOB - SWITCH_W) * 0.5,
        w: SWITCH_W,
        h: SWITCH_W,
    }
}

/// ノブ列 (+ 右端スイッチ) を行の中で中央寄せするときの左端 x。
fn row_start_x(row: Rect, knobs: usize, switch_w: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let knobs_w = KNOB * knobs as f32 + KNOB_GAP * (knobs as f32 - 1.0);
    row.x + (row.w - switch_w - knobs_w).max(0.0) * 0.5
}

/// 行の見出し行。ノブに触れていないときは行の名前、hover / drag 中は
/// **そのノブの値**を出す。
///
/// 80px 幅にノブ 3 個ぶんの数値欄は入らないので、「いま指している 1 個」だけを
/// 1 行で読ませる (Ardour / Live の hover readout と同じ考え方)。
fn row_label(
    ctx: &StripCtx<'_>,
    ui: &mut Ui<'_, AppData>,
    id: impl std::hash::Hash,
    row: Rect,
    default_text: &str,
    hover: Option<String>,
) {
    let p = &ctx.app.theme.core;
    let (text, color) = match &hover {
        Some(t) => (t.as_str(), p.text),
        None => (default_text, p.text_dim),
    };
    ui.label_at_clipped(id, text, Rect { h: LABEL_H, ..row }, LABEL_FONT, color);
}

/// ストリップのノブ 1 個。
///
/// 値の住所は `TrackBuiltinParam` そのもの = オートメーション / 変調の target と
/// 同じなので、`plain_to_norm` / `build_mod` / `push_param_gesture_edges` の既存
/// 経路がそのまま乗る (ノブ専用の値配管を作らない)。
///
/// 戻り値は hover / drag 中なら `"Freq 2500Hz"` のような読み出し文字列。
fn strip_knob(
    ctx: &StripCtx<'_>,
    ui: &mut Ui<'_, AppData>,
    id: impl std::hash::Hash + Copy,
    rect: Rect,
    param: TrackBuiltinParam,
    label: &'static str,
    dimmed: bool,
) -> Option<String> {
    let app = ctx.app;
    let track_id = ctx.track_id;
    let plain = ctx.strip.target_value(&param)?;
    let target = AutomationTarget::TrackBuiltin(param);
    let norm = plain_to_norm(&target, f64::from(plain));
    let default_plain = ChannelStrip::default().target_value(&param).unwrap_or(0.0);
    let default_norm = plain_to_norm(&target, f64::from(default_plain));

    // ゲイン (0 が中央) だけ bipolar、それ以外は 7 時起点。
    let base = if matches!(param, TrackBuiltinParam::StripEq { param: EqParam::Gain, .. }) {
        KnobStyle::BIPOLAR
    } else {
        KnobStyle::UNIPOLAR
    };
    let style = KnobStyle { surface: Some(ctx.bg), ..base };

    let m = build_mod(app, target.clone(), f64::from(norm), ModControlDomain::Norm, track_id);
    let was_dragging = app
        .recording
        .active_param_gestures
        .contains(&(track_id, target.clone()));
    let resp = ui.knob_at(
        id,
        rect,
        norm,
        default_norm,
        &style,
        {
            let target = target.clone();
            move |v| {
                #[allow(clippy::cast_possible_truncation)]
                let value = norm_to_plain(&target, v) as f32;
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::StripEdit {
                        track: track_id,
                        edit: StripEdit::Param { param, value },
                    })
                })
            }
        },
        Some(m.modulation()),
    );
    push_param_gesture_edges(ui, track_id, target.clone(), label, was_dragging, resp.dragging);
    push_mod_depth_bracket(ui, app, track_id, &target, resp.mod_dragging);

    // モードに上書きされているノブは沈めて「回しても今は音が変わらない」を示す。
    if dimmed {
        ui.push_rect(RectCommand {
            rect,
            fill: Color { a: 0.45, ..app.theme.core.window_bg },
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [rect.w * 0.5; 4],
            clip_rect: None,
        });
    }

    if !resp.hovered && !resp.dragging {
        return None;
    }
    // 表示は widget の preview 値から作る (drag 中は model より 1 frame 先行する)。
    let shown_plain = norm_to_plain(&target, resp.displayed_value);
    let desc = automation_value_display(&target, None);
    Some(format!("{label} {}{}", desc.format.format_value(shown_plain), desc.unit))
}
