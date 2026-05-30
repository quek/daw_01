//! プラグインピッカー等の絞り込み用ファジー検索。
//!
//! 連続部分文字列ではなく **subsequence (部分列) マッチ**。 入力文字が対象
//! 文字列に「順序を保って飛び飛びで」出現すれば一致とみなす。 これにより
//! "murv" で "MTurboReverb" (M-Tu**r**bo-Re**v**erb) のような略記検索ができる。

/// `needle` が `haystack` の case-insensitive な subsequence か判定する。
///
/// `needle` の各文字が `haystack` 内に順序を保ったまま (連続でなくてよい)
/// 出現すれば `true`。 空の `needle` は常に `true` (絞り込みなし扱い)。
///
/// # 例
/// ```
/// use daw_gui::fuzzy::subsequence_match;
/// assert!(subsequence_match("MTurboReverb", "murv"));
/// assert!(subsequence_match("MTurboReverb", "reverb"));
/// assert!(!subsequence_match("MTurboReverb", "xyz"));
/// ```
pub fn subsequence_match(haystack: &str, needle: &str) -> bool {
    // haystack を 1 度だけ前進走査する iterator。 needle の各文字を順に探し、
    // 見つかるたびに haystack の読み取り位置を進める (by_ref で消費)。
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    'needle: for nc in needle.chars().flat_map(char::to_lowercase) {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'needle;
            }
        }
        // needle の文字 nc を haystack の残りで見つけられなかった。
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::subsequence_match;

    #[test]
    fn matches_abbreviation_across_words() {
        // 本機能の要件: "murv" で "MTurboReverb" がヒットする。
        // m -> M, u -> tUrbo, r -> tuRbo, v -> reVerb の subsequence。
        assert!(subsequence_match("MTurboReverb", "murv"));
    }

    #[test]
    fn parametric_cases() {
        let cases: &[(&str, &str, bool)] = &[
            ("MTurboReverb", "murv", true),            // 要件の核
            ("MTurboReverb", "MTurboReverb", true),    // 完全一致
            ("MTurboReverb", "", true),                // 空 needle は全マッチ
            ("MTurboReverb", "reverb", true),          // 連続部分列 (case-insensitive)
            ("MTurboReverb", "MTURBO", true),          // 大文字入力でも一致
            ("MTurboReverb", "xyz", false),            // 含まれない文字
            ("MTurboReverb", "mturboz", false),        // 末尾 z が存在しない
            ("MTurboReverb", "vbr", false),            // 順序不一致 (v の後に b,r は来ない)
            ("Pro-Q 3", "proq", true),                 // 区切り '-' を飛ばす
            ("Pro-Q 3", "q3", true),                   // 空白を飛ばす
            ("MTurboReverb", "m r", false),            // needle 内の空白は haystack に無く不一致
        ];
        for &(hay, needle, expected) in cases {
            assert_eq!(
                subsequence_match(hay, needle),
                expected,
                "subsequence_match({hay:?}, {needle:?})",
            );
        }
    }
}
