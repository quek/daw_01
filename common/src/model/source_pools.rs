//! imported media source プール (audio / video / image) の採番・参照数・保存前 GC・load 時の
//! sentinel 解消と、GC と bundle 掃除が共有する到達可能性判定 ([`Song::live_source_ids`])。
//!
//! プールの型 ([`MediaPools`]) は `Song.media` として wire を渡るので model.rs に置き、ここは
//! wire に載らないロジックだけを持つ (`common/build.rs` の `WIRE_SOURCES` 対象外)。

use super::*;

/// [`Song::live_source_ids`] の戻り値: 到達可能な media source id。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LiveSourceIds {
    pub audio: std::collections::HashSet<AudioSourceId>,
    pub video: std::collections::HashSet<VideoSourceId>,
    pub image: std::collections::HashSet<ImageSourceId>,
}

impl Song {
    /// 生きている content (= [`Song::live_content_ids`]) の event と、 track の口パク
    /// `mouth_map` から到達できる media source id。 `gc_*_sources` (保存時の pool 整理)
    /// と `daw_gui::media_bundle` (保存時の bundle 掃除) が同じ判定を使う — ここが
    /// ずれると、 pool から落ちた source のファイルが「未参照」 としてゴミ箱へ行く。
    ///
    /// 到達可能性で数える (pool の存在ではなく) のは、 in-memory の pool は Undo 用に
    /// 参照ゼロの entry を保持し続けるため。 `mouth_map` の slot は event を経由しない
    /// 直接参照なので別途足す (未割当 = `0` は除く)。
    pub fn live_source_ids(&self) -> LiveSourceIds {
        let contents = self.live_content_ids();
        let mut live = LiveSourceIds::default();
        for (id, content) in &self.clip_contents {
            if !contents.contains(id) {
                continue;
            }
            match content {
                ClipContent::Audio(a) => live.audio.extend(a.events.iter().map(|ev| ev.source_id)),
                ClipContent::Video(v) => live.video.extend(v.events.iter().map(|ev| ev.source_id)),
                ClipContent::Image(i) => live.image.extend(i.events.iter().map(|ev| ev.source_id)),
                ClipContent::Midi(_) | ClipContent::Automation(_) | ClipContent::Text(_) => {}
            }
        }
        for map in self.tracks.iter().filter_map(|t| t.mouth_map.as_ref()) {
            live.image.extend(map.all_ids().filter(|id| *id != 0));
        }
        live
    }

    /// Allocate a fresh `AudioSourceId`, bumping the song-level counter.
    pub fn alloc_audio_source_id(&mut self) -> AudioSourceId {
        let id = self.ids.next_audio_source_id.max(1);
        self.ids.next_audio_source_id = id.saturating_add(1);
        id
    }

