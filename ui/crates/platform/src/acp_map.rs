//! UTF-16 ACP (application character position) ⇔ UTF-8 byte offset の純粋マッピング。
//!
//! Windows TSF (`ITextStoreACP`) は文字位置を **UTF-16 code-unit offset** (= ACP) で扱うが、
//! gui_01 の widget は **UTF-8 byte offset** で編集する。両者を相互変換するのがこの module。
//! COM / Windows API に一切依存しない純粋ロジックなので、全プラットフォームでコンパイル・
//! `cargo test` できる (= TSF 連携の最高リスク算術をここで隔離して単体検証する)。
//!
//! サロゲートペア (astral 文字、例 U+1F600 = UTF-16 2 unit / UTF-8 4 byte) の扱いが要点:
//! - 1 文字を構成する全 UTF-16 unit は、その文字の **先頭 byte** に写す (mid-surrogate ACP は
//!   char 先頭へ丸める → UTF-8 を必ず char 境界で slice できる)。
//! - `byte_to_acp` は char 境界の byte を前提に、その char を開始する UTF-16 unit を返す。

/// UTF-16 ACP ⇔ UTF-8 byte の索引表。`build` で 1 つのテキストから構築する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpMap {
    /// `utf8_of_acp[u]` = UTF-16 unit `u` が属する char の **先頭 UTF-8 byte offset**。
    /// 長さは `(UTF-16 unit 数) + 1`。末尾 (`[len16]`) は total byte 数 (sentinel)。
    /// 非減少列。**常に sentinel を 1 つ持つ不変条件** (空テキストでも `[0]`)。
    utf8_of_acp: Vec<u32>,
}

impl Default for AcpMap {
    /// 空テキストの索引表 (`build("")` と同値、sentinel `[0]`)。
    /// `#[derive(Default)]` だと `Vec` が空になり sentinel 不変条件を壊して `len16` が underflow する
    /// (`DocState::new()` が踏んだ実バグ、 2026-06-01 実機起動で発覚)。
    fn default() -> Self {
        Self { utf8_of_acp: vec![0] }
    }
}

impl AcpMap {
    /// UTF-8 文字列から索引表を構築する。
    #[must_use]
    pub fn build(text: &str) -> Self {
        // capacity: 大半が BMP なので chars 数 + 1 で概ね足りる (astral があれば伸びる)。
        let mut utf8_of_acp = Vec::with_capacity(text.len() + 1);
        let mut byte: u32 = 0;
        for ch in text.chars() {
            let start = byte;
            // この char を構成する全 UTF-16 unit (1 or 2) を char 先頭 byte に写す。
            for _ in 0..ch.len_utf16() {
                utf8_of_acp.push(start);
            }
            byte += ch.len_utf8() as u32;
        }
        utf8_of_acp.push(byte); // sentinel = total bytes
        Self { utf8_of_acp }
    }

    /// UTF-16 unit 数 (= 最大 ACP)。
    #[must_use]
    pub fn len16(&self) -> usize {
        // build / Default で必ず sentinel を 1 つ持つ (len >= 1)。万一空でも saturating で防御。
        self.utf8_of_acp.len().saturating_sub(1)
    }

    /// UTF-8 byte 長 (= sentinel 値)。
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        *self.utf8_of_acp.last().unwrap_or(&0) as usize
    }

    /// ACP (UTF-16 unit offset) → UTF-8 byte offset。
    ///
    /// `acp` は `[0, len16]` に clamp する。mid-surrogate の ACP はその char の先頭 byte を返す
    /// (= 返り値は必ず UTF-8 char 境界)。
    #[must_use]
    pub fn acp_to_byte(&self, acp: usize) -> usize {
        let idx = acp.min(self.len16());
        self.utf8_of_acp[idx] as usize
    }

    /// UTF-8 byte offset → ACP (UTF-16 unit offset)。
    ///
    /// `byte` は char 境界である前提 (widget は `prev/next_char_boundary` で境界を保証する)。
    /// 非境界を渡した場合は「その byte 以上で最小の char 先頭」の unit に丸める。
    #[must_use]
    pub fn byte_to_acp(&self, byte: usize) -> usize {
        let byte = byte.min(self.len_bytes());
        // 非減少列で「値 >= byte となる最小 index」= その byte を開始 byte に持つ char の先頭 unit。
        self.utf8_of_acp
            .partition_point(|&b| (b as usize) < byte)
    }
}

#[cfg(test)]
mod tests {
    use super::AcpMap;

    /// ASCII: ACP と byte が 1:1。
    #[test]
    fn ascii_is_identity() {
        let m = AcpMap::build("abc");
        assert_eq!(m.len16(), 3);
        assert_eq!(m.len_bytes(), 3);
        for i in 0..=3 {
            assert_eq!(m.acp_to_byte(i), i, "acp_to_byte({i})");
            assert_eq!(m.byte_to_acp(i), i, "byte_to_acp({i})");
        }
    }

