// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! 波形表示ウィジェット (`Ui::waveform` / `Ui::waveform_segments`)。
//!
//! 設計の要点 (詳細は `docs/plan.html`「波形表示 UI 詳細設計」):
//! - 1 ウィジェット = **複数区間** (`WaveformSegment`) で、区間ごとに (rect ↔ サンプル
//!   範囲) の線形写像を持つ。区間の間は描かない = 無音の隙間。`Ui::waveform` は
//!   1 区間の薄いラッパ。LOD ピラミッドは区間数によらず id ごとに 1 つ。
//! - 入力は **生サンプルの借用** (`SampleSlices<'s>`) と `valid_len` + `generation`。
//!   `generation` が一致すれば内部 LOD ピラミッドを再利用する。
//! - LOD ピラミッド (`WaveformPyramid`) は `WidgetState` の blanket impl 経由で
//!   `UiHost.state: HashMap<WidgetId, Box<dyn WidgetState>>` に乗る。
//!   ユーザ Model 型に `Clone`/`PartialEq`/`Hash`/`Default` は要求しない。
//! - M2 では `WaveformRenderMode::PeakLines` のみ実装 (RMS / SamplePolyline / Auto は M5)。
//! - M2 では完全再構築 (`generation` 変化で全レベル作り直し) のみ。
//!   インクリメンタル拡張 (録音中の `valid_len` 拡大) は後段で。

use std::hash::Hash;

use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::theme::Palette;
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
    /// この範囲を **右→左** に描く (= rect 左端が `start_sample + len_samples`)。
    /// 逆再生 (`AudioEvent.reversed`) のように「時間が進むと source を末尾から
    /// 手前へ読む」 素材を、サンプル範囲を分割せずそのまま渡すための向き指定。
    /// ヒットテストも同じ写像で戻す。
    pub reversed: bool,
}

impl Default for WaveformView {
    fn default() -> Self {
        Self { start_sample: 0, len_samples: 0, vertical_gain: 1.0, reversed: false }
    }
}

