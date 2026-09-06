//! r.md #113: PC キーボードによる仮想鍵盤の **配列** (純データ / 純関数)。
//! 設計は `docs/plan_virtual_keyboard.md`。
//!
//! REAPER / FL Studio 式の 2 段配列。下段 (`Z`..`/`) と上段 (`Q`..`P`) はそれぞれ
//! C から始まる 1 オクターブ半で、上段は下段の 1 オクターブ上 (`Q` と `,` は同じ音)。
//! 「どのキーが何半音か」 はこの表が SSoT で、 描画 (キー名の印字) も入力 (押下 →
//! ピッチ) も同じ表を引く。

use daw_ui_platform::PhysicalKey;

/// 配列 1 鍵: `(物理キー, 下段基準の半音オフセット)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMap {
    pub key: PhysicalKey,
    pub semitone: u8,
}

const fn ch(c: char, semitone: u8) -> KeyMap {
    KeyMap { key: PhysicalKey::Char(c), semitone }
}

const fn dg(d: u8, semitone: u8) -> KeyMap {
    KeyMap { key: PhysicalKey::Digit(d), semitone }
}

/// 下段: `Z S X D C V G B H N J M , L . ; /` = C C# D D# E F F# G G# A A# B C C# D D# E。
pub const LOWER_ROW: [KeyMap; 17] = [
    ch('Z', 0),
    ch('S', 1),
    ch('X', 2),
    ch('D', 3),
    ch('C', 4),
    ch('V', 5),
    ch('G', 6),
    ch('B', 7),
    ch('H', 8),
    ch('N', 9),
    ch('J', 10),
    ch('M', 11),
    ch(',', 12),
    ch('L', 13),
    ch('.', 14),
    ch(';', 15),
    ch('/', 16),
];

/// 上段: `Q 2 W 3 E R 5 T 6 Y 7 U I 9 O 0 P` = 下段の 1 オクターブ上 (C..E)。
pub const UPPER_ROW: [KeyMap; 17] = [
    ch('Q', 12),
    dg(2, 13),
    ch('W', 14),
    dg(3, 15),
    ch('E', 16),
    ch('R', 17),
    dg(5, 18),
    ch('T', 19),
    dg(6, 20),
    ch('Y', 21),
    dg(7, 22),
    ch('U', 23),
    ch('I', 24),
    dg(9, 25),
    ch('O', 26),
    dg(0, 27),
    ch('P', 28),
];

/// 鍵盤が覆う半音数 (下段 `Z` = 0 から上段 `P` = 28 まで、 2 オクターブ + 完全 4 度)。
pub const SPAN_SEMITONES: u8 = 29;

/// オクターブ下げ / 上げ (Shift 付きはベロシティ下げ / 上げ)。
pub const OCTAVE_DOWN_KEY: PhysicalKey = PhysicalKey::Char('[');
pub const OCTAVE_UP_KEY: PhysicalKey = PhysicalKey::Char(']');

/// 下段 `Z` の既定ピッチ = C3 (MIDI 48。 上段 `Q` が C4 = 60 = 中央ド)。
pub const DEFAULT_BASE_PITCH: u8 = 48;
/// 基準ピッチは C 揃え (12 の倍数)。 上端は最高音 `P` (= base + 28) が 127 を超えない最大の C。
pub const MAX_BASE_PITCH: u8 = 96;
const _: () = assert!(MAX_BASE_PITCH + SPAN_SEMITONES - 1 <= 127);
pub const DEFAULT_VELOCITY: u8 = 100;
pub const VELOCITY_STEP: u8 = 10;

/// `key` が鍵盤のどの半音か (`None` = 音のキーではない)。
#[must_use]
pub fn semitone_for(key: PhysicalKey) -> Option<u8> {
    LOWER_ROW
        .iter()
        .chain(UPPER_ROW.iter())
        .find(|m| m.key == key)
        .map(|m| m.semitone)
}

/// key grab に宣言するキー全部 (音 + オクターブ / ベロシティ)。
pub fn grab_keys() -> impl Iterator<Item = PhysicalKey> {
    LOWER_ROW
        .iter()
        .chain(UPPER_ROW.iter())
        .map(|m| m.key)
        .chain([OCTAVE_DOWN_KEY, OCTAVE_UP_KEY])
}

/// 半音 `semitone` に対応するキーの印字 `(下段, 上段)`。 12..=16 は両段にある。
#[must_use]
pub fn key_labels(semitone: u8) -> (Option<&'static str>, Option<&'static str>) {
    let lower = LOWER_ROW.iter().find(|m| m.semitone == semitone).map(|m| key_label(m.key));
    let upper = UPPER_ROW.iter().find(|m| m.semitone == semitone).map(|m| key_label(m.key));
    (lower, upper)
}

