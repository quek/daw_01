//! Rack の行の Mix ノブ (r.md #151): device を通した音と通す前の音の混ぜ具合。plugin / 内蔵 device /
//! Parallel ヘッダの 3 種の行が、× の列のすぐ左の同じ列に置く (列が縦に揃う)。
//!
//! - ドラッグ = Mix (Song の値、オートメーション可)。ダブルクリックで 100%。
//! - Alt+クリック = Δ (その device が足し引きした分だけを聴く。聴き方の都合で Song に書かない)。
//!   Δ 中は値弧の色が変わる。

use common::model::{AutomationTarget, TrackBuiltinParam};
use daw_ui_core::{Edit, KnobStyle, Ui, WidgetId};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent, ParamSurface};
use crate::event_device::DeviceEvent;
use crate::handler::view_model::LiveParamScope;
use crate::view::param_gesture::push_param_gesture;

use super::chain_list::ROW_H;
use super::chain_row::CHAIN_KNOB;

/// Mix の列の幅 (ノブ + 右の × との間)。Mix を持たない行もこの幅を空けて、左のボタン列を揃える。
pub(super) const MIX_COL_W: f32 = CHAIN_KNOB + 4.0;

/// Alt を押しながら掴んだ press が続いている間 (Δ の切り替えであって値のドラッグではない)。
#[derive(Default)]
struct AltPress(bool);

/// 行の右端の × の列の幅 (plugin / 内蔵 device / Parallel ヘッダの 3 種の行で同じ)。
const X_COL_W: f32 = 26.0;

/// 行 `row` の Mix の列 (× のすぐ左) に Mix ノブを描く。行の描画は列を空けるだけで、ノブはここが描く
/// (chain list が行ごとに 1 回呼ぶ)。Mix を持たない device ([`common::model::Device::has_mix`]) には何も描かない。
pub(super) fn draw_mix_knob(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    scope: &LiveParamScope,
    device_id: u64,
    row: Rect,
    popup_open: bool,
) {
    let Some(device) = app.cur.song_doc.device_by_id(device_id) else { return };
    if !device.has_mix() {
        return;
    }
    // レーンの置き場は device の持ち主 (master の device なら song 側)。
    let Some(track_id) = app.cur.song_doc.device_owner_track(device_id) else { return };
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::DeviceMix { device_id });
    let live = app.live_param_value_on(scope, track_id, &target, device.mix());
    let x = row.x + row.w - X_COL_W - MIX_COL_W;
    let rect = Rect { x, y: row.y + (ROW_H - CHAIN_KNOB) * 0.5, w: CHAIN_KNOB, h: CHAIN_KNOB };

    // Alt+press = Δ の切り替え。その press が終わるまでノブの値は動かさない (Alt を先に離しても、
    // 手の揺れのぶん Mix がずれない)。
    let pointer = ui.pointer();
    let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));
    let mut toggled = false;
    let alt_press = {
        let st: &mut AltPress = ui.widget_state(WidgetId::ROOT.child((b"rack_mix_alt", device_id)));
        if pointer.primary_just_pressed {
            st.0 = inside && pointer.modifiers.alt && !popup_open;
            toggled = st.0;
        } else if !pointer.primary_pressed && !pointer.primary_just_released {
            st.0 = false;
        }
        st.0
    };
    if toggled {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Device(DeviceEvent::ToggleDeviceDelta { device_id }));
        }));
    }

    let p = &app.theme.core;
    let delta = app.cur.peph.delta_devices.contains(&device_id);
    let style = KnobStyle {
        surface: Some(p.panel_raised),
        accent: delta.then_some(p.selection_warm),
        ..KnobStyle::UNIPOLAR
    };
    let locked = alt_press || popup_open;
    let resp = ui.knob_at(
        ("inspector_device_mix", device_id),
        rect,
        live.clamp(0.0, 1.0),
        1.0,
        &style,
        move |mix| {
            Edit::mutate(move |app: &mut AppData| {
                if !locked {
                    app.handle_event(AppEvent::Device(DeviceEvent::SetDeviceMix { device_id, mix }));
                }
            })
        },
        None,
    );
    push_param_gesture(ui, app, ParamSurface::Rack, track_id, target, resp.dragging && !alt_press);
}
