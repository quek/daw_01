//! r.md #158: トラックインスペクタの見出し・欄名の文言 (SSoT、`docs/plan_rmd_158_inspector.md` §2.4 / 仕様 11)。
//!
//! 見出しと欄名は日本語。Par / SC / Mix / dB / A のような短い記号・単位と、呼び名「Rack」は残す。
//! テストは描かれた glyph をこの定数で探す (文言を変えてもテストが壊れない)。

use crate::state::inspector_view::ClipBandKind;

/// タイトル: トラックが 1 本も無い。
pub const TITLE_NO_TRACK: &str = "トラックなし";

/// 帯「入力」(入力 / モニター)。
pub const BAND_INPUT: &str = "入力";
/// 帯「Rack」(デバイスの列)。呼び名はユーザーが使う「Rack」のまま。
pub const BAND_RACK: &str = "Rack";
/// 帯「口パク」(口形状 → 画像)。
pub const BAND_MOUTH: &str = "口パク";
/// 帯「モジュレーション」。
pub const BAND_MODULATION: &str = "モジュレーション";
/// 帯「列」(選んでいる列のフォローアクション)。
pub const BAND_SCENE: &str = "列";

/// 選択の帯の中の小見出し「ローンチ」(選んでいるセルのローンチ設定)。
pub const SUB_LAUNCH: &str = "ローンチ";

/// 対象の表示に添える「無効なトラック」(r.md #131: チェーンは host に居ない)。
pub const TARGET_DISABLED: &str = "(無効中)";

/// クリップの帯の見出し (クリップの種類)。
#[must_use]
pub fn clip_band_title(kind: ClipBandKind) -> &'static str {
    match kind {
        ClipBandKind::Audio => "音声クリップ",
        ClipBandKind::Midi => "MIDI クリップ",
        ClipBandKind::Image => "画像クリップ",
        ClipBandKind::Video => "映像クリップ",
        ClipBandKind::Text => "テキストクリップ",
        ClipBandKind::Automation => "オートメーションクリップ",
    }
}

/// 帯の対象: 複数のトラックに効く帯 (入力)。
#[must_use]
pub fn target_selected_tracks(n: usize) -> String {
    format!("選択中の {n} トラック")
}

/// 帯の対象: 複数選択のうちカーソルのトラック 1 本だけに効く帯 (Rack / 口パク / モジュレーション)。
#[must_use]
pub fn target_cursor_only(name: &str) -> String {
    format!("{name} のみ")
}

/// タイトル / 帯の対象: 複数のクリップ。
#[must_use]
pub fn n_clips(n: usize) -> String {
    format!("{n} クリップ")
}

/// タイトル: 複数のトラック。
#[must_use]
pub fn n_tracks(n: usize) -> String {
    format!("{n} トラック")
}

/// タイトル: 列を 1 本選んでいる。
#[must_use]
pub fn scene_title(name: &str) -> String {
    format!("列「{name}」")
}

/// タイトル / 帯の対象: 複数の列。
#[must_use]
pub fn n_scenes(n: usize) -> String {
    format!("{n} 列")
}
