//! 録音の一周 (描画側の判定 → tap → 書き手 → WAV) を、デバイス無しで通す。
//!
//! 入力は「ch `c` のデバイスのフレーム `d` の値 = `d + c * 0.5`」(f32 で厳密に表せる) なので、WAV の各フレームが
//! デバイスのどのフレームの音かをそのまま読める。

use std::path::PathBuf;

use common::audio_take::{AudioTakeRequest, AudioTakeSpec, AudioTakesFinished};
use common::model::TrackInput;

use super::*;

const BLOCK: usize = 128;
const SR: u32 = 48_000;

fn sample(ch: u16, device_frame: u64) -> f32 {
    device_frame as f32 + f32::from(ch) * 0.5
}

/// テストごとの書き先 (終わったら消す)。
struct TempWav(PathBuf);

impl TempWav {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!("daw01_take_{tag}_{}.wav", std::process::id())))
    }
}

impl Drop for TempWav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// デバイス 1 台ぶんの模型: コールバックごとに「描画 (判定) → 取り込み」を 1 block 進める。
struct Rig {
    drive: TakeDrive,
    tap: RecorderTap,
    ctl: Arc<TakeCtl>,
    done: tokio::sync::mpsc::UnboundedReceiver<AudioEvent>,
    device_frame: u64,
    latency: u32,
    /// master 出力の遅延補償の遅れ (frames)。
    pdc: u32,
}

impl Rig {
    fn new(takes: Vec<AudioTakeSpec>, latency: u32) -> Self {
        let (mut hub, tap) = channel();
        let (out_tx, done) = tokio::sync::mpsc::unbounded_channel();
        let ctl = hub.start(ProjectKey(1), AudioTakeRequest { take_id: 7, takes }, SR, out_tx);
        let mut drive = TakeDrive::default();
        assert!(drive.install(Some(Arc::clone(&ctl))).is_none());
        Self { drive, tap, ctl, done, device_frame: 0, latency, pdc: 0 }
    }

    /// 1 block。`live` = この block は録音中、`playhead` = block 先頭の曲の位置 (拍は sample / 1000)。
    fn callback(&mut self, live: bool, playhead: u64) {
        let d = self.device_frame;
        self.drive.observe(TakeBlock {
            live,
            playhead,
            beat: playhead as f64 / 1000.0,
            out_frame: d,
            frames: BLOCK as u32,
            latency: self.latency,
            pdc: self.pdc,
        });
        let chans: Vec<Vec<f32>> = (0..2u16).map(|c| (0..BLOCK as u64).map(|i| sample(c, d + i)).collect()).collect();
        self.tap.capture_block(d, BLOCK, |ch| chans.get(usize::from(ch)).map(Vec::as_slice));
        self.device_frame += BLOCK as u64;
    }

    fn finished(&mut self) -> AudioTakesFinished {
        match self.done.blocking_recv() {
            Some(AudioEvent::AudioTakesFinished { project, result }) => {
                assert_eq!(project, ProjectKey(1));
                *result
            }
            other => panic!("{other:?}"),
        }
    }
}

fn read_wav(path: &std::path::Path) -> (hound::WavSpec, Vec<f32>) {
    let mut r = hound::WavReader::open(path).unwrap();
    (r.spec(), r.samples::<f32>().map(Result::unwrap).collect())
}

/// WAV の先頭フレームは「録音が始まった block の出力と同時に鳴っていた入力」= 出力フレーム + 入出力レイテンシの
/// デバイスフレーム。長さは録音していた block の分ちょうどで、止めた後もレイテンシぶん取り込み続ける。
#[test]
fn wav_は録音開始の_block_からレイテンシを補正した入力を_録音した長さぶん持つ() {
    let wav = TempWav::new("stereo");
    let latency = 300; // 2 block と少し (block の境をまたぐ)
    let mut rig = Rig::new(
        vec![AudioTakeSpec { track_id: 4, input: TrackInput::AudioStereo { left: 0 }, path: wav.0.clone() }],
        latency,
    );
    // count-in (録音待ち) 3 block → 録音 7 block → 止めてからレイテンシぶん + 余分。
    for _ in 0..3 {
        rig.callback(false, 0);
    }
    for k in 0..7 {
        rig.callback(true, 5_000 + k * BLOCK as u64);
    }
    rig.ctl.request_stop();
    for _ in 0..5 {
        rig.callback(false, 5_000 + 7 * BLOCK as u64);
    }
    let result = rig.finished();

    assert_eq!(result.take_id, 7);
    assert_eq!(result.sample_rate, SR);
    assert_eq!(result.segments.len(), 1);
    assert_eq!((result.segments[0].file_frame, result.segments[0].start_beat), (0, 5.0));
    let take = &result.takes[0];
    assert_eq!((take.track_id, take.frames, take.dropped_frames, &take.error), (4, 7 * BLOCK as u64, 0, &None));

    let (spec, samples) = read_wav(&wav.0);
    assert_eq!((spec.channels, spec.sample_rate, spec.bits_per_sample), (2, SR, 32));
    assert_eq!(samples.len(), 7 * BLOCK * 2);
    let first = 3 * BLOCK as u64 + u64::from(latency);
    for (i, frame) in samples.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(*frame, [sample(0, first + i as u64), sample(1, first + i as u64)], "frame {i}");
    }
}

