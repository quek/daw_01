// clip rename smoke: dispatchRenameClip の commit ロジックを検証。
// `daw_gui --script` で headless 実行。 exit code 0 で pass。
//
// flow:
//   1. 1 track + 1 audio clip (name "clip1") の Song を構築 + load
//   2. rename → overwrite → 前後空白 trim → 空白のみ no-op → 空文字 no-op
//   を inspectSongJson で逐次 assert

function expectEq(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(label + ": expected " + expected + " got " + actual);
  }
}

// ---- 1. Song を JSON で構築 + load --------------------------------------
const song = {
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
      clips: [
        {
          id: 1,
          name: "clip1",
          start_beat: 0.0,
          length_beats: 4.0,
          content_id: 1,
        },
      ],
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
          event_length_beats: 4.0,
          source_start_frames: 0,
          source_end_frames: 96000,
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
    "1": {
      path: { Generated: { id: 1 } },
      sample_rate: 48000,
      channels: 1,
      frames: 96000,
    },
  },
  next_audio_source_id: 2,
};
daw.appLoadSongJson(JSON.stringify(song));

// ---- 2. 初期名確認 ------------------------------------------------------
let s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips[0].name, "clip1", "initial name");

// ClipRef は index ベース (track 0 / clip 0)。
const ref = JSON.stringify({ track: 0, clip: 0 });

// ---- 3. rename ----------------------------------------------------------
daw.dispatchRenameClip(ref, "Verse A");
s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips[0].name, "Verse A", "after rename");

// ---- 4. overwrite -------------------------------------------------------
daw.dispatchRenameClip(ref, "Chorus");
s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips[0].name, "Chorus", "after overwrite");

// ---- 5. 前後空白は trim される ------------------------------------------
daw.dispatchRenameClip(ref, "  Bridge  ");
s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips[0].name, "Bridge", "trimmed name");

// ---- 6. 空白のみ → trim 後空 → no-op (前の名前を維持) ------------------
daw.dispatchRenameClip(ref, "   ");
s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips[0].name, "Bridge", "whitespace-only no-op");

// ---- 7. 空文字 → no-op --------------------------------------------------
daw.dispatchRenameClip(ref, "");
s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips[0].name, "Bridge", "empty no-op");

// すべての assert に通れば exit 0 (= test pass)
