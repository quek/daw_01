//! 範囲ラウドネス解析のレポート型と、オフライン走査中に値を積む収集器 (r.md #54)。
//!
//! daw_audio の freewheel 走査 (`daw_audio::export`) が [`LoudnessCollector`] に
//! master バッファを流し込み、[`LoudnessReport`] を `AudioEvent` で daw_gui へ返す。
//! 測定そのものは [`crate::loudness`] / [`crate::truepeak`] = **ライブメーターと
//! 同一の測定器**で、ここはその読み出しを「1 つのレポート」に束ねるだけ。
//!
//! # wire に載る形
//!
//! 時系列グラフとヒストグラムは**固定長配列**で運ぶ (`LOUDNESS_CURVE_COLUMNS` /
//! `LOUDNESS_HISTOGRAM_BINS`)。範囲の長さに依らずメッセージサイズが一定になるので、
//! 「protocol に bulk を直載せしない」不変条件と正面から整合する (spectrum / scope
//! の固定バンド数と同じ思想 — グラフは**表示物**であって解析の生データではない)。
//!
//! このファイルは wire を渡る型を定義するので `common/build.rs` の `WIRE_SOURCES`
//! に登録してある (不変条件 7)。

use bincode::{Decode, Encode};

use crate::loudness::{HIST_BINS, LoudnessMeter};
use crate::truepeak::TruePeakMeter;

/// 時系列グラフの列数。範囲全体をこの数へ等分割する。
/// spectrum (768 バンド) / oscilloscope (768 列) と揃えた。
pub const LOUDNESS_CURVE_COLUMNS: usize = 768;

/// レポートのヒストグラムのビン数 (1 LU 刻み、下端 [`LOUDNESS_HISTOGRAM_MIN_LUFS`])。
/// 測定器側の 0.1 dB / 1000 bin を 10 bin ずつ束ねたもの。
pub const LOUDNESS_HISTOGRAM_BINS: usize = 100;
/// ヒストグラム bin 0 の下端 [LUFS]。
pub const LOUDNESS_HISTOGRAM_MIN_LUFS: f32 = -70.0;
/// ヒストグラム 1 bin の幅 [LU]。
pub const LOUDNESS_HISTOGRAM_STEP_LU: f32 = 1.0;

/// 測定器の 0.1 dB bin をレポートの 1 LU bin へ束ねる比率。
const HIST_GROUP: usize = HIST_BINS / LOUDNESS_HISTOGRAM_BINS;

