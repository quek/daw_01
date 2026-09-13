//! Song の安定 id アロケータ群。
//!
//! model.rs (実コード 1,000 行 budget を大きく超えた god file、不変条件 9) から
//! 切り出した。「次の id を採番する」規則の SSoT で、`Song` のメソッドはここへ委譲する。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::{AudioSourceId, ClipContent, ContentId, ImageSourceId, Song, VideoSourceId};

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
    /// r.md #89: `ModRouting::id` の採番。`0` は未採番 sentinel。
    /// [`crate::model::AutomationTarget::ModRoutingDepth`] が 1 本の変調を指すために要る。
    #[serde(default)]
    pub next_mod_routing_id: u32,
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

    /// 新しい device / chain id を採番する (採番規則の SSoT)。`Song` 全体を可変借用せずに
    /// 呼べるので、正規化が `tracks` / `master_fx_chain` を可変で歩きながら組み込みを補える。
    pub fn alloc_device_id(&mut self) -> u64 {
        let id = self.next_device_id.max(1);
        self.next_device_id = id.saturating_add(1);
        id
    }

    /// 各 allocator を `other` との大きい方へ上げる (下げない)。
    fn raise_to(&mut self, other: &Self) {
        // field を足したら持ち越し漏れがコンパイルで分かるよう、`..` を使わずに分解する。
        let Self {
            next_track_id,
            next_device_id,
            next_content_id,
            next_audio_source_id,
            next_video_source_id,
            next_image_source_id,
            next_song_lane_id,
            next_section_id,
            next_mod_source_id,
            next_mod_routing_id,
            next_scene_id,
        } = other;
        self.next_track_id = self.next_track_id.max(*next_track_id);
        self.next_device_id = self.next_device_id.max(*next_device_id);
        self.next_content_id = self.next_content_id.max(*next_content_id);
        self.next_audio_source_id = self.next_audio_source_id.max(*next_audio_source_id);
        self.next_video_source_id = self.next_video_source_id.max(*next_video_source_id);
        self.next_image_source_id = self.next_image_source_id.max(*next_image_source_id);
        self.next_song_lane_id = self.next_song_lane_id.max(*next_song_lane_id);
        self.next_section_id = self.next_section_id.max(*next_section_id);
        self.next_mod_source_id = self.next_mod_source_id.max(*next_mod_source_id);
        self.next_mod_routing_id = self.next_mod_routing_id.max(*next_mod_routing_id);
        self.next_scene_id = self.next_scene_id.max(*next_scene_id);
    }
}

impl Song {
    /// undo / redo / 履歴ジャンプで live を履歴の Song に差し替えた直後に呼ぶ: 差し替え前の live (`live`)
    /// までに採番した id を**二度と出さない**よう、allocator の high-water を持ち越す (不変条件 1)。
    ///
    /// 履歴の Song は採番の状態ごと過去に戻るので、持ち越さないと「追加 → undo → 別の追加」で同じ id が
    /// 別の物に振られ、id を鍵にした session 状態 (開いた Par / 折り畳み / 行高 / MIDI Learn 待ち /
    /// デコードキャッシュ等) が別物に付く。redo で戻る物は元の id のまま (id は変えない、上げるのは次の番号だけ)。
    ///
    /// 対象: `Song::ids` 全部、同じ id のトラックの clip / send / lane の allocator、同じ id の lane の clip
    /// allocator、同じ id の content の note / event / point allocator。
    pub fn carry_id_high_water_from(&mut self, live: &Song) {
        self.ids.raise_to(&live.ids);
        for t in &mut self.tracks {
            let Some(lt) = live.track_by_id(t.id) else { continue };
            t.next_clip_id = t.next_clip_id.max(lt.next_clip_id);
            t.next_send_id = t.next_send_id.max(lt.next_send_id);
            t.next_lane_id = t.next_lane_id.max(lt.next_lane_id);
            for lane in &mut t.automation_lanes {
                if let Some(ll) = lt.lane_by_id(lane.id) {
                    lane.next_clip_id = lane.next_clip_id.max(ll.next_clip_id);
                }
            }
        }
        for lane in &mut self.song_lanes {
            if let Some(ll) = live.song_lane_by_id(lane.id) {
                lane.next_clip_id = lane.next_clip_id.max(ll.next_clip_id);
            }
        }
        for (id, content) in &mut self.clip_contents {
            let theirs = live.clip_contents.get(id);
            match content {
                ClipContent::Midi(c) => {
                    if let Some(ClipContent::Midi(l)) = theirs {
                        c.next_note_id = c.next_note_id.max(l.next_note_id);
                    }
                }
                ClipContent::Audio(c) => {
                    if let Some(ClipContent::Audio(l)) = theirs {
                        c.next_event_id = c.next_event_id.max(l.next_event_id);
                    }
                }
                ClipContent::Automation(c) => {
                    if let Some(ClipContent::Automation(l)) = theirs {
                        c.next_point_id = c.next_point_id.max(l.next_point_id);
                    }
                }
                // event に安定 id の allocator を持たない種類。
                ClipContent::Video(_) | ClipContent::Image(_) | ClipContent::Text(_) => {}
            }
        }
    }
}
