// r.md #161: フォローアクションの全体 on/off を、daw_audio の再生と書き出しまで通して確かめる。
// harness: daw_gui/tests/follow_global_smoke.rs (440 Hz / 880 Hz のサイン波 WAV を書いて --arg low= / high= /
// dir= で渡し、書き出した WAV の区間ごとの音程を測る)。
//
// 行 1 にセルが 2 つ: A (440 Hz、4 拍、フォローアクション = 1 周したら「次」) と B (880 Hz、4 拍、なし)。
// 行は A を鳴らす状態で読み込む。120 BPM なので 4 拍 = 2 秒。
//   - on (既定) の書き出し: [0, 4) 拍が A、[4, 8) 拍が B。
//   - off にしても書き出しは同じ (書き出しには常にフォローアクションが効く)。export_on.wav / export_off.wav。
//   - off で再生: 4 拍を過ぎても A のまま。on に戻すと **次の境界 (4 拍の格子の上)** で B へ移る (暴発しない)。
//   - off のまま engine を作り直しても (デバイスの切り替え)、4 拍を過ぎて A のまま (GUI と engine が食い違わない)。
//   - 切り替えで `*` は立たない。

function fail(msg) {
  throw new Error("follow_global: " + msg);
}

const low = daw.scriptArgs.low;
const high = daw.scriptArgs.high;
const outDir = daw.scriptArgs.dir;
if (!low || !high || !outDir) fail("--arg low=<wav> / high=<wav> / dir=<dir> が要る");

// 120 BPM → 1 拍 = 24000 frames @48k。source は 2 秒 = 4 拍。
const SOURCE_FRAMES = 96000;
const CELL_BEATS = 4.0;
const CELL_A = 1;
const CELL_B = 2;

