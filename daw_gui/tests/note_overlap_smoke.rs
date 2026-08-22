// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! MIDI ノート非重なり解消の AppData-driven JS smoke test。
//!
//! flow:
//! 1. `tests/scripts/note_overlap_smoke.js` を `daw_gui --script` で実行
//! 2. JS は `appLoadSongJson` / `addNote` / `setNotePositionsJson` /
//!    `inspectSongJson` を呼び、同一ピッチの重なりが last-note-wins で解消され
//!    (異なるピッチは和音として共存)、不変条件が保たれることを assert
//! 3. exit code 0 で pass、JS error で 1
//!
//! 純ロジック (`resolve_note_overlaps`) の単体テストとは別に、AppEvent ハンドラが
//! 実際に解消を呼んでいる (= 配線) ことを end-to-end で検証する。plugin / VST3 不要。

use std::path::Path;

#[test]
fn note_overlap_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("note_overlap_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "note_overlap_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
