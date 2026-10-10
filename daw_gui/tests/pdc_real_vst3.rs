//! PDC integration test: 実 VST3 plugin (MeldaProduction MCenter) を
//! load して、 track 間の位相揃いを WAV 出力で確認する。
//!
//! flow:
//! 1. ここで短い矩形パルス (曲頭) の WAV を書く
//! 2. `tests/scripts/pdc_mcenter.js` を `daw_gui --script` で実行。JS が track A (plugin 無し) と
//!    track B (MCenter) に同じ WAV の audio clip を置き、MCenter の latency の報告が
//!    [`MCENTER_LATENCY`] で届いたことを確かめてから `daw.exportWav` で master を書き出す。
//!    その後少し再生して、MCenter が render の経路に居る (plugin が呼ばれる) ことも確かめる
//! 3. ここで WAV を読み戻し、パルスが重なって曲頭に出ていること / latency ぶん後ろが静かなことを見る
//!
//! 2 の確認が要るのは、PDC が「latency があっても出力を遅延なしと同じにする」仕組みだから。
//! 既定の MCenter は音を変えない (素通り + 遅延) ので、出力だけでは「MCenter が経路に居ない」
//! (bypass 等) でも 3 が通ってしまう (2026-10 に bypass を入れて確かめた)。
//!
//! 回し方は `make test-real-vst3` (env と `-- --ignored` の扱いは `real_vst3_support` の module doc)。
//!
//! 対象は **MeldaProduction MCenter** (VST 3)。 レイテンシを申告する実プラグインなら
//! 原理的には何でもよいが、 期待値 (PDC 後に L/R の peak が揃う) はこれで確認している。

mod real_vst3_support;

use real_vst3_support as rv;

/// 実 VST3 の場所を渡す環境変数。
///
/// **開発機の絶対パスを既定値にしない**。 商用プラグインの標準インストール先はマシンごとに
/// 違ううえ、 既定値として書くと個人の環境がリポジトリに焼き付く (README や bench に同じ形で
/// 混入した前例がある)。 走らせたい人が自分のパスを渡す。
const MCENTER_ENV: &str = "DAW01_TEST_VST3_MCENTER";

/// MCenter の既定の latency (Reaper の "FX: Track 6" dialog で 4096/4096 spls と表示されることで
/// ユーザーが確認済)。
const MCENTER_LATENCY: usize = 4096;
/// パルスの振幅。2 本が重なっても 1.0 に届かない値にする (中央の pan は厳密に 1.0 倍)。
const PULSE_AMP: f32 = 0.25;
/// パルスの長さ。単発の sample にしないのは、書き出し (engine のレート) が音源のレートと違うと
/// resample で 1 sample の山が崩れるため。矩形の平らな部分は resample でも振幅を保つ。
const PULSE_FRAMES: usize = 64;

#[test]
#[ignore = "実物の VST3 (MeldaProduction MCenter) が要る — make test-real-vst3 で回す"]
fn pdc_real_mcenter_aligns_master_output() {
    let plugin = rv::vst3_from_env(MCENTER_ENV, "MeldaProduction MCenter");

    // 1 秒の音源 (latency 4096 より十分長い)。曲頭だけにパルスが立つ。
    let dir = tempfile::tempdir().expect("tempdir");
    let wav = dir.path().join("pulse.wav");
    let frames = rv::SOURCE_SR as usize;
    let mut pulse = vec![0.0f32; frames];
    pulse[..PULSE_FRAMES].fill(PULSE_AMP);
    rv::write_source_wav(&wav, &pulse);

    let out = dir.path().join("out.wav");
    let mut args = rv::source_shape_args(frames);
    args.push(rv::path_arg("wav", &wav));
    // プラグインの場所は script にも渡す (JS 側にも絶対パスを持たせない)。
    args.push(rv::path_arg("plugin", &plugin));
    args.push(("latency", MCENTER_LATENCY.to_string()));
    rv::run_script(env!("CARGO_BIN_EXE_daw_gui"), "pdc_mcenter.js", &out, &args);

    // 仕組み:
    //    - Track A: plugin 無し (latency 0)、パルス @0
    //    - Track B: MCenter が自分で 4096 を報告 (`PluginLatencyChanged` →
    //      `AudioCommand::SetDeviceLatency`)、パルス @0
    //    PDC が効いていれば Track A に `ApplyDelay(4096)` が刺さり、2 本のパルスが重なる
    //    (振幅 2 × PULSE_AMP)。PDC が無効なら PULSE_AMP の山が 2 つ、4096 離れて出る。
    //
    // r.md #39: 書き出しは `daw_audio::export::shift_window_for_master_latency` で窓を
    // master_latency ぶん後ろへずらすので、曲位置 0 のパルスは WAV の先頭に来るのが正しい
    // (ずらし漏れなら重なったパルスが 4096 に居残る)。
    //
    // 壊れ方ごとの (先頭, 後ろ) の山 (2026-10 に変異を当てて確かめた値):
    //    正常 (0.5, 0) / PDC が効かない (0.25, 0.25) / 窓ずらし漏れ (0, 0.5)
    let (l, r) = rv::read_export(&out);
    for (name, ch) in [("L", &l), ("R", &r)] {
        let head = rv::peak(ch, 0..PULSE_FRAMES);
        let late = rv::peak(ch, MCENTER_LATENCY - PULSE_FRAMES..MCENTER_LATENCY + 2 * PULSE_FRAMES);
        eprintln!("{name}: 先頭の山 {head:.4} / latency ぶん後ろの山 {late:.4}");
        // 判定の閾値は「重なった (2×) / 1 本だけ (1×)」と「1 本ある (1×) / 無い (0)」の中点。
        let diagnosis = if late > 1.5 * PULSE_AMP {
            "重なったパルスが後ろに居残る = 書き出しの窓ずらし漏れ"
        } else if late > 0.5 * PULSE_AMP {
            "山が 2 つに割れている = PDC が効いていない"
        } else {
            "後ろにも無い = track B が鳴っていないか、パルスが消えた"
        };
        assert!(
            head > 1.5 * PULSE_AMP && late < 0.5 * PULSE_AMP,
            "{name}: 先頭の山 {head:.4} (期待 ~{:.2}) / sample {MCENTER_LATENCY} 付近の山 {late:.4} \
             (期待 ~0): {diagnosis}",
            2.0 * PULSE_AMP,
        );
    }
}