    /// Refcount of an `AudioSourceId` = total `AudioEvent.source_id`
    /// references across every audio `ClipContent` in the song. Used by
    /// `gc_audio_sources` and Inspector display. `Video` clips do not
    /// reference AudioSource directly — the auto-extracted WAV is wired
    /// via the paired audio track's `AudioEvent`, which is counted here
    /// like any other audio reference.
    pub fn audio_source_refcount(&self, source_id: AudioSourceId) -> usize {
        self.clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Audio(a) => Some(a.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Automation(_)
                | ClipContent::Video(_)
                | ClipContent::Image(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .filter(|ev| ev.source_id == source_id)
            .count()
    }

    /// Drop `audio_sources` entries nothing reachable references
    /// ([`Song::live_source_ids`]). Called before save so the on-disk pool
    /// stays tidy. In-memory entries with refcount=0 are kept so Undo can
    /// restore them.
    pub fn gc_audio_sources(&mut self) {
        let live = self.live_source_ids().audio;
        self.media.audio_sources.retain(|id, _| live.contains(id));
    }

    /// Re-assign fresh `AudioSourceId` to any source whose id is the
    /// `0` sentinel (and bump `next_audio_source_id` above the highest
    /// seen). Idempotent — sources with non-zero ids are left untouched.
    /// Mirrors `ensure_clip_contents` semantics.
    pub fn ensure_audio_source_ids(&mut self) {
        let mut max_seen: AudioSourceId = 0;
        for id in self.media.audio_sources.keys() {
            if *id != 0 {
                max_seen = max_seen.max(*id);
            }
        }
        if self.ids.next_audio_source_id <= max_seen {
            self.ids.next_audio_source_id = max_seen + 1;
        }
        if self.ids.next_audio_source_id == 0 {
            self.ids.next_audio_source_id = 1;
        }
        // Re-key any AudioSource currently held under id 0. AudioEvent
        // references to id 0 are NOT remapped — those remain dangling
        // (= "missing source") which is the correct UX for unresolved
        // imports. Callers that mint a fresh AudioSource should always
        // go through `alloc_audio_source_id` and avoid sentinel 0.
        if let Some(orphan) = self.media.audio_sources.remove(&0) {
            let new_id = self.alloc_audio_source_id();
            self.media.audio_sources.insert(new_id, orphan);
        }
    }

    /// v12 (`docs/plan_video.md` §2.4): allocate a fresh
    /// `VideoSourceId`, bumping the song-level counter. Mirrors
    /// `alloc_audio_source_id`.
    pub fn alloc_video_source_id(&mut self) -> VideoSourceId {
        let id = self.ids.next_video_source_id.max(1);
        self.ids.next_video_source_id = id.saturating_add(1);
        id
    }

    /// v12: refcount of a `VideoSourceId` = total `VideoEvent.source_id`
    /// references across every `Video` `ClipContent` in the song. Used
    /// by `gc_video_sources` and (future) inspector display.
    pub fn video_source_refcount(&self, source_id: VideoSourceId) -> usize {
        self.clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Video(v) => Some(v.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Audio(_)
                | ClipContent::Automation(_)
                | ClipContent::Image(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .filter(|ev| ev.source_id == source_id)
            .count()
    }

    /// v12: drop `video_sources` entries nothing reachable references
    /// ([`Song::live_source_ids`]). Mirrors `gc_audio_sources`.
    pub fn gc_video_sources(&mut self) {
        let live = self.live_source_ids().video;
        self.media.video_sources.retain(|id, _| live.contains(id));
    }

    /// v12: re-assign fresh `VideoSourceId` to any source whose id is
    /// the `0` sentinel and bump `next_video_source_id` above the
    /// highest seen. Mirrors `ensure_audio_source_ids` semantics; v11
    /// files load with all-default fields so this only matters once
    /// v12 sources start being saved with sentinel ids (= shouldn't
    /// happen in practice, but the invariant is cheap to enforce).
    pub fn ensure_video_source_ids(&mut self) {
        let mut max_seen: VideoSourceId = 0;
        for id in self.media.video_sources.keys() {
            if *id != 0 {
                max_seen = max_seen.max(*id);
            }
        }
        if self.ids.next_video_source_id <= max_seen {
            self.ids.next_video_source_id = max_seen + 1;
        }
        if self.ids.next_video_source_id == 0 {
            self.ids.next_video_source_id = 1;
        }
        if let Some(orphan) = self.media.video_sources.remove(&0) {
            let new_id = self.alloc_video_source_id();
            self.media.video_sources.insert(new_id, orphan);
        }
    }

    /// v13 (`docs/plan_image_overlay.md` §2.4): allocate a fresh
    /// `ImageSourceId`, bumping the song-level counter. Mirrors
    /// `alloc_video_source_id`.
    pub fn alloc_image_source_id(&mut self) -> ImageSourceId {
        let id = self.ids.next_image_source_id.max(1);
        self.ids.next_image_source_id = id.saturating_add(1);
        id
    }

    /// v13: refcount of an `ImageSourceId` = total `ImageEvent.source_id`
    /// references across every `Image` `ClipContent` in the song. Used
    /// by `gc_image_sources` and (future) inspector display.
    pub fn image_source_refcount(&self, source_id: ImageSourceId) -> usize {
        self.clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Image(i) => Some(i.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Audio(_)
                | ClipContent::Automation(_)
                | ClipContent::Video(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .filter(|ev| ev.source_id == source_id)
            .count()
    }

    /// v13: drop `image_sources` entries nothing reachable references
    /// ([`Song::live_source_ids`]) — `ImageEvent` に加えて track の `mouth_map`
    /// (口パク slot) も参照に数える。 event に出ていない口形状の画像が save のたびに
    /// pool から落ち、 次回 load で mapping が空を指していた。
    pub fn gc_image_sources(&mut self) {
        let live = self.live_source_ids().image;
        self.media.image_sources.retain(|id, _| live.contains(id));
    }

    /// v13: re-assign fresh `ImageSourceId` to any source whose id is
    /// the `0` sentinel and bump `next_image_source_id` above the
    /// highest seen. Mirrors `ensure_video_source_ids` semantics.
    pub fn ensure_image_source_ids(&mut self) {
        let mut max_seen: ImageSourceId = 0;
        for id in self.media.image_sources.keys() {
            if *id != 0 {
                max_seen = max_seen.max(*id);
            }
        }
        if self.ids.next_image_source_id <= max_seen {
            self.ids.next_image_source_id = max_seen + 1;
        }
        if self.ids.next_image_source_id == 0 {
            self.ids.next_image_source_id = 1;
        }
        if let Some(orphan) = self.media.image_sources.remove(&0) {
            let new_id = self.alloc_image_source_id();
            self.media.image_sources.insert(new_id, orphan);
        }
    }
}
