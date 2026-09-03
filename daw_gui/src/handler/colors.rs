//! トラック / クリップ / オートメーションレーン / オートメーションクリップの色
//! (`docs/plan_track_clip_color.md`)。継承 + 上書きの 2 段は 4 つとも同形:
//! `Track.color → Clip.color`、`AutomationLane.color → AutomationClip.color`。
//! SET は content を共有するクリップ全体へ伝播、RESET は行 (track / lane) に閉じる。

use crate::app_types::{ClipKey, propagate_clip_color};
use crate::state::*;

impl AppData {
    /// `Track.color` の上書き (`None` = id 由来の導出色へ戻す)。
    pub(crate) fn set_track_color(&mut self, track: u32, color: Option<[f32; 3]>) {
        self.edit_song(|song| {
            if let Some(t) = song.track_by_id_mut(track) {
                t.color = color;
            }
        });
    }

    /// track の全 clip (arrangement + session) の上書きを外す (= track 色継承)。
    /// track-scoped: 他 track の共有 clip は触らない (計画書「確定動作 2」)。
    pub(crate) fn reset_track_clip_colors(&mut self, track: u32) {
        self.edit_song(|song| {
            if let Some(t) = song.track_by_id_mut(track) {
                for clip in t.all_clips_mut() {
                    clip.color = None;
                }
            }
        });
    }

    /// `Clip.color` の上書き。content を共有する全 track の全 clip へ伝播。
    pub(crate) fn set_clip_color(&mut self, target: ClipKey, color: Option<[f32; 3]>) {
        self.edit_song(|song| propagate_clip_color(&mut song.tracks, target, color));
    }

    /// `AutomationLane.color` の上書き (`None` = 対象種別の識別色へ戻す)。
    pub(crate) fn set_automation_lane_color(
        &mut self,
        lane: common::model::AutomationLaneKey,
        color: Option<[f32; 3]>,
    ) {
        self.edit_song(|song| {
            if let Some(l) = song.automation_lane_by_key_mut(lane.track, lane.lane) {
                l.color = color;
            }
        });
    }

    /// lane の全 clip (arrangement + session) の上書きを外す (= レーン色継承)。
    /// [`Self::reset_track_clip_colors`] のレーン版 (lane-scoped)。
    pub(crate) fn reset_automation_lane_clip_colors(
        &mut self,
        lane: common::model::AutomationLaneKey,
    ) {
        self.edit_song(|song| {
            if let Some(l) = song.automation_lane_by_key_mut(lane.track, lane.lane) {
                for clip in l.all_clips_mut() {
                    clip.color = None;
                }
            }
        });
    }

    /// `AutomationClip.color` の上書き。[`Self::set_clip_color`] と同じく content を
    /// 共有する全クリップ (全レーン、arrangement + session) へ伝播。
    pub(crate) fn set_automation_clip_color(
        &mut self,
        target: common::model::AutomationClipKey,
        color: Option<[f32; 3]>,
    ) {
        self.edit_song(|song| propagate_automation_clip_color(song, target, color));
    }
}

/// [`propagate_clip_color`] のオートメーションクリップ版: target の `content_id` を共有する
/// **全レーン (track lanes + song lanes、arrangement + session) の全 clip** へ伝播する。
/// `content_id == 0` (未採番 sentinel) は target のみ (defensive、`propagate_clip_color` と同じ)。
pub(crate) fn propagate_automation_clip_color(
    song: &mut common::model::Song,
    target: common::model::AutomationClipKey,
    color: Option<[f32; 3]>,
) {
    let content_id = song
        .automation_lane_by_key(target.track, target.lane)
        .and_then(|l| l.clip_by_id(target.clip))
        .map(|c| c.content_id);
    match content_id {
        Some(cid) if cid != 0 => {
            let lanes = song
                .tracks
                .iter_mut()
                .flat_map(|t| t.automation_lanes.iter_mut())
                .chain(song.song_lanes.iter_mut());
            for lane in lanes {
                for clip in lane.all_clips_mut().filter(|c| c.content_id == cid) {
                    clip.color = color;
                }
            }
        }
        _ => {
            if let Some(clip) = song
                .automation_lane_by_key_mut(target.track, target.lane)
                .and_then(|l| l.clip_by_id_mut(target.clip))
            {
                clip.color = color;
            }
        }
    }
}