/// 範囲ラウドネス解析の結果。走査中は途中経過として同じ型が繰り返し送られ、
/// `complete` が立ったものが確定値 (= 最後の 1 通)。
///
/// 到達していないラウドネス値は `f32::NEG_INFINITY` ([`crate::loudness::LoudnessReadout`]
/// と同じ規約)。位置は「まだ無い」を `None` で表す。
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct LoudnessReport {
    /// 解析した範囲 (拍、song-absolute)。レポート窓の見出しに出す。
    pub range_start_beat: f64,
    pub range_end_beat: f64,
    /// 範囲先頭のフレーム位置 (engine が拍から換算した値)。
    /// レポート内の秒位置 → song 位置の変換基準。
    pub range_start_frame: u64,
    /// 解析に使ったサンプルレート [Hz]。
    pub sample_rate: u32,
    /// 走査済みフレーム数 / 範囲の総フレーム数 (進捗バー)。
    pub done_frames: u64,
    pub total_frames: u64,
    /// `true` = 走査完了 (以後この値は変わらない)。
    pub complete: bool,

    /// BS.1770 の 2 段ゲート積算ラウドネス [LUFS]。
    pub integrated_lufs: f32,
    /// EBU Tech 3342 のラウドネスレンジ [LU]。
    pub lra_lu: f32,
    /// 測定長が 60 秒未満 = LRA はまだ暫定 (Tech 3342)。
    pub lra_provisional: bool,
    /// 最大 Momentary (400ms 窓) [LUFS] とその窓の先頭位置 [秒] (範囲先頭起点)。
    pub max_momentary_lufs: f32,
    pub max_momentary_at_secs: Option<f32>,
    /// 最大 Short-term (3s 窓) [LUFS] とその窓の先頭位置 [秒]。
    pub max_short_term_lufs: f32,
    pub max_short_term_at_secs: Option<f32>,
    /// BS.1770 Annex 2 のトゥルーピーク [dBTP] とその位置 [秒]。
    pub true_peak_dbtp: f32,
    pub true_peak_at_secs: Option<f32>,
    /// サンプルピーク [dBFS] とその位置 [秒]。
    pub sample_peak_dbfs: f32,
    pub sample_peak_at_secs: Option<f32>,
    /// `|x| >= 1.0` だったサンプル数 (デジタルクリップ)。
    pub clipped_samples: u64,
    /// 実際に測定した長さ [秒]。
    pub measured_secs: f32,

    /// Short-term / Momentary の時系列。範囲全体を `LOUDNESS_CURVE_COLUMNS` 列へ
    /// 等分割し、その位置での値 [LUFS] を入れる。まだ走査していない列と
    /// 窓が埋まっていない列は `f32::NEG_INFINITY`。
    pub short_term_curve: [f32; LOUDNESS_CURVE_COLUMNS],
    pub momentary_curve: [f32; LOUDNESS_CURVE_COLUMNS],
    /// Short-term の分布 (= LRA の入力そのもの)。bin `i` は
    /// `LOUDNESS_HISTOGRAM_MIN_LUFS + i * LOUDNESS_HISTOGRAM_STEP_LU` から 1 LU 幅。
    pub histogram: [u32; LOUDNESS_HISTOGRAM_BINS],
}

impl Default for LoudnessReport {
    fn default() -> Self {
        Self {
            range_start_beat: 0.0,
            range_end_beat: 0.0,
            range_start_frame: 0,
            sample_rate: 0,
            done_frames: 0,
            total_frames: 0,
            complete: false,
            integrated_lufs: f32::NEG_INFINITY,
            lra_lu: 0.0,
            lra_provisional: true,
            max_momentary_lufs: f32::NEG_INFINITY,
            max_momentary_at_secs: None,
            max_short_term_lufs: f32::NEG_INFINITY,
            max_short_term_at_secs: None,
            true_peak_dbtp: f32::NEG_INFINITY,
            true_peak_at_secs: None,
            sample_peak_dbfs: f32::NEG_INFINITY,
            sample_peak_at_secs: None,
            clipped_samples: 0,
            measured_secs: 0.0,
            short_term_curve: [f32::NEG_INFINITY; LOUDNESS_CURVE_COLUMNS],
            momentary_curve: [f32::NEG_INFINITY; LOUDNESS_CURVE_COLUMNS],
            histogram: [0; LOUDNESS_HISTOGRAM_BINS],
        }
    }
}

impl LoudnessReport {
    /// 進捗 0.0〜1.0。総フレーム数が未確定なら 0。
    #[must_use]
    pub fn progress(&self) -> f32 {
        if self.total_frames == 0 {
            return 0.0;
        }
        (self.done_frames as f64 / self.total_frames as f64).clamp(0.0, 1.0) as f32
    }

    /// 目標ラウドネスに合わせるために必要なゲイン [dB]。
    /// Integrated が未到達 (無音) なら `None`。
    #[must_use]
    pub fn normalization_gain_db(&self, target_lufs: f32) -> Option<f32> {
        self.integrated_lufs
            .is_finite()
            .then_some(target_lufs - self.integrated_lufs)
    }

    /// 範囲先頭からの秒位置を song 全体のフレーム位置へ直す。
    #[must_use]
    pub fn song_frame_at(&self, secs: f32) -> u64 {
        let off = (f64::from(secs) * f64::from(self.sample_rate)).max(0.0) as u64;
        self.range_start_frame.saturating_add(off)
    }
}

