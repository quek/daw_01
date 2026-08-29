//! Song の安定 id アロケータ群。
//!
//! model.rs (実コード 1,000 行 budget を大きく超えた god file、不変条件 9) から
//! 切り出した。「次の id を採番する」規則の SSoT で、`Song` のメソッドはここへ委譲する。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::{AudioSourceId, ContentId, ImageSourceId, VideoSourceId};

/// Song の安定 id アロケータ群 (§10 bullet 4 で Song のフラットな `next_*_id` カウンタを集約)。
/// 各 `next_*_id` は「次に採番する id」で、`0` は "未採番" sentinel。削除後も id を再利用しない
/// (安定 id addressing、invariant #1)。nested `"ids": {...}` として save / wire し、旧 .daw の
/// フラット形式は load 時の JSON 前処理 `project::migrate_flat_ids_to_allocators` が `ids` 下へ移す
/// (save 互換)。Song は `clip_contents` 等の `HashMap<u32, _>` を持つため serde `flatten` は
/// 使えず (整数キー復元不可)、MediaPools と同じく nested を採る。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct IdAllocators {
    #[serde(default)]
    pub next_track_id: u32,
    #[serde(default)]
    pub next_device_id: u64,
    #[serde(default)]
    pub next_content_id: ContentId,
    #[serde(default)]
    pub next_audio_source_id: AudioSourceId,
    #[serde(default)]
    pub next_video_source_id: VideoSourceId,
    #[serde(default)]
    pub next_image_source_id: ImageSourceId,
    #[serde(default)]
    pub next_song_lane_id: u32,
    #[serde(default)]
    pub next_section_id: u32,
    #[serde(default)]
    pub next_mod_source_id: u32,
    /// v35 (r.md #87): ランチャーの列 [`Scene`] の採番。`0` は未採番 sentinel。
    #[serde(default)]
    pub next_scene_id: u32,
}

impl IdAllocators {
    /// 新しい `ContentId` を採番する (採番規則の SSoT)。`Song` 全体を可変借用せずに
    /// 呼べるので、`ensure_clip_contents` が `tracks` を可変で歩きながら使える。
    pub fn alloc_content_id(&mut self) -> ContentId {
        let id = self.next_content_id.max(1);
        self.next_content_id = id.saturating_add(1);
        id
    }
}
