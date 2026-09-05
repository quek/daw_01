//! Global Sampler / MIDI Capture の範囲をアレンジ / ランチャーのセルへ落とす受け口
//! (`docs/plan_global_sampler.md` Q4)。
//!
//! 下部タブが `Ui::begin_drag` で持ち出した payload を、**ファイル drop と同じ着地解決**
//! ([`arrangement_view::file_drop_target`] + 同じ pixel→beat + snap) で受ける。
//! レーン / ランチャー帯の上で離したときだけ消費し、それ以外は host が frame 末に
//! 捨てる (= キャンセル)。`arrangement_view.rs` の外に置くのはファイル budget
//! (不変条件 9) のため。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::Rect;

use crate::app::{AppData, AppEvent};
use crate::event_sampler::SamplerEvent;
use crate::state::midi_capture::{MIDI_CAPTURE_DRAG_KIND, MidiCaptureDragPayload};
use crate::state::sampler::{SAMPLER_DRAG_KIND, SamplerDragPayload};
use crate::view::arrangement_view::file_drop_target;
use crate::widgets::arrangement::ArrangementResponse;

pub(crate) fn take_capture_drops(
    app: &AppData,
    ui: &mut Ui<'_, AppData>,
    resp: &ArrangementResponse,
    canvas_area: Rect,
    scroll_beat: f64,
    zoom: f32,
    arr_snap: &common::snap::SnapConfig,
) {
    let pointer = ui.pointer();
    if !pointer.primary_just_released {
        return;
    }
    let Some(pos) = pointer.pos else { return };
    let in_launcher = resp.launcher.pane_rect.w > 0.0 && resp.launcher.pane_rect.contains(pos.0, pos.1);
    if !in_launcher && !canvas_area.contains(pos.0, pos.1) {
        return;
    }
    let kind = ui.dragging_kind();
    if kind != Some(SAMPLER_DRAG_KIND) && kind != Some(MIDI_CAPTURE_DRAG_KIND) {
        return;
    }
    let Some(target) = file_drop_target(app, resp, pos) else {
        // 帯の停止列 / 見出しなど、置けない場所: 消費して何もしない (ファイルと同じ契約)。
        ui.cancel_drag();
        return;
    };
    let raw = scroll_beat + ((pos.0 - canvas_area.x) as f64 / zoom as f64);
    let target_beat = Some(arr_snap.snap_beat(raw.max(0.0), /* alt: */ false, zoom));
    if let Some(p) = ui.take_drag_payload::<SamplerDragPayload>(SAMPLER_DRAG_KIND) {
        let (start_frame, end_frame) = (p.start_frame, p.end_frame);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::Drop {
                start_frame,
                end_frame,
                target,
                target_beat,
            }));
        }));
    } else if let Some(p) = ui.take_drag_payload::<MidiCaptureDragPayload>(MIDI_CAPTURE_DRAG_KIND) {
        let (start_ns, end_ns) = (p.start_ns, p.end_ns);
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::Sampler(SamplerEvent::MidiDrop {
                start_ns,
                end_ns,
                target,
                target_beat,
            }));
        }));
    }
}
