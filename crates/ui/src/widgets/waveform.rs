//! 波形表示ウィジェット (`Ui::waveform`)。
//!
//! 設計の要点 (詳細は `docs/plan.md`「波形表示 UI 詳細設計」):
//! - 入力は **生サンプルの借用** (`SampleSlices<'s>`) と `valid_len` + `generation`。
//!   `generation` が一致すれば内部 LOD ピラミッドを再利用する。
//! - LOD ピラミッド (`WaveformPyramid`) は `WidgetState` の blanket impl 経由で
//!   `UiHost.state: HashMap<WidgetId, Box<dyn WidgetState>>` に乗る。
//!   ユーザ Model 型に `Clone`/`PartialEq`/`Hash`/`Default` は要求しない。
//! - M2 では `WaveformRenderMode::PeakLines` のみ実装 (RMS / SamplePolyline / Auto は M5)。
//! - M2 では完全再構築 (`generation` 変化で全レベル作り直し) のみ。
//!   インクリメンタル拡張 (録音中の `valid_len` 拡大) は後段で。

use std::hash::Hash;

use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect};

use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, hovered, pressed_inside};

// ============================================================
// Public types
// ============================================================

/// 1 ピラミッド最終レベルが目指すペア数の下限。
/// このより細かいレベルだけ作る (= 最終レベルの pairs_per_channel < この値)。
const PYRAMID_COARSE_THRESHOLD: usize = 256;

/// 各レベルの decimation 比率 (= 16)。
const DECIMATION: usize = 16;

/// 生サンプルの渡し方。すべて借用のみ — `Clone` は要求しない。
#[derive(Debug, Clone, Copy)]
pub enum SampleSlices<'s> {
    /// 1 ch のみ。
    Mono(&'s [f32]),
    /// 各チャンネルを別スライスで (planar)。
    Planar(&'s [&'s [f32]]),
    /// インターリーブ (frame ごとに channels 個ずつ並ぶ)。
    Interleaved { data: &'s [f32], channels: usize },
}

impl<'s> SampleSlices<'s> {
    pub fn channels(&self) -> usize {
        match self {
            Self::Mono(_) => 1,
            Self::Planar(planes) => planes.len(),
            Self::Interleaved { channels, .. } => *channels,
        }
    }

    /// (channels, ch でアクセス可能な最大サンプル数) を返す。
    /// `valid_len` のクランプに使う。
    fn max_frames(&self, ch: usize) -> usize {
        match self {
            Self::Mono(s) => {
                if ch == 0 { s.len() } else { 0 }
            }
            Self::Planar(planes) => planes.get(ch).map_or(0, |p| p.len()),
            Self::Interleaved { data, channels } => {
                if *channels == 0 || ch >= *channels {
                    0
                } else {
                    data.len() / *channels
                }
            }
        }
    }
}

/// 波形の入力。借用のみで構成し、`generation` でキャッシュ無効化する。
#[derive(Debug, Clone, Copy)]
pub struct WaveformSource<'s> {
    pub samples: SampleSlices<'s>,
    /// 有効長 (フレーム数。録音中で `samples.len()` より小さい場合あり)。
    pub valid_len: usize,
    /// アプリが内容変更時にインクリメントする。一致なら LOD 再利用。
    pub generation: u64,
    pub sample_rate: u32,
}

/// 表示するサンプル範囲 + 縦ゲイン。アプリ側がスクロール/ズーム状態を所有する。
#[derive(Debug, Clone, Copy)]
pub struct WaveformView {
    pub start_sample: u64,
    pub len_samples: u64,
    pub vertical_gain: f32,
}

impl Default for WaveformView {
    fn default() -> Self {
        Self { start_sample: 0, len_samples: 0, vertical_gain: 1.0 }
    }
}

/// チャンネルレイアウト。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelLayout {
    /// チャンネルを縦に並べる (デフォルト)。
    Stack,
    /// 全チャンネルを重ねて描く (透過は呼び出し側が style.fg.a で調整)。
    Overlay,
    /// 1 ch のみ描く。
    FirstOnly,
}

/// 描画モード。M2 では `PeakLines` のみ実装、他は M5 で完成させる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaveformRenderMode {
    /// pixel あたり 1 本の縦線 (min..max)。クリップ表示の標準。
    PeakLines,
    /// pixel あたり ±RMS バー。M5 で実装。M2 では PeakLines にフォールバックする。
    RmsBars,
    /// 1 サンプル/pixel 以下にズーム時の折れ線。M5 で実装。M2 では PeakLines にフォールバック。
    SamplePolyline,
    /// samples_per_pixel から自動切替。M5 で完成。M2 では PeakLines として動く。
    Auto,
}

#[derive(Debug, Clone, Copy)]
pub struct WaveformStyle {
    pub fg: Color,
    /// `|sample| > 1.0` を強調するときの色。
    pub fg_clipped: Color,
    /// `RmsBars` モード用の塗り色 (M5)。
    pub fill: Option<Color>,
    /// 各チャンネルの中央線。`None` で描かない。
    pub baseline: Option<Color>,
    pub channel_layout: ChannelLayout,
    pub render_mode: WaveformRenderMode,
    pub line_width_px: f32,
}

