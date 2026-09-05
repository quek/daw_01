//! handler::note_selection — **ノート選択を時間範囲から導出する**
//! (`docs/plan_range_selection.md` §1)。
//!
//! 選択の SSoT は `SelectionState::time` 1 本で、ピアノロールでは「レーン」 が
//! **鍵盤の行** ([`LaneRef::KeyTrack`]、Live の "key track") になる。 2 次元の
//! ドラッグは「時間区間 × 横切った鍵盤行の集合」 になり、そこに交差するノートが
//! 選択されているノートである。
//!
//! 鍵盤行は**任意集合**なので、離れた 2 つのピッチだけを持つこともできる
//! (時間だけが単一の区間)。 アレンジャーで「トラック集合は飛び飛び、時間は 1 区間」
//! なのと同じ形。

use crate::state::*;
use common::model::{ClipKey, LaneRef};

impl AppData {
    /// 選択されているノート (packed note id)。 **範囲からの導出**。
    ///
    /// `clip_slot` は [`Self::shown_pianoroll_clips`] 内の位置なので、
    /// widget へ渡す id 空間と常に一致する。
    #[must_use]
    pub fn selected_note_ids(&self) -> Vec<u32> {
        let Some(sel) = self.selection.time.as_ref() else {
            return Vec::new();
        };
        // **鍵盤行が 1 つも無ければノートは選ばれていない。** この早期 return が無いと、
        // アレンジャーの範囲を持っているだけのフレームでも表示クリップの解決と
        // 全ノート走査が毎フレーム走る (`edit_surface` / view_build から呼ばれる)。
        if !sel.lanes.iter().any(|l| matches!(l, LaneRef::KeyTrack { .. })) {
            return Vec::new();
        }
        let song = self.song_doc.song();
        let shown = self.shown_pianoroll_clips();
        let mut out = Vec::new();
        for (slot, key) in shown.iter().enumerate() {
            let Some(clip) = song.clip_by_key(*key) else {
                continue;
            };
            for (idx, note) in song.clip_notes(clip).iter().enumerate() {
                if !sel.has_lane(LaneRef::KeyTrack { clip: *key, pitch: note.pitch }) {
                    continue;
                }
                let start = clip.content_to_song_beat(note.start_beat);
                if sel.intersects(start, note.duration_beats) {
                    out.push(Self::pack_note_id(slot, idx));
                }
            }
        }
        out
    }

    /// ノート選択を差し替える = **範囲を張り直す**。
    ///
    /// 時間は渡されたノートの外接区間、レーンはそれらの `(クリップ, ピッチ)` 集合。
    /// ピッチは飛び飛びでよいので、離れた 2 音だけを選ぶこともできる。
    /// 空を渡すと選択解除。
    pub(crate) fn set_note_selection(&mut self, ids: &[u32]) {
        let shown = self.shown_pianoroll_clips();
        let song = self.song_doc.song();
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut lanes: Vec<LaneRef> = Vec::new();
        for id in ids {
            let Some((key, idx)) = Self::decode_note_id_in(&shown, *id) else {
                continue;
            };
            let Some(clip) = song.clip_by_key(key) else {
                continue;
            };
            let Some(note) = song.clip_notes(clip).get(idx) else {
                continue;
            };
            let s = clip.content_to_song_beat(note.start_beat);
            start = start.min(s);
            end = end.max(s + note.duration_beats);
            for lane in song.content_lanes_for(key, LaneRef::KeyTrack { clip: key, pitch: note.pitch }) {
                if !lanes.contains(&lane) {
                    lanes.push(lane);
                }
            }
        }
        if lanes.is_empty() {
            // ノート選択の解除。 **範囲を捨てずにクリップの範囲へ落とす** —
            // 捨てるとピアノロールに出ていたクリップまで消えて、空白クリック 1 回で
            // エディタが真っ白になる。
            self.collapse_note_selection_to_clips(&shown);
            return;
        }
        self.set_time_selection(common::model::TimeSelection::new(start, end, lanes));
        self.selection.range_anchor = self.selection.time.as_ref().map(|t| t.start_beat);
    }

