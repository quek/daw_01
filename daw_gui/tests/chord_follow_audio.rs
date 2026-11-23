//! r.md #157 (`docs/plan_chord_track.md` §3.2): オーディオクリップのコードトラックへの追従を **daw_audio の
//! 書き出しまで通して** 確かめる end-to-end (daw_audio を spawn して audio device を開く = `make test` の起動あり target)。
//!
//! `tests/scripts/chord_follow_audio.js` が 440 Hz のサイン波のクリップにコード C / F / A (2 秒ずつ) を当て、
//! 置いただけ / 素材が Am を弾いている / コード列を書き換えた / 曲全体のスイッチ Off の 4 通りで書き出す。ここ (harness) は WAV の区間ごとの
//! 音程 (零交差の数) を測り、平行移動の半音と突き合わせる。

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

/// 書き出した WAV の左チャンネル。
fn left_channel(path: &Path) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let channels = usize::from(reader.spec().channels.max(1));
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.expect("sample")).collect();
    samples.chunks(channels).map(|c| c[0]).collect()
}

/// `[a, b)` 秒の音程 (Hz、上向きの零交差の数 / 秒)。区間の端 (コードの切り替わり) は呼び出し側で避ける。
fn pitch_hz(left: &[f32], a: f64, b: f64) -> f64 {
    let (i0, i1) = ((a * f64::from(SR)) as usize, ((b * f64::from(SR)) as usize).min(left.len()));
    let w = &left[i0..i1];
    let ups: Vec<usize> = (1..w.len()).filter(|&i| w[i - 1] < 0.0 && w[i] >= 0.0).collect();
    assert!(ups.len() > 10, "区間 {a}-{b} 秒がほぼ無音");
    (ups.len() - 1) as f64 * f64::from(SR) / (ups[ups.len() - 1] - ups[0]) as f64
}

/// 区間 (C / F / A の 2 秒ずつ) ごとの、440 Hz からの半音。
fn semitones(path: &Path) -> Vec<f64> {
    let left = left_channel(path);
    (0..3).map(|i| {
        let start = f64::from(i) * 2.0;
        12.0 * (pitch_hz(&left, start + 0.5, start + 1.7) / 440.0).log2()
    }).collect()
}

#[test]
fn chord_follow_moves_audio_clips_in_the_export() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wav = dir.path().join("tone_a4.wav");
    write_sine_wav(&wav, 6.0, 440.0);

    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("chord_follow_audio.js");
    let output = std::process::Command::new(exe)
        .args([
            "--script",
            script.to_str().unwrap(),
            "--arg",
            &format!("wav={}", wav.display()),
            "--arg",
            &format!("dir={}", dir.path().display()),
        ])
        .output()
        .expect("spawn daw_gui");
    if !output.status.success() {
        panic!(
            "chord_follow_audio.js failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let check = |name: &str, expected: [f64; 3]| {
        let got = semitones(&dir.path().join(name));
        let ok = got.iter().zip(expected).all(|(g, e)| (g - e).abs() < 0.2);
        assert!(ok, "{name}: 区間ごとの半音 {got:?} が {expected:?} にならない");
    };
    // r.md #159: 和音を弾いていない素材にコードを置いただけでは動かない。
    check("follow_none.wav", [0.0, 0.0, 0.0]);
    // 素材が Am を弾いている → C +3 / F -4 / A 0 (最短の向き)。
    check("follow_am.wav", [3.0, -4.0, 0.0]);
    // 和音を弾いていない素材は、書き換えたときだけ書き換えの差 (+2) だけ動く。
    check("follow_rewritten.wav", [2.0, 2.0, 2.0]);
    // 曲全体のスイッチを切ると書いた音。
    check("follow_off.wav", [0.0, 0.0, 0.0]);
}
