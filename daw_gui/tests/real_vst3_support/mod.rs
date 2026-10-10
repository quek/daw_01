//! 実物の VST3 を読む検査 (`pdc_real_vst3` / `sidechain_real_vst3`) の共通部分。
//!
//! **回し方の正本は `make test-real-vst3`** (`scripts/test_real_vst3.sh`)。商用プラグインなので
//! 検査は `#[ignore]` で既定の `cargo test` / `make test` からは外れ、プラグインの場所を env で
//! 渡して `-- --ignored` を付けたときだけ走る。走らせたのに env が無い / 指す先が無いときは
//! **panic する** — 黙って `return` すると「何も確かめていない」のに緑になる (2026-10 まで、
//! 音源の入れ口が no-op になって WAV が全部無音のまま、この形で誰にも気付かれなかった)。
//!
//! 音源は本番と同じ経路で入れる: ここで WAV を書き、script が Song の `audio_sources` に
//! 絶対パスで登録して audio clip から鳴らす (daw_audio が decode する)。
//!
//! `daw_gui` の起動 (`env!("CARGO_BIN_EXE_daw_gui")`) は**各検査のファイルに残す**。
//! `make test-nolaunch` は「その文字列を含む `tests/*.rs`」を起動する検査と判定するので、
//! ここへ移すと起動する検査が起動しない側に数えられる。

use std::path::{Path, PathBuf};

/// 音源の WAV のサンプルレート。書き出しは engine (audio device) のレートなので、違えば
/// 音源は resample される — 検査の判定は単発の sample ではなく区間で見る。
pub const SOURCE_SR: u32 = 48_000;

/// env が指す実物の .vst3。未設定 / 不在なら理由を付けて panic (上の module doc)。
pub fn vst3_from_env(var: &str, product: &str) -> PathBuf {
    let raw = std::env::var_os(var).unwrap_or_else(|| {
        panic!(
            "{var} が未設定です。{product} (VST 3) の .vst3 のパスを {var} に入れて \
             `make test-real-vst3` で回してください (env なしでは何も確かめられない)。"
        )
    });
    let path = PathBuf::from(raw);
    assert!(path.exists(), "{var} が指す {} が存在しません。", path.display());
    path
}

/// stereo 32-bit float の WAV を書く (L = R = `samples`)。
pub fn write_source_wav(path: &Path, samples: &[f32]) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: SOURCE_SR,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("create source wav");
    for &s in samples {
        w.write_sample(s).expect("write sample");
        w.write_sample(s).expect("write sample");
    }
    w.finalize().expect("finalize source wav");
}

/// 音源の WAV の形を script に渡す `--arg` (`frames` / `sr`)。script は Song の `audio_sources` と
/// clip の長さをこれから組む (WAV の形の持ち主はこちら 1 か所)。WAV のパスは呼び出し側が足す。
pub fn source_shape_args(frames: usize) -> Vec<(&'static str, String)> {
    vec![("frames", frames.to_string()), ("sr", SOURCE_SR.to_string())]
}

/// `--arg` に渡すパス。
pub fn path_arg(key: &'static str, path: &Path) -> (&'static str, String) {
    (key, path.display().to_string())
}

/// `daw_gui --script tests/scripts/<script> --output <output> --arg k=v ...` を走らせる。
/// 失敗したら script の stdout / stderr を付けて panic する。
pub fn run_script(exe: &str, script: &str, output: &Path, args: &[(&str, String)]) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join(script);
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--script").arg(&script).arg("--output").arg(output);
    for (key, value) in args {
        cmd.arg("--arg").arg(format!("{key}={value}"));
    }
    let out = cmd.output().expect("spawn daw_gui");
    assert!(
        out.status.success(),
        "{} failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        script.display(),
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// 書き出しの WAV (stereo 32-bit float) を `(L, R)` で読む。
pub fn read_export(path: &Path) -> (Vec<f32>, Vec<f32>) {
    let reader = hound::WavReader::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(reader.spec().channels, 2, "書き出しは stereo のはず");
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.expect("sample")).collect();
    samples.as_chunks::<2>().0.iter().map(|&[l, r]| (l, r)).unzip()
}

/// `x[range]` の絶対値の最大 (範囲が尽きていれば 0)。
pub fn peak(x: &[f32], range: std::ops::Range<usize>) -> f32 {
    let end = range.end.min(x.len());
    x.get(range.start.min(end)..end).unwrap_or_default().iter().fold(0.0, |m, s| m.max(s.abs()))
}
