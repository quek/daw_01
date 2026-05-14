//! Phase 7 B4 Step E (2026-05-13): MIDI export — `Song` を SMF format 1
//! の `.mid` ファイルとして書き出す。 全 MIDI track を 1 つの SMF 内に
//! 並列 track として出力 (= track 0 = tempo / time_sig meta-only、 track
//! 1..N = 各 daw_01 track の MIDI events)。
//!
//! `midly = "0.5"` の `Smf::save` を使用、 `MidiContent` (ClipContent::Midi)
//! を持つ clip だけを events に変換。 audio clip / automation clip は出力
//! 対象外 (skip)。 minimum スコープ: NoteOn / NoteOff のみ (CC / Pitch
//! Bend は本フェーズ範囲外、 Phase 7+ で MIDI input event 拡張時に追加)。
//!
//! tempo は SongTempo automation lane を考慮せず曲頭の `song.bpm` 1 つで
//! 出力 (= SMF spec 上 tempo events は track 0 内 delta-tick 順で複数置ける
//! が、 daw_01 の SongTempo curve を tick 列に展開する処理は別 phase)。

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use midly::num::{u4, u7, u15, u24, u28};
use midly::{
    Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent,
    TrackEventKind,
};

use common::model::{ClipContent, Song};

/// SMF spec 上の Pulses Per Quarter Note (PPQ)。 480 が業界標準 (Cubase /
/// Logic / Bitwig / Live のデフォルト)、 1 quarter note = 480 ticks。
const PPQ: u16 = 480;

/// `Song` を SMF format 1 として `path` に書き出す。 全 MIDI track を並列
/// 出力、 track 0 は tempo / time_sig meta、 track 1..N は各 daw_01 track の
/// 音符 events (NoteOn / NoteOff、 channel 0 固定)。
///
/// audio / automation clip は skip。 MIDI clip が 1 つも無い track (= 全 audio
/// track 等) は出力されない。 全 track が空でも track 0 は必ず出力される
/// (= empty SMF1 として valid)。
pub fn export_midi(song: &Song, path: &Path) -> Result<()> {
    let header = Header::new(Format::Parallel, Timing::Metrical(u15::from(PPQ)));
    let mut smf = Smf::new(header);

    // Track 0: tempo + time_sig meta-only。
    smf.tracks.push(build_meta_track(song));

    // Track 1..N: 各 daw_01 track の MIDI events (1 daw_01 track = 1 SMF track)。
    for track in &song.tracks {
        if let Some(events) = build_midi_track(song, track) {
            smf.tracks.push(events);
        }
    }

    let file = File::create(path)
        .with_context(|| format!("failed to create MIDI file at {}", path.display()))?;
    smf.write_std(file)
        .with_context(|| format!("failed to write SMF1 to {}", path.display()))?;
    Ok(())
}

