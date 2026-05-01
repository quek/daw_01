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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLayout {
    /// チャンネルを縦に並べる (デフォルト)。
    Stack,
    /// 全チャンネルを重ねて描く (透過は呼び出し側が style.fg.a で調整)。
    Overlay,
    /// 1 ch のみ描く。
    FirstOnly,
}

/// 描画モード。M2 では `PeakLines` のみ実装、他は M5 で完成させる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    fn extend(&mut self, other: Self) {
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }
    }
}

#[derive(Debug, Default)]
struct MinMaxLevel {
    /// channels × pairs_per_channel 個のペア。チャンネル順に連続。
    pairs: Vec<MinMaxPair>,
    pairs_per_channel: usize,
    /// 1 ペアが元サンプル何個分か (16, 256, 4096, ...)。
    decimation: u32,
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
    /// `fingerprint` が変わっていれば全レベルを再構築する。
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
        // M2: 完全再構築のみ。インクリメンタル拡張 (録音中追記) は後段。
        self.levels = build_pyramid(&src.samples, valid_len, channels);
        self.fingerprint = new_fp;
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
    let mut l1 = vec![MinMaxPair::ZERO; channels * l1_ppc];
    for ch in 0..channels {
        let offset = ch * l1_ppc;
        for p in 0..l1_ppc {
            let s_start = p * DECIMATION;
            let s_end = ((p + 1) * DECIMATION).min(valid_len);
            l1[offset + p] = peak_in_raw(samples, ch, s_start, s_end);
        }
    }
    levels.push(MinMaxLevel {
        pairs: l1,
        pairs_per_channel: l1_ppc,
        decimation: DECIMATION as u32,
    });

    // 上位レベル: 前レベルの 16 ペアごとに min/max を取り直す。
    while levels.last().is_some_and(|l| l.pairs_per_channel > PYRAMID_COARSE_THRESHOLD) {
        let prev = levels.last().expect("just checked Some");
        let prev_ppc = prev.pairs_per_channel;
        let next_ppc = prev_ppc.div_ceil(DECIMATION);
        let next_decimation = prev.decimation.saturating_mul(DECIMATION as u32);
        let mut next_pairs = vec![MinMaxPair::ZERO; channels * next_ppc];

        for ch in 0..channels {
            let prev_offset = ch * prev_ppc;
            let next_offset = ch * next_ppc;
            for p in 0..next_ppc {
                let p_start = p * DECIMATION;
                let p_end = ((p + 1) * DECIMATION).min(prev_ppc);
                let mut acc = MinMaxPair { min: f32::INFINITY, max: f32::NEG_INFINITY };
                for i in p_start..p_end {
                    acc.extend(prev.pairs[prev_offset + i]);
                }
                if !acc.min.is_finite() {
                    acc.min = 0.0;
                }
                if !acc.max.is_finite() {
                    acc.max = 0.0;
                }
                next_pairs[next_offset + p] = acc;
            }
        }
        levels.push(MinMaxLevel {
            pairs: next_pairs,
            pairs_per_channel: next_ppc,
            decimation: next_decimation,
        });
    }

    levels
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

/// `[sample_start, sample_end)` 範囲の min/max をピラミッド (or 生サンプル) から取る。
fn peak_in_view(
    pyramid: &WaveformPyramid,
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
    let span = s_end - s_start;

    // 使うレベル: decimation <= span な最大レベル (= 1 ペア以上は走査できる粒度)
    let mut chosen: Option<&MinMaxLevel> = None;
    for lvl in &pyramid.levels {
        if (lvl.decimation as usize) <= span {
            chosen = Some(lvl);
        } else {
            break;
        }
    }

    if let Some(lvl) = chosen {
        let decim = lvl.decimation as usize;
        let p_start = s_start / decim;
        let p_end = s_end.div_ceil(decim).min(lvl.pairs_per_channel);
        if p_end <= p_start {
            return MinMaxPair::ZERO;
        }
        let offset = ch * lvl.pairs_per_channel;
        let mut acc = MinMaxPair { min: f32::INFINITY, max: f32::NEG_INFINITY };
        for p in p_start..p_end {
            acc.extend(lvl.pairs[offset + p]);
        }
        if !acc.min.is_finite() {
            acc.min = 0.0;
        }
        if !acc.max.is_finite() {
            acc.max = 0.0;
        }
        acc
    } else {
        // 1 ピクセル < 16 サンプル: 生サンプルで走査
        peak_in_raw(samples, ch, s_start, s_end)
    }
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

    // ピーク線
    for (slot, &ch) in ch_iter.iter().enumerate() {
        let ch_top = rect.y
            + match style.channel_layout {
                ChannelLayout::Stack => per_ch_h * slot as f32,
                _ => 0.0,
            };
        let ch_mid = ch_top + per_ch_h * 0.5;
        let ch_half = per_ch_h * 0.5;

        for px in 0..pixel_count {
            let p_start = view_start + (px as f64) * samples_per_pixel;
            let p_end = view_start + (px as f64 + 1.0) * samples_per_pixel;
            let pair = peak_in_view(pyramid, samples, valid_len, ch, p_start, p_end);

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

        // 1. キャッシュ: 必要なら LOD ピラミッドを再構築 (借用は短いスコープに閉じる)。
        let segments = {
            let pyramid: &mut WaveformPyramid = self.widget_state(wid);
            pyramid.ensure_built(&source);
            build_peak_segments(pyramid, &source, rect, view, style)
        };

        // 2. 描画コマンドを scene に積む。
        if !segments.is_empty() {
            self.push_lines(LineBatch {
                segments,
                line_width_px: style.line_width_px.max(1.0),
                clip_rect: Some(rect),
            });
        }

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