/// 物理キーの印字 (US 配列の刻印)。
#[must_use]
pub fn key_label(key: PhysicalKey) -> &'static str {
    match key {
        PhysicalKey::Char(c) => match c {
            'A' => "A", 'B' => "B", 'C' => "C", 'D' => "D", 'E' => "E", 'F' => "F", 'G' => "G",
            'H' => "H", 'I' => "I", 'J' => "J", 'K' => "K", 'L' => "L", 'M' => "M", 'N' => "N",
            'O' => "O", 'P' => "P", 'Q' => "Q", 'R' => "R", 'S' => "S", 'T' => "T", 'U' => "U",
            'V' => "V", 'W' => "W", 'X' => "X", 'Y' => "Y", 'Z' => "Z", ',' => ",", '.' => ".",
            ';' => ";", '/' => "/", '[' => "[", ']' => "]", _ => "?",
        },
        PhysicalKey::Digit(d) => match d {
            0 => "0", 1 => "1", 2 => "2", 3 => "3", 4 => "4", 5 => "5", 6 => "6", 7 => "7",
            8 => "8", 9 => "9", _ => "?",
        },
        _ => "?",
    }
}

/// 基準ピッチを `delta_octaves` だけ動かした値 (C 揃えのまま `0..=MAX_BASE_PITCH` に収める)。
#[must_use]
pub fn shift_base_pitch(base: u8, delta_octaves: i8) -> u8 {
    let next = i16::from(base) + 12 * i16::from(delta_octaves);
    u8::try_from(next.clamp(0, i16::from(MAX_BASE_PITCH))).unwrap_or(0)
}

/// ベロシティを `delta` 段だけ動かした値 (`1..=127`)。
#[must_use]
pub fn step_velocity(velocity: u8, delta: i8) -> u8 {
    let next = i16::from(velocity) + i16::from(VELOCITY_STEP) * i16::from(delta);
    u8::try_from(next.clamp(1, 127)).unwrap_or(1)
}

/// ピッチ名 (ピアノロールと同じ C4 = 60 表記、 例 48 → `"C3"`)。
#[must_use]
pub fn pitch_name(pitch: u8) -> String {
    let name = crate::widgets::piano_roll::pitch_class_name_spelled(pitch % 12, false);
    let octave = i32::from(pitch / 12) - 1;
    format!("{name}{octave}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 「I と , もド」 (ユーザー指定): 上段 I = 下段 Z の 2 オクターブ上、 下段 , = 上段 Q。
    #[test]
    fn 両段の_c_が揃っている() {
        assert_eq!(semitone_for(PhysicalKey::Char('Z')), Some(0));
        assert_eq!(semitone_for(PhysicalKey::Char(',')), Some(12));
        assert_eq!(semitone_for(PhysicalKey::Char('Q')), Some(12));
        assert_eq!(semitone_for(PhysicalKey::Char('I')), Some(24));
        assert_eq!(semitone_for(PhysicalKey::Char('K')), None, "K は窓の開閉であって音ではない");
        assert_eq!(semitone_for(PhysicalKey::Char('A')), None);
    }

    #[test]
    fn 配列は半音が連続していて最高音が_span_に収まる() {
        let mut lower: Vec<u8> = LOWER_ROW.iter().map(|m| m.semitone).collect();
        lower.dedup();
        assert_eq!(lower, (0..17).collect::<Vec<_>>());
        let upper: Vec<u8> = UPPER_ROW.iter().map(|m| m.semitone).collect();
        assert_eq!(upper, (12..29).collect::<Vec<_>>());
        assert_eq!(SPAN_SEMITONES, 29);
    }

    #[test]
    fn 基準ピッチとベロシティは範囲内に畳まれる() {
        assert_eq!(shift_base_pitch(48, 1), 60);
        assert_eq!(shift_base_pitch(0, -1), 0);
        assert_eq!(shift_base_pitch(96, 1), 96);
        assert_eq!(step_velocity(100, 1), 110);
        assert_eq!(step_velocity(125, 1), 127);
        assert_eq!(step_velocity(5, -1), 1);
    }

    #[test]
    fn ピッチ名はピアノロールと同じ_c4_60_表記() {
        assert_eq!(pitch_name(48), "C3");
        assert_eq!(pitch_name(60), "C4");
        assert_eq!(pitch_name(61), "C#4");
    }
}
