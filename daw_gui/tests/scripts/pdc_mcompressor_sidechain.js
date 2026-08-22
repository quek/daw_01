// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

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

// PR8: vocal の inject 経路は `setGeneratedAudio` に変わり、 vocal mix は
// track が clip を持つ範囲しか走らない。 各 track に length 8 beats の clip
// を 1 つ用意して、 SR=48000 / 1 sec の buffer を全範囲カバーさせる。
const song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 8.0,
  tracks: [
    {
      id: 1,
      name: "Trigger",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [{ id: 1, name: "trigger", start_beat: 0.0, length_beats: 8.0 }],
    },
    {
      id: 2,
      name: "Bass",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [{ id: 1, name: "bass", start_beat: 0.0, length_beats: 8.0 }],
      devices: [
        {
          plugin_id: "MCompressor",
          format: "Vst3",
          state: null,
          sidechain_sources: wired ? [1] : [],
          ports: { has_note_input: false, has_note_output: false, has_audio_output: true },
        },
      ],
    },
  ],
  next_track_id: 3,
};
daw.loadSongFromObject(song);

daw.setSlotPlugin(
  2,
  0,
  "vst3",
  "C:/Program Files/Common Files/VST3/MeldaProduction/Dynamics/MCompressor.vst3",
  "",
);
daw.waitForPluginLoaded(2, 0, 30000);

// Track 1: 短い loud burst (1.0 を 100 sample 続けて MCompressor の
// envelope を確実に超えさせる)。
const trigger = new Float32Array(FRAMES);
for (let i = 0; i < 100; i++) trigger[i] = 1.0;

// Track 2: 一定 DC bias 0.5。 MCompressor が gain reduction を加えれば
// この値から落ちる。
const bass = new Float32Array(FRAMES);
for (let i = 0; i < FRAMES; i++) bass[i] = 0.5;

// PR8: vocal_gen_id(track_id, clip_id) = (track_id << 32) | clip_id
const genId = (track, clip) => track * 0x1_0000_0000 + clip;
daw.setGeneratedAudio(genId(1, 1), trigger, SR);
daw.setGeneratedAudio(genId(2, 1), bass, SR);

daw.exportWav(daw.scriptArgs.output, 60000);
