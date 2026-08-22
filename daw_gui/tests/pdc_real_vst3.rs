//! PDC integration test: 実 VST3 plugin (MeldaProduction MCenter) を
//! load して、 track 間の位相揃いを WAV 出力で確認する。
//!
//! flow:
//! 1. `tests/scripts/pdc_mcenter.js` を `daw_gui --script` で実行
//! 2. JS が track A (no plugin) と track B (MCenter loaded) に同一の
//!    impulse @sample 0 を注入
//! 3. JS が `daw.exportWav` を呼んで master output を WAV に書き出し
//! 4. ここで WAV を読み戻し、 master の peak 位置を検出
//! 5. PDC が効いていれば L/R 両方が同位置に peak を持つ (=「音ずれ」 無し)
//!
//! プラグイン未指定 / 不在の環境では SKIP (`return` で test pass)。
//!
//! 対象は **MeldaProduction MCenter** (VST 3)。 レイテンシを申告する実プラグインなら
//! 原理的には何でもよいが、 期待値 (PDC 後に L/R の peak が揃う) はこれで確認している。

use std::path::{Path, PathBuf};

/// 実 VST3 の場所を渡す環境変数。
///
/// **開発機の絶対パスを既定値にしない**。 商用プラグインの標準インストール先はマシンごとに
/// 違ううえ、 既定値として書くと個人の環境がリポジトリに焼き付く (README や bench に同じ形で
/// 混入した前例がある)。 走らせたい人が自分のパスを渡す。
const MCENTER_ENV: &str = "DAW01_TEST_VST3_MCENTER";

/// 環境変数から実 VST3 の絶対パスを解決する。 未設定 / 不在なら **理由を stderr に出して**
/// `None` (= テストは SKIP)。 商用プラグインなので clone した人の環境にも CI にも無いのが
/// 普通で、 黙って通ると「なぜ何も検証されていないのか」 が分からなくなる。
fn vst3_from_env(var: &str, product: &str) -> Option<PathBuf> {
    let Some(raw) = std::env::var_os(var) else {
        eprintln!(
            "SKIP: {var} が未設定です。{product} (VST 3) の .vst3 パスを \
             {var} に入れて実行すると、このテストが走ります。"
        );
        return None;
    };
    let path = PathBuf::from(raw);
    if !path.exists() {
        eprintln!("SKIP: {var} が指す {} が存在しません。", path.display());
        return None;
    }
    Some(path)
}

#[test]
#[ignore = "PR-V4: setGeneratedAudio 経路廃止に伴い、 click signal の inject path を audio clip + import_audio に置き換える書き直しが必要。 PDC ロジック自体は不変なので、 test 復活は別 PR (= test 用 WAV を generate + ImportAudio で audio_source 登録 + audio clip events 経由で再生)"]
fn pdc_real_mcenter_aligns_master_output() {
    // 1. プラグインの場所が渡されていなければ SKIP (理由は helper が stderr に出す)
    let Some(plugin) = vst3_from_env(MCENTER_ENV, "MeldaProduction MCenter") else {
        return;
    };

    // 2. tempfile に WAV を書き出す
    let tmp = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile()
        .expect("create temp wav");
    let out_path = tmp.path().to_path_buf();
    drop(tmp); // close handle, leave path string

    // 3. daw_gui --script で headless 実行
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("scripts")
        .join("pdc_mcenter.js");
    // プラグインの場所は script にも渡す (JS 側にも絶対パスを持たせない)。
    let plugin_arg = format!("plugin={}", plugin.display());
    let status = std::process::Command::new(exe)
        .args([
            "--script",
            script.to_str().unwrap(),
            "--output",
            out_path.to_str().unwrap(),
            "--arg",
            &plugin_arg,
        ])
        .status()
        .expect("spawn daw_gui");
    assert!(
        status.success(),
        "daw_gui --script exited non-zero (status={status:?})"
    );

    // 4. WAV 読み戻し → 最大値の position を計算
    let mut reader = hound::WavReader::open(&out_path).expect("open wav");
    let spec = reader.spec();
    assert_eq!(spec.channels, 2, "expected stereo WAV");
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
    assert!(samples.len() >= 4, "WAV too short");

    let (l, r): (Vec<f32>, Vec<f32>) = samples
        .chunks_exact(2)
        .map(|c| (c[0], c[1]))
        .unzip();

    // 5. 仕組み (MCenter デフォルト latency = 4096 sample):
    //    - Track A: declared latency 0、 vocal impulse @0、 plugin 無し
    //    - Track B: MCenter が自分で 4096 を報告 (`PluginLatencyChanged` →
    //      `AudioCommand::SetDeviceLatency`)、
    //      MCenter loaded (Fx slot 0)、 vocal impulse @0
    //    PDC が効いていれば:
    //      Track A の path latency (0) を Track B の path latency (4096)
    //      に揃えるため、 Track A の scratch に `ApplyDelay(4096)` が刺さる
    //      → Track A の impulse は sample 4096 で出る
    //      Track B は MCenter で 4096 sample 遅延されて出る
    //      master[4096] = Track A (0.7071) + Track B (0.7071) = **~1.4142**
    //      master[0..4096] = ほぼ 0
    //
    //    PDC が無効なら:
    //      Track A: sample 0 で 0.7071
    //      Track B: sample 4096 で 0.7071 (MCenter 単独)
    //      → 2 つの分離した peak、 master[0] = 0.7071、 master[4096] = 0.7071
    //
    //    判定: **書き出し WAV の sample 0** が ~1.4142 (overlay 印) なら
    //    「PDC が効いている」かつ「export が master latency を差し引いている」。
    //
    // r.md #39: 以前この test は「peak が sample 4096 に出る」ことを期待していたが、
    // それは **書き出し WAV 全体が PDC 遅延ぶん後ろへずれている** バグの記述だった
    // (stem を曲頭に貼り戻すと 85ms ずれる)。export は
    // `daw_audio::export::shift_window_for_master_latency` で書き出し窓を
    // master_latency ぶん後ろへずらすようになったので、曲位置 0 の impulse は
    // wav[0] に来るのが正しい。
    const MCENTER_LATENCY: usize = 4096;
    let amp_0 = l[0].abs();
    eprintln!("L sample[0]: {}", l[0]);
    eprintln!("L sample[{}]: {}", MCENTER_LATENCY, l[MCENTER_LATENCY]);
    eprintln!("R sample[0]: {}", r[0]);
    let _ = r;

    // PDC overlay: 両 track の impulse が曲位置 0 で重なるので
    // master の振幅は ~1.4142 (≈ 2 * 0.7071)。 1.2 以上を許容。
    assert!(
        amp_0 > 1.2,
        "expected PDC-overlaid peak at wav sample 0 (~1.4142), got {:.4}; \
         L[{}]={:.4} (peak がここに出るなら export が master latency を引いていない)",
        amp_0,
        MCENTER_LATENCY,
        l[MCENTER_LATENCY],
    );

    // 書き出しがずれていれば peak は sample 4096 に居残る。そこが静かなことを
    // 確かめて「ずれたまま」を検出する。
    let amp_shifted = l[MCENTER_LATENCY].abs();
    assert!(
        amp_shifted < 0.1,
        "sample {} amplitude {:.4} too high; 書き出し WAV が master latency ぶん \
         後ろへずれている (export の窓ずらし漏れ)",
        MCENTER_LATENCY,
        amp_shifted,
    );
}

