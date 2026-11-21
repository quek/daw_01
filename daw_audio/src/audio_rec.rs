//! オーディオ入力の録音 (r.md #156) — engine 側。
//!
//! 1 回の録音 ([`TakeCtl`]) を 3 つのスレッドが分担する:
//!
//! - **描画側** (`ProjectRt::render_buffer`、[`TakeDrive`]): 録音が始まった / 終わった block を知っている。
//!   その block の出力がデバイスへ渡る時刻 (`out_frame`) に入出力のレイテンシを足して「デバイスの
//!   どのフレームから / までを取り込むか」を決め、曲の位置が飛んだ所 (ループの折り返し・seek) を
//!   区間の表に書く。先読みぶんは `out_frame` に織り込まれている。
//! - **tap** (デバイスのコールバック、[`RecorderTap`]): 届いたままの入力から、その範囲を wait-free の
//!   ring へ出す。先読みに遅らされない。
//! - **書き手** ([`writer`]): ring を WAV (32-bit float) へ書き、終わったら結果を GUI へ返す。
//!
//! 描画側と tap は確保・ロック・I/O をしない。WAV の先頭フレームは「録音が始まった block の先頭の
//! 曲の位置で鳴っていた音」なので、GUI は補正なしでその拍に置けばよい。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use common::audio_take::{AudioTakeRequest, AudioTakeSegment};
use common::protocol::{AudioEvent, ProjectKey};

pub mod writer;

#[cfg(test)]
mod tests;

/// 「まだ決まっていない」デバイスのフレーム。
const NOT_SET: u64 = u64::MAX;
/// 1 回の録音で覚える区間の数 (ループ 1 周で 1 つ増える)。溢れた区間は直前の区間に続けて入る。
const MAX_SEGMENTS: usize = 1024;
/// tap が同時に抱える録音の数 (project ごとに 1 つ + 終わりかけ)。
const MAX_ACTIVE_SETS: usize = common::audio_bridge::MAX_PROJECTS * 2;
/// 取り込み ring の長さ (秒)。書き手は 10 ms ごとに読むので、溢れるのはディスクが止まったとき。
const RING_SECONDS: usize = 2;

struct SegmentCell {
    file_frame: AtomicU64,
    beat_bits: AtomicU64,
}

/// 1 回の録音の共有面 (描画側 / tap / 書き手 / recv loop)。全部 atomic。
pub struct TakeCtl {
    pub take_id: u64,
    pub project: ProjectKey,
    /// 取り込みを始める / 終えるデバイスのフレーム。描画側が 1 回ずつ書く。
    start: AtomicU64,
    end: AtomicU64,
    /// 録音はもう続かない (`StopRecording` / project を閉じた / 次の録音が来た)。recv loop が立てる。
    stop_requested: AtomicBool,
    /// tap が範囲を全部取り込み終えた。
    captured: AtomicBool,
    /// 書き手が待つのをやめた (stream が止まっていて取り込みが進まない)。tap は見つけたら手放す。
    abandoned: AtomicBool,
    n_segments: AtomicU32,
    segments_dropped: AtomicU32,
    segments: Box<[SegmentCell]>,
}

impl TakeCtl {
    fn new(take_id: u64, project: ProjectKey) -> Self {
        let segments = (0..MAX_SEGMENTS)
            .map(|_| SegmentCell { file_frame: AtomicU64::new(0), beat_bits: AtomicU64::new(0) })
            .collect();
        Self {
            take_id,
            project,
            start: AtomicU64::new(NOT_SET),
            end: AtomicU64::new(NOT_SET),
            stop_requested: AtomicBool::new(false),
            captured: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            n_segments: AtomicU32::new(0),
            segments_dropped: AtomicU32::new(0),
            segments,
        }
    }

