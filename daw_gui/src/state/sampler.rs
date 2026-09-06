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
    /// 選択範囲 `[start, end)` (リング絶対フレーム)。スイープ表示ではその場に留まり、
    /// 書き込み位置に上書きされた (リングから押し出された) ら消える。
    pub selection: Option<(u64, u64)>,
    /// スイープ表示の位相 (リング 1 周に対する割合 `[0, 1)`)。折り返し点をまたぐ範囲を
    /// 選べるように、ヘッダの「半周ずらす」/ 波形上のホイールで動かす。session のみ。
    pub sweep_shift: f32,
    /// 試聴中 (engine が範囲を読み終える時刻まで)。
    pub preview_until: Option<std::time::Instant>,
    pub overview: Overview,
    /// 直近の `write_frames` (= 書き込み位置)。
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
            sweep_shift: 0.0,
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

/// 描画に使う「リング座標 → x」の写像。**スイープ表示**: 左端 = リングの物理位置 0、
/// 書き込み位置が左から右へ進み、右端に着いたら左へ戻る (オシロスコープの sweep)。
///
/// スクロールしないので、鳴っている最中でも画面上の波形が流れず、一時停止せずに
/// 範囲選択できる。書き込み位置より **左が今の周回 (新しい)、右が前の周回 (古い)**。
/// ある x に居る絶対 frame は、書き込み位置がそこを通り過ぎるまで (= 1 周) 変わらない
/// ので、選択も小節線もその場に留まる。
///
/// `offset` は表示の位相 (frame、`[0, capacity)`): 物理位置 `p` を `(p + offset) % capacity`
/// の x に描く。折り返し点をまたぐ範囲はドラッグで選べないので、位相をずらして
/// 折り返し点を動かす (ヘッダの「半周ずらす」/ 波形上のホイール)。
#[derive(Debug, Clone, Copy)]
pub struct RingAxis {
    pub x: f32,
    pub w: f32,
    pub write_frames: u64,
    pub capacity: u64,
    pub offset: u64,
}

impl RingAxis {
    pub fn oldest(&self) -> u64 {
        self.write_frames.saturating_sub(self.capacity)
    }

    /// 書き込み位置のリング内物理位置 `[0, capacity)`。
    pub fn head_phys(&self) -> u64 {
        if self.capacity == 0 { 0 } else { self.write_frames % self.capacity }
    }

    /// 物理位置 → 表示位置 (位相 `offset` を足して折り返す)。
    fn phys_to_disp(&self, phys: u64) -> u64 {
        if self.capacity == 0 { 0 } else { (phys + self.offset % self.capacity) % self.capacity }
    }

    /// 表示位置 → 物理位置。`disp == capacity` (右端ぴったり) は `capacity` のまま返す
    /// (`offset == 0` のとき「前の周回の終端」として `phys_to_frame` が解く)。
    fn disp_to_phys(&self, disp: u64) -> u64 {
        if self.capacity == 0 {
            return 0;
        }
        let off = self.offset % self.capacity;
        if off == 0 { disp.min(self.capacity) } else { (disp.min(self.capacity) + self.capacity - off) % self.capacity }
    }

    /// 今の周回の物理位置 0 に当たる絶対 frame。
    fn lap_start(&self) -> u64 {
        self.write_frames - self.head_phys()
    }

    /// 物理位置 → 絶対 frame。書き込み位置より左は今の周回、右は前の周回。
    /// 前の周回がまだ無い (最初の周回で head より右) なら `None` (未書き込み)。
    /// `phys == capacity` (右端ぴったり) は前の周回の終端 = 今の周回の先頭。
    pub fn phys_to_frame(&self, phys: u64) -> Option<u64> {
        if self.capacity == 0 {
            return None;
        }
        let phys = phys.min(self.capacity);
        let head = self.head_phys();
        let lap = self.lap_start();
        if phys <= head {
            Some(lap + phys)
        } else {
            lap.checked_sub(self.capacity).map(|prev| prev + phys)
        }
    }

    /// 物理位置 → x (位相込み)。
    pub fn phys_to_x(&self, phys: u64) -> f32 {
        if self.capacity == 0 {
            return self.x;
        }
        self.x + (self.phys_to_disp(phys) as f64 / self.capacity as f64) as f32 * self.w
    }

