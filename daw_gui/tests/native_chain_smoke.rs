//! M-5: 内蔵 device (r.md #129) を **daw_audio まで通して** 鳴らす end-to-end
//! (`docs/plan_rack_native_devices.md` §15.6)。
//!
//! `tests/scripts/native_chain_smoke.js` を `daw_gui --script` で走らせる (daw_audio を spawn して
//! audio device を開く = `make test` の起動あり target)。script が確かめるのは:
//!
//! 1. 組み込み Comp (thr -40 / ratio 10) の GR が 3 dB を超える
//! 2. bypass 中の Comp で Listen を押すと有効化され、master のピークが検出信号に変わる
//! 3. フェーダー後の Limiter (ceiling -6 dB) に +6 dB を入れても、書き出しも再生中も -5.9 dBFS 以下
//!
//! ここ (harness) は script が書き出した 2 本の WAV を突き合わせて、**Listen が書き出しに乗らない**
//! (Listen 中の書き出し == Listen を外した書き出し) ことを確かめる。

use std::io::Write;
use std::path::Path;

/// 16-bit PCM mono のサイン波 WAV (ヘッダ 44 byte + サンプル)。
fn write_sine_wav(path: &Path, sample_rate: u32, secs: f64, freq: f64, amp: f64) {
    let frames = (f64::from(sample_rate) * secs) as u32;
    let data_bytes = frames * 2;
    let mut buf: Vec<u8> = Vec::with_capacity(44 + data_bytes as usize);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_bytes.to_le_bytes());
    for i in 0..frames {
        let t = f64::from(i) / f64::from(sample_rate);
        let s = ((t * freq * std::f64::consts::TAU).sin() * amp * f64::from(i16::MAX)) as i16;
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::File::create(path).expect("create wav").write_all(&buf).expect("write wav");
}

fn read_samples(path: &Path) -> Vec<f32> {
    let reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    reader.into_samples::<f32>().map(|s| s.expect("sample")).collect()
}

#[test]
fn native_chain_smoke_via_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wav = dir.path().join("tone_2k.wav");
    // 8 秒 = 120 BPM で 16 拍。2 kHz は検出フィルタ 150 Hz の帯域の外 (Listen で大きく下がる)。
    write_sine_wav(&wav, 48_000, 8.0, 2_000.0, 0.9);

    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("native_chain_smoke.js");
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
            "native_chain_smoke.js failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let on = read_samples(&dir.path().join("listen_on.wav"));
    let off = read_samples(&dir.path().join("listen_off.wav"));
    assert!(on.iter().any(|s| s.abs() > 1e-3), "書き出しが無音");
    assert_eq!(on.len(), off.len(), "Listen の有無で書き出しの長さが変わる");
    let first_diff = on.iter().zip(&off).position(|(a, b)| a.to_bits() != b.to_bits());
    assert_eq!(first_diff, None, "Listen 中の書き出しが Listen を外した書き出しと一致しない (Listen が書き出しに乗った)");
}
