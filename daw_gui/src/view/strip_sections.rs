//! mixer strip の上に積む組み込み Comp / EQ の帯 (Mixer 帯)。
//!
//! 設計正本は [docs/plan_rack_native_devices.md](../../../docs/plan_rack_native_devices.md) §10.8
//! (r.md #129 Q16)。帯の寸法と開閉の規則は [docs/plan_channel_strip.md](../../../docs/plan_channel_strip.md)。
//! 既存 strip (名前 / M・S / Pan / Fader / Sends) には一切触らず、**その上に** 3 つの帯を積む:
//!
//! ```text
//! +----------+  Comp / EQ セクション (開いているときだけ。上下は Rack での前後に合わせる)
//! +----------+  EQ / Comp セクション (同上)
//! +----------+  常設サムネイル帯 (GR バー + EQ カーブ、常に見える)
//! | Name ... |  ← ここから下が既存 strip
//! ```
//!
//! 値の持ち主はそのトラックの **組み込み** Comp / EQ (`Device::Native` の `builtin`)。追加分
//! (「Comp 2」) は Rack にだけ出す。つまみ / カーブ / GR は Rack Par・マスターパネルと共有の
//! [`crate::view::native_device`] で描き、編集は `DeviceEvent::NativeEdit` (自動 ON と値 IPC の
//! 唯一の口) を通す。開閉は **全 ch 一括** (`ProjectView::strip_comp_open` / `strip_eq_open`) で、
//! サムネイル帯の GR バー / カーブのクリックがそのトグルを兼ねる。

use common::model::{
    AutomationTarget, CompMode, CompParam, EqBand, EqParam, GR_METER_RANGE_DB, NativeDevice, NativeKind, NativeParamId,
    NativeParams, RackPanelKey, Song,
};
use daw_ui_core::{Edit, ToggleButtonStyle, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent, ParamSurface};
use crate::automation_value::automation_value_display;
use crate::event::StripSection;
use crate::event_device::DeviceEvent;
use crate::event_native::NativeEdit;
use crate::handler::view_model::LiveParamScope;
use crate::theme::Theme;
use crate::view::native_device::{
    CurveLook, EqCurveSource, NativeKnobResponse, NativeKnobSpec, ParamOwner, draw_eq_curve, draw_gr_horizontal,
    draw_gr_vertical, native_knob, wid,
};

/// この帯の描画面 (widget id とジェスチャー所有者の鍵)。
const SURFACE: ParamSurface = ParamSurface::MixerStrip;

/// 常設サムネイル帯の高さ (px)。折り畳んでいてもここだけは必ず出る。
pub const THUMB_H: f32 = 28.0;
/// ノブ 1 個の直径 (px)。3 個並べて strip 内側 68px に収まる最大。
const KNOB: f32 = 20.0;
/// ノブ同士の間隔。
const KNOB_GAP: f32 = 4.0;
/// 各行のラベル / hover 読み出し行の高さ (= [`LABEL_FONT`] の行高)。
const LABEL_H: f32 = 12.0;
/// ラベル / hover 読み出しの font size。80px strip でも値 (`Freq 2500 Hz`) が
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
/// GR メーター行のバーの太さ (行の縦中央に置く)。
const GR_BAR_H: f32 = 6.0;
/// サムネイル帯の GR バーの幅 (px)。**左端に固定**して全 ch で GR の位置を揃える。
const THUMB_GR_W: f32 = 8.0;
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

/// Comp の 3 行 (つまみ 2 個 + 行の名前 + SC Listen を置くか)。行の名前は静的文字列で持つ
/// (毎フレーム join すると strip の本数だけ String を作る)。検出フィルタの行にだけ Listen を置く
/// (聴く対象を決めるつまみの隣)。
const COMP_ROWS: [([CompParam; 2], &str, bool); 3] = [
    ([CompParam::Threshold, CompParam::Ratio], "Thr Rat", false),
    ([CompParam::Attack, CompParam::Release], "Atk Rel", false),
    ([CompParam::ScFreq, CompParam::Makeup], "SC Gain", true),
];

