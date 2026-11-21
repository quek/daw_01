//! オーディオ入力の録音の GUI イベント (r.md #156)。
//!
//! [`AppEvent::AudioRec`](crate::event::AppEvent::AudioRec) が包む [`AudioRecEvent`] 1 本に
//! 集約する (`AppEvent` へ variant を並べない理由は `LauncherEvent` と同じ)。処理は
//! [`AppData::handle_audio_rec_event`](crate::state::AppData::handle_audio_rec_event)。

use common::model::{MonitorMode, TrackInput};

#[derive(Debug, Clone, PartialEq)]
pub enum AudioRecEvent {
    /// インスペクタの「入力」。`track_ids` 全部の [`common::model::Track::input`] を揃える。Undo 対象。
    SetTrackInput { track_ids: Vec<u32>, input: TrackInput },
    /// インスペクタの「モニター」。`track_ids` 全部の [`common::model::Track::monitor`] を揃える。Undo 対象。
    SetTrackMonitor { track_ids: Vec<u32>, monitor: MonitorMode },
}

impl AudioRecEvent {
    /// この event が undo step を積んだときの履歴ラベル (`AppEvent::undo_label` から委譲)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::SetTrackInput { .. } => "トラックの入力を変更",
            Self::SetTrackMonitor { .. } => "モニターを切替",
        }
    }
}
