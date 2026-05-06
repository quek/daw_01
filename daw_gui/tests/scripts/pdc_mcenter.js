// PDC integration test: 実 VST3 (MCenter) をロードして 2 track 間の
// 位相揃いを WAV 出力で検証する。 daw_gui --script で headless 実行。
//
// MCenter のデフォルト latency は **4096 sample** (Reaper の "FX: Track 6"
// dialog で 4096/4096 spls と表示されることでユーザーが確認済)。
//
// 期待: PDC 補償が効くので、 Track A (latency 0) は ApplyDelay(4096) で
// 4096 sample 遅延され、 Track B の MCenter 出力 (4096 sample 後に impulse
// が出る) と **sample 4096 でぴったり重なる**。 → master の peak 位置 ~ 4096、
// 振幅 ~ 1.4142 (= 0.7071 × 2 重なり)。
//
// PDC が無効なら Track A は sample 0 に直送りされ、 Track B は sample 4096
// に MCenter から出るので、 ずれた 2 つの peak になる。

const SR = 48000;
const FRAMES = SR * 1; // 1 秒分の click (4096 sample より十分長い)

// Track 2 の fx_chain に MCenter を **song の時点で含めておく** 必要がある。
// audio engine の `process_track_owned` は `song_track.fx_chain.iter()` で
// 回るので、 SetSlotPlugin で plugin_host に load しただけでは render 経路に
// は乗らない。 production の `daw_gui::app::select_plugin_from_db` も song
// と plugin_host の両方を更新する。
const song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 8.0,
  loop_start_beat: 0.0,
  loop_end_beat: 0.0,
  tracks: [
    {
      id: 1,
      name: "A clean",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      reported_latency_samples: 0,
    },
    {
      id: 2,
      name: "B mcenter",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      reported_latency_samples: 0,
      fx_chain: [
        {
          plugin_id: "MCenter",
          format: "Vst3",
          state: null,
        },
      ],
    },
  ],
  next_track_id: 3,
};
daw.loadSongFromObject(song);

daw.setSlotPlugin(
  2,
  2, // Fx
  0,
  "vst3",
  "C:/Program Files/Common Files/VST3/MeldaProduction/Stereo/MCenter.vst3",
  "",
);
daw.waitForPluginLoaded(2, 2, 0, 30000);

// MCenter の実 internal latency 4096 sample (Reaper FX dialog 表示)。
// PR3.3 の自動 IPC ができるまで script から手動 set。
const MCENTER_LATENCY = 4096;
daw.setTrackLatency(2, MCENTER_LATENCY);

const click = new Float32Array(FRAMES);
click[0] = 1.0;

daw.setVocalAudio(1, 0, 0, click, SR);
daw.setVocalAudio(2, 0, 0, click, SR);

daw.exportWav(daw.scriptArgs.output, 60000);
