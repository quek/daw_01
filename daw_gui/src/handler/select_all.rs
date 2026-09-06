//! handler::select_all — Ctrl+A の「全選択」を面ごとに解く `impl AppData` (grill-me 2026-06-09 / 2026-09-06)。
//!
//! アレンジは段階拡大 (`select_all_arrangement`)、ピアノロール / オーディオエディタ /
//! automation lane はそれぞれ「いま見えているものを全部」 の集合を返す helper。
//! 選択の SSoT は範囲 1 本 (`docs/plan_range_selection.md`) なので、ここは範囲を張るか、
//! 範囲へ変換する前の id 集合を作るかのどちらかしかしない。
use crate::state::*;
use crate::app_types::*;

impl AppData {
    /// Ctrl+A (アレンジ): **段階拡大**で範囲を張る (grill-me 2026-09-06)。
    ///
    /// 1. `track` が指すトラック 1 本 (トラック行 + そのトラックの **全** automation
    ///    lane、閉じている lane も含む) を、そこにあるクリップ (automation クリップ含む)
    ///    の外接区間で覆う。
    /// 2. 今の範囲が既に 1 の結果と一致している (または 1 に覆うクリップが無い /
    ///    `track` が `None`) なら、全トラック × 全 lane (master の song lane も含む)
    ///    を全クリップの外接で覆う。
    ///
    /// 段の判定は「今の範囲が前段の結果と一致するか」だけで、押した回数は数えない
    /// (押す間に対象トラックが変われば 1 からやり直し)。 automation lane 上の
    /// 「点 → lane のクリップ」 段は `root.rs` 側が先に消化し、その次にここへ来る。
    ///
    /// 一括操作なので view ジャンプ (fit / トラック追従) を起こさず、既に全選択なら
    /// 冪等。 selection のみ更新で非 undoable。
    pub(crate) fn select_all_arrangement(&mut self, track: Option<u32>) {
        let all = self.all_tracks_selection();
        let cur = self.selection.time.as_ref();
        let next = if all.is_some() && cur == all.as_ref() {
            // 既に全トラック = 最上段。 1 段目へ戻さない (冪等)。
            all
        } else {
            match track.and_then(|id| self.all_in_track_selection(id)) {
                Some(t1) if cur != Some(&t1) => Some(t1),
                _ => all,
            }
        };
        let Some(next) = next else {
            return;
        };
        // 冪等 early-return より前に last-wins 面だけは更新する (既に全選択でも
        // 「Ctrl+A = 範囲面を選んだ」 という意図は確定している)。
        self.selection.last_edit_select = Some(EditSurface::TimeRange);
        if self.selection.time.as_ref() != Some(&next) {
            self.selection.range_anchor = Some(next.start_beat);
            self.selection.time = Some(next);
        }
        // 冪等 early-return より後 (既に全選択でも「アレンジの面を選んだ」は確定)。
        self.drop_cell_selection_if_arrangement();
    }

    /// 1 トラックの全行 (トラック行 + 全 automation lane) を、そのクリップの外接で
    /// 覆う範囲。 `MASTER_TRACK_ID` は song lane 行だけ。 クリップが 1 つも無ければ `None`。
    fn all_in_track_selection(&self, track_id: u32) -> Option<common::model::TimeSelection> {
        let song = self.song_doc.song();
        let mut lanes = Vec::new();
        if track_id == common::model::MASTER_TRACK_ID {
            lanes.extend(song.song_lanes.iter().map(|l| {
                common::model::LaneRef::Automation(common::model::AutomationLaneKey {
                    track: track_id,
                    lane: l.id,
                })
            }));
        } else {
            let track = song.track_by_id(track_id)?;
            lanes.push(common::model::LaneRef::Track(track_id));
            lanes.extend(track.automation_lanes.iter().map(|l| {
                common::model::LaneRef::Automation(common::model::AutomationLaneKey {
                    track: track_id,
                    lane: l.id,
                })
            }));
        }
        self.arrangement_lanes_extent(lanes)
    }

    /// 全トラック行 + 全 automation lane (master の song lane も) を、全クリップの
    /// 外接で覆う範囲。 クリップが 1 つも無ければ `None`。
    fn all_tracks_selection(&self) -> Option<common::model::TimeSelection> {
        let song = self.song_doc.song();
        let mut lanes = Vec::new();
        for t in &song.tracks {
            lanes.push(common::model::LaneRef::Track(t.id));
            lanes.extend(t.automation_lanes.iter().map(|l| {
                common::model::LaneRef::Automation(common::model::AutomationLaneKey {
                    track: t.id,
                    lane: l.id,
                })
            }));
        }
        lanes.extend(song.song_lanes.iter().map(|l| {
            common::model::LaneRef::Automation(common::model::AutomationLaneKey {
                track: common::model::MASTER_TRACK_ID,
                lane: l.id,
            })
        }));
        self.arrangement_lanes_extent(lanes)
    }

