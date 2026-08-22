// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! docs/plan_video.md P6.4: Video clip の Split / Glue smoke test。
//!
//! `tests/scripts/split_glue_video_smoke.js` を `daw_gui --script` で
//! 実行し、 video clip でも E (Split) / J (Glue) が audio と同じ挙動を
//! することを assertion で確認する。 plugin / VST3 不要なので外部依存
//! なし、 CI で regression 検出可。

use std::path::Path;

#[test]
fn split_glue_video_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("split_glue_video_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "split_glue_video_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
