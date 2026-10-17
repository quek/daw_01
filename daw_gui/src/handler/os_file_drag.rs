//! handler::os_file_drag — r.md #141: アレンジのクリップ / セッションビューのセルを OS のファイルドラッグに切り替えるとき、
//! 運んでいるクリップの **素材ファイル** (Explorer から落とすのと同じ、元のファイルそのまま) を解決する。
//! ドラッグ本体は runner がフレームの後に [`crate::os_file_drag::run`] で行う。

use std::path::PathBuf;

use common::model::{AudioSourceId, AudioSourcePath, ClipContent, ClipKey};

use crate::state::AppData;

impl AppData {
    /// 運んでいるクリップが参照する audio 素材 (重複なし、出てきた順)。
    pub(crate) fn clips_audio_source_ids(&self, keys: &[ClipKey]) -> Vec<AudioSourceId> {
        let song = self.cur.song_doc.song();
        let mut ids: Vec<AudioSourceId> = Vec::new();
        for &key in keys {
            let Some(clip) = song.clip_by_key(key) else { continue };
            let Some(ClipContent::Audio(audio)) = song.clip_contents.get(&clip.content_id) else {
                continue;
            };
            for ev in &audio.events {
                if !ids.contains(&ev.source_id) {
                    ids.push(ev.source_id);
                }
            }
        }
        ids
    }

    /// 素材ファイルの絶対パスを解決して、runner が次に行う OS ドラッグに積む。
    /// ファイルを持たない素材 (Generated) は ARA と同じ書き出し先へ WAV を書いてから渡す。
    pub(crate) fn request_os_file_drag(&mut self, keys: &[ClipKey]) {
        let project_dir = self.project_dir();
        let mut paths: Vec<PathBuf> = Vec::new();
        for id in self.clips_audio_source_ids(keys) {
            let Some(source) = self.cur.song_doc.song().media.audio_sources.get(&id) else { continue };
            let path = match &source.path {
                AudioSourcePath::Absolute(p) => Some(p.clone()),
                AudioSourcePath::ProjectRelative(rel) => project_dir.as_ref().map(|d| d.join(rel)),
                AudioSourcePath::Generated { .. } => {
                    if !self.cur.pipc.ara_pcm_materialized.contains_key(&id)
                        && let Err(e) = self.materialize_generated_source(id)
                    {
                        tracing::warn!(source_id = id, error = %e, "os file drag: failed to materialize generated source");
                    }
                    self.cur.pipc.ara_pcm_materialized.get(&id).cloned()
                }
            };
            match path {
                Some(p) if p.is_file() => {
                    if !paths.contains(&p) {
                        paths.push(p);
                    }
                }
                other => tracing::warn!(source_id = id, path = ?other, "os file drag: source file not found"),
            }
        }
        if paths.is_empty() {
            self.ui_ephemeral.status_message = "ドラッグ: 渡せる素材ファイルがありません".to_owned();
            return;
        }
        self.ui_ephemeral.pending_os_file_drag = paths;
    }
}
