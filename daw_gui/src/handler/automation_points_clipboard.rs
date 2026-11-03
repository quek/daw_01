//! handler::automation_points_clipboard — オートメーションの点の copy / paste
//! (`handler::automation` から分割)。貼り先はレーン + 拍 (アレンジ) か、クリップの key
//! (Automation Editor)。

use common::model::{
    AutomationClipKey, AutomationContent, AutomationLaneKey, AutomationPoint, AutomationTarget,
    ClipContent,
};

use crate::app_types::{AutomationPointKeyRef, EditSurface};
use crate::clipboard::{ClipboardEnvelope, ClipboardPayload, CopiedPoint};
use crate::state::AppData;

impl AppData {
    /// Phase 3: 選択中 automation point を JSON 化して OS clipboard に
    /// 出せるよう text を返す。 [`Self::copy_notes_clip`] と同
    /// idiom。 point の `value` は target ごとに値域が違う (Volume:
    /// 0..=2.0、 Pan: -1..=1 等) ので、 lane の `target` を引いて
    /// **normalized 0..=1** で serialize する。 paste 側でも target を
    /// 引いて plain に戻す (= target が違う lane に貼っても curve の
    /// shape を保てる、 Bitwig 流)。
    ///
    /// 戻り値は `(json, count)`。 何も copy できない (選択無し / lookup
    /// 失敗) 場合は `None`。
    pub fn copy_points_clip(&self) -> Option<(String, usize)> {
        if self.cur.selection.selected_automation_points.is_empty() {
            return None;
        }
        let mut copied: Vec<CopiedPoint> =
            Vec::with_capacity(self.cur.selection.selected_automation_points.len());
        for k in &self.cur.selection.selected_automation_points {
            let Some(lane) = self.cur.song_doc.song().automation_lane_by_key(k.track_id, k.lane_id) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip_id) else {
                continue;
            };
            let Some(ClipContent::Automation(a)) =
                self.cur.song_doc.song().clip_contents.get(&clip.content_id)
            else {
                continue;
            };
            let Some(p) = a.points.get(k.point_idx as usize) else {
                continue;
            };
            let value_norm = common::automation::plain_to_norm_ranged(
                &lane.target,
                p.value,
                self.plugin_param_range(&lane.target),
            );
            copied.push(CopiedPoint { time_beat: p.time_beat, value_norm, curve: p.curve });
        }
        if copied.is_empty() {
            return None;
        }
        // earliest time_beat を anchor として 0.0 にシフト (Note と同じ)。
        let earliest = copied.iter().map(|p| p.time_beat).fold(f64::INFINITY, f64::min);
        if earliest.is_finite() {
            for p in &mut copied {
                p.time_beat -= earliest;
            }
        }
        let count = copied.len();
        let json = ClipboardEnvelope::new(
            self.cur.song_doc.song().project_id,
            ClipboardPayload::AutomationPoints(copied),
        )
        .to_json()?;
        Some((json, count))
    }

    /// `CopiedPoint` 群を「マウス下の automation lane」の `song_beat`
    /// (song-absolute 拍) を含む automation clip に貼る。clip が無い (レーンの空き)
    /// なら no-op + status。`song_beat - clip.start` を clip-local anchor とし、各 point の
    /// 相対 `time_beat` を加算。value は lane.target に応じ norm→plain 復元して sort 維持
    /// insert。貼った点群を新選択にする。戻り値は挿入数。
    pub fn paste_points_at(
        &mut self,
        points_in: Vec<CopiedPoint>,
        lane_key: AutomationLaneKey,
        song_beat: f64,
    ) -> usize {
        let Some(lane) = self.cur.song_doc.song().automation_lane_by_key(lane_key.track, lane_key.lane) else {
            return 0;
        };
        let Some(clip) = lane
            .clips
            .iter()
            .find(|c| song_beat >= c.start_beat && song_beat < c.start_beat + c.length_beats)
        else {
            if !points_in.is_empty() {
                self.ui_ephemeral.status_message =
                    "貼り付け先の automation clip がありません (レーンの空き)".to_string();
            }
            return 0;
        };
        let dest_key = AutomationClipKey { track: lane_key.track, lane: lane_key.lane, clip: clip.id };
        // r.md #44: curve 上の貼り付け位置は content 原点基準。
        let anchor = clip.song_to_content_beat(song_beat).max(0.0);
        self.paste_points_at_content_beat(points_in, dest_key, anchor)
    }

    /// `CopiedPoint` 群を Automation Editor で開いているクリップの `window_beat`
    /// (窓ローカル拍) に貼る。セルはレーンの時間軸に居ないので拍では引けず、key で指す。
    pub fn paste_points_into_clip(
        &mut self,
        points_in: Vec<CopiedPoint>,
        dest_key: AutomationClipKey,
        window_beat: f64,
    ) -> usize {
        let Some(offset) = self
            .cur.song_doc.song()
            .automation_lane_by_key(dest_key.track, dest_key.lane)
            .and_then(|l| l.clip_by_id(dest_key.clip))
            .map(|c| c.content_offset_beats)
        else {
            return 0;
        };
        self.paste_points_at_content_beat(points_in, dest_key, (window_beat + offset).max(0.0))
    }

    /// [`Self::paste_points_at`] / [`Self::paste_points_into_clip`] の本体。`anchor` は
    /// 貼り先クリップの content-local 拍。貼った点群を新選択にする。
    fn paste_points_at_content_beat(
        &mut self,
        points_in: Vec<CopiedPoint>,
        dest_key: AutomationClipKey,
        anchor: f64,
    ) -> usize {
        if points_in.is_empty() {
            return 0;
        }
        let song = self.cur.song_doc.song();
        let Some(lane) = song.automation_lane_by_key(dest_key.track, dest_key.lane) else {
            return 0;
        };
        let Some(clip) = lane.clip_by_id(dest_key.clip) else {
            return 0;
        };
        let target = lane.target.clone();
        let range = self.plugin_param_range(&target);
        let content_id = clip.content_id;

        // dest content が automation でない壊れたモデルなら undo を触る前に bail。
        if let Some(c) = song.clip_contents.get(&content_id)
            && !matches!(c, ClipContent::Automation(_))
        {
            self.ui_ephemeral.status_message =
                "貼り付け先 clip が automation でない (型不整合)".to_string();
            return 0;
        }

        let count = points_in.len();
        let Some(Some(new_indices)) = self.edit_song(move |song| {
            let entry = song
                .clip_contents
                .entry(content_id)
                .or_insert_with(|| ClipContent::Automation(AutomationContent::default()));
            let ClipContent::Automation(content) = entry else {
                return None;
            };
            Some(insert_copied_points(content, &points_in, &target, range, anchor))
        }) else {
            return 0;
        };

        self.cur.selection.selected_automation_points = new_indices
            .into_iter()
            .map(|i| AutomationPointKeyRef {
                track_id: dest_key.track,
                lane_id: dest_key.lane,
                clip_id: dest_key.clip,
                point_idx: i,
            })
            .collect();
        self.cur.selection.last_edit_select = Some(EditSurface::AutomationPoints);
        count
    }
}

