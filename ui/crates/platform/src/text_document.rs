//! OS の text store (Windows TSF) と widget の間でやり取りする **中立データ型**。
//!
//! - [`TextDocument`]: focus 中の編集可能テキストの 1 フレーム snapshot。UI → OS へ publish。
//! - [`ImeTextEdit`]: OS IME (まぜ書き / 再変換 / composition) が返す編集。OS → UI へ drain。
//!
//! `renderer::Rect` は crate 依存方向 (`ui → platform`、`platform` は `renderer` を知らない) の
//! 都合で使えないため、座標は platform-local な [`RectPx`] (物理ピクセル) で持つ。

/// 物理ピクセル単位の矩形 (text store の caret / field 範囲表現用)。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RectPx {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// focus 中の編集可能テキストの 1 フレーム snapshot。
///
/// `WindowBackend::set_text_input_document(Some(doc))` で毎フレーム OS text store に publish し、
/// IME (rtry / MS-IME) が `GetText` / `GetSelection` / `GetTextExt` で読み取る。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextDocument {
    /// 編集対象の全文 (単一行 widget では 1 論理行)。任意 ACP range の `GetText` はこの部分文字列。
    pub text: String,
    /// `(anchor_byte, cursor_byte)` — UTF-8 byte offset。`anchor == cursor` で caret 単独 (選択なし)。
    /// **正規化しない** (IME が caret をどちら端に描くか決められるよう anchor/cursor の前後を保つ)。
    pub selection: (usize, usize),
    /// IME 候補ウィンドウ配置用の caret rect (物理 px、`request_ime` / `set_ime_cursor_area` と同座標系)。
    pub caret_rect: RectPx,
    /// 各文字境界の `(x, byte_offset)` (x は `caret_rect.x` と同座標系 = client 物理 px)。
    /// 文字 `i` は `[char_boundaries[i].0, char_boundaries[i+1].0)` を占める。先頭 = テキスト左端
    /// (byte 0)、末尾 = テキスト右端 (byte len)。`GetACPFromPoint` の逆 hit-test (点→ACP) に使う
    /// (E1 / r.md #8: MS-IME マウス再変換)。空 = layout 無し → store は `TS_E_NOLAYOUT` を返す。
    pub char_boundaries: Vec<(f32, usize)>,
}

/// OS IME (TSF) → widget へ返す編集。byte offset は **直近 publish した [`TextDocument::text`]** に対する。
///
/// まぜ書き / 再変換は selection 以外の range も書き換えるため、commit 専用ではなく汎用 range 置換を表す。
#[derive(Debug, Clone, PartialEq)]
pub enum ImeTextEdit {
    /// `[start_byte, end_byte)` を `text` で置換し、cursor を `new_cursor` (置換後テキスト基準の
    /// byte offset) へ collapse する。
    Replace {
        start_byte: usize,
        end_byte: usize,
        text: String,
        new_cursor: usize,
    },
    /// テキストは変えず selection のみ変更 (`SetSelection`)。
    SetSelection { anchor_byte: usize, cursor_byte: usize },
}