/// SMF track 0: tempo + time_sig + EndOfTrack。 全 delta = 0 (= 曲頭で立てる)。
/// daw_01 の SongTempo automation curve は本フェーズでは展開しない (= 曲頭
/// `song.bpm` 1 つだけ書く)、 将来的には curve sample 列を tick 単位で
/// MetaMessage::Tempo として並べるが minimum scope 外。
fn build_meta_track(song: &Song) -> Vec<TrackEvent<'static>> {
    let mut events: Vec<TrackEvent<'static>> = Vec::new();
    // Tempo: 1 quarter note の microseconds = 60_000_000 / bpm。
    let bpm = song.bpm.max(1.0);
    let tempo_us = (60_000_000.0_f64 / f64::from(bpm)).round() as u32;
    events.push(TrackEvent {
        delta: u28::from(0u32),
        kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(tempo_us))),
    });
    // TimeSignature(numerator, denominator_log2, clocks_per_click, 32nd_per_quarter)。
    // SMF spec の `denominator` は 2^denom_log2 を意味する (= 4 → 2、 8 → 3)。
    let denom_log2 = denom_to_log2(song.time_sig.1);
    events.push(TrackEvent {
        delta: u28::from(0u32),
        kind: TrackEventKind::Meta(MetaMessage::TimeSignature(
            song.time_sig.0,
            denom_log2,
            24, // metronome clocks per click (24 = standard MIDI default)
            8,  // 32nd notes per quarter (= 8、 業界標準)
        )),
    });
    events.push(TrackEvent {
        delta: u28::from(0u32),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    events
}

/// daw_01 track から MIDI events を build。 MIDI clip (`ClipContent::Midi`) が
/// 1 つも無ければ None (= track 自体を出力 skip)。 1 つでもあれば、 全 clip の
/// notes を beat → tick 換算で並べた SMF track を返す。
fn build_midi_track(song: &Song, track: &common::model::Track) -> Option<Vec<TrackEvent<'static>>> {
    // (tick, MidiMessage) の (NoteOn + NoteOff) を集約。
    let mut events: Vec<(u32, MidiMessage)> = Vec::new();
    let mut has_any_midi = false;
    for clip in &track.clips {
        let Some(content) = song.clip_contents.get(&clip.content_id) else {
            continue;
        };
        let ClipContent::Midi(midi) = content else {
            continue;
        };
        has_any_midi = true;
        for note in &midi.notes {
            // clip-local beat → song-domain beat → tick。
            let on_beat = clip.start_beat + note.start_beat;
            let off_beat = on_beat + note.duration_beats;
            let on_tick = beat_to_tick(on_beat);
            let off_tick = beat_to_tick(off_beat).max(on_tick + 1);
            events.push((
                on_tick,
                MidiMessage::NoteOn {
                    key: u7::from(note.pitch.min(127)),
                    vel: u7::from(note.velocity.min(127)),
                },
            ));
            events.push((
                off_tick,
                MidiMessage::NoteOff {
                    key: u7::from(note.pitch.min(127)),
                    vel: u7::from(0),
                },
            ));
        }
    }
    if !has_any_midi {
        return None;
    }
    // tick 順 sort。 同 tick 内の NoteOff → NoteOn 順 (= 同位置 retrigger を
    // 避ける、 SMF 業界標準)。
    events.sort_by_key(|(tick, msg)| {
        let kind_order = match msg {
            MidiMessage::NoteOff { .. } => 0,
            _ => 1,
        };
        (*tick, kind_order)
    });
    // delta-tick 化 + EndOfTrack 終端。
    let mut track_events: Vec<TrackEvent<'static>> = Vec::new();
    let mut last_tick = 0u32;
    for (tick, msg) in events {
        let delta = tick.saturating_sub(last_tick);
        last_tick = tick;
        track_events.push(TrackEvent {
            delta: u28::from(delta),
            kind: TrackEventKind::Midi {
                channel: u4::from(0),
                message: msg,
            },
        });
    }
    track_events.push(TrackEvent {
        delta: u28::from(0u32),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    Some(track_events)
}

/// beat → SMF tick (= 1 beat = PPQ ticks)。
fn beat_to_tick(beat: f64) -> u32 {
    (beat * f64::from(PPQ)).round().max(0.0) as u32
}

/// SMF spec の TimeSignature.denominator_log2 (= 2^N、 e.g., 4 → 2、 8 → 3、
/// 16 → 4)。 daw_01 model の time_sig.1 は denominator 値そのもの (4 / 8 等)
/// なので変換が必要。 不正値は default 2 (= 4 分音符) で fallback。
fn denom_to_log2(denom: u8) -> u8 {
    match denom {
        2 => 1,
        4 => 2,
        8 => 3,
        16 => 4,
        32 => 5,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::model::{Clip, ClipContent, MidiContent, Note, Song, Track};

    fn note(pitch: u8, vel: u8, start: f64, dur: f64) -> Note {
        Note {
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: vel,
            lyric: None,
        }
    }

    #[test]
    fn empty_song_writes_meta_only_smf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.mid");
        let song = Song::default();
        export_midi(&song, &path).unwrap();
        // Re-parse to verify SMF1 validity。
        let raw = std::fs::read(&path).unwrap();
        let parsed = Smf::parse(&raw).unwrap();
        assert!(matches!(parsed.header.format, Format::Parallel));
        assert!(matches!(parsed.header.timing, Timing::Metrical(_)));
        // track 0 = meta + EndOfTrack (3 events: tempo, timesig, EOT)。
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.tracks[0].len(), 3);
    }

    #[test]
    fn single_midi_track_one_note_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one_note.mid");
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![note(60, 100, 0.0, 1.0)], // C4 quarter note at beat 0
            }),
        );
        let track = Track {
            clips: vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                name: "test".into(),
                content_id: cid,
                ..Default::default()
            }],
            next_clip_id: 2,
            ..Default::default()
        };
        song.tracks.push(track);
        export_midi(&song, &path).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let parsed = Smf::parse(&raw).unwrap();
        assert_eq!(parsed.tracks.len(), 2); // meta + 1 midi track
        // midi track: NoteOn(0 delta) + NoteOff(PPQ delta) + EOT
        let midi_track = &parsed.tracks[1];
        assert_eq!(midi_track.len(), 3);
        let on = &midi_track[0];
        assert_eq!(on.delta.as_int(), 0);
        match on.kind {
            TrackEventKind::Midi {
                channel,
                message: MidiMessage::NoteOn { key, vel },
            } => {
                assert_eq!(channel.as_int(), 0);
                assert_eq!(key.as_int(), 60);
                assert_eq!(vel.as_int(), 100);
            }
            _ => panic!("expected NoteOn, got {on:?}"),
        }
        let off = &midi_track[1];
        assert_eq!(off.delta.as_int(), u32::from(PPQ)); // 1 beat = PPQ ticks
        match off.kind {
            TrackEventKind::Midi {
                message: MidiMessage::NoteOff { key, vel },
                ..
            } => {
                assert_eq!(key.as_int(), 60);
                assert_eq!(vel.as_int(), 0);
            }
            _ => panic!("expected NoteOff, got {off:?}"),
        }
        let eot = &midi_track[2];
        assert!(matches!(
            eot.kind,
            TrackEventKind::Meta(MetaMessage::EndOfTrack)
        ));
    }

    #[test]
    fn audio_only_track_is_skipped() {
        // Track に MIDI clip 1 つも無ければ build_midi_track が None を返す
        // (= SMF に含まれない)。
        let song = Song {
            tracks: vec![Track {
                clips: vec![],
                ..Default::default()
            }],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audio_only.mid");
        export_midi(&song, &path).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let parsed = Smf::parse(&raw).unwrap();
        // meta track のみ。
        assert_eq!(parsed.tracks.len(), 1);
    }

    #[test]
    fn beat_to_tick_uses_ppq() {
        assert_eq!(beat_to_tick(0.0), 0);
        assert_eq!(beat_to_tick(1.0), u32::from(PPQ));
        assert_eq!(beat_to_tick(4.0), u32::from(PPQ) * 4);
    }

    #[test]
    fn denom_to_log2_known_values() {
        assert_eq!(denom_to_log2(4), 2);
        assert_eq!(denom_to_log2(8), 3);
        assert_eq!(denom_to_log2(16), 4);
        assert_eq!(denom_to_log2(99), 2); // unknown → default 2
    }
}