    /// 録音はもう続かない (recv loop)。描画側が次の block で範囲を閉じる。
    pub fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    /// 区間を 1 つ足す (描画側だけが呼ぶ)。
    fn push_segment(&self, file_frame: u64, beat: f64) {
        let n = self.n_segments.load(Ordering::Relaxed) as usize;
        let Some(cell) = self.segments.get(n) else {
            self.segments_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        cell.file_frame.store(file_frame, Ordering::Relaxed);
        cell.beat_bits.store(beat.to_bits(), Ordering::Relaxed);
        self.n_segments.store(n as u32 + 1, Ordering::Release);
    }

    /// 区間の表 (書き手が、取り込みが終わってから読む)。
    fn segments(&self) -> Vec<AudioTakeSegment> {
        let n = (self.n_segments.load(Ordering::Acquire) as usize).min(self.segments.len());
        self.segments[..n]
            .iter()
            .map(|c| AudioTakeSegment {
                file_frame: c.file_frame.load(Ordering::Relaxed),
                start_beat: f64::from_bits(c.beat_bits.load(Ordering::Relaxed)),
            })
            .collect()
    }
}

/// 描画側が 1 block ごとに渡す、録音の判定の材料。
#[derive(Debug, Clone, Copy)]
pub struct TakeBlock {
    /// この block は録音中か (`recording_live`: 録音要求 && 再生中 && count-in 完了)。
    pub live: bool,
    /// block 先頭の曲の位置 (sample / 拍)。
    pub playhead: u64,
    pub beat: f64,
    /// block 先頭のフレームがデバイスへ渡る時刻 (デバイスの時計での通し番号)。
    pub out_frame: u64,
    pub frames: u32,
    /// 入力レイテンシ + 出力レイテンシ (frames)。
    pub latency: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Idle,
    /// 録音の開始を待っている (count-in 中 / `StartRecording` がまだ)。
    Armed,
    Rolling,
    Closed,
}

/// 描画側の録音の状態 (`ProjectRt` が持つ)。
#[derive(Default)]
pub struct TakeDrive {
    ctl: Option<Arc<TakeCtl>>,
    phase: Phase,
    /// 録音が始まった block の `out_frame` (WAV の先頭フレームに当たる出力フレーム)。
    origin: u64,
    /// 連続していれば次の block がこの値で始まる。
    next_playhead: u64,
    next_out: u64,
    latency: u32,
}

impl TakeDrive {
    /// 便で届いた録音に差し替える。同じ録音なら何もしない。押し出した (か重複した) `Arc` を返す —
    /// 呼び側が off-thread へ返す (RT で最後の参照を落とさない)。
    pub fn install(&mut self, new: Option<Arc<TakeCtl>>) -> Option<Arc<TakeCtl>> {
        if let (Some(a), Some(b)) = (&self.ctl, &new)
            && Arc::ptr_eq(a, b)
        {
            return new;
        }
        self.close();
        self.phase = if new.is_some() { Phase::Armed } else { Phase::Idle };
        std::mem::replace(&mut self.ctl, new)
    }

    /// 今の録音を、最後に描いた block の末尾で閉じる。
    fn close(&mut self) {
        let Some(ctl) = &self.ctl else { return };
        match self.phase {
            Phase::Armed => {
                // 一度も始まらなかった: 空の範囲。
                ctl.start.store(0, Ordering::Release);
                ctl.end.store(0, Ordering::Release);
            }
            Phase::Rolling => {
                ctl.end.store(self.next_out.saturating_add(u64::from(self.latency)), Ordering::Release);
            }
            Phase::Idle | Phase::Closed => return,
        }
        self.phase = Phase::Closed;
    }

    /// 1 block ぶん進める。確保・ロックなし (atomic の store だけ)。
    pub fn observe(&mut self, block: TakeBlock) {
        let Some(ctl) = &self.ctl else { return };
        let frames = u64::from(block.frames);
        match self.phase {
            Phase::Armed if block.live => {
                self.origin = block.out_frame;
                self.latency = block.latency;
                ctl.push_segment(0, block.beat);
                ctl.start.store(block.out_frame.saturating_add(u64::from(block.latency)), Ordering::Release);
                self.phase = Phase::Rolling;
                self.next_playhead = block.playhead + frames;
                self.next_out = block.out_frame + frames;
            }
            // count-in の取り消し等: 始まる前に終わった。
            Phase::Armed if ctl.stop_requested() => self.close(),
            // デバイスの時計が戻った (stream の作り直し) ら、そこで打ち切る。
            Phase::Rolling if !block.live || ctl.stop_requested() || block.out_frame < self.next_out => self.close(),
            Phase::Rolling => {
                // 曲の位置かデバイスの時計が飛んだ (ループの折り返し / seek / 描画が止まっていた): 新しい区間。
                if block.playhead != self.next_playhead || block.out_frame != self.next_out {
                    ctl.push_segment(block.out_frame - self.origin, block.beat);
                }
                self.next_playhead = block.playhead + frames;
                self.next_out = block.out_frame + frames;
            }
            Phase::Armed | Phase::Idle | Phase::Closed => {}
        }
    }
}

/// 録音 1 本 (トラック 1 つ) の取り込み口 (tap 側)。
struct TakeLane {
    /// デバイスの入力 ch 番号 (モノは両方同じ)。
    left: u16,
    right: u16,
    stereo: bool,
    tx: rtrb::Producer<f32>,
    /// ring に入らず、まだ書けていない無音 (frames)。入るようになったら先に書く = 位置がずれない。
    gap: u64,
    dropped: Arc<AtomicU64>,
}

impl TakeLane {
    fn channels(&self) -> usize {
        if self.stereo { 2 } else { 1 }
    }

