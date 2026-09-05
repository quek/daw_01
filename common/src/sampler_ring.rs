//! Global Sampler の音声リング (`docs/plan_global_sampler.md` §3.1)。
//!
//! [`crate::scope_bridge`] と同じ流儀 — daw_gui (親) が `create`、daw_audio が
//! `open`、書き手は RT スレッド 1 本、読み手は daw_gui 1 本、**上書きリング** —
//! を **長さ可変** (設定秒数 × サンプルレート) にしたもの。scope が 2.7 秒の
//! テレメトリなのに対し、こちらは「後から切り出す」ための素材そのもの。
//!
//! リングに加えて **走行セグメント** を持つ。「リングの何フレーム目から、曲の
//! どこ (playhead) を、どのテンポで再生していたか / 停止していたか」を、状態が
//! 変わったときと一定間隔で RT が 1 件ずつ記録する。GUI はこれで
//! リング座標 ↔ 曲の拍 ↔ wall-clock (MIDI Capture と同じ時計) を引く。
//!
//! RT 側がやるのは事前確保済み shmem への `Relaxed` store と、最後の
//! `write_frames` への `Release` store だけ (CLAUDE.md「Real-Time Audio の制約」)。
//! shmem 名は世代込み ([`sampler_shmem_id`]) — 長さ / 録音源を変えるたびに
//! 新世代を create し、旧世代は両プロセスが drop するまで別名で生き続ける
//! (同名の再利用は非同期解放と衝突する)。

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::Result;

use crate::shmem::NamedShmem;

/// 溜める長さの上限 (秒)。Rolling Sampler の 10 分と同じ。
pub const MAX_SECONDS: u32 = 600;
/// 溜める長さの既定 (秒)。原典 Global Sampler の `len_in_secs = 60`。
pub const DEFAULT_SECONDS: u32 = 60;

/// 走行セグメントの件数。RT は状態変化 + [`SEGMENT_RESYNC_FRAMES`] ごとに 1 件
/// 押すので、最短でも `2 秒 × 1024 = 2048 秒 > MAX_SECONDS` の履歴を保つ。
pub const SEGMENTS: usize = 1024;
const SEGMENT_MASK: u64 = (SEGMENTS as u64) - 1;

/// 状態が変わらなくてもこのフレーム数ごとにセグメントを押し直す。
/// wall-clock とオーディオクロックのドリフト (100ppm で 600 秒 = 60ms) を
/// 2 秒ごとの再同期で抑える。
pub const SEGMENT_RESYNC_FRAMES: u64 = 96_000;

/// `Segment::playhead_beat` の「停止中」sentinel (NaN ではなく明示の bit pattern)。
const STOPPED_BITS: u64 = u64::MAX;

/// 走行セグメント 1 件 (shmem 上の表現)。
#[repr(C)]
pub struct Segment {
    /// このセグメントが始まるリング絶対フレーム (`write_frames` 座標)。
    ring_frame: AtomicU64,
    /// そのときの wall-clock (UNIX epoch からの ns)。
    wall_ns: AtomicU64,
    /// そのときの曲位置 (拍、`f64::to_bits`)。停止中は [`STOPPED_BITS`]。
    playhead_beat_bits: AtomicU64,
    /// そのときの実効テンポ。区間内は `beat = playhead_beat + frames * bpm / 60 / sr`
    /// で進む (曲のテンポ表を後から編集しても、録った当時の小節位置が動かない)。
    bpm_bits: AtomicU32,
    _pad: AtomicU32,
}

/// 読み手が受け取るセグメント。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentInfo {
    pub ring_frame: u64,
    pub wall_ns: u64,
    /// `None` = 停止中。
    pub playhead_beat: Option<f64>,
    pub bpm: f32,
}

impl SegmentInfo {
    /// セグメント先頭から `frames` 進んだ位置の拍 (停止中は `None`)。
    pub fn beat_after(&self, frames: u64, sample_rate: u32) -> Option<f64> {
        let beat0 = self.playhead_beat?;
        Some(beat0 + frames as f64 * f64::from(self.bpm.max(1.0)) / 60.0 / f64::from(sample_rate.max(1)))
    }

