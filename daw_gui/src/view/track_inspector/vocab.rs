//! r.md #158: トラックインスペクタの見出し・欄名の文言 (SSoT、`docs/plan_rmd_158_inspector.md` §2.4 / 仕様 11)。
//!
//! 見出しと欄名は日本語。Par / SC / Mix / dB / A のような短い記号・単位と、呼び名「Rack」は残す。
//! テストは描かれた glyph をこの定数で探す (文言を変えてもテストが壊れない)。
//! dropdown の選択肢の値・空のときの表示・ヒントはここに置かず、値の持ち主 (型の `label` / 選択肢を作る handler /
//! 部品のファイル) が持つ (入力元の「未選択」「無い入力」「録れない」は `handler::audio_rec`)。

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
/// 位置と大きさ (画像クリップ / 字幕 / Transform の Par。X / Y は記号のまま、回転 / 不透明度は同じ語を使う)。
pub const FIELD_X: &str = "X";
pub const FIELD_Y: &str = "Y";
pub const FIELD_W: &str = "幅";
pub const FIELD_H: &str = "高さ";
/// 字幕の塗り / 縁 / 影の色 (R / G / B は記号のまま、アルファは「不透明度」— 自動化の「A」と紛れない)。
pub const FIELD_R: &str = "R";
pub const FIELD_G: &str = "G";
pub const FIELD_B: &str = "B";
pub const FIELD_SCALE_X: &str = "拡大 X";
pub const FIELD_SCALE_Y: &str = "拡大 Y";
pub const FIELD_ANCHOR_X: &str = "基準点 X";
pub const FIELD_ANCHOR_Y: &str = "基準点 Y";

// ---- Rack の行 (r.md #158 仕様 10 / 11 / 15) ----

/// chain の末尾に足す (トラック / chain)。マスターは「エフェクト」(楽器を挿せない)。
pub const ADD_PLUGIN: &str = "+ プラグイン";
pub const ADD_EFFECT: &str = "+ エフェクト";
/// Parallel に chain を足す。
pub const ADD_CHAIN: &str = "+ チェーン";
/// マスターの Rack の末尾の区切り (この下の Limiter はフェーダーの後ろ)。
pub const POST_FADER: &str = "フェーダー後";
/// バイパス中の行の札。
pub const OFF_TAG: &str = "オフ";
/// 読み込みに失敗した plugin の行: 名前の頭と、再読込のボタン。
pub const NOT_LOADED: &str = "[未ロード]";
pub const RELOAD: &str = "再読込";
/// 出力 (パラアウト) のパネル: 子トラックを作って出力を割り当てるボタンと、その欄名。
pub const EXPLODE_OUTPUTS: &str = "展開";
pub const FIELD_CHILD_TRACKS: &str = "子トラック";
/// Parallel の Selector の欄 (アクティブな chain / 切り替えのクロスフェード)。
pub const SPLIT_ACTIVE: &str = "アクティブ";
pub const SPLIT_FADE: &str = "フェード";

/// SC パネルの 1 行の欄名 (plugin の aux 入力 port、1 始まり)。
#[must_use]
pub fn sc_port(n: u8) -> String {
    format!("入力 {n}")
}

/// 出力 (パラアウト) のパネルの 1 行の欄名 (plugin の aux 出力 port、1 始まり)。
#[must_use]
pub fn aux_out_port(n: usize) -> String {
    format!("出力 {n}")
}