/// 1 本の strip の帯を描く間ずっと変わらない引数の束 (§10.8 の `BandCtx`)。
struct BandCtx<'a> {
    app: &'a AppData,
    /// lane / routing の持ち主 (この strip のトラック)。
    owner: ParamOwner<'a>,
    /// この strip の背景色 (= つまみが「載っている面」の色)。
    bg: Color,
    /// 組み込み Comp / EQ。正規化が必ず補うので通常は `Some`。
    comp: Option<&'a NativeDevice>,
    eq: Option<&'a NativeDevice>,
    scope: &'a LiveParamScope,
}

/// この strip 上端に積む帯の総高 (px)。`mixer_strips` が既存 strip の開始 y を
/// 決めるのに使い、`root` が下ペインの必要高を見積もるのにも使う (SSoT)。
/// 並びが入れ替わっても総高は変わらない。
#[must_use]
pub fn head_height(app: &AppData) -> f32 {
    THUMB_H
        + if app.cur.view.strip_comp_open { COMP_H } else { 0.0 }
        + if app.cur.view.strip_eq_open { EQ_H } else { 0.0 }
}

/// 帯を描く。`rect` は strip 全体の矩形で、上端から [`head_height`] 分を使う。
pub fn draw_head<'a>(
    app: &'a AppData,
    ui: &mut Ui<'_, AppData>,
    owner: ParamOwner<'a>,
    rect: Rect,
    pad: f32,
    bg: Color,
    scope: &'a LiveParamScope,
) {
    let (comp, eq, comp_first) = builtin_pair(app.cur.song_doc.song(), owner.id);
    let ctx = BandCtx { app, owner, bg, comp, eq, scope };
    let inner = Rect { x: rect.x + pad, y: rect.y, w: (rect.w - pad * 2.0).max(1.0), h: rect.h };

    // Q キー (= 「カーソル直下のものを無効化」) の対象 device。セクション本体と
    // 常設帯の両方が対象で、算出はここ 1 か所 (`mixer_hovered_track` と同 idiom)。
    let ptr = ui.pointer().pos;
    let under = |r: Rect| ptr.is_some_and(|(px, py)| r.contains(px, py));
    let mut hovered: Option<u64> = None;

    // セクションの上下はチェーン上の前後に合わせる (Q16)。
    let order = if comp_first { [StripSection::Comp, StripSection::Eq] } else { [StripSection::Eq, StripSection::Comp] };
    let mut y = rect.y;
    for section in order {
        let (open, h, dev) = match section {
            StripSection::Comp => (app.cur.view.strip_comp_open, COMP_H, comp),
            StripSection::Eq => (app.cur.view.strip_eq_open, EQ_H, eq),
        };
        if !open {
            continue;
        }
        // 組み込みが一時的に見つからなければ高さだけ確保し、つまみも hover も出さない。
        let sect = Rect { y, h, ..inner };
        if let Some(dev) = dev {
            match section {
                StripSection::Comp => draw_comp_section(&ctx, ui, sect, dev),
                StripSection::Eq => draw_eq_section(&ctx, ui, sect, dev),
            }
            if under(sect) {
                hovered = Some(dev.id);
            }
        }
        y += h;
        separator(ui, app, rect, y);
    }
    let (gr_hit, eq_hit) = draw_thumbnail(&ctx, ui, Rect { y, h: THUMB_H, ..inner });
    for (r, dev) in [(gr_hit, comp), (eq_hit, eq)] {
        if let Some(dev) = dev
            && under(r)
        {
            hovered = Some(dev.id);
        }
    }
    publish_hover(&ctx, ui, hovered);
}

