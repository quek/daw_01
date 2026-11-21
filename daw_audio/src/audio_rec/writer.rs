//! 録音の書き手スレッド: 取り込み ring を WAV (32-bit float) へ書く。
//!
//! 録音 1 回につき 1 スレッド (録音が終わったら結果を GUI へ返して消える)。ファイルを作るのも、書くのも、
//! 閉じるのもここ — RT (描画側 / tap) は I/O をしない。

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::audio_take::{AudioTakePeaks, AudioTakeProgress, AudioTakeResult, AudioTakesFinished, TAKE_PEAK_FRAMES};
use common::protocol::AudioEvent;

use super::TakeCtl;

/// ring を読む間隔。
const POLL: Duration = Duration::from_millis(10);
/// 録音中の波形の山を GUI へ送る間隔。
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// WAV のヘッダを書き直す間隔 (途中で落ちても、ここまでは読めるファイルが残る)。
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// 録音の終わりを頼まれてから、取り込みがこれだけ進まなければ待つのをやめる
/// (stream が止まっている。入出力のレイテンシぶんの取り込みは数十 ms で済む)。
const ABANDON_AFTER: Duration = Duration::from_secs(1);

/// 書き先 1 つ (トラック 1 本)。
pub struct TakeFile {
    track_id: u32,
    path: PathBuf,
    channels: u16,
    rx: rtrb::Consumer<f32>,
    dropped: Arc<AtomicU64>,
    wav: Option<hound::WavWriter<BufWriter<File>>>,
    frames: u64,
    error: Option<String>,
    /// 録音中の波形: 作りかけの山 (ここまでの最大と、溜めたフレーム数) と、まだ GUI へ送っていない山。
    peak: f32,
    peak_frames: u32,
    new_peaks: Vec<u8>,
}

impl TakeFile {
    pub(super) fn new(
        track_id: u32,
        path: PathBuf,
        channels: u16,
        rx: rtrb::Consumer<f32>,
        dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            track_id,
            path,
            channels,
            rx,
            dropped,
            wav: None,
            frames: 0,
            error: None,
            peak: 0.0,
            peak_frames: 0,
            new_peaks: Vec::new(),
        }
    }

    pub(super) fn open(&mut self, sample_rate: u32) {
        let spec = hound::WavSpec {
            channels: self.channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let made = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(|e| e.to_string())
            .and_then(|()| hound::WavWriter::create(&self.path, spec).map_err(|e| e.to_string()));
        match made {
            Ok(wav) => self.wav = Some(wav),
            Err(e) => self.fail(e),
        }
    }

    /// 書けなくなった: 以後は ring を読み捨てる (tap を詰まらせない)。
    fn fail(&mut self, error: String) {
        tracing::error!(path = %self.path.display(), %error, "audio take: write failed");
        self.wav = None;
        self.error.get_or_insert(error);
    }

    /// ring に溜まっている分を書く。書いたフレーム数を返す。
    pub(super) fn drain(&mut self) -> u64 {
        let ch = usize::from(self.channels.max(1));
        let n = self.rx.slots() / ch * ch;
        let Ok(chunk) = self.rx.read_chunk(n) else { return 0 };
        let mut failed = None;
        if let Some(wav) = self.wav.as_mut() {
            let (a, b) = chunk.as_slices();
            // 読むのは必ずフレーム単位 (`n` は ch 数の倍数) なので、`i` を ch 数で割った余りがフレームの中の ch。
            for (i, &s) in a.iter().chain(b).enumerate() {
                if let Err(e) = wav.write_sample(s) {
                    failed = Some(e.to_string());
                    break;
                }
                self.peak = self.peak.max(s.abs());
                if (i + 1) % ch != 0 {
                    continue;
                }
                self.peak_frames += 1;
                if self.peak_frames == TAKE_PEAK_FRAMES {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    self.new_peaks.push((self.peak.clamp(0.0, 1.0) * 255.0).round() as u8);
                    self.peak = 0.0;
                    self.peak_frames = 0;
                }
            }
        }
        chunk.commit_all();
        if let Some(e) = failed {
            self.fail(e);
            return 0;
        }
        let frames = (n / ch) as u64;
        if self.wav.is_some() {
            self.frames += frames;
        }
        frames
    }

    /// 前に取り出してから増えた山 (無ければ `None`)。
    pub(super) fn take_peaks(&mut self) -> Option<AudioTakePeaks> {
        (!self.new_peaks.is_empty())
            .then(|| AudioTakePeaks { track_id: self.track_id, peaks: std::mem::take(&mut self.new_peaks) })
    }

    fn flush(&mut self) {
        if let Some(Err(e)) = self.wav.as_mut().map(hound::WavWriter::flush) {
            self.fail(e.to_string());
        }
    }

    /// WAV を閉じて結果にする。1 フレームも書けなかったファイルは消す。
    fn finish(mut self) -> AudioTakeResult {
        if let Some(Err(e)) = self.wav.take().map(hound::WavWriter::finalize) {
            self.fail(e.to_string());
        }
        if self.error.is_some() {
            self.frames = 0;
        }
        if self.frames == 0 {
            let _ = std::fs::remove_file(&self.path);
        }
        AudioTakeResult {
            track_id: self.track_id,
            path: self.path,
            frames: self.frames,
            dropped_frames: self.dropped.load(Ordering::Relaxed),
            error: self.error,
        }
    }
}