    /// `beat` がセグメント先頭から何フレーム後か (負なら `None`)。
    pub fn frames_until(&self, beat: f64, sample_rate: u32) -> Option<u64> {
        let beat0 = self.playhead_beat?;
        let frames = (beat - beat0) * 60.0 / f64::from(self.bpm.max(1.0)) * f64::from(sample_rate.max(1));
        (frames >= 0.0).then_some(frames.round() as u64)
    }
}

#[repr(C)]
pub struct SamplerRingHeader {
    /// 累積書き込みフレーム数 (monotonic)。writer が 1 ブロック書き終えた最後に
    /// `Release` store し、reader は `Acquire` load してから中身を読む。
    write_frames: AtomicU64,
    /// リングのフレーム数 (create 時に固定)。
    capacity: AtomicU64,
    /// セグメントの累積 push 数。
    seg_write: AtomicU64,
    sample_rate: AtomicU32,
    /// GUI が書く。1 なら RT はリングへ書かない (一時停止)。
    paused: AtomicU32,
    segments: [Segment; SEGMENTS],
}

const HEADER_SIZE: usize = std::mem::size_of::<SamplerRingHeader>();

/// 共有メモリ領域の所有ハンドル。両プロセスがそれぞれ 1 つ持つ。
pub struct SamplerRingHandle {
    shmem: NamedShmem,
    capacity: usize,
}

/// `None` = 容量が大きすぎて size が溢れる (ヘッダから読んだ値の検証にも使う)。
fn shmem_size(capacity: usize) -> Option<usize> {
    capacity
        .checked_mul(std::mem::size_of::<[AtomicU32; 2]>())
        .and_then(|n| n.checked_add(HEADER_SIZE))
}

impl SamplerRingHandle {
    /// `seconds` 秒ぶんのリングを作る (daw_gui)。
    pub fn create(os_id: &str, seconds: u32, sample_rate: u32) -> Result<Self> {
        let seconds = seconds.clamp(1, MAX_SECONDS);
        let sr = if sample_rate == 0 { 48_000 } else { sample_rate };
        let capacity = seconds as usize * sr as usize;
        let size = shmem_size(capacity)
            .ok_or_else(|| anyhow::anyhow!("sampler ring {os_id}: capacity {capacity} too large"))?;
        let shmem = NamedShmem::create(os_id, size)?;
        // ページファイル裏付きの section は OS がゼロ初期化済み (全 atomic が 0 =
        // 無音・未書き込み) なので、数百 MB を GUI スレッドで塗り直さない。
        let h = Self { shmem, capacity };
        h.header().capacity.store(capacity as u64, Ordering::Release);
        h.header().sample_rate.store(sr, Ordering::Release);
        Ok(h)
    }

    /// 既存のリングを開く (daw_audio)。容量はヘッダから読む。
    pub fn open(os_id: &str) -> Result<Self> {
        let shmem = NamedShmem::open(os_id, HEADER_SIZE)?;
        // SAFETY: 少なくとも HEADER_SIZE バイトはマップ済み (open が検証)。
        let capacity = unsafe { &*(shmem.as_ptr() as *const SamplerRingHeader) }
            .capacity
            .load(Ordering::Acquire) as usize;
        anyhow::ensure!(capacity > 0, "sampler ring {os_id}: capacity is 0");
        // ヘッダの容量は共有メモリ越しの外部入力として扱う: 溢れ / mapping 不足を弾く。
        let need = shmem_size(capacity)
            .ok_or_else(|| anyhow::anyhow!("sampler ring {os_id}: capacity {capacity} too large"))?;
        anyhow::ensure!(
            shmem.len() >= need,
            "sampler ring {os_id}: mapping too small for capacity {capacity}"
        );
        Ok(Self { shmem, capacity })
    }

