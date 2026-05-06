// PR4 sidechain integration test (wired version): MCompressor with
// sidechain hooked from Track 1 (impulse) into Track 2 (constant DC).
// 期待される観測: Track 2 の出力は MCompressor が Track 1 の impulse を
// trigger として gain reduction を加える分、 sample 0 周辺で振幅が落ち、
// release で 0.5 (元 DC) に戻る → 時間方向に variation がある。
//
// Test runner (Rust 側) は同じ song で sidechain を 「外した版」 (unwired)
// も render し、 wired vs unwired の **WAV 差分** が閾値以上であることを
// 検証する (= sidechain 経路が実際に plugin に届いている証明)。 plugin の
// 具体的 compression curve を仮定しない portability の高いテスト。

const SR = 48000;
const FRAMES = SR * 1;

const wired = daw.scriptArgs.wireSidechain === "true";

const song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 8.0,
  loop_start_beat: 0.0,
  loop_end_beat: 0.0,
  tracks: [
    {
      id: 1,
      name: "Trigger",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      reported_latency_samples: 0,
    },
    {
      id: 2,
      name: "Bass",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      reported_latency_samples: 0,
      fx_chain: [
        {
          plugin_id: "MCompressor",
          format: "Vst3",
          state: null,
          sidechain_sources: wired ? [1] : [],
        },
      ],
    },
  ],
  next_track_id: 3,
};
daw.loadSongFromObject(song);

daw.setSlotPlugin(
  2,
  2,
  0,
  "vst3",
  "C:/Program Files/Common Files/VST3/MeldaProduction/Dynamics/MCompressor.vst3",
  "",
);
daw.waitForPluginLoaded(2, 2, 0, 30000);

// Track 1: 短い loud burst (1.0 を 100 sample 続けて MCompressor の
// envelope を確実に超えさせる)。
const trigger = new Float32Array(FRAMES);
for (let i = 0; i < 100; i++) trigger[i] = 1.0;

// Track 2: 一定 DC bias 0.5。 MCompressor が gain reduction を加えれば
// この値から落ちる。
const bass = new Float32Array(FRAMES);
for (let i = 0; i < FRAMES; i++) bass[i] = 0.5;

daw.setVocalAudio(1, 0, 0, trigger, SR);
daw.setVocalAudio(2, 0, 0, bass, SR);

daw.exportWav(daw.scriptArgs.output, 60000);
