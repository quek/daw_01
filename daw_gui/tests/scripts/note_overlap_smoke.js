// MIDI ノート非重なりの AppData-driven JS smoke test。
// `daw_gui --script` で headless 実行。exit code 0 で pass。
//
// 純ロジック (resolve_note_overlaps) は app.rs の単体テストで検証済。ここでは
// AppEvent::AddNote / SetNotePositions ハンドラが実際に解消を呼び出している
// (= 配線) ことを end-to-end で確認する。
//
// 検証:
//   1. 同一ピッチで末尾に重なる note を描く → 既存をトリム (last-note-wins)
//   2. 異なるピッチは重なってよい (= 和音、解消されない)
//   3. note を移動して同一ピッチに重ねる → 既存をトリム
//   4. 最終的にどのピッチでも同一ピッチ note が重ならない (不変条件)

function approxEq(a, b) {
  return Math.abs(a - b) < 1e-6;
}

function fail(msg) {
  throw new Error(msg);
}

// content 10 (track0/clip0) の notes を読み戻して [start,dur,pitch] 配列で返す。
function clipNotes() {
  const song = JSON.parse(daw.inspectSongJson());
  const content = song.clip_contents["10"];
  if (!content || !content.notes) fail("content 10 not found");
  return content.notes.map(function (n) {
    return { s: n.start_beat, d: n.duration_beats, p: n.pitch };
  });
}

// 同一ピッチ note が時間的に重なっていないことを検証 (不変条件)。
function assertNoOverlap(notes) {
  for (let i = 0; i < notes.length; i++) {
    for (let j = i + 1; j < notes.length; j++) {
      if (notes[i].p !== notes[j].p) continue;
      const a = notes[i], b = notes[j];
      const overlap = a.s < b.s + b.d - 1e-9 && b.s < a.s + a.d - 1e-9;
      if (overlap) {
        fail(
          "overlap at pitch " + a.p + ": [" + a.s + "," + (a.s + a.d) + ") vs [" +
          b.s + "," + (b.s + b.d) + ")"
        );
      }
    }
  }
}

// 指定 pitch の note を start 昇順で取り出す。
function byPitch(notes, pitch) {
  return notes
    .filter(function (n) { return n.p === pitch; })
    .sort(function (a, b) { return a.s - b.s; });
}

const song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 32.0,
  tracks: [
    {
      id: 1,
      name: "Inst",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [{ id: 1, start_beat: 0.0, length_beats: 32.0, content_id: 10 }],
      next_clip_id: 2,
    },
  ],
  next_track_id: 2,
  clip_contents: { "10": { notes: [] } },
  next_content_id: 11,
};
daw.appLoadSongJson(JSON.stringify(song));
daw.setSelection(JSON.stringify([{ track: 0, clip: 0 }]));

// ---- 1. 末尾重なり: A[0,4) p60、B[2,4) p60 を描く → A を [0,2) にトリム ----
daw.addNote(0, 0, 0.0, 4.0, 60); // A
daw.addNote(0, 0, 2.0, 4.0, 60); // B (A の末尾に重なる)
{
  const p60 = byPitch(clipNotes(), 60);
  if (p60.length !== 2) fail("step1: expected 2 notes at p60, got " + p60.length);
  if (!approxEq(p60[0].s, 0.0) || !approxEq(p60[0].d, 2.0))
    fail("step1: A should be trimmed to [0,2), got [" + p60[0].s + "," + (p60[0].s + p60[0].d) + ")");
  if (!approxEq(p60[1].s, 2.0) || !approxEq(p60[1].d, 4.0))
    fail("step1: B should be [2,6), got [" + p60[1].s + "," + (p60[1].s + p60[1].d) + ")");
}

// ---- 2. 異なるピッチは重なってよい (和音): C[1,2) p62 を描く → 解消されない ----
daw.addNote(0, 0, 1.0, 2.0, 62); // C (時間的に A/B と重なるが別ピッチ)
{
  const notes = clipNotes();
  assertNoOverlap(notes);
  const p62 = byPitch(notes, 62);
  if (p62.length !== 1) fail("step2: expected 1 note at p62, got " + p62.length);
  if (!approxEq(p62[0].s, 1.0) || !approxEq(p62[0].d, 2.0))
    fail("step2: C[1,3) p62 should be untouched");
  // p60 は step1 のまま (別ピッチの描画に影響されない)。
  const p60 = byPitch(notes, 60);
  if (p60.length !== 2) fail("step2: p60 count changed unexpectedly: " + p60.length);
}

// ---- 3. 移動で同一ピッチに重ねる: C(idx2, p62) を start3 / p60 へ移動 ----
//   move 後 C=[3,5) p60、既存 B=[2,6) p60 を [2,3) にトリム。
//   note index は clip_contents の Vec 順 = [A(0), B(1), C(2)]。
daw.setNotePositionsJson(JSON.stringify([[2, 3.0, 60]]));
{
  const notes = clipNotes();
  assertNoOverlap(notes);
  const p60 = byPitch(notes, 60);
  if (p60.length !== 3) fail("step3: expected 3 notes at p60, got " + p60.length);
  // A[0,2), B[2,3), C[3,5)
  if (!approxEq(p60[0].s, 0.0) || !approxEq(p60[0].d, 2.0)) fail("step3: A wrong");
  if (!approxEq(p60[1].s, 2.0) || !approxEq(p60[1].d, 1.0))
    fail("step3: B should be trimmed to [2,3), got [" + p60[1].s + "," + (p60[1].s + p60[1].d) + ")");
  if (!approxEq(p60[2].s, 3.0) || !approxEq(p60[2].d, 2.0))
    fail("step3: C should be [3,5), got [" + p60[2].s + "," + (p60[2].s + p60[2].d) + ")");
  // p62 はもう存在しない (C が p60 へ移動した)。
  if (byPitch(notes, 62).length !== 0) fail("step3: p62 should be empty after move");
}

// すべての assert に通れば exit 0 (= test pass)
