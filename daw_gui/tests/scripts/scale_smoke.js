// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

// Phase 7 B5 (`docs/plan_scale.html`): Scale & Root の AppData-driven JS smoke test。
// `daw_gui --script` で headless 実行。 exit code 0 で pass。
//
// 検証する path:
//   1. setScaleAtPlayhead / clearScaleChanges で `Song.scale_changes` 編集
//   2. snap on draw OFF (default) では addNote の pitch が raw のまま
//   3. snap on draw ON + scale 設定済で addNote の pitch が in-scale に snap
//   4. setNotePositionsJson も同様に snap apply (note y-drag 経路)
//   5. quantizePitchesToScale で既存 out-of-scale note が in-scale に補正
//   6. scale_changes が空のときは snap が no-op (= 機能 OFF 互換)

function expectEq(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(label + ": expected " + expected + " got " + actual);
  }
}

function pitchClassInMajor(pitch) {
  // C Major bits {0, 2, 4, 5, 7, 9, 11} = 0b1010_1011_0101 = 2741
  const d = ((pitch % 12) + 12) % 12;
  return ((2741 >> d) & 1) === 1;
}

// ---- 1. Song を JSON で構築 + load (1 MIDI track + 1 MIDI clip 8 拍) ----
const song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 16.0,
  tracks: [
    {
      id: 1,
      name: "MIDI",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [
        {
          id: 1,
          name: "clip1",
          start_beat: 0.0,
          length_beats: 8.0,
          content_id: 1,
        },
      ],
      next_clip_id: 2,
    },
  ],
  next_track_id: 2,
  clip_contents: {
    // ClipContent::Midi(MidiContent { notes: [] }) は serde untagged で
    // `{ notes: [] }` として decode される (audio events / automation points
    // と field 名が disjoint)。
    "1": { notes: [] },
  },
  next_content_id: 2,
};
daw.appLoadSongJson(JSON.stringify(song));

// selected_clip を必須にする (= setNotePositions / quantize の前提)
daw.setSelection(JSON.stringify([{ track: 0, clip: 0 }]));

// ---- 2. scale 設定: C Major ---------------------------------------------
daw.setScaleAtPlayhead(0, "Major");
let s = JSON.parse(daw.inspectSongJson());
expectEq(s.scale_changes.length, 1, "scale_changes after set");
expectEq(s.scale_changes[0].beat, 0.0, "scale_changes[0].beat");
expectEq(s.scale_changes[0].root, 0, "scale_changes[0].root");
expectEq(s.scale_changes[0].scale, "Major", "scale_changes[0].scale");

// ---- 3. snap OFF 状態 (default) で out-of-scale pitch 追加 → raw のまま -
daw.addNote(0, 0, 0.0, 1.0, 61); // C# (C Major で out)
s = JSON.parse(daw.inspectSongJson());
let notes = s.clip_contents["1"].notes;
expectEq(notes.length, 1, "after first addNote: count");
expectEq(notes[0].pitch, 61, "snap OFF: pitch unchanged (61=C#)");

// ---- 4. Snap on Draw ON で out-of-scale 入力 → in-scale snap ------------
daw.toggleSnapOnDraw();
daw.addNote(0, 0, 1.0, 1.0, 61); // C# → 期待 D (62、 同距離 up 優先)
s = JSON.parse(daw.inspectSongJson());
notes = s.clip_contents["1"].notes;
expectEq(notes.length, 2, "after second addNote: count");
expectEq(notes[1].pitch, 62, "snap ON: 61 (C#) → 62 (D, up 優先)");

// ---- 5. setNotePositionsJson も snap apply (note y-drag 経路) -----------
// 1 番目の note (pitch 61) を beat 2.0 / pitch 66 (F#) に移動 → snap 期待 65 (F、 同距離 down 後に up が hit するため 67=G)
// C Major では F (65), G (67) が in-scale。 F# (66) からは F (65) が -1、 G (67) が +1、 同距離なら up = G (67)。
daw.setNotePositionsJson(JSON.stringify([[0, 2.0, 66]]));
s = JSON.parse(daw.inspectSongJson());
notes = s.clip_contents["1"].notes;
expectEq(notes[0].start_beat, 2.0, "setNotePositions start_beat");
expectEq(notes[0].pitch, 67, "snap ON: 66 (F#) → 67 (G, up 優先)");

// ---- 6. quantize: out-of-scale note を一括補正 --------------------------
// snap OFF に戻して out-of-scale note を直接追加 (= snap がかかってない note)
daw.toggleSnapOnDraw(); // OFF
daw.addNote(0, 0, 3.0, 1.0, 68); // G# (out)
daw.addNote(0, 0, 4.0, 1.0, 70); // A# (out)
s = JSON.parse(daw.inspectSongJson());
notes = s.clip_contents["1"].notes;
expectEq(notes.length, 4, "after out-of-scale adds");
expectEq(notes[2].pitch, 68, "before quantize: 68 (G#)");
expectEq(notes[3].pitch, 70, "before quantize: 70 (A#)");

daw.quantizePitchesToScale("selected_clip_all_notes");
s = JSON.parse(daw.inspectSongJson());
notes = s.clip_contents["1"].notes;
for (let i = 0; i < notes.length; i++) {
  if (!pitchClassInMajor(notes[i].pitch)) {
    throw new Error(
      "note[" + i + "] pitch " + notes[i].pitch + " is out of C Major after quantize",
    );
  }
}

// ---- 7. clearScaleChanges で機能 OFF ------------------------------------
daw.clearScaleChanges();
s = JSON.parse(daw.inspectSongJson());
expectEq(s.scale_changes.length, 0, "scale_changes cleared");

// ---- 8. scale 空 + Snap on Draw ON → snap が no-op (raw pitch) ----------
daw.toggleSnapOnDraw(); // 再 ON
daw.addNote(0, 0, 6.0, 1.0, 61); // C#、 scale 空なので snap が unwrap_or で raw
s = JSON.parse(daw.inspectSongJson());
notes = s.clip_contents["1"].notes;
const last = notes[notes.length - 1];
expectEq(last.pitch, 61, "no scale: snap_on_draw ON でも raw pitch (61=C#)");

// ---- 9. setScaleAtPlayhead で再設定: D Minor ----------------------------
daw.setScaleAtPlayhead(2, "Minor"); // D Minor = D NaturalMinor
s = JSON.parse(daw.inspectSongJson());
expectEq(s.scale_changes.length, 1, "scale_changes after re-set");
expectEq(s.scale_changes[0].root, 2, "D Minor root");
expectEq(s.scale_changes[0].scale, "NaturalMinor", "D Minor scale enum");

// ---- 10. toggleSnapLiveInput / toggleFoldToScale も発火確認 -------------
// state を直接 inspect する API は無いので、 2 回 toggle して idempotent
// 確認のみ (= panic しないこと)
daw.toggleSnapLiveInput();
daw.toggleSnapLiveInput();
daw.toggleFoldToScale();
daw.toggleFoldToScale();

// すべての assert に通れば exit 0 (= test pass)
