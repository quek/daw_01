//! `docs/plan_project_tabs.md` §5.7: プロジェクトタブの headless smoke
//! (`tests/scripts/project_tabs.js` を `daw_gui --script` で実行)。
//!
//! daw_gui 本体を subprocess 起動し daw_audio / daw_plugin_host まで spawn する
//! (= `CARGO_BIN_EXE_daw_gui`、`make test-nolaunch` の対象外)。検証するのは
//! engine 側の「タブ = 独立した transport」 (Q1) で、AppData 単体のテスト
//! (`tests/app_state/project_tabs.rs`) では見えない部分。

use std::path::Path;

#[test]
fn project_tabs_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("project_tabs.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "project_tabs.js failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }
}
