//! r.md #158: トラックインスペクタの寸法 (SSoT、`docs/plan_rmd_158_inspector.md` §2.1)。
//!
//! 帯 / タイトル / 余白の寸法はここだけに置く。テストも同じ定数を読む (手で写さない)。

/// 左右の余白。
pub const PAD: f32 = 12.0;
/// タイトル (2 行 + 余白) の高さ。中身に関係なく一定 — 選択を変えても帯の起点が上下しない。
pub const TITLE_H: f32 = 52.0;
/// タイトル 1 行目 (選んでいるもの) の文字。
pub const TITLE_FONT: f32 = 16.0;
/// タイトル 2 行目 (種類 · トラック) の文字。
pub const TITLE_SUB_FONT: f32 = 11.0;
/// タイトル 1 行目の色見本の一辺。
pub const TITLE_SWATCH: f32 = 16.0;
/// タイトル 2 行目の色見本の一辺。
pub const TITLE_SUB_SWATCH: f32 = 10.0;

/// 帯の見出しの高さ。
pub const BAND_H: f32 = 24.0;
/// 帯と帯の間。
pub const BAND_GAP: f32 = 6.0;
/// 見出しから中身まで。
pub const BAND_BODY_TOP: f32 = 6.0;
/// 中身の下の余白。
pub const BAND_BODY_BOTTOM: f32 = 8.0;
/// 帯の名前の文字。
pub const HEAD_FONT: f32 = 12.0;
/// 帯の対象 (右寄せ、text_dim) の文字。
pub const TARGET_FONT: f32 = 11.0;
/// 開閉マークの文字。
pub const DISCLOSURE_FONT: f32 = 9.0;
/// 開閉マークの列の幅 (名前はこの右から)。
pub const DISCLOSURE_W: f32 = 14.0;

/// 帯の中の小見出し (「ローンチ」など、`native_panel/cell.rs` の「ROOM ――」と同じ形) の文字。
pub const SUB_HEAD_FONT: f32 = 10.0;
/// 小見出しの行の高さ。
pub const SUB_HEAD_H: f32 = 16.0;

/// 右端の幅のドラッグ帯 (scroll viewport の外の列)。
pub const RESIZE_HANDLE_W: f32 = 5.0;

/// 欄 (ラベル左・値右、`form.rs`) のラベル列の幅。欄の幅はこの 1 つだけ。一番長い欄名
/// (「フォルマント st」、11px で 85px 前後) が切れずに入る幅 (計画の 88 では「…」で切れた)。
pub const FORM_LABEL_W: f32 = 96.0;
/// ラベル列 / 値 / A 列の間。
pub const FORM_GAP: f32 = 4.0;
/// 自動化の「A」の列 (A が無い欄でもこの幅を空けて、値の右端を揃える)。
pub const FORM_AUTO_W: f32 = 22.0;
/// 欄の部品 (数値欄 / dropdown / トグル / 入力欄) の高さ。
pub const FORM_ROW_H: f32 = 22.0;
/// 行の送り。
pub const FORM_ROW_PITCH: f32 = 26.0;
/// 欄のラベル (text_dim)。
pub const LABEL_FONT: f32 = 11.0;
/// 値の下に添える補足 (text_dim) の文字と行の送り。
pub const HINT_FONT: f32 = 10.0;
pub const HINT_PITCH: f32 = 16.0;