/// オフライン走査中にレポートを組み立てる収集器。
///
/// 走査は `[range_start, range_end)` のみ (減衰 tail は測らない = 「範囲の
/// ラウドネス」の定義)。呼び出し側 (`daw_audio::export::render_loop`) は窓内の
/// フレームだけを [`Self::push`] する。
pub struct LoudnessCollector {
    meter: LoudnessMeter,
    true_peak: TruePeakMeter,
    sample_rate: u32,
    total_frames: u64,
    done_frames: u64,
    sample_peak: f32,
    sample_peak_at_frame: u64,
    clipped: u64,
    short_term_curve: [f32; LOUDNESS_CURVE_COLUMNS],
    momentary_curve: [f32; LOUDNESS_CURVE_COLUMNS],
    /// 直前に**確定させた**列 (次はこの次の列から埋める)。まだ無ければ `None`。
    filled_col: Option<usize>,
    /// 現在の列に積んでいる最大値 (確定は列が進んだとき)。
    pending_col: usize,
    pending_short_term: f32,
    pending_momentary: f32,
    range_start_beat: f64,
    range_end_beat: f64,
    range_start_frame: u64,
    /// `[[f32; 2]]` へ詰め替える再利用バッファ (この走査は off-RT なので
    /// 確保は開始時の 1 回だけ)。
    interleaved: Vec<[f32; 2]>,
}

impl LoudnessCollector {
    #[must_use]
    pub fn new(
        sample_rate: u32,
        range_start_beat: f64,
        range_end_beat: f64,
        range_start_frame: u64,
        total_frames: u64,
        max_block_frames: usize,
    ) -> Self {
        Self {
            meter: LoudnessMeter::new(sample_rate),
            true_peak: TruePeakMeter::new(sample_rate),
            sample_rate,
            total_frames,
            done_frames: 0,
            sample_peak: 0.0,
            sample_peak_at_frame: 0,
            clipped: 0,
            short_term_curve: [f32::NEG_INFINITY; LOUDNESS_CURVE_COLUMNS],
            momentary_curve: [f32::NEG_INFINITY; LOUDNESS_CURVE_COLUMNS],
            filled_col: None,
            pending_col: 0,
            pending_short_term: f32::NEG_INFINITY,
            pending_momentary: f32::NEG_INFINITY,
            range_start_beat,
            range_end_beat,
            range_start_frame,
            interleaved: vec![[0.0; 2]; max_block_frames],
        }
    }

