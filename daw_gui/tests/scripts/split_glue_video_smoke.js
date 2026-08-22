// docs/plan_video.md P6.4: Video clip の Split / Glue smoke test。
// 既存 split_glue_smoke.js の audio 版を踏襲、 ClipContent::Video +
// VideoEvent + Song.video_sources で同等の flow を流す。
//
// flow:
//   1. 1 track (kind: Video) + 1 video clip (length 4 拍、 source 4_000_000μs)
//      の Song を構築
//   2. hover を clip 中央 (beat 2.0) に置く
//   3. dispatchSplit(false) → clip が 2 つに分かれ、 中央で source_micros
//      が 50/50 (= 2_000_000μs) に partition される
//   4. setSelection で両方選択
//   5. dispatchGlue() → 1 つに戻る (events は 2 つ残るが range は同等)

function expectEq(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(label + ": expected " + expected + " got " + actual);
  }
}

function expectNear(actual, expected, eps, label) {
  if (Math.abs(actual - expected) > eps) {
    throw new Error(label + ": expected ~" + expected + " got " + actual);
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
      kind: "Video",
      name: "Video",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [
        {
          id: 1,
          name: "vclip",
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
          source_start_micros: 0,
          source_end_micros: 4000000,
          muted: false,
          fade_in_beats: 0.0,
          fade_out_beats: 0.0,
          fade_in_curve: "Linear",
          fade_out_curve: "Linear",
        },
      ],
    },
  },
  next_content_id: 2,
  video_sources: {
    "1": {
      path: { Absolute: "/dev/null" },
      width: 320,
      height: 240,
      framerate: 30.0,
      duration_micros: 4000000,
      codec: "h264",
    },
  },
  next_video_source_id: 2,
  video_resolution: [1920, 1080],
  video_framerate: 30.0,
};
daw.appLoadSongJson(JSON.stringify(song));

// ---- 2. 初期状態確認 ----------------------------------------------------
let s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks.length, 1, "initial tracks");
expectEq(s.tracks[0].clips.length, 1, "initial clips");

// ---- 3. hover を clip 中央 (beat 2.0) に置いて split --------------------
daw.setHoverClip(JSON.stringify({ track: 0, clip: 0 }));
daw.setHoverBeat(2.0);
daw.dispatchSplit(false);

s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips.length, 2, "after split clips count");

// 各 clip の長さを確認 (前半 0..2 / 後半 2..4 → 各 length_beats=2.0)
const c0 = s.tracks[0].clips[0];
const c1 = s.tracks[0].clips[1];
expectEq(c0.length_beats, 2.0, "front clip length");
expectEq(c1.start_beat, 2.0, "back clip start");
expectEq(c1.length_beats, 2.0, "back clip length");

// VideoEvent の source_micros が比例分割されたことを確認:
//   front : source [0, 2_000_000)
//   back  : source [2_000_000, 4_000_000)
const c0_content = s.clip_contents[c0.content_id];
const c1_content = s.clip_contents[c1.content_id];
const fEv = c0_content.events[0];
const bEv = c1_content.events[0];
expectNear(fEv.source_start_micros, 0, 1, "front source_start");
expectNear(fEv.source_end_micros, 2000000, 1, "front source_end");
expectNear(bEv.source_start_micros, 2000000, 1, "back source_start");
expectNear(bEv.source_end_micros, 4000000, 1, "back source_end");

// ---- 4. 両方選択して glue → 1 つに戻る ---------------------------------
daw.setSelection(JSON.stringify([
  { track: 0, clip: 0 },
  { track: 0, clip: 1 },
]));
daw.dispatchGlue();

s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips.length, 1, "after glue clips count");
const merged = s.tracks[0].clips[0];
expectEq(merged.start_beat, 0.0, "merged start");
expectEq(merged.length_beats, 4.0, "merged length");
const mergedEvents = s.clip_contents[merged.content_id].events;
// Split が events を 2 つに分けたあと Glue で結合 = 2 events に戻る
// (offset_into_combined で event_start_in_clip_beats を 0 と 2 に
// 振り直し)。 source_start_micros / source_end_micros は維持されるので、
// 全体としては元の 1 event をカバーする 2 event。
expectEq(mergedEvents.length, 2, "merged events count");
expectNear(mergedEvents[0].event_start_in_clip_beats, 0.0, 1e-9, "ev0 start");
expectNear(mergedEvents[1].event_start_in_clip_beats, 2.0, 1e-9, "ev1 start");
expectNear(mergedEvents[0].source_start_micros, 0, 1, "ev0 src_start");
expectNear(mergedEvents[1].source_end_micros, 4000000, 1, "ev1 src_end");
