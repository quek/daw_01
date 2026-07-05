//! Phase 7 B4 Step E (2026-05-13): MIDI export — `Song` を SMF format 1
//! の `.mid` ファイルとして書き出す。 全 MIDI track を 1 つの SMF 内に
//! 並列 track として出力 (= track 0 = tempo / time_sig meta-only、 track
//! 1..N = 各 daw_01 track の MIDI events)。
//!
//! `midly = "0.5"` の `Smf::save` を使用、 `MidiContent` (ClipContent::Midi)
//! を持つ clip だけを events に変換。 audio clip / automation clip は出力
//! 対象外 (skip)。 daw_01 の MIDI clip は note のみ保持 (CC / Pitch Bend を
//! model が持たない) ので NoteOn / NoteOff のみ出力する。
//!
//! tempo は SongTempo automation curve を tick 列に展開して track 0 に複数の
//! Tempo meta event として書く (A3 r.md #8)。 curve が無ければ曲頭 `song.bpm`
//! 1 つ。 SMF は step tempo のみなので ramp/bezier は一定解像度の階段近似で出力。

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

/// SMF track 0: tempo curve + time_sig + EndOfTrack。 SongTempo automation を
/// tick 位置付きの複数 Tempo meta に展開する (A3 r.md #8)。
fn build_meta_track(song: &Song) -> Vec<TrackEvent<'static>> {
    // (tick, MetaMessage) を集約して tick 順 delta-encode する (build_midi_track と同形)。
    let mut metas: Vec<(u32, MetaMessage)> = Vec::new();

    // TimeSignature: 曲頭 (tick 0)。 SMF の denominator は 2^log2 (4 → 2、 8 → 3)。
    let denom_log2 = denom_to_log2(song.time_sig.1);
    metas.push((
        0,
        MetaMessage::TimeSignature(song.time_sig.0, denom_log2, 24, 8),
    ));

    // Tempo: SongTempo curve を展開した各 breakpoint。
    for (tick, bpm) in tempo_breakpoints(song) {
        // `u24::from` は下位 24bit マスクの lossy 変換なので、 u24 上限
        // (16_777_215 = 約 3.58 BPM) を超える低速テンポは飽和させる (旧実装は
        // 1 BPM = 60_000_000 が ~6.2 BPM に化けていた)。
        let tempo_us = ((60_000_000.0_f64 / f64::from(bpm.max(1.0))).round() as u32)
            .min(0x00FF_FFFF);
        metas.push((tick, MetaMessage::Tempo(u24::from(tempo_us))));
    }

    // 同 tick では Tempo を TimeSignature より先に (DAW 慣習)。
    metas.sort_by_key(|(t, m)| (*t, matches!(m, MetaMessage::TimeSignature(..)) as u8));

    let mut events: Vec<TrackEvent<'static>> = Vec::new();
    let mut last_tick = 0u32;
    for (tick, m) in metas {
        let delta = tick.saturating_sub(last_tick);
        last_tick = tick;
        events.push(TrackEvent {
            delta: u28::from(delta),
            kind: TrackEventKind::Meta(m),
        });
    }
    events.push(TrackEvent {
        delta: u28::from(0u32),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    events
}

/// SongTempo curve を `(tick, bpm)` の breakpoint 列に展開する。 lane が無ければ
/// 曲頭 `song.bpm` の 1 点。 SMF は step tempo のみなので、 ramp/bezier は 1/8 拍
/// 解像度でサンプルし bpm が 0.05 超変わるたびに breakpoint を置く (= 階段近似)。
fn tempo_breakpoints(song: &Song) -> Vec<(u32, f32)> {
    let has_tempo_lane = song.song_lanes.iter().any(|l| {
        l.enabled && matches!(l.target, common::model::AutomationTarget::SongTempo)
    });
    if !has_tempo_lane {
        return vec![(0, song.bpm)];
    }
    let mut out: Vec<(u32, f32)> = Vec::new();
    // 曲本体 [0, length_beats) を走査 (end_beat は半開区間で除外 = automation clip
    // [..,end) の外に出て default へ revert する spurious な末尾 event を避ける)。
    let end_beat = song.length_beats.max(1.0);
    const STEP_BEATS: f64 = 0.125; // 1/8 note
    let mut beat = 0.0_f64;
    let mut prev_bpm = f32::NAN;
    while beat < end_beat {
        let bpm = common::automation::evaluate_song_tempo(song, beat);
        if prev_bpm.is_nan() || (bpm - prev_bpm).abs() > 0.05 {
            out.push((beat_to_tick(beat), bpm));
            prev_bpm = bpm;
        }
        beat += STEP_BEATS;
    }
    if out.is_empty() {
        out.push((0, song.bpm));
    }
    out
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
        // 再生 (daw_audio/src/sequencer.rs) と同じ 4 ゲート: muted clip / muted
        // note / On が clip 範囲内 / Off を clip 末端で clamp。 これが無いと
        // 「聞こえないはずのノート」 が SMF に入り、 トリムした clip もフル尺で出る。
        if clip.muted || clip.length_beats <= 0.0 {
            continue;
        }
        let clip_end_beats = clip.start_beat + clip.length_beats;
        for note in &midi.notes {
            if note.muted || note.duration_beats <= 0.0 {
                continue;
            }
            if note.start_beat < 0.0 || note.start_beat >= clip.length_beats {
                continue;
            }
            // clip-local beat → song-domain beat → tick。 Off は clip 末端 clamp。
            let on_beat = clip.start_beat + note.start_beat;
            let off_beat = (on_beat + note.duration_beats).min(clip_end_beats);
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
    use common::model::{
        AutomationClip, AutomationContent, AutomationCurve, AutomationLane, AutomationPoint,
        AutomationTarget, Clip, ClipContent, MidiContent, Note, Song,
    };

    fn note(pitch: u8, vel: u8, start: f64, dur: f64) -> Note {
        Note {
            id: 0,
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: vel,
            lyric: None,
            muted: false,
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

    /// A3 (r.md #8): SongTempo curve を複数の Tempo meta event に展開する
    /// (旧実装は曲頭 bpm の 1 つだけで、 テンポオートメーションを書き出せなかった)。
    #[test]
    fn tempo_curve_exports_multiple_tempo_events() {
        let mut song = Song {
            bpm: 60.0,
            length_beats: 4.0,
            ..Song::default()
        };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 60.0,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 4.0,
                        value: 120.0,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 3,
            }),
        );
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "tempo".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
            }],
            ..AutomationLane::new(AutomationTarget::SongTempo, 60.0)
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tempo.mid");
        export_midi(&song, &path).unwrap();
        let parsed_raw = std::fs::read(&path).unwrap();
        let parsed = Smf::parse(&parsed_raw).unwrap();
        let tempos: Vec<u32> = parsed.tracks[0]
            .iter()
            .filter_map(|e| match e.kind {
                TrackEventKind::Meta(MetaMessage::Tempo(us)) => Some(us.as_int()),
                _ => None,
            })
            .collect();
        assert!(
            tempos.len() > 5,
            "tempo ramp は多数の Tempo event を出すべき, got {}",
            tempos.len()
        );
        // 先頭 = 60bpm (1_000_000us)、 末尾 ≈ 120bpm (500_000us)。
        assert_eq!(tempos.first().copied(), Some(1_000_000), "head tempo = 60bpm");
        assert!(
            tempos.last().copied().unwrap() <= 510_000,
            "tail tempo ≈ 120bpm, got {:?}",
            tempos.last()
        );
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
                next_note_id: 1,
            }),
        );
        let track = crate::app::track_with(|t| {
            t.clips = vec![Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                ..Default::default()
            }];
            t.next_clip_id = 2;
        });
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
            tracks: vec![crate::app::track_with(|t| t.clips = vec![])],
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
