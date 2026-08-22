// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! Phase 1 PR7 follow-up: Split / Glue の AppData-driven JS smoke test。
//!
//! flow:
//! 1. `tests/scripts/split_glue_smoke.js` を `daw_gui --script` で実行
//! 2. JS は AppData の `appLoadSongJson` / `dispatchSplit` / `dispatchGlue`
//!    / `inspectSongJson` 等の API を呼び、 期待する状態を assert
//! 3. exit code 0 で pass、 JS error で 1
//!
//! plugin / VST3 不要なので外部依存なし。 PR7 の Split / Glue 実装が
//! regression したら CI で即検出できる。

use std::path::Path;

#[test]
fn split_glue_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("split_glue_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "split_glue_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
