//! コード名の読み書き (r.md #157、`docs/plan_chord_track.md`)。
//!
//! - [`format_chord`]: [`Chord`] → 表示名 (`Am7` / `C/E` / `G7(b9,13)`)。帯・ピアノロール・ポップアップ・
//!   今 / 次のコード表示が全部ここを通す (表記を経路ごとに写さない)。
//! - [`parse_chord`]: 文字入力 → [`Chord`]。`format_chord` の出力は必ず読み戻せる (往復で同じコード)。
//!
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use crate::model::{Chord, ChordKind, Spelling, Tensions};
use crate::scale::{ScaleChange, pitch_class_name_in_key};

/// コード名の表記。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChordNotation {
    /// 音名 (`Am7`)。
    #[default]
    Regular,
    /// キーからの度数を数字で (`6m7`、Nashville)。キーが無い位置では `Regular` に倒す。
    Number,
    /// キーからの度数をローマ数字で (`vi7`)。キーが無い位置では `Regular` に倒す。
    Roman,
}

impl ChordNotation {
    pub const ALL: &'static [ChordNotation] =
        &[ChordNotation::Regular, ChordNotation::Number, ChordNotation::Roman];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ChordNotation::Regular => "音名",
            ChordNotation::Number => "数字 (Nashville)",
            ChordNotation::Roman => "ローマ数字",
        }
    }
}

/// キーが分からないときの音名 (コード記号で普通に見る綴り: 黒鍵は `Db Eb F# Ab Bb`)。
const NAMES_DEFAULT: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];
const NAMES_SHARP: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
const NAMES_FLAT: [&str; 12] = ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"];

/// 度数 (キーのルートからの半音 → 数字 / ローマ数字の本体と前置の変化記号)。
const DEGREES: [(&str, &str, &str); 12] = [
    ("", "1", "I"),
    ("b", "2", "II"),
    ("", "2", "II"),
    ("b", "3", "III"),
    ("", "3", "III"),
    ("", "4", "IV"),
    ("#", "4", "IV"),
    ("", "5", "V"),
    ("b", "6", "VI"),
    ("", "6", "VI"),
    ("b", "7", "VII"),
    ("", "7", "VII"),
];

/// pitch class の音名。`spelling` が `Auto` なら `key` (その位置のキー) に従い、キーも無ければ既定の綴り。
#[must_use]
pub fn note_name(pc: u8, spelling: Spelling, key: Option<&ScaleChange>) -> &'static str {
    let i = usize::from(pc % 12);
    match (spelling, key) {
        (Spelling::Sharp, _) => NAMES_SHARP[i],
        (Spelling::Flat, _) => NAMES_FLAT[i],
        (Spelling::Auto, Some(k)) => pitch_class_name_in_key(pc, k.root, k.scale),
        (Spelling::Auto, None) => NAMES_DEFAULT[i],
    }
}

/// 文字ごとの綴り `[♭, ナチュラル, ♯]` (C D E F G A B の順) と、その文字の pitch class。
const LETTERS: [([&str; 3], u8); 7] = [
    (["Cb", "C", "C#"], 0),
    (["Db", "D", "D#"], 2),
    (["Eb", "E", "E#"], 4),
    (["Fb", "F", "F#"], 5),
    (["Gb", "G", "G#"], 7),
    (["Ab", "A", "A#"], 9),
    (["Bb", "B", "B#"], 11),
];

/// ベース音 (`C/E` の E) の音名。**ルートの綴りからの音程で綴る** (`A7/C#` を `A7/Db` と書かない、
/// `C7/Bb` を `C7/A#` と書かない)。重変化記号が要る綴りになるときは [`note_name`] に落とす。
fn bass_name(chord: &Chord, bass: u8, root_name: &str, key: Option<&ScaleChange>) -> &'static str {
    let semis = (bass % 12 + 12 - chord.root % 12) % 12;
    let flat5 = chord.tensions.contains(Tensions::FLAT5)
        || matches!(chord.kind, ChordKind::Diminished | ChordKind::Diminished7 | ChordKind::HalfDiminished7);
    let sharp5 = chord.tensions.contains(Tensions::SHARP5)
        || matches!(chord.kind, ChordKind::Augmented | ChordKind::Augmented7 | ChordKind::AugmentedMajor7);
    // ルートから何文字上か (短 2 度 = 1、長 3 度 = 2、…)。増 4 / 減 5、増 5 / 短 6 はコードの種類で決める。
    let steps = match semis {
        0 => 0,
        1 | 2 => 1,
        3 | 4 => 2,
        5 => 3,
        6 => 3 + usize::from(flat5),
        7 => 4,
        8 => 5 - usize::from(sharp5),
        9 => 5,
        _ => 6,
    };
    let fallback = note_name(bass, chord.spelling, key);
    let Some(root_letter) = LETTERS.iter().position(|(names, _)| root_name.starts_with(&names[1][..1])) else {
        return fallback;
    };
    let (names, letter_pc) = LETTERS[(root_letter + steps) % 7];
    match (bass % 12 + 12 - letter_pc) % 12 {
        11 => names[0],
        0 => names[1],
        1 => names[2],
        _ => fallback,
    }
}