    /// ノート選択だけを解除し、表示していたクリップの範囲へ落とす。
    fn collapse_note_selection_to_clips(&mut self, shown: &[ClipKey]) {
        let song = self.song_doc.song();
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut lanes: Vec<LaneRef> = Vec::new();
        for key in shown {
            // ランチャーのセルは曲の時間軸に居ないので、落とせる範囲が無い
            // ([`Song::content_lanes_for`] と同じ理由)。 セルを開いているときはセル選択
            // (`live_launcher_cells`) が表示を支えるので、範囲は捨ててよい —
            // ここで無理にトラック行の区間を張ると、面タグがアレンジへ倒れて
            // **セルのピアノロールで空白を 1 回クリックしただけで
            // 「クリップが選択されていません」** になる (r.md #90)。
            if song.is_session_clip(*key) {
                continue;
            }
            let Some(clip) = song.clip_by_key(*key) else {
                continue;
            };
            let (s, e) = clip.song_window();
            start = start.min(s);
            end = end.max(e);
            let lane = LaneRef::Track(key.track_id);
            if !lanes.contains(&lane) {
                lanes.push(lane);
            }
        }
        let next = if lanes.is_empty() {
            None
        } else {
            common::model::TimeSelection::new(start, end, lanes)
        };
        self.set_time_selection(next);
        self.selection.range_anchor = self.selection.time.as_ref().map(|t| t.start_beat);
    }

    /// ピアノロールの 2 次元ドラッグ (時間 × 鍵盤行) を範囲にする。
    ///
    /// `pitch_lo` / `pitch_hi` は掴んだ鍵盤行の範囲 (両端含む)。 対象クリップは
    /// 表示中の MIDI クリップ全部 (複数同時表示のまま矩形で拾える)。
    pub(crate) fn set_pianoroll_rect_selection(
        &mut self,
        start_beat: f64,
        end_beat: f64,
        pitch_lo: u8,
        pitch_hi: u8,
    ) {
        let (lo, hi) = if pitch_lo <= pitch_hi { (pitch_lo, pitch_hi) } else { (pitch_hi, pitch_lo) };
        let shown = self.shown_pianoroll_clips();
        let mut lanes: Vec<LaneRef> = Vec::new();
        for key in &shown {
            for pitch in lo..=hi {
                lanes.push(LaneRef::KeyTrack { clip: *key, pitch });
            }
            // クリップのトラック行も入れる — アレンジ側でそのクリップが選択表示になり、
            // `shown_pianoroll_clips` もトラック行から解決できる。
            let t = LaneRef::Track(key.track_id);
            if !lanes.contains(&t) {
                lanes.push(t);
            }
        }
        match common::model::TimeSelection::new(start_beat, end_beat, lanes) {
            Some(next) => {
                self.set_time_selection(Some(next));
                self.selection.range_anchor =
                    self.selection.time.as_ref().map(|t| t.start_beat);
            }
            // 幅ゼロのドラッグ (= ただのクリック) はノート選択の解除。
            None => self.collapse_note_selection_to_clips(&shown),
        }
    }

