//! 設定 window (r.md #156) の script API と永続の smoke test。
//!
//! `tests/scripts/settings_smoke.js` を `daw_gui --script` で headless 実行する。JS は設定 window の
//! 開閉 / カテゴリ / 各ページの項目を人の操作と同じ `AppEvent` で動かし、アプリの state と
//! `app_config.json` に保存された内容の両方を assert する (exit code 0 で pass)。
//!
//! daw_audio まで起こして繋ぐので、「オーディオ」のページを出したときにデバイスの一覧が engine から
//! 届くこと (列挙の往復) もここで見る。

use std::path::Path;

#[test]
fn settings_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("settings_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "settings_smoke.js failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }
}
