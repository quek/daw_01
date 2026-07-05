//! Walks the song's clips/notes and emits MIDI transitions for the next
//! audio buffer. Owned by daw_audio; called from each track's worker
//! before handing events off to the plugin host.
//!
//! Migrated from `daw_plugin_host` as part of A2 (audio-engine refactor).

#![allow(dead_code)]

use common::model::{Note, Song};
use common::process_data::MAX_EVENTS;

/// `active_notes` の RT-safe な上限。 push 前にこの値でクランプして
/// `Vec` 再確保 (= RT 違反) を防ぐ。 `midi_bus_a` の `MAX_EVENTS` (=256)
/// と同等にして、 1 buffer 内で出力しうる On 数を吸収する。
/// SSoT: backing の `PerTrackState::with_capacity` (mixer.rs) も同じ
/// `MAX_EVENTS` で確保しているので、 clamp が効く限り再確保は起きない。
const ACTIVE_NOTES_CAP: usize = MAX_EVENTS;

/// PR-V2.4: `note_id` を追加。 audio engine が track 内全 clip notes を
/// flatten した「通し index」 を振り、 plugin host (= builtin VOICEVOX) は
/// この id で `NoteMetadata` (歌詞 / phoneme) や合成 wav frame offset を
/// 引く。 CLAP / VST3 backend はこの field を無視する (= 既存 MIDI
/// pipeline はそのまま動く)。
#[derive(Debug, Clone, Copy)]
pub enum NoteTransition {
    On { note_id: u32, key: u8, velocity: f64 },
    Off { note_id: u32, key: u8 },
}

#[derive(Debug, Clone, Copy)]
pub struct TimedNoteEvent {
    pub time: u32,
    pub event: NoteTransition,
}

/// Phase 2 (`docs/plan_automation.md` §8.3): plugin parameter automation
/// 用の 1 イベント。`time` は buffer 内 sample offset、`param_id` は
/// CLAP `clap_id` / VST3 `ParamID` (共に u32)、`value` は plain 単位
/// (= plugin の `min_value..=max_value` スケール)。 plugin host 側で
/// CLAP `clap_event_param_value` / VST3 `IParameterChanges` に変換して
/// `plugin.process()` の input events に流す。
#[derive(Debug, Clone, Copy)]
pub struct TimedParamEvent {
    pub time: u32,
    pub param_id: u32,
    pub value: f64,
}

/// Per-track state owned exclusively by the audio worker that processes
/// the track. Survives across buffers so notes don't get cut on Stop /
/// loop-wrap.
#[derive(Default)]
pub struct PerTrackState {
    /// Pitches currently sounding on this track. Used to flush stuck notes
    /// on Stop / loop wrap.
    pub active_notes: Vec<u8>,
    /// NoteOffs that must fire at frame 0 of the *next* buffer (after
    /// Stop / clip-end) so notes don't hang.
    pub pending_offs: Vec<u8>,
    /// 鍵盤レーン click のプレビュー note (on/off)。 engine の `pump_commands`
    /// が `EngineCommand::PreviewNote*` を受けてここに積み、
    /// `process_track_owned` が frame 0 で `midi_bus_a` に注入して clear する。
    /// transport に関係なく注入されるので停止中でも発音する。 `active_notes`
    /// とは独立 (= sequencer の note 追跡を汚さない)。 lifecycle は GUI 所有
    /// (= mouse release で note-off を送る、 held-value + caller diff)。
    pub pending_preview: Vec<NoteTransition>,
}

impl PerTrackState {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            active_notes: Vec::with_capacity(cap),
            pending_offs: Vec::with_capacity(cap),
            pending_preview: Vec::with_capacity(cap),
        }
    }
}

