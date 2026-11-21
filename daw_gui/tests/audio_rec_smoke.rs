//! r.md #156: オーディオ入力の録音を実プロセスで一周させる (`tests/scripts/audio_rec.js` を
//! `daw_gui --script` で実行)。
//!
//! daw_gui 本体を subprocess 起動し、daw_audio / daw_plugin_host まで spawn する (`CARGO_BIN_EXE_daw_gui`、
//! `make test-nolaunch` の対象外)。録音に使うデバイスは合成デバイスなので、実機のドライバーは開かない
//! (起動直後だけ、他の script と同じく OS 標準の既定の出力を開く)。音は出ない。
//!
//! script が見るもの (GUI の操作と同じ `AppEvent` の経路。段の中身は script の冒頭):
//! - 版 45 (入力 / モニターの無い) プロジェクトを開いただけでは dirty にならない。
//! - 入力設定 → 録音待機 → 録音 → 停止でクリップができ、undo で消え、保存して開き直しても鳴る。
//! - モニターのオフ / 自動 / オンで、マスターに入力が出る・出ない。入力メーター。録音中の帯と波形。
//! - ループ録音・count-in・曲末越え。
//!
//! ここ (Rust 側) が見るもの: **録れた WAV と書き出した WAV の中身**。script は音を読めないので、
//! script が書いたファイルを後から開く。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use common::audio_device::synthetic::{RAMP_PERIOD, ramp_sample};
use common::model::{Song, Track};

/// 1 本ずつ走らせる (どの段も daw_gui + 子プロセス 2 つを起こし、合成デバイスのスレッドが時計を刻む。
/// 並べて走らせると互いの負荷で録音の取りこぼしが出る)。
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

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

/// script の段を走らせる。失敗したら stdout / stderr を付けて落とす。
fn run_stage(stage: &str, args: &[(&str, &str)]) {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let exe = env!("CARGO_BIN_EXE_daw_gui");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("scripts").join("audio_rec.js");
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["--script", script.to_str().unwrap(), "--arg", &format!("stage={stage}")]);
    for (k, v) in args {
        cmd.args(["--arg", &format!("{k}={v}")]);
    }
    let output = cmd.output().expect("spawn daw_gui");
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!(
            "audio_rec.js stage={stage} {args:?} failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
    }
}

/// script に渡すパス (JS の文字列の中で `\` が要らない形)。
fn js_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// `dir` の下 (保存したプロジェクトの bundle) から、録音の WAV (`*_rec_*.wav`) を全部探す。
fn find_takes(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap().map(Result::unwrap) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "wav")
                && path.file_name().is_some_and(|n| n.to_string_lossy().contains("_rec_"))
            {
                found.push(path);
            }
        }
    }
    found
}

/// 録音の WAV を 1 つだけ見つけて読む (32-bit float であること)。
fn read_take(dir: &Path) -> (hound::WavSpec, Vec<f32>) {
    let takes = find_takes(dir);
    assert_eq!(takes.len(), 1, "録音の WAV が 1 つでない: {takes:?}");
    let mut r = hound::WavReader::open(&takes[0]).unwrap();
    let spec = r.spec();
    assert_eq!((spec.bits_per_sample, spec.sample_format, spec.sample_rate), (32, hound::SampleFormat::Float, 48_000));
    (spec, r.samples::<f32>().map(Result::unwrap).collect())
}

/// 16-bit PCM mono の WAV を書く (`frames` フレームのうち `click` フレーム目だけ 0.5、他は無音)。
fn write_click_wav(path: &Path, frames: u32, click: u32) {
    let spec =
        hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..frames {
        w.write_sample(if i == click { i16::MAX / 2 } else { 0 }).unwrap();
    }
    w.finalize().unwrap();
}

#[test]
fn 版_45_のプロジェクトは開いただけでは_dirty_にならない() {
    let dir = tempfile::tempdir().expect("tempdir");
    let old = dir.path().join("old").join("old.daw");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    write_v45_project(&old);
    run_stage("old", &[("old", &js_path(&old))]);
}

