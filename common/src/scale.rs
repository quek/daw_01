//! Scale & Root: 楽曲のキー (root + scale) を時間軸 event として保持する。
//! 設計は `docs/plan_scale.html`。 SSoT は `Song.scale_changes` 1 本のタイムラインで、
//! 「`beat = 0` から最初の event」 → 「次の event でルート変更」 と表現する。
//! 空 Vec = 機能 OFF / chromatic 互換 (= 既存 project と完全互換)。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// 内蔵スケール一覧 (22 種 + Custom)。 Bitwig 23 / Cubase 22 の合集合をベースに、
/// 実用頻度の高いものを選定。 ルート起点の 12-bit pitch class mask に変換可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum Scale {
    // 教会旋法
    Major,
    NaturalMinor,
    Dorian,
    Phrygian,
    Lydian,
    Mixolydian,
    Locrian,

    // マイナー系
    HarmonicMinor,
    MelodicMinor,

    // ペンタトニック / ブルース
    MajorPentatonic,
    MinorPentatonic,
    Blues,

    // 対称スケール
    WholeTone,
    Diminished,
    HalfWholeDim,
    Chromatic,

    // ジャズ / ワールド
    HarmonicMajor,
    DoubleHarmonic,
    LydianDominant,
    PhrygianDominant,
    HungarianMinor,
    Japanese,

    /// ルート起点の 12-bit pitch class mask (bit 0 が root)。
    /// 上位 4 bit は無視 (= `m & 0x0FFF` で正規化)。
    Custom(u16),
}

/// 半音オフセット列から 12-bit mask を畳み込む const helper。
const fn bits_from(intervals: &[u8]) -> u16 {
    let mut m = 0u16;
    let mut i = 0;
    while i < intervals.len() {
        m |= 1u16 << intervals[i];
        i += 1;
    }
    m
}

impl Scale {
    /// ルート起点で「半音 d (0..=11) が in-scale か」 を表す 12-bit mask。
    /// bit 0 は常に root なので 1。
    pub const fn pitch_class_mask(self) -> u16 {
        match self {
            Scale::Major => bits_from(&[0, 2, 4, 5, 7, 9, 11]),
            Scale::NaturalMinor => bits_from(&[0, 2, 3, 5, 7, 8, 10]),
            Scale::Dorian => bits_from(&[0, 2, 3, 5, 7, 9, 10]),
            Scale::Phrygian => bits_from(&[0, 1, 3, 5, 7, 8, 10]),
            Scale::Lydian => bits_from(&[0, 2, 4, 6, 7, 9, 11]),
            Scale::Mixolydian => bits_from(&[0, 2, 4, 5, 7, 9, 10]),
            Scale::Locrian => bits_from(&[0, 1, 3, 5, 6, 8, 10]),
            Scale::HarmonicMinor => bits_from(&[0, 2, 3, 5, 7, 8, 11]),
            Scale::MelodicMinor => bits_from(&[0, 2, 3, 5, 7, 9, 11]),
            Scale::MajorPentatonic => bits_from(&[0, 2, 4, 7, 9]),
            Scale::MinorPentatonic => bits_from(&[0, 3, 5, 7, 10]),
            Scale::Blues => bits_from(&[0, 3, 5, 6, 7, 10]),
            Scale::WholeTone => bits_from(&[0, 2, 4, 6, 8, 10]),
            Scale::Diminished => bits_from(&[0, 2, 3, 5, 6, 8, 9, 11]),
            Scale::HalfWholeDim => bits_from(&[0, 1, 3, 4, 6, 7, 9, 10]),
            Scale::Chromatic => 0x0FFF,
            Scale::HarmonicMajor => bits_from(&[0, 2, 4, 5, 7, 8, 11]),
            Scale::DoubleHarmonic => bits_from(&[0, 1, 4, 5, 7, 8, 11]),
            Scale::LydianDominant => bits_from(&[0, 2, 4, 6, 7, 9, 10]),
            Scale::PhrygianDominant => bits_from(&[0, 1, 4, 5, 7, 8, 10]),
            Scale::HungarianMinor => bits_from(&[0, 2, 3, 6, 7, 8, 11]),
            Scale::Japanese => bits_from(&[0, 1, 5, 7, 8]),
            Scale::Custom(m) => m & 0x0FFF,
        }
    }