    pub fn frame_to_x(&self, frame: u64) -> f32 {
        if self.capacity == 0 {
            return self.x;
        }
        self.phys_to_x(frame % self.capacity)
    }

    /// x → 物理位置 (位相を戻す)。
    pub fn x_to_phys(&self, x: f32) -> u64 {
        if self.w <= 0.0 || self.capacity == 0 {
            return 0;
        }
        let t = ((x - self.x) / self.w).clamp(0.0, 1.0) as f64;
        self.disp_to_phys((t * self.capacity as f64).round() as u64)
    }

    /// x → 絶対 frame。未書き込みの位置 (最初の周回で head より右) は書き込み位置へ
    /// clamp する (選択が書き込み済みの範囲を超えない)。
    pub fn x_to_frame(&self, x: f32) -> u64 {
        self.phys_to_frame(self.x_to_phys(x)).unwrap_or(self.write_frames).min(self.write_frames)
    }

    /// 絶対 frame 区間 `[start, end)` を x 区間に写す。右端で折り返す区間は 2 本
    /// (`[start.., 右端]` と `[左端, ..end]`)、それ以外は 1 本。リングから押し出された
    /// 部分は切り落とす。
    pub fn x_spans(&self, start: u64, end: u64) -> [Option<(f32, f32)>; 2] {
        if self.capacity == 0 || end <= start {
            return [None, None];
        }
        let start = start.max(self.oldest());
        if end <= start {
            return [None, None];
        }
        let len = (end - start).min(self.capacity);
        let ds = self.phys_to_disp(start % self.capacity);
        let de = ds + len;
        let disp_x = |d: u64| self.x + (d as f64 / self.capacity as f64) as f32 * self.w;
        if de <= self.capacity {
            [Some((disp_x(ds), disp_x(de))), None]
        } else {
            [Some((disp_x(ds), self.x + self.w)), Some((self.x, disp_x(de - self.capacity)))]
        }
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
        // スイープ表示: write=1000 / cap=500 → head は物理 0 (左端)。
        let ax = RingAxis { x: 10.0, w: 100.0, write_frames: 1000, capacity: 500, offset: 0 };
        assert_eq!(ax.frame_to_x(500), 10.0);
        assert_eq!(ax.frame_to_x(1000), 10.0, "head は左端 (折り返した直後)");
        assert_eq!(ax.frame_to_x(750), 60.0);
        // head より右は前の周回: 物理 250 → 1000 - 500 + 250。
        assert_eq!(ax.x_to_frame(60.0), 750);
        // 右端ぴったりは前の周回の終端 (= 今の周回の先頭 = 1000)。
        assert_eq!(ax.x_to_frame(110.0), 1000);
        // head が途中 (物理 100): 左は今の周回、右は前の周回。
        let ax = RingAxis { x: 10.0, w: 100.0, write_frames: 1100, capacity: 500, offset: 0 };
        assert_eq!(ax.x_to_frame(20.0), 1050, "head の左 = 今の周回");
        assert_eq!(ax.x_to_frame(90.0), 1000 - 500 + 400, "head の右 = 前の周回");
        // 最初の周回で head より右は未書き込み → head へ clamp。
        let ax = RingAxis { x: 10.0, w: 100.0, write_frames: 100, capacity: 500, offset: 0 };
        assert_eq!(ax.x_to_frame(90.0), 100);
        // 折り返す区間は 2 本に割れる (`[900, 1100)` = 物理 400..500 + 0..100)。
        let ax = RingAxis { x: 10.0, w: 100.0, write_frames: 1100, capacity: 500, offset: 0 };
        let [a, b] = ax.x_spans(900, 1100);
        assert_eq!(a, Some((90.0, 110.0)));
        assert_eq!(b, Some((10.0, 30.0)));
        assert_eq!(ax.x_spans(600, 900), [Some((30.0, 90.0)), None]);
        // 位相を半周ずらすと同じ区間が 1 本になり、x → frame も往復する。
        let ax = RingAxis { x: 10.0, w: 100.0, write_frames: 1100, capacity: 500, offset: 250 };
        assert_eq!(ax.x_spans(900, 1100), [Some((40.0, 80.0)), None]);
        for x in [15.0, 40.0, 60.0, 80.0, 105.0] {
            let f = ax.x_to_frame(x);
            assert!((ax.frame_to_x(f) - x).abs() < 0.5, "x={x} → {f} → {}", ax.frame_to_x(f));
        }
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