impl Default for WaveformStyle {
    fn default() -> Self {
        Self {
            fg: Color::rgb(0.55, 0.78, 0.95),
            fg_clipped: Color::rgb(0.95, 0.45, 0.40),
            fill: None,
            baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.10)),
            channel_layout: ChannelLayout::Stack,
            render_mode: WaveformRenderMode::PeakLines,
            line_width_px: 1.0,
        }
    }
}

/// クリック/ドラッグでヒットした位置の情報。
#[derive(Debug, Clone, Copy)]
pub struct WaveformHit {
    /// クリップ先頭からの絶対サンプル index (= `view.start_sample + 局所 index`)。
    pub sample_index: u64,
    pub channel: usize,
    pub local_x_px: f32,
    pub local_y_px: f32,
}

/// `Ui::waveform` の戻り値。アプリ側が `Edit` を組み立てるのに使う。
#[derive(Debug, Clone, Copy, Default)]
pub struct WaveformResponse {
    pub hovered: bool,
    pub clicked_at: Option<WaveformHit>,
    pub dragging_at: Option<WaveformHit>,
}

// ============================================================
// Internal LOD pyramid
// ============================================================

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PyramidFingerprint {
    generation: u64,
    valid_len: usize,
    sample_rate: u32,
    channels: u32,
}

#[derive(Debug, Default, Clone, Copy)]
#[repr(C)]
struct MinMaxPair {
    min: f32,
    max: f32,
}

impl MinMaxPair {
    const ZERO: Self = Self { min: 0.0, max: 0.0 };
}

#[derive(Debug, Default)]
struct MinMaxLevel {
    /// チャンネル別のペア列。`per_channel[ch][p]` で 1 ペアにアクセス。
    /// 録音追記時に末尾 push を効かせるため、flat ではなく per-channel にしている。
    /// 全チャンネルで `len()` は同一の前提。
    per_channel: Vec<Vec<MinMaxPair>>,
    /// 1 ペアが元サンプル何個分か (16, 256, 4096, ...)。
    decimation: u32,
}

impl MinMaxLevel {
    fn pairs_per_channel(&self) -> usize {
        self.per_channel.first().map_or(0, Vec::len)
    }
}

/// 1 波形ウィジェット分のキャッシュ。`UiHost.state` に置かれる。
///
/// `WidgetState` の blanket impl (`impl<T: Any + Send + Sync> WidgetState for T`) により
/// 自動的に `Box<dyn WidgetState>` で扱える。
#[derive(Debug, Default)]
pub(crate) struct WaveformPyramid {
    fingerprint: PyramidFingerprint,
    /// `levels[0]` が最も細かい (decimation = 16)、`levels[last]` が最も粗い。
    levels: Vec<MinMaxLevel>,
}

impl WaveformPyramid {
    /// 必要なら LOD を再構築 or インクリメンタル拡張する。
    ///
    /// 3 通りのケースを区別する:
    /// 1. fingerprint 完全一致 → 何もしない (ピラミッド再利用)
    /// 2. `valid_len` のみ増えた (それ以外は同じ) → **インクリメンタル拡張** (録音追記対応)
    /// 3. それ以外 (generation 変化、サイズレート変化、チャンネル数変化、`valid_len` 縮小)
    ///    → 完全再構築
    fn ensure_built(&mut self, src: &WaveformSource<'_>) {
        let channels = src.samples.channels();
        let valid_len = clamp_valid_len(src);

        let new_fp = PyramidFingerprint {
            generation: src.generation,
            valid_len,
            sample_rate: src.sample_rate,
            channels: channels as u32,
        };
        if self.fingerprint == new_fp && !self.levels.is_empty() {
            return;
        }

        let can_incremental = !self.levels.is_empty()
            && self.fingerprint.generation == new_fp.generation
            && self.fingerprint.sample_rate == new_fp.sample_rate
            && self.fingerprint.channels == new_fp.channels
            && self.fingerprint.valid_len < valid_len;

        if can_incremental {
            self.extend_to(&src.samples, channels, valid_len);
        } else {
            self.levels = build_pyramid(&src.samples, valid_len, channels);
        }
        self.fingerprint = new_fp;
    }

    /// 既存のピラミッドを `new_valid_len` まで拡張する。
    /// 各レベルで「old 末尾の (部分的に埋まっていた) 1 ペアを再計算 + 新ペアを末尾追加」する。
    /// 追加コストは概ね追加サンプル数に比例 (1ms 録音追記なら数十ペアのみ更新)。
    fn extend_to(
        &mut self,
        samples: &SampleSlices<'_>,
        channels: usize,
        new_valid_len: usize,
    ) {
        if self.levels.is_empty() {
            self.levels = build_pyramid(samples, new_valid_len, channels);
            return;
        }
        // Level 1: 生サンプルから extend
        extend_level_1(&mut self.levels[0], samples, channels, new_valid_len);
        // Level 2..N: cascading で前レベルから extend
        for i in 1..self.levels.len() {
            let (prev_slice, this_slice) = self.levels.split_at_mut(i);
            let prev = &prev_slice[i - 1];
            let this = &mut this_slice[0];
            extend_level_from_prev(this, prev, channels);
        }
        // 必要なら新しい (より粗い) レベルを追加
        while self
            .levels
            .last()
            .is_some_and(|l| l.pairs_per_channel() > PYRAMID_COARSE_THRESHOLD)
        {
            let new_level = {
                let prev = self.levels.last().expect("checked Some above");
                build_level_from_prev(prev, channels)
            };
            self.levels.push(new_level);
        }
    }
}

