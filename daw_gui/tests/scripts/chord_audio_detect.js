// r.md #157 §3.4: オーディオからのコード検出の精度を実データで確かめる (`docs/plan_chord_track.md`)。
// `daw_gui --features script -- --script <this> --arg mode=... ...` で headless 実行 (無音)。
//
// 2 段で回す (書き出した WAV のサンプルレートとフレーム数は、書き出してから harness が WAV の頭を読んで渡す):
//
//   mode=render  plugin=<.clap/.vst3> format=clap|vst3 id=<plugin id> voicing=<Basic|Piano|Guitar|Adaptive>
//                out=<書き出す WAV>
//     既知の進行 (PROGRESSION) を帯に置き、`ChordsToClip` で音源トラックのノートにして書き出す。
//   mode=detect  wav=<WAV> sr=<Hz> frames=<N> [window=<拍>]
//     その WAV をオーディオクリップにして `DetectAudioClips { place: true }` を撃ち、帯に置かれた
//     コードを元の進行と突き合わせて、合った数を `RESULT <一致>/<全体>` で出す (全部合えば exit 0)。

const BPM = 120.0;
const BAR = 4.0;
// 1 小節ずつ。三和音・7th・sus・転回形を混ぜる。
const PROGRESSION = ["C", "Am", "F", "G7", "Em7", "Dm7", "Gsus4", "C/E"];

function fail(msg) {
  throw new Error("chord_audio_detect: " + msg);
}
function chordEvent(ev) {
  daw.chordEvent(JSON.stringify(ev));
}
function parse(text) {
  const json = daw.parseChord(text);
  if (json === null) fail("parseChord(" + text + ")");
  return JSON.parse(json);
}

const args = daw.scriptArgs;
const mode = args.mode;

if (mode === "render") {
  if (!args.plugin || !args.format || !args.out) fail("--arg plugin= format= id= out= が要る");
  const length = PROGRESSION.length * BAR;
  daw.loadSongFromObject({
    bpm: BPM,
    time_sig: [4, 4],
    length_beats: length,
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
  PROGRESSION.forEach((name, i) => {
    chordEvent({ Chord: { Place: { start: i * BAR, len: BAR, chord: parse(name) } } });
  });
  chordEvent({ ChordTool: { SetVoicing: args.voicing || "Piano" } });
  chordEvent({ ChordTool: { ChordsToClip: { chord_ids: [], track_id: 1 } } });
  daw.setSlotPlugin(1, 0, args.format, args.plugin, args.id || "");
  daw.waitForPluginLoaded(1, 0, 30000);
  daw.sleepMs(500);
  daw.exportWav(args.out, 120000);
  daw.log("rendered " + args.out);
} else if (mode === "detect") {
  if (!args.wav || !args.sr || !args.frames) fail("--arg wav= sr= frames= が要る");
  const sr = Number(args.sr);
  const frames = Number(args.frames);
  const clipBeats = (frames / sr) * (BPM / 60.0);
  daw.appLoadSongJson(
    JSON.stringify({
      bpm: BPM,
      time_sig: [4, 4],
      length_beats: clipBeats,
      tracks: [
        {
          id: 1,
          name: "Audio",
          volume: 1.0,
          pan: 0.0,
          muted: false,
          solo: false,
          clips: [{ id: 1, name: "chords", start_beat: 0.0, length_beats: clipBeats, content_id: 1 }],
          next_clip_id: 2,
        },
      ],
      next_track_id: 2,
      clip_contents: {
        1: {
          events: [
            {
              source_id: 1,
              event_start_in_clip_beats: 0.0,
              event_length_beats: clipBeats,
              source_start_frames: 0,
              source_end_frames: frames,
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
      audio_sources: { 1: { path: { Absolute: args.wav }, sample_rate: sr, channels: 2, frames: frames } },
      next_audio_source_id: 2,
    }),
  );
  // 音源の decode は「開く」経路 (`begin_asset_decode`) が始めるので、保存して開き直す (GUI の Open と同じ)。
  const project = args.wav.replace(/\.wav$/i, "") + "_detect.daw";
  daw.appSaveAs(project);
  // 開いているファイルへの Open はそのタブへの切替になるので、今のタブを別名にしてから開く (新しいタブに読む)。
  daw.appSaveAs(project.replace(/\.daw$/, "_scratch.daw"));
  daw.appOpenProject(project);
  if (args.window) {
    const view = JSON.parse(daw.chordToolsJson()).view;
    view.detect.window_beats = Number(args.window);
    chordEvent({ ChordTool: { SetDetectOptions: view.detect } });
  }
  // 音源の decode を待ってから発注する (decode 前は「読み込みが終わってから」で何もしない)。
  let tries = 0;
  while (!daw.pumpAssetDecode()) {
    if (++tries > 300) fail("音源の decode が終わらない");
    daw.sleepMs(100);
  }
  const t0 = Date.now();
  chordEvent({ ChordTool: { DetectAudioClips: { clips: [[1, 1]], place: true } } });
  if (JSON.parse(daw.chordToolsJson()).detecting === 0) fail("検出が始まらない");
  // 回収は on_tick (script では appTick が回す)。
  for (tries = 0; JSON.parse(daw.chordToolsJson()).detecting > 0; tries++) {
    if (tries > 600) fail("検出が終わらない");
    daw.sleepMs(50);
    daw.appTick();
  }
  daw.log("detect ms: " + (Date.now() - t0));
  const placed = JSON.parse(daw.chordsJson());
  let ok = 0;
  PROGRESSION.forEach((name, i) => {
    const mid = i * BAR + BAR / 2;
    const got = placed.find((c) => c.start_beat <= mid && mid < c.start_beat + c.len_beats);
    const want = parse(name);
    const match =
      got !== undefined &&
      got.chord.root === want.root &&
      (got.chord.kind || "Major") === (want.kind || "Major") &&
      (got.chord.bass === undefined ? null : got.chord.bass) === (want.bass === undefined ? null : want.bass);
    if (match) ok++;
    daw.log("bar " + (i + 1) + ": want " + name + " got " + (got ? got.name : "-") + (match ? "" : "  <-- MISS"));
  });
  daw.log("placed: " + placed.map((c) => c.start_beat + "+" + c.len_beats + " " + c.name).join(", "));
  daw.log("RESULT " + ok + "/" + PROGRESSION.length);
  if (ok !== PROGRESSION.length) fail("一致 " + ok + "/" + PROGRESSION.length);
} else {
  fail("--arg mode=render|detect");
}
