//! マスターパネル内の組み込みブロック (Bus Comp + Tone EQ + フェーダー後 Limiter)。
//!
//! 設計正本は [docs/plan_rack_native_devices.md](../../../docs/plan_rack_native_devices.md) §10.9
//! (r.md #129 Q17)。置き場と寸法は [docs/plan_master_strip.md](../../../docs/plan_master_strip.md) §3。
//! MASTER セクションの **数値欄の列**を上下に割った上側に置く (LU バーとフェーダーは
//! 全高のまま = コンプの GR とフェーダーが必ず並んで見える)。
//!
//! ```text
//! | COMP    ( 針メーター )  |   ← Bus Comp と Tone EQ の上下は Rack での前後に合わせる
//! | Thr Ratio Atk Rel Gain |
//! |------------------------|
//! | EQ    ~~ curve ~~      |
//! | Low  LoMid  High       |
//! |------------------------|
//! | LIM  ########   -1.0   |   ← Limiter はフェーダーの後なので常に一番下
//! ```
//!
//! 値の持ち主は master の **組み込み Bus Comp / Tone EQ** (`Device::Native`) とフェーダー後の
//! `Song::master_limiter`。つまみ / カーブ / GR は Mixer 帯・Rack Par と共有の
//! [`crate::view::native_device`] で描く (再生中はレーン値に追従し、ジェスチャーと変調を持つ)。
//! ON/OFF ボタンは置かない — カーソルを乗せて `Q` (通常 ch の帯と同じ作法)。

use common::model::{
    AutomationTarget, BusCompParam, GR_METER_RANGE_DB, MASTER_TRACK_ID, NativeDevice, NativeKind, NativeParamId,
    RackPanelKey, Song, ToneEqBand,
};
use daw_ui_core::{NeedleMeterStyle, NeedleScale, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, ParamSurface};
use crate::handler::bypass_target::BypassTarget;
use crate::handler::view_model::LiveParamScope;
use crate::view::native_device::{
    CurveLook, EqCurveSource, LIMITER_GR_SEGMENTS, NativeKnobSpec, ParamOwner, draw_eq_curve, draw_gr_segments,
    limiter_knob, native_knob, wid,
};
use crate::view::strip_sections::hover_readout;

/// このブロック群の描画面 (widget id とジェスチャー所有者の鍵)。
const SURFACE: ParamSurface = ParamSurface::MasterPanel;

/// 針式 GR メーターの高さ (px)。マスターで最初に見る物なので、ノブ 2 行ぶんより
/// 大きく取る (文字盤の余白は widget 側で詰めてある)。
const METER_H: f32 = 72.0;
/// ノブ 1 個の直径 (px)。通常 ch の帯と揃える。
const KNOB: f32 = 20.0;
/// ノブ同士の間隔。
const KNOB_GAP: f32 = 6.0;
/// ラベル / hover 読み出し行の高さ (= [`LABEL_FONT`] の行高)。
const LABEL_H: f32 = 13.0;
/// ラベル / hover 読み出しの font size。マスターパネルは strip より幅があるので
/// 通常 ch (10px) より 1 段大きい。
const LABEL_FONT: f32 = 11.0;
/// ノブ行の高さ (ラベル + ノブ + 隙間)。
const ROW_H: f32 = LABEL_H + KNOB + 3.0;
/// EQ カーブの高さ。
const CURVE_H: f32 = 40.0;
/// リミッターの GR セグメント行の高さ。
const LIM_BAR_H: f32 = 10.0;
/// Ceiling の数値欄の高さ (ノブと縦センタで揃える)。
const VALUE_H: f32 = 16.0;
/// ブロック間の隙間。
const BLOCK_GAP: f32 = 4.0;

/// Comp ブロックの高さ (針メーター + ノブ 2 行)。
const COMP_H: f32 = METER_H + ROW_H * 2.0;
/// EQ ブロックの高さ (カーブ + ノブ 1 行)。
const EQ_H: f32 = CURVE_H + ROW_H;
/// リミッターブロックの高さ (セグメント + ノブ 1 行)。
const LIM_H: f32 = LIM_BAR_H + ROW_H;

