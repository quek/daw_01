//! app.rs から分割した unit test 群 (元は各 free fn / associated fn と同一
//! module に居たので、 テスト対象を `crate::app` / `crate::app_types` へ再指定)。
#![cfg(test)]
#[cfg(test)]
mod lipsync_merge_tests {
    use crate::app_types::merge_lipsync_events_by_priority;

    #[test]
    fn non_overlapping_sources_both_kept() {
        // 上(prio 0) [0,2) img1、下(prio 1) [3,5) img2 — 重ならない → 両方残る。
        let m = merge_lipsync_events_by_priority(vec![(0.0, 2.0, 1, 0), (3.0, 5.0, 2, 1)]);
        assert_eq!(m, vec![(0.0, 2.0, 1), (3.0, 5.0, 2)]);
    }

    #[test]
    fn overlap_upper_priority_wins() {
        // 上(prio 0) [1,3) img1、下(prio 1) [0,4) img2。重なる [1,3) は上が勝ち、
        // 下は [0,1) と [3,4) のみ残る。
        let m = merge_lipsync_events_by_priority(vec![(1.0, 3.0, 1, 0), (0.0, 4.0, 2, 1)]);
        assert_eq!(m, vec![(0.0, 1.0, 2), (1.0, 3.0, 1), (3.0, 4.0, 2)]);
    }

    #[test]
    fn adjacent_same_image_coalesced() {
        let m = merge_lipsync_events_by_priority(vec![(0.0, 1.0, 5, 0), (1.0, 2.0, 5, 1)]);
        assert_eq!(m, vec![(0.0, 2.0, 5)]);
    }
}

#[cfg(test)]
mod lipsync_fingerprint_tests {
    //! `lipsync_input_fingerprint` が「口パク出力に影響する入力が変わったときだけ」
    //! 変化することを保証する。報告バグ (背景 track の rename で口パクが再生成
    //! される) の回帰ガード + 入力フィールド選択の精度ガード。
    use crate::app::AppData;
    use crate::app_types::track_with;
    use common::model::{Clip, ClipContent, MidiContent, MouthMap, Note, Song};

    const MOUTH_TRACK_ID: u32 = 2;

    fn note(pitch: u8, vel: u8, start: f64, dur: f64, lyric: &str) -> Note {
        Note {
            id: 0,
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: vel,
            lyric: Some(lyric.to_string()),
            muted: false,
        }
    }

    /// vocal track (id 1) → 口 track (id 2)。vocal に notes 入り MIDI clip、
    /// 口 track に mouth_map を設定した最小構成。
    fn base_song() -> Song {
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![note(60, 100, 0.0, 1.0, "ら")],
                next_note_id: 2,
            }),
        );
        let vocal = track_with(|t| {
            t.id = 1;
            t.lipsync_target_track = Some(MOUTH_TRACK_ID);
            t.clips = vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                ..Default::default()
            }];
        });
        let mouth = track_with(|t| {
            t.id = MOUTH_TRACK_ID;
            t.mouth_map = Some(MouthMap {
                a: 10,
                ..Default::default()
            });
        });
        song.tracks.push(vocal);
        song.tracks.push(mouth);
        song
    }

    fn fp(song: &Song) -> u64 {
        AppData::lipsync_input_fingerprint(song, MOUTH_TRACK_ID)
    }

    fn first_note(song: &mut Song, f: impl FnOnce(&mut Note)) {
        let cid = song.tracks[0].clips[0].content_id;
        match song.clip_contents.get_mut(&cid) {
            Some(ClipContent::Midi(m)) => f(&mut m.notes[0]),
            _ => panic!("expected midi content"),
        }
    }

    #[test]
    fn rename_any_track_keeps_fingerprint() {
        // 報告バグ: 背景 (口) track の rename で口パクが再生成されていた。
        let song = base_song();
        let before = fp(&song);
        let mut renamed = song.clone();
        renamed.tracks[0].name = "vocal renamed".to_string();
        renamed.tracks[1].name = "background renamed".to_string();
        assert_eq!(fp(&renamed), before, "rename は口パク入力ではない");
    }

    #[test]
    fn velocity_and_mute_keep_fingerprint() {
        // phoneme query が読まないフィールド (velocity / muted) は再生成不要。
        let song = base_song();
        let before = fp(&song);
        let mut edited = song.clone();
        first_note(&mut edited, |n| {
            n.velocity = 1;
            n.muted = true;
        });
        assert_eq!(fp(&edited), before);
    }

    #[test]
    fn note_pitch_changes_fingerprint() {
        let song = base_song();
        let before = fp(&song);
        let mut edited = song.clone();
        first_note(&mut edited, |n| n.pitch = 62);
        assert_ne!(fp(&edited), before);
    }

    #[test]
    fn lyric_changes_fingerprint() {
        let song = base_song();
        let before = fp(&song);
        let mut edited = song.clone();
        first_note(&mut edited, |n| n.lyric = Some("み".to_string()));
        assert_ne!(fp(&edited), before);
    }

    #[test]
    fn note_timing_changes_fingerprint() {
        let song = base_song();
        let before = fp(&song);
        let mut edited = song.clone();
        first_note(&mut edited, |n| n.start_beat = 0.5);
        assert_ne!(fp(&edited), before);
    }

    #[test]
    fn clip_move_changes_fingerprint() {
        let song = base_song();
        let before = fp(&song);
        let mut edited = song.clone();
        edited.tracks[0].clips[0].start_beat = 8.0;
        assert_ne!(fp(&edited), before);
    }

    #[test]
    fn mouth_map_change_changes_fingerprint() {
        let song = base_song();
        let before = fp(&song);
        let mut edited = song.clone();
        edited.tracks[1].mouth_map = Some(MouthMap {
            a: 10,
            i: 11,
            ..Default::default()
        });
        assert_ne!(fp(&edited), before);
    }

    #[test]
    fn bpm_change_changes_fingerprint() {
        let song = base_song();
        let before = fp(&song);
        let mut edited = song.clone();
        edited.bpm += 10.0;
        assert_ne!(fp(&edited), before);
    }
}

