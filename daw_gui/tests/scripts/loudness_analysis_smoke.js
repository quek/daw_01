// r.md #54: 範囲ラウドネス解析の **プロセス横断** smoke test。
// `daw_gui --script` で headless 実行。exit 0 で pass、JS error で 1。
//
// 検証するのは「daw_gui → daw_audio → freewheel 走査 → レポート → daw_gui」の
// 往復が実際に成立すること:
//   loadSongFromObject で engine に song を届ける
//   → analyzeLoudnessJson(startBeat, endBeat) が拍で範囲を送る
//   → engine が RenderWindow::resolve で拍→サンプル換算し、render_loop を
//      LoudnessSink で走らせ、LoudnessAnalysisComplete を返す
//   → スカラーが JSON で戻る
//
// 音源 (プラグイン / オーディオファイル) 無しの空 song なので中身は無音。
// よって Integrated は null (= -inf) が正解で、ここで見るのは **配線と窓の算術**:
//   - エラーにならない
//   - 測定長 / 総フレーム数が拍から正しく換算されている
//   - 無音の範囲が「クリップ 0 / ピーク null」として返る
// 測定そのものの正しさは common::loudness / loudness_report の適合テストが持つ。

var song = {
  bpm: 120.0,
  time_sig: [4, 4],
  length_beats: 32.0,
  tracks: [
    { id: 1, name: "T", volume: 1.0, pan: 0.0, muted: false, solo: false, devices: [] },
  ],
};

daw.loadSongFromObject(song);
daw.sleepMs(300);

// 8 拍 – 24 拍 = 16 拍。120 BPM なので 8 秒 = 384000 frames @48k。
var json = daw.analyzeLoudnessJson(8.0, 24.0, 60000);
var r = JSON.parse(json);

function fail(msg) {
  throw new Error("loudness_analysis_smoke: " + msg + " (report=" + json + ")");
}

if (r.sample_rate <= 0) fail("sample_rate が不正");

// 拍→サンプル換算は engine 側 SSoT。16 拍 @120BPM = 8 秒。
var expected_frames = Math.round((16.0 * 60.0 / 120.0) * r.sample_rate);
if (Math.abs(r.total_frames - expected_frames) > 2) {
  fail("total_frames expected ~" + expected_frames + " got " + r.total_frames);
}
if (Math.abs(r.measured_secs - 8.0) > 0.05) {
  fail("measured_secs expected 8.0 got " + r.measured_secs);
}
// 無音なので到達値は無い (JSON では null)。
if (r.integrated_lufs !== null) fail("無音なのに Integrated が出ている");
if (r.true_peak_dbtp !== null) fail("無音なのに True Peak が出ている");
if (r.clipped_samples !== 0) fail("無音なのにクリップしている");

// 全曲 (start >= end で range = None) も通ること。32 拍 = 16 秒。
var full = JSON.parse(daw.analyzeLoudnessJson(0.0, 0.0, 60000));
if (Math.abs(full.measured_secs - 16.0) > 0.05) {
  fail("全曲 measured_secs expected 16.0 got " + full.measured_secs);
}
