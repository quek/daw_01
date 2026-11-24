//! r.md #161: フォローアクションの全体 on/off を **daw_audio の再生と書き出しまで通して** 確かめる end-to-end
//! (daw_audio を spawn して audio device を開く = `make test` の起動あり target)。
//!
//! `tests/scripts/follow_global.js` が、440 Hz のセル A (1 周したら「次」) と 880 Hz のセル B を置いた行で
//! 「off の間は発火しない / on に戻すと次の境界から / engine を作り直しても off のまま」を engine の走行状態で
//! 確かめ、on と off の両方で書き出す。ここ (harness) は WAV の区間ごとの音程を測り、**書き出しは on / off の
//! どちらでもフォローアクションが効いている** (前半 440 Hz・後半 880 Hz) ことを見る。

use std::io::Write;
use std::path::Path;

const SR: u32 = 48_000;

/// 16-bit PCM mono のサイン波 WAV。
fn write_sine_wav(path: &Path, secs: f64, freq: f64) {
    let frames = (f64::from(SR) * secs) as u32;
    let data_bytes = frames * 2;
    let mut buf: Vec<u8> = Vec::with_capacity(44 + data_bytes as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&SR.to_le_bytes());
    buf.extend_from_slice(&(SR * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for i in 0..frames {
        let t = f64::from(i) / f64::from(SR);
        let s = ((t * freq * std::f64::consts::TAU).sin() * 0.5 * f64::from(i16::MAX)) as i16;
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::File::create(path).expect("create wav").write_all(&buf).expect("write wav");
}

/// 書き出した WAV の左チャンネルと、そのサンプルレート (書き出しは開いているデバイスのレートで出るので、
/// 48 kHz と決め打ちしない)。
fn left_channel(path: &Path) -> (Vec<f32>, f64) {
    let reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let (channels, rate) = (usize::from(reader.spec().channels.max(1)), f64::from(reader.spec().sample_rate));
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.expect("sample")).collect();
    (samples.chunks(channels).map(|c| c[0]).collect(), rate)
}

/// `[a, b)` 秒の音程 (Hz、上向きの零交差の数 / 秒)。区間の端 (セルの切り替わり) は呼び出し側で避ける。
fn pitch_hz(left: &[f32], rate: f64, a: f64, b: f64) -> f64 {
    let (i0, i1) = ((a * rate) as usize, ((b * rate) as usize).min(left.len()));
    let w = &left[i0..i1];
    let ups: Vec<usize> = (1..w.len()).filter(|&i| w[i - 1] < 0.0 && w[i] >= 0.0).collect();
    assert!(ups.len() > 10, "区間 {a}-{b} 秒がほぼ無音");
    (ups.len() - 1) as f64 * rate / (ups[ups.len() - 1] - ups[0]) as f64
}

#[test]
fn follow_actions_global_switch_holds_live_playback_but_not_the_export() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (low, high) = (dir.path().join("tone_440.wav"), dir.path().join("tone_880.wav"));
    write_sine_wav(&low, 2.0, 440.0);
    write_sine_wav(&high, 2.0, 880.0);

    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("follow_global.js");
    let output = std::process::Command::new(exe)
        .args([
            "--script",
            script.to_str().unwrap(),
            "--arg",
            &format!("low={}", low.display()),
            "--arg",
            &format!("high={}", high.display()),
            "--arg",
            &format!("dir={}", dir.path().display()),
        ])
        .output()
        .expect("spawn daw_gui");
    if !output.status.success() {
        panic!(
            "follow_global.js failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    // 書き出しは on / off のどちらでも、前半 (0-2 秒) が A = 440 Hz、後半 (2-4 秒) が B = 880 Hz。
    for name in ["export_on.wav", "export_off.wav"] {
        let (left, rate) = left_channel(&dir.path().join(name));
        let (first, second) = (pitch_hz(&left, rate, 0.3, 1.7), pitch_hz(&left, rate, 2.3, 3.7));
        assert!((first - 440.0).abs() < 5.0, "{name}: 前半 {first} Hz が A (440 Hz) でない");
        assert!((second - 880.0).abs() < 5.0, "{name}: 後半 {second} Hz が B (880 Hz) でない — フォローアクションが効いていない");
    }
}