    /// 与えた行集合にあるクリップ (トラック行のクリップ + lane 行の automation クリップ)
    /// の外接区間 × その行集合。 クリップが無ければ `None`。
    fn arrangement_lanes_extent(
        &self,
        lanes: Vec<common::model::LaneRef>,
    ) -> Option<common::model::TimeSelection> {
        let song = self.song_doc.song();
        let (mut start, mut end) = (f64::INFINITY, f64::NEG_INFINITY);
        let mut cover = |s: f64, len: f64| {
            start = start.min(s);
            end = end.max(s + len);
        };
        for lane in &lanes {
            match *lane {
                common::model::LaneRef::Track(id) => {
                    for c in song.track_by_id(id).map_or(&[][..], |t| t.clips.as_slice()) {
                        cover(c.start_beat, c.length_beats);
                    }
                }
                common::model::LaneRef::Automation(key) => {
                    for c in song
                        .automation_lane_by_key(key.track, key.lane)
                        .map_or(&[][..], |l| l.clips.as_slice())
                    {
                        cover(c.start_beat, c.length_beats);
                    }
                }
                common::model::LaneRef::KeyTrack { .. } | common::model::LaneRef::AudioLane(_) => {}
            }
        }
        if !start.is_finite() {
            return None;
        }
        common::model::TimeSelection::new(start, end, lanes)
    }

    /// Ctrl+A (ピアノロール): **表示中の全 MIDI クリップ** の全ノートを packed note id で返す。
    /// 各 id = `pack_note_id(clip_slot, local_index)`。ロック中クリップは選択対象に
    /// しない (掴めないので除外)。表示クリップが無ければ空。
    pub fn all_shown_pianoroll_note_ids(&self) -> Vec<u32> {
        let shown = self.shown_pianoroll_clips();
        let mut out = Vec::new();
        for (slot, &r) in shown.iter().enumerate() {
            if self.is_pianoroll_clip_locked_in(&shown, r) {
                continue;
            }
            let Some(track) = self.song_doc.song().track_by_id(r.track_id) else {
                continue;
            };
            let Some(clip) = track.clip_by_id(r.clip_id) else {
                continue;
            };
            let n = self.song_doc.song().clip_notes(clip).len();
            out.extend((0..n).map(|local| Self::pack_note_id(slot, local)));
        }
        out
    }

    /// Ctrl+A (オーディオエディタ): 開いている clip の全 audio event index
    /// を返す。 audio_editor_clip が無い / 非 audio なら空。
    pub fn all_audio_event_indices(&self) -> Vec<usize> {
        let Some(target) = self.ui_ephemeral.audio_editor_clip else {
            return Vec::new();
        };
        let Some(track) = self.song_doc.song().track_by_id(target.track_id) else {
            return Vec::new();
        };
        let Some(clip) = track.clip_by_id(target.clip_id) else {
            return Vec::new();
        };
        match self.song_doc.song().clip_contents.get(&clip.content_id) {
            Some(common::model::ClipContent::Audio(audio)) => (0..audio.events.len()).collect(),
            _ => Vec::new(),
        }
    }

    /// Ctrl+A (automation lane): 指定 lane 内の全ポイントを
    /// `AutomationPointKeyRef` で列挙する。 lane.clips の各 clip の content
    /// (`ClipContent::Automation`) points を走査。 master row
    /// (`MASTER_TRACK_ID`) も `automation_lane_by_key` 経由で対応。
    /// lane が無い / ポイントが無いなら空。
    pub fn all_automation_points_in_lane(
        &self,
        lane: common::model::AutomationLaneKey,
    ) -> Vec<AutomationPointKeyRef> {
        let Some(lane_ref) = self.song_doc.song().automation_lane_by_key(lane.track, lane.lane) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for clip in &lane_ref.clips {
            let n = match self.song_doc.song().clip_contents.get(&clip.content_id) {
                Some(common::model::ClipContent::Automation(a)) => a.points.len(),
                _ => 0,
            };
            for point_idx in 0..n as u32 {
                out.push(AutomationPointKeyRef {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    clip_id: clip.id,
                    point_idx,
                });
            }
        }
        out
    }

    /// Ctrl+A (automation lane / #071): 指定 lane 内の全 automation clip を
    /// `AutomationClipKey` で列挙する。 lane が無い / clip が無いなら空。
    /// `all_automation_points_in_lane` の clip 版 (= Ctrl+A 段階拡大の clip 段)。
    pub fn all_automation_clips_in_lane(
        &self,
        lane: common::model::AutomationLaneKey,
    ) -> Vec<common::model::AutomationClipKey> {
        let Some(lane_ref) = self.song_doc.song().automation_lane_by_key(lane.track, lane.lane) else {
            return Vec::new();
        };
        lane_ref
            .clips
            .iter()
            .map(|clip| common::model::AutomationClipKey {
                track: lane.track,
                lane: lane.lane,
                clip: clip.id,
            })
            .collect()
    }
}