    /// 1 つのノートを選択に足す / 外す (Ctrl・Shift+クリック)。
    ///
    /// `additive` ならその鍵盤行を範囲に足し、時間を外接まで広げる
    /// (アレンジャーの Ctrl+クリックと同じ規則)。 そうでなければそのノートだけ。
    pub(crate) fn select_note_in_range(&mut self, id: u32, additive: bool) {
        let shown = self.shown_pianoroll_clips();
        let Some((key, idx)) = Self::decode_note_id_in(&shown, id) else {
            return;
        };
        let song = self.song_doc.song();
        let Some(clip) = song.clip_by_key(key) else {
            return;
        };
        let Some(note) = song.clip_notes(clip).get(idx) else {
            return;
        };
        let s = clip.content_to_song_beat(note.start_beat);
        let e = s + note.duration_beats;
        let lanes: Vec<LaneRef> = song
            .content_lanes_for(key, LaneRef::KeyTrack { clip: key, pitch: note.pitch })
            .collect();
        if additive && let Some(sel) = self.selection.time.as_mut() {
            sel.extend(s, e, lanes);
            self.selection.last_edit_select = Some(crate::app::EditSurface::TimeRange);
            self.drop_cell_selection_if_arrangement();
            return;
        }
        let next = common::model::TimeSelection::new(s, e, lanes);
        self.set_time_selection(next);
        self.selection.range_anchor = Some(s);
    }

    /// 表示中クリップの**全ノート**を選択する (`Ctrl+A`)。
    pub(crate) fn select_all_shown_notes(&mut self) {
        let shown = self.shown_pianoroll_clips();
        let song = self.song_doc.song();
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        let mut lanes: Vec<LaneRef> = Vec::new();
        for key in &shown {
            let Some(clip) = song.clip_by_key(*key) else {
                continue;
            };
            for note in song.clip_notes(clip) {
                let s = clip.content_to_song_beat(note.start_beat);
                start = start.min(s);
                end = end.max(s + note.duration_beats);
                for lane in
                    song.content_lanes_for(*key, LaneRef::KeyTrack { clip: *key, pitch: note.pitch })
                {
                    if !lanes.contains(&lane) {
                        lanes.push(lane);
                    }
                }
            }
        }
        let next = common::model::TimeSelection::new(start, end, lanes);
        self.set_time_selection(next);
        self.selection.range_anchor = self.selection.time.as_ref().map(|t| t.start_beat);
    }


    /// `ClipKey` と clip 内 index からその時点の packed id を作る (再選択用)。
    #[must_use]
    pub fn pack_note_ids_for(&self, key: ClipKey, indices: &[usize]) -> Vec<u32> {
        let shown = self.shown_pianoroll_clips();
        let Some(slot) = shown.iter().position(|k| *k == key) else {
            return Vec::new();
        };
        indices.iter().map(|i| Self::pack_note_id(slot, *i)).collect()
    }
}

impl AppData {
    /// ノート選択を解除する。
    ///
    /// 選択の SSoT は範囲 1 本なので、**ピアノロールの範囲 (鍵盤行を含む範囲) の
    /// ときだけ**捨てる。 アレンジャーの範囲が立っている状態でノート編集の後始末が
    /// 走っても、アレンジの選択を巻き込まない。
    pub(crate) fn clear_note_selection(&mut self) {
        let is_note_range = self
            .selection
            .time
            .as_ref()
            .is_some_and(|t| t.lanes.iter().any(|l| matches!(l, LaneRef::KeyTrack { .. })));
        if is_note_range {
            self.selection.time = None;
            self.selection.range_anchor = None;
        }
    }
}

