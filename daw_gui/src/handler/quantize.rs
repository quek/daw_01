//! handler::quantize — 選択ノートの始まり / 選択したオートメーションの点を格子へ寄せる
//! (ピアノロールのツールバーの「Quantize」・`Ctrl+U`・点の右クリック「格子に揃える」)。
//!
//! 格子は **曲の拍 0 を原点にした** 絶対位置 ([`common::snap::quantize_content_beat`])。clip が格子の上から
//! 始まらなくても / content のオフセットがあっても、曲の上で格子に載る (ドラッグのスナップと同じ)。
//! ランチャーのセルは時間軸の原点がセルの先頭 (`start_beat = 0`) なので、同じ式がセルの先頭からの格子になる。
//! 格子の幅は送り手が決めて event に載せる ([`AppData::note_quantize_grid`] /
//! [`AppData::automation_point_quantize_grid`]) — 分割はスナップの ON / OFF に依らない (`SnapConfig::grid_unit`)。
use crate::state::*;
use crate::app_types::*;

use common::model::ClipContent;
use common::snap::quantize_content_beat;

/// 窓の端の許容 (浮動小数の誤差で窓の中の点を「外」と判定しない)。
const WINDOW_EPS: f64 = 1e-9;

impl AppData {
    /// ピアノロールで選んでいる分割 1 つ分 (拍)。スナップ OFF でも同じ値 (`Adaptive` はピアノロールのズーム)。
    #[must_use]
    pub fn note_quantize_grid(&self) -> Option<f64> {
        crate::view::snap::piano_roll_snap_config(self).grid_unit(self.pianoroll_zoom_x())
    }

    /// オートメーションの点の格子 (拍) = 点のドラッグと同じアレンジのスナップ設定の分割 1 つ分。
    /// `zoom_x` は `Adaptive` の段を決める、点が描かれている面の px/拍。
    #[must_use]
    pub fn automation_point_quantize_grid(&self, zoom_x: f32) -> Option<f64> {
        crate::view::snap::arrange_snap_config(self).grid_unit(zoom_x)
    }

    /// `Ctrl+U` の宛先: 編集面 ([`Self::edit_surface`] — 最後に選んだ面) がノートならノート、
    /// オートメーションの点なら点を寄せる event。それ以外の面 / 分割が決まらないときは `None`。
    /// 点の `Adaptive` の段はアレンジのズームで決める (Automation Editor の上で押しても同じ)。
    #[must_use]
    pub fn quantize_selection_event(&self, surface: Option<EditSurface>) -> Option<crate::event::AppEvent> {
        use crate::event::AppEvent;
        match surface? {
            EditSurface::Notes => Some(AppEvent::QuantizeSelectedNotes { grid_beats: self.note_quantize_grid()? }),
            EditSurface::AutomationPoints => Some(AppEvent::QuantizeSelectedAutomationPoints {
                grid_beats: self.automation_point_quantize_grid(self.cur.view.arrange_zoom_x)?,
            }),
            _ => None,
        }
    }

    /// 選択ノートの始まりを `grid_beats` 拍の格子へ寄せる (長さは変えない)。重なりは
    /// `edit_clip_notes` が寄せた側を勝者として解消し、選択 (packed id) も追従させる。
    /// クリップごとに `edit_song` を呼ぶが、1 event = 1 undo step (`SongDoc::event_scope`)。
    pub(crate) fn quantize_selected_notes(&mut self, grid_beats: f64) {
        if !(grid_beats.is_finite() && grid_beats > 0.0) {
            return;
        }
        let selected = self.selected_note_ids();
        if selected.is_empty() {
            return;
        }
        self.for_each_note_clip_group(selected.into_iter().map(|id| (id, ())), |app, slot, r, items| {
            let origin = app.clip_start_beat_of(r);
            let locals: Vec<usize> = items.iter().map(|&(local, ())| local).collect();
            app.edit_clip_notes(slot, r, move |notes| {
                let mut winners = Vec::with_capacity(locals.len());
                for local in locals {
                    if let Some(n) = notes.get_mut(local) {
                        n.start_beat = quantize_content_beat(n.start_beat, origin, grid_beats);
                        winners.push(u32::try_from(local).unwrap_or(u32::MAX));
                    }
                }
                winners
            });
        });
    }