/// 帯の欄 (`form::cols`) のラベル列 (`layout::FORM_LABEL_W`) に入る欄名の全部。テストが全部を測り、ラベル列に
/// 収まることを確かめる (登録し忘れは、描いた画面で「…」に切れた欄名を探すテストが拾う)。
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
    FIELD_SCALE_X,
    FIELD_SCALE_Y,
    FIELD_ANCHOR_X,
    FIELD_ANCHOR_Y,
    FIELD_CHILD_TRACKS,
    FIELD_X,
    FIELD_Y,
    FIELD_W,
    FIELD_H,
    FIELD_R,
    FIELD_G,
    FIELD_B,
    FIELD_FOLLOW_TRIGGER,
    FIELD_FOLLOW_COUNT,
    FIELD_FOLLOW_INTERVAL,
    FIELD_FOLLOW_A,
    FIELD_FOLLOW_CHANCE_A,
    FIELD_FOLLOW_B,
    FIELD_LEARN_LAUNCH,
    FIELD_LEARN_STOP,
    FIELD_LEARN_TO_ARRANGER,
    FIELD_LEARN_FOLLOW,
    FIELD_LEARN_CLEAR,
];

/// Rack の行の直下 (Par / SC / 出力、`form::cols_nested`) の欄名。ラベル列は行の名前の左端から始まるぶん狭い
/// (深さ 0 で `FORM_LABEL_W - ROW_NAME_X`、Parallel の中は 1 段ごとに `BAR_W` 狭い)。SC / 出力の「入力 N」/
/// 「出力 N」は [`sc_port`] / [`aux_out_port`] で、テストが別に測る。
pub const NESTED_FORM_LABELS: &[&str] = &[
    FIELD_LIPSYNC_TARGET,
    FIELD_X,
    FIELD_Y,
    FIELD_ROTATION,
    FIELD_SCALE_X,
    FIELD_SCALE_Y,
    FIELD_ANCHOR_X,
    FIELD_ANCHOR_Y,
    FIELD_OPACITY,
    FIELD_CHILD_TRACKS,
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

// ---- フォローアクション (セル / 列) と MIDI 割り当て ----

/// 小見出し「フォローアクション」(セルと列で同じ形)。
pub const SUB_FOLLOW: &str = "フォローアクション";
/// [有効] [クリップ終端 ▾ / 指定時間 ▾]。
pub const FIELD_FOLLOW_TRIGGER: &str = "発動";
pub const TOGGLE_FOLLOW_ON: &str = "有効";
/// 発動が「クリップ終端」のときのループ回数 / 「指定時間」のときの間隔 (同じ行で入れ替わる)。
pub const FIELD_FOLLOW_COUNT: &str = "回数";
pub const FIELD_FOLLOW_INTERVAL: &str = "間隔 (拍)";
/// 次に何をするか (A / B と、A を選ぶ確率。B は 100 − A)。ジャンプのときは同じ行の右に飛び先。
pub const FIELD_FOLLOW_A: &str = "アクション A";
pub const FIELD_FOLLOW_CHANCE_A: &str = "A の確率 %";
pub const FIELD_FOLLOW_B: &str = "アクション B";
/// MIDI 割り当て (Learn) の行: 何を MIDI に割り当てるか。ボタンは押すと学習を始める宛先。
pub const FIELD_LEARN_LAUNCH: &str = "鳴らす";
pub const FIELD_LEARN_STOP: &str = "止める";
pub const FIELD_LEARN_TO_ARRANGER: &str = "アレンジへ戻す";
pub const FIELD_LEARN_CLEAR: &str = "消去";
/// r.md #161: フォローアクションの全体 on/off (トランスポートの `Follow` ボタンと同じスイッチ)。
pub const FIELD_LEARN_FOLLOW: &str = "フォロー";
pub const LEARN_FOLLOW_ALL: &str = "全体 on/off";
pub const LEARN_CELL: &str = PLACE_CELL;
/// 列 (シーン) — 帯の名前と同じ語 (同じものに 2 つの呼び名を付けない)。
pub const LEARN_SCENE: &str = BAND_SCENE;
pub const LEARN_ROW: &str = "行";
pub const LEARN_ALL: &str = "全部";
pub const CLEAR_BINDINGS: &str = "割り当てを全部消す";

/// 詳細「MIDI 割り当て (N 件)」。学習の待ち受け中は「· 学習中」を添える (詳細を畳んでいても、次の MIDI 入力が
/// 割り当てられることが見える)。
#[must_use]
pub fn launch_midi_detail(n: usize, learning: bool) -> String {
    let head = format!("MIDI 割り当て ({n} 件)");
    if learning { format!("{head}{SEP}学習中") } else { head }
}

// ---- タイトルと帯の対象 ----

/// 語と語の区切り (タイトルの 2 行目「音声クリップ · トラック Audio」/ 帯の対象「Synth · 無効中」)。
pub const SEP: &str = " · ";
/// 無効なトラック (r.md #131: チェーンは host に居ない)。タイトルの 2 行目と Rack の帯の対象に添える。
pub const DISABLED: &str = "無効中";
/// タイトルの 2 行目: 普通のトラックの種類。
pub const KIND_TRACK: &str = "トラック";
/// タイトルの 2 行目: ランチャーのセル (アレンジのクリップと見分ける)。
pub const PLACE_CELL: &str = "セル";
/// タイトル: 名前の無いクリップ (種類は 2 行目に出す)。
pub const UNNAMED_CLIP: &str = "(名前なし)";

/// タイトルの 2 行目: クリップの居るトラック。
#[must_use]
pub fn track_label(name: &str) -> String {
    format!("{KIND_TRACK} {name}")
}

/// タイトルの 2 行目: グループ (バス) トラックの種類と子の数。
#[must_use]
pub fn group_kind(children: usize) -> String {
    format!("グループ{SEP}子 {children} 本")
}

/// Rack の帯の対象に添える、読み込みに失敗した plugin の数 (警告色、畳んだ Parallel の中も数える)。
#[must_use]
pub fn failed_loads(n: usize) -> String {
    format!("読み込み失敗 {n}")
}

/// 畳んだ Parallel / 開いていない chain の行に添える、中に隠れた読み込み失敗の数 (警告色)。
#[must_use]
pub fn hidden_failures(n: usize) -> String {
    format!("失敗 {n}")
}

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

/// タイトルの 2 行目と選択の帯の見出し: 種類の違うクリップを一緒に選んでいる (代表の種類を名乗らない)。
pub const KIND_MIXED: &str = "種類の違うクリップ";

/// タイトルの 2 行目 / 選択の帯の見出しの種類の語 (`mixed` = 種類が混ざっている)。
#[must_use]
pub fn clip_kind_label(kind: ClipBandKind, mixed: bool) -> &'static str {
    if mixed { KIND_MIXED } else { clip_band_title(kind) }
}

