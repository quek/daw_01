//! 次のコードの候補 (r.md #157 Q4、`docs/plan_chord_track.md` §3.1)。ポップアップの候補の列と
//! 上下キーの巡回が使う。
//!
//! 直前のコードとその位置のキーから、**ダイアトニック** (キーの音階の上に積んだ 3 和音) を
//! 土台にし、**終止形** (直前の 4 度上 = 5 度下への進行、属和音 → 主和音) と **五度圏の近さ**
//! (直前から五度圏で隣)、**共通音** の多さで並べる。キーが無ければ直前のコードからキーを推す
//! (メジャー → そのメジャー、マイナー → その自然短音階)。どちらも無ければ C メジャー。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use crate::model::{Chord, ChordKind};
use crate::scale::Scale;

/// 候補の最大数 (ポップアップの 1 行に並ぶ数)。
pub const MAX_SUGGESTIONS: usize = 8;

/// `key` = その位置のキー `(ルート, スケール)`。`prev` = 直前の箱のコード。
#[must_use]
pub fn suggest_next(prev: Option<&Chord>, key: Option<(u8, Scale)>) -> Vec<Chord> {
    let (key_root, mask) = key_mask(prev, key);
    let diatonic = diatonic_triads(key_root, mask);
    let mut scored: Vec<(f32, Chord)> = diatonic.iter().map(|c| (score(prev, c, key_root, true), *c)).collect();
    // ダイアトニックの外: 直前の 5 度下 (4 度上) のメジャー / 直前を主和音とみた属 7 (二次ドミナントの入口)。
    if let Some(p) = prev {
        for c in [Chord::new(p.root + 5, ChordKind::Major), Chord::new(p.root + 7, ChordKind::Dominant7)] {
            if !scored.iter().any(|(_, d)| d.root == c.root && d.kind == c.kind) {
                scored.push((score(prev, &c, key_root, false), c));
            }
        }
    }
    scored.retain(|(_, c)| prev.is_none_or(|p| p.root != c.root || p.kind != c.kind));
    // 同点は音階の度数順 (並べ直しで候補の並びが揺れない)。
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(MAX_SUGGESTIONS).map(|(_, c)| c).collect()
}

/// キーのルートと、ルート起点の 7 音の音階 (12-bit mask)。7 音でないスケール (ペンタトニック等) は
/// 短 3 度の有無でメジャー / 自然短音階に寄せる (3 和音を積めないため)。
fn key_mask(prev: Option<&Chord>, key: Option<(u8, Scale)>) -> (u8, u16) {
    let (root, scale) = match (key, prev) {
        (Some(k), _) => k,
        (None, Some(p)) if p.kind.is_minor() => (p.root, Scale::NaturalMinor),
        (None, Some(p)) => (p.root, Scale::Major),
        (None, None) => (0, Scale::Major),
    };
    let mask = scale.pitch_class_mask() & 0x0FFF;
    if mask.count_ones() == 7 {
        return (root % 12, mask);
    }
    let minor = mask & (1 << 3) != 0 && mask & (1 << 4) == 0;
    let fallback = if minor { Scale::NaturalMinor } else { Scale::Major };
    (root % 12, fallback.pitch_class_mask() & 0x0FFF)
}

/// 音階の各音の上に、音階の 3 度・5 度を積んだ 3 和音 (度数順)。
fn diatonic_triads(root: u8, mask: u16) -> Vec<Chord> {
    let degrees: Vec<u8> = (0..12u8).filter(|i| mask & (1 << i) != 0).collect();
    let n = degrees.len();
    (0..n)
        .filter_map(|i| {
            let third = (degrees[(i + 2) % n] + 12 - degrees[i]) % 12;
            let fifth = (degrees[(i + 4) % n] + 12 - degrees[i]) % 12;
            let kind = match (third, fifth) {
                (4, 7) => ChordKind::Major,
                (3, 7) => ChordKind::Minor,
                (3, 6) => ChordKind::Diminished,
                (4, 8) => ChordKind::Augmented,
                _ => return None,
            };
            Some(Chord::new(root + degrees[i], kind))
        })
        .collect()
}

fn score(prev: Option<&Chord>, c: &Chord, key_root: u8, diatonic: bool) -> f32 {
    let mut s = if diatonic { 2.0 } else { 0.0 };
    let Some(p) = prev else {
        // 直前が無い: 主和音 → 下属 → 属の順 (曲の頭の定番)。
        return s + match (c.root + 12 - key_root) % 12 {
            0 => 3.0,
            5 => 2.0,
            7 => 1.5,
            _ => 0.0,
        };
    };
    let motion = (c.root + 12 - p.root % 12) % 12;
    s += match motion {
        5 => 3.0,     // 5 度下 (終止形 / 強進行)
        7 => 1.5,     // 5 度上 (五度圏の反対隣)
        2 | 10 => 1.0, // 2 度 (IV → V 等)
        _ => 0.0,
    };
    // 属和音 → 主和音。
    if (p.root + 12 - key_root) % 12 == 7 && (c.root + 12 - key_root) % 12 == 0 {
        s += 1.5;
    }
    let common = (p.pitch_class_mask() & c.pitch_class_mask()).count_ones();
    s + 0.5 * common as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C メジャーで G の次は C が先頭 (属 → 主)、候補は直前を含まず、ダイアトニックが並ぶ。
    #[test]
    fn dominant_resolves_to_tonic_first() {
        let g = Chord::new(7, ChordKind::Major);
        let got = suggest_next(Some(&g), Some((0, Scale::Major)));
        assert_eq!(got.first(), Some(&Chord::new(0, ChordKind::Major)));
        assert!(!got.contains(&g));
        assert!(got.contains(&Chord::new(9, ChordKind::Minor)), "Am (vi) も候補に入る: {got:?}");
        assert!(got.len() <= MAX_SUGGESTIONS);
    }

    /// キーが無ければ直前のコードからキーを推す (Am → A 自然短音階: Dm / Em / F / G / C …)。
    #[test]
    fn key_is_inferred_from_the_previous_chord() {
        let am = Chord::new(9, ChordKind::Minor);
        let got = suggest_next(Some(&am), None);
        assert_eq!(got.first(), Some(&Chord::new(2, ChordKind::Minor)), "5 度下の Dm が先頭: {got:?}");
        assert!(got.contains(&Chord::new(4, ChordKind::Minor)));
    }
}
