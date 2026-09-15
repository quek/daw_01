//! Arranger セクション帯の編集イベント (`AppEvent::Section` の中身)。
//!
//! 帯のドラッグ / ダブルクリック (`widgets::arrangement::release`)、右クリックメニュー
//! (`view::arrangement_view`)、Delete キー (`AppData::delete_current_surface`) が発行し、
//! [`AppData::handle_section_event`](crate::state::AppData::handle_section_event) が適用する。
//! 改名 / 色は既存の `BeginRenameSection` … / `SetSectionColor`。

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SectionEvent {
    /// 帯を作る (`start` / `len` は widget で snap 済)。
    Create { start: f64, len: f64 },
    /// 帯を範囲の中身ごと `start` へ動かす (前後は ripple)。
    Move { id: u32, start: f64 },
    /// 帯の被覆範囲だけを変える (中身は動かさない)。
    Resize { id: u32, start: f64, len: f64 },
    /// 帯を範囲の中身ごと `dest_start` へ複製挿入する。
    Duplicate { id: u32, dest_start: f64 },
    /// 帯だけ消す (中身は残す)。
    DeleteBand(u32),
    /// 帯の時間範囲を中身ごと消して詰める。
    DeleteRange(u32),
    /// 選択中の帯を消す (帯だけ、Delete キー)。
    DeleteSelected,
}

impl SectionEvent {
    /// 編集履歴のラベル (`AppEvent::undo_label` から委譲される)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::Create { .. } => "セクション作成",
            Self::Move { .. } => "セクション移動",
            Self::Resize { .. } => "セクション長さ変更",
            Self::Duplicate { .. } => "セクション複製",
            Self::DeleteBand(_) | Self::DeleteSelected => "セクション削除",
            Self::DeleteRange(_) => "セクションを範囲ごと削除",
        }
    }
}
