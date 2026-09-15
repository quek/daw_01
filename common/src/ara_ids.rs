//! **ARA の persistent id** の SSoT (r.md #132 残件、2026-09-15 決定) — ホストが ARA document に作るモデル
//! object の名前と、旧アーカイブの読み替え。
//!
//! ARA の audio source / audio modification の `persistentID` は、アーカイブと document のグラフをつなぎ直す
//! 名前で、document の中で一意であればよい (`ara-sys/vendor/ARA_API/ARAInterface.h`
//! `ARAAudioSourceProperties::persistentID`: "ID used to re-connect model graph when archiving/unarchiving.
//! This ID must be unique for all audio sources within the document."、`ARAAudioModificationProperties` も同文)。
//! daw_01 は **安定 id** から作る (不変条件 1):
//!
//! - audio source = 素材 1 つ ([`source_id`])。 同ヘッダ "Typically a host will create an audio source object
//!   for each audio file used with ARA plug-ins."
//! - audio modification = content と take と素材 ([`modification_id`]、take は `AudioEvent::take_key`)。 分割の片は
//!   同じ take なので 1 つの modification を共有し、Melodyne の編集が片をまたいで続き、位置もずれない
//!   (region は take の写像で置く)。 同ヘッダ "All playback regions that share the same audio modification play
//!   back the same musical content"。 同じ content を見る linked clip も共有する (中身の編集を共有するのと同じ)。
//! - playback region は永続しない (同ヘッダ "Playback regions are not persistent when storing documents, instead
//!   the host re-creates them as needed.") ので、ホストの中で置き方を更新するためのキー ([`region_key`]) だけ。
//!
//! v41 以前のアーカイブは位置由来の id (`"{素材}:{クリップ}:{content の中の event の位置}"`、modification は
//! その後ろに `/mod`) で書かれている。 読み込みで旧 id → 今の id の表 ([`AraIdAlias`]) を作って device に持たせ
//! ([`migrate_legacy_archives`])、document を組むときに `ARARestoreObjectsFilter` の archive id / current id の
//! 対応で読み替える (同ヘッダ `ARARestoreObjectsFilter`: "The given IDs refer to objects in the archive, but can
//! optionally be mapped to those used in the current document.")。 新しいアーカイブが届いたら表は捨てる。

use std::collections::HashSet;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::model::{AudioEvent, AudioSourceId, ClipContent, ContentId, Song, for_each_plugin_mut};

/// 素材 `source` の ARA audio source の persistent id。
#[must_use]
pub fn source_id(source: AudioSourceId) -> String {
    format!("daw01.source.{source}")
}

/// content `content` の event `event` が属する take の ARA audio modification の persistent id: content と take
/// (`AudioEvent::take_key`) と **素材**。
///
/// 素材を含めるのは、modification の状態がその素材の上の編集だから (同ヘッダ: "Restoring an audio modification
/// without restoring its underlying audio source may not succeed if the audio source state has changed")。 content を
/// 丸ごと置き換える操作 (Bounce In Place の `Song::replace_window_content`) は新しい content の event id を 1 から
/// 振るので、別の素材の take が置き換える前と同じ content / take の id になる。 素材を含めないと、焼いた音の
/// modification に元の素材の上の編集 (保存したアーカイブや、plug-in host が destroy した時点で取っておいた状態) を
/// restore してしまう。
#[must_use]
pub fn modification_id(content: ContentId, event: &AudioEvent) -> String {
    format!("daw01.take.{content}.{}.{}", event.take_key(), event.source_id)
}

/// content `content` の event `event` の modification を **写して始める元** の modification (content を複製して
/// 共有を解いたとき、複製元の同じ take の modification、`Song::content_forked_from`)。 複製でなければ `None`。
/// 同ヘッダ `cloneAudioModification`: "used to create independent variations of the audio edits as opposed to
/// creating aliases by merely adding playback regions to a given audio modification"。
#[must_use]
pub fn modification_origin(song: &Song, content: ContentId, event: &AudioEvent) -> Option<String> {
    song.content_forked_from.get(&content).map(|&origin| modification_id(origin, event))
}