    /// UI / 永続化用の英語表示名 (i18n は当面英語固定)。
    pub const fn display_name(self) -> &'static str {
        match self {
            Scale::Major => "Major",
            Scale::NaturalMinor => "Minor",
            Scale::Dorian => "Dorian",
            Scale::Phrygian => "Phrygian",
            Scale::Lydian => "Lydian",
            Scale::Mixolydian => "Mixolydian",
            Scale::Locrian => "Locrian",
            Scale::HarmonicMinor => "Harmonic Minor",
            Scale::MelodicMinor => "Melodic Minor",
            Scale::MajorPentatonic => "Major Pentatonic",
            Scale::MinorPentatonic => "Minor Pentatonic",
            Scale::Blues => "Blues",
            Scale::WholeTone => "Whole Tone",
            Scale::Diminished => "Diminished",
            Scale::HalfWholeDim => "Half-Whole Diminished",
            Scale::Chromatic => "Chromatic",
            Scale::HarmonicMajor => "Harmonic Major",
            Scale::DoubleHarmonic => "Double Harmonic",
            Scale::LydianDominant => "Lydian Dominant",
            Scale::PhrygianDominant => "Phrygian Dominant",
            Scale::HungarianMinor => "Hungarian Minor",
            Scale::Japanese => "Japanese",
            Scale::Custom(_) => "Custom",
        }
    }

    /// UI dropdown 用、 Custom を除く 22 種類の並び。
    pub const ALL_PRESETS: &'static [Scale] = &[
        Scale::Major,
        Scale::NaturalMinor,
        Scale::Dorian,
        Scale::Phrygian,
        Scale::Lydian,
        Scale::Mixolydian,
        Scale::Locrian,
        Scale::HarmonicMinor,
        Scale::MelodicMinor,
        Scale::MajorPentatonic,
        Scale::MinorPentatonic,
        Scale::Blues,
        Scale::WholeTone,
        Scale::Diminished,
        Scale::HalfWholeDim,
        Scale::Chromatic,
        Scale::HarmonicMajor,
        Scale::DoubleHarmonic,
        Scale::LydianDominant,
        Scale::PhrygianDominant,
        Scale::HungarianMinor,
        Scale::Japanese,
    ];
}

/// ピッチクラス (0..=11) → 英語音名 (sharp 表記)。 キー文脈が無い (chromatic /
/// 調性 OFF) ときの既定。 キーが分かる場面では [`pitch_class_name_in_key`] を使い、
/// flat 系キーで `Bb` / `Eb` 等を正しく綴る。
pub const fn pitch_class_name(pc: u8) -> &'static str {
    PITCH_NAMES_SHARP[(pc % 12) as usize]
}

const PITCH_NAMES_SHARP: [&str; 12] =
    ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
const PITCH_NAMES_FLAT: [&str; 12] =
    ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"];

/// このキー (root + scale) が音名表記で flat を好むか。 五度圏上の調号で決める:
/// 各スケールを「相対メジャー」 (church mode / 主要 minor) の調号に従わせ、
/// メジャー tonic の五度圏位置 `(tonic*7) mod 12` が 7..=11 (= flat 側) なら true。
/// 対称・非調性・綴りが曖昧なスケール (WholeTone / Diminished / exotic) は慣習どおり
/// sharp を既定にして `false`。
#[must_use]
pub fn prefers_flats(root: u8, scale: Scale) -> bool {
    // scale tonic に足して相対メジャー tonic を得る半音数。
    let to_major: i32 = match scale {
        Scale::Major | Scale::MajorPentatonic | Scale::HarmonicMajor => 0,
        Scale::Dorian => 10,
        Scale::Phrygian => 8,
        Scale::Lydian => 7,
        Scale::Mixolydian => 5,
        Scale::NaturalMinor
        | Scale::MinorPentatonic
        | Scale::Blues
        | Scale::HarmonicMinor
        | Scale::MelodicMinor => 3,
        Scale::Locrian => 1,
        // 調性中心が曖昧 / 対称スケールは sharp 既定。
        Scale::WholeTone
        | Scale::Diminished
        | Scale::HalfWholeDim
        | Scale::Chromatic
        | Scale::DoubleHarmonic
        | Scale::LydianDominant
        | Scale::PhrygianDominant
        | Scale::HungarianMinor
        | Scale::Japanese
        | Scale::Custom(_) => return false,
    };
    let major_root = (i32::from(root) + to_major).rem_euclid(12) as u32;
    // 相対メジャーの五度圏位置。 0..=6 = sharp 側 (C, G, D, A, E, B, F#)、
    // 7..=11 = flat 側 (Db, Ab, Eb, Bb, F)。
    (major_root * 7) % 12 > 6
}

/// ピッチクラス (0..=11) → キー文脈に応じた英語音名。 [`prefers_flats`] が真なら
/// flat 表記 (`Db` / `Bb` 等)、 偽なら sharp 表記。 UI の root / 音名表示はこの関数を
/// 経由して、 例えば Bb メジャーで root を `A#` でなく `Bb` と綴る。
#[must_use]
pub fn pitch_class_name_in_key(pc: u8, root: u8, scale: Scale) -> &'static str {
    if prefers_flats(root, scale) {
        PITCH_NAMES_FLAT[(pc % 12) as usize]
    } else {
        PITCH_NAMES_SHARP[(pc % 12) as usize]
    }
}