/// `valid_len` を「実際にチャンネル全てで揃っている範囲」にクランプする。
fn clamp_valid_len(src: &WaveformSource<'_>) -> usize {
    let channels = src.samples.channels();
    if channels == 0 {
        return 0;
    }
    let mut min_frames = src.valid_len;
    for ch in 0..channels {
        min_frames = min_frames.min(src.samples.max_frames(ch));
    }
    min_frames
}

fn build_pyramid(
    samples: &SampleSlices<'_>,
    valid_len: usize,
    channels: usize,
) -> Vec<MinMaxLevel> {
    if valid_len == 0 || channels == 0 {
        return Vec::new();
    }
    let mut levels: Vec<MinMaxLevel> = Vec::with_capacity(4);

    // Level 1: 生サンプル → 16:1
    let l1_ppc = valid_len.div_ceil(DECIMATION);
    let mut l1_per_channel: Vec<Vec<MinMaxPair>> = Vec::with_capacity(channels);
    for ch in 0..channels {
        let mut col = vec![MinMaxPair::ZERO; l1_ppc];
        for (p, slot) in col.iter_mut().enumerate().take(l1_ppc) {
            let s_start = p * DECIMATION;
            let s_end = ((p + 1) * DECIMATION).min(valid_len);
            *slot = peak_in_raw(samples, ch, s_start, s_end);
        }
        l1_per_channel.push(col);
    }
    levels.push(MinMaxLevel {
        per_channel: l1_per_channel,
        decimation: DECIMATION as u32,
    });

    // 上位レベル: 前レベルから build_level_from_prev で順次積む。
    while levels
        .last()
        .is_some_and(|l| l.pairs_per_channel() > PYRAMID_COARSE_THRESHOLD)
    {
        let new_level = {
            let prev = levels.last().expect("just checked Some");
            build_level_from_prev(prev, channels)
        };
        levels.push(new_level);
    }

    levels
}

/// 1 つのレベルを前レベルから新規構築する。完全再構築のみで使う (extend では使わない)。
fn build_level_from_prev(prev: &MinMaxLevel, channels: usize) -> MinMaxLevel {
    let prev_ppc = prev.pairs_per_channel();
    let next_ppc = prev_ppc.div_ceil(DECIMATION);
    let next_decimation = prev.decimation.saturating_mul(DECIMATION as u32);

    let mut per_channel: Vec<Vec<MinMaxPair>> = Vec::with_capacity(channels);
    for ch in 0..channels {
        let prev_ch = &prev.per_channel[ch];
        let mut this_ch = vec![MinMaxPair::ZERO; next_ppc];
        for (p, slot) in this_ch.iter_mut().enumerate().take(next_ppc) {
            let p_start = p * DECIMATION;
            let p_end = ((p + 1) * DECIMATION).min(prev_ch.len());
            *slot = fold_pairs(&prev_ch[p_start..p_end]);
        }
        per_channel.push(this_ch);
    }
    MinMaxLevel { per_channel, decimation: next_decimation }
}

/// Level 1 を `new_valid_len` まで拡張する。古い末尾ペア (部分埋まりだったもの) を
/// 再計算し、新規ペアを末尾に追加する。
fn extend_level_1(
    level: &mut MinMaxLevel,
    samples: &SampleSlices<'_>,
    channels: usize,
    new_valid_len: usize,
) {
    let new_ppc = new_valid_len.div_ceil(DECIMATION);
    let old_ppc = level.pairs_per_channel();
    // 古い末尾ペア (部分埋まりの可能性) から再計算する。
    let recompute_start = old_ppc.saturating_sub(1);
    if level.per_channel.len() < channels {
        level.per_channel.resize_with(channels, Vec::new);
    }
    for ch in 0..channels {
        let col = &mut level.per_channel[ch];
        col.resize(new_ppc, MinMaxPair::ZERO);
        // index アクセスは samples / ch も併用するため enumerate に書き換えづらい (hot path)。
        #[allow(clippy::needless_range_loop)]
        for p in recompute_start..new_ppc {
            let s_start = p * DECIMATION;
            let s_end = ((p + 1) * DECIMATION).min(new_valid_len);
            col[p] = peak_in_raw(samples, ch, s_start, s_end);
        }
    }
}

/// Level k (k>=2) を前レベルから拡張する。前レベルが extend 済みである前提。
fn extend_level_from_prev(level: &mut MinMaxLevel, prev: &MinMaxLevel, channels: usize) {
    let prev_ppc = prev.pairs_per_channel();
    let new_ppc = prev_ppc.div_ceil(DECIMATION);
    let old_ppc = level.pairs_per_channel();
    // 古い末尾ペア (前レベル境界が変わったので再計算が必要) から。
    let recompute_start = old_ppc.saturating_sub(1);
    if level.per_channel.len() < channels {
        level.per_channel.resize_with(channels, Vec::new);
    }
    for ch in 0..channels {
        let prev_ch = &prev.per_channel[ch];
        let col = &mut level.per_channel[ch];
        col.resize(new_ppc, MinMaxPair::ZERO);
        // index アクセスは prev_ch にも掛かるため enumerate に書き換えづらい (hot path)。
        #[allow(clippy::needless_range_loop)]
        for p in recompute_start..new_ppc {
            let p_start = p * DECIMATION;
            let p_end = ((p + 1) * DECIMATION).min(prev_ch.len());
            col[p] = fold_pairs(&prev_ch[p_start..p_end]);
        }
    }
}

