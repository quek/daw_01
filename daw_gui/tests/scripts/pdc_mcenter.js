// PDC integration test: 実 VST3 (MCenter) をロードして 2 track 間の
// 位相揃いを WAV 出力で検証する。 daw_gui --script で headless 実行。
// harness: daw_gui/tests/pdc_real_vst3.rs (音源の WAV を書いて --arg wav= / frames= / sr= /
// plugin= / latency= と --output で渡し、書き出しの判定もそちらが持つ)。
//
// MCenter のデフォルト latency は **4096 sample** (Reaper の "FX: Track 6"
// dialog で 4096/4096 spls と表示されることでユーザーが確認済)。
//
// 期待: PDC 補償が効くので、 Track A (latency 0) は ApplyDelay(4096) で
// 4096 sample 遅延され、 Track B の MCenter 出力と **ぴったり重なる**。書き出しは
// master latency ぶん窓をずらすので、重なったパルスは WAV の先頭に出る。
//
// PDC が無効なら Track A は直送りされ、 Track B は 4096 sample 後に
// MCenter から出るので、 ずれた 2 つの山になる。

function fail(msg) {
  throw new Error("pdc_mcenter.js: " + msg);
}

// 音源と plugin の場所は harness が渡す (開発機のパスをここに書かない — 別マシンでは必ず外れ、
// 個人の環境が repo に焼き付く)。
const args = daw.scriptArgs;
if (!args.wav || !args.frames || !args.sr) fail("--arg wav=<WAV> frames=<N> sr=<Hz> が要る");
if (!args.plugin || !args.latency) fail("--arg plugin=<MCenter.vst3 のパス> latency=<N> が要る");
const FRAMES = Number(args.frames);
const BPM = 120.0;
const CLIP_BEATS = (FRAMES / Number(args.sr)) * (BPM / 60.0);

// 音源 1 本を曲頭から最後まで鳴らす audio clip の中身 (本番の import と同じ形の AudioContent)。
function wholeSourceContent() {
  return {
    events: [
      {
        source_id: 1,
        event_start_in_clip_beats: 0.0,
        event_length_beats: CLIP_BEATS,
        source_start_frames: 0,
        source_end_frames: FRAMES,
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
  };
}

function audioTrack(id, name, contentId, devices) {
  return {
    id: id,
    name: name,
    volume: 1.0,
    pan: 0.0,
    muted: false,
    solo: false,
    clips: [{ id: 1, name: name, start_beat: 0.0, length_beats: CLIP_BEATS, content_id: contentId }],
    next_clip_id: 2,
    devices: devices,
  };
}

// Track B の device chain に MCenter を **song の時点で含めておく** (render 経路は song の
// device chain を回すので、SetSlotPlugin で plugin_host に load しただけでは乗らない。
// production の `daw_gui::app::select_plugin_from_db` も song と plugin_host の両方を更新する)。
const song = {
  bpm: BPM,
  time_sig: [4, 4],
  length_beats: 8.0,
  tracks: [
    audioTrack(1, "A clean", 1, []),
    audioTrack(2, "B mcenter", 2, [
      {
        plugin_id: "MCenter",
        format: "Vst3",
        state: null,
        ports: { has_note_input: false, has_note_output: false, has_audio_input: true, has_audio_output: true },
      },
    ]),
  ],
  next_track_id: 3,
  clip_contents: { 1: wholeSourceContent(), 2: wholeSourceContent() },
  next_content_id: 3,
  audio_sources: {
    1: { path: { Absolute: args.wav }, sample_rate: Number(args.sr), channels: 2, frames: FRAMES },
  },
  next_audio_source_id: 2,
};
daw.loadSongFromObject(song);

daw.setSlotPlugin(
  2,
  0, // device index (単一デバイスチェーン)
  "vst3",
  args.plugin,
  "",
);
daw.waitForPluginLoaded(2, 0, 30000);

// plugin は `PluginEvent::PluginLatencyChanged { device_id, samples }` で自身の latency を
// 通知し、script の host が `AudioCommand::SetDeviceLatency` として daw_audio へ中継する
// (engine が `compile_schedule` で PDC を再計算。r.md #9: 報告 latency は Song に載せない)。
//
// PDC は「latency があっても出力を遅延なしと同じにする」仕組みなので、書き出しだけでは
// 「MCenter が経路に居ない (latency 0)」と PDC 成功を区別できない (既定の MCenter は素通り +
// 遅延だけで、音を変えない)。だから PDC の前提を 2 つ、ここで確かめる:
//   1. latency の報告が harness の想定 (--arg latency=) どおり届いた
//   2. MCenter が render の経路に居る (書き出しの後に少し再生し、plugin が呼ばれた回数を見る。
//      live と書き出しは同じ render 関数 = 不変条件 6。bypass 等で経路に居なければ 0 回)
const latency = daw.waitForPluginLatency(2, 0, 30000);
if (latency !== Number(args.latency)) {
  fail("MCenter の latency の報告が " + args.latency + " でない: " + latency + " (PDC を確かめる前提が崩れた)");
}

daw.exportWav(args.output, 60000);

daw.perfJson(); // 計測の窓を空にする
daw.play();
daw.sleepMs(300);
const graph = JSON.parse(daw.perfJson()).graph;
daw.stop();
if (!(graph.dispatches > 0)) {
  fail("再生中に plugin が 1 度も呼ばれない (MCenter が render の経路に居ない): " + JSON.stringify(graph));
}