/// 時間軸上の root + scale 変化点。 `beat` 昇順で `Song.scale_changes` に格納される。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ScaleChange {
    /// タイムライン位置 (beat、 song-global)。
    pub beat: f64,
    /// ルート pitch class (0..=11、 0 = C)。
    pub root: u8,
    /// スケール種別。
    pub scale: Scale,
}

impl ScaleChange {
    /// 指定 MIDI pitch (0..=127) が in-scale か。
    pub fn contains(&self, pitch: u8) -> bool {
        let d = (pitch as i32 - self.root as i32).rem_euclid(12) as u16;
        (self.scale.pitch_class_mask() >> d) & 1 == 1
    }

    /// 最寄りの in-scale な pitch を返す。 同距離なら上向きを優先 (Cubase 流)。
    /// 対象 mask が全 0 (= `Custom(0)` 等の異常 mask) のときは入力をそのまま返す。
    pub fn snap(&self, pitch: u8) -> u8 {
        if self.scale.pitch_class_mask() == 0 {
            return pitch;
        }
        if self.contains(pitch) {
            return pitch;
        }
        for off in 1i32..=12 {
            let up = pitch as i32 + off;
            if (0..=127).contains(&up) && self.contains(up as u8) {
                return up as u8;
            }
            let down = pitch as i32 - off;
            if (0..=127).contains(&down) && self.contains(down as u8) {
                return down as u8;
            }
        }
        pitch
    }

    /// scale 度 (0 = root、 1 = 2nd、 ...) で pitch を transpose。
    /// `pitch` が out-of-scale なら unchanged (= 度数の定義が曖昧なため)。
    /// 結果が 0..=127 を外れる場合も unchanged。
    pub fn step_transpose(&self, pitch: u8, steps: i32) -> u8 {
        if steps == 0 || !self.contains(pitch) {
            return pitch;
        }
        let mut p = pitch as i32;
        let mut remaining = steps;
        let dir = if steps > 0 { 1 } else { -1 };
        while remaining != 0 {
            loop {
                p += dir;
                if !(0..=127).contains(&p) {
                    return pitch;
                }
                if self.contains(p as u8) {
                    break;
                }
            }
            remaining -= dir;
        }
        p as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_mask() {
        // bits {0, 2, 4, 5, 7, 9, 11} = C, D, E, F, G, A, B
        assert_eq!(Scale::Major.pitch_class_mask(), 0b1010_1011_0101);
    }

    #[test]
    fn natural_minor_mask() {
        assert_eq!(Scale::NaturalMinor.pitch_class_mask(), 0b0101_1010_1101);
    }

    #[test]
    fn pentatonic_has_five_notes() {
        assert_eq!(Scale::MajorPentatonic.pitch_class_mask().count_ones(), 5);
        assert_eq!(Scale::MinorPentatonic.pitch_class_mask().count_ones(), 5);
    }

    #[test]
    fn chromatic_all_in() {
        assert_eq!(Scale::Chromatic.pitch_class_mask(), 0x0FFF);
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Chromatic };
        for p in 0u8..=127 {
            assert!(c.contains(p));
        }
    }

    #[test]
    fn custom_truncates_high_bits() {
        // 上位 4 bit はマスクで切られる
        assert_eq!(Scale::Custom(0xFFFF).pitch_class_mask(), 0x0FFF);
    }

    #[test]
    fn all_presets_have_root() {
        // root (bit 0) は必ず in-scale
        for s in Scale::ALL_PRESETS {
            assert!(s.pitch_class_mask() & 1 == 1, "{:?} missing root", s);
        }
    }

    #[test]
    fn c_major_contains_d() {
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        assert!(c.contains(62)); // D4
        assert!(c.contains(60)); // C4 = root
        assert!(!c.contains(61)); // C#4 = out
        assert!(!c.contains(63)); // D#4 = out
        assert!(c.contains(64)); // E4
    }

    #[test]
    fn snap_prefers_up_on_tie() {
        // C Major で D♭ (61) は C (60) と D (62) の同距離 → up = D を返す
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        assert_eq!(c.snap(61), 62);
    }

    #[test]
    fn snap_idempotent_on_in_scale() {
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        assert_eq!(c.snap(60), 60);
        assert_eq!(c.snap(62), 62);
        assert_eq!(c.snap(64), 64);
    }

    #[test]
    fn snap_boundary_low() {
        // pitch 0 は C = in-scale なら unchanged
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        assert_eq!(c.snap(0), 0);
        // pitch 1 (C#) は up = D (2) に snap される (down は -1 で out of range)
        assert_eq!(c.snap(1), 2);
    }