#[cfg(test)]
mod image_drop_target_tests {
    use crate::app_types::{resolve_media_drop_target, ImportTrackTarget};

    #[test]
    fn resolves_existing_track_or_falls_back_to_new() {
        use ImportTrackTarget::*;
        // 既存 track を指す (drop が乗った) → その index に貼り付け。
        assert_eq!(resolve_media_drop_target(Track(0), 3), Some(0));
        assert_eq!(resolve_media_drop_target(Track(2), 3), Some(2));
        // 範囲外 index (= track の無い下の領域へのドロップ) → 新規 track (None = 一番下)。
        assert_eq!(resolve_media_drop_target(Track(3), 3), None);
        assert_eq!(resolve_media_drop_target(Track(99), 3), None);
        // 空きスペース drop (NewTrackBottom) / dialog 経由 (NoHint) → 新規 track (None = 一番下)。
        assert_eq!(resolve_media_drop_target(NewTrackBottom, 3), None);
        assert_eq!(resolve_media_drop_target(NoHint, 3), None);
        // track が 0 本 → 何を指しても新規 track。
        assert_eq!(resolve_media_drop_target(Track(0), 0), None);
        assert_eq!(resolve_media_drop_target(NewTrackBottom, 0), None);
        assert_eq!(resolve_media_drop_target(NoHint, 0), None);
    }
}

#[cfg(test)]
mod follow_scroll_tests {
    use crate::app::AppData;
    use common::model::FollowMode;

    /// 再生追従スクロールの scroll_beat 計算 (純関数)。Page のページめくり境界、
    /// Scroll の中央固定 + 頭打ち、Off / 可視幅 0 の退化を 1 表で網羅する。
    #[test]
    fn follow_scroll_beat_pages_and_centers() {
        // (mode, playhead, scroll, visible_beats, expected_new_scroll)
        let cases = [
            // Off は常に view を動かさない。
            (FollowMode::Off, 100.0_f32, 0.0_f32, 16.0_f32, None),
            // Page: 可視範囲 [scroll, scroll+visible) 内なら据え置き。
            (FollowMode::Page, 0.0, 0.0, 16.0, None),
            (FollowMode::Page, 8.0, 0.0, 16.0, None),
            // Page: 右端 (= scroll+visible) 到達でプレイヘッドを左端へページめくり。
            (FollowMode::Page, 16.0, 0.0, 16.0, Some(16.0)),
            (FollowMode::Page, 20.0, 0.0, 16.0, Some(20.0)),
            // Page: 逆方向 (playhead < scroll、シーク / ループ折返し) も左端へ。
            (FollowMode::Page, 4.0, 16.0, 16.0, Some(4.0)),
            // Scroll: プレイヘッドを中央へ (playhead - visible/2)。
            (FollowMode::Scroll, 100.0, 0.0, 16.0, Some(92.0)),
            (FollowMode::Scroll, 10.0, 0.0, 16.0, Some(2.0)),
            // Scroll: 曲頭付近 (playhead < visible/2) は 0 で頭打ち → 据え置き。
            (FollowMode::Scroll, 4.0, 0.0, 16.0, None),
            // 可視幅 0 / 負は計算不能 → None (0 除算を避ける)。
            (FollowMode::Page, 100.0, 0.0, 0.0, None),
            (FollowMode::Scroll, 100.0, 0.0, 0.0, None),
        ];
        for (mode, ph, scroll, vis, expected) in cases {
            let got = AppData::follow_scroll_beat(mode, ph, scroll, vis);
            match (got, expected) {
                (None, None) => {}
                (Some(g), Some(e)) => assert!(
                    (g - e).abs() < 1e-4,
                    "mode={mode:?} ph={ph} scroll={scroll} vis={vis}: got {g}, want {e}"
                ),
                _ => panic!(
                    "mode={mode:?} ph={ph} scroll={scroll} vis={vis}: got {got:?}, want {expected:?}"
                ),
            }
        }
    }
}

#[cfg(test)]
mod stretch_remap_tests {
    use crate::app_types::stretch_remap;

    #[test]
    fn right_edge_stretch_scales_from_left() {
        // clip [0,4] を右端 drag で [0,8] (2x)。 左端固定。
        // spanning event (0,4) → (0,8)。
        let (s, l) = stretch_remap(0.0, 4.0, 0.0, 8.0, 0.0, 4.0);
        assert!((s - 0.0).abs() < 1e-9 && (l - 8.0).abs() < 1e-9, "got ({s},{l})");
        // 中間 event (2,1) → start 4 (= 2*2)、 len 2。
        let (s, l) = stretch_remap(0.0, 4.0, 0.0, 8.0, 2.0, 1.0);
        assert!((s - 4.0).abs() < 1e-9 && (l - 2.0).abs() < 1e-9, "got ({s},{l})");
    }

