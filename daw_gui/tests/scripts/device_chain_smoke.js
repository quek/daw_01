// 単一デバイスチェーン (`docs/plan_linear_chain.md`) を end-to-end で検証する JS
// smoke test。`daw_gui --script` で headless 実行。exit 0 で pass、JS error で 1。
//
// production と同じ経路を通す:
//   appLoadSongJson(songWithDevices) → host.app.song に load (ensure_ids /
//     port 解決 / migration 含む)
//   → deviceChain(trackId) が各 device の {plugin_id, ports} を順序通り返す。
//
// 役割判定はしない (engine は port を順に直結するだけ)。ここで検証するのは
// 「チェーンの並び順」と「各 device の port 構成」が load を通して保たれること。
// (D&D 並び替え / 音追従) の回帰 — device の欠落・並び崩れ・port 喪失 —
// を CI で即検出する。plugin / VST3 不要 (ports を JSON に直接書く)。
//
// r.md #71 (プラグインのコピー / 移動): 末尾に **トラックを跨いだ運搬** の
// 並び順 / port も足してある (`daw.relocateDevices`)。
//
// port = (note_in, note_out, audio_out, audio_in)。

function dev(id, ni, no, ao, ai) {
  return {
    plugin_id: id,
    ports: {
      has_note_input: ni,
      has_note_output: no,
      has_audio_output: ao,
      has_audio_input: ai,
    },
  };
}

// 代表 plugin の port 構成 (実機 probe 値に対応):
const SCALER = dev("scaler", true, true, true, true); // 生成器+音源 (note_out 有り)
const ANALOG = dev("analog", true, false, true, true); // 音源 (note_out 無し)
const DELAY = dev("delay", true, false, true, true); // audio エフェクト
const REVERB = dev("reverb", false, false, true, true); // audio エフェクト (note 無し)

function songWithDevices(devices) {
  return {
    bpm: 120.0,
    time_sig: [4, 4],
    length_beats: 16.0,
    tracks: [
      {
        id: 1,
        name: "T",
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        devices: devices,
      },
    ],
  };
}

// r.md #71 (プラグインのコピー / 移動) 用: 2 トラックの曲。
function songWithTwoTracks(t1Devices, t2Devices) {
  const song = songWithDevices(t1Devices);
  song.tracks.push({
    id: 2,
    name: "T2",
    volume: 1.0,
    pan: 0.0,
    muted: false,
    solo: false,
    devices: t2Devices,
  });
  return song;
}

function chainFor(devices) {
  daw.appLoadSongJson(JSON.stringify(songWithDevices(devices)));
  return JSON.parse(daw.deviceChain(1));
}

// load を通したチェーンが、与えた順序・plugin_id・port をそのまま保持することを assert。
function expectChain(devices, label) {
  const got = chainFor(devices);
  if (got.length !== devices.length) {
    throw new Error(
      label + ": length expected " + devices.length + " got " + got.length
    );
  }
  for (let i = 0; i < devices.length; i++) {
    if (got[i].plugin_id !== devices[i].plugin_id) {
      throw new Error(
        label + ": index " + i + " plugin_id expected " +
        devices[i].plugin_id + " got " + got[i].plugin_id
      );
    }
    const wp = devices[i].ports;
    const gp = got[i].ports;
    if (
      gp.has_note_input !== wp.has_note_input ||
      gp.has_note_output !== wp.has_note_output ||
      gp.has_audio_output !== wp.has_audio_output ||
      gp.has_audio_input !== wp.has_audio_input
    ) {
      throw new Error(
        label + ": index " + i + " ports mismatch want " +
        JSON.stringify(wp) + " got " + JSON.stringify(gp)
      );
    }
  }
}

// 単体。
expectChain([SCALER], "single device round-trips");

// Scaler → Analog Lab の順序が保たれる。
expectChain([SCALER, ANALOG], "scaler then analog keeps order");

// 逆順 Analog Lab → Scaler も別物として保たれる (並び替えで音が変わる前提)。
expectChain([ANALOG, SCALER], "analog then scaler keeps order");

// audio エフェクトだけ (audio クリップ駆動トラック)。
expectChain([REVERB], "fx-only chain round-trips");

// フルチェーン。
expectChain([SCALER, ANALOG, DELAY], "full chain keeps order and ports");

// 空チェーン。
expectChain([], "empty chain round-trips");

// ---------------------------------------------------------------------------
// r.md #71 (プラグインのコピー / 移動): device を別トラックへ運んだあとの
// **並び順と port** が両チェーンで保たれること。 移動は device instance を
// 作り直さない (= 音が切れない) 設計なので、Song 側の並びが唯一の観測点。
// ---------------------------------------------------------------------------

// 与えたチェーンを assert する小道具 (トラック指定版)。
function expectTrackChain(trackId, want, label) {
  const got = JSON.parse(daw.deviceChain(trackId));
  if (got.length !== want.length) {
    throw new Error(
      label + ": track " + trackId + " length expected " + want.length +
      " got " + got.length
    );
  }
  for (let i = 0; i < want.length; i++) {
    if (got[i].plugin_id !== want[i].plugin_id) {
      throw new Error(
        label + ": track " + trackId + " index " + i + " plugin_id expected " +
        want[i].plugin_id + " got " + got[i].plugin_id
      );
    }
    const wp = want[i].ports;
    const gp = got[i].ports;
    if (
      gp.has_note_input !== wp.has_note_input ||
      gp.has_note_output !== wp.has_note_output ||
      gp.has_audio_output !== wp.has_audio_output ||
      gp.has_audio_input !== wp.has_audio_input
    ) {
      throw new Error(
        label + ": track " + trackId + " index " + i + " ports mismatch want " +
        JSON.stringify(wp) + " got " + JSON.stringify(gp)
      );
    }
  }
}

// (1) track1 の DELAY (index 1) を track2 の先頭へ **移動**。
daw.appLoadSongJson(JSON.stringify(songWithTwoTracks([ANALOG, DELAY], [REVERB])));
daw.relocateDevices(1, "[1]", 2, 0, false);
expectTrackChain(1, [ANALOG], "move: source chain loses the device");
expectTrackChain(2, [DELAY, REVERB], "move: dest chain gains it at index 0 with ports intact");

// (2) 同一チェーン内の移動 = 並べ替え (末尾へ)。
daw.appLoadSongJson(JSON.stringify(songWithTwoTracks([ANALOG, DELAY, REVERB], [])));
daw.relocateDevices(1, "[0]", 1, 3, false);
expectTrackChain(1, [DELAY, REVERB, ANALOG], "same-chain move reorders");

// (3) **コピー**は元を残し、落とし先にも同じ plugin_id / port が現れる。
daw.appLoadSongJson(JSON.stringify(songWithTwoTracks([SCALER], [REVERB])));
daw.relocateDevices(1, "[0]", 2, 1, true);
expectTrackChain(1, [SCALER], "copy keeps the source device");
expectTrackChain(2, [REVERB, SCALER], "copy lands at the requested index with ports intact");

// (4) 複数選択の移動は **指定順 (= チェーン表示順)** を保つ。
daw.appLoadSongJson(JSON.stringify(songWithTwoTracks([ANALOG, DELAY, REVERB], [])));
daw.relocateDevices(1, "[0,2]", 2, 0, false);
expectTrackChain(1, [DELAY], "multi move leaves the unselected device");
expectTrackChain(2, [ANALOG, REVERB], "multi move keeps the chain order of the selection");

// すべて throw されなければ exit 0 で pass。
