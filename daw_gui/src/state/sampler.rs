//! Global Sampler の GUI 側状態 (`docs/plan_global_sampler.md` §3.3)。
//!
//! - [`SamplerState`]: AppData が持つ「いま開いているリング」と選択 / 一時停止 /
//!   試聴 / 波形オーバービュー。session-only、保存対象は `UiPrefs::sampler_seconds` のみ。
//! - [`SamplerShared`]: テレメトリスレッド (playhead poller) と共有する現世代のリング。
//!   poller は 33ms ごとにリングを読み進めてバケツ (`BUCKET_FRAMES` ごとの min/max)
//!   を作り `SamplerEvent::Tick` で GUI へ流す。
//! - [`OverviewBuilder`]: poller 側の逐次バケツ化。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use common::protocol::SamplerSource;
use common::sampler_ring::{RingReader, SamplerRingHandle, SegmentInfo};

/// 波形オーバービューの 1 バケツのフレーム数。48kHz で 5.3ms。600 秒でも
/// 112,500 バケツ = 900KB。
pub const BUCKET_FRAMES: u64 = 256;

/// poller と共有する現世代のリング。`None` = 開いていない。
#[derive(Default)]
pub struct SamplerShared {
    pub ring: Mutex<Option<(u32, Arc<SamplerRingHandle>)>>,
}

impl SamplerShared {
    pub fn current(&self) -> Option<(u32, Arc<SamplerRingHandle>)> {
        self.ring.lock().ok().and_then(|g| g.clone())
    }
}

/// [`SamplerEvent::Tick`](crate::event_sampler::SamplerEvent::Tick) の中身。
#[derive(Debug, Clone, PartialEq)]
pub struct SamplerTick {
    pub generation: u32,
    /// `buckets[0]` の絶対バケツ番号 (= リングフレーム / `BUCKET_FRAMES`)。
    pub first_bucket: u64,
    pub buckets: Vec<(f32, f32)>,
    pub write_frames: u64,
    /// セグメントが増えたときだけ `Some` (全件)。
    pub segments: Option<Vec<SegmentInfo>>,
}

/// GUI が持つオーバービューのリング (絶対バケツ番号 `% len` で添字)。
#[derive(Debug, Default, Clone)]
pub struct Overview {
    pub buckets: Vec<(f32, f32)>,
    /// 直近に受け取った末尾バケツ番号 + 1 (= 有効範囲の右端)。
    pub end_bucket: u64,
}

impl Overview {
    pub fn with_capacity_frames(capacity: usize) -> Self {
        let n = (capacity as u64).div_ceil(BUCKET_FRAMES) as usize;
        Self { buckets: vec![(0.0, 0.0); n.max(1)], end_bucket: 0 }
    }

    pub fn apply(&mut self, first: u64, incoming: &[(f32, f32)]) {
        let n = self.buckets.len() as u64;
        for (i, b) in incoming.iter().enumerate() {
            let idx = first + i as u64;
            self.buckets[(idx % n) as usize] = *b;
        }
        self.end_bucket = self.end_bucket.max(first + incoming.len() as u64);
    }

    /// 絶対バケツ番号 `i` の (min, max)。有効範囲外は無音。
    pub fn get(&self, i: u64) -> (f32, f32) {
        let n = self.buckets.len() as u64;
        if i >= self.end_bucket || i + n < self.end_bucket {
            return (0.0, 0.0);
        }
        self.buckets[(i % n) as usize]
    }
}

pub struct SamplerState {
    /// 現世代のリング (世代 + handle) の **唯一の置き場**。GUI スレッドと poller が
    /// 共有する。世代は `sampler_ring::sampler_shmem_id` の suffix、0 = 未 open。
    pub shared: Arc<SamplerShared>,
    pub source: SamplerSource,
    pub paused: bool,
    /// 選択範囲 `[start, end)` (リング絶対フレーム)。時間とともに左へ流れ、
    /// 左端から出たら消える。
    pub selection: Option<(u64, u64)>,
    /// 試聴中 (engine が範囲を読み終える時刻まで)。
    pub preview_until: Option<std::time::Instant>,
    pub overview: Overview,
    /// 直近の `write_frames` (描画の右端 = 今)。
    pub write_frames: u64,
    pub segments: Vec<SegmentInfo>,
}

