// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! clip rename の AppData-driven JS smoke test。
//!
//! flow:
//! 1. `tests/scripts/clip_rename_smoke.js` を `daw_gui --script` で実行
//! 2. JS は `appLoadSongJson` / `dispatchRenameClip` / `inspectSongJson` を
//!    呼び、 rename / overwrite / trim / 空文字 no-op を assert
//! 3. exit code 0 で pass、 JS error で 1
//!
//! plugin / VST3 不要なので外部依存なし。 clip rename の commit ロジック
//! (trim + 空文字 no-op) が regression したら CI で即検出できる。

use std::path::Path;

#[test]
fn clip_rename_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("clip_rename_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "clip_rename_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
