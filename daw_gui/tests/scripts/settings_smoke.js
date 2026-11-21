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

// ---- 5. デバイスを替える: 「オーディオ」のページが読む state --------------------
// どの PC にもあるもの (合成のデバイス・この PC に無いドライバー) だけを使う。切り替えそのもの
// (engine が回る・再生位置が進む) は audio_device_switch.js が見る。ここはページが頼っている形。
daw.appSetSettingsCategory("audio");

// 入力を「使わない」: ch の一覧は残り、使っている範囲だけが 0 本になる (ページはそれを選択として出す)。
let info = daw.setAudioDevice({ driver: "synthetic", device: "ramp", inputCount: 0 });
s = state();
expectEq(s.audioDevice.problem, null, "合成のデバイスは設定どおりに開く");
expectEq(s.audioDevice.config.input_channels.count, 0, "「使わない」は count 0 の範囲");
expectEq(s.saved.audio_device.input_channels.count, 0, "「使わない」が保存される");
expectEq(info.activeInputs.count, 0, "使っている入力は 0 本");
if (info.inputChannels.length < 1) {
  throw new Error("入力を使わなくても ch の一覧は残る: " + JSON.stringify(info));
}
// ch の番号は 0 から連続 (出力の組・入力の範囲の dropdown はこれを前提に並べる)。
for (const list of [info.outputChannels, info.inputChannels]) {
  list.forEach((c, i) => expectEq(c.index, i, "ch の番号は 0 から連続"));
}
// 選べる候補があるデバイス (dropdown になる)。実際の値は候補の中にある。
if (info.sampleRates.indexOf(info.sampleRate) < 0 || info.bufferSizes.indexOf(info.bufferFrames) < 0) {
  throw new Error("実際のレート / バッファが候補に無い: " + JSON.stringify(info));
}

// この PC に無いドライバー: OS 標準の既定デバイスで動き、理由が出る。選んだ設定は消えない。
info = daw.setAudioDevice({
  driver: "asio",
  id: "{00000000-0000-0000-0000-000000000000}",
  device: "No Such ASIO Driver",
});
s = state();
expectEq(info.driver, "system", "開けなければ OS 標準で動く");
expectEq(s.audioDevice.problem, info.fallbackReason, "ページに出す理由 = engine が報告した理由");
if (!s.audioDevice.problem || info.outputName.length < 1) {
  throw new Error("理由と、実際に開いているデバイスの名前がある: " + JSON.stringify(info));
}
expectEq(s.audioDevice.config.driver, "Asio", "設定は ASIO のまま");
expectEq(s.audioDevice.config.asio_driver.name, "No Such ASIO Driver", "選んだドライバーは選ばれたまま");
expectEq(s.saved.audio_device.asio_driver.name, "No Such ASIO Driver", "選んだドライバーが保存されている");
expectEq(s.audioDevice.connected, true, "起こし直した engine と繋がっている");

// 切り替えた直後にページを出し直しても、一覧は新しい engine から届く。
daw.appSetSettingsCategory("theme");
daw.appSetSettingsCategory("audio");
for (let i = 0; i < 250 && state().audioDevice.listing; i++) {
  daw.sleepMs(20);
}
expectEq(state().audioDevice.listing, false, "切り替えた後も一覧が届く");

daw.setAudioDevice({});
s = state();
expectEq(s.audioDevice.problem, null, "OS 標準の既定へ戻したら理由は消える");
expectEq(s.saved.audio_device.driver, "System", "戻したことが保存される");

daw.log("settings_smoke: ok");