/// `[MinMaxPair]` から min/max を畳み込む。空なら `MinMaxPair::ZERO`。
/// peak_in_view の毎ピクセル hot path で呼ばれるので `#[inline(always)]`。
#[allow(clippy::inline_always)]
#[inline(always)]
fn fold_pairs(pairs: &[MinMaxPair]) -> MinMaxPair {
    if pairs.is_empty() {
        return MinMaxPair::ZERO;
    }
    let mut mn = f32::INFINITY;
    let mut mx = f32::NEG_INFINITY;
    for p in pairs {
        if p.min < mn {
            mn = p.min;
        }
        if p.max > mx {
            mx = p.max;
        }
    }
    if !mn.is_finite() {
        mn = 0.0;
    }
    if !mx.is_finite() {
        mx = 0.0;
    }
    MinMaxPair { min: mn, max: mx }
}

/// 生サンプルから `[start, end)` の min/max を取る。enum variant ごとに hot loop を分ける。
fn peak_in_raw(samples: &SampleSlices<'_>, ch: usize, start: usize, end: usize) -> MinMaxPair {
    if end <= start {
        return MinMaxPair::ZERO;
    }
    let mut mn = f32::INFINITY;
    let mut mx = f32::NEG_INFINITY;
    match samples {
        SampleSlices::Mono(s) => {
            if ch == 0 {
                let end = end.min(s.len());
                if start < end {
                    for &v in &s[start..end] {
                        if v < mn {
                            mn = v;
                        }
                        if v > mx {
                            mx = v;
                        }
                    }
                }
            }
        }
        SampleSlices::Planar(planes) => {
            if let Some(plane) = planes.get(ch) {
                let end = end.min(plane.len());
                if start < end {
                    for &v in &plane[start..end] {
                        if v < mn {
                            mn = v;
                        }
                        if v > mx {
                            mx = v;
                        }
                    }
                }
            }
        }
        SampleSlices::Interleaved { data, channels } => {
            let stride = *channels;
            if stride > 0 && ch < stride {
                let total_frames = data.len() / stride;
                let end = end.min(total_frames);
                for i in start..end {
                    let v = data[i * stride + ch];
                    if v < mn {
                        mn = v;
                    }
                    if v > mx {
                        mx = v;
                    }
                }
            }
        }
    }
    if !mn.is_finite() {
        mn = 0.0;
    }
    if !mx.is_finite() {
        mx = 0.0;
    }
    MinMaxPair { min: mn, max: mx }
}

/// 1 フレーム分の波形セグメントを構築する。
///
/// `&Ui` を借りないので、ピラミッドへの `&mut` 借用と独立に scene へ push できる。
fn build_peak_segments(
    pyramid: &WaveformPyramid,
    src: &WaveformSource<'_>,
    rect: Rect,
    view: WaveformView,
    style: WaveformStyle,
) -> Vec<LineSegment> {
    let channels = src.samples.channels();
    let valid_len = clamp_valid_len(src);
    if rect.w < 1.0 || rect.h < 1.0 || channels == 0 {
        return Vec::new();
    }

    let samples = &src.samples;
    let view_start = view.start_sample as f64;
    let view_len = view.len_samples.max(1) as f64;
    let pixel_w = f64::from(rect.w);
    let samples_per_pixel = view_len / pixel_w;
    let pixel_count = rect.w as usize;

    let (per_ch_h, ch_iter): (f32, Vec<usize>) = match style.channel_layout {
        ChannelLayout::Stack => (rect.h / channels as f32, (0..channels).collect()),
        ChannelLayout::Overlay => (rect.h, (0..channels).collect()),
        ChannelLayout::FirstOnly => (rect.h, vec![0]),
    };

    let mut segments: Vec<LineSegment> = Vec::with_capacity(pixel_count * ch_iter.len() + ch_iter.len());

    // baseline (各チャンネルの中央線)
    if let Some(base_color) = style.baseline {
        match style.channel_layout {
            ChannelLayout::Stack => {
                for (slot, _ch) in ch_iter.iter().enumerate() {
                    let y_mid = rect.y + per_ch_h * slot as f32 + per_ch_h * 0.5;
                    segments.push(LineSegment {
                        a: [rect.x, y_mid],
                        b: [rect.x + rect.w, y_mid],
                        color: base_color,
                    });
                }
            }
            ChannelLayout::Overlay | ChannelLayout::FirstOnly => {
                let y_mid = rect.y + rect.h * 0.5;
                segments.push(LineSegment {
                    a: [rect.x, y_mid],
                    b: [rect.x + rect.w, y_mid],
                    color: base_color,
                });
            }
        }
    }

    // 描画レベルを widget 単位で 1 回だけ選ぶ (samples_per_pixel は widget 内で
    // ほぼ一定なので、毎ピクセル走査するのは無駄)。
    let span_int = samples_per_pixel as usize;
    let chosen_level: Option<&MinMaxLevel> = pyramid
        .levels
        .iter()
        .rfind(|l| (l.decimation as usize) <= span_int);

    // ピーク線
    for (slot, &ch) in ch_iter.iter().enumerate() {
        let ch_top = rect.y
            + match style.channel_layout {
                ChannelLayout::Stack => per_ch_h * slot as f32,
                _ => 0.0,
            };
        let ch_mid = ch_top + per_ch_h * 0.5;
        let ch_half = per_ch_h * 0.5;
        // チャンネル別の per_channel スライス参照を hoist。
        let cached_col: Option<&[MinMaxPair]> =
            chosen_level.and_then(|l| l.per_channel.get(ch).map(Vec::as_slice));

        for px in 0..pixel_count {
            let p_start = view_start + (px as f64) * samples_per_pixel;
            let p_end = view_start + (px as f64 + 1.0) * samples_per_pixel;
            let pair = peak_in_view_cached(
                cached_col,
                chosen_level.map(|l| l.decimation as usize),
                samples,
                valid_len,
                ch,
                p_start,
                p_end,
            );

            let clipped = pair.min < -1.0 || pair.max > 1.0;
            let color = if clipped { style.fg_clipped } else { style.fg };

            // -1..1 を [ch_mid - ch_half, ch_mid + ch_half] にマップ。
            let v_max = (pair.max * view.vertical_gain).clamp(-1.0, 1.0);
            let v_min = (pair.min * view.vertical_gain).clamp(-1.0, 1.0);
            let y_top = ch_mid - v_max * ch_half;
            let y_bot = ch_mid - v_min * ch_half;
            // 最低 1px の縦線を確保 (min == max のとき潰れないように)。
            let y_bot = y_bot.max(y_top + 1.0);
            let x = rect.x + px as f32 + 0.5;

            segments.push(LineSegment {
                a: [x, y_top],
                b: [x, y_bot],
                color,
            });
        }
    }

    segments
}

