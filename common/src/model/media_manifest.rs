//! 別プロジェクトへクリップを運ぶときの **媒体テーブルの写し** (`docs/plan_project_tabs.md` §5.6)。
//!
//! `ClipContent` は音源 / 映像 / 画像を **id** (`AudioSourceId` 等 = Song スコープの名前) で
//! 指すので、content だけを別プロジェクトへ貼ると `source_id` が宙に浮き、クリップの
//! 殻だけが貼られて中身が鳴らない。clipboard / タブ間ドラッグの envelope は content と
//! 一緒にこの写しを運び、貼り先が別プロジェクトなら [`Song::import_media`] で自分の
//! テーブルへ取り込んで id を張り替える ([`ClipContent::remap_media`])。
//!
//! パスはコピー側で **絶対パスに解いてから** 載せる (`ProjectRelative` は元プロジェクトの
//! フォルダ基準なので、貼り先のフォルダでは解決できない)。

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    AudioSource, AudioSourceId, AudioSourcePath, ClipContent, ImageSource, ImageSourceId,
    ImageSourcePath, Song, VideoSource, VideoSourceId, VideoSourcePath,
};

/// content 群が参照する媒体 (id → メタデータ) の写し。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaManifest {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<(AudioSourceId, AudioSource)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<(VideoSourceId, VideoSource)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image: Vec<(ImageSourceId, ImageSource)>,
}

impl MediaManifest {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.audio.is_empty() && self.video.is_empty() && self.image.is_empty()
    }

    /// `ProjectRelative` を元プロジェクトのフォルダ (`project_dir`) で絶対パスに解く。
    /// `None` (未保存プロジェクト) なら相対のまま (解きようがない)。
    pub fn absolutize(&mut self, project_dir: Option<&Path>) {
        let Some(dir) = project_dir else { return };
        for (_, a) in &mut self.audio {
            if let AudioSourcePath::ProjectRelative(rel) = &a.path {
                a.path = AudioSourcePath::Absolute(dir.join(rel));
            }
        }
        for (_, v) in &mut self.video {
            if let VideoSourcePath::ProjectRelative(rel) = &v.path {
                v.path = VideoSourcePath::Absolute(dir.join(rel));
            }
        }
        for (_, i) in &mut self.image {
            if let ImageSourcePath::ProjectRelative(rel) = &i.path {
                i.path = ImageSourcePath::Absolute(dir.join(rel));
            }
        }
    }
}

impl MediaManifest {
    /// [`Self::absolutize`] の逆。`project_dir` の配下にある絶対パスを
    /// `ProjectRelative` へ戻す。
    ///
    /// **取り込む直前に、貼り先のフォルダで**通す ([`Song::import_media`] はパスの
    /// 一致で既存 entry を流用するので、同じ音源でも表記が違うともう 1 本登録される)。
    pub fn relativize(&mut self, project_dir: Option<&Path>) {
        let Some(dir) = project_dir else { return };
        for (_, a) in &mut self.audio {
            if let AudioSourcePath::Absolute(p) = &a.path
                && let Ok(rel) = p.strip_prefix(dir)
            {
                a.path = AudioSourcePath::ProjectRelative(rel.to_path_buf());
            }
        }
        for (_, v) in &mut self.video {
            if let VideoSourcePath::Absolute(p) = &v.path
                && let Ok(rel) = p.strip_prefix(dir)
            {
                v.path = VideoSourcePath::ProjectRelative(rel.to_path_buf());
            }
        }
        for (_, i) in &mut self.image {
            if let ImageSourcePath::Absolute(p) = &i.path
                && let Ok(rel) = p.strip_prefix(dir)
            {
                i.path = ImageSourcePath::ProjectRelative(rel.to_path_buf());
            }
        }
    }
}

/// [`Song::import_media`] の結果: 写しの id → 貼り先の id。
#[derive(Debug, Clone, Default)]
pub struct MediaRemap {
    pub audio: HashMap<AudioSourceId, AudioSourceId>,
    pub video: HashMap<VideoSourceId, VideoSourceId>,
    pub image: HashMap<ImageSourceId, ImageSourceId>,
}

impl ClipContent {
    /// この content が参照する媒体 id を集める。
    pub fn collect_media_ids(
        &self,
        audio: &mut Vec<AudioSourceId>,
        video: &mut Vec<VideoSourceId>,
        image: &mut Vec<ImageSourceId>,
    ) {
        match self {
            ClipContent::Audio(a) => audio.extend(a.events.iter().map(|e| e.source_id)),
            ClipContent::Video(v) => video.extend(v.events.iter().map(|e| e.source_id)),
            ClipContent::Image(i) => image.extend(i.events.iter().map(|e| e.source_id)),
            ClipContent::Midi(_) | ClipContent::Automation(_) | ClipContent::Text(_) => {}
        }
    }

