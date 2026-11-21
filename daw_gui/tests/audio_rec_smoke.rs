//! r.md #156: オーディオ入力の録音を実プロセスで一周させる (`tests/scripts/audio_rec.js` を
//! `daw_gui --script` で実行)。
//!
//! daw_gui 本体を subprocess 起動し、daw_audio / daw_plugin_host まで spawn する (`CARGO_BIN_EXE_daw_gui`、
//! `make test-nolaunch` の対象外)。デバイスは合成デバイスに替えるので、実機のドライバーは開かない。音も出ない。
//!
//! script が見るもの (GUI の操作と同じ `AppEvent` の経路):
//! - 版 45 (入力 / モニターの無い) プロジェクトを開いただけでは dirty にならない。
//! - 入力設定 → 録音待機 → 録音 → 停止でクリップができ、undo で消え、保存して開き直しても鳴る。
//! - モニターのオフ / 自動 / オンで、マスターに入力が出る・出ない。入力メーター。
//! - 書き出しに入力音が入らない。
//!
//! ここ (Rust 側) が見るもの: 録れた WAV の中身。script は音を読めないので、WAV の場所だけ受け取って開く。

use std::path::Path;

use common::model::{Song, Track};

/// 版 45 のプロジェクトファイルを書く。「入力」と「モニター」は既定値のとき書き出されないので、いまの形式で
/// 保存して版の番号だけ戻せば、版 45 が書いたファイルと同じ中身になる。
fn write_v45_project(path: &Path) {
    let song = Song { tracks: vec![Track { id: 1, name: "Old".into(), ..Track::default() }], ..Song::default() };
    common::project::save(path, &song).expect("save project");
    let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let text = serde_json::to_string(&json).unwrap();
    assert!(!text.contains("\"input\"") && !text.contains("\"monitor\""), "既定値は書き出されない前提");
    json["version"] = 45.into();
    std::fs::write(path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
}

/// script を走らせる。失敗したら stdout / stderr を付けて落とす。stdout を返す。
fn run_script(args: &[(&str, &Path)], extra: &[(&str, &str)]) -> String {
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("audio_rec.js");
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--script", script.to_str().unwrap()]);
    for (k, v) in args {
        cmd.args(["--arg", &format!("{k}={}", v.display())]);
    }
    for (k, v) in extra {
        cmd.args(["--arg", &format!("{k}={v}")]);
    }
    let output = cmd.output().expect("spawn daw_gui");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("audio_rec.js failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}", output.status);
    }
    stdout
}

#[test]
fn 版_45_のプロジェクトは開いただけでは_dirty_にならない() {
    let dir = tempfile::tempdir().expect("tempdir");
    let old = dir.path().join("old").join("old.daw");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    write_v45_project(&old);
    run_script(&[("old", &old)], &[("stage", "old")]);
}