/// クリップ `clip_id` の event `event_id` の playback region のキー (永続しない、document の中で一意)。
#[must_use]
pub fn region_key(clip_id: u32, event_id: u32) -> String {
    format!("{clip_id}.{event_id}")
}

/// アーカイブに書かれている persistent id (`archived`) と、今の document で同じ object が持つ id (`current`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct AraIdAlias {
    pub archived: String,
    pub current: String,
}

/// 今の id `current` の object の状態が、アーカイブではどの id で書かれているか (表に無ければ同じ id)。
#[must_use]
pub fn archived_id<'a>(aliases: &'a [AraIdAlias], current: &'a str) -> &'a str {
    aliases.iter().find(|a| a.current == current).map_or(current, |a| a.archived.as_str())
}

/// v41 以前の ARA アーカイブを持つ device に、旧 id → 今の id の表を持たせる (`project` の version-gated
/// migration、deserialize 直後の保存時点の姿で走る)。
///
/// - 旧版は modification を **クリップごと** に持っていた。 1 つのトラックで content を共有する linked clip
///   は、今の id では 1 つの modification にまとまってしまい、クリップごとの編集が 1 つしか残らない。 開いた
///   直後の音を変えないために、2 本目以降のクリップの content を複製して分ける (音も編集もそれまでと同じ)。
/// - 旧版は audio source もクリップの event ごとに持っていた。 今は素材ごとに 1 つなので、最初に現れた旧 source
///   の状態 (同じ素材の解析結果) を読み替える。
pub fn migrate_legacy_archives(song: &mut Song) {
    for t in 0..song.tracks.len() {
        if !song.tracks[t].plugins().any(|p| p.ara_archive.is_some()) {
            continue;
        }
        unshare_audio_contents(song, t);
        let aliases = legacy_aliases(song, t);
        for_each_plugin_mut(&mut song.tracks[t].devices, &mut |p| {
            if p.ara_archive.is_some() {
                p.ara_archive_ids.clone_from(&aliases);
            }
        });
    }
}

/// トラック `t` のクリップが 1 つの audio content を共有していたら、2 本目以降に複製を持たせる。
fn unshare_audio_contents(song: &mut Song, t: usize) {
    let mut seen = HashSet::new();
    for c in 0..song.tracks[t].clips.len() {
        let content_id = song.tracks[t].clips[c].content_id;
        let Some(content @ ClipContent::Audio(_)) = song.clip_contents.get_mut(&content_id) else {
            continue;
        };
        // 旧版の id は event の安定 id を前提にしない (位置由来) が、今の take は event id で決まる。
        content.ensure_element_ids();
        if !seen.insert(content_id) {
            let copy = song.fork_content(content_id);
            // 複製した content の編集はアーカイブにクリップごとに書かれている (読み替え表で自分の旧 id から
            // restore する) ので、複製元の編集を写す元としては記録しない。
            song.content_forked_from.remove(&copy);
            song.tracks[t].clips[c].content_id = copy;
        }
    }
}

/// トラック `t` の旧 id → 今の id (旧版の `collect_ara_clips_for_track` と同じ走査順、今の id ごとに最初の 1 つ)。
fn legacy_aliases(song: &Song, t: usize) -> Vec<AraIdAlias> {
    let mut aliases = Vec::new();
    let mut mapped = HashSet::new();
    for clip in &song.tracks[t].clips {
        let Some(ClipContent::Audio(audio)) = song.clip_contents.get(&clip.content_id) else {
            continue;
        };
        for (index, event) in audio.events.iter().enumerate() {
            let legacy = format!("{}:{}:{index}", event.source_id, clip.id);
            let pairs = [
                (legacy.clone(), source_id(event.source_id)),
                (format!("{legacy}/mod"), modification_id(clip.content_id, event)),
            ];
            for (archived, current) in pairs {
                if mapped.insert(current.clone()) {
                    aliases.push(AraIdAlias { archived, current });
                }
            }
        }
    }
    aliases
}