/// 種類の接尾辞 (`m7` / `maj7` / `sus4`)。メジャーは空。
#[must_use]
pub const fn kind_suffix(kind: ChordKind) -> &'static str {
    match kind {
        ChordKind::Major => "",
        ChordKind::Minor => "m",
        ChordKind::Diminished => "dim",
        ChordKind::Augmented => "aug",
        ChordKind::Sus2 => "sus2",
        ChordKind::Sus4 => "sus4",
        ChordKind::Power => "5",
        ChordKind::Major6 => "6",
        ChordKind::Minor6 => "m6",
        ChordKind::Dominant7 => "7",
        ChordKind::Major7 => "maj7",
        ChordKind::Minor7 => "m7",
        ChordKind::MinorMajor7 => "mMaj7",
        ChordKind::Diminished7 => "dim7",
        ChordKind::HalfDiminished7 => "m7b5",
        ChordKind::Augmented7 => "aug7",
        ChordKind::AugmentedMajor7 => "augMaj7",
        ChordKind::Dominant7Sus4 => "7sus4",
    }
}

/// 3 和音 (6th / 7th を持たない) か — テンション 1 個を `add9` と書くかの判定。
const fn is_triad(kind: ChordKind) -> bool {
    matches!(
        kind,
        ChordKind::Major
            | ChordKind::Minor
            | ChordKind::Diminished
            | ChordKind::Augmented
            | ChordKind::Sus2
            | ChordKind::Sus4
            | ChordKind::Power
    )
}

/// テンションの表記 (`add9` / `(b9,13)`)。無ければ空。
fn tension_suffix(kind: ChordKind, tensions: Tensions) -> String {
    let names: Vec<&str> =
        Tensions::ALL.iter().filter(|(t, _, _)| tensions.contains(*t)).map(|(_, n, _)| *n).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] if is_triad(kind) && !matches!(*one, "b5" | "#5") => format!("add{one}"),
        many => format!("({})", many.join(",")),
    }
}

/// [`Chord`] の表示名。`key` はその位置のキー (綴りと度数表記に使う)。
#[must_use]
pub fn format_chord(chord: &Chord, notation: ChordNotation, key: Option<&ScaleChange>) -> String {
    let tensions = tension_suffix(chord.kind, chord.tensions);
    let bass = chord.bass.filter(|b| b % 12 != chord.root % 12);
    let (ChordNotation::Number | ChordNotation::Roman, Some(k)) = (notation, key) else {
        let root_name = note_name(chord.root, chord.spelling, key);
        let mut out = String::from(root_name);
        out.push_str(kind_suffix(chord.kind));
        out.push_str(&tensions);
        if let Some(b) = bass {
            out.push('/');
            out.push_str(bass_name(chord, b, root_name, key));
        }
        return out;
    };
    let degree = |pc: u8| DEGREES[usize::from((pc % 12 + 12 - k.root % 12) % 12)];
    let (acc, num, roman) = degree(chord.root);
    let mut out = String::from(acc);
    if notation == ChordNotation::Number {
        out.push_str(num);
        out.push_str(kind_suffix(chord.kind));
    } else {
        // ローマ数字は小文字が短 3 度を表すので、接尾辞の頭の `m` (minor の印) は落とす。
        let suffix = kind_suffix(chord.kind);
        if chord.kind.is_minor() {
            out.push_str(&roman.to_ascii_lowercase());
            out.push_str(suffix.strip_prefix('m').unwrap_or(suffix));
        } else {
            out.push_str(roman);
            out.push_str(suffix);
        }
    }
    out.push_str(&tensions);
    if let Some(b) = bass {
        let (acc, num, _) = degree(b);
        out.push('/');
        out.push_str(acc);
        out.push_str(num);
    }
    out
}

