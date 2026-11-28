// ピアノロールの鍵盤レーンを押すと VOICEVOX のトラックも鳴る、を実プロセス + 実 engine で確かめる。
//
//   cargo test -p common --lib gen_talk_fixture -- --ignored     # target/talk_fixture.daw (VOICEVOX トラック 1 本)
//   cargo run -p daw_gui --features script -- \
//     --script daw_gui/tests/scripts/voicevox_key_preview.js --arg song=target/talk_fixture.daw
//
// VOICEVOX engine (localhost:50021) が要る。窓なしの `--script` なのでスピーカーは鳴らない (メーターで見る)。
//
//   1. 押す前は無音。
//   2. 鍵を押すと、その高さの 1 音が合成されてトラックのメーターが振れる (初回は合成ぶん遅れる)。
//   3. 離すと止まる。
//   4. 同じ鍵をもう一度押すと、合成済みなのですぐ鳴る。
//   5. 初めての鍵を短く click しても鳴る (合成が離した後に届く)。
//
// 合成結果はディスクキャッシュに残るので、2 回目以降の実行は「初めての鍵」も読むだけで速い。

function fail(msg) {
  throw new Error("voicevox_key_preview: " + msg);
}
const song = daw.scriptArgs.song;
if (!song) fail("--arg song=<VOICEVOX トラックを持つ .daw> が要る");
const TRACK = Number(daw.scriptArgs.track || 1);
const THRESHOLD = 0.01;

// 押してからメーターが振れるまでの時間 (ms)。`limitMs` 以内に振れなければ -1。
function pressUntilSound(pitch, limitMs) {
  const t0 = Date.now();
  daw.appPreviewPitch(TRACK, pitch);
  while (Date.now() - t0 < limitMs) {
    if (daw.trackPeak(TRACK, 20) > THRESHOLD) return Date.now() - t0;
  }
  return -1;
}
function release() {
  daw.appPreviewPitch(TRACK, null);
  daw.sleepMs(400); // 離したときのフェードとメーターの戻りを待つ
}

daw.sleepMs(2000);
daw.loadSongFile(song);
daw.sleepMs(6000); // device の load と、曲そのものの合成が済むまで

const before = daw.trackPeak(TRACK, 500);
if (before > THRESHOLD) fail("押す前から鳴っている: peak " + before);

const first = pressUntilSound(60, 5000);
daw.log("初めて押す鍵 (C4): " + first + " ms で鳴る");
if (first < 0) fail("鍵を押しても鳴らない");
release();
const afterRelease = daw.trackPeak(TRACK, 300);
if (afterRelease > THRESHOLD) fail("離しても鳴り続ける: peak " + afterRelease);

const again = pressUntilSound(60, 2000);
daw.log("同じ鍵をもう一度: " + again + " ms で鳴る");
if (again < 0) fail("合成済みの鍵が鳴らない");
release();

const other = pressUntilSound(67, 5000);
daw.log("別の鍵 (G4): " + other + " ms で鳴る");
if (other < 0) fail("別の鍵が鳴らない");
release();

// 5. 初めての鍵を短く click (合成が届く前に離す) しても、届いたところで短く鳴る。
const t0 = Date.now();
daw.appPreviewPitch(TRACK, 64);
daw.appPreviewPitch(TRACK, null);
let tap = -1;
while (Date.now() - t0 < 3000) {
  if (daw.trackPeak(TRACK, 20) > THRESHOLD) {
    tap = Date.now() - t0;
    break;
  }
}
daw.log("初めての鍵を短く click (E4): " + tap + " ms で鳴る");
if (tap < 0) fail("短い click が鳴らない");
daw.sleepMs(800);
const afterTap = daw.trackPeak(TRACK, 300);
if (afterTap > THRESHOLD) fail("短い click の音が止まらない: peak " + afterTap);
daw.log("OK");
