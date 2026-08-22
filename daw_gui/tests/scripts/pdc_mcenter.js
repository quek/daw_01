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
//
// PR8: vocal の inject 経路は `setGeneratedAudio(id, samples, sr)` に変わり、
// `process_track_owned` の vocal block は **track が clip を持っている範囲**
// しか mix しないので、 各 track に length 8 beats (= 1 sec @ 120 bpm × 4)
// の clip を 1 つ置く。 click は sample 0 にしかパルスが立たないので長さは
// 何でも良いが、 song.length_beats (=8) と揃えて全範囲カバー。
const song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 8.0,
  tracks: [
    {
      id: 1,
      name: "A clean",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [{ id: 1, name: "click", start_beat: 0.0, length_beats: 8.0 }],
    },
    {
      id: 2,
      name: "B mcenter",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [{ id: 1, name: "click", start_beat: 0.0, length_beats: 8.0 }],
      devices: [
        {
          plugin_id: "MCenter",
          format: "Vst3",
          state: null,
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
  0, // device index (末尾 append、 単一デバイスチェーン)
  "vst3",
  "C:/Program Files/Common Files/VST3/MeldaProduction/Stereo/MCenter.vst3",
  "",
);
daw.waitForPluginLoaded(2, 0, 30000);

// PR3.3: plugin が `PluginEvent::PluginLatencyChanged { device_id, samples }`
// で自身の latency を IPC 経由で通知する。 script.rs の `pump_until` が受信して
// `AudioCommand::SetDeviceLatency` として daw_audio へ中継し、engine が
// `compile_schedule` で PDC を再計算する (r.md #9: 報告 latency は Song に
// 載せない = 保存されない)。 `setDeviceLatency` 手動 call は不要。
//
// 確実に最新 latency が schedule に反映されてから vocal を inject する
// よう、 `waitForPluginLoaded` の後に明示的に IPC drain を 1 frame 挟む
// のが理想だが、 `pump_until` 中に PluginLatencyChanged は同じ ack で
// 届く (plugin_host が SlotPluginLoaded → PluginLatencyChanged の順で
// emit) ので、 waitForPluginLoaded 完了時には latency 反映済。

const click = new Float32Array(FRAMES);
click[0] = 1.0;

// PR8: vocal_gen_id(track_id, clip_id) = (track_id << 32) | clip_id
const genId = (track, clip) => track * 0x1_0000_0000 + clip;
daw.setGeneratedAudio(genId(1, 1), click, SR);
daw.setGeneratedAudio(genId(2, 1), click, SR);

daw.exportWav(daw.scriptArgs.output, 60000);
