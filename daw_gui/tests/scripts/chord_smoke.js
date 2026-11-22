// r.md #157 (`docs/plan_chord_track.md`): コードトラックの土台の通し確認。
// `daw_gui --script` で headless 実行。exit code 0 で pass。
//
// 通す経路: script → AppEvent::Chord / ChordFollow / ChordTool → handler → edit_song → Song、
// と追従の評価 (`common::chord_follow`)。

function expectEq(actual, expected, label) {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    throw new Error(label + ": expected " + JSON.stringify(expected) + " got " + JSON.stringify(actual));
  }
}
function chordEvent(ev) {
  daw.chordEvent(JSON.stringify(ev));
}
function chord(text) {
  const json = daw.parseChord(text);
  if (json === null) throw new Error("parseChord(" + text + ") が読めない");
  return JSON.parse(json);
}
function names() {
  return JSON.parse(daw.chordsJson()).map((c) => [c.start_beat, c.len_beats, c.name]);
}

daw.appLoadSongJson(
  JSON.stringify({
    bpm: 120.0,
    time_sig: [4, 4],
    length_beats: 32.0,
    tracks: [
      { id: 1, name: "MIDI", volume: 1.0, pan: 0.0, muted: false, solo: false, clips: [], next_clip_id: 1 },
    ],
    next_track_id: 2,
  }),
);

// ---- 箱を置く / 上書き規則 ------------------------------------------------
expectEq(daw.parseChord("???"), null, "読めないコード名は null");
chordEvent({ Chord: { Place: { start: 0, len: 4, chord: chord("C") } } });
chordEvent({ Chord: { Place: { start: 4, len: 4, chord: chord("Am7") } } });
chordEvent({ Chord: { Place: { start: 8, len: 8, chord: chord("G7/B") } } });
expectEq(names(), [[0, 4, "C"], [4, 4, "Am7"], [8, 8, "G7/B"]], "置いた箱");
// 後から置いた箱が勝つ (Am7 の後半と G7/B の頭を削る)
chordEvent({ Chord: { Place: { start: 6, len: 4, chord: chord("Fmaj7") } } });
expectEq(names(), [[0, 4, "C"], [4, 2, "Am7"], [6, 4, "Fmaj7"], [10, 6, "G7/B"]], "上書き規則");

// ---- 追従の評価 -----------------------------------------------------------
// 既定は Off: 書いた音のまま
expectEq(daw.followedKey(1, 61, 0), 61, "Off は書いた音");
chordEvent({ ChordFollow: { SetTracks: { track_ids: [1], mode: "Narrow" } } });
expectEq(JSON.parse(daw.inspectSongJson()).tracks[0].follow_chords, "Narrow", "トラックのモード");
// C の上の C# (61) は最寄りのコード音 C (60) へ
expectEq(daw.followedKey(1, 61, 0), 60, "Narrow: C#→C (C の上)");
// Am7 (A C E G) の上の D (62) は同距離 → 上の E (64)
expectEq(daw.followedKey(1, 62, 4), 64, "Narrow: D→E (Am7 の上、同距離は上)");
// 箱の無い位置は追従しない
expectEq(daw.followedKey(1, 61, 20), 61, "箱の無い位置は書いた音");
// 平行移動: 素材のコードが分からなければ C 基準 → Am7 で -3
chordEvent({ ChordFollow: { SetTracks: { track_ids: [1], mode: "Parallel" } } });
expectEq(daw.followedKey(1, 60, 4), 57, "Parallel: C→A (Am7 の上)");
// 全体スイッチを切ると追従しない
chordEvent({ ChordFollow: { SetEnabled: false } });
expectEq(daw.followedKey(1, 60, 4), 60, "全体 Off は書いた音");
chordEvent({ ChordFollow: { SetEnabled: true } });

// ---- コード → ノート → コード ---------------------------------------------
chordEvent({ ChordTool: { ChordsToClip: { chord_ids: [], track_id: 1 } } });
let s = JSON.parse(daw.inspectSongJson());
expectEq(s.tracks[0].clips.length, 1, "ノート化でクリップが 1 つ");
const clip = s.tracks[0].clips[0];
expectEq([clip.start_beat, clip.length_beats], [0, 16], "クリップの範囲 = 箱の範囲");
const before = names();
chordEvent({ Chord: "Clear" });
expectEq(names(), [], "全部削除");
chordEvent({ ChordTool: { ExtractFromClips: { clips: [[1, clip.id]], window_beats: 2 } } });
expectEq(names(), before, "ノートから抽出すると元の進行に戻る");

// ---- 移動 / 移調 ----------------------------------------------------------
const ids = JSON.parse(daw.chordsJson()).map((c) => c.id);
chordEvent({ Chord: { Move: { ids: ids, delta: 4 } } });
expectEq(names()[0], [4, 4, "C"], "まとめて移動");
chordEvent({ Chord: { Transpose: { ids: [], semitones: 2 } } });
expectEq(names().map((n) => n[2]), ["D", "Bm7", "Gmaj7", "A7/C#"], "全部を移調");

daw.log("chord_smoke: ok");
