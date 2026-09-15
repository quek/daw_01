//! **ARA の persistent id** の SSoT (r.md #132 残件、2026-09-15 決定) — ホストが ARA document に作るモデル
//! object の名前、アーカイブの目次、コピーが写す元。
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
//! **コピー** (v43): 写した take は別の modification になり (以後は別々に編集できる variation)、写した元の take を
//! `AudioEvent::take_origins` に持つ。 document に初めて現れるとき、元の編集から始める ([`origin_modification_id`]、
//! 同ヘッダ `cloneAudioModification`: "used to create independent variations of the audio edits as opposed to
//! creating aliases by merely adding playback regions to a given audio modification"、別の document から写すときは
//! partial archive: "copying and pasting audio source and audio modification state between songs")。 どこから写すかは
//! `daw_plugin_host::ara::graph_plan::modification_start`。
//!
//! **アーカイブの目次** ([`AraArchiveEntry`]、`PluginInstance::ara_archive_ids`): 保存したアーカイブの中にある
//! object の今の id と、アーカイブに書かれている id。 目次に無い object はアーカイブに状態が無いので、ほかの元
//! (写した元) から始められる。 v41 以前のアーカイブは位置由来の id (`"{素材}:{クリップ}:{content の中の event の
//! 位置}"`、modification はその後ろに `/mod`) で書かれているので、読み込みで旧 id → 今の id の目次を作る
//! ([`migrate_legacy_archives`])。 document を組むときに `ARARestoreObjectsFilter` の archive id / current id の
//! 対応で読み替える (同ヘッダ: "The given IDs refer to objects in the archive, but can optionally be mapped to those
//! used in the current document.")。

use std::collections::{HashMap, HashSet};

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::model::{
    AudioEvent, AudioSourceId, ClipContent, ContentId, Song, TakeOrigin, Track, for_each_plugin_mut, shown_indices,
};

const SOURCE_PREFIX: &str = "daw01.source.";
const TAKE_PREFIX: &str = "daw01.take.";

/// 素材 `source` の ARA audio source の persistent id。
#[must_use]
pub fn source_id(source: AudioSourceId) -> String {
    format!("{SOURCE_PREFIX}{source}")
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
    take_modification_id(content, event.take_key(), event.source_id)
}

/// content `content` の take `take` (素材 `source`) の ARA audio modification の persistent id。
#[must_use]
pub fn take_modification_id(content: ContentId, take: u32, source: AudioSourceId) -> String {
    format!("{TAKE_PREFIX}{content}.{take}.{source}")
}

/// 写した元の take ([`TakeOrigin`]) の modification の persistent id (元のプロジェクトの id の空間)。
#[must_use]
pub fn origin_modification_id(origin: &TakeOrigin) -> String {
    take_modification_id(origin.content, origin.take, origin.source)
}

/// クリップ `clip_id` の event `event_id` の playback region のキー (永続しない、document の中で一意)。
#[must_use]
pub fn region_key(clip_id: u32, event_id: u32) -> String {
    format!("{clip_id}.{event_id}")
}

/// トラック `track` の ARA document に並ぶ object の persistent id (素材の audio source と、クリップの窓に見えている
/// take の audio modification)。 region を置く規則 (窓に見えている片) は daw_gui の
/// `collect_ara_clips_for_track` と同じ `shown_indices`。 素材の file が見つからない片もここには数える
/// (目次は「アーカイブに状態があり得る object」なので、多めに数えても何も起きない)。
#[must_use]
pub fn document_objects(song: &Song, track: &Track) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for clip in &track.clips {
        let Some(ClipContent::Audio(audio)) = song.clip_contents.get(&clip.content_id) else {
            continue;
        };
        for i in shown_indices(&audio.events, clip.content_window()) {
            let event = &audio.events[i];
            for id in [source_id(event.source_id), modification_id(clip.content_id, event)] {
                if seen.insert(id.clone()) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// アーカイブの目次の 1 行: アーカイブの中にある object の **今の id** と、アーカイブに書かれている id
/// (`None` = 今の id のまま書かれている)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub struct AraArchiveEntry {
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<String>,
}

impl AraArchiveEntry {
    /// 今の id のまま書かれている object (plug-in host が今書いたアーカイブ)。
    #[must_use]
    pub fn stored(id: String) -> Self {
        Self { current: id, archived: None }
    }

    /// アーカイブに書かれている id。
    #[must_use]
    pub fn archived_id(&self) -> &str {
        self.archived.as_deref().unwrap_or(&self.current)
    }
}

/// 今の id `current` の object の状態が、アーカイブではどの id で書かれているか。 目次に無ければ `None`
/// (= アーカイブにその object の状態は無い)。
#[must_use]
pub fn archived_id<'a>(entries: &'a [AraArchiveEntry], current: &str) -> Option<&'a str> {
    entries.iter().find(|e| e.current == current).map(AraArchiveEntry::archived_id)
}