function content(sourceId) {
  return {
    events: [
      {
        source_id: sourceId,
        event_start_in_clip_beats: 0.0,
        event_length_beats: CELL_BEATS,
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
  };
}
function cell(id, sceneId, name, launch) {
  const c = {
    scene_id: sceneId,
    clip: { id: id, name: name, start_beat: 0.0, length_beats: CELL_BEATS, content_id: id },
  };
  if (launch) c.launch = launch;
  return c;
}

daw.appLoadSongJson(
  JSON.stringify({
    bpm: 120.0,
    time_sig: [4, 4],
    length_beats: 32.0,
    tracks: [
      {
        id: 1,
        name: "Audio",
        volume: 1.0,
        pan: 0.0,
        muted: false,
        solo: false,
        clips: [],
        session_clips: [
          cell(CELL_A, 1, "A", { follow: { enabled: true, a: "Next" } }),
          cell(CELL_B, 2, "B", null),
        ],
        launcher: { Launcher: { clip_id: CELL_A } },
        next_clip_id: 3,
      },
    ],
    next_track_id: 2,
    scenes: [{ id: 1 }, { id: 2 }],
    next_scene_id: 3,
    clip_contents: { "1": content(1), "2": content(2) },
    next_content_id: 3,
    audio_sources: {
      "1": { path: { Absolute: low }, sample_rate: 48000, channels: 1, frames: SOURCE_FRAMES },
      "2": { path: { Absolute: high }, sample_rate: 48000, channels: 1, frames: SOURCE_FRAMES },
    },
    next_audio_source_id: 3,
  }),
);
daw.sleepMs(500);

function state() {
  return JSON.parse(daw.launcherStateJson());
}
function dirty() {
  return JSON.parse(daw.tabsJson()).find((t) => t.active).dirty;
}
// 行 1 でいま鳴っているセル (engine の telemetry)。
function playingCell() {
  const row = state().rows.find((r) => r.trackId === 1 && r.laneId === 0);
  if (!row) fail("行 1 の走行状態が無い: " + daw.launcherStateJson());
  return row;
}
// 再生位置が `beat` を過ぎるまで待つ (上限 `timeoutMs`)。
function waitBeat(beat, timeoutMs) {
  const until = Date.now() + timeoutMs;
  while (Date.now() < until) {
    const t = daw.transportState();
    if (t.playing && t.beat >= beat) return t.beat;
    daw.sleepMs(40);
  }
  fail("再生位置が " + beat + " 拍に届かない: " + JSON.stringify(daw.transportState()));
}

if (!state().hasFollowActions) fail("前提: フォローアクションがある");
if (!state().followActionsEnabled) fail("前提: 既定は on");

// ---- 書き出しは on / off で同じ ----
daw.exportWavRange(outDir + "/export_on.wav", 0.0, 8.0, 120000);
const dirtyBefore = dirty();
daw.appSetFollowActionsEnabled(false);
if (state().followActionsEnabled) fail("off に切り替わらない");
if (dirty() !== dirtyBefore) fail("切り替えで `*` が変わった");
daw.sleepMs(300);
daw.exportWavRange(outDir + "/export_off.wav", 0.0, 8.0, 120000);

// ---- off で再生: 発火しない。on に戻すと次の境界から ----
// 再生開始から A が 1 周する拍 4 を十分に過ぎた拍 6.5 でも A のまま。そこで on に戻すと、次の境界 = 拍 8 で B へ。
function playOffAndCheckHold(step) {
  daw.appPlay();
  const at = waitBeat(6.5, 20000);
  const row = playingCell();
  daw.log(`[${step}] off: beat=${at} row=${JSON.stringify(row)}`);
  // A は再生開始の拍 0 で撃たれたまま (撃ち直されていない = 拍 4 の境界を A のまま越えた)。
  if (row.state !== 1 || row.playingClipId !== CELL_A || row.launchBeat !== 0) {
    fail(`[${step}] off なのにフォローアクションが発火した: ${JSON.stringify(row)}`);
  }
  return at;
}
const heldAt = playOffAndCheckHold("live");
daw.appSetFollowActionsEnabled(true);
// on に戻した時点の再生位置 (実際に engine へ届いたのはこれより前)。次の境界はここから 4 拍以内にある。
const enabledBy = daw.transportState().beat;
// B へ移るまで待つ (負荷で poll が遅れても、移った拍は engine が記録しているので判定は時間に依らない)。
let moved = playingCell();
for (const until = Date.now() + 20000; moved.playingClipId !== CELL_B && Date.now() < until; ) {
  daw.sleepMs(40);
  moved = playingCell();
}
daw.log(`[live] on: enabledBy=${enabledBy} row=${JSON.stringify(moved)}`);
if (moved.state !== 1 || moved.playingClipId !== CELL_B) fail("on に戻しても B へ移らない: " + JSON.stringify(moved));
// 移った拍は A を撃った拍 0 からの格子 (4 拍ごと) の上で、on に戻す前に確かめた拍より後、戻してから 1 周以内。
// 見送ったぶん (拍 4) をまとめて撃っていれば、ここが格子の外か、確かめた拍より前になる。
const at = moved.launchBeat;
if (at % CELL_BEATS !== 0 || at <= heldAt || at > enabledBy + CELL_BEATS) {
  fail(`B へ移った拍 ${at} が次の境界でない (off を確かめた拍 ${heldAt}、on に戻した拍 ${enabledBy})`);
}
daw.appStop();
daw.sleepMs(300);

// ---- off のまま engine を作り直す (デバイスの切り替え = daw_audio の起こし直し) ----
daw.appSetFollowActionsEnabled(false);
daw.setAudioDevice({ driver: "synthetic", device: "ramp" });
daw.sleepMs(500);
const back = daw.setAudioDevice({});
if (back === null) fail("既定のデバイスへ戻せない");
daw.sleepMs(1000);
if (state().followActionsEnabled) fail("engine を作り直したら GUI の表示が on に戻った");
playOffAndCheckHold("respawn");
daw.appStop();
daw.sleepMs(200);

daw.log("follow_global: ok");
