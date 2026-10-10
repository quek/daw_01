// PR4 sidechain integration smoke: MCompressor (Track 2) の aux 入力へ Track 1 を
// sidechain としてつないだ状態で書き出しが最後まで走り、両 track の音が master に届くこと。
// harness: daw_gui/tests/sidechain_real_vst3.rs (trigger / bass の WAV を書いて --arg で渡し、
// 書き出しの判定もそちらが持つ)。
//
// MCompressor は既定で external sidechain を検出に使わない (= internal detection) ので、
// sidechain による gain reduction の形は見ない (理由は harness の module doc)。

function fail(msg) {
  throw new Error("pdc_mcompressor_sidechain.js: " + msg);
}

// 音源と plugin の場所は harness が渡す (開発機のパスをここに書かない — 別マシンでは必ず外れ、
// 個人の環境が repo に焼き付く)。
const args = daw.scriptArgs;
if (!args.trigger || !args.bass || !args.frames || !args.sr) {
  fail("--arg trigger=<WAV> bass=<WAV> frames=<N> sr=<Hz> が要る");
}
if (!args.plugin) fail("--arg plugin=<MCompressor.vst3 のパス> が要る");
const FRAMES = Number(args.frames);
const BPM = 120.0;
const CLIP_BEATS = (FRAMES / Number(args.sr)) * (BPM / 60.0);

// 音源 `sourceId` を曲頭から最後まで鳴らす audio clip の中身 (本番の import と同じ形の AudioContent)。
function wholeSourceContent(sourceId) {
  return {
    events: [
      {
        source_id: sourceId,
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

function wavSource(path) {
  return { path: { Absolute: path }, sample_rate: Number(args.sr), channels: 2, frames: FRAMES };
}

const song = {
  bpm: BPM,
  time_sig: [4, 4],
  length_beats: 8.0,
  tracks: [
    audioTrack(1, "Trigger", 1, []),
    audioTrack(2, "Bass", 2, [
      {
        plugin_id: "MCompressor",
        format: "Vst3",
        state: null,
        // aux 入力 0 番 = Track 1 の fader 後。
        aux_inputs: [{ tap: { source_track: 1, tap_point: "PostFader" } }],
        ports: { has_note_input: false, has_note_output: false, has_audio_input: true, has_audio_output: true },
      },
    ]),
  ],
  next_track_id: 3,
  clip_contents: { 1: wholeSourceContent(1), 2: wholeSourceContent(2) },
  next_content_id: 3,
  audio_sources: { 1: wavSource(args.trigger), 2: wavSource(args.bass) },
  next_audio_source_id: 3,
};
daw.loadSongFromObject(song);

daw.setSlotPlugin(
  2,
  0,
  "vst3",
  args.plugin,
  "",
);
daw.waitForPluginLoaded(2, 0, 30000);

daw.exportWav(args.output, 60000);