    /// 無音 `n` フレームを予約する (取り込めなかった分)。
    fn skip(&mut self, n: u64) {
        self.gap += n;
        self.dropped.fetch_add(n, Ordering::Relaxed);
    }

    /// `n` フレームを ring へ出す (`None` の ch は無音)。入らない分は無音として後で書く。
    fn push(&mut self, l: Option<&[f32]>, r: Option<&[f32]>, n: usize) {
        let ch = self.channels();
        if self.gap > 0 {
            let fit = ((self.tx.slots() / ch) as u64).min(self.gap);
            if let Ok(chunk) = self.tx.write_chunk_uninit(fit as usize * ch) {
                chunk.fill_from_iter(std::iter::repeat_n(0.0, fit as usize * ch));
                self.gap -= fit;
            }
        }
        // 無音がまだ残っている: 先にそれを書かないと順番が崩れるので、この block も落とす。
        let fit = if self.gap > 0 { 0 } else { (self.tx.slots() / ch).min(n) };
        if fit > 0
            && let Ok(chunk) = self.tx.write_chunk_uninit(fit * ch)
        {
            let at = |s: Option<&[f32]>, i: usize| s.and_then(|s| s.get(i)).copied().unwrap_or(0.0);
            if self.stereo {
                chunk.fill_from_iter((0..fit).flat_map(|i| [at(l, i), at(r, i)]));
            } else {
                chunk.fill_from_iter((0..fit).map(|i| at(l, i)));
            }
        }
        if fit < n {
            self.skip((n - fit) as u64);
        }
    }
}

/// tap へ届ける 1 回の録音 (同時に録るトラックの組)。
pub struct TakeSet {
    ctl: Arc<TakeCtl>,
    lanes: Vec<TakeLane>,
    /// 次に取り込むデバイスのフレーム (`NOT_SET` = まだ始まっていない)。
    next: u64,
}

/// デバイスのコールバックから呼ばれる取り込み口。
pub struct RecorderTap {
    incoming: rtrb::Consumer<Box<TakeSet>>,
    /// 終わった録音を off-thread へ返す口 (RT で解放しない)。
    retired: rtrb::Producer<Box<TakeSet>>,
    active: Vec<Box<TakeSet>>,
    /// 直前の block の末尾 (時計が戻ったら stream の作り直し)。
    last_end: u64,
}

/// デバイス層の取り出し口 (`crate::device::input::InputTap`) につなぐ。
impl crate::device::input::InputTap for RecorderTap {
    fn capture(&mut self, input: &crate::device::input::InputBlock<'_>) {
        self.capture_block(input.device_frame(), input.frames(), |ch| input.channel(ch));
    }
}

impl RecorderTap {
    /// コールバック 1 回ぶんの入力を取り込む。`channel(ch)` はデバイスの入力 ch 番号 `ch` の音
    /// (長さ `frames`。無い ch は `None` = 無音)。確保・ロック・I/O なし。
    pub fn capture_block<'a>(&mut self, device_frame: u64, frames: usize, channel: impl Fn(u16) -> Option<&'a [f32]>) {
        while self.active.len() < self.active.capacity() {
            let Ok(set) = self.incoming.pop() else { break };
            self.active.push(set);
        }
        let restarted = device_frame < self.last_end;
        let block_end = device_frame + frames as u64;
        self.last_end = block_end;

        let mut i = 0;
        while i < self.active.len() {
            if Self::capture_set(&mut self.active[i], device_frame, frames, restarted, &channel) {
                // 返す ring が満杯なら次のコールバックで (ここで解放しない)。
                if self.retired.is_full() {
                    i += 1;
                    continue;
                }
                let set = self.active.swap_remove(i);
                let _ = self.retired.push(set);
            } else {
                i += 1;
            }
        }
    }

