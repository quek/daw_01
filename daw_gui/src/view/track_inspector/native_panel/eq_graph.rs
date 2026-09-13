//! EQ / Tone EQ の Par の上段: カーブ (+ 背後のスペクトラム) と、カーブ上の点の直接操作 (Q13 / Q14、§10.7)。
//!
//! 点の座標と点ドラッグの逆写像は、描画と同じ [`CurveAxes`] を通す (片方だけ写像を持つと点がカーブから
//! 浮く)。点を動かすと左右 = Freq (対数)、上下 = Gain を **変わった軸だけ** 1 イベントにまとめて出す。
//! ホイールは Q を持つバンド (LMF / HMF) だけで、1 notch で `Q *= 2^(±1/8)`。OFF のバンドや bypass 中の
//! device を触ったときに ON にするのは `NativeEdit::apply` (自動 ON の唯一の SSoT) の責務。

use common::model::{EQ_Q_MAX, EQ_Q_MIN, EqParam, NativeParamId, RackPanelKey};
use daw_ui_core::{Edit, Palette, Ui, XyAxes, XyPointStyle};
use daw_ui_renderer::{Color, Rect};

use super::PanelCtx;
use crate::app::{AppData, AppEvent, ParamSurface};
use crate::event_device::DeviceEvent;
use crate::event_native::NativeEdit;
use crate::view::native_device::{
    CurveAxes, CurveBand, CurveLook, EqCurveSource, HandleAxes, curve_handles, draw_eq_curve, wid,
};

/// ホイール 1 notch あたりの Q の倍率の指数 (`Q *= 2^(notch / 8)`)。
const Q_OCTAVES_PER_NOTCH: f32 = 1.0 / 8.0;
/// 点の座標が「動いた」とみなす最小差 (px)。
const MOVE_EPSILON: f32 = 1e-3;

/// カーブ側で動かしている param の集合。下の段のつまみの `external_drag` に渡し、つまみ・点・ホイールの
/// ジェスチャーを 1 本に OR する。
#[derive(Debug, Default)]
pub(super) struct GraphDrags(Vec<NativeParamId>);

impl GraphDrags {
    pub(super) fn has(&self, p: NativeParamId) -> bool {
        self.0.contains(&p)
    }
}

/// `rect` にカーブを描き、点の操作を処理する。戻り値 = このフレームにカーブ側で動かしている param。
pub(super) fn draw_eq_graph(app: &AppData, ui: &mut Ui<'_, AppData>, ctx: &PanelCtx<'_>, rect: Rect) -> GraphDrags {
    let mut drags = GraphDrags::default();
    let Some(src) = EqCurveSource::from_params(&ctx.live.params) else {
        return drags;
    };
    let p = &app.theme.core;
    let id = ctx.dev.id;
    let key = RackPanelKey::Device(id);
    let active = !ctx.dev.bypassed;
    ui.panel(wid(ParamSurface::Rack, key, "curve_well", ()), rect, p.inset_bg, 2.0);
    draw_eq_curve(app, ui, rect, &src, &CurveLook { active, spectrum_db: app.device_spectrum_db(id) });
    let axes = CurveAxes::for_source(&src);
    for h in curve_handles(&src, &axes, rect) {
        let lock = match h.axes {
            HandleAxes::Both => XyAxes::BOTH,
            HandleAxes::Horizontal => XyAxes::HORIZONTAL,
            HandleAxes::Vertical => XyAxes::VERTICAL,
        };
        let (band, from) = (h.band, h.pos);
        let style = handle_style(p, h.on && active);
        let resp = ui.xy_point_at(wid(ParamSurface::Rack, key, "pt", band), rect, from, lock, h.wheel_q, &style, move |to| {
            point_edit(id, band, axes, rect, from, to)
        });
        if resp.dragging {
            drags.0.extend(moved_params(band, h.axes));
        }
        if let CurveBand::Eq(b) = band
            && h.wheel_q
        {
            let q = NativeParamId::Eq { band: b, param: EqParam::Q };
            if resp.wheel != 0.0
                && let Some(cur) = ctx.live.param(q)
            {
                let v = (cur * 2f32.powf(resp.wheel * Q_OCTAVES_PER_NOTCH)).clamp(EQ_Q_MIN, EQ_Q_MAX);
                ui.push_edit(native_edit(id, NativeEdit::param(q, v)));
            }
            if resp.wheel_active {
                drags.0.push(q);
            }
        }
    }
    drags
}

/// 点を動かすとき同時に動く param (ジェスチャーの対象)。
fn moved_params(band: CurveBand, axes: HandleAxes) -> impl Iterator<Item = NativeParamId> {
    let (freq, gain) = match band {
        CurveBand::Eq(b) => (
            Some(NativeParamId::Eq { band: b, param: EqParam::Freq }),
            (axes == HandleAxes::Both).then_some(NativeParamId::Eq { band: b, param: EqParam::Gain }),
        ),
        CurveBand::Tone(b) => (None, Some(NativeParamId::ToneEq(b))),
    };
    freq.into_iter().chain(gain)
}

/// 点を `from` から `to` へ動かした編集。変わった軸だけを 1 イベントに入れる。
fn point_edit(id: u64, band: CurveBand, axes: CurveAxes, rect: Rect, from: (f32, f32), to: (f32, f32)) -> Edit<AppData> {
    let moved_x = (to.0 - from.0).abs() > MOVE_EPSILON;
    let moved_y = (to.1 - from.1).abs() > MOVE_EPSILON;
    let mut params: Vec<(NativeParamId, f32)> = Vec::with_capacity(2);
    match band {
        CurveBand::Eq(b) => {
            if moved_x {
                params.push((NativeParamId::Eq { band: b, param: EqParam::Freq }, axes.x_to_freq(rect, to.0)));
            }
            if moved_y && b.has_gain() {
                params.push((NativeParamId::Eq { band: b, param: EqParam::Gain }, axes.y_to_db(rect, to.1)));
            }
        }
        CurveBand::Tone(b) => {
            if moved_y {
                params.push((NativeParamId::ToneEq(b), axes.y_to_db(rect, to.1)));
            }
        }
    }
    if params.is_empty() {
        return Edit::mutate(|_: &mut AppData| {});
    }
    native_edit(id, NativeEdit::Params(params))
}

fn native_edit(device_id: u64, edit: NativeEdit) -> Edit<AppData> {
    Edit::mutate(move |app: &mut AppData| {
        app.handle_event(AppEvent::Device(DeviceEvent::NativeEdit { device_id, edit }));
    })
}

/// 点の見た目。効いているバンドは可動ハンドルの色で塗り、OFF のバンド / bypass 中は輪だけ
/// (カーブの線とスペクトラムの塗りの上でも沈まない面の反対色で縁取る)。
pub(super) fn handle_style(p: &Palette, lit: bool) -> XyPointStyle {
    let base = XyPointStyle::from_palette(p);
    if lit {
        base
    } else {
        XyPointStyle { fill: Color::TRANSPARENT, border: p.text_dim, border_width: 1.5, ..base }
    }
}
