//! clip 表示ラベル導出の AppData-driven JS smoke test。
//!
//! flow:
//! 1. `tests/scripts/clip_label_display_smoke.js` を `daw_gui --script` で実行
//! 2. JS は `appLoadSongJson` / `clipDisplayLabel` / `dispatchRenameClip` を
//!    呼び、 歌詞付き MIDI クリップで「明示名が歌詞より優先」 + rename が表示に
//!    即追従することを assert
//! 3. exit code 0 で pass、 JS error で 1
//!
//! plugin / VST3 不要なので外部依存なし。 歌詞付きクリップを rename しても
//! 表示が歌詞のまま変わらない (#69) が regression したら CI で即検出できる。

use std::path::Path;

#[test]
fn clip_label_display_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("clip_label_display_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "clip_label_display_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
