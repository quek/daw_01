// `J` (Glue) の焼き込みが音を変えないことを測る (docs/plan_glue_bake.md §3)。
// harness: daw_gui/tests/glue_bake_parity.rs (サイン波 WAV を書いて --arg wav= で渡す)。
//
// flow:
//   1. サイン波 WAV を 2 クリップ (0..2 拍 / 2..4 拍) に分けて並べた song を load。
//      トラックのフェーダーは 0.5 (= -6dB)。
//   2. 0..4 拍のラウドネスを測る (結合前)。
//   3. 範囲を選んで J → 焼き込み完了 (1 clip / 1 event) を待つ。
//   4. 同じ範囲をもう一度測り、一致することを確かめる。

function fail(msg) {
  throw new Error("glue_bake_parity: " + msg);
}

function expectEq(actual, expected, label) {
  if (actual !== expected) {
    fail(label + ": expected " + expected + " got " + actual);
  }
}

const wavPath = daw.scriptArgs.wav;
if (!wavPath) fail("--arg wav=<path> が渡っていない");

// 120BPM → 1 拍 = 0.5 秒 = 24000 frames @48k。source は 2 秒 = 96000 frames。
function audioEvent(startBeat, srcStart, srcEnd) {
  return {
    source_id: 1,
    event_start_in_clip_beats: startBeat,
    event_length_beats: 2.0,
    source_start_frames: srcStart,
    source_end_frames: srcEnd,
    gain_db: 0.0,
    pan: 0.0,
    pitch_semitones: 0.0,
    formant_semitones: 0.0,
    // Raw = テープ挙動そのまま (= 時間軸を触らない)。結合前後の比較を素材の音で行う。
    stretch_mode: "Raw",
    fade_in_beats: 0.0,
    fade_out_beats: 0.0,
    fade_in_curve: "Linear",
    fade_out_curve: "Linear",
    reversed: false,
    muted: false,
  };
}

const song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 8.0,
  tracks: [
    {
      id: 1,
      name: "Audio",
      // **フェーダーを下げておく**: 焼き込みがこれを含んでしまうと、再生時に
      // もう一度掛かって -6dB ずれる (= この test が落ちる)。
      volume: 0.5,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [
        { id: 1, name: "front", start_beat: 0.0, length_beats: 2.0, content_id: 1 },
        { id: 2, name: "back", start_beat: 2.0, length_beats: 2.0, content_id: 2 },
      ],
      next_clip_id: 3,
    },
  ],
  next_track_id: 2,
  clip_contents: {
    "1": { events: [audioEvent(0.0, 0, 48000)] },
    "2": { events: [audioEvent(0.0, 48000, 96000)] },
  },
  next_content_id: 3,
  audio_sources: {
    "1": {
      path: { Absolute: wavPath },
      sample_rate: 48000,
      channels: 1,
      frames: 96000,
    },
  },
  next_audio_source_id: 2,
};
daw.appLoadSongJson(JSON.stringify(song));
daw.sleepMs(300);

// ---- 2. 結合前のラウドネス -----------------------------------------------
const before = JSON.parse(daw.analyzeLoudnessJson(0.0, 4.0, 60000));
if (before.integrated_lufs === null) {
  fail("結合前が無音 (WAV が engine に届いていない): " + JSON.stringify(before));
}

// ---- 3. 範囲を選んで Glue (= 焼き込み) ------------------------------------
daw.setHoverClip("null");
daw.setHoverBeat(null);
daw.setSelection(
  JSON.stringify([
    { track_id: 1, clip_id: 1 },
    { track_id: 1, clip_id: 2 },
  ]),
);
daw.dispatchGlue();

let s = null;
let waited = 0;
while (waited < 30000) {
  s = JSON.parse(daw.inspectSongJson());
  if (s.tracks[0].clips.length === 1) break;
  daw.sleepMs(200);
  waited += 200;
}
expectEq(s.tracks[0].clips.length, 1, "after glue clips count");
const merged = s.tracks[0].clips[0];
expectEq(merged.length_beats, 4.0, "merged clip length");
expectEq(
  s.clip_contents[String(merged.content_id)].events.length,
  1,
  "merged content event count",
);
// フェーダーは song 側に残っている (焼き込んで消してはいけない)。
expectEq(s.tracks[0].volume, 0.5, "track fader kept");

