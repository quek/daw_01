// 設定 window (r.md #156) の script API と永続を検証する。
// `daw_gui --script` で headless 実行。exit code 0 で pass。
//
// 開閉 / カテゴリ / 各ページの項目を、人の操作と同じ AppEvent で動かし、そのたびに
// アプリの state と、app_config.json に実際に保存された内容 (`saved`) の両方を見る。
// (`--script` の起動は隔離した root を使うので、ユーザーの設定ファイルは変わらない。)

function expectEq(actual, expected, label) {
  if (actual !== expected) {
    throw new Error(label + ": expected " + expected + " got " + actual);
  }
}

function state() {
  return JSON.parse(daw.settingsJson());
}

// ---- 1. 開く: 既定のカテゴリは「オーディオ」 --------------------------------
let s = state();
expectEq(s.open, false, "起動時は閉じている");
expectEq(s.category, "audio", "既定のカテゴリ");
expectEq(s.audioDevice.connected, true, "engine がデバイスを開いて報告している");
expectEq(s.audioDevice.listed, false, "開くまではデバイスを列挙しない");

daw.appSetSettingsOpen(true);
s = state();
expectEq(s.open, true, "開いた");
expectEq(s.saved.settings_open, true, "開いたことが保存される");
// 「オーディオ」のページが出たら、デバイスの一覧を engine に取りに行く。
for (let i = 0; i < 250 && !state().audioDevice.listed; i++) {
  daw.sleepMs(20);
}
s = state();
expectEq(s.audioDevice.listed, true, "デバイスの一覧が届く");
expectEq(s.audioDevice.listing, false, "一覧が届いたら取得中ではない");

// ---- 2. カテゴリを替える ------------------------------------------------------
daw.appSetSettingsCategory("theme");
s = state();
expectEq(s.category, "theme", "カテゴリが替わる");
expectEq(s.saved.settings_category, "theme", "カテゴリが保存される");
if (s.themes.indexOf("dark") < 0 || s.themes.indexOf("light") < 0) {
  throw new Error("テーマのページを出したら一覧がある: " + JSON.stringify(s.themes));
}

let threw = false;
try {
  daw.appSetSettingsCategory("midi");
} catch (e) {
  threw = true;
}
expectEq(threw, true, "知らないカテゴリは error");
expectEq(state().category, "theme", "知らないカテゴリでは替わらない");

// ---- 3. 各ページの項目 --------------------------------------------------------
daw.appSetTheme("light");
s = state();
expectEq(s.theme, "light", "テーマが替わる");
expectEq(s.saved.theme, "light", "テーマが保存される");

daw.appSetSettingsCategory("voicevox");
daw.appSetVoicevoxChunkSecs(30);
s = state();
expectEq(s.category, "voicevox", "カテゴリが替わる (2)");
expectEq(s.voicevoxChunkSecs, 30, "塊の長さが替わる");
expectEq(s.saved.voicevox_chunk_secs, 30, "塊の長さが保存される");

daw.appSetSettingsCategory("audio");
const before = state().renderAhead;
daw.appSetRenderAhead(!before);
s = state();
expectEq(s.renderAhead, !before, "先読みが替わる");
expectEq(s.saved.render_ahead, !before, "先読みが保存される");
expectEq(s.saved.settings_category, "audio", "カテゴリが保存される (2)");

// ---- 4. 閉じる: カテゴリは覚えたまま ------------------------------------------
daw.appSetSettingsCategory("voicevox");
daw.appSetSettingsOpen(false);
s = state();
expectEq(s.open, false, "閉じた");
expectEq(s.saved.settings_open, false, "閉じたことが保存される");
expectEq(s.saved.settings_category, "voicevox", "閉じてもカテゴリは残る");
daw.appSetSettingsOpen(true);
expectEq(state().category, "voicevox", "開き直すと同じカテゴリ");

daw.log("settings_smoke: ok");