impl SamplerState {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(SamplerShared::default()),
            source: SamplerSource::Master,
            paused: false,
            selection: None,
            preview_until: None,
            overview: Overview::default(),
            write_frames: 0,
            segments: Vec::new(),
        }
    }

    /// 現世代のリング (`None` = 未 open)。
    pub fn ring(&self) -> Option<Arc<SamplerRingHandle>> {
        self.shared.current().map(|(_, r)| r)
    }

    /// 現世代 (0 = 未 open)。
    pub fn generation(&self) -> u32 {
        self.shared.current().map_or(0, |(g, _)| g)
    }

    /// 新世代を据える (poller は次の tick から新しい reader で読む)。
    pub fn install(&mut self, generation: u32, ring: Arc<SamplerRingHandle>) {
        if let Ok(mut g) = self.shared.ring.lock() {
            *g = Some((generation, ring));
        }
    }

    pub fn capacity(&self) -> u64 {
        self.ring().map_or(0, |r| r.capacity() as u64)
    }

    pub fn sample_rate(&self) -> u32 {
        self.ring().map_or(48_000, |r| r.sample_rate())
    }

    /// 選択がリングから押し出されていたら消す。
    pub fn prune_selection(&mut self) {
        let oldest = self.write_frames.saturating_sub(self.capacity());
        if let Some((s, _)) = self.selection
            && s < oldest
        {
            self.selection = None;
        }
    }
}

impl Default for SamplerState {
    fn default() -> Self {
        Self::new()
    }
}

/// poller 側: リングを読み進めて `BUCKET_FRAMES` ごとの (min, max) を作る。
pub struct OverviewBuilder {
    generation: u32,
    reader: RingReader,
    /// 進行中バケツ `(番号, min, max, 積んだフレーム数)`。
    partial: Option<(u64, f32, f32, u64)>,
    last_segment_count: u64,
    buf: Vec<[f32; 2]>,
}

impl OverviewBuilder {
    pub fn new(generation: u32, ring: &SamplerRingHandle) -> Self {
        Self {
            generation,
            reader: ring.reader(),
            partial: None,
            last_segment_count: 0,
            // 1 tick の読み上限 (= 1 秒ぶん) を最初から確保しておく。
            buf: Vec::with_capacity(ring.sample_rate().max(1) as usize),
        }
    }

    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// 1 tick ぶん読み、送るものがあれば `Some`。`max_frames` は 1 tick で読む上限
    /// (省電力からの復帰で一気に読み過ぎない)。
    pub fn tick(&mut self, ring: &SamplerRingHandle, max_frames: usize) -> Option<SamplerTick> {
        self.buf.clear();
        let (start, n) = self.reader.read(ring, max_frames, &mut self.buf);
        let write_frames = ring.write_frames();
        let mut out: Vec<(f32, f32)> = Vec::new();
        let mut first_bucket = start / BUCKET_FRAMES;
        if n > 0 {
            // 読み飛ばし (一周された) があれば進行中バケツは捨てる。
            if let Some((b, _, _, cnt)) = self.partial
                && b * BUCKET_FRAMES + cnt != start
            {
                self.partial = None;
            }
            if let Some((b, ..)) = self.partial {
                first_bucket = b;
            }
            for (i, s) in self.buf.iter().enumerate() {
                let frame = start + i as u64;
                fold_frame(&mut self.partial, frame, s[0].min(s[1]), s[0].max(s[1]), &mut out);
            }
        }
        let seg_count = ring.segment_count();
        let segments = (seg_count != self.last_segment_count).then(|| {
            self.last_segment_count = seg_count;
            let mut v = Vec::new();
            ring.segments(&mut v);
            v
        });
        if out.is_empty() && segments.is_none() && n == 0 {
            return None;
        }
        Some(SamplerTick {
            generation: self.generation,
            first_bucket,
            buckets: out,
            write_frames,
            segments,
        })
    }
}

/// 1 フレームを進行中バケツへ畳み込み、バケツが満ちたら `out` へ出す。
fn fold_frame(
    partial: &mut Option<(u64, f32, f32, u64)>,
    frame: u64,
    lo: f32,
    hi: f32,
    out: &mut Vec<(f32, f32)>,
) {
    let bucket = frame / BUCKET_FRAMES;
    let (mn, mx, cnt) = match partial {
        Some((b, mn, mx, cnt)) if *b == bucket => (mn.min(lo), mx.max(hi), *cnt + 1),
        _ => (lo, hi, 1),
    };
    if frame % BUCKET_FRAMES == BUCKET_FRAMES - 1 || cnt == BUCKET_FRAMES {
        out.push((mn, mx));
        *partial = None;
    } else {
        *partial = Some((bucket, mn, mx, cnt));
    }
}

/// MIDI Capture 側と共用の wall-clock (UNIX ns)。engine のセグメントと同じ時計。
pub fn wall_clock_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// 選択範囲を持ち運ぶ drag payload (`Ui::begin_drag`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplerDragPayload {
    pub start_frame: u64,
    pub end_frame: u64,
}

pub const SAMPLER_DRAG_KIND: &str = "daw_01.sampler_range";

/// 描画に使う「リング座標 → x」の写像 (右端 = 今)。
#[derive(Debug, Clone, Copy)]
pub struct RingAxis {
    pub x: f32,
    pub w: f32,
    pub write_frames: u64,
    pub capacity: u64,
}

