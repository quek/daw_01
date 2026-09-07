//! LFO / ADSR 本体 (`bodies.rs` から分割、 不変条件 9)。 r.md #116 の LFO 追加行 (Shape / Jitter /
//! Smooth / Steps / Delay / Fade In) と r.md #117 の ADSR 本体。 共通のツマミ idiom
//! (`mod_param_field` / `mod_rate_full` / `mod_retrigger_toggle` / `preview_series`) は `bodies.rs`。

use common::model::ModParam;
use daw_ui_core::{Edit, ScrubCurve, ScrubableNumberFormat, ScrubableNumberStyle, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ModSourceRow};
use crate::view::track_inspector::scrub_style;

use super::bodies::{
    ModParamField, mod_param_field, mod_rate_full, mod_retrigger_toggle, preview_series, series_refs,
};
use super::{MOD_CANVAS_H, ModBodyCtx, ROW_H, ROW_PITCH};

/// r.md #117: ADSR のプレビュー — 「押して (A + D + [`ADSR_PREVIEW_HOLD_SECS`]) 離す (R)」 の
/// 1 回分を `n+1` 点。 時間軸は秒 (config の実値)。
pub(super) fn adsr_preview_samples(c: &common::model::AdsrConfig, n: usize) -> Vec<(f32, f32)> {
    let a = f64::from(c.attack_ms.max(0.0)) * 1e-3;
    let d = f64::from(c.decay_ms.max(0.0)) * 1e-3;
    let r = f64::from(c.release_ms.max(0.0)) * 1e-3;
    let gate = a + d + ADSR_PREVIEW_HOLD_SECS;
    let total = (gate + r).max(1e-3);
    (0..=n)
        .map(|i| {
            let f = i as f32 / n as f32;
            let t = f64::from(f) * total;
            let released = (t > gate).then_some(t - gate);
            let v = common::modulators::adsr_env(c.attack_ms, c.decay_ms, c.sustain, c.release_ms, t, released);
            (f, v)
        })
        .collect()
}

/// ADSR プレビューで sustain を見せる保持時間 (秒)。
const ADSR_PREVIEW_HOLD_SECS: f64 = 0.25;

/// r.md #117: 鳴っているボイスごとの ADSR カーソル `(位置, 今の値)` (位置は
/// [`adsr_preview_samples`] と同じ時間軸)。 押している間は A + D を進み sustain の保持区間の
/// 端で止まり、 離したら R 区間を進む。 値はそのボイスの実際の包絡 (`adsr_env`)。
fn adsr_voice_cursors(cx: &ModBodyCtx<'_>, sid: u32, c: &common::model::AdsrConfig) -> Vec<(f32, f32)> {
    let a = f64::from(c.attack_ms.max(0.0)) * 1e-3;
    let d = f64::from(c.decay_ms.max(0.0)) * 1e-3;
    let r = f64::from(c.release_ms.max(0.0)) * 1e-3;
    let gate = a + d + ADSR_PREVIEW_HOLD_SECS;
    let total = (gate + r).max(1e-3);
    super::bodies::owner_track_voices(cx, sid)
        .map(|v| {
            let t_on = cx.secs - v.on_secs;
            let released = v.off_secs.map(|off| (cx.secs - off).max(0.0));
            let x = match released {
                Some(tr) => gate + tr,
                None => t_on.min(gate),
            };
            let value = common::modulators::adsr_env(c.attack_ms, c.decay_ms, c.sustain, c.release_ms, t_on, released);
            #[allow(clippy::cast_possible_truncation)]
            {
                ((x / total).clamp(0.0, 1.0) as f32, value)
            }
        })
        .collect()
}