    #[test]
    fn left_edge_stretch_scales_from_right() {
        // clip [0,4] を左端 drag で [2,2] (start +2, len 0.5x)。 右端固定。
        // spanning event (0,4) は新 clip-local [0,2] を覆う。
        let (s, l) = stretch_remap(0.0, 4.0, 2.0, 2.0, 0.0, 4.0);
        assert!((s - 0.0).abs() < 1e-9 && (l - 2.0).abs() < 1e-9, "got ({s},{l})");
        // 元 clip 末尾 (4) にあった点は新 clip 末尾 (local 2) へ。
        let (s, _l) = stretch_remap(0.0, 4.0, 2.0, 2.0, 4.0, 0.0);
        assert!((s - 2.0).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn degenerate_prev_len_is_identity() {
        let (s, l) = stretch_remap(0.0, 0.0, 0.0, 4.0, 1.5, 2.0);
        assert!((s - 1.5).abs() < 1e-9 && (l - 2.0).abs() < 1e-9);
    }
}

#[cfg(test)]
mod add_track_insert_index_tests {
    use crate::app_types::{add_track_insert_index, track_with};
    use common::model::Track;

    fn tracks(ids: &[u32]) -> Vec<Track> {
        ids.iter()
            .map(|&id| track_with(|t| t.id = id))
            .collect()
    }

    #[test]
    fn appends_at_end_when_no_selection() {
        let t = tracks(&[10, 11, 12]);
        assert_eq!(add_track_insert_index(&t, &[]), 3);
    }

    #[test]
    fn inserts_above_single_selected() {
        let t = tracks(&[10, 11, 12]);
        // 先頭 (id 10, index 0) を選択 → 直上 = index 0。
        assert_eq!(add_track_insert_index(&t, &[10]), 0);
        // 中央 (id 11, index 1) → index 1。
        assert_eq!(add_track_insert_index(&t, &[11]), 1);
        // 末尾 (id 12, index 2) → index 2。
        assert_eq!(add_track_insert_index(&t, &[12]), 2);
    }

    #[test]
    fn inserts_above_top_most_of_multi_selection() {
        let t = tracks(&[10, 11, 12, 13]);
        // 選択 {10, 12} の最上段は 10 (index 0) → index 0。 vec の順序に依らない。
        assert_eq!(add_track_insert_index(&t, &[10, 12]), 0);
        assert_eq!(add_track_insert_index(&t, &[12, 10]), 0);
        // {11, 12} の最上段は 11 (index 1) → index 1。
        assert_eq!(add_track_insert_index(&t, &[11, 12]), 1);
    }

    #[test]
    fn stale_ids_fall_back_to_end() {
        let t = tracks(&[10, 11]);
        // 全部 stale → 末尾。
        assert_eq!(add_track_insert_index(&t, &[999, 1000]), 2);
        // 一部 stale → 生きている最上段 (id 10, index 0) の直上。
        assert_eq!(add_track_insert_index(&t, &[10, 999]), 0);
    }

    #[test]
    fn empty_track_list() {
        assert_eq!(add_track_insert_index(&[], &[]), 0);
        assert_eq!(add_track_insert_index(&[], &[5]), 0);
    }
}

#[cfg(test)]
mod clip_color_tests {
    use crate::app_types::{ClipRef, propagate_clip_color, track_with};
    use common::model::{Clip, Track};

    fn clip(id: u32, content_id: u32) -> Clip {
        Clip { id, content_id, length_beats: 4.0, ..Clip::default() }
    }

    fn track(id: u32, clips: Vec<Clip>) -> Track {
        track_with(|t| {
            t.id = id;
            t.clips = clips;
        })
    }

    #[test]
    fn set_color_propagates_to_all_clips_sharing_content_cross_track() {
        // track0: cid=7 と cid=9、 track1: cid=7 (linked, cross-track)。
        let mut tracks = vec![
            track(1, vec![clip(1, 7), clip(2, 9)]),
            track(2, vec![clip(3, 7)]),
        ];
        propagate_clip_color(&mut tracks, ClipRef { track: 0, clip: 0 }, Some([0.9, 0.3, 0.3]));
        // cid==7 は cross-track 含め全部同色、 cid==9 は不変 (= 確定動作 1)。
        assert_eq!(tracks[0].clips[0].color, Some([0.9, 0.3, 0.3]));
        assert_eq!(tracks[1].clips[0].color, Some([0.9, 0.3, 0.3]));
        assert_eq!(tracks[0].clips[1].color, None);
    }

    #[test]
    fn set_color_content_id_zero_colors_only_target() {
        // content_id == 0 (未採番 sentinel) は伝播せず target のみ (別の cid==0 を巻き込まない)。
        let mut tracks =
            vec![track(1, vec![clip(1, 0), clip(2, 0)])];
        propagate_clip_color(&mut tracks, ClipRef { track: 0, clip: 0 }, Some([0.1, 0.2, 0.3]));
        assert_eq!(tracks[0].clips[0].color, Some([0.1, 0.2, 0.3]));
        assert_eq!(tracks[0].clips[1].color, None);
    }

    #[test]
    fn set_color_out_of_range_target_is_noop() {
        let mut tracks = vec![track(1, vec![clip(1, 7)])];
        propagate_clip_color(&mut tracks, ClipRef { track: 5, clip: 0 }, Some([0.5, 0.5, 0.5]));
        assert_eq!(tracks[0].clips[0].color, None);
    }
}

#[cfg(test)]
mod preview_tests {
    use crate::app_types::{PreviewAction, diff_preview};

    #[test]
    fn none_to_some_emits_note_on() {
        assert_eq!(
            diff_preview(None, Some((3, 60))),
            vec![PreviewAction::NoteOn { track_id: 3, pitch: 60 }],
        );
    }

    #[test]
    fn same_pitch_held_emits_nothing() {
        assert_eq!(diff_preview(Some((3, 60)), Some((3, 60))), vec![]);
    }

    #[test]
    fn glissando_emits_off_then_on() {
        // Some(a) → Some(b): 旧 pitch off → 新 pitch on の順 (CLAP の同 time
        // Off→On 要件と整合)。
        assert_eq!(
            diff_preview(Some((3, 60)), Some((3, 62))),
            vec![
                PreviewAction::NoteOff { track_id: 3, pitch: 60 },
                PreviewAction::NoteOn { track_id: 3, pitch: 62 },
            ],
        );
    }

    #[test]
    fn release_emits_note_off() {
        assert_eq!(
            diff_preview(Some((3, 60)), None),
            vec![PreviewAction::NoteOff { track_id: 3, pitch: 60 }],
        );
    }

    #[test]
    fn track_change_retriggers_on_new_track() {
        // 同 pitch でも track が変われば旧 track off + 新 track on。
        assert_eq!(
            diff_preview(Some((3, 60)), Some((5, 60))),
            vec![
                PreviewAction::NoteOff { track_id: 3, pitch: 60 },
                PreviewAction::NoteOn { track_id: 5, pitch: 60 },
            ],
        );
    }
}

#[cfg(test)]
mod note_duplicate_tests {
    use crate::app_types::{copy_notes_into, duplicate_notes_into};
    use common::model::{MidiContent, Note};

    fn note(start: f64, dur: f64, pitch: u8) -> Note {
        Note {
            id: 0,
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: 100,
            lyric: None,
            muted: false,
        }
    }

    /// 元 note に実 id (1..=n) を振った MidiContent を作る (allocator は n+1 から)。
    /// 複製後に元と id が衝突しないことを検証できるようにする。
    fn content(mut notes: Vec<Note>) -> MidiContent {
        for (i, n) in notes.iter_mut().enumerate() {
            n.id = i as u32 + 1;
        }
        let next_note_id = notes.len() as u32 + 1;
        MidiContent { notes, next_note_id }
    }

    /// 複製 index 群 `new` の note id が全て非 0 かつ互いに / 元 (先頭 base 件) と
    /// 一意であることを検証する (per-content 一意 id 不変条件、 M4 sibling の回帰)。
    fn assert_ids_unique(c: &MidiContent) {
        let mut ids: Vec<u32> = c.notes.iter().map(|n| n.id).collect();
        assert!(ids.iter().all(|&id| id != 0), "全 note が非 0 id を持つ");
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "note id は content 内で一意 (複製で衝突しない)");
    }

    #[test]
    fn single_note_duplicated_after_itself() {
        let mut c = content(vec![note(0.0, 1.0, 60)]);
        let new_ids = duplicate_notes_into(&mut c, &[0]);
        assert_eq!(new_ids, vec![1]);
        assert_eq!(c.notes.len(), 2);
        // offset = (0+1) - 0 = 1 → 複製は start 1.0、長さ/pitch は維持
        assert_eq!(c.notes[1].start_beat, 1.0);
        assert_eq!(c.notes[1].duration_beats, 1.0);
        assert_eq!(c.notes[1].pitch, 60);
        // 元ノートは不変
        assert_eq!(c.notes[0].start_beat, 0.0);
        assert_ids_unique(&c);
    }

    #[test]
    fn multi_note_keeps_relative_positions_and_shifts_by_span() {
        // [0,1) と [2,3)。選択範囲 span = 3 - 0 = 3。
        let mut c = content(vec![note(0.0, 1.0, 60), note(2.0, 1.0, 64)]);
        let new_ids = duplicate_notes_into(&mut c, &[0, 1]);
        assert_eq!(new_ids, vec![2, 3]);
        assert_eq!(c.notes.len(), 4);
        assert_eq!(c.notes[2].start_beat, 3.0); // 0 + 3
        assert_eq!(c.notes[2].pitch, 60);
        assert_eq!(c.notes[3].start_beat, 5.0); // 2 + 3
        assert_eq!(c.notes[3].pitch, 64);
        assert_ids_unique(&c);
    }

    #[test]
    fn subset_selection_duplicates_only_selected() {
        // 3 ノート、index 0 と 2 だけ選択。選択範囲 span = (2+1) - 0 = 3。
        let mut c = content(vec![note(0.0, 1.0, 60), note(1.0, 1.0, 62), note(2.0, 1.0, 64)]);
        let new_ids = duplicate_notes_into(&mut c, &[0, 2]);
        assert_eq!(new_ids, vec![3, 4]);
        assert_eq!(c.notes.len(), 5);
        assert_eq!(c.notes[3].start_beat, 3.0); // 0 + 3
        assert_eq!(c.notes[3].pitch, 60);
        assert_eq!(c.notes[4].start_beat, 5.0); // 2 + 3
        assert_eq!(c.notes[4].pitch, 64);
        // 選択外の index 1 は複製されず元のまま
        assert_eq!(c.notes[1].start_beat, 1.0);
        assert_ids_unique(&c);
    }

    #[test]
    fn empty_selection_is_noop() {
        let mut c = content(vec![note(0.0, 1.0, 60)]);
        let new_ids = duplicate_notes_into(&mut c, &[]);
        assert!(new_ids.is_empty());
        assert_eq!(c.notes.len(), 1);
    }

    #[test]
    fn out_of_range_index_ignored() {
        let mut c = content(vec![note(0.0, 1.0, 60)]);
        let new_ids = duplicate_notes_into(&mut c, &[5]);
        assert!(new_ids.is_empty());
        assert_eq!(c.notes.len(), 1);
    }

    #[test]
    fn copy_places_clone_at_target_beat_and_pitch() {
        // Ctrl+drag: note0 を beat 4.0 / pitch 67 へコピー。元は据え置き。
        let mut c = content(vec![note(0.0, 1.0, 60)]);
        let new_ids = copy_notes_into(&mut c, &[(0, 4.0, 67)]);
        assert_eq!(new_ids, vec![1]);
        assert_eq!(c.notes.len(), 2);
        assert_eq!(c.notes[1].start_beat, 4.0);
        assert_eq!(c.notes[1].pitch, 67);
        assert_eq!(c.notes[1].duration_beats, 1.0); // 長さは維持
        assert_eq!(c.notes[0].start_beat, 0.0); // 元は不変
        assert_eq!(c.notes[0].pitch, 60);
        assert_ids_unique(&c);
    }

    #[test]
    fn copy_multi_preserves_each_target() {
        let mut c = content(vec![note(0.0, 1.0, 60), note(1.0, 0.5, 62)]);
        let new_ids = copy_notes_into(&mut c, &[(0, 2.0, 60), (1, 3.0, 64)]);
        assert_eq!(new_ids, vec![2, 3]);
        assert_eq!(c.notes[2].start_beat, 2.0);
        assert_eq!(c.notes[2].pitch, 60);
        assert_eq!(c.notes[3].start_beat, 3.0);
        assert_eq!(c.notes[3].pitch, 64);
        assert_eq!(c.notes[3].duration_beats, 0.5);
        assert_ids_unique(&c);
    }

    #[test]
    fn copy_empty_entries_is_noop() {
        let mut c = content(vec![note(0.0, 1.0, 60)]);
        let new_ids = copy_notes_into(&mut c, &[]);
        assert!(new_ids.is_empty());
        assert_eq!(c.notes.len(), 1);
    }
}

#[cfg(test)]
mod note_overlap_tests {
    // resolve_note_overlaps の純ロジック検証
    // (docs/plan_fixme_83_note_overlap.md)。
    use crate::app_types::{remap_indices, resolve_note_overlaps};
    use common::model::Note;