    /// 1 回の録音にこの block を取り込む。取り込み終えた (手放してよい) ら true。
    fn capture_set<'a>(
        set: &mut TakeSet,
        device_frame: u64,
        frames: usize,
        restarted: bool,
        channel: &impl Fn(u16) -> Option<&'a [f32]>,
    ) -> bool {
        let ctl = &set.ctl;
        if ctl.abandoned.load(Ordering::Acquire) || ctl.captured.load(Ordering::Acquire) {
            return true;
        }
        let start = ctl.start.load(Ordering::Acquire);
        if start == NOT_SET {
            return false;
        }
        let end = ctl.end.load(Ordering::Acquire);
        if set.next == NOT_SET {
            set.next = start;
        } else if restarted {
            // デバイスの時計が戻った: 続きはもう別の時計なので、ここまでで終わりにする。
            ctl.captured.store(true, Ordering::Release);
            return true;
        }
        let block_end = device_frame + frames as u64;
        // block が `next` より先から始まっている (コールバックが飛んだ / 開始が間に合わなかった): 無音で埋める。
        let missed = device_frame.min(end).saturating_sub(set.next);
        if missed > 0 {
            for lane in &mut set.lanes {
                lane.skip(missed);
            }
            set.next += missed;
        }
        let lo = set.next.max(device_frame);
        let hi = block_end.min(end);
        if hi > lo {
            let (a, b) = ((lo - device_frame) as usize, (hi - device_frame) as usize);
            for lane in &mut set.lanes {
                let l = channel(lane.left).and_then(|s| s.get(a..b));
                let r = if lane.stereo { channel(lane.right).and_then(|s| s.get(a..b)) } else { None };
                lane.push(l, r, b - a);
            }
            set.next = hi;
        }
        if end != NOT_SET && set.next >= end {
            ctl.captured.store(true, Ordering::Release);
            return true;
        }
        false
    }
}

/// recv loop 側の口: 録音を作って tap と書き手へ渡す。
pub struct RecHub {
    take_tx: rtrb::Producer<Box<TakeSet>>,
    retired_rx: rtrb::Consumer<Box<TakeSet>>,
}

/// recv loop の口と、デバイスのコールバックへ差し込む tap。
#[must_use]
pub fn channel() -> (RecHub, RecorderTap) {
    let (take_tx, incoming) = rtrb::RingBuffer::new(MAX_ACTIVE_SETS);
    let (retired, retired_rx) = rtrb::RingBuffer::new(MAX_ACTIVE_SETS * 2);
    let tap = RecorderTap { incoming, retired, active: Vec::with_capacity(MAX_ACTIVE_SETS), last_end: 0 };
    (RecHub { take_tx, retired_rx }, tap)
}

impl RecHub {
    /// `StartAudioTakes`: WAV の書き手スレッドを起こし、取り込み口を tap へ届ける。戻り値は描画側へ
    /// 便で届ける共有面。オーディオ入力でないトラックの指示は捨てる。
    pub fn start(
        &mut self,
        project: ProjectKey,
        request: AudioTakeRequest,
        sample_rate: u32,
        out_tx: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
    ) -> Arc<TakeCtl> {
        let ctl = Arc::new(TakeCtl::new(request.take_id, project));
        let mut lanes = Vec::new();
        let mut files = Vec::new();
        for spec in request.takes {
            let Some((left, right)) = spec.input.audio_channels() else { continue };
            let channels = spec.input.file_channels();
            let cap = (sample_rate.max(1) as usize) * RING_SECONDS * usize::from(channels);
            let (tx, rx) = rtrb::RingBuffer::new(cap);
            let dropped = Arc::new(AtomicU64::new(0));
            lanes.push(TakeLane { left, right, stereo: channels == 2, tx, gap: 0, dropped: Arc::clone(&dropped) });
            files.push(writer::TakeFile::new(spec.track_id, spec.path, channels, rx, dropped));
        }
        let set = Box::new(TakeSet { ctl: Arc::clone(&ctl), lanes, next: NOT_SET });
        if self.take_tx.push(set).is_err() {
            // tap が居ない / 詰まっている: 取り込めないので、書き手は空の結果を返す。
            tracing::error!(project = project.0, "audio take: tap delivery ring full");
            ctl.abandoned.store(true, Ordering::Release);
        }
        writer::spawn(Arc::clone(&ctl), files, sample_rate, out_tx);
        ctl
    }

    /// tap が手放した録音を解放する (ring の producer の解放は off-RT)。
    pub fn housekeeping(&mut self) {
        while let Ok(set) = self.retired_rx.pop() {
            drop(set);
        }
    }
}