/// ASCII の大文字小文字を無視して接頭辞 `tok` を剥がす。
fn strip_ci<'a>(s: &'a str, tok: &str) -> Option<&'a str> {
    let head = s.get(..tok.len())?;
    head.eq_ignore_ascii_case(tok).then(|| &s[tok.len()..])
}

/// 先頭の音名 (`C` / `F#` / `Bb` / `E♭`) を読む。返すのは (pitch class、綴り、残り)。
fn parse_note(s: &str) -> Option<(u8, Spelling, &str)> {
    let mut chars = s.chars();
    let base = match chars.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let rest = chars.as_str();
    let mut acc_chars = rest.chars();
    let (delta, spelling, rest) = match acc_chars.next() {
        Some('#' | '♯') => (1, Spelling::Sharp, acc_chars.as_str()),
        Some('b' | '♭') => (11, Spelling::Flat, acc_chars.as_str()),
        _ => (0, Spelling::Auto, rest),
    };
    Some(((base + delta) % 12, spelling, rest))
}

/// 数字の拡張 (`9` / `11` / `13`) が含むテンション (`C9` = 7th + 9、`C11` = 7th + 9 + 11、`C13` = 7th + 9 + 13)。
fn extension(s: &str) -> Option<(Tensions, &str)> {
    if let Some(r) = s.strip_prefix("13") {
        Some((Tensions::NINE.with(Tensions::THIRTEEN), r))
    } else if let Some(r) = s.strip_prefix("11") {
        Some((Tensions::NINE.with(Tensions::ELEVEN), r))
    } else if let Some(r) = s.strip_prefix('9') {
        Some((Tensions::NINE, r))
    } else {
        s.strip_prefix('7').map(|r| (Tensions::NONE, r))
    }
}

/// 種類の部分を読む。返すのは (種類、数字の拡張が含むテンション、残り)。
fn parse_kind(s: &str) -> (ChordKind, Tensions, &str) {
    let none = Tensions::NONE;
    // maj 系 (`maj7` / `M7` / `Δ`)。`M` 単独は大文字だけ (小文字の `m` は minor)。
    let maj = strip_ci(s, "maj").or_else(|| strip_ci(s, "ma").filter(|r| r.starts_with(['7', '9', '1'])));
    let maj = maj.or_else(|| s.strip_prefix('M')).or_else(|| s.strip_prefix('Δ'));
    if let Some(r) = maj {
        return match extension(r) {
            Some((t, r)) => (ChordKind::Major7, t, r),
            None if s.starts_with('Δ') => (ChordKind::Major7, none, r),
            None => (ChordKind::Major, none, r),
        };
    }
    // minor 系 (`m` / `min` / `-`)。
    let min = strip_ci(s, "min").or_else(|| s.strip_prefix('m')).or_else(|| s.strip_prefix('-'));
    if let Some(r) = min {
        let mm = strip_ci(r, "maj7").or_else(|| r.strip_prefix("M7")).or_else(|| r.strip_prefix("Δ7"));
        if let Some(r) = mm {
            return (ChordKind::MinorMajor7, none, r);
        }
        if let Some(r) = r.strip_prefix("7b5").or_else(|| r.strip_prefix("7-5")) {
            return (ChordKind::HalfDiminished7, none, r);
        }
        if let Some(r) = r.strip_prefix('6') {
            return (ChordKind::Minor6, none, r);
        }
        return match extension(r) {
            Some((t, r)) => (ChordKind::Minor7, t, r),
            None => (ChordKind::Minor, none, r),
        };
    }
    if let Some(r) = strip_ci(s, "dim7").or_else(|| s.strip_prefix("°7")).or_else(|| s.strip_prefix("o7")) {
        return (ChordKind::Diminished7, none, r);
    }
    if let Some(r) = strip_ci(s, "dim").or_else(|| s.strip_prefix('°')).or_else(|| s.strip_prefix('o')) {
        return (ChordKind::Diminished, none, r);
    }
    if let Some(r) = s.strip_prefix("ø7").or_else(|| s.strip_prefix('ø')) {
        return (ChordKind::HalfDiminished7, none, r);
    }
    if let Some(r) = strip_ci(s, "aug").or_else(|| s.strip_prefix('+')) {
        let mm = strip_ci(r, "maj7").or_else(|| r.strip_prefix("M7"));
        if let Some(r) = mm {
            return (ChordKind::AugmentedMajor7, none, r);
        }
        return match r.strip_prefix('7') {
            Some(r) => (ChordKind::Augmented7, none, r),
            None => (ChordKind::Augmented, none, r),
        };
    }
    if let Some(r) = strip_ci(s, "7sus4").or_else(|| strip_ci(s, "7sus")) {
        return (ChordKind::Dominant7Sus4, none, r);
    }
    if let Some(r) = strip_ci(s, "sus2") {
        return (ChordKind::Sus2, none, r);
    }
    if let Some(r) = strip_ci(s, "sus4").or_else(|| strip_ci(s, "sus")) {
        return (ChordKind::Sus4, none, r);
    }
    if let Some(r) = s.strip_prefix('5') {
        return (ChordKind::Power, none, r);
    }
    if let Some(r) = s.strip_prefix('6') {
        return (ChordKind::Major6, none, r);
    }
    match extension(s) {
        Some((t, r)) => (ChordKind::Dominant7, t, r),
        None => (ChordKind::Major, none, s),
    }
}

