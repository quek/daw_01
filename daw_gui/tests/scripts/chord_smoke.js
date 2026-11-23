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

// トラック 1 = 空 (ノート化の行き先)、2 = メロディー (E F G A の 4 分音符)、3 = ピアノ (C の 3 和音)。どちらも拍 0-4。
const note = (id, start, len, pitch) => ({ id: id, start_beat: start, duration_beats: len, pitch: pitch, velocity: 100 });
const clipOf = (id, content) => ({ id: id, start_beat: 0.0, length_beats: 4.0, content_id: content });
const trackOf = (id, name, clips) => ({ id: id, name: name, volume: 1.0, pan: 0.0, muted: false, solo: false, clips: clips, next_clip_id: 10 });
daw.appLoadSongJson(
  JSON.stringify({
    bpm: 120.0,
    time_sig: [4, 4],
    length_beats: 32.0,
    tracks: [trackOf(1, "MIDI", []), trackOf(2, "Melody", [clipOf(1, 1)]), trackOf(3, "Piano", [clipOf(2, 2)])],
    next_track_id: 4,
    clip_contents: {
      "1": { type: "Midi", notes: [note(1, 0, 1, 76), note(2, 1, 1, 77), note(3, 2, 1, 79), note(4, 3, 1, 81)], next_note_id: 5 },
      "2": { type: "Midi", notes: [note(1, 0, 4, 60), note(2, 0, 4, 64), note(3, 0, 4, 67)], next_note_id: 4 },
    },
    next_content_id: 3,
  }),
);
const melodyKeys = () => [1, 2, 3, 4].map((id) => daw.soundingKey(2, 1, id));
const pianoKeys = () => [1, 2, 3].map((id) => daw.soundingKey(3, 2, id));

// ---- 箱を置く / 上書き規則 ------------------------------------------------
expectEq(daw.parseChord("???"), null, "読めないコード名は null");
chordEvent({ Chord: { Place: { start: 0, len: 4, chord: chord("C") } } });
chordEvent({ Chord: { Place: { start: 4, len: 4, chord: chord("Am7") } } });
chordEvent({ Chord: { Place: { start: 8, len: 8, chord: chord("G7/B") } } });
expectEq(names(), [[0, 4, "C"], [4, 4, "Am7"], [8, 8, "G7/B"]], "置いた箱");
// 後から置いた箱が勝つ (Am7 の後半と G7/B の頭を削る)
chordEvent({ Chord: { Place: { start: 6, len: 4, chord: chord("Fmaj7") } } });
expectEq(names(), [[0, 4, "C"], [4, 2, "Am7"], [6, 4, "Fmaj7"], [10, 6, "G7/B"]], "上書き規則");

// ---- 追従の評価 (r.md #159: 置いた時点のコードがメロディーの基準) -------------
// 既定は Off: 書いた音のまま
expectEq(pianoKeys(), [60, 64, 67], "Off は書いた音");
chordEvent({ ChordFollow: { SetTracks: { track_ids: [2, 3], mode: "Parallel" } } });
expectEq(JSON.parse(daw.inspectSongJson()).tracks[1].follow_chords, "Parallel", "トラックのモード");
// 拍 0-4 は C の箱: ピアノの和音 (C) は収まっているので動かない。メロディーも置いただけでは動かない。
expectEq(pianoKeys(), [60, 64, 67], "和音が箱のコードに収まっていれば動かない");
expectEq(melodyKeys(), [76, 77, 79, 81], "置いただけではメロディーは動かない");
// C の箱を Dm に書き換える: ピアノは自分の和音 C → Dm、メロディーは書き換える前の C → Dm の差 (+2)。
const firstId = JSON.parse(daw.chordsJson())[0].id;
chordEvent({ Chord: { SetChord: { ids: [firstId], chord: chord("Dm") } } });
expectEq(pianoKeys(), [62, 66, 69], "ピアノは Dm へ");
expectEq(melodyKeys(), [78, 79, 81, 83], "書き換えるとメロディーは C → Dm の差だけ");
// 最寄りのコード音: ピアノは Dm の構成音へ、メロディーは元の C の構成音 (E / G) だけを寄せる。
chordEvent({ ChordFollow: { SetTracks: { track_ids: [2, 3], mode: "Narrow" } } });
expectEq(pianoKeys(), [62, 65, 69], "Narrow: ピアノは Dm の構成音へ");
expectEq(melodyKeys(), [77, 77, 81, 81], "Narrow: メロディーは C の構成音だけ寄せる");
// 全体スイッチを切ると追従しない
chordEvent({ ChordFollow: { SetEnabled: false } });
expectEq(melodyKeys(), [76, 77, 79, 81], "全体 Off は書いた音");
chordEvent({ ChordFollow: { SetEnabled: true } });
chordEvent({ ChordFollow: { SetTracks: { track_ids: [2, 3], mode: "Off" } } });
chordEvent({ Chord: { SetChord: { ids: [firstId], chord: chord("C") } } });

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
// 「コードを抽出…」の「抽出」が撃つイベント (長さは自動 = 既定): 窓を指定しなくても、長さの違う箱 (4 / 2 / 4 / 6 拍) に戻る。
chordEvent({ Chord: "Clear" });
expectEq(JSON.parse(daw.chordToolsJson()).view.detect.auto_length, true, "1 コードの長さは既定で自動");
chordEvent({ ChordTool: { ExtractChords: { clips: [[1, clip.id]] } } });
expectEq(names(), before, "長さを指定せずに抽出しても元の進行に戻る");

// ---- 移動 / 移調 ----------------------------------------------------------
const ids = JSON.parse(daw.chordsJson()).map((c) => c.id);
chordEvent({ Chord: { Move: { ids: ids, delta: 4 } } });
expectEq(names()[0], [4, 4, "C"], "まとめて移動");
chordEvent({ Chord: { Transpose: { ids: [], semitones: 2 } } });
expectEq(names().map((n) => n[2]), ["D", "Bm7", "Gmaj7", "A7/C#"], "全部を移調");

daw.log("chord_smoke: ok");
