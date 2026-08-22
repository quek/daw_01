// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #30: トラック複製 (独立 / リンク) の AppData-driven JS smoke test。
//!
//! flow:
//! 1. `tests/scripts/track_duplicate_smoke.js` を `daw_gui --script` で実行
//! 2. JS は `appLoadSongJson` / `duplicateTracks` / `inspectSongJson` を呼び、
//!    group subtree の複製・挿入位置・reparent・独立/リンク content を assert
//! 3. exit code 0 で pass、 JS error で 1
//!
//! plugin / VST3 不要なので外部依存なし。 複製の root/挿入ロジックが regression
//! したら即検出できる (unit test では AppData を組めない部分をカバー)。

use std::path::Path;

#[test]
fn track_duplicate_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("track_duplicate_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "track_duplicate_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