/// 目次の今の id を、content と素材の付け替え (`contents` / `sources` の 旧 id → 新しい id、載っていない id は
/// そのまま) に合わせる。 トラックを別のプロジェクトへ貼る / 独立に複製すると、device のアーカイブは元の content と
/// 素材の id で書かれたまま、トラックのクリップは新しい id を指すので、目次の今の id を新しい側へ移す
/// (書かれている id はそのまま)。
#[must_use]
pub fn remap_archive_contents(
    entries: &[AraArchiveEntry],
    contents: &HashMap<ContentId, ContentId>,
    sources: &HashMap<AudioSourceId, AudioSourceId>,
) -> Vec<AraArchiveEntry> {
    let source_of = |s: AudioSourceId| sources.get(&s).copied().unwrap_or(s);
    entries
        .iter()
        .map(|e| {
            let current = if let Some(s) = parse_source_id(&e.current) {
                source_id(source_of(s))
            } else if let Some((c, take, s)) = parse_modification_id(&e.current) {
                take_modification_id(contents.get(&c).copied().unwrap_or(c), take, source_of(s))
            } else {
                e.current.clone()
            };
            let archived = (current != e.current).then(|| e.archived_id().to_owned()).or_else(|| e.archived.clone());
            AraArchiveEntry { current, archived }
        })
        .collect()
}

fn parse_source_id(id: &str) -> Option<AudioSourceId> {
    id.strip_prefix(SOURCE_PREFIX)?.parse().ok()
}

fn parse_modification_id(id: &str) -> Option<(ContentId, u32, AudioSourceId)> {
    let mut parts = id.strip_prefix(TAKE_PREFIX)?.split('.');
    let parsed = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    parts.next().is_none().then_some(parsed)
}

/// v41 以前の ARA アーカイブを持つ device に、旧 id → 今の id の目次を持たせる (`project` の version-gated
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
        let entries = legacy_entries(song, t);
        for_each_plugin_mut(&mut song.tracks[t].devices, &mut |p| {
            if p.ara_archive.is_some() {
                p.ara_archive_ids.clone_from(&entries);
            }
        });
    }
}

/// v42 のアーカイブ (今の id で書かれていて目次を持たない) に、トラックの document の object から目次を作る
/// (`project` の version-gated migration)。 アーカイブは保存した時点の document (そのトラックのクリップの窓に
/// 見えていた take と素材) を書いたものなので、同じ Song から数え直せば中身と一致する。 目次を既に持つ
/// device (v41 以前の読み替え) には触らない。
pub fn migrate_archive_contents(song: &mut Song) {
    for t in 0..song.tracks.len() {
        if !song.tracks[t].plugins().any(|p| p.ara_archive.is_some() && p.ara_archive_ids.is_empty()) {
            continue;
        }
        let entries: Vec<AraArchiveEntry> =
            document_objects(song, &song.tracks[t]).into_iter().map(AraArchiveEntry::stored).collect();
        for_each_plugin_mut(&mut song.tracks[t].devices, &mut |p| {
            if p.ara_archive.is_some() && p.ara_archive_ids.is_empty() {
                p.ara_archive_ids.clone_from(&entries);
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
            // 複製した content の編集はアーカイブにクリップごとに書かれている (目次で自分の旧 id から restore
            // する) ので、複製元の編集を写す元としては記録しない。
            if let Some(ClipContent::Audio(audio)) = song.clip_contents.get_mut(&copy) {
                for event in &mut audio.events {
                    event.take_origins.clear();
                }
            }
            song.tracks[t].clips[c].content_id = copy;
        }
    }
}

/// トラック `t` の旧 id → 今の id (旧版の `collect_ara_clips_for_track` と同じ走査順、今の id ごとに最初の 1 つ)。
fn legacy_entries(song: &Song, t: usize) -> Vec<AraArchiveEntry> {
    let mut entries = Vec::new();
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
                    entries.push(AraArchiveEntry { current, archived: Some(archived) });
                }
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 目次の今の id は content と素材の付け替えに合わせて移り、書かれている id (旧い読み替えを含む) は残る。
    /// 付け替えに載っていない id と、安定 id の形でない id はそのまま。
    #[test]
    fn 目次の今の_id_は付け替えた_content_と素材へ移り書かれている_id_は残る() {
        let entries = vec![
            AraArchiveEntry::stored(source_id(3)),
            AraArchiveEntry::stored(take_modification_id(10, 1, 3)),
            AraArchiveEntry { current: take_modification_id(11, 2, 4), archived: Some("4:7:0/mod".into()) },
            AraArchiveEntry::stored(take_modification_id(12, 1, 4)),
            AraArchiveEntry::stored("other".into()),
        ];
        let contents = HashMap::from([(10, 20), (11, 21)]);
        let sources = HashMap::from([(3, 30)]);
        let remapped = remap_archive_contents(&entries, &contents, &sources);
        let expected = vec![
            AraArchiveEntry { current: source_id(30), archived: Some(source_id(3)) },
            AraArchiveEntry { current: take_modification_id(20, 1, 30), archived: Some(take_modification_id(10, 1, 3)) },
            AraArchiveEntry { current: take_modification_id(21, 2, 4), archived: Some("4:7:0/mod".into()) },
            AraArchiveEntry::stored(take_modification_id(12, 1, 4)),
            AraArchiveEntry::stored("other".into()),
        ];
        assert_eq!(remapped, expected);
        assert_eq!(archived_id(&remapped, &take_modification_id(20, 1, 30)), Some(take_modification_id(10, 1, 3).as_str()));
        assert_eq!(archived_id(&remapped, &take_modification_id(10, 1, 3)), None, "移った後の目次に元の今の id は無い");
    }
}
