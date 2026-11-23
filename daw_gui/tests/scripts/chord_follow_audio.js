// r.md #157 (`docs/plan_chord_track.md` §3.2): オーディオクリップのコードトラックへの追従を、daw_audio の
// 書き出しまで通して確かめる。harness: daw_gui/tests/chord_follow_audio.rs (440 Hz のサイン波 WAV を書いて
// --arg wav= / dir= で渡し、書き出した WAV の区間ごとの音程を測る)。
//
// コード: C [0,4) / F [4,8) / A [8,12) 拍 (120 BPM で 2 秒ずつ)。トラックは平行移動で追従する。
// r.md #159: 置いた時点のコードがメロディー (和音を弾いていない素材) の基準。
//   follow_none.wav      : 和音を弾いていない素材にコードを置いただけ → 0 / 0 / 0 半音
//   follow_am.wav        : 素材が Am を弾いている (和音を弾いている区間として保存) → +3 / -4 / 0 半音
//   follow_rewritten.wav : 和音を弾いていない素材のまま、コード列を +2 に書き換える → +2 / +2 / +2 半音
//   follow_off.wav       : 曲全体のスイッチを切る → 全部書いた音 (0 半音)

function fail(msg) {
  throw new Error("chord_follow_audio: " + msg);
}
function chordEvent(ev) {
  daw.chordEvent(JSON.stringify(ev));
}
function chord(text) {
  const json = daw.parseChord(text);
  if (json === null) fail("parseChord(" + text + ") が読めない");
  return JSON.parse(json);
}

const wavPath = daw.scriptArgs.wav;
const outDir = daw.scriptArgs.dir;
if (!wavPath || !outDir) fail("--arg wav=<path> と --arg dir=<dir> が要る");

// 120 BPM → 1 拍 = 24000 frames @48k。source は 6 秒 = 12 拍。
const SOURCE_FRAMES = 288000;
const CLIP_BEATS = 12.0;

daw.appLoadSongJson(
  JSON.stringify({
    bpm: 120.0,
    time_sig: [4, 4],
    length_beats: 16.0,
    tracks: [
      {
        id: 1,
        name: "Audio",
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        clips: [{ id: 1, name: "tone", start_beat: 0.0, length_beats: CLIP_BEATS, content_id: 1 }],
        next_clip_id: 2,
      },
    ],
    next_track_id: 2,
    clip_contents: {
      "1": {
        events: [
          {
            source_id: 1,
            event_start_in_clip_beats: 0.0,
            event_length_beats: CLIP_BEATS,
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
            gain_db: 0.0,
            pan: 0.0,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            stretch_mode: "Raw",
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: "Linear",
            fade_out_curve: "Linear",
            reversed: false,
            muted: false,
          },
        ],
      },
    },
    next_content_id: 2,
    audio_sources: {
      "1": { path: { Absolute: wavPath }, sample_rate: 48000, channels: 1, frames: SOURCE_FRAMES },
    },
    next_audio_source_id: 2,
  }),
);
daw.sleepMs(300);

chordEvent({ Chord: { Place: { start: 0, len: 4, chord: chord("C") } } });
chordEvent({ Chord: { Place: { start: 4, len: 4, chord: chord("F") } } });
chordEvent({ Chord: { Place: { start: 8, len: 4, chord: chord("A") } } });
chordEvent({ ChordFollow: { SetTracks: { track_ids: [1], mode: "Parallel" } } });
daw.sleepMs(300);
daw.exportWavRange(outDir + "/follow_none.wav", 0.0, CLIP_BEATS, 120000);

function setAudioChord(text, chordal) {
  const span = { start_frame: 0, end_frame: SOURCE_FRAMES, chord: chord(text), chordal: chordal };
  chordEvent({ ChordTool: { SetAudioChords: { source_id: 1, spans: [span] } } });
  daw.sleepMs(300);
}
setAudioChord("Am", true);
daw.exportWavRange(outDir + "/follow_am.wav", 0.0, CLIP_BEATS, 120000);

setAudioChord("Am", false);
chordEvent({ Chord: { Transpose: { ids: [], semitones: 2 } } });
daw.sleepMs(300);
daw.exportWavRange(outDir + "/follow_rewritten.wav", 0.0, CLIP_BEATS, 120000);

chordEvent({ ChordFollow: { SetEnabled: false } });
daw.sleepMs(300);
daw.exportWavRange(outDir + "/follow_off.wav", 0.0, CLIP_BEATS, 120000);

daw.log("chord_follow_audio: ok");