    #[test]
    fn snap_boundary_high() {
        // pitch 127 (G8) は C Major で in-scale (127 % 12 = 7 = G)
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        assert_eq!(c.snap(127), 127);
        // pitch 126 (F#8) は up = 127 (G), down = 125 (F)、 同距離なら up
        assert_eq!(c.snap(126), 127);
    }

    #[test]
    fn snap_with_zero_mask_passthrough() {
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Custom(0) };
        assert_eq!(c.snap(60), 60); // 無限ループ防止
    }

    #[test]
    fn step_transpose_within_scale() {
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        // C (60) → +1 step → D (62)
        assert_eq!(c.step_transpose(60, 1), 62);
        // C (60) → +2 step → E (64)
        assert_eq!(c.step_transpose(60, 2), 64);
        // C (60) → +7 step → 次オクターブの C (72)
        assert_eq!(c.step_transpose(60, 7), 72);
        // C (60) → -1 step → 1 オクターブ下の B (59)
        assert_eq!(c.step_transpose(60, -1), 59);
    }

    #[test]
    fn step_transpose_out_of_scale_unchanged() {
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        // C# (61) は out-of-scale なので unchanged
        assert_eq!(c.step_transpose(61, 1), 61);
    }

    #[test]
    fn step_transpose_zero_unchanged() {
        let c = ScaleChange { beat: 0.0, root: 0, scale: Scale::Major };
        assert_eq!(c.step_transpose(60, 0), 60);
    }

    #[test]
    fn pitch_class_name_basic() {
        assert_eq!(pitch_class_name(0), "C");
        assert_eq!(pitch_class_name(1), "C#");
        assert_eq!(pitch_class_name(11), "B");
        assert_eq!(pitch_class_name(12), "C"); // overflow safe via % 12
    }

    #[test]
    fn prefers_flats_follows_circle_of_fifths() {
        // (root, scale, expected_prefers_flats)
        let cases = [
            (0, Scale::Major, false),  // C major: 0 accidentals → sharp 既定
            (7, Scale::Major, false),  // G major: 1 sharp
            (2, Scale::Major, false),  // D major: 2 sharps
            (11, Scale::Major, false), // B major: 5 sharps
            (6, Scale::Major, false),  // F#/Gb major: tie → F# (sharp 慣習)
            (5, Scale::Major, true),   // F major: 1 flat
            (10, Scale::Major, true),  // Bb major: 2 flats
            (3, Scale::Major, true),   // Eb major: 3 flats
            (1, Scale::Major, true),   // Db major: 5 flats (C# は 7 sharp で非慣習)
            (9, Scale::NaturalMinor, false), // A minor → C major: 0 accidentals
            (4, Scale::NaturalMinor, false), // E minor → G major: 1 sharp
            (2, Scale::NaturalMinor, true),  // D minor → F major: 1 flat
            (7, Scale::NaturalMinor, true),  // G minor → Bb major: 2 flats
            // 対称・非調性は常に sharp 既定。
            (5, Scale::WholeTone, false),
            (10, Scale::Diminished, false),
            (1, Scale::Chromatic, false),
            (3, Scale::Custom(0xABC), false),
        ];
        for (root, scale, expected) in cases {
            assert_eq!(
                prefers_flats(root, scale),
                expected,
                "root={root} scale={scale:?}"
            );
        }
    }

    #[test]
    fn pitch_class_name_in_key_spells_flat_keys() {
        // Bb major (root=10): root は A# でなく Bb、 4th(Eb) も flat 綴り。
        assert_eq!(pitch_class_name_in_key(10, 10, Scale::Major), "Bb");
        assert_eq!(pitch_class_name_in_key(3, 10, Scale::Major), "Eb");
        // C major (root=0): sharp 既定 (黒鍵は #)。
        assert_eq!(pitch_class_name_in_key(10, 0, Scale::Major), "A#");
        assert_eq!(pitch_class_name_in_key(1, 0, Scale::Major), "C#");
        // D minor (root=2) は flat 側 → Bb。
        assert_eq!(pitch_class_name_in_key(10, 2, Scale::NaturalMinor), "Bb");
        // 白鍵はどちらの綴りでも同じ。
        assert_eq!(pitch_class_name_in_key(0, 10, Scale::Major), "C");
        assert_eq!(pitch_class_name_in_key(4, 10, Scale::Major), "E");
    }

    #[test]
    fn serde_roundtrip() {
        let c = ScaleChange { beat: 16.0, root: 5, scale: Scale::Dorian };
        let json = serde_json::to_string(&c).unwrap();
        let back: ScaleChange = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn bincode_roundtrip() {
        let c = ScaleChange { beat: 16.0, root: 5, scale: Scale::Custom(0xABC) };
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(c, cfg).unwrap();
        let (back, _): (ScaleChange, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(c, back);
    }
}