    /// 選択したオートメーションの点の時刻を `grid_beats` 拍の格子へ寄せる。
    ///
    /// 点は選んだクリップ (`clip_id`) の原点で絶対位置にする — content を共有する別のクリップから選んだ点は
    /// そのクリップの原点で寄せる。窓の中にあった点は窓の外へ出さない (Automation Editor のドラッグと同じ)。
    /// 点の並び (時刻の昇順) を保つために content ごとに並べ直し、選択の位置 (`point_idx`) も付け替える。
    pub(crate) fn quantize_selected_automation_points(&mut self, grid_beats: f64) {
        if !(grid_beats.is_finite() && grid_beats > 0.0) {
            return;
        }
        let selected = self.cur.selection.selected_automation_points.clone();
        // content ごとの (点の位置, 寄せた時刻)。同じ点を 2 つのクリップから選んでいたら先の 1 つ。
        let mut moves: std::collections::BTreeMap<common::model::ContentId, Vec<(usize, f64)>> =
            std::collections::BTreeMap::new();
        for k in &selected {
            let song = self.cur.song_doc.song();
            let Some(clip) = song.automation_lane_by_key(k.track_id, k.lane_id).and_then(|l| l.clip_by_id(k.clip_id))
            else {
                continue;
            };
            let Some(p) = song.clip_contents.get(&clip.content_id).and_then(ClipContent::automation_points).and_then(|pts| pts.get(k.point_idx as usize))
            else {
                continue;
            };
            let idx = k.point_idx as usize;
            let list = moves.entry(clip.content_id).or_default();
            if list.iter().any(|&(i, _)| i == idx) {
                continue;
            }
            let (w0, w1) = clip.content_window();
            let mut t = quantize_content_beat(p.time_beat, clip.content_origin_beat(), grid_beats);
            if p.time_beat >= w0 - WINDOW_EPS && p.time_beat <= w1 + WINDOW_EPS {
                t = t.clamp(w0.max(0.0), w1.max(0.0));
            }
            list.push((idx, t));
        }
        if moves.is_empty() {
            return;
        }
        // content ごとの「元の位置 → 並べ直した後の位置」。
        let Some(remaps) = self.edit_song(move |song| {
            let mut remaps: std::collections::BTreeMap<common::model::ContentId, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (content_id, list) in moves {
                let Some(ClipContent::Automation(a)) = song.clip_contents.get_mut(&content_id) else {
                    continue;
                };
                for (idx, t) in list {
                    if let Some(p) = a.points.get_mut(idx) {
                        p.time_beat = t;
                    }
                }
                remaps.insert(content_id, sort_points_by_time(&mut a.points));
            }
            remaps
        }) else {
            return;
        };
        let song = self.cur.song_doc.song();
        let remapped: Vec<AutomationPointKeyRef> = selected
            .into_iter()
            .map(|k| {
                let content = song.automation_lane_by_key(k.track_id, k.lane_id).and_then(|l| l.clip_by_id(k.clip_id)).map(|c| c.content_id);
                let new_idx = content.and_then(|c| remaps.get(&c)).and_then(|m| m.get(k.point_idx as usize));
                match new_idx {
                    Some(&i) => AutomationPointKeyRef { point_idx: u32::try_from(i).unwrap_or(u32::MAX), ..k },
                    None => k,
                }
            })
            .collect();
        self.cur.selection.selected_automation_points = remapped;
    }
}

/// 点を時刻の昇順へ並べ直し (同時刻は元の順 = 安定)、`[元の位置] = 新しい位置` を返す。
fn sort_points_by_time(points: &mut Vec<common::model::AutomationPoint>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..points.len()).collect();
    order.sort_by(|&i, &j| points[i].time_beat.total_cmp(&points[j].time_beat));
    let mut new_pos = vec![0; points.len()];
    for (pos, &old) in order.iter().enumerate() {
        new_pos[old] = pos;
    }
    let old = std::mem::take(points);
    let mut slots: Vec<Option<common::model::AutomationPoint>> = old.into_iter().map(Some).collect();
    points.extend(order.iter().filter_map(|&i| slots[i].take()));
    new_pos
}

