// r.md #160: 実物の VST3 で、plugin の中だけの変更を「未保存」として扱う 2 段のうち、実物でしか分からない
// ところを確かめる。daw_gui --script で headless 実行。harness: daw_gui/tests/plugin_dirty_real_vst3.rs
// (--arg plugin=<.vst3 のパス>)。
//
//   1. 開いただけ / 再生しただけでは未保存にならない (読み込みの state 復元や再生で、plugin が
//      「中身が変わった」と報告しても数えない)。
//   2. 触っていなければ確認なしで閉じる (ユーザーの手が届いていない plugin は state を比べない —
//      触っていなくても取るたびに bytes が変わる plugin が多い)。
//   3. plugin の値を **報告なしで** 変えると (インスペクタのツマミ → undo)、閉じる前の比較が見つけて保存を聞く
//      (実物の state が param の値を含んでいて、hash が変わること)。
//
// 窓の中の操作 (ユーザーの入力) は headless では起こせない — 報告の判定は daw_plugin_host の unit test と
// 実機の sign-off で見る。

function fail(msg) {
  throw new Error("plugin_dirty_real.js: " + msg);
}

const args = daw.scriptArgs;
if (!args.plugin) fail("--arg plugin=<.vst3 のパス> が要る");

function activeTab() {
  const tabs = JSON.parse(daw.tabsJson());
  for (let i = 0; i < tabs.length; i++) {
    if (tabs[i].active) return tabs[i];
  }
  fail("アクティブなタブが無い: " + JSON.stringify(tabs));
}

function guard() {
  return JSON.parse(daw.appCloseGuardJson());
}

// plugin 1 つの曲 (plugin の id / port 構成は仮の値)。
const SEED_SONG = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 8.0,
  tracks: [
    {
      id: 1,
      name: "fx",
      volume: 1.0,
      pan: 0.0,
      muted: false,
      solo: false,
      clips: [],
      next_clip_id: 1,
      devices: [
        {
          plugin_id: "plugin-under-test",
          format: "Vst3",
          state: null,
          ports: { has_note_input: false, has_note_output: false, has_audio_input: true, has_audio_output: true },
        },
      ],
    },
  ],
  next_track_id: 2,
};

// アクティブなタブに `song` を開いて plugin を載せ、読み込みと「閉じるときに比べる基準」の取り寄せを待つ。
function openSong(song) {
  daw.loadSongFromObject(song);
  daw.setSlotPlugin(1, 0, "vst3", args.plugin, "");
  daw.waitForPluginLoaded(1, 0, 30000);
  // 基準の取り寄せ (RequestAllStates の往復) が済むまで。
  for (let waited = 0; guard().stateRequests > 0; waited += 100) {
    if (waited > 10000) fail("基準の state の応答が来ない: " + JSON.stringify(guard()));
    daw.sleepMs(100);
  }
  daw.sleepMs(300);
  return activeTab().key;
}

// 仮の曲を一度載せると、読み込みの応答が Song の plugin を実物 (id / port 構成 / aux の数) に書き直す (これは
// Song の変更なので未保存になる — 保存したプロジェクトを開くときには起きない)。書き直された Song を
// 「保存してあったプロジェクト」として使う: 開き直すと実物と一致していて、未保存にならない。
openSong(SEED_SONG);
const PROJECT = JSON.parse(daw.inspectSongJson());
daw.closeTab();

function openProjectWithPlugin() {
  return openSong(PROJECT);
}

// ✕ と同じ経路で閉じ、比較の往復が済むまで待つ。
function closeAndSettle() {
  daw.appCloseTab();
  for (let waited = 0; guard().stateRequests > 0; waited += 100) {
    if (waited > 10000) fail("閉じる前の state の応答が来ない: " + JSON.stringify(guard()));
    daw.sleepMs(100);
  }
}

// ---- 1 + 2: 開いただけ / 再生しただけ → 未保存にならず、確認なしで閉じる -------------------------
let key = openProjectWithPlugin();
if (activeTab().dirty) fail("開いただけで未保存になった");

daw.appPlay();
daw.sleepMs(1500);
daw.appStop();
daw.sleepMs(300);
if (activeTab().dirty) fail("再生しただけで未保存になった");

closeAndSettle();
if (guard().modal !== null) fail("触っていないのに保存を聞かれた: " + JSON.stringify(guard()));
if (activeTab().key === key) fail("触っていないタブが閉じなかった: " + JSON.stringify(JSON.parse(daw.tabsJson())));

// ---- 3: 報告なしで値を変える → 閉じる前の比較が見つける --------------------------------------
key = openProjectWithPlugin();
const params = JSON.parse(daw.pluginParamsJson(1, 0));
let param = null;
for (let i = 0; i < params.length && param === null; i++) {
  const p = params[i];
  if (!p.readonly && !p.hidden && p.automatable && p.max > p.min) param = p;
}
if (param === null) fail("動かせる param が無い: " + JSON.stringify(params).slice(0, 400));
// ツマミを回して (Song の編集) undo する: Song は開いた時点へ戻る (未保存の印なし) が、host は書き戻す値を
// 持たないので plugin の値は動いたまま残る = 報告の無い変更。
const moved = Math.abs(param.default - param.max) > (param.max - param.min) / 4 ? param.max : param.min;
daw.appSetPluginParam(1, 0, param.id, moved);
daw.appPlay();
daw.sleepMs(800);
daw.appStop();
daw.sleepMs(300);
daw.appUndo();
daw.sleepMs(300);
if (activeTab().dirty) fail("undo で開いた時点へ戻ったのに未保存 (報告なしの変更という前提が作れていない)");

closeAndSettle();
if (guard().modal !== "close") {
  fail("plugin の値 (" + param.name + " → " + moved + ") が変わったのに、閉じる前の比較が見つけなかった: " + JSON.stringify(guard()));
}
if (!activeTab().dirty || activeTab().key !== key) fail("未保存として残っていない: " + JSON.stringify(activeTab()));
daw.log("plugin_dirty_real.js: ok (param " + param.name + ")");