/// Bus Comp のノブ 2 行と、触れていないときに出す行の名前。
const BUS_COMP_ROWS: [(&[NativeParamId], &str); 2] = [
    (
        &[
            NativeParamId::BusComp(BusCompParam::Threshold),
            NativeParamId::BusComp(BusCompParam::Ratio),
            NativeParamId::BusComp(BusCompParam::Attack),
        ],
        "Thr Ratio Atk",
    ),
    (&[NativeParamId::BusComp(BusCompParam::Release), NativeParamId::BusComp(BusCompParam::Makeup)], "Rel Makeup"),
];
/// Tone EQ のノブ 1 行。
const TONE_EQ_PARAMS: [NativeParamId; 3] = [
    NativeParamId::ToneEq(ToneEqBand::Low),
    NativeParamId::ToneEq(ToneEqBand::LoMid),
    NativeParamId::ToneEq(ToneEqBand::High),
];

/// このブロック群が要求する高さ (px)。`master_panel` がラウドネス数値欄との
/// 上下分割に使う。並びが入れ替わっても総高は変わらない。
#[must_use]
pub fn desired_height() -> f32 {
    COMP_H + EQ_H + LIM_H + BLOCK_GAP * 2.0
}

/// マスターパネルの 1 ブロック。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    BusComp,
    ToneEq,
    Limiter,
}

impl Block {
    fn height(self) -> f32 {
        match self {
            Self::BusComp => COMP_H,
            Self::ToneEq => EQ_H,
            Self::Limiter => LIM_H,
        }
    }
}

/// 全ブロックが共有する引数の束。
struct MasterCtx<'a> {
    app: &'a AppData,
    /// master の store (`song_lanes` / `song_mod_routings`)。
    owner: ParamOwner<'a>,
    scope: &'a LiveParamScope,
    /// つまみが載っている面の色 (マスターパネルの地)。
    bg: Color,
}

/// 組み込みブロックを描き、**カーソル直下のブロックの `Q` の宛先を返す** (書き込みは
/// `master_panel` が 1 か所で行う。§18-AB)。`rect` は数値欄の列の上側 (高さは caller が決める)。
///
/// 高さが足りないときに何を描くかは優先度 (Bus Comp > Tone EQ > Limiter) で決め、描くと決めた
/// ものを表示の並び (Bus Comp / Tone EQ はチェーン上の前後、Limiter は一番下) で積む。
/// コンプの GR が最後まで残るのは、マスターで最初に見たいのがそれだから。
pub fn draw<'a>(app: &'a AppData, ui: &mut Ui<'a, AppData>, rect: Rect) -> Option<BypassTarget> {
    let song = app.cur.song_doc.song();
    let (bus, tone, tone_first) = builtin_pair(song);
    let scope = app.live_param_scope();
    let ctx = MasterCtx { app, owner: ParamOwner::master(song), scope: &scope, bg: app.theme.core.panel_raised };
    let ptr = ui.pointer().pos;
    let mut hovered: Option<BypassTarget> = None;
    let mut y = rect.y;
    for block in visible_blocks(rect.h, tone_first).into_iter().flatten() {
        let r = Rect { y, h: block.height(), ..rect };
        // 組み込みが一時的に見つからなければ場所だけ確保し、描かず hover も出さない。
        let target = match block {
            Block::BusComp => bus.map(|d| {
                draw_bus_comp(&ctx, ui, r, d);
                BypassTarget::Device(d.id)
            }),
            Block::ToneEq => tone.map(|d| {
                draw_tone_eq(&ctx, ui, r, d);
                BypassTarget::Device(d.id)
            }),
            Block::Limiter => {
                draw_limiter(&ctx, ui, r);
                Some(BypassTarget::MasterLimiter)
            }
        };
        if ptr.is_some_and(|(px, py)| r.contains(px, py)) && target.is_some() {
            hovered = target;
        }
        y += block.height() + BLOCK_GAP;
    }
    hovered
}

/// master の組み込み Bus Comp / Tone EQ と、チェーン上で Tone EQ が Bus Comp より前か。
fn builtin_pair(song: &Song) -> (Option<&NativeDevice>, Option<&NativeDevice>, bool) {
    let (mut bus, mut tone, mut tone_first) = (None, None, false);
    for dev in song.builtin_natives(MASTER_TRACK_ID) {
        match dev.kind() {
            NativeKind::BusComp if bus.is_none() => bus = Some(dev),
            NativeKind::ToneEq if tone.is_none() => {
                tone = Some(dev);
                tone_first = bus.is_none();
            }
            _ => {}
        }
    }
    (bus, tone, tone_first)
}