/// Walk every clip on `track_idx` and emit `On` / `Off` events that fall
/// inside the half-open buffer `[playhead_beats, playhead_beats + buf_len_beats)`.
///
/// Phase 5 follow-up (MIDI tempo follow): beat-domain comparison。 caller の
/// engine が SongTempo lane を評価した `current_bpm` と、 累積 `playhead_beats`
/// を渡す。 sample-domain の `playhead: u64` は使わず (= 変動 tempo で sample
/// ↔ beat の線形変換が破綻するため)、 buffer 内の time offset (= note の sample
/// 位置) は `current_bpm` で beat → sample 換算する。 sub-buffer の tempo
/// 変化は scope 外 (= 1 buffer 内 constant tempo、 ~5..20ms なので user 体感 OK)。
///
/// `active_notes` is the audio worker's running set of pitches currently
/// sounding for this track — the caller maintains it across buffers so it
/// can flush stuck notes on Stop / loop wrap.
///
/// RT-safe: pushes into the caller-provided `out` (pre-allocated capacity)
/// and uses `sort_unstable_by_key` (in-place pdqsort).
#[allow(clippy::too_many_arguments)]
pub fn collect_events_for_buffer(
    song: Option<&Song>,
    track_idx: u32,
    sample_rate: u32,
    playhead_beats: f64,
    current_bpm: f32,
    frames: u32,
    out: &mut Vec<TimedNoteEvent>,
    active_notes: &mut Vec<u8>,
) {
    let Some(song) = song else { return };
    let Some(track) = song.tracks.get(track_idx as usize) else {
        return;
    };
    if current_bpm <= 0.0 {
        return;
    }

    let samples_per_beat = f64::from(sample_rate) * 60.0 / f64::from(current_bpm);
    // buffer 終端 beat (= playhead_beats + 1 buffer 分の beat 経過)。
    let buf_len_beats =
        f64::from(frames) * f64::from(current_bpm) / (60.0 * f64::from(sample_rate));
    let buf_end_beats = playhead_beats + buf_len_beats;

    // PR-V2.4: track 内全 clip の notes を flatten した「通し index」 を
    // note_id とする。 同 track の `sync_vocal_metadata` (daw_gui 側) と
    // 同じ番号体系で flush されるので、 builtin plugin 側で `note_id` →
    // 合成 wav frame offset の対応が成立する。 clip skip (= 範囲外 / 0
    // length) でも `note_id_base` は necessary に進める必要があるが、
    // current loop は `length_beats <= 0` の clip も notes を読まずに
    // skip している。 ここでは「skip した clip の notes は数えない」 と
    // 「skip しない clip 内 notes だけで通し番号」 のどちらでも builtin
    // plugin 側の expected note_id とずれる可能性がある。 sync_vocal_
    // metadata 側も同じ `length_beats <= 0` skip を入れて整合させるのが
    // 正しいが、 通常 audio engine 側は無効 clip を skip しないので
    // (= length_beats <= 0 は GUI で防がれる)、 ここは「全 clip notes を
    // 通し」 で実装する。
    let mut note_id_base: u32 = 0;
    for clip in &track.clips {
        // v6 linked clip: notes は Song.clip_contents から取り出す。
        // 共有 clip 群は同じ content から同じ notes を見るので、 別々の
        // 配置位置 (clip.start_beat) で同じ内容が再生される。
        let notes: &[Note] = song
            .clip_contents
            .get(&clip.content_id)
            .and_then(|c| c.notes())
            .unwrap_or(&[]);
        let clip_note_count = notes.len() as u32;

        // muted clip は全 note を skip (note_id 通し番号は維持して
        // builtin VOICEVOX 側の note_id 対応がずれないよう base は加算する)。
        if clip.muted {
            note_id_base += clip_note_count;
            continue;
        }

        if clip.length_beats <= 0.0 {
            note_id_base += clip_note_count;
            continue;
        }
        let clip_end_beats = clip.start_beat + clip.length_beats;
        // beat-domain で「clip が buffer 範囲外」 を判定 (= 旧 sample 比較を
        // 不要に)。 [clip.start_beat, clip.start_beat + length_beats) が
        // [playhead_beats, buf_end_beats) と重ならなければ skip。
        if clip_end_beats <= playhead_beats || clip.start_beat >= buf_end_beats {
            note_id_base += clip_note_count;
            continue;
        }

        for (note_idx, note) in notes.iter().enumerate() {
            let note_id = note_id_base + note_idx as u32;
            // muted note は On/Off を一切 emit しない (On を出さないので
            // stuck note にならない)。note_id 通し番号は enumerate で維持される。
            if note.muted {
                continue;
            }
            if note.duration_beats <= 0.0 {
                continue;
            }
            // Skip notes whose On is outside the clip — otherwise we could
            // emit On but lose Off to clamping, leaving a stuck note.
            if note.start_beat < 0.0 || note.start_beat >= clip.length_beats {
                continue;
            }
            // beat-domain で note の絶対 beat 位置を求める。 Off は clip 末端
            // で clamp (= 旧 sample-domain ロジックと同 idiom)。
            let on_abs_beat = clip.start_beat + note.start_beat;
            let raw_off_abs_beat =
                clip.start_beat + (note.start_beat + note.duration_beats);
            let off_abs_beat = raw_off_abs_beat.min(clip_end_beats);

            if on_abs_beat >= playhead_beats && on_abs_beat < buf_end_beats {
                // RT-safe: 容量超過分は drop し `Vec` 再確保を避ける。 On を
                // drop したら対応する `active_notes` も積まず整合を保つ
                // (= 後で flush しても残らない)。 `out` は `MAX_EVENTS`、
                // `active_notes` は `ACTIVE_NOTES_CAP` でクランプ。
                if out.len() >= MAX_EVENTS || active_notes.len() >= ACTIVE_NOTES_CAP {
                    continue;
                }
                let time_samples =
                    ((on_abs_beat - playhead_beats) * samples_per_beat).max(0.0);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let time = time_samples as u32;
                out.push(TimedNoteEvent {
                    time,
                    event: NoteTransition::On {
                        note_id,
                        key: note.pitch,
                        velocity: f64::from(note.velocity) / 127.0,
                    },
                });
                active_notes.push(note.pitch);
            }
            if off_abs_beat > on_abs_beat
                && off_abs_beat >= playhead_beats
                && off_abs_beat < buf_end_beats
            {
                // RT-safe: `out` 容量超過時は Off を emit せず、 `active_notes`
                // からも除かない (= 後続の Stop / loop-wrap flush で NoteOff が
                // 送られ note が残らない)。 push できない Off を握りつぶして
                // 追跡解除すると stuck note になるため、 両方とも skip する。
                if out.len() >= MAX_EVENTS {
                    continue;
                }
                let time_samples =
                    ((off_abs_beat - playhead_beats) * samples_per_beat).max(0.0);
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss
                )]
                let time = time_samples as u32;
                out.push(TimedNoteEvent {
                    time,
                    event: NoteTransition::Off {
                        note_id,
                        key: note.pitch,
                    },
                });
                if let Some(pos) = active_notes.iter().position(|&k| k == note.pitch) {
                    active_notes.swap_remove(pos);
                }
            }
        }
        note_id_base += clip_note_count;
    }

    // (talk) 読み上げトリガ (`docs/plan_voicevox_talk.md` §3.4)。VOICEVOX デバイス付き
    // トラックの `ClipContent::Text` の各 TextEvent 開始位置で、合成 note_on を発火する。
    // note_id = `talk_event_id(clip.id, event_index)` (= builtin の note_offsets と対応する
    // high band id)。builtin は wav 終端で自動 drain するので note_off は不要 (= active_notes
    // にも積まない)。空テキストは flush 側 (sync_vocal_metadata) と同条件で skip して
    // event_id の対応を保つ。歌唱 MIDI clip と talk Text clip が混在しても、note_id (= 小さい
    // 通し index) と event_id (= high band) は衝突しない。
    if track.is_voicevox_vocal() {
        for clip in &track.clips {
            // muted な Text(読み上げ) clip は talk note_on を発火しない。
            if clip.muted {
                continue;
            }
            let Some(events) = song
                .clip_contents
                .get(&clip.content_id)
                .and_then(|c| c.text_events())
            else {
                continue;
            };
            for (event_index, ev) in events.iter().enumerate() {
                if ev.text.is_empty() {
                    continue;
                }
                let on_abs_beat = clip.start_beat + ev.event_start_in_clip_beats;
                if on_abs_beat >= playhead_beats
                    && on_abs_beat < buf_end_beats
                    && out.len() < MAX_EVENTS
                {
                    let time_samples =
                        ((on_abs_beat - playhead_beats) * samples_per_beat).max(0.0);
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let time = time_samples as u32;
                    out.push(TimedNoteEvent {
                        time,
                        event: NoteTransition::On {
                            note_id: common::plugin_metadata::talk_event_id(
                                clip.id,
                                event_index as u32,
                            ),
                            key: 0,
                            velocity: 1.0,
                        },
                    });
                }
            }
        }
    }

    // CLAP requires in-events sorted by time. At equal times, Off must come
    // before On so a re-attack at the same frame doesn't drop because the
    // synth saw On→Off in the same buffer.
    out.sort_unstable_by_key(|e| {
        let priority: u8 = match e.event {
            NoteTransition::Off { .. } => 0,
            NoteTransition::On { .. } => 1,
        };
        (e.time, priority)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Clip, ClipContent, MidiContent, Track};

    /// v23 single-chain: `Track` の `legacy_*` migration fields は `common`
    /// に `pub(crate)` で閉じているので、 downstream の test では
    /// `Track { .., ..Track::default() }` が E0451。 `Track::default()` を
    /// mutator で埋める helper で回避する。
    fn track(f: impl FnOnce(&mut Track)) -> Track {
        let mut t = Track::default();
        f(&mut t);
        t
    }

    fn one_note_song(start_beat: f64, duration_beats: f64, pitch: u8) -> Song {
        // v6: notes は Song.clip_contents に置く。 inline の `notes:` は
        // legacy field (空) のままで、 ensure_clip_contents が migrate する
        // 想定だが、 ここでは直接 clip_contents を構築して migrate を挟まず
        // production と同形にする。
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let content_id = song.alloc_content_id();
        song.clip_contents.insert(
            content_id,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    id: 1,
                    start_beat,
                    duration_beats,
                    pitch,
                    velocity: 100,
                    lyric: None,
                    muted: false,
                }],
                next_note_id: 2,
            }),
        );
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.clips = vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 8.0,
                content_id,
                color: None,
                auto_lipsync: false,
                ..Default::default()
            }];
        }));
        song
    }

    /// 120 BPM, 48 kHz: samples_per_beat = 24000.
    const SR: u32 = 48000;
    const SPB: u64 = 24_000;

    #[test]
    fn note_starting_at_buffer_zero_emits_on_at_time_zero() {
        let song = one_note_song(0.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].time, 0);
        assert!(matches!(out[0].event, NoteTransition::On { key: 60, .. }));
        assert_eq!(active, vec![60]);
    }

    /// muted clip は note イベントを 1 つも emit しない。
    #[test]
    fn muted_clip_emits_no_note_events() {
        let mut song = one_note_song(0.0, 1.0, 60);
        song.tracks[0].clips[0].muted = true;
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty(), "muted clip must emit no events");
        assert!(active.is_empty());
    }

    /// muted note を skip しても、 同 clip 内の sibling note の `note_id`
    /// (= enumerate 通し index) はずれない (builtin VOICEVOX の note_id ↔ 合成 wav
    /// frame offset 対応を壊さないための不変条件)。
    #[test]
    fn muted_note_skipped_but_sibling_keeps_running_note_id() {
        let mut song = one_note_song(0.0, 1.0, 60);
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Midi(m)) = song.clip_contents.get_mut(&cid) {
            // idx 0 = mute、 idx 1 = 鳴る (pitch 64)。
            m.notes[0].muted = true;
            m.notes.push(Note {
                id: 2,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 64,
                velocity: 100,
                lyric: None,
                muted: false,
            });
        }
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        let ons: Vec<_> = out
            .iter()
            .filter(|e| matches!(e.event, NoteTransition::On { .. }))
            .collect();
        assert_eq!(ons.len(), 1, "only the unmuted sibling emits On");
        match ons[0].event {
            NoteTransition::On { note_id, key, .. } => {
                assert_eq!(key, 64);
                assert_eq!(
                    note_id, 1,
                    "unmuted sibling keeps running note_id = enumerate index 1"
                );
            }
            NoteTransition::Off { .. } => unreachable!(),
        }
        assert_eq!(active, vec![64]);
    }

    #[test]
    fn note_off_emitted_in_buffer_containing_end() {
        let song = one_note_song(0.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = vec![60u8];
        // SPB-100 samples ≈ beat 0.9958 (= 1 beat 直前)、 buffer 200 frames で
        // beat 1.0 の note off を捕まえる。 sample→beat 換算は `samples / SPB`。
        let playhead_beats = (SPB - 100) as f64 / SPB as f64;
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            playhead_beats,
            120.0,
            200,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].event, NoteTransition::Off { key: 60, .. }));
        assert!(active.is_empty(), "active set must drop the off note");
    }

    #[test]
    fn note_entirely_inside_buffer_emits_on_then_off() {
        let song = one_note_song(0.0, 0.01, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0].event, NoteTransition::On { key: 60, .. }));
        assert!(matches!(out[1].event, NoteTransition::Off { key: 60, .. }));
        assert!(out[0].time < out[1].time);
        assert!(active.is_empty());
    }

    #[test]
    fn chord_emits_two_ons_at_same_time() {
        let mut song = one_note_song(0.0, 1.0, 60);
        let cid = song.tracks[0].clips[0].content_id;
        song.clip_contents
            .get_mut(&cid)
            .unwrap()
            .notes_mut()
            .expect("Midi variant")
            .push(Note {
                id: 2,
                start_beat: 0.0,
                duration_beats: 1.0,
                pitch: 64,
                velocity: 100,
                lyric: None,
                muted: false,
            });
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 2);
        for e in &out {
            assert_eq!(e.time, 0);
            assert!(matches!(e.event, NoteTransition::On { .. }));
        }
        active.sort_unstable();
        assert_eq!(active, vec![60, 64]);
    }

    #[test]
    fn no_song_returns_empty() {
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            None,
            0,
            SR,
            0.0,
            120.0,
            1024,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn note_outside_buffer_emits_nothing() {
        let song = one_note_song(2.0, 1.0, 60);
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            1000,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn note_extending_past_clip_end_is_clamped() {
        let mut song = one_note_song(7.0, 4.0, 60);
        song.tracks[0].clips[0].length_beats = 8.0;
        let playhead = 8 * SPB - 100;
        let frames = 200u32;
        let mut out = Vec::new();
        let mut active = vec![60u8];
        // playhead (samples) を beat に変換: samples / SPB。
        let playhead_beats = playhead as f64 / SPB as f64;
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            playhead_beats,
            120.0,
            frames,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].event, NoteTransition::Off { key: 60, .. }));
        assert!(active.is_empty());
    }

    #[test]
    fn note_past_clip_end_is_skipped_entirely() {
        let mut song = one_note_song(10.0, 1.0, 60);
        song.tracks[0].clips[0].length_beats = 4.0;
        let mut out = Vec::new();
        let mut active = Vec::new();
        let playhead_beats = (10 * SPB - 100) as f64 / SPB as f64;
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            playhead_beats,
            120.0,
            200,
            &mut out,
            &mut active,
        );
        assert!(out.is_empty());
        assert!(active.is_empty());
    }

    #[test]
    fn output_is_sorted_with_off_before_on_at_same_time() {
        let mut song = Song {
            bpm: 120.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note {
                        id: 1,
                        start_beat: 0.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: None,
                        muted: false,
                    },
                    Note {
                        id: 2,
                        start_beat: 1.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: None,
                        muted: false,
                    },
                ],
                next_note_id: 3,
            }),
        );
        song.tracks.push(track(|t| {
            t.id = 1;
            t.name = "T".into();
            t.clips = vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                color: None,
                auto_lipsync: false,
                ..Default::default()
            }];
        }));
        let mut out = Vec::new();
        let mut active = Vec::new();
        collect_events_for_buffer(
            Some(&song),
            0,
            SR,
            0.0,
            120.0,
            (2 * SPB) as u32,
            &mut out,
            &mut active,
        );
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].event, NoteTransition::On { .. }));
        assert_eq!(out[0].time, 0);
        assert!(matches!(out[1].event, NoteTransition::Off { .. }));
        assert!(matches!(out[2].event, NoteTransition::On { .. }));
        assert_eq!(out[1].time, out[2].time);
    }
}
