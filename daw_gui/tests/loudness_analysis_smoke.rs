// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #54: 範囲ラウドネス解析の **プロセス横断** end-to-end smoke test。
//!
//! flow:
//! 1. `tests/scripts/loudness_analysis_smoke.js` を `daw_gui --script` で実行
//! 2. JS は `loadSongFromObject` で engine へ song を届け、
//!    `analyzeLoudnessJson(startBeat, endBeat)` で **拍** の範囲を解析させる
//! 3. daw_audio が `RenderWindow::resolve` で拍→サンプル換算し、`render_loop` を
//!    `LoudnessSink` で走らせ、`LoudnessAnalysisComplete` を返す
//! 4. 戻ってきたスカラー (総フレーム数 / 測定長 / 無音の扱い) を assert
//!
//! プラグイン / オーディオファイル不要 (空 song = 無音) なので外部依存なし。
//! ここで捕まえるのは **配線と窓の算術** — 「解析コマンドが engine に届かない」
//! 「範囲の拍→サンプル換算がずれる」「完了通知が返らない」系の回帰。
//! 測定そのものの正しさ (BS.1770 / Tech 3341・3342) は `common::loudness` /
//! `common::loudness_report` の適合テストが持つ。

use std::path::Path;

#[test]
fn loudness_analysis_smoke_via_script() {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("loudness_analysis_smoke.js");

    let output = std::process::Command::new(exe)
        .args(["--script", script.to_str().unwrap()])
        .output()
        .expect("spawn daw_gui");

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "loudness_analysis_smoke.js failed: status={:?}\nstdout:\n{stdout}\n\
             stderr:\n{stderr}",
            output.status,
        );
    }
}
