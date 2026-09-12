//! ruler 上の press — Shift + drag の loop range 編集と、素の click / drag の playhead seek。
//!
//! `run.rs::piano_roll` から切り出した単位 (arrangement の `press.rs` の ruler 分岐と同 idiom)。
//! grid / vel_area とは y 軸で完全分離されているので note_drag / velocity_drag と競合しない。

use super::*;
use crate::app::AppData;

/// ruler 内で primary press が起きたフレームに呼ぶ。 戻り値は **press 即発行する seek 拍**
/// (playhead seek session を開いたときだけ `Some`)。
#[allow(clippy::too_many_arguments)]
pub(super) fn press(
    ui: &mut Ui<'_, AppData>,
    wid: WidgetId,
    app: &AppData,
    view: &PianoRollView,
    ruler: Rect,
    beat_per_px: f64,
    zoom_x_px_per_beat: f32,
    px: f32,
    alt: bool,
    shift: bool,
) -> Option<f64> {
    let press_beat = view.start_beat + f64::from(px - ruler.x) * beat_per_px;
    if shift {
        // Shift + ruler drag → loop range edit (NewRange / Start/End/Middle handle)。
        let kind = if let Some(range) = view.loop_range {
            match loop_band_hit_kind(range, view.start_beat, view.len_beats, ruler, px, 4.0) {
                Some(LoopBandHit::Start) => LoopDragKind::Start,
                Some(LoopBandHit::End) => LoopDragKind::End,
                Some(LoopBandHit::Middle) => LoopDragKind::Middle,
                None => LoopDragKind::NewRange,
            }
        } else {
            LoopDragKind::NewRange
        };
        // NewRange の anchor 端点は press 時 snap で grid に着地 (release 端点も
        // `compute_loop_drag_endpoints` で snap される、 arrangement #024 と同 idiom)。
        let anchor_press_beat_for_session = match kind {
            LoopDragKind::NewRange => view.snap.snap_beat(press_beat, alt, zoom_x_px_per_beat),
            _ => press_beat,
        };
        let anchor_loop = view
            .loop_range
            .unwrap_or((anchor_press_beat_for_session, anchor_press_beat_for_session));
        let state: &mut PianoRollState = ui.widget_state(wid);
        state.loop_drag = Some(LoopDragSession {
            kind,
            anchor_loop,
            anchor_press_beat: anchor_press_beat_for_session,
            anchor_mouse_x: px,
            last_mouse_x: px,
            last_alt: alt,
        });
        None
    } else {
        // plain (Shift 非保持) ruler click/drag → playhead seek session。
        let snapped = view.snap.snap_beat(press_beat, alt, zoom_x_px_per_beat).max(0.0);
        let state: &mut PianoRollState = ui.widget_state(wid);
        state.playhead_drag = Some(PlayheadDragSession {
            last_mouse_x: px,
            last_emitted_beat: snapped,
            anchor_beat: app.cur.transport.playhead_beat.map(f64::from),
        });
        Some(snapped)
    }
}