/// 種類の後ろに続くテンション / 変化音 (`b9` / `add9` / `#11` / `b5` / `sus4`) を読む。読み残しがあれば `None`。
fn parse_tensions(mut s: &str, mut kind: ChordKind, mut tensions: Tensions) -> Option<(ChordKind, Tensions)> {
    // 長い表記から先に当てる (`#11` を `#1` + `1` と読まない、`13` を `1` + `3` と読まない)。
    const TOKENS: &[(&str, Tensions)] = &[
        ("b13", Tensions::FLAT13),
        ("-13", Tensions::FLAT13),
        ("#11", Tensions::SHARP11),
        ("+11", Tensions::SHARP11),
        ("13", Tensions::THIRTEEN),
        ("11", Tensions::ELEVEN),
        ("b9", Tensions::FLAT9),
        ("-9", Tensions::FLAT9),
        ("#9", Tensions::SHARP9),
        ("+9", Tensions::SHARP9),
        ("b5", Tensions::FLAT5),
        ("-5", Tensions::FLAT5),
        ("#5", Tensions::SHARP5),
        ("+5", Tensions::SHARP5),
        ("9", Tensions::NINE),
    ];
    'outer: while !s.is_empty() {
        if let Some(r) = s.strip_prefix('(').or_else(|| strip_ci(s, "add")) {
            s = r;
            continue;
        }
        if let Some(r) = strip_ci(s, "sus4").or_else(|| strip_ci(s, "sus")) {
            kind = match kind {
                ChordKind::Major => ChordKind::Sus4,
                ChordKind::Dominant7 => ChordKind::Dominant7Sus4,
                _ => return None,
            };
            s = r;
            continue;
        }
        for (tok, t) in TOKENS {
            if let Some(r) = s.strip_prefix(tok) {
                tensions = tensions.with(*t);
                s = r;
                continue 'outer;
            }
        }
        return None;
    }
    Some((kind, tensions))
}

