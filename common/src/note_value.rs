//! 「`1/N` 音符 → 拍」の換算の SSoT。
//!
//! label `"1/N"` は **N 分音符** を表し、quarter note (1/4) を 1 beat の基準とする
//! (Cubase / Live / Reaper / FL Studio で共通の慣行、MIDI ticks per quarter note とも整合)。
//! 付点は 1.5 倍、三連は 2/3 倍。
//!
//! この換算は grid snap ([`crate::snap::SnapMode`])、クリップランチャーの量子化
//! ([`crate::model::LaunchQuantize`])、内蔵 Delay の tempo sync ([`crate::model::DelayDiv`])
//! の 3 か所が使う。**式はここにしか書かない** — 以前は snap と launcher に同じ式が 2 本あり、
//! 「片方だけ直すとグリッドがズレるので両方を見ること」という散文の申し送りで凌いでいた。
//!
//! `Bars` は拍子依存の別概念なので [`beats_per_bar`] に分けてある。

/// 音符値の種別 (係数)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NoteKind {
    /// 素の N 分音符 (係数 1)。
    #[default]
    Straight,
    /// 付点 (係数 1.5)。
    Dotted,
    /// 三連 (係数 2/3)。
    Triplet,
}

impl NoteKind {
    /// [`NoteKind::Straight`] を 1 としたときの倍率。
    #[must_use]
    pub fn factor(self) -> f64 {
        match self {
            Self::Straight => 1.0,
            Self::Dotted => 1.5,
            Self::Triplet => 2.0 / 3.0,
        }
    }

    /// label の接尾辞 (`""` / `"."` / `"T"`)。
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Straight => "",
            Self::Dotted => ".",
            Self::Triplet => "T",
        }
    }
}

/// 音符値 (`div` 分音符 + 種別)。`div = 0` は 1 とみなす (防御)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteValue {
    pub div: u32,
    pub kind: NoteKind,
}

impl NoteValue {
    #[must_use]
    pub const fn new(div: u32, kind: NoteKind) -> Self {
        Self { div, kind }
    }

    /// 拍の長さ。whole note (1/1) = 4 拍を base に `4/div × 係数`。
    ///
    /// - `1/4` → 1.0 / `1/8` → 0.5 / `1/16` → 0.25
    /// - `1/4.` → 1.5 / `1/8.` → 0.75
    /// - `1/4T` → 2/3 / `1/8T` → 1/3
    #[must_use]
    pub fn beats(self) -> f64 {
        4.0 / f64::from(self.div.max(1)) * self.kind.factor()
    }

    /// 表示ラベル (`"1/8"` / `"1/8."` / `"1/8T"`)。
    #[must_use]
    pub fn label(self) -> String {
        format!("1/{}{}", self.div.max(1), self.kind.suffix())
    }
}

/// 1 小節の拍数 (`numerator * 4 / denominator`)。4/4 → 4、3/4 → 3、6/8 → 3。
/// 各成分は 0 防御で `max(1)`。
#[must_use]
pub fn beats_per_bar(time_sig: (u8, u8)) -> f64 {
    let num = f64::from(time_sig.0.max(1));
    let den = f64::from(time_sig.1.max(1));
    num * 4.0 / den
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 音符値は業界標準の_1_over_n_解釈で拍に変換される() {
        // (div, kind, expected beats)
        let cases = [
            (1, NoteKind::Straight, 4.0),
            (2, NoteKind::Straight, 2.0),
            (4, NoteKind::Straight, 1.0),
            (8, NoteKind::Straight, 0.5),
            (16, NoteKind::Straight, 0.25),
            (32, NoteKind::Straight, 0.125),
            (4, NoteKind::Dotted, 1.5),
            (8, NoteKind::Dotted, 0.75),
            (4, NoteKind::Triplet, 2.0 / 3.0),
            (8, NoteKind::Triplet, 1.0 / 3.0),
            // div = 0 は 1 とみなす (dropdown から 0 が漏れても破綻しない)。
            (0, NoteKind::Straight, 4.0),
        ];
        for (div, kind, expected) in cases {
            let got = NoteValue::new(div, kind).beats();
            assert!((got - expected).abs() < 1e-12, "1/{div}{} got={got}", kind.suffix());
        }
    }

    #[test]
    fn 小節の拍数は拍子から求まる() {
        assert_eq!(beats_per_bar((4, 4)), 4.0);
        assert_eq!(beats_per_bar((3, 4)), 3.0);
        assert_eq!(beats_per_bar((6, 8)), 3.0);
        assert_eq!(beats_per_bar((0, 0)), 4.0);
    }
}