/// トラック `owner` の組み込み Comp / EQ と、チェーン上で Comp が EQ より前か。
/// 同じ種類が 2 つあっても (正規化前の一瞬) 先に見つかった方を使い、panic しない。
fn builtin_pair(song: &Song, owner: u32) -> (Option<&NativeDevice>, Option<&NativeDevice>, bool) {
    let (mut comp, mut eq, mut comp_first) = (None, None, true);
    for dev in song.builtin_natives(owner) {
        match dev.kind() {
            NativeKind::Comp if comp.is_none() => {
                comp = Some(dev);
                comp_first = eq.is_none();
            }
            NativeKind::Eq if eq.is_none() => eq = Some(dev),
            _ => {}
        }
    }
    (comp, eq, comp_first)
}

/// カーソル直下の device を `AppData` へ反映する (変化時のみ)。
///
/// 自分の strip から外れたときは、**自分が最後に立てた値だったときだけ** 消す
/// (他の strip が立てた値を横から消さない)。
fn publish_hover(ctx: &BandCtx<'_>, ui: &mut Ui<'_, AppData>, hovered: Option<u64>) {
    let current = ctx.app.cur.peph.mixer_hovered_native;
    let mine = current.is_some_and(|id| [ctx.comp, ctx.eq].into_iter().flatten().any(|d| d.id == id));
    if hovered == current || (hovered.is_none() && !mine) {
        return;
    }
    ui.push_edit(Edit::mutate(move |app: &mut AppData| {
        app.cur.peph.mixer_hovered_native = hovered;
    }));
}

