//! `J` (Glue) の**焼き込みが音を変えない**ことを実機経路で確かめる
//! (`docs/plan_glue_bake.md` §3)。
//!
//! 構造 (クリップが 1 つになった / event が 1 つになった) は
//! `split_glue_smoke` が見る。ここで見るのは **鳴る音**:
//!
//! 1. 440Hz のサイン波 WAV を書き、それを 2 つのクリップに分けて並べた song を作る。
//!    トラックのフェーダーは **0.5 (= -6dB)** にしておく。
//! 2. `analyzeLoudnessJson` で結合前のラウドネスを測る。
//! 3. `J` で結合 (= offline render → 1 clip / 1 event) して、同じ範囲をもう一度測る。
//! 4. 両者が一致すること。
//!
//! フェーダーを 0.5 にしてあるのが肝で、**焼き込みがフェーダーまで焼いてしまうと
//! 再生時にもう一度掛かって -6dB ずれる** ため、この 1 本で二重適用が捕まる
//! (ランチャー行の主導権が残ったまま焼く / 無音を焼く、も同時に落ちる)。

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// ディレクトリ直下のファイル一覧 (無ければ空)。テストが増やしたぶんだけ消すため。
fn snapshot_dir(dir: Option<&Path>) -> HashSet<PathBuf> {
    let Some(dir) = dir else {
        return HashSet::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return HashSet::new();
    };
    entries.filter_map(|e| e.ok().map(|e| e.path())).collect()
}

/// 16-bit PCM mono の WAV を書く。dev-dependency を増やさないため手書き
/// (ヘッダ 44 byte + サンプル)。
///
/// 振幅は先頭から末尾へ **単調に増える** (`amp` は最大値)。定常波だと時間位置が
/// ずれて焼かれてもラウドネスもピークも変わらず、「ずれ」を検出できない。
fn write_sine_wav(path: &Path, sample_rate: u32, secs: f64, freq: f64, amp: f64) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
    let frames = (f64::from(sample_rate) * secs) as u32;
    let data_bytes = frames * 2; // mono / 16-bit
    let mut buf: Vec<u8> = Vec::with_capacity(44 + data_bytes as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // channels
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for i in 0..frames {
        let t = f64::from(i) / f64::from(sample_rate);
        // 0.15 → 1.0 の直線エンベロープ。位置がずれれば測定値が動く。
        let env = 0.15 + 0.85 * f64::from(i) / f64::from(frames.max(1));
        let v = (t * freq * std::f64::consts::TAU).sin() * amp * env;
        #[allow(clippy::cast_possible_truncation)]
        let s = (v * f64::from(i16::MAX)) as i16;
        buf.extend_from_slice(&s.to_le_bytes());
    }
    let mut f = std::fs::File::create(path).expect("create wav");
    f.write_all(&buf).expect("write wav");
}

#[test]
fn glue_bake_keeps_audio_identical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wav = dir.path().join("sine.wav");
    // 2 秒 = 120BPM で 4 拍。前半 2 拍 / 後半 2 拍の 2 クリップに割って並べる。
    write_sine_wav(&wav, 48_000, 2.0, 440.0, 0.5);

    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("glue_bake_parity.js");

    // 未保存プロジェクトの焼き込み先は **ユーザーのアプリデータ**
    // (`%LOCALAPPDATA%/daw_01/bounce_cache`)。テストの残骸を置いていかないよう、
    // 実行前後の差分を後で消す (実機の bounce 結果は消さない)。
    let cache = std::env::var_os("LOCALAPPDATA")
        .map(|p| Path::new(&p).join("daw_01").join("bounce_cache"));
    let before = snapshot_dir(cache.as_deref());

    let output = std::process::Command::new(exe)
        .args([
            "--script",
            script.to_str().unwrap(),
            "--arg",
            &format!("wav={}", wav.display()),
        ])
        .output()
        .expect("spawn daw_gui");

    for path in snapshot_dir(cache.as_deref()).difference(&before) {
        let _ = std::fs::remove_file(path);
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        panic!(
            "glue_bake_parity.js failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }
}
