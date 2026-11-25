//! r.md #160: 実物の VST3 (MeldaProduction MCenter) で、plugin の中だけの変更を「未保存」として扱う仕組みの
//! うち、実物でしか分からないところを見る — シナリオと判定は `tests/scripts/plugin_dirty_real.js`:
//!
//! - 開いただけ / 再生しただけでは未保存にならない。
//! - 触っていなければ、閉じる前に取り直した state は読み込み直後に取った基準と同じ (確認なしで閉じる)。
//! - 報告なしで値が変わると、閉じる前の比較が見つけて保存を聞く。
//!
//! 合成の state で同じ流れを見る検査は `tests/app_state/plugin_dirty.rs`。ここは「実物の plugin が返す state が
//! 取るたびに同じで、値を変えると変わる」という前提そのものを確かめる。
//!
//! 回し方は `make test-real-vst3` (env と `-- --ignored` の扱いは `real_vst3_support` の module doc)。

// 音源の WAV / 書き出しの読み戻しの helper はこの検査では使わない (音を見ない)。
#[allow(dead_code)]
mod real_vst3_support;

use real_vst3_support as rv;

/// 実 VST3 の場所を渡す環境変数。開発機の絶対パスを既定値にしない (理由は `pdc_real_vst3.rs` の同名定数)。
const MCENTER_ENV: &str = "DAW01_TEST_VST3_MCENTER";

#[test]
#[ignore = "実物の VST3 (MeldaProduction MCenter) が要る — make test-real-vst3 で回す"]
fn real_plugin_state_is_compared_before_closing() {
    let plugin = rv::vst3_from_env(MCENTER_ENV, "MeldaProduction MCenter");
    let dir = tempfile::tempdir().expect("tempdir");
    // 書き出しはしない (`--output` は harness の共通引数)。
    let unused_output = dir.path().join("unused.wav");
    let args = [rv::path_arg("plugin", &plugin)];
    rv::run_script(env!("CARGO_BIN_EXE_daw_gui"), "plugin_dirty_real.js", &unused_output, &args);
}
