//! 貼り付け / カット / コピーのイベント (`AppEvent::Clipboard` の中身)。
//!
//! 何を貼るか (`PasteContent`) は view が OS clipboard / タブ間のドラッグの payload と
//! ポインタ位置から決め、Song への適用はすべて `handle_event` を通す
//! ([`AppData::handle_clipboard_event`](crate::state::AppData::handle_clipboard_event))。
//! 以前は view が `paste_*_at` / `cut_tracks` を直接呼んでいて、undo 履歴に操作名が付かず、
//! 直前の event の undo step に吸収されることがあった。

use common::model::{AudioEvent, AutomationLaneKey, MediaManifest, Note};

use crate::clipboard::{AutomationClipCopy, ClipCopy, CopiedPoint, DeviceCopy, LauncherCellCopy, TracksCopy};
use crate::state::LauncherFocus;

#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardEvent {
    /// 貼り付け。`origin` はステータスバーの文言だけに効く (Song への適用は同じ)。
    Paste { content: PasteContent, origin: PasteOrigin },
    /// トラック面の `Ctrl+C` (Song は変えない。plugin があれば state の往復を待つ)。
    CopyTracks(Vec<u32>),
    /// トラック面の `Ctrl+X` (コピー + 削除で 1 undo step)。
    CutTracks(Vec<u32>),
    /// device 面の `Ctrl+C` / 行メニューの「コピー」。
    CopyDevices(Vec<u64>),
    /// device 面の `Ctrl+X` / 行メニューの「カット」。
    CutDevices(Vec<u64>),
}

/// 貼り付けの出どころ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteOrigin {
    /// `Ctrl+V` (OS clipboard)。
    Clipboard,
    /// 別のタブからのドラッグ (`docs/plan_project_tabs.md` §5.6)。
    OtherTab,
}

/// 貼る中身と貼り先。貼り先は view がポインタから解決済み。
#[derive(Debug, Clone, PartialEq)]
pub enum PasteContent {
    /// ピアノロールの `at` 拍 (song 拍)。
    Notes { notes: Vec<Note>, at: f64 },
    /// オーディオエディタの `at` 拍 (クリップ内)。
    AudioEvents { events: Vec<AudioEvent>, at: f64 },
    AutomationPoints { points: Vec<CopiedPoint>, lane: AutomationLaneKey, at: f64 },
    AutomationClips { clips: Vec<AutomationClipCopy>, lane: AutomationLaneKey, at: f64 },
    /// いまインスペクタに出ているチェーン。
    Devices { devices: Vec<DeviceCopy>, dest_track: u32 },
    Clips { clips: Vec<ClipCopy>, source_project_id: u64, media: MediaManifest, dest: ClipPasteDest, at: f64 },
    LauncherCells { cells: Vec<LauncherCellCopy>, source_project_id: u64, media: MediaManifest, dest: CellPasteDest },
    /// `above` の直上 (未知の id なら末尾)。
    Tracks { payload: TracksCopy, source_project_id: u64, media: MediaManifest, above: u32 },
}

/// クリップの貼り先トラック。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipPasteDest {
    /// このトラックを `track_offset == 0` の行にする。
    Track(u32),
    /// 行の無い余白: 末尾に新しいトラックを `n` 本作り、1 本目を `track_offset == 0` の行にする
    /// (トラックの追加と貼り付けで 1 undo step)。
    NewTracks(usize),
}

/// セルの貼り先。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CellPasteDest {
    Cell(LauncherFocus),
    /// 行の無い余白: 末尾に新しいトラックを作り、その行の `scene_index` 列に置く。
    NewTrack { scene_index: usize },
}

impl ClipboardEvent {
    /// 編集履歴のラベル (`AppEvent::undo_label` から委譲される)。
    #[must_use]
    pub fn undo_label(&self) -> &'static str {
        match self {
            Self::Paste { content, .. } => match content {
                PasteContent::Notes { .. } => "ノート貼り付け",
                PasteContent::AudioEvents { .. } => "オーディオイベント貼り付け",
                PasteContent::AutomationPoints { .. } => "ポイント貼り付け",
                PasteContent::AutomationClips { .. } => "オートメーションクリップ貼り付け",
                PasteContent::Devices { .. } => "デバイス貼り付け",
                PasteContent::Clips { .. } => "クリップ貼り付け",
                PasteContent::LauncherCells { .. } => "セル貼り付け",
                PasteContent::Tracks { .. } => "トラック貼り付け",
            },
            Self::CutTracks(_) => "トラックをカット",
            Self::CutDevices(_) => "デバイスをカット",
            // Song を変えないので snapshot は積まれない。
            Self::CopyTracks(_) | Self::CopyDevices(_) => crate::state::song_doc::GENERIC_UNDO_LABEL,
        }
    }
}