    /// master バッファ 1 ブロックを取り込む。`l` / `r` は同じ長さ。
    ///
    /// 非有限サンプルは入口で 0 に潰す。1 つでも NaN / Inf が混ざると
    /// K-weighting の biquad 状態が汚染され、以降の測定値が二度と戻らない
    /// (ライブメーター側と同じ防御 — `docs/plan_master_meters.md` §6)。
    pub fn push(&mut self, l: &[f32], r: &[f32]) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        if self.interleaved.len() < n {
            self.interleaved.resize(n, [0.0; 2]);
        }
        for i in 0..n {
            let a = if l[i].is_finite() { l[i] } else { 0.0 };
            let b = if r[i].is_finite() { r[i] } else { 0.0 };
            self.interleaved[i] = [a, b];
            let peak = a.abs().max(b.abs());
            if peak > self.sample_peak {
                self.sample_peak = peak;
                self.sample_peak_at_frame = self.done_frames + i as u64;
            }
            if a.abs() >= 1.0 {
                self.clipped += 1;
            }
            if b.abs() >= 1.0 {
                self.clipped += 1;
            }
        }
        let block = &self.interleaved[..n];
        self.meter.process(block);
        self.true_peak.process(block);
        self.done_frames += n as u64;
        self.fill_curve();
    }

    /// 走査位置に対応する列まで曲線を埋める。
    ///
    /// 1 列には複数ブロックが入りうる (10 分の範囲なら 1 列 ≒ 0.78 秒 = 約 37
    /// ブロック) ので、**列内は最大値で畳む**。点サンプルすると Momentary の山
    /// (400ms) が列幅より短いときに構造的に消えて、「最大 Momentary は -14 と
    /// 書いてあるのに曲線はどこも -40」になる。widget 側の列畳み込み
    /// (`to_columns`) も最大なので、規約が上下で揃う。
    /// 逆に 1 ブロックが複数列にまたがる (= 短い範囲) 場合は、間の列を同じ値で
    /// 埋めて穴を作らない。
    fn fill_curve(&mut self) {
        if self.total_frames == 0 || self.done_frames == 0 {
            return;
        }
        let pos = (self.done_frames - 1).min(self.total_frames.saturating_sub(1));
        let col = ((pos as u128 * LOUDNESS_CURVE_COLUMNS as u128)
            / self.total_frames.max(1) as u128) as usize;
        let col = col.min(LOUDNESS_CURVE_COLUMNS - 1);
        let s = self.meter.short_term_lufs().map_or(f32::NEG_INFINITY, |v| v as f32);
        let m = self.meter.momentary_lufs().map_or(f32::NEG_INFINITY, |v| v as f32);

        if col > self.pending_col {
            // 列が進んだ = 前の列を確定させる。
            self.commit_pending_columns();
            self.pending_col = col;
            self.pending_short_term = f32::NEG_INFINITY;
            self.pending_momentary = f32::NEG_INFINITY;
        }
        self.pending_short_term = self.pending_short_term.max(s);
        self.pending_momentary = self.pending_momentary.max(m);
        // 走査中も「そこまでの最大」が見えるよう、現在列にも即書き込む
        // (確定時に同じ値で上書きされる)。
        self.commit_pending_columns();
    }

    /// `filled_col` の次から `pending_col` までを、積んだ最大値で埋める。
    fn commit_pending_columns(&mut self) {
        let from = match self.filled_col {
            Some(prev) if prev >= self.pending_col => self.pending_col,
            Some(prev) => prev + 1,
            None => 0,
        };
        for c in from..=self.pending_col {
            self.short_term_curve[c] = self.pending_short_term;
            self.momentary_curve[c] = self.pending_momentary;
        }
        self.filled_col = Some(self.pending_col);
    }

    /// 現時点のレポートを組み立てる。`complete` は呼び出し側が渡す
    /// (走査が最後まで回ったかどうかを知っているのはループ側)。
    #[must_use]
    pub fn report(&self, complete: bool) -> LoudnessReport {
        let readout = self.meter.readout();
        let sr = f64::from(self.sample_rate.max(1));
        let mut histogram = [0u32; LOUDNESS_HISTOGRAM_BINS];
        let bins = self.meter.short_term_histogram();
        for (i, &c) in bins.iter().enumerate() {
            let j = (i / HIST_GROUP).min(LOUDNESS_HISTOGRAM_BINS - 1);
            histogram[j] = histogram[j].saturating_add(u32::try_from(c).unwrap_or(u32::MAX));
        }
        LoudnessReport {
            range_start_beat: self.range_start_beat,
            range_end_beat: self.range_end_beat,
            range_start_frame: self.range_start_frame,
            sample_rate: self.sample_rate,
            done_frames: self.done_frames,
            total_frames: self.total_frames,
            complete,
            integrated_lufs: readout.integrated_lufs,
            lra_lu: readout.lra_lu,
            lra_provisional: readout.lra_provisional,
            max_momentary_lufs: readout.max_momentary_lufs,
            max_momentary_at_secs: readout.max_momentary_at_secs,
            max_short_term_lufs: readout.max_short_term_lufs,
            max_short_term_at_secs: readout.max_short_term_at_secs,
            true_peak_dbtp: self.true_peak.max_dbtp(),
            true_peak_at_secs: self
                .true_peak
                .max_at_frame()
                .map(|f| (f as f64 / sr) as f32),
            sample_peak_dbfs: if self.sample_peak > 0.0 {
                20.0 * self.sample_peak.log10()
            } else {
                f32::NEG_INFINITY
            },
            sample_peak_at_secs: (self.sample_peak > 0.0)
                .then(|| (self.sample_peak_at_frame as f64 / sr) as f32),
            clipped_samples: self.clipped,
            measured_secs: (self.done_frames as f64 / sr) as f32,
            short_term_curve: self.short_term_curve,
            momentary_curve: self.momentary_curve,
            histogram,
        }
    }
}