/// 文字入力をコードとして読む (`Am7` / `C/E` / `F#m7b5` / `G7(b9,13)` / `Cadd9` / `Bbmaj7`)。
/// 空白・閉じ括弧・カンマは無視する。読めなければ `None`。
///
/// **開き括弧は残す** — `C(b5)` の `(` を消すと `Cb5` (C♭ のパワーコード) と読んでしまう。
/// 括弧はルートの直後の変化記号とテンションを分ける区切りとして [`parse_tensions`] が読み飛ばす。
#[must_use]
pub fn parse_chord(text: &str) -> Option<Chord> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace() && !matches!(c, ')' | ',')).collect();
    let (root, spelling, rest) = parse_note(&cleaned)?;
    let (body, bass) = match rest.rsplit_once('/') {
        Some((body, bass_text)) => {
            let (bass, _, tail) = parse_note(bass_text)?;
            if !tail.is_empty() {
                return None;
            }
            (body, Some(bass))
        }
        None => (rest, None),
    };
    let (kind, ext, tail) = parse_kind(body);
    let (kind, tensions) = parse_tensions(tail, kind, ext)?;
    Some(Chord { root, kind, tensions, bass, spelling }.sanitized())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全種類 × テンションの代表 × ベースで、表示名が同じコードへ読み戻る。
    #[test]
    fn formatted_names_parse_back_to_the_same_chord() {
        let tension_sets = [
            Tensions::NONE,
            Tensions::NINE,
            Tensions::FLAT9.with(Tensions::THIRTEEN),
            Tensions::SHARP11,
            Tensions::FLAT5,
            Tensions::SHARP5.with(Tensions::SHARP9),
        ];
        for root in 0..12u8 {
            for &kind in ChordKind::ALL {
                for tensions in tension_sets {
                    for bass in [None, Some((root + 4) % 12)] {
                        let chord = Chord { root, kind, tensions, bass, spelling: Spelling::Auto };
                        let name = format_chord(&chord, ChordNotation::Regular, None);
                        let back = parse_chord(&name).unwrap_or_else(|| panic!("{name} が読めない"));
                        assert_eq!(
                            (back.root, back.kind, back.tensions, back.bass),
                            (chord.root, chord.kind, chord.tensions, chord.bass),
                            "{name}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn common_spellings_are_read() {
        let cases: &[(&str, u8, ChordKind, Tensions, Option<u8>)] = &[
            ("C", 0, ChordKind::Major, Tensions::NONE, None),
            ("am", 9, ChordKind::Minor, Tensions::NONE, None),
            ("Am7", 9, ChordKind::Minor7, Tensions::NONE, None),
            ("A-7", 9, ChordKind::Minor7, Tensions::NONE, None),
            ("CM7", 0, ChordKind::Major7, Tensions::NONE, None),
            ("Bbmaj9", 10, ChordKind::Major7, Tensions::NINE, None),
            ("F#m7b5", 6, ChordKind::HalfDiminished7, Tensions::NONE, None),
            ("G7(b9,13)", 7, ChordKind::Dominant7, Tensions::FLAT9.with(Tensions::THIRTEEN), None),
            ("G13", 7, ChordKind::Dominant7, Tensions::NINE.with(Tensions::THIRTEEN), None),
            ("Cadd9", 0, ChordKind::Major, Tensions::NINE, None),
            ("C/E", 0, ChordKind::Major, Tensions::NONE, Some(4)),
            ("D7sus4", 2, ChordKind::Dominant7Sus4, Tensions::NONE, None),
            ("Dsus", 2, ChordKind::Sus4, Tensions::NONE, None),
            ("E+", 4, ChordKind::Augmented, Tensions::NONE, None),
            ("Bdim7", 11, ChordKind::Diminished7, Tensions::NONE, None),
            ("C7#5", 0, ChordKind::Dominant7, Tensions::SHARP5, None),
            ("E♭m", 3, ChordKind::Minor, Tensions::NONE, None),
        ];
        for (text, root, kind, tensions, bass) in cases {
            let c = parse_chord(text).unwrap_or_else(|| panic!("{text} が読めない"));
            assert_eq!((c.root, c.kind, c.tensions, c.bass), (*root, *kind, *tensions, *bass), "{text}");
        }
        for bad in ["", "H", "Cxyz", "C/", "C/X", "7"] {
            assert!(parse_chord(bad).is_none(), "{bad} は読めないはず");
        }
    }

    /// ベース音はルートからの音程で綴る (黒鍵の既定の綴りに落とすと `A7/Db` になる)。
    #[test]
    fn the_bass_is_spelled_from_the_root() {
        for name in ["A7/C#", "C7/Bb", "Cm/Eb", "Db/F", "F#/A#", "Cdim/Gb", "Caug/G#", "E/G#", "Bb/D", "Ebm/Gb"] {
            let c = parse_chord(name).unwrap_or_else(|| panic!("{name} が読めない"));
            assert_eq!(format_chord(&c, ChordNotation::Regular, None), name);
        }
    }

    #[test]
    fn degree_notation_follows_the_key() {
        let key = ScaleChange { beat: 0.0, root: 0, scale: crate::scale::Scale::Major };
        let am7 = Chord::new(9, ChordKind::Minor7);
        assert_eq!(format_chord(&am7, ChordNotation::Number, Some(&key)), "6m7");
        assert_eq!(format_chord(&am7, ChordNotation::Roman, Some(&key)), "vi7");
        let bb = Chord::new(10, ChordKind::Major);
        assert_eq!(format_chord(&bb, ChordNotation::Roman, Some(&key)), "bVII");
        // キーが無い位置では音名に倒す。
        assert_eq!(format_chord(&am7, ChordNotation::Roman, None), "Am7");
    }
}
