//! PR4 sidechain integration smoke test: 実 VST3 plugin (MeldaProduction
//! MCompressor) を sidechain 入力付きでロードした状態でも export pipeline
//! 全体が落ちずに WAV を生成することを確認する。
//!
//! 注意: MCompressor は default で external sidechain 入力を「使わない」
//! (= internal detection を使う) 仕様。 そのため wired vs unwired で
//! plugin output が一致しても、 それは本テストの **pipeline が壊れた**
//! 印にはならない (= plugin 設計上の挙動)。 plugin が external sidechain
//! を実際に honor することの検証は plugin の保存 state を経由するか
//! 別 plugin を用意する必要がある (M3 で対応予定)。
//!
//! ここで checking する PR4 不変量:
//! - schedule layer の `compile_schedule` が `SidechainTap` を emit する
//!   (= unit test `sidechain_emits_tap_before_destination_process_track`)
//! - engine の `NodeOp::SidechainTap` ハンドラが source TrackScratch の
//!   signal を `pd.buffer_aux_in` にコピーする
//!   (= unit test `sidechain_tap_copies_source_track_into_plugin_aux_in_buffer`)
//! - plugin_host が `pd.buffer_aux_in` を `LoadedPlugin::process` に渡す
//!   (= 本 integration test の存在自体: plugin 側で aux 入力を持つ
//!   bus arrangement / clap_audio_buffer で setBusArrangements が
//!   通り、 export が正常に完了すること)

use std::path::Path;

const MCOMPRESSOR_PATH: &str =
    "C:/Program Files/Common Files/VST3/MeldaProduction/Dynamics/MCompressor.vst3";

#[test]
fn sidechain_real_mcompressor_pipeline_does_not_crash() {
    if !Path::new(MCOMPRESSOR_PATH).exists() {
        eprintln!("SKIP: {MCOMPRESSOR_PATH} not installed");
        return;
    }

    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("pdc_mcompressor_sidechain.js");

    let tmp = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile()
        .expect("create temp wav");
    let out_path = tmp.path().to_path_buf();
    drop(tmp);

    // wireSidechain=true で実行 (sidechain pipeline 全体を経由)。
    let status = std::process::Command::new(exe)
        .args([
            "--script",
            script.to_str().unwrap(),
            "--output",
            out_path.to_str().unwrap(),
            "--arg",
            "wireSidechain=true",
        ])
        .status()
        .expect("spawn daw_gui");
    assert!(
        status.success(),
        "daw_gui --script (wired sidechain) exited non-zero (status={status:?})"
    );

    // WAV が読み戻せて空でないこと (export 自体が機能していること)。
    let mut reader = hound::WavReader::open(&out_path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 2, "expected stereo WAV");
    let n_samples = reader.duration();
    assert!(
        n_samples > 0,
        "WAV is empty — export pipeline crashed mid-render"
    );

    // Track 1 の trigger (1.0 で 100 sample) と Track 2 の bass (0.5 DC) が
    // master に届いていること (= 全 sample silence では無い)。
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .expect("read float samples"),
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / max))
                .collect::<Result<Vec<_>, _>>()
                .expect("read int samples")
        }
    };
    let max_amp = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    assert!(
        max_amp > 0.01,
        "WAV is all-silent (max_amp={max_amp}) — sidechain pipeline likely lost signal"
    );
}
