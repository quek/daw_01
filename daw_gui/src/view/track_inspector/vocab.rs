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

// ---- クリップの帯の欄 (r.md #158 仕様 3 / 11) ----

/// 欄「名前」(テキストクリップ以外。テキストクリップの名前は本文そのもの)。
pub const FIELD_NAME: &str = "名前";
/// 欄「再生」(ミュート / 逆再生)。
pub const FIELD_PLAYBACK: &str = "再生";
pub const TOGGLE_MUTE: &str = "ミュート";
pub const TOGGLE_REVERSE: &str = "逆再生";
/// 音声クリップ。
pub const FIELD_STRETCH: &str = "伸縮";
pub const FIELD_GAIN: &str = "ゲイン dB";
pub const FIELD_PAN: &str = "パン";
pub const FIELD_PITCH: &str = "ピッチ st";
pub const FIELD_FORMANT: &str = "フォルマント st";
pub const FIELD_FADE_IN: &str = "フェードイン";
pub const FIELD_FADE_OUT: &str = "フェードアウト";
/// 画像クリップ / 字幕。
pub const FIELD_OPACITY: &str = "不透明度";
pub const FIELD_ROTATION: &str = "回転 °";
pub const FIELD_FLIP: &str = "反転";
pub const TOGGLE_FLIP_H: &str = "左右";
pub const TOGGLE_FLIP_V: &str = "上下";
/// 歌声 (VOICEVOX の歌唱クリップの声)。
pub const SUB_VOICE: &str = "歌声";
/// 読み上げ (VOICEVOX の Text クリップ)。
pub const SUB_TALK: &str = "読み上げ";
/// 字幕 (字幕デバイスが映す Text クリップの見た目)。
pub const SUB_SUBTITLE: &str = "字幕";
/// テキストクリップの本文: 読み上げがあるトラックでは「セリフ」、無ければ「文字」。
pub const FIELD_LINE: &str = "セリフ";
pub const FIELD_TEXT: &str = "文字";
pub const FIELD_CHARACTER: &str = "キャラ";
pub const FIELD_STYLE: &str = "スタイル";
pub const REFETCH_VOICES: &str = "声の一覧を取り直す";
pub const FIELD_READ_START: &str = "読み始め";
pub const TOGGLE_READ_HERE: &str = "ここから読む";
pub const ADD_SUBTITLE_DEVICE: &str = "+ 字幕デバイス (画面に出す)";
pub const FIELD_FONT: &str = "フォント";
pub const FIELD_ALIGN: &str = "揃え";
pub const FIELD_FONT_SIZE: &str = "大きさ px";
/// 字幕の詳細 (最初は畳む)。
pub const DETAIL_SUBTITLE_STYLE: &str = "字幕の細かい設定";
pub const SUB_FILL: &str = "塗り";
pub const SUB_OUTLINE: &str = "縁";
pub const SUB_SHADOW: &str = "影";
pub const FIELD_OUTLINE_W: &str = "太さ px";
pub const FIELD_OFFSET_X: &str = "X px";
pub const FIELD_OFFSET_Y: &str = "Y px";
pub const FIELD_BLUR: &str = "ぼかし px";

// ---- 入力の帯 ----

/// 欄「入力元」(帯の名前「入力」と同じ語を並べない)。
pub const FIELD_INPUT_SOURCE: &str = "入力元";
pub const FIELD_MONITOR: &str = "モニター";

// ---- Par の欄 ----

/// VOICEVOX の Par の欄「口パク先」(口形状の画像を焼き込むトラック)。
pub const FIELD_LIPSYNC_TARGET: &str = "口パク先";

/// ラベル列 (`layout::FORM_LABEL_W`) に入る欄名の全部。テストが全部を測り、ラベル列に収まることを確かめる。
pub const FORM_LABELS: &[&str] = &[
    FIELD_NAME,
    FIELD_PLAYBACK,
    FIELD_STRETCH,
    FIELD_GAIN,
    FIELD_PAN,
    FIELD_PITCH,
    FIELD_FORMANT,
    FIELD_FADE_IN,
    FIELD_FADE_OUT,
    FIELD_OPACITY,
    FIELD_ROTATION,
    FIELD_FLIP,
    FIELD_LINE,
    FIELD_TEXT,
    FIELD_CHARACTER,
    FIELD_STYLE,
    FIELD_READ_START,
    SUB_SUBTITLE,
    FIELD_FONT,
    FIELD_ALIGN,
    FIELD_FONT_SIZE,
    FIELD_OUTLINE_W,
    FIELD_OFFSET_X,
    FIELD_OFFSET_Y,
    FIELD_BLUR,
    FIELD_INPUT_SOURCE,
    FIELD_MONITOR,
    FIELD_LIPSYNC_TARGET,
    FIELD_LAUNCH_QUANTIZE,
    FIELD_LAUNCH_MODE,
    FIELD_LAUNCH_LENGTH,
    FIELD_LAUNCH_BEHAVIOR,
    FIELD_TALK_SPEED,
    FIELD_TALK_PITCH,
    FIELD_TALK_INTONATION,
    FIELD_TALK_VOLUME,
];

// ---- ローンチ / 読み上げのスケール ----

pub const FIELD_LAUNCH_QUANTIZE: &str = "量子化";
pub const FIELD_LAUNCH_MODE: &str = "モード";
pub const FIELD_LAUNCH_LENGTH: &str = "長さ (拍)";
/// セルの [ループ] [レガート] (同じ帯の先頭の「再生」[ミュート] と同じ語を並べない)。
pub const FIELD_LAUNCH_BEHAVIOR: &str = "動作";
pub const FIELD_TALK_SPEED: &str = "話速";
pub const FIELD_TALK_PITCH: &str = "音高";
pub const FIELD_TALK_INTONATION: &str = "抑揚";
pub const FIELD_TALK_VOLUME: &str = "音量";

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