impl AppData {
    /// オーディオエディタで選択されている event の index。 **範囲からの導出**。
    ///
    /// レーンは波形行 ([`LaneRef::AudioLane`]) 1 本。 index は
    /// `ui_ephemeral.audio_editor_clip` が指すクリップの `AudioContent.events` への添字。
    #[must_use]
    pub fn selected_audio_event_indices(&self) -> Vec<usize> {
        let Some(key) = self.ui_ephemeral.audio_editor_clip else {
            return Vec::new();
        };
        let Some(sel) = self.selection.time.as_ref() else {
            return Vec::new();
        };
        if !sel.has_lane(LaneRef::AudioLane(key)) {
            return Vec::new();
        }
        let song = self.song_doc.song();
        let Some(clip) = song.clip_by_key(key) else {
            return Vec::new();
        };
        let Some(common::model::ClipContent::Audio(audio)) =
            song.clip_contents.get(&clip.content_id)
        else {
            return Vec::new();
        };
        audio
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                let start = clip.content_to_song_beat(e.event_start_in_clip_beats);
                sel.intersects(start, e.event_length_beats)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// オーディオエディタの event 選択を差し替える = **範囲を張り直す**。
    ///
    /// 時間は渡された event の外接区間、レーンは波形行 1 本。
    /// 空を渡すとそのクリップの区間へ落とす (エディタが空表示にならないよう、
    /// ノート選択の解除と同じ扱い)。
    pub(crate) fn set_audio_event_selection(&mut self, indices: &[usize]) {
        let Some(key) = self.ui_ephemeral.audio_editor_clip else {
            return;
        };
        let song = self.song_doc.song();
        let Some(clip) = song.clip_by_key(key).cloned() else {
            return;
        };
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        if let Some(common::model::ClipContent::Audio(audio)) =
            song.clip_contents.get(&clip.content_id)
        {
            for i in indices {
                let Some(e) = audio.events.get(*i) else {
                    continue;
                };
                let s = clip.content_to_song_beat(e.event_start_in_clip_beats);
                start = start.min(s);
                end = end.max(s + e.event_length_beats);
            }
        }
        let cell = song.is_session_clip(key);
        let next = if start.is_finite() && end > start {
            common::model::TimeSelection::new(
                start,
                end,
                song.content_lanes_for(key, LaneRef::AudioLane(key)).collect(),
            )
        } else if cell {
            // セルは曲の時間軸に居ないので落とせる区間が無い
            // ([`Song::content_lanes_for`] と同じ理由)。 エディタの表示は
            // `ui_ephemeral.audio_editor_clip` が支えるので範囲は捨てる。
            None
        } else {
            // 選択解除: クリップの区間へ落とす (エディタの表示は保つ)。
            let (s, e) = clip.song_window();
            common::model::TimeSelection::new(s, e, vec![LaneRef::Track(key.track_id)])
        };
        self.set_time_selection(next);
        self.selection.range_anchor = self.selection.time.as_ref().map(|t| t.start_beat);
    }
}

#[cfg(test)]
mod launcher_cell_editing_tests {
    //! r.md #90: ランチャーのセルをピアノロールで開いた後、**ピアノロールの中を
    //! 触ってもセルが開いたまま**であること。
    //!
    //! セルは曲の時間軸に居ない (`start_beat` は常に 0 / `track.clips` にも居ない)
    //! ので、セルのノート選択をアレンジの時間範囲として書き戻すと壊れる。
    //! 壊れ方は 2 通りあって、どちらも同じ根 (`Song::content_lanes_for`) を持つ。
    use common::model::{Clip, ClipKey, Track};

    use crate::app::{AppData, AppEvent};
    use crate::event_launcher::{LauncherCellKey, LauncherEvent, LauncherRow};

    fn build_app() -> AppData {
        crate::test_support::headless_app()
    }