    fn header(&self) -> &SamplerRingHeader {
        // SAFETY: マッピングは少なくとも `shmem_size(capacity)` バイト (create/open が
        // 検証)、`MapViewOfFile` のポインタは 64KiB 境界なので `AtomicU64` の
        // アラインを満たす。全フィールドが atomic = どんなビット列も有効。
        unsafe { &*(self.shmem.as_ptr() as *const SamplerRingHeader) }
    }

    fn slot(&self, frame: u64) -> &[AtomicU32; 2] {
        let idx = (frame % self.capacity as u64) as usize;
        // SAFETY: `idx < capacity` で、サンプル領域は header 直後に capacity 個
        // 確保されている (`shmem_size`)。
        unsafe {
            &*(self.shmem.as_ptr().add(HEADER_SIZE) as *const [AtomicU32; 2]).add(idx)
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn sample_rate(&self) -> u32 {
        self.header().sample_rate.load(Ordering::Acquire)
    }

    pub fn write_frames(&self) -> u64 {
        self.header().write_frames.load(Ordering::Acquire)
    }

    /// 読める最古のフレーム (これより前は上書き済み)。
    pub fn oldest_frame(&self) -> u64 {
        self.write_frames().saturating_sub(self.capacity as u64)
    }

    pub fn set_paused(&self, paused: bool) {
        self.header().paused.store(u32::from(paused), Ordering::Release);
    }

    pub fn paused(&self) -> bool {
        self.header().paused.load(Ordering::Acquire) != 0
    }

    /// **RT スレッド専用**。1 ブロックをリングへ書く。確保もロックもしない。
    /// `l` / `r` の短い方に合わせて書く。
    pub fn write_block(&self, l: &[f32], r: &[f32]) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        let h = self.header();
        let base = h.write_frames.load(Ordering::Relaxed);
        for i in 0..n {
            let s = self.slot(base + i as u64);
            s[0].store(l[i].to_bits(), Ordering::Relaxed);
            s[1].store(r[i].to_bits(), Ordering::Relaxed);
        }
        h.write_frames.store(base + n as u64, Ordering::Release);
    }

    /// **RT スレッド専用**。走行セグメントを 1 件押す。`ring_frame` は通常
    /// `write_frames()` (= これから書くブロックの先頭)。
    pub fn push_segment(&self, seg: SegmentInfo) {
        let h = self.header();
        let n = h.seg_write.load(Ordering::Relaxed);
        let s = &h.segments[(n & SEGMENT_MASK) as usize];
        s.ring_frame.store(seg.ring_frame, Ordering::Relaxed);
        s.wall_ns.store(seg.wall_ns, Ordering::Relaxed);
        s.playhead_beat_bits.store(
            seg.playhead_beat.map_or(STOPPED_BITS, f64::to_bits),
            Ordering::Relaxed,
        );
        s.bpm_bits.store(seg.bpm.to_bits(), Ordering::Relaxed);
        h.seg_write.store(n + 1, Ordering::Release);
    }

    /// 1 フレーム読む (RT の試聴用。範囲検証は呼び側)。
    pub fn read_frame(&self, frame: u64) -> (f32, f32) {
        let s = self.slot(frame);
        (
            f32::from_bits(s[0].load(Ordering::Relaxed)),
            f32::from_bits(s[1].load(Ordering::Relaxed)),
        )
    }

    /// `[start, end)` を `out` へ push する (clear しない)。読み終えた時点で
    /// `start` がまだ上書きされていなければ `Ok`、押し出されていれば `Err` で
    /// `out` は読む前の長さに戻す。
    pub fn read_range(&self, start: u64, end: u64, out: &mut Vec<[f32; 2]>) -> Result<(), RingOverrun> {
        let write = self.write_frames();
        if start >= end || end > write || start < self.safe_oldest_frame() {
            return Err(RingOverrun);
        }
        let before = out.len();
        out.reserve((end - start) as usize);
        for f in start..end {
            out.push(self.read_frame(f).into());
        }
        // 読んでいる間に一周されていたら中身は混ざっている。
        if start < self.safe_oldest_frame() {
            out.truncate(before);
            return Err(RingOverrun);
        }
        Ok(())
    }

    /// 読んで安全な最古フレーム。`write_frames` は 1 ブロック書き終えてから publish
    /// されるので、publish 前の書き込み中ブロック (最大 `MAX_FRAMES`) が `oldest_frame`
    /// 直後の slot を上書きしている最中でありうる。その帯を除いた位置。
    pub fn safe_oldest_frame(&self) -> u64 {
        let write = self.write_frames();
        if write <= self.capacity as u64 {
            // まだ一周していない: 書き込み中の slot は未使用領域で、読める frame と
            // 重ならない。
            return 0;
        }
        self.oldest_frame()
            .saturating_add(crate::process_data::MAX_FRAMES as u64)
            .min(write)
    }

    /// セグメントの累積 push 数 (変化検出用)。
    pub fn segment_count(&self) -> u64 {
        self.header().seg_write.load(Ordering::Acquire)
    }

    /// 生きているセグメントを古い順に `out` へ push する (clear する)。
    pub fn segments(&self, out: &mut Vec<SegmentInfo>) {
        out.clear();
        let h = self.header();
        let n = h.seg_write.load(Ordering::Acquire);
        let first = n.saturating_sub(SEGMENTS as u64);
        for i in first..n {
            let s = &h.segments[(i & SEGMENT_MASK) as usize];
            let beat_bits = s.playhead_beat_bits.load(Ordering::Relaxed);
            out.push(SegmentInfo {
                ring_frame: s.ring_frame.load(Ordering::Relaxed),
                wall_ns: s.wall_ns.load(Ordering::Relaxed),
                playhead_beat: (beat_bits != STOPPED_BITS).then(|| f64::from_bits(beat_bits)),
                bpm: f32::from_bits(s.bpm_bits.load(Ordering::Relaxed)),
            });
        }
    }

    /// 前回以降に書かれたフレームを読み進める reader。
    pub fn reader(&self) -> RingReader {
        RingReader { cursor: self.write_frames() }
    }
}

// 全フィールドが lock-free atomic で、読み手はどんな観測値も許容する。
unsafe impl Send for SamplerRingHandle {}
unsafe impl Sync for SamplerRingHandle {}

/// 切り出したい範囲がリングから押し出されていた。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingOverrun;

impl std::fmt::Display for RingOverrun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("選択した範囲はもうリングに残っていません")
    }
}