/// 高さ `h` に描くブロックを表示の並びで返す (描かないものは `None`)。
fn visible_blocks(h: f32, tone_first: bool) -> [Option<Block>; 3] {
    let mut used: Option<f32> = None;
    let mut fits = |block: Block| {
        let need = used.map_or(block.height(), |u| u + BLOCK_GAP + block.height());
        let ok = need <= h;
        if ok {
            used = Some(need);
        }
        ok.then_some(block)
    };
    // 優先度の順に判定する。
    let bus = fits(Block::BusComp);
    let tone = fits(Block::ToneEq);
    let limiter = fits(Block::Limiter);
    if tone_first { [tone, bus, limiter] } else { [bus, tone, limiter] }
}

/// ブロックの「面」の色。**ON は窪んだ井戸 (`window_bg`) / OFF はパネルと同じ面
/// (`panel_raised`)** — 通常 ch の常設帯と同じ規則で、効いているかどうかを線や針の
/// 色だけでなく面の明暗でも読ませる (Q で切り替えた瞬間に一目で分かる)。
fn block_bg(app: &AppData, on: bool) -> Color {
    let p = &app.theme.core;
    if on { p.window_bg } else { p.panel_raised }
}

/// OFF のブロックを半透明のパネル色で覆って沈める (= バイパスされた
/// プラグインがグレーアウトする DAW の作法)。ノブは触れるまま — 触ると自動で ON
/// になるので、沈んでいても操作の入口として残す。
fn dim_if_off(app: &AppData, ui: &mut Ui<'_, AppData>, rect: Rect, on: bool) {
    if on {
        return;
    }
    ui.push_rect(RectCommand {
        rect,
        fill: Color { a: 0.6, ..app.theme.core.panel_raised },
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [2.0; 4],
        clip_rect: None,
    });
}

// ---------------------------------------------------------------------------
// Bus Comp
// ---------------------------------------------------------------------------

fn draw_bus_comp(ctx: &MasterCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect, bus: &NativeDevice) {
    let app = ctx.app;
    let p = &app.theme.core;
    let on = !bus.bypassed;
    // ---- 針式 GR メーター (静的に OFF でも On レーンで効いていれば実際の GR を隠さない) ----
    let style = NeedleMeterStyle {
        bg: block_bg(app, on),
        needle: if on { app.theme.daw.strip_gr } else { p.text_dim },
        ..NeedleMeterStyle::from_palette(p)
    };
    ui.needle_meter(
        wid(SURFACE, RackPanelKey::Device(bus.id), "needle", ()),
        Rect { h: METER_H - 2.0, ..rect },
        app.cur.transport.native_gr.get(bus.id),
        NeedleScale {
            range: (0.0, GR_METER_RANGE_DB),
            // Reason の文字盤と同じ刻み。
            ticks: &[(0.0, "0"), (2.0, "2"), (4.0, "4"), (8.0, "8"), (12.0, "12"), (20.0, "20")],
            // 単位ラベルは置かない (文字盤が小さく、数字と重なって読みにくい)。
            unit: "",
        },
        &style,
    );

    // ---- ノブ 2 行 ----
    let mut y = rect.y + METER_H;
    for (i, (params, name)) in BUS_COMP_ROWS.into_iter().enumerate() {
        let row = Rect { y, h: ROW_H, ..rect };
        let hover = knob_row(ctx, ui, row, bus, params);
        row_label(app, ui, wid(SURFACE, RackPanelKey::Device(bus.id), "row_label", i), row, name, hover);
        y += ROW_H;
    }
    dim_if_off(app, ui, Rect { y: rect.y + METER_H, h: ROW_H * 2.0, ..rect }, on);
}

// ---------------------------------------------------------------------------
// Tone EQ
// ---------------------------------------------------------------------------

