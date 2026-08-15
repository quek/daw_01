//! ITU-R BS.1770-5 Annex 2 のトゥルーピーク測定 (r.md #50)。
//!
//! 規格が推奨する **order 48 / 4 phase の補間 FIR** をそのまま使う
//! (phase あたり 12 tap、phase2 = phase1 の逆順、phase3 = phase0 の逆順 =
//! 線形位相の対称性)。EBU Tech 3341 のトゥルーピーク適合テスト #15〜#19 を
//! 許容誤差 (+0.2 / -0.4 dBTP) 内で通ることを回帰テストで固定してある。
//!
//! 規格の「12.04 dB 減衰 → 補間 → +12.04 dB 復元」は **整数演算のヘッドルーム
//! 確保用**で、規格自身が「浮動小数なら不要」と明記しているので行わない。

/// phase あたりの tap 数。
const TAPS: usize = 12;

/// BS.1770-5 Annex 2 Table 3 の phase 0 係数 (最古 → 最新の順)。
///
/// 桁を規格の表そのままにしてある。すべて `k / 2^n` の**厳密な 2 進小数**なので
/// f32 でも誤差なく表現でき、clippy の言う「余分な桁」は無い (丸めた表記に
/// 書き換えると、規格の表と突き合わせられなくなるほうが損)。
#[allow(clippy::excessive_precision)]
const PHASE0: [f32; TAPS] = [
    0.001_708_984_375,
    0.010_986_328_125,
    -0.019_653_320_312_5,
    0.033_203_125,
    -0.059_448_242_187_5,
    0.137_329_101_562_5,
    0.972_167_968_75,
    -0.102_294_921_875,
    0.047_607_421_875,
    -0.026_611_328_125,
    0.014_892_578_125,
    -0.008_300_781_25,
];

/// 同 phase 1 係数 ([`PHASE0`] と同じ理由で規格の桁のまま)。
#[allow(clippy::excessive_precision)]
const PHASE1: [f32; TAPS] = [
    -0.029_174_804_687_5,
    0.029_296_875,
    -0.051_757_812_5,
    0.089_111_328_125,
    -0.166_503_906_25,
    0.465_087_890_625,
    0.779_785_156_25,
    -0.200_317_382_812_5,
    0.101_562_5,
    -0.058_227_539_062_5,
    0.033_081_054_687_5,
    -0.018_920_898_437_5,
];

fn reversed(src: &[f32; TAPS]) -> [f32; TAPS] {
    let mut out = [0.0; TAPS];
    for (i, v) in src.iter().rev().enumerate() {
        out[i] = *v;
    }
    out
}

/// 1 チャンネルぶんのトゥルーピーク検出器。
struct Channel {
    /// 最古 → 最新の順に並べた入力履歴。
    hist: [f32; TAPS],
    /// フィルタが充填されるまでに流したサンプル数 (充填前の出力は捨てる)。
    filled: usize,
}

impl Channel {
    fn new() -> Self {
        Self { hist: [0.0; TAPS], filled: 0 }
    }

    fn reset(&mut self) {
        self.hist = [0.0; TAPS];
        self.filled = 0;
    }

    /// 1 サンプル入れて、そのサンプル位置での補間ピーク (線形振幅) を返す。
    /// フィルタ充填中は `0.0` (= 過渡による過大読みを混ぜない)。
    #[inline]
    fn push(&mut self, x: f32, phases: &[[f32; TAPS]]) -> f32 {
        self.hist.copy_within(1.., 0);
        self.hist[TAPS - 1] = x;
        if self.filled < TAPS {
            self.filled += 1;
            return 0.0;
        }
        if phases.is_empty() {
            // fs >= 192kHz: 補間不要 (規格の n=1 相当)。
            return x.abs();
        }
        let mut peak = 0.0_f32;
        for coeffs in phases {
            let mut acc = 0.0_f32;
            for (c, h) in coeffs.iter().zip(self.hist.iter()) {
                acc += c * h;
            }
            let a = acc.abs();
            if a > peak {
                peak = a;
            }
        }
        peak
    }
}

/// ステレオのトゥルーピークメーター。
pub struct TruePeakMeter {
    sample_rate: u32,
    /// 使用する phase 係数列。fs < 96k なら 4 本 (4x)、fs < 192k なら 2 本 (2x)、
    /// それ以上は空 (補間せず `|x|`)。
    phases: Vec<[f32; TAPS]>,
    channels: [Channel; 2],
    /// 直近 `process` 呼び出しでのトゥルーピーク (線形振幅)。
    block_peak: f32,
    /// リセット以降の最大トゥルーピーク (線形振幅)。
    max_peak: f32,
}

