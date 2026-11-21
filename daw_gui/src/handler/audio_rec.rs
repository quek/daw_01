//! handler::audio_rec — オーディオ入力の録音 (r.md #156)。
//!
//! - トラックの「入力」と「モニター」の編集 (インスペクタ)。`Song` の編集なので `edit_song` を通り、
//!   engine へはフレーム末の同期 (`LoadSong`) で届く。
//! - デバイスの入力 ch の一覧 ([`AppData::audio_input_choices`]): インスペクタの「入力」の選択肢。

use common::model::{MASTER_TRACK_ID, MonitorMode, TrackInput};

use crate::event_audio_rec::AudioRecEvent;
use crate::state::*;

/// インスペクタの「入力」の選択肢 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct InputChoice {
    pub input: TrackInput,
    pub label: String,
}

impl AppData {
    /// [`AppEvent::AudioRec`](crate::app::AppEvent::AudioRec) の入口。
    pub fn handle_audio_rec_event(&mut self, ev: AudioRecEvent) {
        use AudioRecEvent as E;
        match ev {
            E::SetTrackInput { track_ids, input } => self.set_tracks_input(&track_ids, input),
            E::SetTrackMonitor { track_ids, monitor } => self.set_tracks_monitor(&track_ids, monitor),
        }
    }

    /// インスペクタの「入力 / モニター」が編集するトラック: 複数選択ならその全部、そうでなければ
    /// cursor track。master は入力を持たない。
    #[must_use]
    pub fn input_target_track_ids(&self) -> Vec<u32> {
        let song = self.cur.song_doc.song();
        let selected = &self.cur.selection.selected_track_ids;
        let ids: Vec<u32> = if selected.len() > 1 {
            selected.iter().copied().collect()
        } else {
            self.cursor_track_id().into_iter().collect()
        };
        ids.into_iter().filter(|&id| id != MASTER_TRACK_ID && song.track_by_id(id).is_some()).collect()
    }

    /// 今開いているデバイスの入力 ch の名前 (index = デバイスの入力 ch 番号)。
    #[must_use]
    pub fn device_input_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// 「入力」の選択肢: MIDI、デバイスの入力 ch (モノ)、隣り合う 2 ch (ステレオ、1+2 / 3+4 …)。
    /// `current` (いま選ばれている入力) がデバイスに無ければ、末尾に「無い入力」として足す
    /// (黙って別の ch に付け替えない)。
    #[must_use]
    pub fn audio_input_choices(&self, current: &[TrackInput]) -> Vec<InputChoice> {
        let names = self.device_input_names();
        let mut out = vec![InputChoice { input: TrackInput::Midi, label: TrackInput::Midi.label() }];
        for (ch, name) in names.iter().enumerate() {
            let Ok(channel) = u16::try_from(ch) else { break };
            let input = TrackInput::AudioMono { channel };
            let label = if name.is_empty() { input.label() } else { format!("{} ({name})", input.label()) };
            out.push(InputChoice { input, label });
        }
        for left in (0..names.len().saturating_sub(1)).step_by(2) {
            let Ok(left) = u16::try_from(left) else { break };
            let input = TrackInput::AudioStereo { left };
            out.push(InputChoice { input, label: format!("{} (ステレオ)", input.label()) });
        }
        for &input in current {
            if !out.iter().any(|c| c.input == input) {
                out.push(InputChoice { input, label: format!("{} (無い入力)", input.label()) });
            }
        }
        out
    }

    /// `track_ids` の入力を `input` に揃える (1 undo step、変化が無ければ何もしない)。
    fn set_tracks_input(&mut self, track_ids: &[u32], input: TrackInput) {
        self.edit_song_checked(|song| {
            let mut changed = false;
            for t in song.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
                changed |= t.input != input;
                t.input = input;
            }
            changed
        });
    }

    /// `track_ids` のモニターを `monitor` に揃える (1 undo step、変化が無ければ何もしない)。
    fn set_tracks_monitor(&mut self, track_ids: &[u32], monitor: MonitorMode) {
        self.edit_song_checked(|song| {
            let mut changed = false;
            for t in song.tracks.iter_mut().filter(|t| track_ids.contains(&t.id)) {
                changed |= t.monitor != monitor;
                t.monitor = monitor;
            }
            changed
        });
    }
}