/// ヒストグラム bin `i` の中心ラウドネス [LUFS]。
#[must_use]
pub fn report_histogram_bin_lufs(i: usize) -> f32 {
    LOUDNESS_HISTOGRAM_MIN_LUFS + (i as f32 + 0.5) * LOUDNESS_HISTOGRAM_STEP_LU
}

/// 測定器側 (0.1 dB / 1000 bin) とレポート側 (1 LU / 100 bin) の対応を固定する。
/// bin をまとめる比率と、**下端の一致**の両方を静的に見る (片方だけだと
/// 下端が食い違ったまま「10 個ずつ束ねている」ので通ってしまう)。
const _: () = assert!(HIST_GROUP == 10);
const _: () = assert!(
    LOUDNESS_HISTOGRAM_MIN_LUFS as i32 == crate::loudness::HIST_MIN_LUFS as i32,
    "レポートと測定器のヒストグラム下端がずれている"
);
const _: () = assert!(
    (LOUDNESS_HISTOGRAM_STEP_LU as i32) == (HIST_GROUP as i32) / 10,
    "レポート 1 bin = 測定器 10 bin = 1.0 LU"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// 指定 dBFS の 1kHz 正弦をステレオで `secs` 秒ぶん流し込む。
    fn feed_sine(c: &mut LoudnessCollector, sample_rate: u32, dbfs: f32, secs: f32) {
        let amp = 10f32.powf(dbfs / 20.0);
        let total = (sample_rate as f32 * secs) as usize;
        let mut l = vec![0.0f32; 1024];
        let mut r = vec![0.0f32; 1024];
        let mut n = 0usize;
        while n < total {
            let block = 1024.min(total - n);
            for i in 0..block {
                let t = (n + i) as f32 / sample_rate as f32;
                let v = amp * (std::f32::consts::TAU * 1000.0 * t).sin();
                l[i] = v;
                r[i] = v;
            }
            c.push(&l[..block], &r[..block]);
            n += block;
        }
    }

    #[test]
    fn 定常正弦の_integrated_は入力レベルと一致する() {
        // BS.1770: -18 dBFS のステレオ相関信号 = -18.0 LUFS
        // (L/R 重み 1.0 の合成で +3.01、K-weighting の 997Hz ゲインで -3.01 が相殺)。
        let sr = 48_000;
        let total = u64::from(sr) * 10;
        let mut c = LoudnessCollector::new(sr, 0.0, 20.0, 0, total, 1024);
        feed_sine(&mut c, sr, -18.0, 10.0);
        let rep = c.report(true);
        assert!(
            (rep.integrated_lufs - (-18.0)).abs() < 0.1,
            "integrated = {}",
            rep.integrated_lufs
        );
        assert!(
            (rep.sample_peak_dbfs - (-18.0)).abs() < 0.1,
            "sample peak = {}",
            rep.sample_peak_dbfs
        );
        assert_eq!(rep.clipped_samples, 0);
        assert!(rep.complete);
        assert!((rep.measured_secs - 10.0).abs() < 0.01);
    }

    #[test]
    fn 曲線は走査した範囲を左から埋め切る() {
        let sr = 48_000;
        let total = u64::from(sr) * 5;
        let mut c = LoudnessCollector::new(sr, 0.0, 10.0, 0, total, 1024);
        feed_sine(&mut c, sr, -20.0, 5.0);
        let rep = c.report(true);
        // 最終列まで到達している。
        assert!(
            rep.short_term_curve[LOUDNESS_CURVE_COLUMNS - 1].is_finite(),
            "最終列が埋まっていない"
        );
        // 先頭 (窓が埋まる前) は未到達のまま。
        assert!(rep.short_term_curve[0].is_infinite());
        // 3s 窓が埋まったあと (= 全体の 3/5 より後) は有限。
        let idx = LOUDNESS_CURVE_COLUMNS * 4 / 5;
        assert!((rep.short_term_curve[idx] - (-20.0)).abs() < 0.2);
        // Momentary は 400ms で埋まるので、Short-term より早く立ち上がる。
        let early = LOUDNESS_CURVE_COLUMNS / 5;
        assert!(rep.momentary_curve[early].is_finite());
    }

    #[test]
    fn 最大値の発生位置を返す() {
        let sr = 48_000;
        let total = u64::from(sr) * 8;
        let mut c = LoudnessCollector::new(sr, 0.0, 16.0, 0, total, 1024);
        // 静か 4 秒 → 大きい 4 秒。最大 Momentary は後半に出る。
        feed_sine(&mut c, sr, -30.0, 4.0);
        feed_sine(&mut c, sr, -10.0, 4.0);
        let rep = c.report(true);
        let at = rep.max_momentary_at_secs.expect("最大 Momentary の位置");
        assert!(at >= 4.0, "静かな前半を指している: {at}");
        assert!((rep.max_momentary_lufs - (-10.0)).abs() < 0.3);
        let tp_at = rep.true_peak_at_secs.expect("トゥルーピークの位置");
        assert!(tp_at >= 4.0, "トゥルーピークが前半を指している: {tp_at}");
    }

    #[test]
    fn クリップしたサンプルを数える() {
        let sr = 48_000;
        let mut c = LoudnessCollector::new(sr, 0.0, 1.0, 0, 100, 8);
        c.push(&[0.5, 1.0, -1.5, 0.2], &[0.1, 0.2, 0.3, 2.0]);
        let rep = c.report(false);
        // L の 1.0 / -1.5 と R の 2.0 で 3 サンプル。
        assert_eq!(rep.clipped_samples, 3);
        assert!(!rep.complete);
    }

    #[test]
    fn 非有限サンプルは測定器へ入る前に潰す() {
        let sr = 48_000;
        let total = u64::from(sr);
        let mut c = LoudnessCollector::new(sr, 0.0, 2.0, 0, total, 1024);
        c.push(&[f32::NAN, f32::INFINITY], &[f32::NAN, 0.0]);
        feed_sine(&mut c, sr, -23.0, 1.0);
        let rep = c.report(true);
        assert!(
            rep.integrated_lufs.is_finite(),
            "NaN 混入で測定が復帰不能になっている: {}",
            rep.integrated_lufs
        );
        assert!((rep.integrated_lufs - (-23.0)).abs() < 0.3);
    }

    /// 列より短い山を曲線が取りこぼさない (列内は最大値で畳む)。
    ///
    /// 点サンプルしていた頃は、10 分の範囲 (1 列 ≒ 0.78 秒) に 0.15 秒の
    /// バーストを置くと数値 (最大 Momentary) と曲線が 26 dB 食い違った。
    #[test]
    fn 列より短い山も曲線に残る() {
        let sr = 48_000;
        // 列幅を Momentary 窓 (400ms) より広くするために長めの範囲にする
        // (768 列 × 1 秒 = 12.8 分相当を、時間を掛けずに総フレーム数だけで表現)。
        let total = u64::from(sr) * 60;
        let mut c = LoudnessCollector::new(sr, 0.0, 120.0, 0, total, 1024);
        feed_sine(&mut c, sr, -40.0, 20.0);
        feed_sine(&mut c, sr, -10.0, 0.5); // 列幅より短い山
        feed_sine(&mut c, sr, -40.0, 20.0);
        let rep = c.report(true);
        let curve_max = rep
            .momentary_curve
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(
            (curve_max - rep.max_momentary_lufs).abs() < 1.0,
            "曲線 {curve_max} が数値 {} と食い違う",
            rep.max_momentary_lufs
        );
    }

    #[test]
    fn 目標との差はゲインとして返る() {
        let mut rep = LoudnessReport { integrated_lufs: -20.0, ..Default::default() };
        assert_eq!(rep.normalization_gain_db(-14.0), Some(6.0));
        rep.integrated_lufs = f32::NEG_INFINITY;
        assert_eq!(rep.normalization_gain_db(-14.0), None);
    }
}
