//! r.md #156: オーディオデバイスの切り替えの headless smoke
//! (`tests/scripts/audio_device_switch.js` を `daw_gui --script` で実行)。
//!
//! daw_gui 本体を subprocess 起動し daw_audio / daw_plugin_host まで spawn する
//! (= `CARGO_BIN_EXE_daw_gui`、`make test-nolaunch` の対象外)。切り替え = daw_audio の起こし直しなので、
//! 実プロセスでないと確かめられない (デバイス情報・engine のレートとバッファ・再生位置・既定へ戻る経路)。
//!
//! 引数なしで回すので、開くのは OS 標準の既定の出力と合成デバイスだけ (ASIO ドライバーは開かない。開く形は
//! script の冒頭)。窓なしの script は無音。

use std::path::Path;

#[test]
fn audio_device_switch_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("audio_device_switch.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "audio_device_switch.js failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }
}
