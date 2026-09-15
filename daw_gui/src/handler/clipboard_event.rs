//! handler::clipboard_event — 貼り付け / カット / コピーの入口 (`AppEvent::Clipboard`)。
//!
//! 貼り先の解決 (ポインタ下の面 / タブ間ドラッグの着地行) は view が済ませて
//! [`PasteContent`] に載せる。ここは適用とステータスの文言だけを持つ — 1 event の中で
//! 走るので、トラックを足してから貼る操作も 1 undo step になる。

use crate::event_clipboard::{CellPasteDest, ClipPasteDest, ClipboardEvent, PasteContent, PasteOrigin};
use crate::event_launcher::LauncherRow;
use crate::state::{AppData, LauncherFocus};

impl AppData {
    /// [`ClipboardEvent`] の処理 (`AppEvent::Clipboard` の 1 arm)。
    pub(crate) fn handle_clipboard_event(&mut self, ev: ClipboardEvent) {
        match ev {
            ClipboardEvent::Paste { content, origin } => self.paste_content(content, origin),
            ClipboardEvent::CopyTracks(ids) => self.copy_tracks(ids),
            ClipboardEvent::CutTracks(ids) => self.cut_tracks(ids),
            ClipboardEvent::CopyDevices(ids) => self.copy_devices(ids),
            ClipboardEvent::CutDevices(ids) => self.cut_devices(ids),
        }
    }

    fn paste_content(&mut self, content: PasteContent, origin: PasteOrigin) {
        match content {
            PasteContent::Notes { notes, at } => {
                let n = self.paste_notes_at(notes, at);
                self.report_paste(n, "ノート", origin, None);
            }
            PasteContent::AudioEvents { events, at } => {
                let n = self.paste_events_at(events, at);
                self.report_paste(n, "イベント", origin, None);
            }
            PasteContent::AutomationPoints { points, lane, at } => {
                let n = self.paste_points_at(points, lane, at);
                self.report_paste(n, "オートメーションポイント", origin, None);
            }
            PasteContent::AutomationClips { clips, lane, at } => {
                let n = self.paste_automation_clips_at(clips, lane, at);
                self.report_paste(n, "オートメーションクリップ", origin, None);
            }
            PasteContent::Devices { devices, dest_track } => {
                let n = self.paste_devices(devices, dest_track);
                self.report_paste(n, "プラグイン", origin, None);
            }
            PasteContent::Clips { clips, source_project_id, media, dest, at } => {
                self.paste_clips_to(clips, source_project_id, &media, dest, at, origin);
            }
            PasteContent::LauncherCells { cells, source_project_id, media, dest } => {
                self.paste_cells_to(cells, source_project_id, &media, dest, origin);
            }
            PasteContent::Tracks { payload, source_project_id, media, above } => {
                let n = self.paste_tracks_at(payload, source_project_id, above, &media);
                let from_tab = format!("別のタブからトラックを {n} 本コピーしました");
                self.report_paste(n, "トラック", origin, Some(from_tab));
            }
        }
    }

    /// クリップを貼る。行の無い余白 ([`ClipPasteDest::NewTracks`]) なら、要る本数のトラックを
    /// 足してから貼る (同じ undo step)。
    fn paste_clips_to(
        &mut self,
        clips: Vec<crate::clipboard::ClipCopy>,
        source_project_id: u64,
        media: &common::model::MediaManifest,
        dest: ClipPasteDest,
        at: f64,
        origin: PasteOrigin,
    ) {
        let anchor = match dest {
            ClipPasteDest::Track(id) => Some(id),
            ClipPasteDest::NewTracks(n) => self.append_empty_tracks(n),
        };
        // トラックを足せなかった (書き出し中) なら何も貼らない。
        let Some(anchor) = anchor else {
            return;
        };
        let n = self.paste_clips_at(clips, source_project_id, anchor, at, media);
        let from_tab = match dest {
            ClipPasteDest::Track(_) => format!("別のタブからクリップを {n} 個コピーしました"),
            ClipPasteDest::NewTracks(_) => format!("別のタブからクリップを {n} 個、新しいトラックにコピーしました"),
        };
        self.report_paste(n, "クリップ", origin, Some(from_tab));
    }

    /// ランチャーのセルを貼る。行の無い余白 ([`CellPasteDest::NewTrack`]) なら末尾にトラックを
    /// 1 本足してその行に置く (同じ undo step)。
    fn paste_cells_to(
        &mut self,
        cells: Vec<crate::clipboard::LauncherCellCopy>,
        source_project_id: u64,
        media: &common::model::MediaManifest,
        dest: CellPasteDest,
        origin: PasteOrigin,
    ) {
        let focus = match dest {
            CellPasteDest::Cell(focus) => Some(focus),
            CellPasteDest::NewTrack { scene_index } => self
                .append_empty_track()
                .map(|track_id| LauncherFocus { row: LauncherRow::Track(track_id), scene_index }),
        };
        let Some(focus) = focus else {
            return;
        };
        let n = self.paste_launcher_cells(cells, source_project_id, focus, media);
        self.report_paste(n, "セル", origin, Some(format!("別のタブからセルを {n} 個コピーしました")));
    }

    /// 貼り付けの結果をステータスバーへ。`Ctrl+V` は **0 件なら出さない** (「貼れなかった」のに
    /// 成功の文言が出ると何が起きたか分からない)、タブ間のドラッグは `from_tab` の文言を出す。
    fn report_paste(&mut self, count: usize, noun: &str, origin: PasteOrigin, from_tab: Option<String>) {
        match (origin, from_tab) {
            (PasteOrigin::Clipboard, _) if count > 0 => {
                self.ui_ephemeral.status_message = format!("貼り付け: {count} {noun}");
            }
            (PasteOrigin::OtherTab, Some(message)) => self.ui_ephemeral.status_message = message,
            _ => {}
        }
    }
}
