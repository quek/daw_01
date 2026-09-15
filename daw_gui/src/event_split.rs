//! r.md #132: 分割 / 結合キー (`E` / `Alt+E` / `Shift+E` / `J`) のイベント
//! (`docs/plan_rmd_132_grid_split.md`)。
//!
//! [`AppEvent::SplitJoin`](crate::event::AppEvent::SplitJoin) が包む 1 本に集約する
//! (`LauncherEvent` / `VirtualKeyboardEvent` と同じ「1 arm = 1 サブ enum」)。 キーの
//! 振り分けは `view::split_keys`、処理は
//! [`AppData::handle_split_join_event`](crate::state::AppData::handle_split_join_event)。
//!
//! どれも `handle_event` を通るので、1 回のキー操作が 1 undo step になり、履歴に
//! 操作名が付く (以前のピアノロールの `E` / `J` は `Edit::mutate` から handler を
//! 直接呼んでいて、直前のイベントの undo scope とラベルのまま積まれていた)。

/// 分割 / 結合の対象面。 キーを押したときのポインタ位置で決まる (`view::split_keys`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitSurface {
    /// ピアノロール (オーディオエディタを開いていない下部パネル) のノート。
    Notes,
    /// アレンジのクリップ。 オーディオエディタの波形の上ならその event
    /// (`AppData::split_clips`)。
    Clips,
}

/// 切り口の決め方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAt {
    /// カーソル (無ければ再生ヘッド) の 1 点。 `snap` = グリッドへ吸着する (`Alt+E` で `false`)。
    Cursor { snap: bool },
    /// その画面のグリッド線 (曲の拍 0 を原点にした絶対グリッド) すべて (`Shift+E`)。
    Grid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitJoinEvent {
    /// `E` / `Alt+E` / `Shift+E`。
    Split { surface: SplitSurface, at: SplitAt },
    /// `J` (アレンジ = 範囲を 1 クリップへ焼き込む / ピアノロール = 同じ音のノートを結合)。
    Join { surface: SplitSurface },
}

impl SplitJoinEvent {
    /// 編集履歴のラベル (`AppEvent::undo_label` から委譲される)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        use SplitAt::{Cursor, Grid};
        use SplitSurface::{Clips, Notes};
        match *self {
            Self::Split { surface: Notes, at: Cursor { .. } } => "ノート分割",
            Self::Split { surface: Notes, at: Grid } => "ノートをグリッドで分割",
            Self::Split { surface: Clips, at: Cursor { .. } } => "クリップ分割",
            Self::Split { surface: Clips, at: Grid } => "クリップをグリッドで分割",
            Self::Join { surface: Notes } => "ノート結合",
            Self::Join { surface: Clips } => "クリップ結合",
        }
    }
}