/// `Auto` モードのとき samples_per_pixel から実モードを決定する。
/// 1 ピクセルあたり 1 サンプル未満 (= ズームイン状態) なら `SamplePolyline`、
/// それ以外 (典型的な俯瞰表示) は `PeakLines` を選ぶ。閾値は plan.md の M5 仕様
/// 「1 サンプル/ピクセル以下にズームしたとき」に合わせ 1.0 固定 (Phase 15 では
/// flicker 対策のヒステリシスは入れず、目視で気になれば別タスクで対応)。
fn resolve_render_mode(mode: WaveformRenderMode, samples_per_pixel: f64) -> WaveformRenderMode {
    match mode {
        WaveformRenderMode::Auto => {
            if samples_per_pixel < 1.0 {
                WaveformRenderMode::SamplePolyline
            } else {
                WaveformRenderMode::PeakLines
            }
        }
        other => other,
    }
}

/// `SamplePolyline` 描画: 生サンプルを直接読み、view 範囲内の連続 N 点を結ぶ
/// `LineSegment` 列を生成する (LOD ピラミッド不使用)。`samples_per_pixel < 1.0` 前提。
///
/// `vertical_gain` / clamp / clipped 判定は `build_peak_segments` と挙動を揃える
/// (gain 適用後 clamp、clipped は gain 適用前の生サンプル `|s| > 1.0` で判定)。
/// segment の色は端点いずれかが clipped なら `fg_clipped`、両端正常なら `fg`。
fn build_sample_polyline_segments(
    src: &WaveformSource<'_>,
    rect: Rect,
    view: WaveformView,
    style: WaveformStyle,
) -> Vec<LineSegment> {
    let channels = src.samples.channels();
    let valid_len = clamp_valid_len(src);
    if rect.w < 1.0 || rect.h < 1.0 || channels == 0 {
        return Vec::new();
    }

    let view_start = view.start_sample as f64;
    let view_len = view.len_samples.max(1) as f64;
    let pixel_w = f64::from(rect.w);
    let x_per_sample = pixel_w / view_len;

    let (per_ch_h, ch_iter): (f32, Vec<usize>) = match style.channel_layout {
        ChannelLayout::Stack => (rect.h / channels as f32, (0..channels).collect()),
        ChannelLayout::Overlay => (rect.h, (0..channels).collect()),
        ChannelLayout::FirstOnly => (rect.h, vec![0]),
    };

    // view 範囲のサンプル idx 列 (端点 1 つ extra で line を view 末端まで届かせる)
    let s_start_int = view_start.max(0.0) as usize;
    let s_end_unclamped = (view_start + view_len) as usize + 1;
    let s_end_int = s_end_unclamped.min(valid_len);
    let n_samples = s_end_int.saturating_sub(s_start_int);
    let segs_per_ch = n_samples.saturating_sub(1);

    let mut segments: Vec<LineSegment> =
        Vec::with_capacity(segs_per_ch * ch_iter.len() + ch_iter.len());

    // baseline (各チャンネル中央線、PeakLines と同形)
    if let Some(base_color) = style.baseline {
        match style.channel_layout {
            ChannelLayout::Stack => {
                for (slot, _ch) in ch_iter.iter().enumerate() {
                    let y_mid = rect.y + per_ch_h * slot as f32 + per_ch_h * 0.5;
                    segments.push(LineSegment {
                        a: [rect.x, y_mid],
                        b: [rect.x + rect.w, y_mid],
                        color: base_color,
                    });
                }
            }
            ChannelLayout::Overlay | ChannelLayout::FirstOnly => {
                let y_mid = rect.y + rect.h * 0.5;
                segments.push(LineSegment {
                    a: [rect.x, y_mid],
                    b: [rect.x + rect.w, y_mid],
                    color: base_color,
                });
            }
        }
    }

    if n_samples < 2 {
        return segments;
    }

    for (slot, &ch) in ch_iter.iter().enumerate() {
        let ch_top = rect.y
            + match style.channel_layout {
                ChannelLayout::Stack => per_ch_h * slot as f32,
                _ => 0.0,
            };
        let ch_mid = ch_top + per_ch_h * 0.5;
        let ch_half = per_ch_h * 0.5;

        let mut prev: Option<(f32, f32, bool)> = None; // (x, y, is_clipped)
        for i in 0..n_samples {
            let sample_idx = s_start_int + i;
            let s = sample_at(&src.samples, ch, sample_idx);
            let local_pos = sample_idx as f64 - view_start;
            let x = rect.x + (local_pos * x_per_sample) as f32;
            let v = (s * view.vertical_gain).clamp(-1.0, 1.0);
            let y = ch_mid - v * ch_half;
            let is_clipped = s.abs() > 1.0;
            if let Some((px, py, prev_clipped)) = prev {
                let color = if is_clipped || prev_clipped {
                    style.fg_clipped
                } else {
                    style.fg
                };
                segments.push(LineSegment { a: [px, py], b: [x, y], color });
            }
            prev = Some((x, y, is_clipped));
        }
    }

    segments
}

