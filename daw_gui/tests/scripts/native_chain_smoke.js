// M-5: 内蔵 device (r.md #129) を実プロセス (daw_audio) で鳴らして確かめる。
// harness: daw_gui/tests/native_chain_smoke.rs (2 kHz のサイン波 WAV を書いて --arg wav= / dir= で渡す)。
//
// flow:
//   1. 大きいクリップ 1 本のトラックを load し、組み込み Comp を thr -40 / ratio 10 にする (触ると自動で ON)。
//      1 秒再生して GR > 3 dB。
//   2. Listen: 検出フィルタを 150 Hz にしてから Comp を bypass → master のピークを測る。
//      Listen を押すと Comp が有効化され (bypassed=false)、2 kHz の音が検出信号 (150 Hz の帯域通過) に
//      置き換わって master のピークが下がる。
//   3. Listen 中に書き出した WAV と、Listen を外して書き出した WAV を harness が突き合わせる (Listen は乗らない)。
//   4. Limiter: トラックを +6 dB、Limiter の ceiling を -6 dB にすると、書き出しのサンプルピークも
//      再生中の master のピークも -5.9 dBFS 以下。

function fail(msg) {
  throw new Error("native_chain_smoke: " + msg);
}

function expectEq(actual, expected, label) {
  if (actual !== expected) fail(label + ": expected " + expected + " got " + actual);
}

const wavPath = daw.scriptArgs.wav;
const outDir = daw.scriptArgs.dir;
if (!wavPath || !outDir) fail("--arg wav=<path> と --arg dir=<dir> が要る");

// 120 BPM → 1 拍 = 0.5 秒 = 24000 frames @48k。source は 8 秒 = 384000 frames = 16 拍。
const SOURCE_FRAMES = 384000;
const CLIP_BEATS = 16.0;

function songWith(trackPatch, songPatch) {
  const song = {
    bpm: 120.0,
    time_sig: [4, 4],
    length_beats: 32.0,
    tracks: [
      Object.assign(
        {
          id: 1,
          name: "Audio",
          volume: 1.0,
          pan: 0.0,
          muted: false,
          solo: false,
          clips: [{ id: 1, name: "tone", start_beat: 0.0, length_beats: CLIP_BEATS, content_id: 1 }],
          next_clip_id: 2,
        },
        trackPatch,
      ),
    ],
    next_track_id: 2,
    clip_contents: {
      "1": {
        events: [
          {
            source_id: 1,
            event_start_in_clip_beats: 0.0,
            event_length_beats: CLIP_BEATS,
            source_start_frames: 0,
            source_end_frames: SOURCE_FRAMES,
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
      "1": { path: { Absolute: wavPath }, sample_rate: 48000, channels: 1, frames: SOURCE_FRAMES },
    },
    next_audio_source_id: 2,
  };
  return Object.assign(song, songPatch);
}

function builtinComp() {
  const natives = JSON.parse(daw.nativeDevices(1));
  const comp = natives.find((n) => n.kind === "Comp" && n.builtin);
  if (!comp) fail("トラック 1 に組み込み Comp が居ない: " + JSON.stringify(natives));
  return comp;
}

function compParams(pairs) {
  return JSON.stringify({ Params: pairs.map(([p, v]) => [{ Comp: p }, v]) });
}

// ---- 1. 組み込み Comp の GR -------------------------------------------------
daw.appLoadSongJson(JSON.stringify(songWith({}, {})));
daw.sleepMs(300);
const compId = builtinComp().id;
expectEq(builtinComp().bypassed, true, "新しいトラックの組み込み Comp は OFF で始まる");
daw.nativeEdit(compId, compParams([["Threshold", -40.0], ["Ratio", 10.0]]));
expectEq(builtinComp().bypassed, false, "つまみに触ると組み込み Comp が自動で ON");
daw.sleepMs(300);

daw.play();
daw.sleepMs(1000);
const gr = daw.nativeGainReduction(compId);
if (!(gr > 3.0)) fail("thr -40 / ratio 10 の Comp で GR が 3 dB を超えない: " + gr);

// ---- 2. SC Listen: bypass の Comp を有効化し、検出信号に置き換わる ------------------
daw.nativeEdit(compId, compParams([["ScFreq", 150.0]]));
daw.setDevicesBypassed(JSON.stringify([compId]), true);
daw.sleepMs(300);
const peakBypassed = daw.masterPeakDbfs(700);
if (peakBypassed === null) fail("bypass 中の master が無音");

daw.setScListen(compId);
expectEq(builtinComp().bypassed, false, "bypass 中に Listen を押すと Comp が有効化される");
daw.sleepMs(300);
const peakListen = daw.masterPeakDbfs(700);
if (peakListen === null || Math.abs(peakListen - peakBypassed) < 3.0) {
  fail(
    "Listen で master のピークが検出信号 (150 Hz の帯域通過) に変わらない: bypass=" +
      peakBypassed + " listen=" + peakListen,
  );
}
daw.stop();
daw.sleepMs(300);

// ---- 3. 書き出しに Listen は乗らない (WAV の突き合わせは harness) -----------------
daw.exportWavRange(outDir + "/listen_on.wav", 0.0, CLIP_BEATS, 120000);
daw.setScListen(null);
daw.sleepMs(100);
daw.exportWavRange(outDir + "/listen_off.wav", 0.0, CLIP_BEATS, 120000);

// ---- 4. フェーダー後の Limiter -----------------------------------------------
daw.appLoadSongJson(JSON.stringify(songWith({ volume: 2.0 }, { master_limiter: { on: true, ceiling_db: -6.0 } })));
daw.sleepMs(300);
const report = JSON.parse(daw.analyzeLoudnessJson(0.0, CLIP_BEATS, 120000));
if (report.sample_peak_dbfs === null || report.sample_peak_dbfs > -5.9) {
  fail("+6 dB を ceiling -6 dB の Limiter に入れた書き出しのピークが -5.9 dBFS を超える: " + report.sample_peak_dbfs);
}
daw.play();
daw.sleepMs(500);
const livePeak = daw.masterPeakDbfs(1000);
const limiterGr = daw.masterLimiterGainReduction();
daw.stop();
if (livePeak === null || livePeak > -5.9) fail("再生中の master のピークが -5.9 dBFS を超える: " + livePeak);
if (!(limiterGr > 3.0)) fail("+6 dB を入れても Limiter の GR が 3 dB を超えない: " + limiterGr);
