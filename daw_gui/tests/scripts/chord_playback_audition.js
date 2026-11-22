// r.md #157 §3.4: 「再生中も鳴らす」(コードトラックを試聴トラックのノート源にする) を実プロセスで確かめる。
// `daw_gui --features script -- --script <this> --arg plugin=<音源> --arg format=clap|vst3 --arg id=<plugin id>
//   --arg out=<書き出す WAV>` で headless 実行 (無音)。
//
//   1. 音源トラック (クリップなし) とコードの箱を置き、試聴トラックにしてから「再生中も鳴らす」ON で再生 →
//      トラックのメーターが鳴る。
//   2. 書き出しには入らない (書き出した WAV のピークを harness が見る: `out` が無音であること)。
//   3. OFF にして再生 → 鳴らない。

function fail(msg) {
  throw new Error("chord_playback_audition: " + msg);
}
function chordEvent(ev) {
  daw.chordEvent(JSON.stringify(ev));
}
const args = daw.scriptArgs;
if (!args.plugin || !args.format || !args.out) fail("--arg plugin= format= id= out= が要る");

daw.loadSongFromObject({
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 16.0,
  tracks: [
    {
      id: 1,
      name: "Inst",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [],
      next_clip_id: 1,
      devices: [
        {
          plugin_id: args.id || "",
          format: args.format === "clap" ? "Clap" : "Vst3",
          state: null,
          ports: { has_note_input: true, has_note_output: false, has_audio_input: false, has_audio_output: true },
        },
      ],
    },
  ],
  next_track_id: 2,
});
daw.setSlotPlugin(1, 0, args.format, args.plugin, args.id || "");
daw.waitForPluginLoaded(1, 0, 30000);
["C", "Am", "F", "G"].forEach((name, i) => {
  chordEvent({ Chord: { Place: { start: i * 4, len: 4, chord: JSON.parse(daw.parseChord(name)) } } });
});

function playPeak(label) {
  daw.appTick(); // 設定の差分送信は on_tick
  daw.sleepMs(300);
  daw.appPlay();
  const peak = daw.trackPeak(1, 1500);
  daw.appStop();
  daw.sleepMs(500);
  daw.log(label + ": track peak " + peak);
  return peak;
}

const silent = playPeak("試聴トラック未選択");
if (silent > 1e-4) fail("試聴トラックが無いのに鳴った: " + silent);

chordEvent({ ChordTool: { SetAuditionTrack: 1 } });
chordEvent({ ChordTool: { SetAuditionWhilePlaying: true } });
const on = playPeak("再生中も鳴らす ON");
if (!(on > 0.01)) fail("再生中も鳴らす ON で鳴らない: " + on);

daw.exportWav(args.out, 60000);
daw.log("exported " + args.out);

chordEvent({ ChordTool: { SetAuditionWhilePlaying: false } });
const off = playPeak("再生中も鳴らす OFF");
if (off > 1e-4) fail("OFF でも鳴った: " + off);
daw.log("OK");
