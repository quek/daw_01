//! オーディオ入力の録音 (r.md #156) を Song に置く。

use super::*;
use crate::audio_take::AudioTakeSegment;

/// engine が書いた録音 WAV 1 本 (1 トラックぶん)。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedWav {
    pub path: AudioSourcePath,
    /// 録音したサンプルレート (= デバイス = engine のレート)。
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

impl Song {
    /// 録音した WAV を pool に入れ、区間ごとのクリップを `track_id` のアレンジへ置く。置いた素材の id を返す
    /// (トラックが無い / 置ける区間が無ければ何も変えずに `None`)。
    ///
    /// `segments` は「WAV のフレーム `file_frame` から先が、曲の `start_beat` から鳴っていた」の並び
    /// (`file_frame` の昇順)。ふつうは 1 つで、ループの折り返しや録音中の seek のたびに 1 つ増える。区間 1 つが
    /// クリップ 1 つになり、録った順に [`Track::place_clip`] で置く — 掛かる既存クリップは上書き規則で削れるので、
    /// ループ録音は後の周が前の周を上書きする (音は同じ WAV に全部残っている)。
    ///
    /// **`stretch_mode` は `Raw`。** 録った音は「そのときのテンポで鳴っていた実時間の波形」なので、焼き込みと
    /// 同じ理由 ([`Self::add_baked_audio`]) でテンポ追従を通さない。クリップの長さは録った実時間をテンポ
    /// (オートメーション込み) で拍へ直す。後から追従させられるよう `original_bpm` は入れておく。
    pub fn place_recorded_take(
        &mut self,
        track_id: u32,
        wav: RecordedWav,
        segments: &[AudioTakeSegment],
        name: &str,
    ) -> Option<AudioSourceId> {
        self.track_by_id(track_id)?;
        let sr = wav.sample_rate;
        // (曲の開始拍, 長さ (拍), WAV の区間)。空の区間は捨てる。
        let windows: Vec<(f64, f64, u64, u64)> = segments
            .iter()
            .enumerate()
            .filter_map(|(i, seg)| {
                let from = seg.file_frame.min(wav.frames);
                let to = segments.get(i + 1).map_or(wav.frames, |next| next.file_frame.min(wav.frames));
                let start_sample = crate::automation::beats_to_samples(self, sr, seg.start_beat);
                let end_beat = crate::automation::samples_to_beats(self, sr, start_sample + to.saturating_sub(from));
                let length = end_beat - seg.start_beat;
                (to > from && length > 0.0).then_some((seg.start_beat, length, from, to))
            })
            .collect();
        if windows.is_empty() {
            return None;
        }
        let source_id = self.alloc_audio_source_id();
        self.media.audio_sources.insert(
            source_id,
            AudioSource {
                path: wav.path,
                sample_rate: sr,
                channels: wav.channels,
                frames: wav.frames,
                original_bpm: Some(self.bpm),
                root_key: None,
            },
        );
        for (start_beat, length_beats, from, to) in windows {
            let event = AudioEvent {
                // 新規 content の単一 event なので id=1 / allocator は 2 から。
                id: 1,
                source_id,
                event_start_in_clip_beats: 0.0,
                event_length_beats: length_beats,
                source_start_frames: from,
                source_end_frames: to,
                stretch_mode: StretchMode::Raw,
                ..AudioEvent::default()
            };
            let content = ClipContent::Audio(AudioContent { events: vec![event], next_event_id: 2 });
            let content_id = self.alloc_content(content, name.to_string());
            let clip = Clip { start_beat, length_beats, content_id, ..Default::default() };
            if let Some(track) = self.track_by_id_mut(track_id) {
                track.place_clip(clip);
            }
        }
        Some(source_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 120 BPM / 48 kHz: 1 拍 = 24000 フレーム。
    fn song_with_track() -> (Song, u32) {
        let id = 7;
        (Song { bpm: 120.0, tracks: vec![Track { id, ..Track::default() }], ..Song::default() }, id)
    }

    fn wav(frames: u64) -> RecordedWav {
        RecordedWav { path: AudioSourcePath::Absolute("take.wav".into()), sample_rate: 48_000, channels: 1, frames }
    }

    fn clips(song: &Song, track_id: u32) -> Vec<(f64, f64, u64, u64)> {
        let track = song.track_by_id(track_id).unwrap();
        let mut out: Vec<_> = track
            .clips
            .iter()
            .map(|c| {
                let Some(ClipContent::Audio(content)) = song.clip_contents.get(&c.content_id) else { panic!() };
                let e = &content.events[0];
                (c.start_beat, c.length_beats, e.source_start_frames, e.source_end_frames)
            })
            .collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    #[test]
    fn 録音は_wav_の長さぶんのクリップになる() {
        let (mut song, id) = song_with_track();
        let seg = [AudioTakeSegment { file_frame: 0, start_beat: 4.0 }];
        let source = song.place_recorded_take(id, wav(36_000), &seg, "In 録音").unwrap();
        assert_eq!(clips(&song, id), [(4.0, 1.5, 0, 36_000)]);
        let s = &song.media.audio_sources[&source];
        assert_eq!((s.channels, s.frames, s.sample_rate, s.original_bpm), (1, 36_000, 48_000, Some(120.0)));
    }

    /// ループ録音 (2 拍のループを 1 周半): 後の周が前の周の頭を上書きし、前の周の残りが後ろに残る。
    #[test]
    fn 区間ごとにクリップができ_後の区間が前の区間を上書きする() {
        let (mut song, id) = song_with_track();
        let segs = [
            AudioTakeSegment { file_frame: 0, start_beat: 8.0 },
            AudioTakeSegment { file_frame: 48_000, start_beat: 8.0 },
        ];
        song.place_recorded_take(id, wav(72_000), &segs, "In 録音").unwrap();
        // 1 周目 [8, 10) のうち、2 周目 [8, 9) に掛からない後ろ半分が残る (content の窓が 1 拍ずれる)。
        let track = song.track_by_id(id).unwrap();
        let mut windows: Vec<(f64, f64, f64)> =
            track.clips.iter().map(|c| (c.start_beat, c.length_beats, c.content_offset_beats)).collect();
        windows.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(windows, [(8.0, 1.0, 0.0), (9.0, 1.0, 1.0)]);
    }

    #[test]
    fn 置ける区間が無ければ何も変えない() {
        let (mut song, id) = song_with_track();
        let before = song.clone();
        assert_eq!(song.place_recorded_take(id, wav(0), &[AudioTakeSegment { file_frame: 0, start_beat: 0.0 }], ""), None);
        assert_eq!(song.place_recorded_take(id, wav(100), &[], ""), None);
        assert_eq!(song.place_recorded_take(id + 99, wav(100), &[AudioTakeSegment { file_frame: 0, start_beat: 0.0 }], ""), None);
        assert_eq!(song, before);
    }
}