/// 1 ウィジェットの中に描く波形 1 区間 (rect ↔ サンプル範囲の線形写像)。
///
/// [`Ui::waveform_segments`] は複数区間を **1 つの LOD ピラミッド**で描く。
/// 区間ごとに `Ui::waveform` を呼ぶと WidgetId ごとにピラミッドが作られ、
/// source 全長ぶんのメモリが区間数だけ複製されてしまう (スライス 30 個の
/// ループで 17 MB 超)。 スライス配置 / warp 区間 / 逆再生はこの型で表す。
#[derive(Debug, Clone, Copy)]
pub struct WaveformSegment {
    pub rect: Rect,
    pub view: WaveformView,
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

impl WaveformStyle {
    /// パレットから既定スタイルを組む (r.md #48)。
    ///
    /// **`Default` にはしない**: テーマ色を読む `Default::default()` は隠れたグローバル依存で、
    /// ライトテーマに追従しない。
    ///
    /// 波形インクは **極性固定** (テーマではなく「どんな背景の上に描くか」 で決まる)。 この既定は
    /// **暗い背景**を前提とした `waveform_on_dark` / `waveform_peak_on_dark`。 ユーザー着色クリップの
    /// ように背景が可変な呼び出し側は、 塗った背景色から
    /// [`Palette::waveform_for`] で `fg` / `fg_clipped` を取り直すこと
    /// (明るいクリップの上で波形が消える事故を構造的に防ぐ)。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            fg: p.waveform_on_dark,
            fg_clipped: p.waveform_peak_on_dark,
            fill: None,
            // 中央線はグリッド hairline と同じ層 (テーマ従属)。 既定のグリッドより更に薄い。
            baseline: Some(p.grid_line.with_alpha(0.10)),
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
    /// このペアがカバーするサンプル範囲の `sum(s^2)`。RmsBars 描画時に
    /// `sqrt(sum_sq / n)` で RMS を計算する (Phase 16)。
    /// メモリ +50% (8B → 12B/pair) は DAW project の MB スケールに対し許容範囲。
    rms_sum_sq: f32,
}

impl MinMaxPair {
    const ZERO: Self = Self { min: 0.0, max: 0.0, rms_sum_sq: 0.0 };
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

/// `[MinMaxPair]` から min/max/rms_sum_sq を畳み込む。空なら `MinMaxPair::ZERO`。
/// peak_in_view の毎ピクセル hot path で呼ばれるので `#[inline(always)]`。
#[allow(clippy::inline_always)]
#[inline(always)]
fn fold_pairs(pairs: &[MinMaxPair]) -> MinMaxPair {
    if pairs.is_empty() {
        return MinMaxPair::ZERO;
    }
    let mut mn = f32::INFINITY;
    let mut mx = f32::NEG_INFINITY;
    let mut sum_sq = 0.0_f32;
    for p in pairs {
        if p.min < mn {
            mn = p.min;
        }
        if p.max > mx {
            mx = p.max;
        }
        sum_sq += p.rms_sum_sq;
    }
    if !mn.is_finite() {
        mn = 0.0;
    }
    if !mx.is_finite() {
        mx = 0.0;
    }
    MinMaxPair { min: mn, max: mx, rms_sum_sq: sum_sq }
}

/// 生サンプルから `[start, end)` の min/max/sum_sq を取る。enum variant ごとに hot loop を分ける。
fn peak_in_raw(samples: &SampleSlices<'_>, ch: usize, start: usize, end: usize) -> MinMaxPair {
    if end <= start {
        return MinMaxPair::ZERO;
    }
    let mut mn = f32::INFINITY;
    let mut mx = f32::NEG_INFINITY;
    let mut sum_sq = 0.0_f32;
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
                        sum_sq += v * v;
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
                        sum_sq += v * v;
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
                    sum_sq += v * v;
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
    MinMaxPair { min: mn, max: mx, rms_sum_sq: sum_sq }
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
            // `reversed` は同じサンプル範囲を右→左に読む (x ↔ sample の対応だけ反転)。
            let src_px = if view.reversed { pixel_count - 1 - px } else { px };
            let p_start = view_start + (src_px as f64) * samples_per_pixel;
            let p_end = p_start + samples_per_pixel;
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
            let x = rect.x + (sample_x_offset(local_pos, view_len, view.reversed) * x_per_sample) as f32;
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

/// `SamplePolyline` モード時の各サンプル点マーカー (rect 角丸円、knob と同パターン) を生成
/// (Phase 16)。`samples_per_pixel < 0.25` (= 1 sample あたり 4px 以上) のときのみ描画する
/// (それより粗いと点が密集して視認性が落ちる + rect 数爆発)。
///
/// `radius: [r; 4]` (r = サイズ/2) で完全な円。
#[allow(clippy::many_single_char_names)]
fn build_sample_polyline_markers(
    src: &WaveformSource<'_>,
    rect: Rect,
    view: WaveformView,
    style: WaveformStyle,
) -> Vec<RectCommand> {
    let view_len = view.len_samples.max(1) as f64;
    let pixel_w = f64::from(rect.w.max(1.0));
    let samples_per_pixel = view_len / pixel_w;
    if samples_per_pixel >= 0.25 {
        return Vec::new();
    }

    let channels = src.samples.channels();
    let valid_len = clamp_valid_len(src);
    if rect.w < 1.0 || rect.h < 1.0 || channels == 0 {
        return Vec::new();
    }

    let view_start = view.start_sample as f64;
    let x_per_sample = pixel_w / view_len;

    let (per_ch_h, ch_iter): (f32, Vec<usize>) = match style.channel_layout {
        ChannelLayout::Stack => (rect.h / channels as f32, (0..channels).collect()),
        ChannelLayout::Overlay => (rect.h, (0..channels).collect()),
        ChannelLayout::FirstOnly => (rect.h, vec![0]),
    };

    let s_start_int = view_start.max(0.0) as usize;
    let s_end_unclamped = (view_start + view_len) as usize + 1;
    let s_end_int = s_end_unclamped.min(valid_len);
    let n_samples = s_end_int.saturating_sub(s_start_int);

    // マーカーは line の 6 倍 (最低 6px) で line と区別できる視認サイズに。
    let marker_size = (style.line_width_px * 6.0).max(6.0);
    let r = marker_size * 0.5;
    let mut markers: Vec<RectCommand> =
        Vec::with_capacity(n_samples * ch_iter.len());

    for (slot, &ch) in ch_iter.iter().enumerate() {
        let ch_top = rect.y
            + match style.channel_layout {
                ChannelLayout::Stack => per_ch_h * slot as f32,
                _ => 0.0,
            };
        let ch_mid = ch_top + per_ch_h * 0.5;
        let ch_half = per_ch_h * 0.5;

        for i in 0..n_samples {
            let sample_idx = s_start_int + i;
            let s = sample_at(&src.samples, ch, sample_idx);
            let local_pos = sample_idx as f64 - view_start;
            let x = rect.x + (sample_x_offset(local_pos, view_len, view.reversed) * x_per_sample) as f32;
            let v = (s * view.vertical_gain).clamp(-1.0, 1.0);
            let y = ch_mid - v * ch_half;
            let is_clipped = s.abs() > 1.0;
            let fill = if is_clipped { style.fg_clipped } else { style.fg };
            markers.push(RectCommand {
                rect: Rect { x: x - r, y: y - r, w: marker_size, h: marker_size },
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [r; 4],
                clip_rect: None,
            });
        }
    }

    markers
}

/// view 内サンプル位置 `local_pos` (= `sample_idx - view.start_sample`) を
/// rect 左端からの x オフセット (サンプル単位) に写す。 `reversed` なら
/// 右→左 (= `view_len - local_pos`)。 `SamplePolyline` の線と点で共有する。
#[inline]
fn sample_x_offset(local_pos: f64, view_len: f64, reversed: bool) -> f64 {
    if reversed { view_len - local_pos } else { local_pos }
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

/// 1 ピクセル分の RMS = `sqrt(sum_sq / n)` を返す (Phase 16)。
/// `peak_in_view_cached` を呼んで sum_sq を取り出し、サンプル数で割って sqrt する。
#[allow(clippy::inline_always)]
#[inline(always)]
fn rms_in_view_cached(
    col: Option<&[MinMaxPair]>,
    decim: Option<usize>,
    samples: &SampleSlices<'_>,
    valid_len: usize,
    ch: usize,
    sample_start: f64,
    sample_end: f64,
) -> f32 {
    let s_start = sample_start.max(0.0) as usize;
    let s_end = (sample_end.max(0.0) as usize).min(valid_len);
    let n = s_end.saturating_sub(s_start).max(1);
    let pair = peak_in_view_cached(col, decim, samples, valid_len, ch, sample_start, sample_end);
    (pair.rms_sum_sq / n as f32).sqrt()
}

/// `RmsBars` 描画 (Phase 16): `build_peak_segments` と同形だが、min/max の代わりに
/// `±RMS` の縦線を描く。RMS は常に正値なので `0..1` を `[ch_mid - ch_half, ch_mid + ch_half]`
/// に対称マップする (= ch_mid を中心に ±RMS の縦バー)。
#[allow(clippy::too_many_lines)]
fn build_rms_bar_segments(
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

    let mut segments: Vec<LineSegment> =
        Vec::with_capacity(pixel_count * ch_iter.len() + ch_iter.len());

    // baseline (build_peak_segments と完全同形)
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

    let span_int = samples_per_pixel as usize;
    let chosen_level: Option<&MinMaxLevel> = pyramid
        .levels
        .iter()
        .rfind(|l| (l.decimation as usize) <= span_int);

    for (slot, &ch) in ch_iter.iter().enumerate() {
        let ch_top = rect.y
            + match style.channel_layout {
                ChannelLayout::Stack => per_ch_h * slot as f32,
                _ => 0.0,
            };
        let ch_mid = ch_top + per_ch_h * 0.5;
        let ch_half = per_ch_h * 0.5;
        let cached_col: Option<&[MinMaxPair]> =
            chosen_level.and_then(|l| l.per_channel.get(ch).map(Vec::as_slice));

        for px in 0..pixel_count {
            let src_px = if view.reversed { pixel_count - 1 - px } else { px };
            let p_start = view_start + (src_px as f64) * samples_per_pixel;
            let p_end = p_start + samples_per_pixel;
            let rms = rms_in_view_cached(
                cached_col,
                chosen_level.map(|l| l.decimation as usize),
                samples,
                valid_len,
                ch,
                p_start,
                p_end,
            );

            let clipped = rms > 1.0;
            let color = if clipped { style.fg_clipped } else { style.fg };
            let v = (rms * view.vertical_gain).clamp(0.0, 1.0);
            let y_top = ch_mid - v * ch_half;
            let y_bot = (ch_mid + v * ch_half).max(y_top + 1.0);
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

/// scissor (`Ui::current_clip`) と交差しない区間を捨て、はみ出す区間は rect と
/// サンプル範囲を **同じ比率で** 切り詰める。
///
/// `PeakLines` / `RmsBars` のセグメント生成は `rect.w` の pixel ループなので、
/// 画面外に大きくはみ出す rect をそのまま渡すとコストが rect 幅に比例して跳ねる。
/// ここで画面内に有界化することで、アプリ側が手書きのクリップ計算を持たなくて済む。
fn cull_segment(seg: &WaveformSegment, clip: Option<Rect>) -> Option<WaveformSegment> {
    let r = seg.rect;
    if r.w <= 0.0 || r.h <= 0.0 {
        return None;
    }
    let Some(c) = clip else {
        return Some(*seg);
    };
    if r.y >= c.y + c.h || r.y + r.h <= c.y {
        return None;
    }
    let x0 = r.x.max(c.x);
    let x1 = (r.x + r.w).min(c.x + c.w);
    if x1 <= x0 {
        return None;
    }
    if x0 <= r.x && x1 >= r.x + r.w {
        return Some(*seg);
    }
    let f0 = f64::from((x0 - r.x) / r.w).clamp(0.0, 1.0);
    let f1 = f64::from((x1 - r.x) / r.w).clamp(0.0, 1.0);
    let start = seg.view.start_sample as f64;
    let len = seg.view.len_samples as f64;
    // `reversed` は左端が範囲末尾なので、x の [f0, f1] は sample の [1-f1, 1-f0]。
    let (s0, s1) = if seg.view.reversed {
        (start + (1.0 - f1) * len, start + (1.0 - f0) * len)
    } else {
        (start + f0 * len, start + f1 * len)
    };
    // `WaveformView` はサンプル範囲を整数で持つので、素朴に切り捨てると rect (float)
    // との対応が frac(s0) サンプルぶんずれる (= 高倍率で波形が最大数 px 横に動く)。
    // **サンプル境界の外側へ丸めた分だけ rect も広げて**位相を保つ (はみ出しは
    // 呼び出し側の scissor が落とす)。
    let px_per_sample = f64::from(r.w) / len.max(1.0);
    let (i0, i1) = (s0.max(0.0).floor(), s1.max(0.0).ceil().max(s0.max(0.0).floor() + 1.0));
    // 左端がどちらのサンプルに対応するかは向きで変わる (forward = i0、reversed = i1)。
    let lead = if seg.view.reversed { i1 - s1 } else { s0 - i0 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(WaveformSegment {
        rect: Rect {
            x: x0 - (lead * px_per_sample) as f32,
            y: r.y,
            w: ((i1 - i0) * px_per_sample) as f32,
            h: r.h,
        },
        view: WaveformView {
            start_sample: i0 as u64,
            len_samples: ((i1 - i0) as u64).max(1),
            ..seg.view
        },
    })
}

/// 全区間 + source + style を畳み込んだ `with_widget_node` の input hash。
/// 一致すれば LOD 再構築も描画もスキップされる (M4 Phase 12)。
/// 注: Rust の `Hash` 実装は tuple 要素 12 個まで。ネスト tuple で回避。
fn segments_input_hash(
    source: &WaveformSource<'_>,
    segments: &[WaveformSegment],
    style: WaveformStyle,
) -> u64 {
    let mut h = hash_inputs((
        b"waveform",
        (source.generation, source.valid_len as u64, source.sample_rate),
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
        segments.len() as u64,
    ));
    for seg in segments {
        h = hash_inputs((
            h,
            (
                seg.rect.x.to_bits(),
                seg.rect.y.to_bits(),
                seg.rect.w.to_bits(),
                seg.rect.h.to_bits(),
            ),
            (
                seg.view.start_sample,
                seg.view.len_samples,
                seg.view.vertical_gain.to_bits(),
            ),
            seg.view.reversed,
        ));
    }
    h
}

/// カリング済み 1 区間の線分を `lines` に積む (push はしない)。
///
/// 区間ごとに `LineBatch` を push すると renderer 側で
/// 「`set_scissor_rect` + `draw`」 が区間数ぶん走る (スライス数に比例した draw call)
/// ので、呼び出し側で 1 バッチにまとめられるよう **生成だけ**を行う。生成される
/// 線分は必ず区間 rect の内側なので、バッチ単位の scissor で足りる。
///
/// M5 Phase 15: `render_mode` を resolve して SamplePolyline / PeakLines を分岐。
/// SamplePolyline のときは LOD ピラミッドを触らず生サンプルから直接描画する
/// (samples_per_pixel < 1.0 = ピラミッド無意味な領域)。
fn build_waveform_segment<'a, M: ?Sized + 'static>(
    ui: &mut Ui<'a, M>,
    wid: WidgetId,
    source: &WaveformSource<'_>,
    seg: &WaveformSegment,
    style: WaveformStyle,
    lines: &mut Vec<LineSegment>,
) {
    let (rect, view) = (seg.rect, seg.view);
    let view_len = view.len_samples.max(1) as f64;
    let samples_per_pixel = view_len / f64::from(rect.w.max(1.0));
    let effective_mode = resolve_render_mode(style.render_mode, samples_per_pixel);
    let built = match effective_mode {
        WaveformRenderMode::SamplePolyline => {
            build_sample_polyline_segments(source, rect, view, style)
        }
        WaveformRenderMode::PeakLines => {
            let pyramid: &mut WaveformPyramid = ui.widget_state(wid);
            pyramid.ensure_built(source);
            build_peak_segments(pyramid, source, rect, view, style)
        }
        WaveformRenderMode::RmsBars => {
            let pyramid: &mut WaveformPyramid = ui.widget_state(wid);
            pyramid.ensure_built(source);
            build_rms_bar_segments(pyramid, source, rect, view, style)
        }
        WaveformRenderMode::Auto => unreachable!("resolve_render_mode で除去済"),
    };
    lines.extend(built);
    // M5 Phase 16: SamplePolyline のとき、samples_per_pixel < 0.25 ならサンプル点
    // マーカー (rect 角丸円、knob と同パターン) を追加 push。閾値超過なら markers
    // は空 Vec が返るので no-op。
    if effective_mode == WaveformRenderMode::SamplePolyline {
        for m in build_sample_polyline_markers(source, rect, view, style) {
            ui.push_rect(m);
        }
    }
}

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
    /// 1 区間だけの [`Ui::waveform_segments`] (= 実装本体)。
    pub fn waveform<'s>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        source: WaveformSource<'s>,
        view: WaveformView,
        style: WaveformStyle,
    ) -> WaveformResponse {
        self.waveform_segments(id, source, &[WaveformSegment { rect, view }], style)
    }

    /// 複数区間の波形を **1 つの LOD ピラミッド**で描く。
    ///
    /// 各区間は独立した (rect, サンプル範囲) の線形写像なので、スライス配置
    /// (`StretchMode::Slice` の onset ごとの trigger + gap)、warp marker の区分線形、
    /// 逆再生をすべて同じ経路で描ける。区間の間は何も描かない = 無音の隙間になる。
    ///
    /// - LOD ピラミッドは `id` 単位で 1 つ。区間を増やしてもメモリは増えない
    ///   (区間ごとに `waveform` を呼ぶと source 全長のピラミッドが区間数だけ複製される)。
    /// - 各区間は現在の scissor (`with_clip_rect`) で自動カリング + 切り詰めされる
    ///   (`cull_segment`)。画面外へ大きくはみ出す rect を渡してよい。
    /// - ヒットテストは pointer を含む最初の区間を返す (`WaveformHit.sample_index` は
    ///   その区間の写像で戻す)。
    pub fn waveform_segments<'s>(
        &mut self,
        id: impl Hash,
        source: WaveformSource<'s>,
        segments: &[WaveformSegment],
        style: WaveformStyle,
    ) -> WaveformResponse {
        let wid = WidgetId::ROOT.child((b"waveform", &id));

        // pyramid は wid 経由の widget_state で持つので、closure 内で取り直す。
        // source / style は closure 内で borrow / Copy。
        let input_hash = segments_input_hash(&source, segments, style);
        self.with_widget_node(wid, input_hash, |ui| {
            let clip = ui.current_clip;
            // 全区間を 1 バッチにまとめる (区間ごとに push すると renderer 側の
            // scissor 切替 + draw call がスライス数に比例して増える)。scissor は
            // 可視区間の bbox 1 つで足りる (線分は各区間 rect の内側に収まる)。
            let mut lines: Vec<LineSegment> = Vec::new();
            let mut bbox: Option<Rect> = None;
            for seg in segments {
                let Some(seg) = cull_segment(seg, clip) else {
                    continue;
                };
                build_waveform_segment(ui, wid, &source, &seg, style, &mut lines);
                bbox = Some(match bbox {
                    None => seg.rect,
                    Some(b) => {
                        let x0 = b.x.min(seg.rect.x);
                        let y0 = b.y.min(seg.rect.y);
                        let x1 = (b.x + b.w).max(seg.rect.x + seg.rect.w);
                        let y1 = (b.y + b.h).max(seg.rect.y + seg.rect.h);
                        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
                    }
                });
            }
            if !lines.is_empty() {
                ui.push_lines(LineBatch {
                    segments: lines.into(),
                    line_width_px: style.line_width_px.max(1.0),
                    clip_rect: bbox,
                });
            }
        });

        // 3. ヒットテスト。
        let pointer = self.pointer;
        let mut response = WaveformResponse {
            hovered: segments.iter().any(|s| hovered(s.rect, pointer)),
            clicked_at: None,
            dragging_at: None,
        };
        if let Some((px, py)) = pointer.pos
            && let Some(seg) = segments.iter().find(|s| s.rect.contains(px, py))
        {
            let rect = seg.rect;
            let view = seg.view;
            let local_x = px - rect.x;
            let local_y = py - rect.y;
            let frac = (f64::from(local_x) / f64::from(rect.w)).clamp(0.0, 1.0);
            let frac = if view.reversed { 1.0 - frac } else { frac };
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
                    // rms_sum_sq は浮動小数の累積順序 (peak の sum vs fold の sum) で
                    // 微妙に異なるため epsilon=1e-5 相対誤差 + 1e-12 絶対許容で比較。
                    let r_rms = r_pair.rms_sum_sq;
                    let a_rms = a_pair.rms_sum_sq;
                    let abs_diff = (r_rms - a_rms).abs();
                    let tol = r_rms.abs().max(a_rms.abs()) * 1e-5 + 1e-12;
                    assert!(
                        abs_diff <= tol,
                        "level {lvl_idx} ch {ch} pair {p} rms_sum_sq diff (ref={r_rms}, actual={a_rms}, diff={abs_diff}, tol={tol})",
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

    fn seg(x: f32, w: f32, start: u64, len: u64, reversed: bool) -> WaveformSegment {
        WaveformSegment {
            rect: Rect { x, y: 0.0, w, h: 40.0 },
            view: WaveformView { start_sample: start, len_samples: len, vertical_gain: 1.0, reversed },
        }
    }

    /// scissor ではみ出した区間は rect と **同じ比率で** サンプル範囲も切り詰める
    /// (= 画面内に有界化しても波形が横にずれない)。
    #[test]
    fn cull_segment_narrows_rect_and_view_proportionally() {
        let clip = Rect { x: 100.0, y: 0.0, w: 100.0, h: 40.0 };
        // [50, 250) の rect を [100, 200) に切ると、左 25% と右 25% が落ちる。
        let got = cull_segment(&seg(50.0, 200.0, 1_000, 4_000, false), Some(clip)).expect("visible");
        assert!((got.rect.x - 100.0).abs() < 1e-6 && (got.rect.w - 100.0).abs() < 1e-6);
        assert_eq!(got.view.start_sample, 2_000);
        assert_eq!(got.view.len_samples, 2_000);

        // 完全に外 / 交差なし → 描かない。
        assert!(cull_segment(&seg(0.0, 40.0, 0, 100, false), Some(clip)).is_none());
        assert!(cull_segment(&seg(300.0, 40.0, 0, 100, false), Some(clip)).is_none());
        // 完全に内側 → そのまま (切り詰めによる丸め誤差を入れない)。
        let got = cull_segment(&seg(120.0, 40.0, 7, 99, false), Some(clip)).expect("visible");
        assert_eq!((got.view.start_sample, got.view.len_samples), (7, 99));
    }

    /// `reversed` の切り詰めは forward と **別式** であること (左端 = 範囲末尾)。
    /// 対称なカリング (左右同量) では両式が一致してしまい判別できないので、
    /// **非対称** (右半分だけ残す) で検証する。
    #[test]
    fn cull_segment_reversed_uses_mirrored_sample_range() {
        // seg = x[0,200) / sample[1000,5000)、clip = x[0,100) → 左半分だけ可視。
        let clip = Rect { x: 0.0, y: 0.0, w: 100.0, h: 40.0 };
        let s = seg(0.0, 200.0, 1_000, 4_000, true);
        let got = cull_segment(&s, Some(clip)).expect("visible");
        // reversed の左半分 = サンプル範囲の **後半** (3000..5000)。
        // forward 式に退化していると 1000..3000 になる。
        assert_eq!(got.view.start_sample, 3_000, "reversed の左半分は範囲後半");
        assert_eq!(got.view.len_samples, 2_000);
        assert!(got.view.reversed);
        // 同じ切り方を forward でやると前半 (1000..3000)。
        let got = cull_segment(&seg(0.0, 200.0, 1_000, 4_000, false), Some(clip)).expect("visible");
        assert_eq!(got.view.start_sample, 1_000);
        assert_eq!(got.view.len_samples, 2_000);
    }

    /// 整数丸めで rect とサンプル範囲の位相がずれない (= 高倍率で波形が横に飛ばない)。
    /// サンプル境界の外側へ丸めた分だけ rect も広げるので、
    /// 「rect 左端が指すサンプル」 は切り詰め前後で不変。
    #[test]
    fn cull_segment_keeps_sample_phase_when_rounding() {
        // 1 サンプル = 4px。x=10 で切ると s0 = 2.5 サンプル目 (非整数)。
        let clip = Rect { x: 10.0, y: 0.0, w: 100.0, h: 40.0 };
        let s = seg(0.0, 40.0, 0, 10, false);
        let got = cull_segment(&s, Some(clip)).expect("visible");
        assert_eq!(got.view.start_sample, 2, "floor したサンプルから始める");
        // sample 2 の x = 元 rect 基準で 8.0 → 丸めた分だけ rect も左へ広がる。
        assert!((got.rect.x - 8.0).abs() < 1e-4, "got {}", got.rect.x);
        let px_per_sample = f64::from(got.rect.w) / got.view.len_samples as f64;
        assert!((px_per_sample - 4.0).abs() < 1e-4, "スケールも保つ: {px_per_sample}");
    }

    /// `reversed` は同じサンプル範囲を右→左に描く (ピーク線の x が鏡像になる)。
    #[test]
    fn reversed_view_mirrors_peak_lines() {
        // 前半が無音・後半が振幅 1.0 の素材 → 通常は右半分、reversed は左半分が振れる。
        let mut s = vec![0.0f32; 2_000];
        for (i, v) in s.iter_mut().enumerate() {
            *v = if i >= 1_000 { if i % 2 == 0 { 1.0 } else { -1.0 } } else { 0.0 };
        }
        let samples = SampleSlices::Mono(&s);
        let src = WaveformSource { samples, valid_len: 2_000, generation: 1, sample_rate: 48_000 };
        let pyramid = build_full(&samples, 2_000);
        let rect = Rect { x: 0.0, y: 0.0, w: 20.0, h: 40.0 };
        let style = WaveformStyle {
            baseline: None,
            channel_layout: ChannelLayout::Overlay,
            ..WaveformStyle::from_palette(&Palette::dark())
        };
        let height = |reversed: bool, px: usize| -> f32 {
            let view = WaveformView { start_sample: 0, len_samples: 2_000, vertical_gain: 1.0, reversed };
            let segs = build_peak_segments(&pyramid, &src, rect, view, style);
            (segs[px].b[1] - segs[px].a[1]).abs()
        };
        assert!(height(false, 15) > height(false, 4), "通常は右側が振れる");
        assert!(height(true, 4) > height(true, 15), "reversed は左側が振れる");
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