// ---- 4. 結合後のラウドネス = 結合前と一致 ---------------------------------
const after = JSON.parse(daw.analyzeLoudnessJson(0.0, 4.0, 60000));
if (after.integrated_lufs === null) {
  fail("結合後が無音 (焼き込み結果が鳴っていない): " + JSON.stringify(after));
}
const delta = Math.abs(after.integrated_lufs - before.integrated_lufs);
if (delta > 0.5) {
  fail(
    "結合の前後でラウドネスが変わった: before=" +
      before.integrated_lufs +
      " after=" +
      after.integrated_lufs +
      " (差 " +
      delta.toFixed(2) +
      " LU)。フェーダー / insert の二重適用を疑う",
  );
}
const peakDelta = Math.abs(after.sample_peak_dbfs - before.sample_peak_dbfs);
if (peakDelta > 0.5) {
  fail(
    "結合の前後でピークが変わった: before=" +
      before.sample_peak_dbfs +
      " after=" +
      after.sample_peak_dbfs,
  );
}

// ---- 5. クリップの途中から始まる範囲でも音が変わらないこと ----------------
// 焼き込みは範囲の頭から cold で走る。範囲の頭より前から鳴っているクリップの
// 「途中」が正しく焼けないと、ここで頭が無音になって落ちる。
daw.appLoadSongJson(JSON.stringify(song));
daw.sleepMs(300);
const midBefore = JSON.parse(daw.analyzeLoudnessJson(1.0, 3.0, 60000));
if (midBefore.integrated_lufs === null) fail("再 load 後が無音");

// 1..3 拍 = 前クリップの後半 + 後クリップの前半 (= 両端でクリップを跨ぐ範囲)。
daw.setTimeSelection(1.0, 3.0, JSON.stringify([1]));
daw.dispatchGlue();

waited = 0;
while (waited < 30000) {
  s = JSON.parse(daw.inspectSongJson());
  // 範囲の外は元のクリップとして残るので、1(前) + 1(結合) + 1(後) = 3 本。
  if (s.tracks[0].clips.length === 3) break;
  daw.sleepMs(200);
  waited += 200;
}
expectEq(s.tracks[0].clips.length, 3, "範囲 Glue 後のクリップ数 (外側は残る)");
const midClip = s.tracks[0].clips.filter((c) => c.start_beat === 1.0)[0];
if (!midClip) fail("1 拍から始まる結合クリップが無い");
expectEq(midClip.length_beats, 2.0, "結合クリップの長さ");
expectEq(
  s.clip_contents[String(midClip.content_id)].events.length,
  1,
  "結合クリップの event 数",
);

const midAfter = JSON.parse(daw.analyzeLoudnessJson(1.0, 3.0, 60000));
if (midAfter.integrated_lufs === null) {
  fail("範囲 Glue の結果が無音 (クリップの途中が焼けていない)");
}
const midDelta = Math.abs(midAfter.integrated_lufs - midBefore.integrated_lufs);
if (midDelta > 0.5) {
  fail(
    "クリップ途中から始まる範囲の結合で音が変わった: before=" +
      midBefore.integrated_lufs +
      " after=" +
      midAfter.integrated_lufs +
      " (差 " +
      midDelta.toFixed(2) +
      " LU)",
  );
}

// ---- 6. 同名クリップの 2 トラックを一度に焼いても混ざらないこと ------------
// 出力 WAV 名の一意化が「名前 + ミリ秒」だけだと、同名トラックを同じミリ秒で
// 採番して **同じファイル**を掴み、後の render が前を上書きして片方のトラックから
// もう片方の音が鳴る。名前は content 名由来なので、同じ素材を 2 トラックに置くだけで踏む。
const two = JSON.parse(JSON.stringify(song));
two.tracks.push({
  id: 2,
  name: "Audio 2",
  volume: 1.0,
  pan: 0.0,
  muted: false,
  solo: false,
  clips: [
    { id: 1, name: "front", start_beat: 0.0, length_beats: 2.0, content_id: 3 },
    { id: 2, name: "back", start_beat: 2.0, length_beats: 2.0, content_id: 4 },
  ],
  next_clip_id: 3,
});
two.next_track_id = 3;
two.clip_contents["3"] = { events: [audioEvent(0.0, 0, 48000)] };
two.clip_contents["4"] = { events: [audioEvent(0.0, 48000, 96000)] };
two.next_content_id = 5;
daw.appLoadSongJson(JSON.stringify(two));
daw.sleepMs(300);

