//! r.md #127: **ボタンを押したまま Esc = ドラッグのキャンセル** (arrangement の `cancel.rs` と同 idiom)。
//!
//! commit-by-release の session (note の移動 / 範囲 / velocity / loop / 作成) は捨てるだけで
//! 何も起きない。 プレイヘッドの scrub は per-frame で seek 済なので press 直前の位置へ戻す。
//! 押している鍵盤 (`keyboard_pressing`) は音の release を通常経路に任せるため触らない。

use super::*;
use crate::app::AppData;

pub(super) fn on_escape(ui: &mut Ui<'_, AppData>, wid: WidgetId) {
    if !ui.drag_cancel_requested() {
        return;
    }
    let restore_playhead = {
        let state: &mut PianoRollState = ui.widget_state(wid);
        state.note_drag = None;
        state.range_drag = None;
        state.velocity_drag = None;
        state.loop_drag = None;
        state.note_create = None;
        state.edge_scroll_press = None;
        state.edge_pitch_accum = 0.0;
        state.playhead_drag.take().and_then(|pd| pd.anchor_beat)
    };
    if let Some(beat) = restore_playhead {
        ui.push_edit(Edit::mutate(move |app: &mut AppData| {
            app.seek_playhead_to(beat);
        }));
    }
}
