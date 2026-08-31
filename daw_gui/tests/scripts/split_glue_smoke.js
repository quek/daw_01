// Phase 1 PR7 follow-up: Split / Glue を AppData 直接駆動で smoke test。
// `daw_gui --script` で headless 実行。 exit code 0 で pass。
//
// flow:
//   1. 1 track + 1 audio clip (length 4 拍) の Song を構築
//   2. hover を clip の中央 (beat 2.0) に置く
//   3. dispatchSplit(true) → clip が 2 つに分かれる
//   4. setSelection で両方選択
//   5. dispatchGlue() → 1 つに戻る

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

// ---- 2. 初期状態確認 ----------------------------------------------------
let s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks.length, 1, "initial tracks");
expectEq(s.tracks[0].clips.length, 1, "initial clips");

// ---- 3. hover を clip の中央 (beat 2.0) に置いて split ------------------
daw.setHoverClip(JSON.stringify({ track_id: 1, clip_id: 1 }));
daw.setHoverBeat(2.0);
daw.dispatchSplit(false); // snap off (= raw beat = 2.0 そのまま)

s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips.length, 2, "after split clips count");

// 各 clip の長さを確認 (前半 0..2 / 後半 2..4 → 各 length_beats=2.0)
const c0 = s.tracks[0].clips[0];
const c1 = s.tracks[0].clips[1];
expectEq(c0.length_beats, 2.0, "front clip length");
expectEq(c1.start_beat, 2.0, "back clip start");
expectEq(c1.length_beats, 2.0, "back clip length");

// ---- 4. 両方選択して glue → 1 つに戻る ---------------------------------
// hover を外しておかないと glue 後の selection が hover に上書きされる可能性
daw.setHoverClip("null");
daw.setHoverBeat(null);
daw.setSelection(
  JSON.stringify([
    { track_id: 1, clip_id: 1 },
    { track_id: 1, clip_id: 2 },
  ]),
);
// audio の Glue は **焼き込み** (offline render) なので非同期。
// 完了 (= 1 クリップに戻る) まで IPC を汲みながら待つ。
daw.dispatchGlue();

let waited = 0;
while (waited < 30000) {
  s = JSON.parse(daw.inspectSongJson());
  if (s.tracks[0].clips.length === 1) {
    break;
  }
  daw.sleepMs(200);
  waited += 200;
}
expectEq(s.tracks[0].clips.length, 1, "after glue clips count");
const merged = s.tracks[0].clips[0];
expectEq(merged.start_beat, 0.0, "merged clip start");
expectEq(merged.length_beats, 4.0, "merged clip length");

// 焼き込みなので中身は **1 event ちょうど** (= 継ぎ目のフェードハンドルが残らない、
// docs/plan_glue_bake.md)。 旧実装は元クリップの event を並べるだけで 2 つ残っていた。
const mergedContent = s.clip_contents[String(merged.content_id)];
expectEq(mergedContent.events.length, 1, "merged content event count");
expectEq(mergedContent.events[0].event_start_in_clip_beats, 0.0, "baked event start");
expectEq(mergedContent.events[0].event_length_beats, 4.0, "baked event length");

// ---- 5. 異 kind 混在 Glue が reject されることを確認 (補足) ------------
// ここでは省略 (= MIDI clip 構築が必要、 今後 PR で追加)。

// すべての assert に通れば exit 0 (= test pass)