#[cfg(test)]
mod tests {
    use common::model::{
        AutomationClip, AutomationContent, AutomationLane, AutomationPoint, AutomationTarget, Clip, ClipContent,
        ClipKey, ContentId, LaunchSettings, MidiContent, Note, SessionAutomationClip, Track, TrackBuiltinParam,
    };

    use crate::app::{AppData, AppEvent, AutomationPointKeyRef};

    const GRID: f64 = 1.0;

    fn note(id: u32, start_beat: f64, pitch: u8) -> Note {
        Note { id, start_beat, duration_beats: 0.5, pitch, velocity: 100, ..Note::default() }
    }

    /// 曲の拍で見たノートの始まり (`key` のクリップの原点で換算)。
    fn song_starts(app: &AppData, key: ClipKey) -> Vec<f64> {
        let song = app.cur.song_doc.song();
        let clip = song.clip_by_key(key).expect("clip");
        song.clip_notes(clip).iter().map(|n| clip.content_to_song_beat(n.start_beat)).collect()
    }

    /// 格子の上から始まらず content のオフセットもあるクリップ (原点 = 曲の 1.25 拍) と、格子の上のクリップ。
    /// 両方のノートを全部選ぶ。
    fn app_with_two_midi_clips() -> (AppData, ClipKey, ClipKey) {
        let mut app = crate::test_support::headless_app();
        let (a, b) = (ClipKey { track_id: 1, clip_id: 1 }, ClipKey { track_id: 1, clip_id: 2 });
        app.edit_song(|song| {
            song.tracks.clear();
            let midi = |notes| ClipContent::Midi(MidiContent { notes, next_note_id: 3 });
            song.clip_contents.insert(101, midi(vec![note(1, 1.0, 60), note(2, 2.6, 62)]));
            song.clip_contents.insert(102, midi(vec![note(1, 0.4, 64)]));
            let clip = |id, start_beat, content_offset_beats, content_id| Clip {
                id,
                start_beat,
                length_beats: 4.0,
                content_offset_beats,
                content_id,
                ..Clip::default()
            };
            song.tracks.push(Track {
                id: 1,
                clips: vec![clip(1, 1.5, 0.25, 101), clip(2, 8.0, 0.0, 102)],
                next_clip_id: 3,
                ..Track::default()
            });
        });
        app.handle_event(AppEvent::SelectClip { target: a, additive: false });
        app.handle_event(AppEvent::SelectClip { target: b, additive: true });
        let mut ids = app.pack_note_ids_for(a, &[0, 1]);
        ids.extend(app.pack_note_ids_for(b, &[0]));
        app.set_note_selection(&ids);
        assert_eq!(app.selected_note_ids().len(), 3, "前提: 3 音とも選ばれている");
        (app, a, b)
    }

    /// クリップの位置と content のオフセットに依らず、曲の格子へ載る (clip-local の拍で丸めると
    /// 原点 1.25 拍のクリップは格子から 0.25 拍ずれたまま)。長さは変えず、2 クリップで 1 undo step。
    #[test]
    fn ノートの始まりを曲の格子へ寄せ長さは変えず_1_回の_undo_で戻る() {
        let (mut app, a, b) = app_with_two_midi_clips();
        let before = app.cur.song_doc.song().clone();

        app.handle_event(AppEvent::QuantizeSelectedNotes { grid_beats: GRID });

        assert_eq!(song_starts(&app, a), vec![2.0, 4.0], "2.25 → 2、3.85 → 4 (曲の拍)");
        assert_eq!(song_starts(&app, b), vec![8.0]);
        let song = app.cur.song_doc.song();
        let durations: Vec<f64> = [a, b]
            .iter()
            .flat_map(|k| song.clip_notes(song.clip_by_key(*k).expect("clip")).iter().map(|n| n.duration_beats))
            .collect();
        assert_eq!(durations, vec![0.5; 3], "長さは変えない");
        assert_eq!(app.selected_note_ids().len(), 3, "寄せた後も選んだまま");

        app.handle_event(AppEvent::Undo);
        assert_eq!(app.cur.song_doc.song().clip_contents, before.clip_contents, "1 回の undo で両方のクリップが戻る");
    }

    const LANE_CONTENT: ContentId = 201;
    const CELL_CONTENT: ContentId = 202;