    fn note(start: f64, dur: f64, pitch: u8) -> Note {
        Note {
            id: 0,
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: 100,
            lyric: None,
            muted: false,
        }
    }

    #[test]
    fn full_cover_deletes_existing() {
        // 既存 [0,4) の上に同一ピッチ winner [0,4) → 既存削除、winner だけ残る。
        let mut notes = vec![note(0.0, 4.0, 60), note(0.0, 4.0, 60)];
        let remap = resolve_note_overlaps(&mut notes, &[1]);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].start_beat, 0.0);
        assert_eq!(notes[0].duration_beats, 4.0);
        // 古い idx0 (loser) は削除、idx1 (winner) は新 idx0 へ。
        assert_eq!(remap, vec![None, Some(0)]);
        assert_eq!(remap_indices(&remap, &[1]), vec![0]);
    }

    #[test]
    fn tail_overlap_trims_existing_tail() {
        // 既存 [0,4)、winner [2,6) が既存の末尾に食い込む → 既存を [0,2) にトリム。
        let mut notes = vec![note(0.0, 4.0, 60), note(2.0, 4.0, 60)];
        let remap = resolve_note_overlaps(&mut notes, &[1]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].start_beat, 0.0);
        assert_eq!(notes[0].duration_beats, 2.0); // 末尾を winner 開始でトリム
        assert_eq!(notes[1].start_beat, 2.0);
        assert_eq!(notes[1].duration_beats, 4.0); // winner は不変
        assert_eq!(remap, vec![Some(0), Some(1)]);
    }

    #[test]
    fn head_overlap_keeps_remnant() {
        // winner [0,4) が既存 [2,12) の先頭を覆う。後半 [4,12) を残す (REAPER 流)。
        let mut notes = vec![note(0.0, 4.0, 60), note(2.0, 10.0, 60)];
        resolve_note_overlaps(&mut notes, &[0]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].start_beat, 0.0);
        assert_eq!(notes[0].duration_beats, 4.0); // winner 不変
        assert_eq!(notes[1].start_beat, 4.0); // 開始を winner 終端へ前送り
        assert_eq!(notes[1].duration_beats, 8.0); // 12 - 4 = 8
    }

    #[test]
    fn middle_insertion_truncates_not_split() {
        // 長い既存 [0,8) の中央に短い winner [3,5) → 既存を [0,3) に切り詰め。
        // 自動 split しない (= note 数は 2 のまま、後半 [5,8) は残さない)。
        let mut notes = vec![note(0.0, 8.0, 60), note(3.0, 2.0, 60)];
        resolve_note_overlaps(&mut notes, &[1]);
        assert_eq!(notes.len(), 2); // split されない
        assert_eq!(notes[0].start_beat, 0.0);
        assert_eq!(notes[0].duration_beats, 3.0); // 切り詰め
        assert_eq!(notes[1].start_beat, 3.0); // winner 不変
        assert_eq!(notes[1].duration_beats, 2.0);
    }

    #[test]
    fn different_pitch_untouched() {
        // ピッチが違えば重なってよい (= 和音)。何も変わらない。
        let mut notes = vec![note(0.0, 4.0, 60), note(1.0, 2.0, 62)];
        let remap = resolve_note_overlaps(&mut notes, &[1]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].duration_beats, 4.0);
        assert_eq!(notes[1].start_beat, 1.0);
        assert_eq!(notes[1].duration_beats, 2.0);
        assert_eq!(remap, vec![Some(0), Some(1)]);
    }

    #[test]
    fn adjacent_notes_not_trimmed() {
        // 隣接 (既存 [0,2)、winner [2,2)+) は重なっていないので不干渉。
        let mut notes = vec![note(0.0, 2.0, 60), note(2.0, 2.0, 60)];
        resolve_note_overlaps(&mut notes, &[1]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].duration_beats, 2.0);
        assert_eq!(notes[1].start_beat, 2.0);
    }

    #[test]
    fn winner_trims_multiple_losers() {
        // winner [0.5,3.5) が前後 2 つの loser を同時に解消。
        // B1 [0,2) → 末尾トリム [0,0.5)、B2 [2,4) → 先頭前送り [3.5,4)。
        let mut notes = vec![note(0.0, 2.0, 60), note(2.0, 2.0, 60), note(0.5, 3.0, 60)];
        resolve_note_overlaps(&mut notes, &[2]);
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].start_beat, 0.0);
        assert_eq!(notes[0].duration_beats, 0.5);
        assert_eq!(notes[1].start_beat, 3.5);
        assert!((notes[1].duration_beats - 0.5).abs() < 1e-9);
        assert_eq!(notes[2].start_beat, 0.5); // winner 不変
        assert_eq!(notes[2].duration_beats, 3.0);
    }

    #[test]
    fn winner_winner_quantize_collision() {
        // 量子化で同一ピッチの 2 winner が重なる → 後勝ち、前の末尾をトリム。
        let mut notes = vec![note(0.0, 2.0, 60), note(1.0, 2.0, 60)];
        resolve_note_overlaps(&mut notes, &[0, 1]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].start_beat, 0.0);
        assert_eq!(notes[0].duration_beats, 1.0); // [0,2)→[0,1)
        assert_eq!(notes[1].start_beat, 1.0);
        assert_eq!(notes[1].duration_beats, 2.0); // 後発は不変
    }

    #[test]
    fn coincident_winners_dedup() {
        // 同位置・同ピッチの 2 winner → 前を削除 (長さ 0)、1 つに集約。
        let mut notes = vec![note(0.0, 2.0, 60), note(0.0, 2.0, 60)];
        let remap = resolve_note_overlaps(&mut notes, &[0, 1]);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].start_beat, 0.0);
        assert_eq!(notes[0].duration_beats, 2.0);
        assert_eq!(remap, vec![None, Some(0)]);
    }

    #[test]
    fn group_translation_preserves_all() {
        // move/copy 相当: winner 2 つが並進 (互いに重ならない)、loser 無し → 無変化。
        let mut notes = vec![note(0.0, 1.0, 60), note(2.0, 1.0, 60)];
        let remap = resolve_note_overlaps(&mut notes, &[0, 1]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].duration_beats, 1.0);
        assert_eq!(notes[1].start_beat, 2.0);
        assert_eq!(remap, vec![Some(0), Some(1)]);
    }

    #[test]
    fn empty_winners_is_noop() {
        let mut notes = vec![note(0.0, 4.0, 60), note(1.0, 4.0, 60)];
        let remap = resolve_note_overlaps(&mut notes, &[]);
        assert_eq!(notes.len(), 2); // 重なりがあっても winner 無しなら触らない
        assert_eq!(remap, vec![Some(0), Some(1)]);
    }

    #[test]
    fn remap_drops_deleted_and_shifts_survivors() {
        // 削除されたノートを参照する selected_notes は除外され、後続は前詰めされる。
        // notes: [loser0, winner1(覆う), survivor2]。loser0 を winner1 が完全被覆。
        let mut notes = vec![note(0.0, 2.0, 60), note(0.0, 2.0, 60), note(5.0, 1.0, 72)];
        let remap = resolve_note_overlaps(&mut notes, &[1]);
        assert_eq!(notes.len(), 2);
        assert_eq!(remap, vec![None, Some(0), Some(1)]);
        // 旧 selection [0,2] → 0 は削除で消え、2 は 1 へ。
        assert_eq!(remap_indices(&remap, &[0, 2]), vec![1]);
    }
}