impl std::error::Error for RingOverrun {}

/// 波形オーバービュー用の逐次 reader (daw_gui の poller が持つ)。
pub struct RingReader {
    cursor: u64,
}

impl RingReader {
    /// `cursor` から書き込み位置までを **最大 `max_frames`** だけ読み、
    /// `(読んだ先頭フレーム, 読んだ数)` を返す。一周されていたら最古へ飛ぶ。
    pub fn read(
        &mut self,
        handle: &SamplerRingHandle,
        max_frames: usize,
        out: &mut Vec<[f32; 2]>,
    ) -> (u64, usize) {
        let write = handle.write_frames();
        if write <= self.cursor {
            self.cursor = write;
            return (write, 0);
        }
        let oldest = handle.oldest_frame();
        let mut start = self.cursor.max(oldest);
        if write - start > max_frames as u64 {
            start = write - max_frames as u64;
        }
        for f in start..write {
            out.push(handle.read_frame(f).into());
        }
        self.cursor = write;
        (start, (write - start) as usize)
    }

}

/// shmem 名。**世代込み** — 長さ / 録音源の変更ごとに `generation` を進める。
pub fn sampler_shmem_id(parent_pid: u32, generation: u32) -> String {
    format!("daw_01_sampler_{parent_pid}_{generation}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(seconds: u32, sr: u32) -> SamplerRingHandle {
        let id = format!("daw_01_sampler_test_{}_{}", std::process::id(), rand_suffix());
        SamplerRingHandle::create(&id, seconds, sr).unwrap()
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::AtomicU64;
        static N: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        t ^ N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn write_then_read_range_roundtrip() {
        let h = ring(1, 100); // 100 frames
        let l: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let r: Vec<f32> = (0..10).map(|i| -(i as f32)).collect();
        h.write_block(&l, &r);
        h.write_block(&l, &r);
        assert_eq!(h.write_frames(), 20);
        let mut out = Vec::new();
        h.read_range(5, 15, &mut out).unwrap();
        assert_eq!(out.len(), 10);
        assert_eq!(out[0], [5.0, -5.0]);
        assert_eq!(out[5], [0.0, 0.0]); // 2 ブロック目の先頭
    }

    #[test]
    fn read_range_fails_after_overwrite() {
        let h = ring(1, 4096);
        let block = vec![1.0f32; 3000];
        h.write_block(&block, &block);
        h.write_block(&block, &block); // 6000 frames written, oldest = 1904
        let guard = crate::process_data::MAX_FRAMES as u64;
        assert_eq!(h.safe_oldest_frame(), 1904 + guard);
        let mut out = Vec::new();
        assert_eq!(h.read_range(0, 10, &mut out), Err(RingOverrun));
        // 書き込み中でありうる帯 (oldest 直後の 1 ブロック) も弾く
        assert_eq!(h.read_range(1904, 2000, &mut out), Err(RingOverrun));
        assert!(out.is_empty());
        assert!(h.read_range(1904 + guard, 6000, &mut out).is_ok());
        assert_eq!(out.len() as u64, 6000 - 1904 - guard);
        // 範囲外 (未来) も弾く
        assert_eq!(h.read_range(5000, 6100, &mut Vec::new()), Err(RingOverrun));
    }

    #[test]
    fn reader_follows_write_and_skips_to_oldest_after_wrap() {
        let h = ring(1, 100);
        let mut rd = h.reader();
        let block = vec![0.5f32; 30];
        h.write_block(&block, &block);
        let mut out = Vec::new();
        assert_eq!(rd.read(&h, 1000, &mut out), (0, 30));
        // 130 frames more → 一周 (capacity 100) → 最古 = 60
        for _ in 0..5 {
            h.write_block(&block, &block);
        }
        out.clear();
        let (start, n) = rd.read(&h, 1000, &mut out);
        assert_eq!((start, n), (80, 100));
        // max_frames で切り詰めると末尾側が残る
        h.write_block(&block, &block);
        out.clear();
        assert_eq!(rd.read(&h, 10, &mut out), (200, 10));
    }

    #[test]
    fn segments_come_back_oldest_first_with_stopped_sentinel() {
        let h = ring(1, 100);
        h.push_segment(SegmentInfo { ring_frame: 0, wall_ns: 10, playhead_beat: None, bpm: 120.0 });
        h.push_segment(SegmentInfo { ring_frame: 50, wall_ns: 20, playhead_beat: Some(7.5), bpm: 90.0 });
        let mut out = Vec::new();
        h.segments(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].playhead_beat, None);
        assert_eq!(out[1], SegmentInfo { ring_frame: 50, wall_ns: 20, playhead_beat: Some(7.5), bpm: 90.0 });
        // 区間内の拍は録った bpm で進む: 90bpm / 100Hz → 1 拍 = 66.67 frame
        let b = out[1].beat_after(200, 100).unwrap();
        assert!((b - 10.5).abs() < 1e-9, "{b}");
        assert_eq!(out[1].frames_until(10.5, 100), Some(200));
        assert_eq!(out[1].frames_until(7.0, 100), None);
    }

    #[test]
    fn open_reads_capacity_from_header() {
        let id = format!("daw_01_sampler_test_open_{}_{}", std::process::id(), rand_suffix());
        let a = SamplerRingHandle::create(&id, 2, 100).unwrap();
        let b = SamplerRingHandle::open(&id).unwrap();
        assert_eq!(b.capacity(), 200);
        assert_eq!(b.sample_rate(), 100);
        a.set_paused(true);
        assert!(b.paused());
    }
}
