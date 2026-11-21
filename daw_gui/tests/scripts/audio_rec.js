// r.md #156: オーディオ入力の録音を実プロセスで一周させる。`tests/audio_rec_smoke.rs` が段ごとに呼ぶ:
//
//   target/debug/daw_gui.exe --script daw_gui/tests/scripts/audio_rec.js --arg stage=<段> ...
//
// 成功 = exit 0、失敗 = throw して exit 1 (観測値は例外メッセージに載せる)。
// 操作は GUI と同じ `AppEvent` の経路 (`daw.app*` / `daw.setTrackInput` 等) を通す。headless には
// テレメトリの poller が居ないので、engine の transport は `daw.appTick()` で GUI に観測させる。

function check(cond, msg) {
  if (!cond) {
    throw new Error("r.md #156 audio rec FAILED: " + msg);
  }
}

function activeTab() {
  return JSON.parse(daw.tabsJson()).find((t) => t.active);
}

// ---- 段 "old": 版 45 のプロジェクトを開いただけでは dirty にならない ----------------
function stageOld() {
  daw.appOpenProject(daw.scriptArgs.old);
  // 開いた後の正規化・asset decode・engine への同期が一巡するまで。
  for (let i = 0; i < 20; i++) {
    daw.sleepMs(50);
    daw.appTick();
  }
  const tab = activeTab();
  check(tab.path !== null && tab.path.endsWith("old.daw"), "開けていない: " + JSON.stringify(tab));
  check(!tab.dirty, "開いただけで dirty になった: " + JSON.stringify(tab));
  const song = JSON.parse(daw.inspectSongJson());
  const t = song.tracks[0];
  // 旧ファイルの入力は MIDI、モニターは自動 (どちらも既定値なので書き出されない)。
  check(t.name === "Old", "トラックが読めていない: " + JSON.stringify(t.name));
  check(t.input === undefined || t.input === "Midi", "入力が MIDI でない: " + JSON.stringify(t.input));
  check(t.monitor === undefined || t.monitor === "Auto", "モニターが自動でない: " + JSON.stringify(t.monitor));
}

const stages = { old: stageOld };
const stage = stages[daw.scriptArgs.stage];
check(stage !== undefined, "unknown stage: " + daw.scriptArgs.stage);
stage();