/// 複数クリップ同時表示の packed note id 契約 (= cross-clip 編集の根幹)。
/// id = `(clip_slot << 24) | (local_index & 0x00FF_FFFF)`。view が widget へ渡す id と
/// handler が decode する id がこの 1 つの bit レイアウトを共有することで、 異なるクリップの
/// note が衝突せず、 decode で正しいクリップ・正しい local index に解決される。
#[cfg(test)]
mod multiclip_pianoroll_id_tests {
    use crate::app::AppData;
    use crate::app_types::ClipRef;

    fn cr(track: u32, clip: u32) -> ClipRef {
        ClipRef { track, clip }
    }

    #[test]
    fn pack_decode_round_trip() {
        for &(slot, idx) in &[
            (0usize, 0usize),
            (0, 5),
            (1, 0),
            (3, 42),
            (255, 0x00FF_FFFF),
        ] {
            let packed = AppData::pack_note_id(slot, idx);
            assert_eq!(AppData::note_id_clip_slot(packed), slot, "slot={slot} idx={idx}");
            assert_eq!(AppData::note_id_local_index(packed), idx, "slot={slot} idx={idx}");
        }
    }

    #[test]
    fn pack_masks_index_to_24_bits() {
        // 24 bit を超える index は下位 24 bit に丸め、 slot ビットを侵食しない。
        let packed = AppData::pack_note_id(2, 0x0100_0000 + 7); // (2^24 + 7)
        assert_eq!(AppData::note_id_clip_slot(packed), 2);
        assert_eq!(AppData::note_id_local_index(packed), 7);
    }

