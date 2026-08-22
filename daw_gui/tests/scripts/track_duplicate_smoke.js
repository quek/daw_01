// r.md #30: トラック複製 (独立 / リンク) の headless smoke test。
// `daw_gui --script` で AppData を直接駆動し、 production の右クリック「複製」/
// D・Alt+D と同じ AppEvent 経路 (`daw.duplicateTracks`) を通して結果を検証する。
//
// 検証:
//   A. group + 子 2 本 + 単独 track の Song で group を **独立複製** → 元 subtree の
//      直下に新 group + 新子 2 本が挿入され、 子は複製後の group を親に持ち、 単独
//      track は末尾へ押し下がる。 クリップ中身は独立 (新 content_id)。
//   B. clip 付き 1 track を **リンク複製** → クリップ中身が元と content_id 共有。

function expectEq(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(label + ": expected " + JSON.stringify(expected) + " got " + JSON.stringify(actual));
  }
}
function expectNe(actual, forbidden, label) {
  if (actual === forbidden) {
    throw new Error(label + ": expected != " + JSON.stringify(forbidden) + " but got it");
  }
}

// 共通: audio clip 1 個ぶんの content + source (split_glue_smoke.js と同形)。
const audioContent = {
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
};
const audioSources = {
  "1": { path: { Generated: { id: 1 } }, sample_rate: 48000, channels: 1, frames: 96000 },
};

// ===== Scenario A: group を独立複製 =======================================
const songA = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 16.0,
  tracks: [
    { id: 1, name: "Grp", volume: 1.0, pan: 0.0 }, // group (parent 省略 = None)
    {
      id: 2,
      name: "C1",
      volume: 1.0,
      pan: 0.0,
      parent_group_id: 1,
      clips: [{ id: 1, name: "clip1", start_beat: 0.0, length_beats: 4.0, content_id: 1 }],
      next_clip_id: 2,
    },
    { id: 3, name: "C2", volume: 1.0, pan: 0.0, parent_group_id: 1 },
    { id: 4, name: "Solo", volume: 1.0, pan: 0.0 },
  ],
  next_track_id: 5,
  clip_contents: { "1": audioContent },
  next_content_id: 2,
  audio_sources: audioSources,
  next_audio_source_id: 2,
};
daw.appLoadSongJson(JSON.stringify(songA));

let s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks.length, 4, "A initial tracks");

// group (id=1) を独立複製 (linked=false)。
daw.duplicateTracks(JSON.stringify([1]), false);

s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks.length, 7, "A tracks after duplicate");
// 元 subtree は不変。
expectEq(s.tracks[0].id, 1, "A src group id");
expectEq(s.tracks[1].id, 2, "A src child1 id");
expectEq(s.tracks[2].id, 3, "A src child2 id");
// dup block は元 group の直下 (index 3..6)。
const dupGroup = s.tracks[3];
const dupC1 = s.tracks[4];
const dupC2 = s.tracks[5];
expectNe(dupGroup.id, 1, "A dup group has new id");
// 複製後の group は top-level (parent_group_id は None → JSON で省略 = undefined)。
expectEq(dupGroup.parent_group_id, undefined, "A dup group is top-level");
// 子は複製後の group を親に持つ (元 group=1 ではない)。
expectEq(dupC1.parent_group_id, dupGroup.id, "A dup child1 reparented to new group");
expectEq(dupC2.parent_group_id, dupGroup.id, "A dup child2 reparented to new group");
// 独立複製なので clip の content は新採番 (元 content_id=1 と別)。
expectNe(dupC1.clips[0].content_id, 1, "A dup child1 clip content independent");
// 単独 track は末尾へ押し下がる。
expectEq(s.tracks[6].id, 4, "A solo pushed to end");
// 新 content が clip_contents に追加されている。
if (s.clip_contents[String(dupC1.clips[0].content_id)] === undefined) {
  throw new Error("A dup content id not present in clip_contents");
}

// ===== Scenario B: track をリンク複製 → content 共有 =====================
const songB = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 16.0,
  tracks: [
    {
      id: 1,
      name: "T",
      volume: 1.0,
      pan: 0.0,
      clips: [{ id: 1, name: "clip1", start_beat: 0.0, length_beats: 4.0, content_id: 1 }],
      next_clip_id: 2,
    },
  ],
  next_track_id: 2,
  clip_contents: { "1": audioContent },
  next_content_id: 2,
  audio_sources: audioSources,
  next_audio_source_id: 2,
};
daw.appLoadSongJson(JSON.stringify(songB));

s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks.length, 1, "B initial tracks");

// リンク複製 (linked=true) → クリップ中身は元と content_id 共有。
daw.duplicateTracks(JSON.stringify([1]), true);

s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks.length, 2, "B tracks after duplicate");
expectNe(s.tracks[1].id, 1, "B dup track has new id");
expectEq(s.tracks[1].clips[0].content_id, 1, "B linked dup shares content_id");

// すべての assert に通れば exit 0 (= test pass)
