//! PR4 sidechain integration smoke test: 実 VST3 plugin (MeldaProduction
//! MCompressor) を sidechain 入力付きでロードした状態でも export pipeline
//! 全体が落ちずに WAV を生成し、 両 track の音が master に届き、 MCompressor が
//! 実際に音を処理している (= 経路に居る) ことを確認する。
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
//!
//! 回し方は `make test-real-vst3` (env と `-- --ignored` の扱いは `real_vst3_support` の module doc)。

mod real_vst3_support;

use real_vst3_support as rv;

/// 実 VST3 の場所を渡す環境変数 (対象は **MeldaProduction MCompressor**)。
///
/// **開発機の絶対パスを既定値にしない** — 理由は `pdc_real_vst3.rs` の同名定数と同じ。
const MCOMPRESSOR_ENV: &str = "DAW01_TEST_VST3_MCOMPRESSOR";

/// Track 1 (sidechain の元) の音: 曲頭に **負** の矩形 burst。
const TRIGGER_AMP: f32 = -0.5;
const TRIGGER_FRAMES: usize = 100;
/// Track 2 (MCompressor) の音: 曲頭から **正** に立ち上がるサイン波 (半周期 ≈ 218 sample なので、
/// trigger の区間ではずっと正)。符号を分けて、曲頭の負の山は Track 1 からしか来ないようにする。
///
/// 振幅は既定の MCompressor の threshold を越える大きさにする。出力が入力より小さいことが
/// 「MCompressor が経路に居る」唯一の証拠になる (素通りなら同じ振幅が出る)。2026-10 の実測:
/// 0.9 → 0.544 (約 -4.4 dB)。0.25 は素通り (threshold の下)。
const BASS_AMP: f32 = 0.9;
const BASS_HZ: f32 = 110.0;

#[test]
#[ignore = "実物の VST3 (MeldaProduction MCompressor) が要る — make test-real-vst3 で回す"]
fn sidechain_real_mcompressor_pipeline_does_not_crash() {
    let plugin = rv::vst3_from_env(MCOMPRESSOR_ENV, "MeldaProduction MCompressor");

    let dir = tempfile::tempdir().expect("tempdir");
    let frames = rv::SOURCE_SR as usize;
    let trigger_wav = dir.path().join("trigger.wav");
    let mut trigger = vec![0.0f32; frames];
    trigger[..TRIGGER_FRAMES].fill(TRIGGER_AMP);
    rv::write_source_wav(&trigger_wav, &trigger);
    let bass_wav = dir.path().join("bass.wav");
    let bass: Vec<f32> = (0..frames)
        .map(|i| BASS_AMP * (std::f32::consts::TAU * BASS_HZ * i as f32 / rv::SOURCE_SR as f32).sin())
        .collect();
    rv::write_source_wav(&bass_wav, &bass);

    let out = dir.path().join("out.wav");
    let mut args = rv::source_shape_args(frames);
    args.push(rv::path_arg("trigger", &trigger_wav));
    args.push(rv::path_arg("bass", &bass_wav));
    // プラグインの場所は script にも渡す (JS 側にも絶対パスを持たせない)。
    args.push(rv::path_arg("plugin", &plugin));
    rv::run_script(env!("CARGO_BIN_EXE_daw_gui"), "pdc_mcompressor_sidechain.js", &out, &args);

    // 書き出しが最後まで走り (= run_script が成功)、両 track が master に届き、Track 2 が
    // MCompressor で圧縮されていること。閾値は実測 (BASS_AMP の doc) との中点。
    let (l, r) = rv::read_export(&out);
    let quarter = frames / 4;
    for (name, ch) in [("L", &l), ("R", &r)] {
        let head_min = ch[..TRIGGER_FRAMES.min(ch.len())].iter().copied().fold(0.0f32, f32::min);
        let tail = rv::peak(ch, quarter..3 * quarter);
        eprintln!("{name}: 曲頭の負の山 {head_min:.4} / Track 2 の区間の山 {tail:.4} (入力 {BASS_AMP})");
        assert!(
            head_min < 0.5 * TRIGGER_AMP,
            "{name}: 曲頭に Track 1 (sidechain の元) の負の burst が無い: {head_min:.4}",
        );
        assert!(
            tail > 0.3 * BASS_AMP,
            "{name}: Track 2 (MCompressor + sidechain) の音が master に届いていない: {tail:.4}",
        );
        assert!(
            tail < 0.8 * BASS_AMP,
            "{name}: Track 2 が圧縮されていない ({tail:.4} / 入力 {BASS_AMP}) — MCompressor が経路に居ない",
        );
    }
}
