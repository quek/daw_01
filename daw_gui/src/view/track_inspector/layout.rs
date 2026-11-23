//! r.md #158: トラックインスペクタの寸法 (SSoT、`docs/plan_rmd_158_inspector.md` §2.1)。
//!
//! 帯 / タイトル / 余白 / 欄 / Rack の行の寸法はここだけに置く。テストも同じ定数を読む (手で写さない)。

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

// ---- Rack (chain list) の行と、行の直下に開く Par (r.md #158 仕様 9) ----

/// Rack の行 (plugin / 内蔵 / Parallel 開始 / chain) の見出しの高さ。行の背景と hover はこの高さだけ
/// (展開した Par / SC にはかけない)。
pub const RACK_ROW_H: f32 = 26.0;
/// 行の名前の左端 (行の中身の左端から)。行の直下に開く Par / SC / 出力の中身もここから始める。
pub const ROW_NAME_X: f32 = 8.0;
/// 行の名前の上端 (行の上端から)。
pub const ROW_NAME_TOP: f32 = 8.0;
/// 行の名前の文字。
pub const ROW_NAME_FONT: f32 = 11.0;
/// 行の直下に開く Par (plugin / 映像 FX / VOICEVOX / Transform) の中身の上下の余白。
pub const PAR_PAD: f32 = 6.0;
/// 読み込みに失敗した plugin の行の直下に出す理由の 1 行の高さ。
pub const FAIL_REASON_H: f32 = 18.0;
/// バイパス中の行の「オフ」の札 (`inset_bg` の窪み + 10px `text_dim`)。
pub const OFF_TAG_FONT: f32 = 10.0;
pub const OFF_TAG_H: f32 = 16.0;
pub const OFF_TAG_PAD_X: f32 = 8.0;

/// 行の直下に開く展開 (Par / SC / 出力) の行幅いっぱいの矩形 `full` から、中身を描く矩形を返す。左端は
/// 行の名前と同じ ([`ROW_NAME_X`])、右端は行の右端。余白の press は `full` で名乗る (行が動かない)。
#[must_use]
pub fn expansion_inset(full: daw_ui_renderer::Rect) -> daw_ui_renderer::Rect {
    daw_ui_renderer::Rect { x: full.x + ROW_NAME_X, w: (full.w - ROW_NAME_X).max(1.0), ..full }
}

/// 行の中身 `row_content` (行の上端 = 見出しの上端) の直下に開く Par の矩形 (他の展開が開いていないとき)。
/// 描画とテスト (座標を求める側) が同じ関数を通す。
#[must_use]
pub fn par_rect(row_content: daw_ui_renderer::Rect, panel_h: f32) -> daw_ui_renderer::Rect {
    expansion_inset(daw_ui_renderer::Rect { y: row_content.y + RACK_ROW_H, h: panel_h, ..row_content })
}

/// 欄 (ラベル左・値右) を `rows` 行積んだ展開 (SC / 出力のパネル) の高さ (上下の [`PAR_PAD`] 込み)。
#[must_use]
pub fn form_block_h(rows: usize) -> f32 {
    if rows == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = rows as f32;
    PAR_PAD * 2.0 + n * FORM_ROW_PITCH - (FORM_ROW_PITCH - FORM_ROW_H)
}