/// LFO 本体 (プレビュー / shape / rate / φ / Pulse width / retrigger)。
pub(super) fn draw_lfo_body(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
    c: &common::model::LfoConfig,
    mut y: f32,
) -> (f32, bool) {
    use crate::app::ModSourceEdit as E;
    let (sid, lx, p) = (src.id, cx.lx, &cx.app.theme.core);
    let mut drag = false;

    let (fg, ghosts, cursors) = preview_series(cx, src);
    ui.signal_preview(
        ("inspector_lfo_prev", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &fg,
        &series_refs(&ghosts),
        &cursors,
        cx.editor,
    );
    y += MOD_CANVAS_H + 4.0;

    // row A: shape + rate(+Hz)
    let shapes = ["Sin", "Tri", "SawU", "SawD", "Sqr", "Pulse"];
    let ssel = match c.shape {
        common::model::LfoShape::Sine => 0,
        common::model::LfoShape::Triangle => 1,
        common::model::LfoShape::SawUp => 2,
        common::model::LfoShape::SawDown => 3,
        common::model::LfoShape::Square => 4,
        common::model::LfoShape::Pulse { .. } => 5,
    };
    // Pulse の現 width を保持して shape 切替時に維持。
    let cur_width = if let common::model::LfoShape::Pulse { width } = c.shape { width } else { 0.5 };
    if let Some(pick) = ui.dropdown(
        ("inspector_lfo_shape", sid),
        Rect { x: lx, y, w: 56.0, h: ROW_H },
        &shapes,
        ssel,
    ) {
        let shape = match pick {
            0 => common::model::LfoShape::Sine,
            1 => common::model::LfoShape::Triangle,
            2 => common::model::LfoShape::SawUp,
            3 => common::model::LfoShape::SawDown,
            4 => common::model::LfoShape::Square,
            _ => common::model::LfoShape::Pulse { width: cur_width },
        };
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::EditModSource { id: sid, edit: E::LfoShape(shape) });
        }));
    }
    drag |= mod_rate_full(ui, cx, lx + 62.0, y, &c.rate, sid);
    y += ROW_PITCH;

    // row B: φ phase + (Pulse width) + retrig
    ui.label_at(("inspector_lfo_ph_lbl", sid), "\u{03c6}", lx, y + 4.0, 10.0, p.text);
    drag |= mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::LfoPhase,
            rect: Rect { x: lx + 12.0, y, w: 50.0, h: ROW_H },
            default_plain: 0.0,
            unit: "",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: E::LfoPhase(v as f32),
                    });
                })
            },
        },
    );
    let mut next_x = lx + 68.0;
    if matches!(c.shape, common::model::LfoShape::Pulse { .. }) {
        ui.label_at(("inspector_lfo_w_lbl", sid), "w", next_x, y + 4.0, 10.0, p.text);
        drag |= draw_lfo_width_field(ui, cx, sid, next_x + 12.0, y);
        next_x += 64.0;
    }
    mod_retrigger_toggle(ui, Rect { x: next_x, y, w: 56.0, h: ROW_H }, &c.retrigger, sid, cx.beat);
    y += ROW_PITCH;

    // row C (r.md #116): Shape / Jitter / Smooth — どれも変調先になるツマミ (`mod_param_field`)。
    let knobs = [
        LfoKnob { label_key: "inspector_lfo_shp_lbl", label: "Shp", param: ModParam::LfoShapeAmt, default_plain: 0.5, make: E::LfoShapeAmt },
        LfoKnob { label_key: "inspector_lfo_jit_lbl", label: "Jit", param: ModParam::LfoJitter, default_plain: 0.0, make: E::LfoJitter },
        LfoKnob { label_key: "inspector_lfo_smo_lbl", label: "Smo", param: ModParam::LfoSmooth, default_plain: 0.0, make: E::LfoSmooth },
    ];
    let mut x = lx;
    for k in knobs {
        ui.label_at((k.label_key, sid), k.label, x, y + 4.0, 10.0, p.text);
        let mk = k.make;
        drag |= mod_param_field(
            ui,
            cx,
            ModParamField {
                sid,
                param: k.param,
                rect: Rect { x: x + 22.0, y, w: 46.0, h: ROW_H },
                default_plain: k.default_plain,
                unit: "",
                on_change: &move |v| {
                    let edit = mk(v as f32);
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::EditModSource { id: sid, edit: edit.clone() });
                    })
                },
            },
        );
        x += 22.0 + 46.0 + 8.0;
    }
    y += ROW_PITCH;

    // row D (r.md #116): Steps (段数) / Delay / Fade In (拍) — 設定値 (変調先ではない)。
    ui.label_at(("inspector_lfo_steps_lbl", sid), "Steps", lx, y + 4.0, 10.0, p.text);
    drag |= lfo_plain_field(
        ui,
        cx,
        ("inspector_lfo_steps", sid),
        Rect { x: lx + 32.0, y, w: 36.0, h: ROW_H },
        f64::from(c.steps),
        0.0,
        ScrubableNumberFormat::Integer,
        (0.0, f64::from(common::model::LFO_STEPS_MAX)),
        0.05,
        &move |v| E::LfoSteps(v.round().clamp(0.0, 255.0) as u8),
    );
    ui.label_at(("inspector_lfo_dly_lbl", sid), "Dly", lx + 74.0, y + 4.0, 10.0, p.text);
    drag |= lfo_plain_field(
        ui,
        cx,
        ("inspector_lfo_delay", sid),
        Rect { x: lx + 96.0, y, w: 46.0, h: ROW_H },
        f64::from(c.delay_beats),
        0.0,
        ScrubableNumberFormat::Decimal(2),
        (0.0, f64::from(common::model::LFO_TIME_BEATS_MAX)),
        0.02,
        &move |v| E::LfoDelay(v as f32),
    );
    ui.label_at(("inspector_lfo_fade_lbl", sid), "Fade", lx + 148.0, y + 4.0, 10.0, p.text);
    drag |= lfo_plain_field(
        ui,
        cx,
        ("inspector_lfo_fade", sid),
        Rect { x: lx + 174.0, y, w: 46.0, h: ROW_H },
        f64::from(c.fade_in_beats),
        0.0,
        ScrubableNumberFormat::Decimal(2),
        (0.0, f64::from(common::model::LFO_TIME_BEATS_MAX)),
        0.02,
        &move |v| E::LfoFadeIn(v as f32),
    );
    (y + ROW_PITCH, drag)
}

