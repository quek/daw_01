//! handler::chord_tool_event — コードの検出 / 変換 / 取り込みと、試聴の設定の入口 (`AppEvent::ChordTool`、r.md #157)。

use common::chord_voicing::Voicing;

use crate::app_types::PREVIEW_VELOCITY;
use crate::event_chord::ChordToolEvent;
use crate::state::AppData;

impl AppData {
    /// [`ChordToolEvent`] の処理 (`AppEvent::ChordTool` の 1 arm)。
    pub(crate) fn handle_chord_tool_event(&mut self, ev: ChordToolEvent) {
        match ev {
            ChordToolEvent::SetAudioChords { source_id, spans, place_on } => {
                self.set_audio_chords(source_id, spans, &place_on);
            }
            ChordToolEvent::ChordsToClip { chord_ids, track_id } => {
                let voicing: Voicing = self.cur.chord_tools.view.voicing;
                // クリップを置けるのは自分のクリップを鳴らす行だけ (グループには置かない — 帯の widget と同じ判定を
                // ここでも引く。script からも通る口なので widget の絞り込みに頼らない)。
                self.edit_song_checked(|song| {
                    !song.track_is_pure_group(track_id)
                        && song.chords_to_clip(&chord_ids, track_id, voicing, PREVIEW_VELOCITY).is_some()
                });
            }
            ChordToolEvent::ExtractFromClips { clips, window_beats } => {
                // 窓を名指しした抽出 (script): 長さの自動は切る。
                let detect = self.cur.chord_tools.view.detect;
                self.extract_midi_chords(
                    &clips,
                    common::chord_detect::DetectOptions { auto_length: false, window_beats, ..detect },
                );
            }
            ChordToolEvent::DetectAudioClips { clips, place } => self.detect_audio_clips(&clips, place),
            ChordToolEvent::ImportMidiFile { path, at } => {
                self.import_midi_chords(&path, at);
            }
            ChordToolEvent::ExtractChords { clips } => {
                self.cur.chord_tools.extract_dialog = None;
                self.extract_chords_from_clips(&clips);
            }
            ChordToolEvent::OpenExtractDialog { clips } => self.cur.chord_tools.extract_dialog = Some(clips),
            ChordToolEvent::CloseExtractDialog => self.cur.chord_tools.extract_dialog = None,
            ChordToolEvent::SetAuditionTrack(track) => {
                self.release_chord_audition();
                self.cur.chord_tools.view.audition_track = track;
            }
            ChordToolEvent::SetAuditionWhilePlaying(on) => self.cur.chord_tools.view.audition_while_playing = on,
            ChordToolEvent::SetVoicing(v) => {
                self.cur.chord_tools.view.voicing = v;
                self.cur.chord_tools.last_voiced.clear();
            }
            ChordToolEvent::SetDetectOptions(o) => self.cur.chord_tools.view.detect = o.sanitized(),
            ChordToolEvent::SetRecordArmed(on) => self.cur.chord_tools.record_armed = on,
        }
    }

    /// MIDI クリップ `clips` のノートから設定 `opts` でコードを検出して帯に置く。設定は外 (script / 保存値) から
    /// 来うるので有効域に揃える。1 つも見つからなければ、何も起きなかったように見えないよう status に出す。
    fn extract_midi_chords(&mut self, clips: &[(u32, u32)], opts: common::chord_detect::DetectOptions) {
        let opts = opts.sanitized();
        let placed = self.edit_song_checked(|song| {
            let found = song.chords_from_clips(clips, &opts);
            song.place_chords(found)
        });
        self.ui_ephemeral.status_message = if placed {
            "コードを抽出しました".to_string()
        } else {
            "コードが見つかりません (和音になっているノートがありません)".to_string()
        };
    }

    /// `ChordToolEvent::ExtractChords`: MIDI クリップはノートから即座に、オーディオクリップは音源から
    /// off-thread で (終わったら帯に置く)。
    pub(crate) fn extract_chords_from_clips(&mut self, clips: &[(u32, u32)]) {
        let song = self.cur.song_doc.song();
        let is_audio = |&(t, c): &(u32, u32)| {
            song.track_by_id(t)
                .and_then(|t| t.clips.iter().find(|x| x.id == c))
                .and_then(|clip| song.clip_contents.get(&clip.content_id))
                .is_some_and(|content| content.audio_events().is_some())
        };
        let (audio, midi): (Vec<_>, Vec<_>) = clips.iter().copied().partition(is_audio);
        if !midi.is_empty() {
            self.extract_midi_chords(&midi, self.cur.chord_tools.view.detect);
        }
        if !audio.is_empty() {
            self.detect_audio_clips(&audio, true);
        }
    }

    /// クリップの右クリック「コードを抽出…」: 対象は、右クリックしたクリップが選択に入っていれば選択の
    /// クリップ全部、でなければそのクリップだけ (Auto-Fade 等の「選択に効く」項目と同じ読み方)。
    pub(crate) fn open_chord_extract_dialog(&mut self, target: crate::app::ClipKey) {
        let selected = self.selected_clip_refs();
        let keys = if selected.contains(&target) { selected } else { vec![target] };
        let clips = keys.into_iter().map(|k| (k.track_id, k.clip_id)).collect();
        self.handle_event(crate::event::AppEvent::ChordTool(ChordToolEvent::OpenExtractDialog { clips }));
    }

    /// 保存値 (`ViewState.chord_tools`) を流し込む (`restore_view_state` から)。
    pub(crate) fn restore_chord_tools_view(&mut self, v: common::model::ChordToolsView) {
        self.release_chord_audition();
        self.cur.chord_tools.last_voiced.clear();
        self.cur.chord_tools.view = common::model::ChordToolsView { detect: v.detect.sanitized(), ..v };
    }
}