/// 書き手スレッドを起こす。スレッドを作れなければ、その場で空の結果を返す。
pub(super) fn spawn(
    ctl: Arc<TakeCtl>,
    files: Vec<TakeFile>,
    sample_rate: u32,
    out_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
) {
    let spawned = std::thread::Builder::new().name("audio-take-writer".into()).spawn({
        let ctl = Arc::clone(&ctl);
        let out_tx = out_tx.clone();
        move || {
            let result = run(&ctl, files, sample_rate, &out_tx);
            let _ = out_tx.send(AudioEvent::AudioTakesFinished { project: ctl.project, result: Box::new(result) });
        }
    });
    if let Err(e) = spawned {
        tracing::error!(error = ?e, "failed to spawn audio take writer");
        ctl.abandoned.store(true, Ordering::Release);
        let result = AudioTakesFinished {
            take_id: ctl.take_id,
            sample_rate,
            segments: Vec::new(),
            segments_dropped: 0,
            takes: Vec::new(),
        };
        let _ = out_tx.send(AudioEvent::AudioTakesFinished { project: ctl.project, result: Box::new(result) });
    }
}

/// 取り込みが終わるまで ring を WAV へ書き続け、結果を組む。録っている間は、波形の山と区間を小刻みに
/// `out_tx` へ送る (増えていなければ送らない)。
fn run(
    ctl: &TakeCtl,
    mut files: Vec<TakeFile>,
    sample_rate: u32,
    out_tx: &tokio::sync::mpsc::UnboundedSender<AudioEvent>,
) -> AudioTakesFinished {
    for f in &mut files {
        f.open(sample_rate);
    }
    let mut idle_since: Option<Instant> = None;
    let mut flushed = Instant::now();
    let mut progressed = Instant::now();
    let mut sent_segments = 0;
    loop {
        // 「終わった」を読んでから ring を空にする (逆順だと、最後の block を取りこぼす)。
        let done = ctl.captured.load(Ordering::Acquire) || ctl.abandoned.load(Ordering::Acquire);
        let moved: u64 = files.iter_mut().map(TakeFile::drain).sum();
        if done {
            break;
        }
        if moved > 0 {
            idle_since = None;
        } else if ctl.stop_requested() && idle_since.get_or_insert_with(Instant::now).elapsed() > ABANDON_AFTER {
            // stream が止まっていて、残りが届かない。tap に手放させて、ここまでで閉じる。
            ctl.abandoned.store(true, Ordering::Release);
            continue;
        }
        if flushed.elapsed() > FLUSH_EVERY {
            files.iter_mut().for_each(TakeFile::flush);
            flushed = Instant::now();
        }
        if progressed.elapsed() > PROGRESS_EVERY {
            progressed = Instant::now();
            let new_segments = ctl.segments_from(sent_segments);
            sent_segments += new_segments.len();
            let tracks: Vec<AudioTakePeaks> = files.iter_mut().filter_map(TakeFile::take_peaks).collect();
            if !new_segments.is_empty() || !tracks.is_empty() {
                let progress = AudioTakeProgress { take_id: ctl.take_id, sample_rate, new_segments, tracks };
                let _ = out_tx
                    .send(AudioEvent::AudioTakeProgress { project: ctl.project, progress: Box::new(progress) });
            }
        }
        std::thread::sleep(POLL);
    }
    AudioTakesFinished {
        take_id: ctl.take_id,
        sample_rate,
        segments: ctl.segments_from(0),
        segments_dropped: ctl.segments_dropped.load(Ordering::Relaxed),
        takes: files.into_iter().map(TakeFile::finish).collect(),
    }
}
