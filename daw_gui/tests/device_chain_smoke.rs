//! 単一デバイスチェーン (`docs/plan_linear_chain.md`) の end-to-end JS smoke test。
//!
//! flow:
//! 1. `tests/scripts/device_chain_smoke.js` を `daw_gui --script` で実行
//! 2. JS は `appLoadSongJson` で device 列 (ports 付き) を load し、
//!    `deviceChain(track_id)` で production と同じ load → migration → port 解決
//!    経路を通したチェーンを読み戻して、並び順と各 device の port が保たれている
//!    ことを assert する (役割判定はしない)
//! 3. exit code 0 で pass、 JS error で 1
//!
//! plugin / VST3 不要 (ports を JSON に直接書く) なので外部依存なし。
//! 「D&D で並び替えできない / 音が追従しない」系の回帰 (チェーンの並び・port 喪失)
//! を CI で即検出する。

use std::path::Path;

#[test]
fn device_chain_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("device_chain_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "device_chain_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