daw.setTimeSelection(0.0, 4.0, JSON.stringify([1, 2]));
daw.dispatchGlue();

waited = 0;
while (waited < 60000) {
  s = JSON.parse(daw.inspectSongJson());
  if (s.tracks[0].clips.length === 1 && s.tracks[1].clips.length === 1) break;
  daw.sleepMs(200);
  waited += 200;
}
expectEq(s.tracks[0].clips.length, 1, "2 トラック同時 Glue: track1 のクリップ数");
expectEq(s.tracks[1].clips.length, 1, "2 トラック同時 Glue: track2 のクリップ数");
const srcOf = (clip) =>
  s.clip_contents[String(clip.content_id)].events[0].source_id;
const s1 = srcOf(s.tracks[0].clips[0]);
const s2 = srcOf(s.tracks[1].clips[0]);
if (s1 === s2) fail("2 トラックの焼き込み結果が同じ audio source を指している");
// 保存形は nested (`media.audio_sources`)。load 時の前処理がフラット形を移す。
const sources = s.media.audio_sources;
const path1 = JSON.stringify(sources[String(s1)].path);
const path2 = JSON.stringify(sources[String(s2)].path);
if (path1 === path2) {
  fail("2 トラックの焼き込み WAV が同じファイル (後の render が前を上書きする): " + path1);
}

// ---- 7. マスターストリップが ON でも音が変わらないこと (r.md #92) ------------
// 焼き込みはトラック単独の isolate render だが、`master_strip` (バスコンプ / リミッター) を
// 外し忘れると GR が WAV に焼き込まれ、再生時にもう一度マスターを通って二重に掛かる
// (実機: comp + limiter ON の曲で Glue した Kick が -4.5 dB)。強めの設定で差を露出させる。
const withMaster = JSON.parse(JSON.stringify(song));
withMaster.master_strip = {
  comp: {
    on: true,
    threshold_db: -30.0,
    ratio: "R10",
    attack: "A3",
    release: "R300",
    makeup_db: 0.0,
  },
  eq: { on: false, low_db: 0.0, lomid_db: 0.0, high_db: 0.0 },
  limiter: { on: true, ceiling_db: -20.0 },
};
daw.appLoadSongJson(JSON.stringify(withMaster));
daw.sleepMs(300);
const masterBefore = JSON.parse(daw.analyzeLoudnessJson(0.0, 4.0, 60000));
if (masterBefore.integrated_lufs === null) fail("master strip ON の song が無音");

daw.setTimeSelection(0.0, 4.0, JSON.stringify([1]));
daw.dispatchGlue();

waited = 0;
while (waited < 30000) {
  s = JSON.parse(daw.inspectSongJson());
  if (s.tracks[0].clips.length === 1) break;
  daw.sleepMs(200);
  waited += 200;
}
expectEq(s.tracks[0].clips.length, 1, "master strip ON で Glue 後のクリップ数");
// ストリップは song 側に残っている (焼き込みが外すのは render 用の使い捨て Song だけ)。
expectEq(s.master_strip.comp.on, true, "master comp kept");
expectEq(s.master_strip.limiter.on, true, "master limiter kept");

const masterAfter = JSON.parse(daw.analyzeLoudnessJson(0.0, 4.0, 60000));
if (masterAfter.integrated_lufs === null) fail("master strip ON の Glue 結果が無音");
const masterDelta = Math.abs(masterAfter.integrated_lufs - masterBefore.integrated_lufs);
if (masterDelta > 0.5) {
  fail(
    "マスターストリップ ON で結合の前後のラウドネスが変わった: before=" +
      masterBefore.integrated_lufs +
      " after=" +
      masterAfter.integrated_lufs +
      " (差 " +
      masterDelta.toFixed(2) +
      " LU)。焼き込みが master_strip を通している (二重適用) を疑う",
  );
}