impl RingAxis {
    pub fn oldest(&self) -> u64 {
        self.write_frames.saturating_sub(self.capacity)
    }

    pub fn frame_to_x(&self, frame: u64) -> f32 {
        if self.capacity == 0 {
            return self.x;
        }
        let rel = frame as f64 - self.oldest() as f64;
        self.x + (rel / self.capacity as f64) as f32 * self.w
    }

    pub fn x_to_frame(&self, x: f32) -> u64 {
        if self.w <= 0.0 || self.capacity == 0 {
            return self.write_frames;
        }
        let t = ((x - self.x) / self.w).clamp(0.0, 1.0) as f64;
        // リングが一周する前は右端が write head より先 (未来 = 未書き込み) に写るので、
        // 選択が書き込み済みの範囲を超えないよう clamp する。
        (self.oldest() + (t * self.capacity as f64).round() as u64).min(self.write_frames)
    }
}

/// wall-clock `at_ns` に曲がどの拍を再生していたか (セグメントから)。
/// 停止中 / セグメント無しは `None`。
pub fn beat_at_wall_ns(segments: &[SegmentInfo], at_ns: u64, sample_rate: u32) -> Option<f64> {
    let seg = segments.iter().rev().find(|s| s.wall_ns <= at_ns)?;
    let frames = (at_ns - seg.wall_ns) as u128 * u128::from(sample_rate.max(1)) / 1_000_000_000;
    seg.beat_after(frames as u64, sample_rate)
}

/// 描画用の 1 セグメント (`[start_frame, end_frame)` の間、曲位置は線形に進む)。
pub fn segment_spans(segments: &[SegmentInfo], end: u64) -> VecDeque<(u64, u64, &SegmentInfo)> {
    let mut out = VecDeque::new();
    for (i, s) in segments.iter().enumerate() {
        let next = segments.get(i + 1).map_or(end, |n| n.ring_frame);
        if next > s.ring_frame {
            out.push_back((s.ring_frame, next.min(end), s));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_ring_wraps_and_masks_stale_entries() {
        let mut ov = Overview::with_capacity_frames((BUCKET_FRAMES * 4) as usize);
        ov.apply(0, &[(0.0, 1.0), (0.0, 2.0)]);
        ov.apply(2, &[(0.0, 3.0), (0.0, 4.0), (0.0, 5.0)]);
        assert_eq!(ov.get(4), (0.0, 5.0));
        assert_eq!(ov.get(1), (0.0, 2.0));
        // 5 個目で 0 番は押し出された (capacity 4)
        assert_eq!(ov.get(0), (0.0, 0.0));
        assert_eq!(ov.get(9), (0.0, 0.0));
    }

    #[test]
    fn builder_emits_full_buckets_only_and_carries_partial() {
        let id = format!("daw_01_sampler_gui_test_{}_{}", std::process::id(), wall_clock_ns());
        let ring = SamplerRingHandle::create(&id, 1, 4096).unwrap();
        let mut b = OverviewBuilder::new(1, &ring);
        let half = vec![0.5f32; (BUCKET_FRAMES / 2) as usize];
        ring.write_block(&half, &half);
        // 半バケツ: セグメントも無いので何も出ない (partial に持ち越し)
        let t = b.tick(&ring, 100_000);
        assert!(t.is_none_or(|t| t.buckets.is_empty()));
        let neg = vec![-0.25f32; (BUCKET_FRAMES / 2) as usize];
        ring.write_block(&neg, &neg);
        let t = b.tick(&ring, 100_000).unwrap();
        assert_eq!(t.first_bucket, 0);
        assert_eq!(t.buckets, vec![(-0.25, 0.5)]);
    }

    #[test]
    fn ring_axis_roundtrip_and_beat_lookup() {
        let ax = RingAxis { x: 10.0, w: 100.0, write_frames: 1000, capacity: 500 };
        assert_eq!(ax.frame_to_x(500), 10.0);
        assert_eq!(ax.frame_to_x(1000), 110.0);
        assert_eq!(ax.x_to_frame(60.0), 750);
        let segs = vec![
            SegmentInfo { ring_frame: 0, wall_ns: 0, playhead_beat: None, bpm: 120.0 },
            SegmentInfo { ring_frame: 600, wall_ns: 5_000, playhead_beat: Some(16.0), bpm: 120.0 },
        ];
        assert_eq!(beat_at_wall_ns(&segs, 4_000, 48_000), None, "停止区間");
        // 5,000ns から 1 秒後 = 120bpm で 2 拍
        let b = beat_at_wall_ns(&segs, 1_000_005_000, 48_000).unwrap();
        assert!((b - 18.0).abs() < 1e-6, "{b}");
    }
}