impl TruePeakMeter {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            phases: Self::phases_for(sample_rate),
            channels: [Channel::new(), Channel::new()],
            block_peak: 0.0,
            max_peak: 0.0,
        }
    }

    fn phases_for(sample_rate: u32) -> Vec<[f32; TAPS]> {
        if sample_rate >= 192_000 {
            // 既に十分な時間分解能があるので補間しない。
            Vec::new()
        } else if sample_rate >= 96_000 {
            // 2x = 4 phase 補間器の offset 0 と 0.5 (= phase0 / phase2)。
            vec![PHASE0, reversed(&PHASE1)]
        } else {
            vec![PHASE0, PHASE1, reversed(&PHASE1), reversed(&PHASE0)]
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate == self.sample_rate {
            return;
        }
        self.sample_rate = sample_rate;
        self.phases = Self::phases_for(sample_rate);
        for c in &mut self.channels {
            c.reset();
        }
    }

    /// 積算最大値をリセットして、そこから測り直す。
    ///
    /// **フィルタ履歴も捨てる**。残しておくと直前のプログラムの尾がリセット
    /// 直後の 12 サンプルに漏れて、新しい測定の最大値を汚す。失うのは 0.25ms
    /// (48kHz で 12 サンプル) だけで、その間の出力は充填中として捨てられる。
    pub fn reset_max(&mut self) {
        self.max_peak = 0.0;
        self.block_peak = 0.0;
        for c in &mut self.channels {
            c.reset();
        }
    }

    pub fn process(&mut self, frames: &[[f32; 2]]) {
        let mut block = 0.0_f32;
        for fr in frames {
            for (ch, sample) in fr.iter().enumerate() {
                let p = self.channels[ch].push(*sample, &self.phases);
                if p > block {
                    block = p;
                }
            }
        }
        self.block_peak = block;
        if block > self.max_peak {
            self.max_peak = block;
        }
    }

    /// 直近ブロックのトゥルーピーク [dBTP]。無音は `f32::NEG_INFINITY`。
    pub fn block_dbtp(&self) -> f32 {
        to_dbtp(self.block_peak)
    }

    /// リセット以降の最大トゥルーピーク [dBTP]。
    pub fn max_dbtp(&self) -> f32 {
        to_dbtp(self.max_peak)
    }
}

fn to_dbtp(v: f32) -> f32 {
    if v <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * v.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EBU Tech 3341 Table 1 のトゥルーピークテスト信号を作る。
    /// `freq_div` = fs / 周波数 (4 なら fs/4)、`phase_deg` は度。
    fn tp_signal(fs: u32, freq_div: f64, amp: f64, phase_deg: f64, secs: f64) -> Vec<[f32; 2]> {
        let n = (f64::from(fs) * secs) as usize;
        let w = std::f64::consts::TAU / freq_div;
        let ph = phase_deg.to_radians();
        (0..n)
            .map(|i| {
                let v = (amp * (w * i as f64 + ph).sin()) as f32;
                [v, v]
            })
            .collect()
    }

    fn measure(fs: u32, freq_div: f64, amp: f64, phase_deg: f64) -> f32 {
        let mut m = TruePeakMeter::new(fs);
        m.process(&tp_signal(fs, freq_div, amp, phase_deg, 0.5));
        m.max_dbtp()
    }

    /// 期待値 ±(+0.2 / -0.4) dBTP (EBU Tech 3341 Table 1 の許容誤差)。
    fn assert_within_tolerance(got: f32, expected: f32, what: &str) {
        assert!(
            got <= expected + 0.2 && got >= expected - 0.4,
            "{what}: expected {expected} (+0.2/-0.4), got {got}"
        );
    }

    #[test]
    fn ebu_3341_case_15_fs_over_4_zero_phase() {
        assert_within_tolerance(measure(48_000, 4.0, 0.50, 0.0), -6.0, "#15");
    }

    #[test]
    fn ebu_3341_case_16_fs_over_4_45_degrees() {
        assert_within_tolerance(measure(48_000, 4.0, 0.50, 45.0), -6.0, "#16");
    }

    #[test]
    fn ebu_3341_case_17_fs_over_6_60_degrees() {
        assert_within_tolerance(measure(48_000, 6.0, 0.50, 60.0), -6.0, "#17");
    }

    #[test]
    fn ebu_3341_case_18_fs_over_8_67_5_degrees() {
        assert_within_tolerance(measure(48_000, 8.0, 0.50, 67.5), -6.0, "#18");
    }

    #[test]
    fn ebu_3341_case_19_fs_over_4_amplitude_1_41() {
        assert_within_tolerance(measure(48_000, 4.0, 1.41, 45.0), 3.0, "#19");
    }

    /// 充填前の過渡を最大値に含めない (含めると +0.65 dB の過大読みが出る)。
    #[test]
    fn the_filter_fill_transient_is_discarded() {
        let mut m = TruePeakMeter::new(48_000);
        // いきなり 1.0 の DC を入れても、最初の 12 サンプルは 0 を返す。
        m.process(&vec![[1.0, 1.0]; TAPS]);
        assert_eq!(m.max_dbtp(), f32::NEG_INFINITY);
    }

    /// 192kHz では補間せず素のサンプルピークになる。
    #[test]
    fn at_192k_the_meter_falls_back_to_sample_peak() {
        let mut m = TruePeakMeter::new(192_000);
        let mut frames = vec![[0.0_f32, 0.0]; TAPS];
        frames.push([0.5, 0.25]);
        m.process(&frames);
        let expected = 20.0 * 0.5_f32.log10();
        assert!((m.max_dbtp() - expected).abs() < 1e-4, "got {}", m.max_dbtp());
    }

    #[test]
    fn reset_max_clears_the_accumulated_peak() {
        let mut m = TruePeakMeter::new(48_000);
        m.process(&tp_signal(48_000, 4.0, 0.5, 45.0, 0.1));
        assert!(m.max_dbtp().is_finite());
        m.reset_max();
        assert_eq!(m.max_dbtp(), f32::NEG_INFINITY);
    }
}