    fn pt(id: u32, time_beat: f64) -> AutomationPoint {
        AutomationPoint { id, time_beat, value: f64::from(id), ..AutomationPoint::default() }
    }

    fn key(clip_id: u32, point_idx: u32) -> AutomationPointKeyRef {
        AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id, point_idx }
    }

    /// Volume レーンに、原点 1.25 拍 (start 1.5 / offset 0.25) のアレンジのクリップと、offset 1 拍のセル。
    fn app_with_automation(lane_points: Vec<AutomationPoint>, cell_points: Vec<AutomationPoint>) -> AppData {
        let mut app = crate::test_support::headless_app();
        app.edit_song(move |song| {
            song.tracks.clear();
            let scene_id = song.ensure_scene_at(0);
            let content = |points| ClipContent::Automation(AutomationContent { points, next_point_id: 10 });
            song.clip_contents.insert(LANE_CONTENT, content(lane_points));
            song.clip_contents.insert(CELL_CONTENT, content(cell_points));
            let clip = |id, start_beat, content_offset_beats, content_id| AutomationClip {
                id,
                start_beat,
                length_beats: 4.0,
                content_offset_beats,
                content_id,
                ..AutomationClip::default()
            };
            song.tracks.push(Track {
                id: 1,
                automation_lanes: vec![AutomationLane {
                    id: 1,
                    clips: vec![clip(1, 1.5, 0.25, LANE_CONTENT)],
                    session_clips: vec![SessionAutomationClip {
                        scene_id,
                        clip: clip(2, 0.0, 1.0, CELL_CONTENT),
                        launch: LaunchSettings::default(),
                    }],
                    next_clip_id: 3,
                    ..AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), 1.0)
                }],
                ..Track::default()
            });
        });
        app
    }

    fn times(app: &AppData, content: ContentId) -> Vec<(u32, f64)> {
        let song = app.cur.song_doc.song();
        let pts = song.clip_contents.get(&content).and_then(ClipContent::automation_points).expect("points");
        pts.iter().map(|p| (p.id, p.time_beat)).collect()
    }

    /// アレンジのクリップの点は曲の格子へ (local 0.5 = 曲 1.75 → 2 = local 0.75)。寄せた点が選んでいない点を
    /// 追い越したら並べ直し、選択は動いた点についていく。
    #[test]
    fn アレンジのクリップの点は曲の格子へ寄せ追い越しても選択がついてくる() {
        let mut app = app_with_automation(vec![pt(1, 0.5), pt(2, 2.4), pt(3, 2.6)], Vec::new());
        app.handle_event(AppEvent::SelectAutomationPoints { prev: Vec::new(), next: vec![key(1, 0), key(1, 1)] });

        app.handle_event(AppEvent::QuantizeSelectedAutomationPoints { grid_beats: GRID });

        // 点 2: local 2.4 = 曲 3.65 → 4 = local 2.75 (選んでいない点 3 の 2.6 を追い越す)。
        assert_eq!(times(&app, LANE_CONTENT), vec![(1, 0.75), (3, 2.6), (2, 2.75)]);
        assert_eq!(app.cur.selection.selected_automation_points, vec![key(1, 0), key(1, 2)], "選択は点 1 と点 2 のまま");

        app.handle_event(AppEvent::Undo);
        assert_eq!(times(&app, LANE_CONTENT), vec![(1, 0.5), (2, 2.4), (3, 2.6)], "1 回の undo で戻る");
    }

    /// セルの点はセルの先頭からの格子 (offset 1 拍: local 1.3 = 窓の 0.3 → 0 = local 1)。窓の中の点は
    /// 窓の外へ出さない (local 4.9 = 窓の 3.9 → 4 = 窓の終わり)。
    #[test]
    fn セルの点はセルの先頭からの格子へ寄せる() {
        let mut app = app_with_automation(Vec::new(), vec![pt(1, 1.3), pt(2, 4.9)]);
        app.handle_event(AppEvent::SelectAutomationPoints { prev: Vec::new(), next: vec![key(2, 0), key(2, 1)] });

        app.handle_event(AppEvent::QuantizeSelectedAutomationPoints { grid_beats: GRID });

        assert_eq!(times(&app, CELL_CONTENT), vec![(1, 1.0), (2, 5.0)]);
    }
}