    /// BMP CJK: 1 char = UTF-16 1 unit / UTF-8 3 byte。
    #[test]
    fn bmp_cjk_three_byte_one_unit() {
        let m = AcpMap::build("あいう"); // 各 3 byte / 1 unit
        assert_eq!(m.len16(), 3);
        assert_eq!(m.len_bytes(), 9);
        assert_eq!(m.acp_to_byte(0), 0);
        assert_eq!(m.acp_to_byte(1), 3);
        assert_eq!(m.acp_to_byte(2), 6);
        assert_eq!(m.acp_to_byte(3), 9);
        assert_eq!(m.byte_to_acp(0), 0);
        assert_eq!(m.byte_to_acp(3), 1);
        assert_eq!(m.byte_to_acp(6), 2);
        assert_eq!(m.byte_to_acp(9), 3);
    }

    /// astral (emoji U+1F600): UTF-16 2 unit (surrogate pair) / UTF-8 4 byte。
    #[test]
    fn astral_surrogate_pair() {
        let m = AcpMap::build("😀"); // 1 char = 2 unit / 4 byte
        assert_eq!(m.len16(), 2);
        assert_eq!(m.len_bytes(), 4);
        // unit 0 = char 先頭、unit 1 = mid-surrogate (char 先頭へ丸め)、unit 2 = 末尾。
        assert_eq!(m.acp_to_byte(0), 0);
        assert_eq!(m.acp_to_byte(1), 0, "mid-surrogate は char 先頭 byte へ丸める");
        assert_eq!(m.acp_to_byte(2), 4);
        // byte 0 → unit 0、byte 4 (char 境界) → unit 2。
        assert_eq!(m.byte_to_acp(0), 0);
        assert_eq!(m.byte_to_acp(4), 2);
    }

    /// 混在: ASCII + CJK + emoji。ShiftStart(-N) を ACP 上で模擬し、pre-cursor substring が
    /// 正しく取れることを確認 (rtry まぜ書きの読み取り経路の核)。
    #[test]
    fn mixed_shift_start_simulation() {
        let text = "aあ😀b"; // a(1u/1b) あ(1u/3b) 😀(2u/4b) b(1u/1b)
        let m = AcpMap::build(text);
        assert_eq!(m.len16(), 1 + 1 + 2 + 1);
        assert_eq!(m.len_bytes(), 1 + 3 + 4 + 1);

        // cursor が末尾 (acp = len16) のとき ShiftStart(-3): acp_end - 3 を clamp(0)。
        let cursor_acp = m.len16(); // 5
        let start_acp = cursor_acp.saturating_sub(3); // 2 → "😀b" 直前 = あ の後
        let lo = m.acp_to_byte(start_acp);
        let hi = m.acp_to_byte(cursor_acp);
        assert_eq!(&text[lo..hi], "😀b", "ShiftStart(-3)+GetText の pre-cursor 部分");

        // ShiftStart(-10) は 0 に clamp → 全文。
        let lo_all = m.acp_to_byte(cursor_acp.saturating_sub(10));
        assert_eq!(&text[lo_all..hi], text);
    }

    /// selection の往復 (byte ↔ acp 双方向)。
    #[test]
    fn selection_round_trip() {
        let text = "xあy😀z";
        let m = AcpMap::build(text);
        for (byte, _) in text.char_indices().chain([(text.len(), '\0')]) {
            let acp = m.byte_to_acp(byte);
            assert_eq!(m.acp_to_byte(acp), byte, "round-trip byte={byte}");
        }
    }

    /// 範囲外 ACP / byte は clamp される。
    #[test]
    fn out_of_range_clamps() {
        let m = AcpMap::build("ab");
        assert_eq!(m.acp_to_byte(999), 2);
        assert_eq!(m.byte_to_acp(999), 2);
        let empty = AcpMap::build("");
        assert_eq!(empty.len16(), 0);
        assert_eq!(empty.len_bytes(), 0);
        assert_eq!(empty.acp_to_byte(0), 0);
        assert_eq!(empty.acp_to_byte(5), 0);
        assert_eq!(empty.byte_to_acp(5), 0);
    }

    /// `Default` は `build("")` と同値の有効な空マップ (sentinel `[0]`)。
    /// `len16` が underflow しないことの回帰テスト (2026-06-01 実機起動 panic)。
    #[test]
    fn default_is_valid_empty_map() {
        let m = AcpMap::default();
        assert_eq!(m, AcpMap::build(""));
        assert_eq!(m.len16(), 0, "underflow しない");
        assert_eq!(m.len_bytes(), 0);
        assert_eq!(m.acp_to_byte(0), 0);
        assert_eq!(m.byte_to_acp(0), 0);
    }
}
