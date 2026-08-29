// clip 表示ラベル導出の AppData-driven JS smoke test。
// `daw_gui --script` で headless 実行。 exit code 0 で pass。
//
// 検証: 歌詞付き MIDI クリップ (Bell トラックの「あかねに」 = 20260512.daw の
// 再現) を rename しても表示が歌詞のまま変わらなかった不具合の回帰テスト。
// clipDisplayLabel は inspectSongJson (= content_name = モデルの明示名) と違い、
// Text 本文 / 歌詞 / 明示名 を導出した後の **画面に出る文字列** を返す。
//
// flow:
//   1. Bell トラック (MIDI clip, 歌詞「あかねに」+ 明示名 "Bell") と
//      Vox トラック (MIDI clip, 歌詞「あかねさくにわ」, 明示名なし) を構築 + load
//   2. 明示名優先: Bell は歌詞でなく "Bell" を表示する (= #69 修正)
//   3. 明示名なし: Vox は歌詞「あかねさくにわ」を表示する
//   4. rename → 表示が即追従する (Bell / Vox 両方)

function expectEq(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(label + ": expected '" + expected + "' got '" + actual + "'");
  }
}

function note(start, lyric) {
  return { start_beat: start, duration_beats: 0.25, pitch: 74, velocity: 100, lyric: lyric };
}

function midiTrack(id, name, clipContentId) {
  return {
    id: id,
    name: name,
    volume: 1.0,
    pan: 0.0,
    muted: false,
    solo: false,
    clips: [
      { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: clipContentId },
    ],
    next_clip_id: 2,
  };
}

const song = {
  bpm: 140.0,
  time_sig: [4, 4],
  length_beats: 16.0,
  tracks: [
    midiTrack(1, "Bell", 4), // content 4: 歌詞「あかねに」+ 明示名 "Bell"
    midiTrack(2, "Vox", 5),  // content 5: 歌詞「あかねさくにわ」, 明示名なし
  ],
  next_track_id: 3,
  clip_contents: {
    "4": { notes: [note(1.0, "あ"), note(1.25, "か"), note(1.5, "ね"), note(2.0, "に")] },
    "5": {
      notes: [
        note(0.0, "あ"), note(0.5, "か"), note(1.0, "ね"), note(1.5, "さ"),
        note(2.0, "く"), note(3.0, "に"), note(3.5, "わ"),
      ],
    },
  },
  // Bell (content 4) だけ明示名を持つ。 Vox (content 5) は名前なし。
  clip_content_names: { "4": "Bell" },
  next_content_id: 6,
};
daw.appLoadSongJson(JSON.stringify(song));

const bell = JSON.stringify({ track_id: 1, clip_id: 1 });
const vox = JSON.stringify({ track_id: 2, clip_id: 1 });

// ---- 2. 明示名優先: Bell は歌詞「あかねに」でなく明示名 "Bell" を表示 (#69) ----
expectEq(daw.clipDisplayLabel(bell), "Bell", "Bell: explicit name beats lyric");

// ---- 3. 明示名なし: Vox は歌詞を連結して表示 -----------------------------
expectEq(daw.clipDisplayLabel(vox), "あかねさくにわ", "Vox: lyric when unnamed");

// ---- 4. rename → 表示が即追従 (歌詞付きクリップでも名前が出る) -----------
daw.dispatchRenameClip(bell, "ベル");
expectEq(daw.clipDisplayLabel(bell), "ベル", "Bell: rename updates display");

// 名前のなかった歌詞クリップに名前を付けると、 以後その名前が出る。
daw.dispatchRenameClip(vox, "サビ");
expectEq(daw.clipDisplayLabel(vox), "サビ", "Vox: naming a lyric clip shows the name");

// すべての assert に通れば exit 0 (= test pass)