/// セクション同士を分ける 1px の区切り線。
fn separator(ui: &mut Ui<'_, AppData>, app: &AppData, rect: Rect, y: f32) {
    fill(ui, Rect { x: rect.x, y, w: rect.w, h: 1.0 }, app.theme.core.border, 0.0);
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
fn draw_thumbnail(ctx: &BandCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) -> (Rect, Rect) {
    let app = ctx.app;
    let body = Rect { x: rect.x, y: rect.y + 2.0, w: rect.w, h: rect.h - 4.0 };

    // ---- EQ カーブ (帯の全幅)。形はレーン値を重ねた live 値、OFF は形を保って薄く描く ----
    let eq_on = ctx.eq.is_some_and(|d| !d.bypassed);
    fill(ui, body, band_bg(ctx, eq_on), 2.0);
    if let Some(eq) = ctx.eq {
        let live = app.live_native_device(ctx.scope, ctx.owner, eq);
        if let Some(src) = EqCurveSource::from_params(&live.params) {
            draw_eq_curve(app, ui, body, &src, &CurveLook { active: eq_on, spectrum_db: None });
        }
    }

    // ---- GR バー (カーブの上に重ねる。面は必ず自分で塗る) ----
    let gr_rect = Rect { w: THUMB_GR_W, ..body };
    match ctx.comp {
        Some(comp) => {
            let on = !comp.bypassed;
            let gr = app.cur.transport.native_gr.get(comp.id);
            let id = wid(SURFACE, RackPanelKey::Device(comp.id), "thumb_gr", ());
            draw_gr_vertical(app, ui, id, gr_rect, gr, on, band_bg(ctx, on));
        }
        None => fill(ui, gr_rect, band_bg(ctx, false), 2.0),
    }

    // 当たり判定は左 8px = コンプ、残り = EQ (描画の重なりと同じ切り分け)。
    let eq_rect = Rect { x: body.x + THUMB_GR_W, w: (body.w - THUMB_GR_W).max(1.0), ..body };
    section_toggle_click(ui, gr_rect, StripSection::Comp);
    section_toggle_click(ui, eq_rect, StripSection::Eq);
    (gr_rect, eq_rect)
}

/// 常設帯の面の色。**ON は窪んだ井戸 / OFF は strip と同じ面**にして、
/// 「効いているかどうか」を線の色だけでなく面でも読ませる。
fn band_bg(ctx: &BandCtx<'_>, on: bool) -> Color {
    if on { ctx.app.theme.core.window_bg } else { ctx.bg }
}

/// 常設帯の 1 面をクリックしたらそのセクションを開閉する (全 ch 一括)。
fn section_toggle_click(ui: &mut Ui<'_, AppData>, rect: Rect, section: StripSection) {
    if ui.take_primary_press_in_rect(rect).is_some() {
        ui.push_edit(dispatch(AppEvent::ToggleStripSection(section)));
    }
}

// ---------------------------------------------------------------------------
// Comp セクション
// ---------------------------------------------------------------------------

fn draw_comp_section(ctx: &BandCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect, comp: &NativeDevice) {
    let NativeParams::Comp(settings) = &comp.params else { return };
    let key = RackPanelKey::Device(comp.id);
    let mut y = rect.y + SECTION_PAD;
    draw_comp_mode(ctx, ui, Rect { y, h: MODE_ROW_H, ..rect }, comp.id, settings.mode);
    y += MODE_ROW_H + 2.0;

    for (row_idx, (params, row_name, with_listen)) in COMP_ROWS.into_iter().enumerate() {
        let row = Rect { y, h: ROW_H, ..rect };
        let switch_w = if with_listen { SWITCH_W + 2.0 } else { 0.0 };
        let start_x = row_start_x(row, params.len(), switch_w);
        let mut hover: Option<String> = None;
        for (i, param) in params.into_iter().enumerate() {
            let knob_rect = Rect { x: start_x + (KNOB + KNOB_GAP) * i as f32, y: row.y + LABEL_H, w: KNOB, h: KNOB };
            let dimmed = settings.mode.overrides(param);
            hover = knob(ctx, ui, knob_rect, comp, NativeParamId::Comp(param), dimmed).or(hover);
        }
        if with_listen {
            draw_sc_listen(ctx, ui, comp, row);
        }
        row_label(ctx, ui, wid(SURFACE, key, "row_label", row_idx), row, row_name, hover);
        y += ROW_H;
    }

    // ---- GR メーター (横、行の縦中央に細いバー + 右端に数値) ----
    let bar = Rect { x: rect.x, y: y + (GR_ROW_H - GR_BAR_H) * 0.5, w: rect.w, h: GR_BAR_H };
    let gr = ctx.app.cur.transport.native_gr.get(comp.id);
    let id = wid(SURFACE, key, "gr", ());
    draw_gr_horizontal(ctx.app, ui, id, bar, gr, !comp.bypassed, GR_METER_RANGE_DB, Some(LABEL_FONT));
}

/// モード切替 (LEV / CMP / LIM の 3 択)。
fn draw_comp_mode(ctx: &BandCtx<'_>, ui: &mut Ui<'_, AppData>, row: Rect, device_id: u64, current: CompMode) {
    let p = &ctx.app.theme.core;
    let style = ToggleButtonStyle {
        on_color: p.control_active,
        radius: 2.0,
        font_size: SWITCH_FONT,
        ..ToggleButtonStyle::from_palette(p)
    };
    let gap = 2.0;
    let w = (row.w - gap * 2.0) / 3.0;
    for (i, mode) in CompMode::ALL.into_iter().enumerate() {
        ui.toggle_button_at(
            wid(SURFACE, RackPanelKey::Device(device_id), "mode", mode.label()),
            mode.label(),
            Rect { x: row.x + (w + gap) * i as f32, w, ..row },
            current == mode,
            &style,
            move |_| native_edit(device_id, NativeEdit::CompMode(mode)),
        );
    }
}

/// 検出信号の試聴トグル。**プロジェクトで同時に 1 つだけ** (`sc_listen_device` が Option 1 個)。
/// 点灯は「この Comp を Listen 中で、かつ ON」(Rack Par と同じ規則)。bypass 中に押すと
/// 有効化してから聴く (handler の `request_sc_listen`)。
fn draw_sc_listen(ctx: &BandCtx<'_>, ui: &mut Ui<'_, AppData>, comp: &NativeDevice, row: Rect) {
    let device_id = comp.id;
    let lit = ctx.app.cur.peph.sc_listen_device == Some(device_id) && !comp.bypassed;
    ui.toggle_button_at(
        wid(SURFACE, RackPanelKey::Device(device_id), "listen", ()),
        "\u{25b6}",
        switch_rect(row),
        lit,
        &switch_style(&ctx.app.theme),
        move |_| dispatch(AppEvent::Device(DeviceEvent::SetScListen { device_id: (!lit).then_some(device_id) })),
    );
}

// ---------------------------------------------------------------------------
// EQ セクション
// ---------------------------------------------------------------------------

fn draw_eq_section(ctx: &BandCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect, eq: &NativeDevice) {
    let NativeParams::Eq(settings) = &eq.params else { return };
    let key = RackPanelKey::Device(eq.id);
    let mut y = rect.y + SECTION_PAD;

    // ---- フィルタ行: HP と LP を 1 行ずつ ----
    // 1 行にまとめると ON スイッチが 2 段重ねになり、他の行のボタンと大きさが
    // 揃わない (= ボタンに見えない)。行を割って正方形のまま置く。
    for band in [EqBand::Hp, EqBand::Lp] {
        let row = Rect { y, h: ROW_H, ..rect };
        let start_x = row_start_x(row, 1, SWITCH_W + 2.0);
        let band_on = settings.band(band).on;
        let knob_rect = Rect { x: start_x, y: row.y + LABEL_H, w: KNOB, h: KNOB };
        let hover = knob(ctx, ui, knob_rect, eq, NativeParamId::Eq { band, param: EqParam::Freq }, !band_on);
        // 14px 角に入る 1 文字。ON/OFF は背景色が示すので、字は「これは点いたり消えたりする物」の目印で足りる。
        let edit = NativeEdit::EqBandOn { band, on: !band_on };
        band_switch(ctx, ui, wid(SURFACE, key, "band_on", band), switch_rect(row), "\u{25cf}", eq.id, edit, band_on);
        row_label(ctx, ui, wid(SURFACE, key, "row_label", band), row, band.label(), hover);
        y += ROW_H;
    }

    // ---- ゲインバンド行 (高い順: HF / HMF / LMF / LF) ----
    for band in EqBand::GAIN_BANDS {
        let row = Rect { y, h: ROW_H, ..rect };
        let params: &[EqParam] =
            if band.has_q_knob() { &[EqParam::Freq, EqParam::Gain, EqParam::Q] } else { &[EqParam::Freq, EqParam::Gain] };
        let switch_w = if band.has_bell_switch() { SWITCH_W + 2.0 } else { 0.0 };
        let start_x = row_start_x(row, params.len(), switch_w);
        let mut hover: Option<String> = None;
        for (i, &param) in params.iter().enumerate() {
            let knob_rect = Rect { x: start_x + (KNOB + KNOB_GAP) * i as f32, y: row.y + LABEL_H, w: KNOB, h: KNOB };
            hover = knob(ctx, ui, knob_rect, eq, NativeParamId::Eq { band, param }, false).or(hover);
        }
        if band.has_bell_switch() {
            // ベル (山) ⇄ シェルフ (棚) の切替。ON = ベル。
            let bell = settings.band(band).bell;
            let edit = NativeEdit::EqBell { band, bell: !bell };
            band_switch(ctx, ui, wid(SURFACE, key, "bell", band), switch_rect(row), "B", eq.id, edit, bell);
        }
        row_label(ctx, ui, wid(SURFACE, key, "row_label", band), row, band.label(), hover);
        y += ROW_H;
    }
}

/// バンドの ON / ベル切替のような、オートメーションに載せない小スイッチ。
#[allow(clippy::too_many_arguments)]
fn band_switch(
    ctx: &BandCtx<'_>,
    ui: &mut Ui<'_, AppData>,
    id: impl std::hash::Hash,
    rect: Rect,
    text: &str,
    device_id: u64,
    edit: NativeEdit,
    on: bool,
) {
    let style = switch_style(&ctx.app.theme);
    ui.toggle_button_at(id, text, rect, on, &style, move |_| native_edit(device_id, edit.clone()));
}

// ---------------------------------------------------------------------------
// 共通部品
// ---------------------------------------------------------------------------

/// つまみ 1 個 (共有部品 [`native_knob`])。戻り値は hover / drag 中なら `"Thr -12.0 dB"` のような読み出し。
fn knob(
    ctx: &BandCtx<'_>,
    ui: &mut Ui<'_, AppData>,
    rect: Rect,
    device: &NativeDevice,
    param: NativeParamId,
    dimmed: bool,
) -> Option<String> {
    let spec = NativeKnobSpec {
        surface: SURFACE,
        owner: ctx.owner,
        device,
        param,
        rect,
        surface_bg: ctx.bg,
        dimmed,
        external_drag: false,
        scope: ctx.scope,
    };
    let resp = native_knob(ctx.app, ui, &spec);
    hover_readout(param.knob_label(), &AutomationTarget::NativeParam { device_id: device.id, param }, resp)
}

/// つまみに触れている間の読み出し文字列 (`"{label} {値と単位}"`)。触れていなければ `None`。
///
/// 値の書式は [`automation_value_display`] が SSoT (段階式の `4:1` / SC の `OFF` もここで決まる)。
/// マスターパネルの行見出しも同じ関数を使う。
pub(crate) fn hover_readout(label: &str, target: &AutomationTarget, resp: NativeKnobResponse) -> Option<String> {
    (resp.hovered || resp.dragging)
        .then(|| format!("{label} {}", automation_value_display(target, None).format_with_unit(resp.displayed_plain)))
}

/// 組み込み device への値編集イベント (自動 ON と値 IPC は handler の `NativeEdit::apply` が持つ)。
fn native_edit(device_id: u64, edit: NativeEdit) -> Edit<AppData> {
    dispatch(AppEvent::Device(DeviceEvent::NativeEdit { device_id, edit }))
}

fn dispatch(event: AppEvent) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(event);
    })
}

