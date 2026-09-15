//! 範囲選択の中身を動かす / 複製する / ミュートする編集イベント (`AppEvent::Range` の中身、
//! `docs/plan_range_selection.md`)。
//!
//! キー (`Q` / ←→ / `D`) と、クリップヘッダのドラッグ (`widgets::arrangement::release`) が発行し、
//! [`AppData::handle_range_event`](crate::state::AppData::handle_range_event) が適用する。
//! 範囲の削除は `AppEvent::DeleteTimeSelection`、Live の "…Time" は `AppEvent::DeleteTime` ほか。

#[derive(Debug, Clone, PartialEq)]
pub enum RangeEvent {
    /// `Q`: 範囲の境界で割り、範囲部分のクリップのミュートを切り替える。
    Mute,
    /// ←→: 範囲の中身を `delta_beats` 拍ずらす (キーリピートは 1 undo step に畳む)。
    Nudge { delta_beats: f64 },
    /// `D` / `Alt+D`: 範囲を 1 つ後ろへ複製する (選択中のオートメーションクリップも)。
    Duplicate { unique: bool },
    /// クリップヘッダのドラッグの確定: `range` の中身を `delta_beats` 拍、行ごとに `rows` の
    /// 行き先へ動かす / 複製する。
    Move { range: (f64, f64), delta_beats: f64, rows: Vec<(u32, RowDest)>, mode: RangeMoveMode },
}

/// 範囲の 1 行 (移動元トラック) の行き先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowDest {
    Track(u32),
    /// 行の無い余白: この移動で末尾に作る新しいトラックの何本目か (0 始まり)。
    /// トラックの追加と移動で 1 undo step。
    NewTrack(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeMoveMode {
    Move,
    /// `Ctrl`: content を共有する複製。
    CopyLinked,
    /// `Ctrl+Shift`: content も複製する。
    CopyUnique,
}

impl RangeEvent {
    /// 編集履歴のラベル (`AppEvent::undo_label` から委譲される)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::Mute => "範囲のミュート",
            Self::Nudge { .. } => "範囲の移動",
            Self::Duplicate { .. } => "範囲の複製",
            Self::Move { mode: RangeMoveMode::Move, .. } => "クリップ移動",
            Self::Move { .. } => "クリップ複製",
        }
    }
}