/// 生サンプルから 1 サンプル分の値を取り出す (channels = 0 / 範囲外は 0.0)。
/// `peak_in_raw` の Interleaved stride アクセスと整合。
#[inline]
fn sample_at(samples: &SampleSlices<'_>, ch: usize, idx: usize) -> f32 {
    match samples {
        SampleSlices::Mono(s) => {
            if ch == 0 { s.get(idx).copied().unwrap_or(0.0) } else { 0.0 }
        }
        SampleSlices::Planar(planes) => planes
            .get(ch)
            .and_then(|c| c.get(idx))
            .copied()
            .unwrap_or(0.0),
        SampleSlices::Interleaved { data, channels } => {
            let stride = *channels;
            if stride == 0 || ch >= stride {
                0.0
            } else {
                data.get(idx * stride + ch).copied().unwrap_or(0.0)
            }
        }
    }
}

/// 事前に選択済みレベル (`col` + `decim`) を使って 1 ピクセル分の min/max を取る。
/// レベル未選択 (= samples_per_pixel が小さすぎる) のときは生サンプルを走査する。
#[allow(clippy::inline_always)]
#[inline(always)]
fn peak_in_view_cached(
    col: Option<&[MinMaxPair]>,
    decim: Option<usize>,
    samples: &SampleSlices<'_>,
    valid_len: usize,
    ch: usize,
    sample_start: f64,
    sample_end: f64,
) -> MinMaxPair {
    let s_start = sample_start.max(0.0) as usize;
    let s_end = (sample_end.max(0.0) as usize).min(valid_len);
    if s_end <= s_start {
        return MinMaxPair::ZERO;
    }
    if let (Some(col), Some(decim)) = (col, decim) {
        let p_start = s_start / decim;
        let p_end = s_end.div_ceil(decim).min(col.len());
        if p_end <= p_start {
            return MinMaxPair::ZERO;
        }
        fold_pairs(&col[p_start..p_end])
    } else {
        peak_in_raw(samples, ch, s_start, s_end)
    }
}