/// 種類の内訳の短い語 (「音声 2 · MIDI 1」)。
#[must_use]
fn clip_kind_short(kind: ClipBandKind) -> &'static str {
    match kind {
        ClipBandKind::Audio => "音声",
        ClipBandKind::Midi => "MIDI",
        ClipBandKind::Image => "画像",
        ClipBandKind::Video => "映像",
        ClipBandKind::Text => "テキスト",
        ClipBandKind::Automation => "オートメーション",
    }
}

/// 選択の帯の対象: 種類ごとの数 (`counts` は `ClipBandKind::ALL` の並び。0 の種類は出さない)。
#[must_use]
pub fn clip_kind_counts(counts: &[usize; ClipBandKind::ALL.len()]) -> String {
    let parts: Vec<String> = ClipBandKind::ALL
        .iter()
        .zip(counts)
        .filter(|(_, n)| **n > 0)
        .map(|(k, n)| format!("{} {n}", clip_kind_short(*k)))
        .collect();
    parts.join(SEP)
}

/// 種類の違うクリップを選んでいるとき、種類ごとの欄の上に出す小見出し (この下の欄が効く相手)。
#[must_use]
pub fn only_kind(kind: ClipBandKind, n: usize, total: usize) -> String {
    format!("{} だけに効く ({total} のうち {n})", clip_band_title(kind))
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