/// 入力設定 → モニター → 書き出し → count-in 付きの録音 → undo / redo → 保存して開き直して再生 (script)。
/// ここでは、録れた WAV が入力そのもの (1 フレームも欠けず・重ならず、ch も合っている) で、書き出した WAV に
/// 入力音が入っていないことを見る。
#[test]
fn 合成入力を録音してクリップにし_保存して開き直しても鳴る() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_stage("ramp", &[("dir", &js_path(dir.path()))]);

    let (spec, samples) = read_take(&dir.path().join("proj"));
    assert_eq!(spec.channels, 2);
    // 先頭のフレームがのこぎり波のどこかを ch 0 から決める。そこから先は全フレーム・全 ch が続きであること。
    let phase = (0..RAMP_PERIOD).find(|&p| samples[0] == ramp_sample(0, p)).expect("先頭が RAMP の値でない");
    for (i, frame) in samples.as_chunks::<2>().0.iter().enumerate() {
        let want = [ramp_sample(0, phase + i as u64), ramp_sample(1, phase + i as u64)];
        assert_eq!(*frame, want, "frame {i} (phase {phase})");
    }

    // 書き出しは、モニターがオンのトラックがあっても入力音を含まない (曲には音が無いので無音)。
    let mut export = hound::WavReader::open(dir.path().join("export.wav")).unwrap();
    assert!(export.duration() > 0);
    let silent = match export.spec().sample_format {
        hound::SampleFormat::Float => export.samples::<f32>().all(|s| s.unwrap() == 0.0),
        hound::SampleFormat::Int => export.samples::<i32>().all(|s| s.unwrap() == 0),
    };
    assert!(silent, "書き出しに入力音が入っている");
}

/// ループ録音: 後の周が前の周を上書きし、undo 1 回で全部消える (script が見る)。
#[test]
fn ループ録音は後の周が前の周を上書きする() {
    let dir = tempfile::tempdir().expect("tempdir");
    run_stage("loop", &[("dir", &js_path(dir.path()))]);
}

/// 録音の位置の補正: 曲の `CLICK` フレーム目で鳴らしたクリックを LOOPBACK (出力が「入力レイテンシ + 出力
/// レイテンシ」遅れて入力に戻る) で録ると、録れた WAV の同じフレームにクリックがある。先読みの on / off と、
/// デバイスの 1 回が engine の作業場 (1024 フレーム) より大きい場合も同じ。
#[test]
fn 録音の位置は入出力のレイテンシと先読みを補正する() {
    const CLICK: u32 = 30_000;
    for (render_ahead, buffer_frames) in [("0", "128"), ("1", "128"), ("0", "2048"), ("1", "2048")] {
        let dir = tempfile::tempdir().expect("tempdir");
        let wav = dir.path().join("click.wav");
        write_click_wav(&wav, 96_000, CLICK);
        run_stage(
            "loopback",
            &[
                ("dir", &js_path(dir.path())),
                ("wav", &js_path(&wav)),
                ("renderAhead", render_ahead),
                ("bufferFrames", buffer_frames),
            ],
        );
        let case = format!("renderAhead={render_ahead} bufferFrames={buffer_frames}");
        let (spec, samples) = read_take(&dir.path().join("proj"));
        assert_eq!(spec.channels, 2, "{case}");
        let frames = samples.as_chunks::<2>().0;
        assert!(frames.len() > CLICK as usize, "{case}: クリックより手前で録音が終わった ({} frames)", frames.len());
        for ch in 0..2 {
            let loud: Vec<(usize, f32)> =
                frames.iter().map(|f| f[ch]).enumerate().filter(|(_, s)| s.abs() > 1e-4).collect();
            assert_eq!(loud.len(), 1, "{case} ch {ch}: 音のあるフレームが 1 つでない: {:?}", &loud[..loud.len().min(8)]);
            let (at, level) = loud[0];
            assert_eq!(at, CLICK as usize, "{case} ch {ch}: クリックの位置がずれている");
            assert!((level - 0.5).abs() < 0.01, "{case} ch {ch}: クリックの大きさ {level}");
        }
    }
}