fn draw_tone_eq(ctx: &MasterCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect, tone: &NativeDevice) {
    let app = ctx.app;
    let on = !tone.bypassed;
    let key = RackPanelKey::Device(tone.id);
    // ---- カーブ (形はレーン値を重ねた live 値、OFF は形を保って薄く描く) ----
    let curve = Rect { h: CURVE_H - 2.0, ..rect };
    ui.panel(wid(SURFACE, key, "curve_bg", ()), curve, block_bg(app, on), 2.0);
    let live = app.live_native_device(ctx.scope, ctx.owner, tone);
    if let Some(src) = EqCurveSource::from_params(&live.params) {
        draw_eq_curve(app, ui, curve, &src, &CurveLook { active: on, spectrum_db: None });
    }

    let row = Rect { y: rect.y + CURVE_H, h: ROW_H, ..rect };
    let hover = knob_row(ctx, ui, row, tone, &TONE_EQ_PARAMS);
    row_label(app, ui, wid(SURFACE, key, "row_label", 0), row, "Low LoMid High", hover);
    dim_if_off(app, ui, row, on);
}

// ---------------------------------------------------------------------------
// Limiter
// ---------------------------------------------------------------------------

fn draw_limiter(ctx: &MasterCtx<'_>, ui: &mut Ui<'_, AppData>, rect: Rect) {
    let app = ctx.app;
    let on = app.cur.song_doc.song().master_limiter.on;
    // ---- GR セグメント (1 個 = 1dB) ----
    let bar = Rect { h: LIM_BAR_H - 2.0, ..rect };
    let gr_id = wid(SURFACE, RackPanelKey::MasterLimiter, "gr", ());
    draw_gr_segments(app, ui, gr_id, bar, app.cur.transport.master_limiter_gr, on, LIMITER_GR_SEGMENTS);
    dim_if_off(app, ui, bar, on);

    // ---- ノブ 1 個 + 常時表示の数値欄 ----
    // この行はノブが 1 個で右側が空くので、hover を待たずに値をノブの横へ常に出す
    // (見出しは行名のまま固定)。数値欄はクリックで入力 / ダブルクリックで既定値。
    let row = Rect { y: rect.y + LIM_BAR_H, h: ROW_H, ..rect };
    let knob = Rect { x: row.x + (row.w - KNOB).max(0.0) * 0.5, y: row.y + LABEL_H, w: KNOB, h: KNOB };
    let value_x = knob.x + KNOB + KNOB_GAP;
    let value = Rect { x: value_x, y: knob.y + (KNOB - VALUE_H) * 0.5, w: (row.x + row.w - value_x).max(0.0), h: VALUE_H };
    limiter_knob(app, ui, SURFACE, knob, ctx.bg, ctx.scope, Some(value));
    row_label(app, ui, wid(SURFACE, RackPanelKey::MasterLimiter, "row_label", 0), row, "Limiter Ceiling", None);
    dim_if_off(app, ui, row, on);
}

// ---------------------------------------------------------------------------
// 共通部品
// ---------------------------------------------------------------------------

/// ノブを 1 行ぶん中央寄せで描く。戻り値は hover / drag 中のノブの読み出し文字列。
fn knob_row(
    ctx: &MasterCtx<'_>,
    ui: &mut Ui<'_, AppData>,
    row: Rect,
    device: &NativeDevice,
    params: &[NativeParamId],
) -> Option<String> {
    let n = params.len() as f32;
    let start_x = row.x + (row.w - (KNOB * n + KNOB_GAP * (n - 1.0))).max(0.0) * 0.5;
    let mut hover = None;
    for (i, &param) in params.iter().enumerate() {
        let spec = NativeKnobSpec {
            surface: SURFACE,
            owner: ctx.owner,
            device,
            param,
            rect: Rect { x: start_x + (KNOB + KNOB_GAP) * i as f32, y: row.y + LABEL_H, w: KNOB, h: KNOB },
            surface_bg: ctx.bg,
            dimmed: false,
            external_drag: false,
            scope: ctx.scope,
        };
        let resp = native_knob(ctx.app, ui, &spec);
        let target = AutomationTarget::NativeParam { device_id: device.id, param };
        hover = hover_readout(param.knob_label(), &target, resp).or(hover);
    }
    hover
}

/// 行の見出し。ノブに触れていない間は行の名前、触れている間はその値。
fn row_label(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    id: impl std::hash::Hash,
    row: Rect,
    default_text: &str,
    hover: Option<String>,
) {
    let p = &app.theme.core;
    let (text, color) = match &hover {
        Some(t) => (t.as_str(), p.text),
        None => (default_text, p.text_dim),
    };
    ui.label_at_clipped(id, text, Rect { h: LABEL_H, ..row }, LABEL_FONT, color);
}