    /// 媒体 id を貼り先のものへ張り替える (写しに無かった id はそのまま)。
    pub fn remap_media(&mut self, remap: &MediaRemap) {
        match self {
            ClipContent::Audio(a) => {
                for e in &mut a.events {
                    if let Some(&id) = remap.audio.get(&e.source_id) {
                        e.source_id = id;
                    }
                }
            }
            ClipContent::Video(v) => {
                for e in &mut v.events {
                    if let Some(&id) = remap.video.get(&e.source_id) {
                        e.source_id = id;
                    }
                }
            }
            ClipContent::Image(i) => {
                for e in &mut i.events {
                    if let Some(&id) = remap.image.get(&e.source_id) {
                        e.source_id = id;
                    }
                }
            }
            ClipContent::Midi(_) | ClipContent::Automation(_) | ClipContent::Text(_) => {}
        }
    }
}

impl Song {
    /// `contents` が参照する媒体の写し (この Song のテーブルから引く。無い id は落とす)。
    pub fn media_manifest_for<'a>(
        &self,
        contents: impl IntoIterator<Item = &'a ClipContent>,
    ) -> MediaManifest {
        let (mut audio, mut video, mut image) = (Vec::new(), Vec::new(), Vec::new());
        for c in contents {
            c.collect_media_ids(&mut audio, &mut video, &mut image);
        }
        audio.sort_unstable();
        audio.dedup();
        video.sort_unstable();
        video.dedup();
        image.sort_unstable();
        image.dedup();
        MediaManifest {
            audio: audio
                .into_iter()
                .filter_map(|id| self.media.audio_sources.get(&id).map(|s| (id, s.clone())))
                .collect(),
            video: video
                .into_iter()
                .filter_map(|id| self.media.video_sources.get(&id).map(|s| (id, s.clone())))
                .collect(),
            image: image
                .into_iter()
                .filter_map(|id| self.media.image_sources.get(&id).map(|s| (id, s.clone())))
                .collect(),
        }
    }

    /// 写しの媒体を自分のテーブルへ取り込む。**同じパスの entry が既にあれば流用** する
    /// (同じクリップを 2 度貼っても音源が増えない)。戻り値 = 写しの id → 自分の id。
    pub fn import_media(&mut self, manifest: &MediaManifest) -> MediaRemap {
        let mut remap = MediaRemap::default();
        for (old, src) in &manifest.audio {
            let existing = self
                .media
                .audio_sources
                .iter()
                .find(|(_, s)| s.path == src.path)
                .map(|(id, _)| *id);
            let id = existing.unwrap_or_else(|| {
                let id = self.alloc_audio_source_id();
                self.media.audio_sources.insert(id, src.clone());
                id
            });
            remap.audio.insert(*old, id);
        }
        for (old, src) in &manifest.video {
            let existing = self
                .media
                .video_sources
                .iter()
                .find(|(_, s)| s.path == src.path)
                .map(|(id, _)| *id);
            let id = existing.unwrap_or_else(|| {
                let id = self.alloc_video_source_id();
                self.media.video_sources.insert(id, src.clone());
                id
            });
            remap.video.insert(*old, id);
        }
        for (old, src) in &manifest.image {
            let existing = self
                .media
                .image_sources
                .iter()
                .find(|(_, s)| s.path == src.path)
                .map(|(id, _)| *id);
            let id = existing.unwrap_or_else(|| {
                let id = self.alloc_image_source_id();
                self.media.image_sources.insert(id, src.clone());
                id
            });
            remap.image.insert(*old, id);
        }
        remap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioContent, AudioEvent};
    use std::path::PathBuf;

    fn audio_content(source_id: AudioSourceId) -> ClipContent {
        ClipContent::Audio(AudioContent {
            events: vec![AudioEvent { source_id, ..AudioEvent::default() }],
            next_event_id: 2,
        })
    }

    #[test]
    fn 別プロジェクトへ貼ると音源を取り込んで_id_を張り替え_同じパスは流用する() {
        let mut src = Song::default();
        let sid = src.alloc_audio_source_id();
        src.media.audio_sources.insert(
            sid,
            AudioSource {
                path: AudioSourcePath::ProjectRelative(PathBuf::from("samples/a.wav")),
                sample_rate: 48_000,
                channels: 2,
                frames: 10,
                original_bpm: None,
                root_key: None,
            },
        );
        let content = audio_content(sid);
        let mut manifest = src.media_manifest_for([&content]);
        manifest.absolutize(Some(Path::new("C:/proj_a")));
        assert_eq!(
            manifest.audio[0].1.path,
            AudioSourcePath::Absolute(PathBuf::from("C:/proj_a").join("samples/a.wav")),
            "相対パスは元プロジェクトのフォルダで絶対化"
        );

        let mut dst = Song::default();
        // 貼り先で同じ id が別の音源に使われていても衝突しない。
        let taken = dst.alloc_audio_source_id();
        assert_eq!(taken, sid);
        let remap = dst.import_media(&manifest);
        let new_id = remap.audio[&sid];
        assert_ne!(new_id, sid, "貼り先で新採番");
        let mut pasted = content.clone();
        pasted.remap_media(&remap);
        let ClipContent::Audio(a) = &pasted else { panic!() };
        assert_eq!(a.events[0].source_id, new_id);

        // 2 度目は同じパスなので流用 (音源テーブルが増えない)。
        let again = dst.import_media(&manifest);
        assert_eq!(again.audio[&sid], new_id);
        assert_eq!(dst.media.audio_sources.len(), 1);
    }
}