    #[test]
    fn slot_zero_packed_equals_local_index() {
        // 単一クリップ (slot 0) では packed id == local index = 旧単一表示と byte 互換。
        for idx in [0usize, 1, 100, 0x00FF_FFFF] {
            assert_eq!(AppData::pack_note_id(0, idx), idx as u32);
        }
    }

    #[test]
    fn decode_routes_to_correct_clip_by_slot() {
        // shown = [A(slot0), B(slot1), C(slot2)]。上位 8 bit が所属クリップを決める。
        let shown = [cr(0, 0), cr(1, 2), cr(0, 5)];
        assert_eq!(
            AppData::decode_note_id_in(&shown, AppData::pack_note_id(0, 3)),
            Some((cr(0, 0), 3))
        );
        assert_eq!(
            AppData::decode_note_id_in(&shown, AppData::pack_note_id(1, 9)),
            Some((cr(1, 2), 9))
        );
        assert_eq!(
            AppData::decode_note_id_in(&shown, AppData::pack_note_id(2, 0)),
            Some((cr(0, 5), 0))
        );
    }

    #[test]
    fn decode_out_of_range_slot_is_none() {
        // shown に存在しない slot は None (= 集合縮小 / ロック後の stale id を握り潰す)。
        let shown = [cr(0, 0), cr(1, 1)];
        assert_eq!(
            AppData::decode_note_id_in(&shown, AppData::pack_note_id(2, 0)),
            None
        );
        assert_eq!(
            AppData::decode_note_id_in(&shown, AppData::pack_note_id(255, 5)),
            None
        );
    }
}

#[cfg(test)]
mod aspect_fit_tests {
    use crate::app_types::aspect_fit_pip_rect;