/// ループの折り返し (曲の位置が戻る) で区間が分かれる。音は 1 本の WAV に続けて入る。
#[test]
fn 曲の位置が飛ぶと区間が分かれる() {
    let wav = TempWav::new("loop");
    let mut rig = Rig::new(
        vec![AudioTakeSpec { track_id: 1, input: TrackInput::AudioMono { channel: 1 }, path: wav.0.clone() }],
        0,
    );
    // 4 block 進んで、曲の位置 2000 へ戻って 3 block。
    for k in 0..4 {
        rig.callback(true, 8_000 + k * BLOCK as u64);
    }
    for k in 0..3 {
        rig.callback(true, 2_000 + k * BLOCK as u64);
    }
    rig.ctl.request_stop();
    rig.callback(false, 0);
    let result = rig.finished();

    let segs: Vec<(u64, f64)> = result.segments.iter().map(|s| (s.file_frame, s.start_beat)).collect();
    assert_eq!(segs, [(0, 8.0), (4 * BLOCK as u64, 2.0)]);
    assert_eq!(result.takes[0].frames, 7 * BLOCK as u64);
    let (spec, samples) = read_wav(&wav.0);
    assert_eq!(spec.channels, 1);
    // モノは選んだ ch (1) だけ。レイテンシ 0 なので WAV のフレーム i はデバイスのフレーム i。
    assert!(samples.iter().enumerate().all(|(i, &s)| s == sample(1, i as u64)));
}

/// プラグインの遅延補償がある曲では、曲の位置の音が聞こえるのはその分だけ後。区間の頭 (録り始めも折り返しも)
/// をその分だけ後ろのフレームに置く — 折り返しの直後に聞こえているのは、まだ折り返す前の尻尾。
#[test]
fn 遅延補償の分だけ区間の頭が後ろのフレームになる() {
    let wav = TempWav::new("pdc");
    let mut rig = Rig::new(
        vec![AudioTakeSpec { track_id: 1, input: TrackInput::AudioMono { channel: 0 }, path: wav.0.clone() }],
        0,
    );
    rig.pdc = 100;
    for k in 0..4 {
        rig.callback(true, 8_000 + k * BLOCK as u64);
    }
    for k in 0..3 {
        rig.callback(true, 2_000 + k * BLOCK as u64);
    }
    rig.ctl.request_stop();
    rig.callback(false, 0);
    let result = rig.finished();
    let segs: Vec<(u64, f64)> = result.segments.iter().map(|s| (s.file_frame, s.start_beat)).collect();
    assert_eq!(segs, [(100, 8.0), (4 * BLOCK as u64 + 100, 2.0)]);
    assert_eq!(result.takes[0].frames, 7 * BLOCK as u64, "取り込む長さは変わらない");
}

/// count-in の間に取り消した録音は、空の結果を返してファイルを残さない。
#[test]
fn 始まる前に取り消した録音は空で終わる() {
    let wav = TempWav::new("cancel");
    let mut rig = Rig::new(
        vec![AudioTakeSpec { track_id: 1, input: TrackInput::AudioMono { channel: 0 }, path: wav.0.clone() }],
        64,
    );
    rig.callback(false, 0);
    rig.ctl.request_stop();
    rig.callback(false, 0);
    let result = rig.finished();
    assert!(result.segments.is_empty());
    assert_eq!(result.takes[0].frames, 0);
    assert!(!wav.0.exists(), "何も録れなかったファイルは消す");
}

/// ring が溢れても位置はずれない: 入らなかった分は無音として後で書き、落とした数を数える。
#[test]
fn ring_が溢れた分は無音で埋めて位置を保つ() {
    let (tx, mut rx) = rtrb::RingBuffer::new(8);
    let dropped = Arc::new(AtomicU64::new(0));
    let mut lane = TakeLane { left: 0, right: 0, stereo: false, tx, gap: 0, dropped: Arc::clone(&dropped) };
    let input: Vec<f32> = (1..=20).map(|v| v as f32).collect();

    lane.push(Some(&input[..12]), None, 12); // 8 入って 4 落ちる
    assert_eq!((lane.gap, dropped.load(Ordering::Relaxed)), (4, 4));
    lane.push(Some(&input[12..14]), None, 2); // 無音すら入らない: これも落とす
    assert_eq!((lane.gap, dropped.load(Ordering::Relaxed)), (6, 6));

    let mut out = Vec::new();
    let drain = |rx: &mut rtrb::Consumer<f32>, out: &mut Vec<f32>| {
        while let Ok(v) = rx.pop() {
            out.push(v);
        }
    };
    drain(&mut rx, &mut out);
    lane.push(Some(&input[14..16]), None, 2); // 先に無音 6、続けて 2
    drain(&mut rx, &mut out);
    assert_eq!(out, [1., 2., 3., 4., 5., 6., 7., 8., 0., 0., 0., 0., 0., 0., 15., 16.]);
    assert_eq!(lane.gap, 0);
}
