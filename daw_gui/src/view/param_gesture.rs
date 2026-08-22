//! Phase 4 Step B / Phase 5 Step 5.1 follow-up: knob / fader / scrubable_number
//! の drag edge を見て `ParamGestureBegin / End` を `Ui::push_edit` する共通
//! helper。 mixer_strips / transport / 今後の inspector knob 等で再利用。
//!
//! `was_dragging` は caller が `AppData.active_param_gestures` から引いて渡す。
//! 同 frame で widget が `Edit::mutate` を push → 次 frame の
//! `app.recording.active_param_gestures` が反映 → `was_dragging` が更新、 という 1
//! frame 遅延 chain で edge が安定検知される。 race なし (= immediate-mode 各
//! frame 間で edit queue が必ず drain される)。

use common::model::AutomationTarget;
use daw_ui_core::{Edit, Ui};

use crate::app::{AppData, AppEvent};

/// drag edge (was_dragging → is_dragging) を検知して `ParamGestureBegin /
/// End` を発火する。 `display_name` は `'static str` で UI に表示する
/// 文字列の root。 caller は自前の lifetime で文字列 literal を渡す。
pub(crate) fn push_param_gesture_edges(
    ui: &mut Ui<'_, AppData>,
    track_id: u32,
    target: AutomationTarget,
    display_name: &'static str,
    was_dragging: bool,
    is_dragging: bool,
) {
    if is_dragging == was_dragging {
        return;
    }
    if is_dragging {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ParamGestureBegin {
                track_id,
                target,
                display_name: display_name.to_string(),
            })
        }));
    } else {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.handle_event(AppEvent::ParamGestureEnd { track_id, target })
        }));
    }
}