// ============================================================
// Public widget API
// ============================================================

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 波形ウィジェット。生サンプルを借りて min/max ピーク線を描く。
    ///
    /// - `id`: ウィジェット識別子。フレーム間の LOD ピラミッドキャッシュキーになる。
    /// - `rect`: 物理ピクセルでの描画矩形。
    /// - `source`: 生サンプル + `valid_len` + `generation`。`generation` 一致で LOD 再利用。
    /// - `view`: 表示するサンプル範囲 + 縦ゲイン (アプリ側状態)。
    /// - `style`: 描画スタイル。M2 では `render_mode` は実質 `PeakLines` のみ。
    ///
    /// 戻り値で hover / clicked / dragging を取り出し、`Edit` の組み立てはアプリ側で行う。
    pub fn waveform<'s>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        source: WaveformSource<'s>,
        view: WaveformView,
        style: WaveformStyle,
    ) -> WaveformResponse {
        let wid = WidgetId::ROOT.child((b"waveform", &id));

        // M4 Phase 12: input_hash 一致なら LOD ピラミッドの再構築 + 描画を全部スキップ。
        // generation 変化が主な invalidation トリガ (= LOD pyramid の fingerprint と整合)。
        // 注: Rust の `Hash` 実装は tuple 要素 12 個まで。ネスト tuple で回避。
        let input_hash = hash_inputs((
            b"waveform",
            (rect.x.to_bits(), rect.y.to_bits(), rect.w.to_bits(), rect.h.to_bits()),
            (source.generation, source.valid_len as u64, source.sample_rate),
            (view.start_sample, view.len_samples, view.vertical_gain.to_bits()),
            style.line_width_px.to_bits(),
            (
                style.fg.r.to_bits(),
                style.fg.g.to_bits(),
                style.fg.b.to_bits(),
                style.fg.a.to_bits(),
            ),
            (
                style.fg_clipped.r.to_bits(),
                style.fg_clipped.g.to_bits(),
                style.fg_clipped.b.to_bits(),
                style.fg_clipped.a.to_bits(),
            ),
            style.channel_layout,
            style.render_mode,
        ));

        // pyramid は wid 経由の widget_state で持つので、closure 内で取り直す。
        // source / view / style は closure 内で borrow / Copy。
        // M5 Phase 15: render_mode を resolve して SamplePolyline / PeakLines を分岐。
        // SamplePolyline モードのときは LOD ピラミッドを触らず、生サンプルから直接描画する
        // (samples_per_pixel < 1.0 = ピラミッド無意味な領域)。
        self.with_widget_node(wid, input_hash, |ui| {
            let view_len = view.len_samples.max(1) as f64;
            let samples_per_pixel = view_len / f64::from(rect.w.max(1.0));
            let effective_mode = resolve_render_mode(style.render_mode, samples_per_pixel);
            let segments = match effective_mode {
                WaveformRenderMode::SamplePolyline => {
                    build_sample_polyline_segments(&source, rect, view, style)
                }
                // TODO(Phase 16): RmsBars 専用描画。Phase 15 では PeakLines にフォールバック。
                WaveformRenderMode::PeakLines | WaveformRenderMode::RmsBars => {
                    let pyramid: &mut WaveformPyramid = ui.widget_state(wid);
                    pyramid.ensure_built(&source);
                    build_peak_segments(pyramid, &source, rect, view, style)
                }
                WaveformRenderMode::Auto => unreachable!("resolve_render_mode で除去済"),
            };
            if !segments.is_empty() {
                ui.push_lines(LineBatch {
                    segments,
                    line_width_px: style.line_width_px.max(1.0),
                    clip_rect: Some(rect),
                });
            }
        });

        // 3. ヒットテスト。
        let pointer = self.pointer;
        let mut response = WaveformResponse {
            hovered: hovered(rect, pointer),
            clicked_at: None,
            dragging_at: None,
        };
        if let Some((px, py)) = pointer.pos
            && rect.contains(px, py)
        {
            let local_x = px - rect.x;
            let local_y = py - rect.y;
            let frac = (f64::from(local_x) / f64::from(rect.w)).clamp(0.0, 1.0);
            let sample_index =
                (view.start_sample as f64 + frac * view.len_samples as f64) as u64;
            let channels = source.samples.channels().max(1);
            let channel = match style.channel_layout {
                ChannelLayout::Stack => {
                    let per_h = rect.h / channels as f32;
                    if per_h > 0.0 {
                        ((local_y / per_h) as usize).min(channels - 1)
                    } else {
                        0
                    }
                }
                ChannelLayout::Overlay | ChannelLayout::FirstOnly => 0,
            };
            let hit = WaveformHit {
                sample_index,
                channel,
                local_x_px: local_x,
                local_y_px: local_y,
            };
            if pointer.primary_just_released {
                response.clicked_at = Some(hit);
            }
            if pressed_inside(rect, pointer) {
                response.dragging_at = Some(hit);
            }
        }

        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 決定論的なテスト用サンプルを作る (sin 重ね合わせ、値域 [-1, 1])。
    fn deterministic_samples(n: usize, ch_seed: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let f1 = (i as f32 * 0.0017 + ch_seed as f32 * 0.137).sin();
                let f2 = (i as f32 * 0.0083).sin() * 0.4;
                (f1 + f2).clamp(-1.0, 1.0)
            })
            .collect()
    }

    fn build_full(samples: &SampleSlices<'_>, valid_len: usize) -> WaveformPyramid {
        let mut p = WaveformPyramid::default();
        p.ensure_built(&WaveformSource {
            samples: *samples,
            valid_len,
            generation: 1,
            sample_rate: 48_000,
        });
        p
    }

    fn assert_pyramid_eq(reference: &WaveformPyramid, actual: &WaveformPyramid) {
        assert_eq!(
            reference.levels.len(),
            actual.levels.len(),
            "level count mismatch (ref={}, actual={})",
            reference.levels.len(),
            actual.levels.len(),
        );
        for (lvl_idx, (r_lvl, a_lvl)) in reference.levels.iter().zip(&actual.levels).enumerate() {
            assert_eq!(
                r_lvl.decimation, a_lvl.decimation,
                "level {lvl_idx} decimation mismatch",
            );
            assert_eq!(
                r_lvl.per_channel.len(),
                a_lvl.per_channel.len(),
                "level {lvl_idx} channel count mismatch",
            );
            for (ch, (r_col, a_col)) in r_lvl
                .per_channel
                .iter()
                .zip(&a_lvl.per_channel)
                .enumerate()
            {
                assert_eq!(
                    r_col.len(),
                    a_col.len(),
                    "level {lvl_idx} ch {ch} ppc mismatch (ref={}, actual={})",
                    r_col.len(),
                    a_col.len(),
                );
                for (p, (r_pair, a_pair)) in r_col.iter().zip(a_col).enumerate() {
                    assert_eq!(
                        r_pair.min, a_pair.min,
                        "level {lvl_idx} ch {ch} pair {p} min mismatch (ref={}, actual={})",
                        r_pair.min, a_pair.min,
                    );
                    assert_eq!(
                        r_pair.max, a_pair.max,
                        "level {lvl_idx} ch {ch} pair {p} max mismatch (ref={}, actual={})",
                        r_pair.max, a_pair.max,
                    );
                }
            }
        }
    }

    /// 多段階インクリメンタル拡張で得られる pyramid が、最終 valid_len で完全再構築した
    /// pyramid と pair 単位で完全一致すること。境界ペアの再計算が正しく行われることの保証。
    #[test]
    fn incremental_extension_matches_full_rebuild() {
        let l = deterministic_samples(50_000, 0);
        let r = deterministic_samples(50_000, 1);
        let planes: [&[f32]; 2] = [&l, &r];
        let samples = SampleSlices::Planar(&planes);

        // 16/256 ペア境界をまたぐ valid_len で段階的に extend する。
        let stages = [
            17,      // < 16 → l1_ppc=2 で空に近い
            512,     // l1_ppc=32, l2_ppc=2
            5_000,   // l1_ppc=313, l2_ppc=20
            10_000,  // l1_ppc=625, l2_ppc=40
            17_001,  // 中途半端な境界
            32_001,  // ほぼ 2 のべき
            49_999,  // 末尾境界 (1 サンプル足りない)
            50_000,  // 末尾完全一致
        ];

        let mut incremental = WaveformPyramid::default();
        for &vl in &stages {
            incremental.ensure_built(&WaveformSource {
                samples,
                valid_len: vl,
                generation: 1,
                sample_rate: 48_000,
            });
        }

        let reference = build_full(&samples, 50_000);
        assert_pyramid_eq(&reference, &incremental);
    }

    /// `valid_len` 縮小は完全再構築 (インクリメンタル拡張は使わない) されること。
    #[test]
    fn shrinking_valid_len_triggers_full_rebuild() {
        let l = deterministic_samples(10_000, 0);
        let r = deterministic_samples(10_000, 1);
        let planes: [&[f32]; 2] = [&l, &r];
        let samples = SampleSlices::Planar(&planes);

        let mut p = WaveformPyramid::default();
        p.ensure_built(&WaveformSource {
            samples,
            valid_len: 10_000,
            generation: 1,
            sample_rate: 48_000,
        });
        // 縮小
        p.ensure_built(&WaveformSource {
            samples,
            valid_len: 5_000,
            generation: 1,
            sample_rate: 48_000,
        });

        let reference = build_full(&samples, 5_000);
        assert_pyramid_eq(&reference, &p);
    }

    /// 1ch Interleaved (`channels=1`) は同データの Mono / Planar と完全一致したピラミッドを
    /// 構築すること。`peak_in_raw` の Interleaved 経路 (`data[i*1+0]`) が Mono の生 slice 走査と
    /// 同等の min/max を返すことを担保する (Phase 15)。
    #[test]
    fn interleaved_1ch_matches_mono_pyramid() {
        let s = deterministic_samples(10_000, 0);
        let mono_samples = SampleSlices::Mono(&s);
        let interleaved_samples = SampleSlices::Interleaved { data: &s, channels: 1 };

        let mono_pyr = build_full(&mono_samples, 10_000);
        let interleaved_pyr = build_full(&interleaved_samples, 10_000);

        assert_pyramid_eq(&mono_pyr, &interleaved_pyr);
    }

    /// 2ch Interleaved データで ch=0 (L) と ch=1 (R) が独立に正しく取り出せること。
    /// `[L0, R0, L1, R1, ...]` の stride アクセス (`data[i*2+ch]`) が Planar 等価であることを
    /// 担保する (Phase 15)。
    #[test]
    fn interleaved_2ch_channels_independent() {
        let l = deterministic_samples(10_000, 0);
        let r = deterministic_samples(10_000, 1);
        // Interleaved `[L0, R0, L1, R1, ...]` を組み立て
        let mut interleaved: Vec<f32> = Vec::with_capacity(20_000);
        for i in 0..10_000 {
            interleaved.push(l[i]);
            interleaved.push(r[i]);
        }
        let interleaved_samples =
            SampleSlices::Interleaved { data: &interleaved, channels: 2 };

        // Planar 版を reference に
        let planes: [&[f32]; 2] = [&l, &r];
        let planar_samples = SampleSlices::Planar(&planes);

        let planar_pyr = build_full(&planar_samples, 10_000);
        let interleaved_pyr = build_full(&interleaved_samples, 10_000);

        assert_pyramid_eq(&planar_pyr, &interleaved_pyr);
    }

    /// `generation` 変化は完全再構築されること (たとえ valid_len が増えていても)。
    #[test]
    fn generation_change_triggers_full_rebuild() {
        let l1 = deterministic_samples(10_000, 0);
        let r1 = deterministic_samples(10_000, 1);
        let planes1: [&[f32]; 2] = [&l1, &r1];

        // 別データ (内容が違う)
        let l2 = deterministic_samples(20_000, 7);
        let r2 = deterministic_samples(20_000, 11);
        let planes2: [&[f32]; 2] = [&l2, &r2];

        let mut p = WaveformPyramid::default();
        p.ensure_built(&WaveformSource {
            samples: SampleSlices::Planar(&planes1),
            valid_len: 10_000,
            generation: 1,
            sample_rate: 48_000,
        });
        p.ensure_built(&WaveformSource {
            samples: SampleSlices::Planar(&planes2),
            valid_len: 20_000,
            generation: 2, // 異なる generation
            sample_rate: 48_000,
        });

        let reference = build_full(&SampleSlices::Planar(&planes2), 20_000);
        assert_pyramid_eq(&reference, &p);
    }
}
