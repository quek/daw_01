//! handler::chord_preset — コード進行プリセット (`.chords` = JSON) の保存と挿入、帯の右クリックメニューの
//! 項目の口 (r.md #157 §3.1)。
//!
//! 保存するのは箱の列 (最も早い箱を 0 とした相対) と、その頭の位置のキーのルート。挿入するときは
//! 挿入先の位置のキーへ移調する (両方にキーがあるときだけ。どちらかが無ければ書いたまま)。
//! ファイルダイアログは他と同じく別スレッド (`spawn_file_dialog`)、結果は `FileDialogResult`。

use std::path::Path;

use common::model::ChordEvent;
use serde::{Deserialize, Serialize};

use crate::app::{AppEvent, FileDialogKind, FileDialogMode};
use crate::event_chord::ChordTrackEvent;
use crate::state::AppData;

/// プリセットのファイル形式の版 (読めない版は断る)。
const PRESET_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct ChordPresetFile {
    version: u32,
    /// 列の頭の位置のキーのルート (pitch class)。キーが無かった位置なら `None`。
    #[serde(default)]
    key_root: Option<u8>,
    chords: Vec<ChordEvent>,
}

impl AppData {
    /// 選んでいる箱 (空なら全部) の色を変える (`None` = 既定色)。右クリックした箱 `id` は必ず含める。
    pub(crate) fn set_selected_chord_color(&mut self, id: u32, color: Option<[f32; 3]>) {
        let mut ids = self.live_chord_ids();
        if !ids.contains(&id) {
            ids.push(id);
        }
        self.handle_event(AppEvent::Chord(ChordTrackEvent::SetColor { ids, color }));
    }

    /// 選んでいる箱を移調する (右クリックメニュー)。
    pub(crate) fn transpose_selected_chords(&mut self, semitones: i32) {
        let ids = self.live_chord_ids();
        if !ids.is_empty() {
            self.handle_event(AppEvent::Chord(ChordTrackEvent::Transpose { ids, semitones }));
        }
    }

    /// キーボードの ↑↓: 選んでいる箱を移調し、移調後のいちばん左の箱を試聴する (耳で確かめながら動かす。
    /// ノートの ↑↓ と同じ)。
    pub(crate) fn nudge_selected_chords(&mut self, semitones: i32) {
        self.transpose_selected_chords(semitones);
        let song = self.cur.song_doc.song();
        let first = self
            .live_chord_ids()
            .into_iter()
            .filter_map(|id| song.chord_by_id(id))
            .min_by(|a, b| a.start_beat.total_cmp(&b.start_beat))
            .map(|c| c.id);
        if let Some(id) = first {
            self.audition_chord_box(id);
        }
    }

    /// プリセットに保存: 選んでいる箱 (選択が無ければ全部) を書き出す先を選ばせる。
    pub(crate) fn begin_save_chord_preset(&mut self) {
        let mut ids = self.live_chord_ids();
        if ids.is_empty() {
            ids = self.cur.song_doc.song().chords.iter().map(|c| c.id).collect();
        }
        if ids.is_empty() {
            self.ui_ephemeral.status_message = "保存するコードがありません".to_string();
            return;
        }
        let dialog = rfd::FileDialog::new().add_filter("コード進行", &["chords"]).set_file_name("progression.chords");
        self.spawn_file_dialog(dialog, FileDialogMode::Save, FileDialogKind::SaveChordPreset { ids });
    }

    /// プリセットを挿入: `at` (拍) から置くファイルを選ばせる。
    pub(crate) fn begin_insert_chord_preset(&mut self, at: f64) {
        let dialog = rfd::FileDialog::new().add_filter("コード進行", &["chords"]);
        self.spawn_file_dialog(dialog, FileDialogMode::PickFile, FileDialogKind::InsertChordPreset { at });
    }

