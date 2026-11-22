//! r.md #157 (`docs/plan_chord_track.md`): コードトラックの AppData-driven JS smoke test。
//!
//! `tests/scripts/chord_smoke.js` を `daw_gui --script` で実行する (exit code 0 で pass)。
//! script → `AppEvent::Chord` / `ChordFollow` / `ChordTool` → handler → `edit_song` → Song の配線と、
//! 追従の評価を 1 本で通す。plugin 不要。

use std::path::Path;

#[test]
fn chord_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("chord_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!("chord_smoke.js failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", output.status);
    }
}