/// 点を時間順を保って挿入し、挿入した点の新しい index を返す。index は挿入のたびに変わるので、
/// 全部入れ終えてから「挿入した値ペア」で引き直す。
fn insert_copied_points(
    content: &mut AutomationContent,
    points_in: &[CopiedPoint],
    target: &AutomationTarget,
    range: Option<(f64, f64)>,
    anchor: f64,
) -> Vec<u32> {
    let mut inserted: Vec<(f64, f64)> = Vec::with_capacity(points_in.len());
    for src in points_in {
        let value = common::automation::norm_to_plain_ranged(target, src.value_norm, range);
        let time_beat = (src.time_beat + anchor).max(0.0);
        // v29: 新規 point は allocator で安定 id を採番。
        let new_point = AutomationPoint { id: content.alloc_point_id(), time_beat, value, curve: src.curve };
        let insert_at = content.points.partition_point(|p| p.time_beat <= time_beat);
        content.points.insert(insert_at, new_point);
        inserted.push((time_beat, value));
    }
    let index_of = |&(t, v): &(f64, f64)| {
        let found = content.points.iter().position(|p| (p.time_beat - t).abs() < 1e-9 && (p.value - v).abs() < 1e-9);
        #[allow(clippy::cast_possible_truncation)]
        found.map(|i| i as u32)
    };
    inserted.iter().filter_map(index_of).collect()
}