    pub(crate) fn save_chord_preset(&mut self, path: &Path, ids: &[u32]) {
        let song = self.cur.song_doc.song();
        let picked: Vec<&ChordEvent> = song.chords.iter().filter(|c| ids.contains(&c.id)).collect();
        let Some(origin) = picked.iter().map(|c| c.start_beat).reduce(f64::min) else { return };
        let file = ChordPresetFile {
            version: PRESET_VERSION,
            key_root: song.scale_at(origin).map(|k| k.root),
            // 系統 (r.md #159) はプロジェクトの中の番号なので書き出さない (挿入は新しく置く箱 = 新しい系統)。
            chords: picked
                .into_iter()
                .map(|c| ChordEvent { id: 0, lineage: 0, start_beat: c.start_beat - origin, ..c.clone() })
                .collect(),
        };
        let n = file.chords.len();
        let result = serde_json::to_vec_pretty(&file)
            .map_err(std::io::Error::other)
            .and_then(|bytes| common::atomic_file::write_replace(path, &bytes));
        self.ui_ephemeral.status_message = match result {
            Ok(()) => format!("コード進行を保存: {n} 個 ({})", path.display()),
            Err(e) => format!("コード進行の保存に失敗: {e} ({})", path.display()),
        };
    }

    pub(crate) fn insert_chord_preset(&mut self, path: &Path, at: f64) {
        let parsed = std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice::<ChordPresetFile>(&bytes).map_err(|e| e.to_string()));
        let file = match parsed {
            Ok(f) if f.version == PRESET_VERSION => f,
            Ok(f) => {
                self.ui_ephemeral.status_message = format!("読めない版のコード進行です (版 {}): {}", f.version, path.display());
                return;
            }
            Err(e) => {
                self.ui_ephemeral.status_message = format!("コード進行を読めません: {e} ({})", path.display());
                return;
            }
        };
        let dest_key = self.cur.song_doc.song().scale_at(at).map(|k| k.root);
        let shift = file.key_root.zip(dest_key).map_or(0, |(from, to)| i32::from(to) - i32::from(from));
        let chords: Vec<ChordEvent> = file
            .chords
            .into_iter()
            .map(|c| ChordEvent { id: 0, chord: c.chord.sanitized().transposed(shift), ..c })
            .collect();
        let n = chords.len();
        self.place_chords_at(at, chords);
        self.ui_ephemeral.status_message = format!("コード進行を挿入: {n} 個");
    }
}

#[cfg(test)]
mod tests {
    use common::model::{Chord, ChordKind};
    use common::scale::{Scale, ScaleChange};

    use super::*;

    /// C メジャーで保存した進行を G メジャーの位置へ挿入すると 7 半音上へ移り、相対位置を保つ。
    #[test]
    fn preset_is_transposed_to_the_key_at_the_insert_point() {
        let mut app = crate::test_support::headless_app();
        app.edit_song(|song| {
            song.scale_changes = vec![
                ScaleChange { beat: 0.0, root: 0, scale: Scale::Major },
                ScaleChange { beat: 16.0, root: 7, scale: Scale::Major },
            ];
            for (s, root, kind) in [(0.0, 0, ChordKind::Major), (2.0, 9, ChordKind::Minor7)] {
                song.place_chord(ChordEvent::new(s, 2.0, Chord::new(root, kind)));
            }
        });
        let ids: Vec<u32> = app.cur.song_doc.song().chords.iter().map(|c| c.id).collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.chords");
        app.save_chord_preset(&path, &ids);
        app.insert_chord_preset(&path, 16.0);
        let got: Vec<(f64, u8, ChordKind)> =
            app.cur.song_doc.song().chords.iter().filter(|c| c.start_beat >= 16.0).map(|c| (c.start_beat, c.chord.root, c.chord.kind)).collect();
        assert_eq!(got, vec![(16.0, 7, ChordKind::Major), (18.0, 4, ChordKind::Minor7)]);
        assert_eq!(app.cur.selection.selected_chord_ids.len(), 2, "挿入した箱が選ばれる");
    }
}