    #[test]
    fn square_image_in_16_9_preview_pillarbox() {
        // 正方形 (1:1) を 16:9 preview → 縦一杯、 左右余白。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (500, 500));
        assert!((h - 1.0).abs() < 1e-5);
        assert!((w - (9.0 / 16.0)).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
        assert!((x - (1.0 - 9.0 / 16.0) * 0.5).abs() < 1e-5);
    }

    #[test]
    fn portrait_image_in_16_9_preview_pillarbox() {
        // 縦長 PNG (例 2894x4613) を 16:9 preview → 縦一杯、 左右に大きな余白。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (2894, 4613));
        assert!((h - 1.0).abs() < 1e-5);
        let expected_w = (2894.0 / 4613.0) / (1920.0 / 1080.0);
        assert!((w - expected_w).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
        assert!((x - (1.0 - expected_w) * 0.5).abs() < 1e-5);
    }

    #[test]
    fn landscape_image_in_16_9_preview_letterbox() {
        // 21:9 (超横長) を 16:9 preview → 横一杯、 上下に余白。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (2100, 900));
        assert!((w - 1.0).abs() < 1e-5);
        let expected_h = (1920.0 / 1080.0) / (2100.0 / 900.0);
        assert!((h - expected_h).abs() < 1e-5);
        assert!((x - 0.0).abs() < 1e-5);
        assert!((y - (1.0 - expected_h) * 0.5).abs() < 1e-5);
    }

    #[test]
    fn same_aspect_fills_preview() {
        // 16:9 を 16:9 preview → 全画面 (= 余白なし)。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (1280, 720));
        assert!((w - 1.0).abs() < 1e-5);
        assert!((h - 1.0).abs() < 1e-5);
        assert!((x - 0.0).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
    }

    #[test]
    fn zero_dimension_falls_back_to_full_screen() {
        assert_eq!(aspect_fit_pip_rect((0, 1080), (500, 500)), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(aspect_fit_pip_rect((1920, 1080), (0, 500)), (0.0, 0.0, 1.0, 1.0));
    }
}

#[cfg(test)]
mod master_fx_tests {
    use crate::app_types::{compute_slot_reconcile_actions, SlotReconcileAction};
    use common::model::{MASTER_TRACK_ID, PluginInstance, Song};
    use common::plugin_format::PluginFormat;
    use std::collections::HashMap;

    #[test]
    fn reconcile_emits_loadslot_for_master_fx() {
        // master_fx_chain に 1 plugin、 host (loaded_slots) は空 → master の
        // device index 0 に対する LoadSlot が 1 件出る。
        let mut song = Song::default();
        song.master_fx_chain.push(PluginInstance::new(
            "vendor.reverb".to_string(),
            PluginFormat::Clap,
        ));
        let loaded = HashMap::new();
        let actions = compute_slot_reconcile_actions(&song, &loaded);
        assert!(
            actions.iter().any(|a| matches!(
                a,
                SlotReconcileAction::LoadSlot { track_id, index, plugin_id_str, .. }
                    if *track_id == MASTER_TRACK_ID
                        && *index == 0
                        && plugin_id_str == "vendor.reverb"
            )),
            "master fx に対する LoadSlot が emit されること: {actions:?}"
        );
    }

    #[test]
    fn master_fx_chain_survives_serde_roundtrip_and_forward_migrates() {
        // master_fx_chain 付き Song を JSON 経由で往復しても保持される。
        let mut song = Song::default();
        song.master_fx_chain.push(PluginInstance::new(
            "vendor.eq".to_string(),
            PluginFormat::Clap,
        ));
        let json = serde_json::to_string(&song).unwrap();
        let back: Song = serde_json::from_str(&json).unwrap();
        assert_eq!(back.master_fx_chain.len(), 1);
        assert_eq!(back.master_fx_chain[0].plugin_id, "vendor.eq");

        // master_fx_chain field を持たない旧 file は空 Vec に forward-migrate。
        let legacy = r#"{"bpm":120.0,"time_sig":[4,4],"length_beats":16.0}"#;
        let migrated: Song = serde_json::from_str(legacy).unwrap();
        assert!(migrated.master_fx_chain.is_empty());
    }
}

#[cfg(test)]
mod plugin_category_tests {
    use crate::app_types::PluginCategory;

    fn feats(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn routing_priority_and_fallback() {
        // 統合ピッカーの自動振り分け規則: 優先順 note-effect > instrument >
        // audio-effect、 どの主カテゴリも無ければ FX チェーンへ (plan_unified_plugin_picker.md)。
        let cases: &[(&[&str], PluginCategory)] = &[
            (&["instrument", "synthesizer"], PluginCategory::Instrument),
            (&["audio-effect"], PluginCategory::Fx),
            (&["audio-effect", "reverb"], PluginCategory::Fx),
            (&["note-effect"], PluginCategory::MidiFx),
            // 音を出す方が勝つ: instrument は audio-effect に優先 (features 順非依存)。
            (&["instrument", "audio-effect"], PluginCategory::Instrument),
            (&["audio-effect", "instrument"], PluginCategory::Instrument),
            // note-effect は最優先 (features 順非依存)。
            (&["note-effect", "instrument"], PluginCategory::MidiFx),
            (&["instrument", "note-effect"], PluginCategory::MidiFx),
            // 未分類 (主カテゴリ無し / 空) は FX チェーンへ倒す。
            (&[], PluginCategory::Fx),
            (&["reverb"], PluginCategory::Fx),
            // video-effect は最優先で映像カテゴリへ (排他)。
            (&["video-effect", "video-color"], PluginCategory::Video),
            (&["video-effect"], PluginCategory::Video),
        ];
        for (features, expected) in cases {
            assert_eq!(
                PluginCategory::from_features(&feats(features)),
                *expected,
                "features = {features:?}",
            );
        }
    }
}

/// r.md #30 トラック複製の remap エンジン (`build_pasted_tracks`) の core 挙動。
/// linked (D 相当) は content_id 共有、 independent (Alt+D 相当) は新採番、 parent は
/// 集合内なら新 group へ remap・集合外は据え置きで top-level は None のまま。
#[cfg(test)]
mod track_duplicate_tests {
    use crate::app::AppData;
    use crate::app_types::track_with;
    use crate::clipboard::{ContentEntry, TrackCopy};
    use common::model::{Clip, ClipContent, Song};

    fn clip(id: u32, content_id: u32) -> Clip {
        Clip { id, content_id, length_beats: 4.0, ..Clip::default() }
    }

    /// content 付き top-level MIDI track 1 本の Song。戻り値は (song, track id, content id)。
    fn song_one_track() -> (Song, u32, u32) {
        let mut song = Song::default();
        let cid = song.alloc_content(ClipContent::default(), String::new());
        let tid = song.alloc_track_id();
        song.tracks.push(track_with(|t| {
            t.id = tid;
            t.name = "T".into();
            t.clips = vec![clip(1, cid)];
        }));
        (song, tid, cid)
    }

    fn copy_of(song: &Song, tid: u32, cid: u32) -> TrackCopy {
        let track = song.track_by_id(tid).unwrap().clone();
        let content = song.clip_contents.get(&cid).cloned().unwrap_or_default();
        TrackCopy {
            order: 0,
            track,
            contents: vec![ContentEntry { content_id: cid, content, name: None }],
        }
    }

    #[test]
    fn linked_duplicate_shares_content_id() {
        let (mut song, tid, cid) = song_one_track();
        let before = song.clip_contents.len();
        let tc = copy_of(&song, tid, cid);
        // linked = force_independent_content=false。
        let built = AppData::build_pasted_tracks(&mut song, &[tc], true, false, None);
        assert_eq!(built.len(), 1);
        let (src, t) = &built[0];
        assert_eq!(*src, tid);
        assert_ne!(t.id, tid, "新 track id が採番される");
        assert_eq!(t.parent_group_id, None, "top-level は top-level のまま");
        // クリップ中身は元と content_id 共有 (= 連動)。
        assert_eq!(t.clips[0].content_id, cid);
        // 新規 content は増えない。
        assert_eq!(song.clip_contents.len(), before);
    }

    #[test]
    fn independent_duplicate_allocs_new_content() {
        let (mut song, tid, cid) = song_one_track();
        let before = song.clip_contents.len();
        let tc = copy_of(&song, tid, cid);
        // independent = force_independent_content=true。
        let built = AppData::build_pasted_tracks(&mut song, &[tc], true, true, None);
        let (_, t) = &built[0];
        assert_ne!(t.clips[0].content_id, cid, "独立コピーは新 content_id");
        assert_eq!(song.clip_contents.len(), before + 1, "content が 1 件増える");
    }

    #[test]
    fn duplicated_group_child_reparents_to_new_group() {
        // group(gid) + child(parent=gid)。 両方複製すると child は複製後の group を指す。
        let mut song = Song::default();
        let gid = song.alloc_track_id();
        let child = song.alloc_track_id();
        song.tracks.push(track_with(|t| {
            t.id = gid;
            t.name = "G".into();
        }));
        song.tracks.push(track_with(|t| {
            t.id = child;
            t.name = "C".into();
            t.parent_group_id = Some(gid);
        }));
        let tcs = vec![
            TrackCopy { order: 0, track: song.track_by_id(gid).unwrap().clone(), contents: vec![] },
            TrackCopy { order: 1, track: song.track_by_id(child).unwrap().clone(), contents: vec![] },
        ];
        let built = AppData::build_pasted_tracks(&mut song, &tcs, true, true, None);
        let new_group = built[0].1.id;
        assert_ne!(new_group, gid);
        assert_eq!(built[1].1.parent_group_id, Some(new_group), "child は複製後の group を指す");
    }

    #[test]
    fn duplicated_child_alone_keeps_original_parent() {
        // child だけを複製 (group は集合外・実在) → 元 group を継承 (同じ group 内に残る)。
        let mut song = Song::default();
        let gid = song.alloc_track_id();
        let child = song.alloc_track_id();
        song.tracks.push(track_with(|t| {
            t.id = gid;
            t.name = "G".into();
        }));
        song.tracks.push(track_with(|t| {
            t.id = child;
            t.name = "C".into();
            t.parent_group_id = Some(gid);
        }));
        let tc = TrackCopy {
            order: 0,
            track: song.track_by_id(child).unwrap().clone(),
            contents: vec![],
        };
        let built = AppData::build_pasted_tracks(&mut song, &[tc], true, true, None);
        assert_eq!(built[0].1.parent_group_id, Some(gid), "同じ group 内に残る");
    }
}


#[cfg(test)]
mod gpu_derived_cache_tests {
    //! r.md #42: main renderer 上の派生テクスチャ (動画サムネイル / 画像) は
    //! `AppData` 側の HashMap と `Renderer` 側の `TextureStore` に参照が二重化する。
    //! `AppData` は `Renderer` を持てないので、cache を捨てる側は **破棄予約に積む**
    //! 必要がある。単に `clear()` すると GPU 側 entry が解放されず、プロジェクトを
    //! 開き直すたびに VRAM が単調増加する (サムネイルはネイティブ解像度で 4K なら
    //! 1 枚 33MB)。
    use std::num::NonZeroU32;
    use std::sync::Arc;

    use common::protocol::{AudioCommand, PluginCommand};
    use daw_ui_renderer::TextureHandle;
    use tokio::sync::mpsc;

    use crate::app::AppData;
    use crate::dispatcher::{
        BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
    };

    fn build_app() -> AppData {
        let (audio_tx, _audio_rx) = mpsc::unbounded_channel::<AudioCommand>();
        let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel::<PluginCommand>();
        let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
        let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
        AppData::new(
            audio_tx,
            plugin_tx,
            None,
            None,
            event_dispatcher,
            job_dispatcher,
            None,
            None,
            common::audio_bridge::DEFAULT_SAMPLE_RATE,
        )
    }

    fn handle(raw: u32) -> TextureHandle {
        TextureHandle::from_raw(NonZeroU32::new(raw).expect("nonzero"))
    }

    /// プロジェクト差し替えで cache を捨てるとき、handle は **破棄予約へ移す**。
    #[test]
    fn after_song_replaced_queues_texture_destroys() {
        let mut app = build_app();
        app.ui_ephemeral.video_texture_cache.insert(7, handle(1));
        app.ui_ephemeral.image_texture_cache.insert(9, handle(2));

        app.after_song_replaced();

        assert!(
            app.ui_ephemeral.video_texture_cache.is_empty(),
            "別 project の id が誤 hit しないよう参照は捨てる"
        );
        assert!(app.ui_ephemeral.image_texture_cache.is_empty());
        let mut queued: Vec<u32> = app
            .ui_ephemeral
            .pending_texture_destroys
            .iter()
            .map(|h| h.raw())
            .collect();
        queued.sort_unstable();
        assert_eq!(queued, vec![1, 2], "捨てた handle は必ず destroy 予約に積む");
    }

    /// GPU 復旧時も同じ (片方の renderer だけ生きていた場合に orphan を残さない)。
    #[test]
    fn rebuild_gpu_derived_caches_queues_texture_destroys() {
        let mut app = build_app();
        app.ui_ephemeral.video_texture_cache.insert(1, handle(11));
        app.ui_ephemeral.image_texture_cache.insert(2, handle(12));

        app.rebuild_gpu_derived_caches();

        assert!(app.ui_ephemeral.video_texture_cache.is_empty());
        assert!(app.ui_ephemeral.image_texture_cache.is_empty());
        assert_eq!(app.ui_ephemeral.pending_texture_destroys.len(), 2);
    }
}
