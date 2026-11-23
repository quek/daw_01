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

impl ChordBandEphemeral {
    /// 曲を差し替えたとき (`AppData::reset_song_scoped_state`): 箱の id を持つもの (ポップアップ / メニュー) を
    /// 捨てる。箱の id は曲ごとに 1 から振るので、残すと新しい曲の別の箱に当たる。帯の位置は曲に依らない。
    pub fn reset_song_scoped(&mut self) {
        self.popup = None;
        self.menu = None;
        self.menu_open = false;
    }
}

/// ポップアップが編集しているもの。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PopupTarget {
    /// 曲にある箱。
    Box(u32),
    /// r.md #159: まだ曲に無い箱 (空きのダブルクリック / Tab の行き先)。コードを選んだ時点で曲に置き
    /// (`ChordTrackEvent::Place`)、`Box` に変わる。選ばずに閉じる / ←→ で離れると何も残さない — 見えている箱は
    /// どれも自分で選んだコードになり、選んでいない主音の箱を後から直して「書き換え」(メロディーが動く) にならない。
    Draft(ChordDraft),
}

/// 置く前の箱 ([`PopupTarget::Draft`])。帯には半透明の枠で出す。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChordDraft {
    pub start_beat: f64,
    pub len_beats: f64,
    /// 入力欄に出しているコード (位置のキーの主音 / Tab なら候補の先頭)。Enter / Tab はこれで決める。
    pub chord: common::model::Chord,
}

/// 開いているポップアップ 1 つ。
#[derive(Debug, Clone, PartialEq)]
pub struct ChordPopup {
    /// 編集中の箱 (または置く前の箱)。
    pub target: PopupTarget,
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
    /// MIDI 鍵盤で弾いた和音 (`ChordToolsState::played`) の通し番号のうち、もう拾ったもの。開いた時点の
    /// 番号を入れておくので、開く前に弾いた和音は拾わない (Instrument Input)。
    pub played_seen: u64,
    /// r.md #159: このポップアップで **作った** 箱 (下書きにコードを選んで置いた箱)。閉じるまで、←→ で戻って直しても
    /// これらの箱のコードを決めるのは「置く」(メロディーは動かない、`ChordTrackEvent::Recast`)。ほかの箱のコードを
    /// 変えるのは「書き換え」(`ChordTrackEvent::SetChord`)。
    pub placing: Vec<u32>,
}

impl ChordPopup {
    /// 曲にある箱 `chord_id` のポップアップ。
    #[must_use]
    pub fn new(chord_id: u32, text: String) -> Self {
        Self::on(PopupTarget::Box(chord_id), text)
    }

    #[must_use]
    pub fn on(target: PopupTarget, text: String) -> Self {
        Self { target, rev: 0, text, typed: false, candidate: None, played_seen: 0, placing: Vec::new() }
    }

    /// 編集中の箱の id (置く前の箱なら `None`)。
    #[must_use]
    pub fn box_id(&self) -> Option<u32> {
        match self.target {
            PopupTarget::Box(id) => Some(id),
            PopupTarget::Draft(_) => None,
        }
    }

    /// 置く前の箱 (曲にある箱なら `None`)。
    #[must_use]
    pub fn draft(&self) -> Option<ChordDraft> {
        match self.target {
            PopupTarget::Draft(d) => Some(d),
            PopupTarget::Box(_) => None,
        }
    }
}