/// 行右端の小スイッチ (ON / BELL / Listen) 共通 style。ON は accent で「点いた」と分かる強さにする
/// (`control_active` だと 14px 角では面の色と見分けが付かない)。
fn switch_style(theme: &Theme) -> ToggleButtonStyle {
    let p = &theme.core;
    ToggleButtonStyle {
        on_color: p.accent,
        on_text_color: Some(p.ink_for(p.accent)),
        radius: 2.0,
        font_size: SWITCH_FONT,
        ..ToggleButtonStyle::from_palette(p)
    }
}

/// 行の右端に置く小スイッチの矩形。**全行で同じ正方形**、ノブと縦センタ揃え。
fn switch_rect(row: Rect) -> Rect {
    Rect { x: row.x + row.w - SWITCH_W, y: row.y + LABEL_H + (KNOB - SWITCH_W) * 0.5, w: SWITCH_W, h: SWITCH_W }
}

/// ノブ列 (+ 右端スイッチ) を行の中で中央寄せするときの左端 x。
fn row_start_x(row: Rect, knobs: usize, switch_w: f32) -> f32 {
    let knobs_w = KNOB * knobs as f32 + KNOB_GAP * (knobs as f32 - 1.0);
    row.x + (row.w - switch_w - knobs_w).max(0.0) * 0.5
}

/// 行の見出し行。ノブに触れていないときは行の名前、hover / drag 中は **そのノブの値**を出す。
///
/// 80px 幅にノブ 3 個ぶんの数値欄は入らないので、「いま指している 1 個」だけを
/// 1 行で読ませる (Ardour / Live の hover readout と同じ考え方)。
fn row_label(
    ctx: &BandCtx<'_>,
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

fn fill(ui: &mut Ui<'_, AppData>, rect: Rect, color: Color, radius: f32) {
    ui.push_rect(RectCommand {
        rect,
        fill: color,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [radius; 4],
        clip_rect: None,
    });
}