/// r.md #117: ADSR 本体 (プレビュー + `A [ms] D [ms] S [0..1] R [ms]`、 全部変調先)。
/// retrigger 欄は無い (常にノート起点)。
pub(super) fn draw_adsr_body(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    src: &ModSourceRow,
    c: &common::model::AdsrConfig,
    mut y: f32,
) -> (f32, bool) {
    use crate::app::ModSourceEdit as E;
    let (sid, lx, p) = (src.id, cx.lx, &cx.app.theme.core);
    let mut drag = false;
    let fg = adsr_preview_samples(c, 160);
    let cursors = adsr_voice_cursors(cx, sid, c);
    ui.signal_preview(
        ("inspector_adsr_prev", sid),
        Rect { x: lx, y, w: cx.row_w, h: MOD_CANVAS_H },
        &fg,
        &[],
        &cursors,
        cx.editor,
    );
    y += MOD_CANVAS_H + 4.0;
    let knobs = [
        LfoKnob { label_key: "inspector_adsr_a_lbl", label: "A", param: ModParam::AdsrAttack, default_plain: 10.0, make: E::AdsrAttack },
        LfoKnob { label_key: "inspector_adsr_d_lbl", label: "D", param: ModParam::AdsrDecay, default_plain: 200.0, make: E::AdsrDecay },
        LfoKnob { label_key: "inspector_adsr_s_lbl", label: "S", param: ModParam::AdsrSustain, default_plain: 0.7, make: E::AdsrSustain },
        LfoKnob { label_key: "inspector_adsr_r_lbl", label: "R", param: ModParam::AdsrRelease, default_plain: 300.0, make: E::AdsrRelease },
    ];
    let mut x = lx;
    for k in knobs {
        ui.label_at((k.label_key, sid), k.label, x, y + 4.0, 10.0, p.text);
        let mk = k.make;
        let unit = if k.param == ModParam::AdsrSustain { "" } else { "ms" };
        drag |= mod_param_field(
            ui,
            cx,
            ModParamField {
                sid,
                param: k.param,
                rect: Rect { x: x + 10.0, y, w: 46.0, h: ROW_H },
                default_plain: k.default_plain,
                unit,
                on_change: &move |v| {
                    let edit = mk(v as f32);
                    Edit::mutate(move |app: &mut AppData| {
                        app.handle_event(AppEvent::EditModSource { id: sid, edit: edit.clone() });
                    })
                },
            },
        );
        x += 10.0 + 46.0 + 6.0;
    }
    (y + ROW_PITCH, drag)
}

/// r.md #116: LFO 本体の行 C のツマミ 1 本 (Shape / Jitter / Smooth) の記述。
struct LfoKnob {
    label_key: &'static str,
    label: &'static str,
    param: ModParam,
    default_plain: f64,
    make: fn(f32) -> crate::app::ModSourceEdit,
}

/// r.md #116: LFO の **変調先にならない** 設定値の欄 (Steps / Delay / Fade In)。 undo bracket は
/// 呼び側の `any_mod_drag` (`ScrubGesture::ModRack`) に乗せる。 戻り値 = ドラッグ / 入力中か。
#[allow(clippy::too_many_arguments)]
fn lfo_plain_field(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    id: (&'static str, u32),
    rect: Rect,
    value: f64,
    default: f64,
    fmt: ScrubableNumberFormat,
    range: (f64, f64),
    sensitivity: f32,
    make: &dyn Fn(f64) -> crate::app::ModSourceEdit,
) -> bool {
    let sid = id.1;
    let style = ScrubableNumberStyle {
        range: Some(range),
        curve: ScrubCurve::Linear,
        sensitivity,
        ..scrub_style(&cx.app.theme)
    };
    let resp = ui.scrubable_number_at(
        id,
        rect,
        value,
        default,
        fmt,
        &style,
        move |v| {
            let edit = make(v);
            Edit::mutate(move |app: &mut AppData| {
                app.handle_event(AppEvent::EditModSource { id: sid, edit: edit.clone() });
            })
        },
        None,
        None,
    );
    resp.dragging || resp.editing_text
}

/// Pulse の duty (`w`) 欄。 **`draw_lfo_body` の `if` の中に置かない** — 閉包 + 構造体
/// リテラルが重なってインデントが 7 段に届き、 1 関数 6 段の budget を割る (不変条件 9)。
fn draw_lfo_width_field(
    ui: &mut Ui<'_, AppData>,
    cx: &ModBodyCtx<'_>,
    sid: u32,
    x: f32,
    y: f32,
) -> bool {
    use crate::app::ModSourceEdit as E;
    mod_param_field(
        ui,
        cx,
        ModParamField {
            sid,
            param: ModParam::LfoPulseWidth,
            rect: Rect { x, y, w: 46.0, h: ROW_H },
            default_plain: 0.5,
            unit: "",
            on_change: &move |v| {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::EditModSource {
                        id: sid,
                        edit: E::LfoShape(common::model::LfoShape::Pulse { width: v as f32 }),
                    });
                })
            },
        },
    )
}

