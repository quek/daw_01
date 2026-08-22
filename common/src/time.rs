// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `TimeMapping` — sample / bar / beat / SMPTE 間の双方向変換 (M7 Phase 27)。
//!
//! piano_roll / arrangement / time_ruler が共通で使う時間軸モデル。
//! 4/4 + tempo 120 BPM などの拍子情報を 1 つに集約。

/// 時間軸の表示モード (time_ruler の label にも使う)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeDisplay {
    BarBeat,
    Smpte24,
    Smpte25,
    Smpte30,
    Seconds,
}

/// 拍子 + tempo + sample rate を 1 つに束ねた時間軸モデル。
#[derive(Debug, Clone, Copy)]
pub struct TimeMapping {
    pub sample_rate: f64,
    pub tempo_bpm: f64,
    /// 例: 4/4 → (numerator=4, denominator=4)
    pub time_sig: (u8, u8),
    pub display: TimeDisplay,
}

impl TimeMapping {
    /// 4/4, tempo 120, 48 kHz, BarBeat 表示の典型デフォルト。
    pub const fn default_4_4_120() -> Self {
        Self {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            time_sig: (4, 4),
            display: TimeDisplay::BarBeat,
        }
    }

    /// 1 beat あたりの sample 数。
    pub fn samples_per_beat(&self) -> f64 {
        self.sample_rate * 60.0 / self.tempo_bpm
    }

    /// 1 bar あたりの sample 数。`time_sig.0 / time_sig.1` から計算。
    /// 4/4 なら 4 beat = 4 * samples_per_beat。
    pub fn samples_per_bar(&self) -> f64 {
        let beats_per_bar = f64::from(self.time_sig.0) * 4.0 / f64::from(self.time_sig.1);
        self.samples_per_beat() * beats_per_bar
    }

    /// `samples` から (bar, beat_in_bar [1-based, 整数 + 小数]) を返す。
    pub fn samples_to_bar_beat(&self, samples: f64) -> (u32, f64) {
        let spb = self.samples_per_beat();
        let beats_total = samples / spb;
        let beats_per_bar = f64::from(self.time_sig.0) * 4.0 / f64::from(self.time_sig.1);
        let bar = (beats_total / beats_per_bar).floor() as u32 + 1;
        let beat_in_bar = (beats_total - (f64::from(bar) - 1.0) * beats_per_bar) + 1.0;
        (bar, beat_in_bar)
    }

    /// `(bar, beat_in_bar)` (1-based) を sample 数に。
    pub fn bar_beat_to_samples(&self, bar: u32, beat_in_bar: f64) -> f64 {
        let beats_per_bar = f64::from(self.time_sig.0) * 4.0 / f64::from(self.time_sig.1);
        let beats_total = (f64::from(bar) - 1.0) * beats_per_bar + (beat_in_bar - 1.0);
        beats_total * self.samples_per_beat()
    }

    /// `samples` を秒数に。
    pub fn samples_to_seconds(&self, samples: f64) -> f64 {
        samples / self.sample_rate
    }

    /// SMPTE フレームレート (display に応じて 24/25、それ以外は 30 default)。
    pub fn smpte_fps(&self) -> u32 {
        match self.display {
            TimeDisplay::Smpte24 => 24,
            TimeDisplay::Smpte25 => 25,
            _ => 30,
        }
    }

    /// `samples` を SMPTE (HH:MM:SS:FF) 文字列に。
    pub fn samples_to_smpte(&self, samples: f64) -> String {
        let total_secs = self.samples_to_seconds(samples);
        let fps = self.smpte_fps();
        let total_frames = (total_secs * f64::from(fps)).floor() as u64;
        let ff = total_frames % u64::from(fps);
        let total_secs_int = total_frames / u64::from(fps);
        let ss = total_secs_int % 60;
        let total_min = total_secs_int / 60;
        let mm = total_min % 60;
        let hh = total_min / 60;
        format!("{hh:02}:{mm:02}:{ss:02}:{ff:02}")
    }

    /// 表示モードに従って `samples` を文字列化 (time_ruler の label に使う)。
    pub fn format(&self, samples: f64) -> String {
        match self.display {
            TimeDisplay::BarBeat => {
                let (bar, beat) = self.samples_to_bar_beat(samples);
                format!("{bar}.{:.0}", beat.floor())
            }
            TimeDisplay::Seconds => format!("{:.2}s", self.samples_to_seconds(samples)),
            TimeDisplay::Smpte24 | TimeDisplay::Smpte25 | TimeDisplay::Smpte30 => {
                self.samples_to_smpte(samples)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn samples_per_beat_120bpm_48k() {
        let m = TimeMapping::default_4_4_120();
        // 120 BPM = 2 beats/sec → 1 beat = 0.5 sec → 24000 samples @ 48k
        assert_eq!(m.samples_per_beat(), 24_000.0);
    }

    #[test]
    fn samples_per_bar_4_4_120bpm() {
        let m = TimeMapping::default_4_4_120();
        // 4/4 → 4 beat/bar → 96000 samples
        assert_eq!(m.samples_per_bar(), 96_000.0);
    }

    #[test]
    fn samples_to_bar_beat_round_trip() {
        let m = TimeMapping::default_4_4_120();
        // bar 3, beat 2.5 → 3*96000 + 1.5*24000 = 288000 + 36000 …
        // 実際: bar_beat_to_samples(3, 2.5) → (3-1)*4 + (2.5-1) = 8 + 1.5 = 9.5 beats = 9.5 * 24000 = 228000
        let s = m.bar_beat_to_samples(3, 2.5);
        let (bar, beat) = m.samples_to_bar_beat(s);
        assert_eq!(bar, 3);
        assert!((beat - 2.5).abs() < 1e-6);
    }

    #[test]
    fn smpte_format_basic() {
        let m = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            time_sig: (4, 4),
            display: TimeDisplay::Smpte30,
        };
        // 1 sec @ 48k = 48000 samples = 30 frames @ 30fps → 00:00:01:00
        let s = m.samples_to_smpte(48_000.0);
        assert_eq!(s, "00:00:01:00");
    }
}