    /// トラック 1 本 + 列 1 本の曲を作り、その行にセルを 1 つ置く。
    fn seed_with_cell(app: &mut AppData) -> ClipKey {
        app.edit_song(|song| {
            song.tracks.clear();
            song.scenes.clear();
            song.tracks.push(Track { id: 1, next_clip_id: 1, ..Track::default() });
            song.ids.next_track_id = 2;
            song.push_scene();
        });
        app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell {
            row: LauncherRow::Track(1),
            scene_index: 0,
        }));
        let scene_id = app.song_doc.song().scenes[0].id;
        let cell = app
            .cell_in_row_at_scene(LauncherRow::Track(1), scene_id)
            .expect("セルが作られている");
        let LauncherCellKey::Track(key) = cell else {
            panic!("トラック行のセル");
        };
        key
    }

    /// アレンジ側の 0..4 拍に MIDI クリップを 1 つ置く (セルの窓と重なる位置)。
    fn put_arrangement_clip(app: &mut AppData) -> ClipKey {
        let mut key = ClipKey { track_id: 1, clip_id: 0 };
        app.edit_song(|song| {
            let content_id = song.alloc_content_id();
            song.clip_contents.insert(content_id, common::model::ClipContent::default());
            let track = song.track_by_id_mut(1).expect("track");
            let id = track.alloc_clip_id();
            track.clips.push(Clip {
                id,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id,
                ..Clip::default()
            });
            key.clip_id = id;
        });
        key
    }

    /// セルを開いた後にグリッドの空白をクリックしても、セルは開いたまま。
    ///
    /// 空白クリックは幅ゼロの範囲ドラッグ (widget の `range_release`) として届く。
    /// 以前はここでセルの窓をアレンジのトラック行の区間として張り直していたため、
    /// 面タグが `TimeRange` へ倒れて表示集合が空になり、ピアノロールが
    /// 「(クリップが選択されていません)」 になっていた。
    #[test]
    fn セルを開いた後に空白をクリックしてもセルは開いたまま() {
        let mut app = build_app();
        let cell = seed_with_cell(&mut app);
        app.open_cell_editor(LauncherCellKey::Track(cell));
        assert_eq!(app.shown_pianoroll_clips(), vec![cell], "ダブルクリックで開く");

        app.set_pianoroll_rect_selection(2.0, 2.0, 60, 60);
        assert_eq!(app.shown_pianoroll_clips(), vec![cell], "空白クリック後も開いたまま");

        // ノートを選んで (= 面タグが Notes へ移る) から、もう一度空白クリック。
        app.handle_event(AppEvent::AddNote {
            key: cell,
            start_beat: 1.0,
            duration: 1.0,
            pitch: 60,
        });
        assert_eq!(app.shown_pianoroll_clips(), vec![cell]);
        app.set_pianoroll_rect_selection(2.0, 2.0, 60, 60);
        assert_eq!(app.shown_pianoroll_clips(), vec![cell], "ノート選択を解除しても開いたまま");
    }

    /// セルのノートを選んでも、アレンジのクリップは巻き込まれない。
    ///
    /// セルの窓は常に 0 拍始まりなので、トラック行の区間として張ると「曲頭に
    /// 居るだけの無関係なクリップ」が表示集合に割り込む。 しかも開始拍順に
    /// 並ぶので先頭に入り、packed note id の `clip_slot` がずれて
    /// ノート選択が別のクリップを指す。
    #[test]
    fn セルのノートを選んでもアレンジのクリップは混ざらない() {
        let mut app = build_app();
        let cell = seed_with_cell(&mut app);
        let arranged = put_arrangement_clip(&mut app);
        app.open_cell_editor(LauncherCellKey::Track(cell));

        app.handle_event(AppEvent::AddNote {
            key: cell,
            start_beat: 1.0,
            duration: 1.0,
            pitch: 60,
        });

        assert_eq!(app.shown_pianoroll_clips(), vec![cell], "映るのはセルだけ");
        assert!(
            !app.selected_clip_refs().contains(&arranged),
            "アレンジのクリップは選択されない"
        );
    }

    /// アレンジのクリップを選んだらセル選択は降り、ピアノロールもそちらへ移る
    /// (オブジェクト選択は 1 面だけ)。
    #[test]
    fn アレンジのクリップを選ぶとセル選択は降りる() {
        let mut app = build_app();
        let cell = seed_with_cell(&mut app);
        let arranged = put_arrangement_clip(&mut app);
        app.open_cell_editor(LauncherCellKey::Track(cell));
        assert_eq!(app.shown_pianoroll_clips(), vec![cell]);

        app.handle_event(AppEvent::SelectClip { target: arranged, additive: false });

        assert!(
            app.selection.selected_launcher_cells.is_empty(),
            "セル選択は降りる (選択枠も消える)"
        );
        assert_eq!(app.shown_pianoroll_clips(), vec![arranged], "ピアノロールはアレンジへ移る");
    }
}
