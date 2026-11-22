//! r.md #157 (`docs/plan_chord_track.md` §3.1): コードトラックの帯とポップアップの GUI 状態。
//!
//! 曲の中身 (箱の列 / 追従のスイッチ) は `Song` に居る。ここは「見方の都合」と一時状態だけ:
//! - [`ChordBandView`] (`ProjectView::chord_band`): 帯の表示 / 表記。`ViewState` に保存し、dirty は立てない。
//! - [`ChordBandEphemeral`] (`ProjectEphemeral::chord_band`): ポップアップ / 右クリックメニュー。保存しない。

use common::chord_name::ChordNotation;
use daw_ui_renderer::Rect;

/// 帯の見方 (保存する、dirty は立てない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordBandView {
    /// 帯を出すか (Arranger 帯の見出しのトグル)。
    pub visible: bool,
    /// コード名の表記 (`AppData::chord_notation` の中身)。
    pub notation: ChordNotation,
}

impl Default for ChordBandView {
    fn default() -> Self {
        Self { visible: true, notation: ChordNotation::Regular }
    }
}

/// 帯の画面上の位置 (widget が毎フレーム mirror する。`arrange_arranger_rect` と同じ idiom)。
/// ポップアップの置き場所、貼り付け先 (ポインタが帯の上か) の判定が読む。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChordLaneGeom {
    /// 帯の矩形 (lanes 幅)。隠していれば高さ 0。
    pub rect: Rect,
    /// 帯の左端の拍と 1px あたりの拍 (ruler / クリップと同じ写像)。
    pub start_beat: f64,
    pub beat_per_px: f64,
}

impl Default for ChordLaneGeom {
    fn default() -> Self {
        Self { rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, start_beat: 0.0, beat_per_px: 1.0 }
    }
}

impl ChordLaneGeom {
    /// 拍 → 画面 x。
    #[must_use]
    pub fn x_of(&self, beat: f64) -> f32 {
        self.rect.x + ((beat - self.start_beat) / self.beat_per_px) as f32
    }

    /// 画面 x → 拍。
    #[must_use]
    pub fn beat_of(&self, x: f32) -> f64 {
        self.start_beat + f64::from(x - self.rect.x) * self.beat_per_px
    }
}

/// 帯の一時状態 (保存しない)。
#[derive(Debug, Default)]
pub struct ChordBandEphemeral {
    /// 帯の画面上の位置 (widget の mirror)。
    pub lane: ChordLaneGeom,
    /// 開いているポップアップ (Q4 の Chord Selector)。`None` = 閉じている。
    pub popup: Option<ChordPopup>,
    /// 帯の右クリックメニュー `(右クリックした箱, 右クリック pos)`。箱の外なら `None` の箱。
    pub menu: Option<(Option<u32>, (f32, f32))>,
    /// 上のメニューの 1-shot open trigger (`section_menu_open` と同じ idiom)。
    pub menu_open: bool,
}

/// 開いているポップアップ 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct ChordPopup {
    /// 編集中の箱。
    pub chord_id: u32,
    /// 入力欄の世代。ボタン / 候補でコードを変えるたびに進め、入力欄を作り直して今の名前を
    /// 全選択で出し直す (入力欄は focus 中は自分の buffer を正とするので、値を差し替えるには作り直す)。
    pub rev: u32,
    /// 入力欄の編集中の文字列 (`parse_chord` で読めたときだけ確定する)。
    pub text: String,
    /// 入力欄を打ち換えたか。打ち換えている間だけ ←→ を入力欄の caret 移動に譲る
    /// (触っていなければ ←→ は隣の箱へ移る)。
    pub typed: bool,
    /// 次のコードの候補のうち上下キーで選んでいるもの。
    pub candidate: Option<usize>,
}

impl ChordPopup {
    #[must_use]
    pub fn new(chord_id: u32, text: String) -> Self {
        Self { chord_id, rev: 0, text, typed: false, candidate: None }
    }
}
