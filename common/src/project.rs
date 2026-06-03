use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::{CURRENT_VERSION, ProjectFile, Song};

/// Oldest project-file version `load` will accept. Versions below this
/// (currently `1` = the retired row-based format) are rejected with a
/// "re-create the project" error. Versions in `[MIN_LOADABLE_VERSION,
/// CURRENT_VERSION)` are accepted and forward-migrated via
/// `#[serde(default)]` on any new fields — fine because every field
/// added since v2 has a sensible default and no reinterpretation of
/// existing data.
const MIN_LOADABLE_VERSION: u32 = 2;

pub fn save(path: impl AsRef<Path>, song: &Song) -> Result<()> {
    let path = path.as_ref();
    let tmp = tmp_path(path);

    // GC orphan content / audio_source entries (refcount == 0) before
    // serializing so disk files stay tidy. Working on a clone —
    // caller's in-memory Song is not mutated.
    let mut song = song.clone();
    song.gc_clip_contents();
    song.gc_audio_sources();
    let project = ProjectFile {
        version: CURRENT_VERSION,
        song,
    };
    let json = serde_json::to_string_pretty(&project)
        .context("failed to serialize project to JSON")?;

    let mut file = fs::File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    file.write_all(json.as_bytes())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", tmp.display()))?;
    drop(file);

    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(())
}

pub fn load(path: impl AsRef<Path>) -> Result<Song> {
    let path = path.as_ref();
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let project: ProjectFile = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse project JSON from {}", path.display()))?;
    if project.version > CURRENT_VERSION {
        anyhow::bail!(
            "project file {} has version {} newer than supported {}",
            path.display(),
            project.version,
            CURRENT_VERSION
        );
    }
    if project.version < MIN_LOADABLE_VERSION {
        anyhow::bail!(
            "project file {} uses retired version {} (the row-based \
             format predating version 2); re-create the project in the \
             current free-time-note format.",
            path.display(),
            project.version,
        );
    }
    if project.version < CURRENT_VERSION {
        tracing::info!(
            path = %path.display(),
            from_version = project.version,
            current_version = CURRENT_VERSION,
            "loaded legacy project file; missing fields filled with serde defaults"
        );
    }
    let mut song = project.song;
    // v5 → v6 migration: drain any legacy `Clip.notes` into the shared
    // `clip_contents` store, allocating fresh `content_id` for clips that
    // still carry the sentinel `0`. Idempotent for v6 / v7 files.
    song.ensure_clip_contents();
    // v6 → v7 forward migration: `audio_sources` is empty for v6 files
    // (serde default), but we still bump `next_audio_source_id` above
    // any sentinel just in case. Idempotent.
    song.ensure_audio_source_ids();
    // Phase 6 review (SSOT 違反 fix): 旧コードは `ensure_ids()` 呼出を
    // caller (= `daw_gui::app::open_project` / `script::open` 等) に依存
    // していて、 invariant が caller 依存だった。 ここで呼ぶことで
    // `common::project::load` の戻り値が常に「track_id / clip_id / parent_
    // group_id が consistent」 という不変条件を満たす。 idempotent なので
    // caller 側が再呼び出ししても安全。
    song.ensure_ids();
    Ok(song)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Clip, ClipContent, InstrumentSource, MidiContent, Note, Track};
    use tempfile::tempdir;

    #[test]
    fn save_and_load_default_song() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        let song = Song::default();
        save(&path, &song).unwrap();
        assert_eq!(load(&path).unwrap(), song);
    }

    #[test]
    fn save_and_load_vocal_clip_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        // v6 形式で構築 (notes は clip_contents へ、 clip.notes は空)。
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note {
                        start_beat: 0.0,
                        duration_beats: 1.0,
                        pitch: 60,
                        velocity: 100,
                        lyric: Some("こ".into()),
                    },
                    Note {
                        start_beat: 1.0,
                        duration_beats: 0.5,
                        pitch: 62,
                        velocity: 100,
                        lyric: Some("ん".into()),
                    },
                ],
            }),
        );
        song.tracks.push(Track {
            id: 1,
            name: "Vocal".into(),
            source: InstrumentSource::Vocal {
                speaker_id: 3,
                style_name: "ノーマル".into(),
            },
            clips: vec![Clip {
                id: 1,
                name: "こんにちは".into(),
                start_beat: 0.0,
                length_beats: 16.0,
                content_id: cid,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
            }],
            ..Track::default()
        });
        // Phase 6 review: ensure_ids() を load 内で呼ぶようになったので、
        // assert する original 側でも同じ normalization を適用する (= idempotent
        // なので両方かけると 1 回 + 0 回 = 同じ最終状態)。 元データは
        // `next_track_id == track.id` という不変条件違反の状態で構築されて
        // いて、 ensure_ids が `next_id > max_existing_id` を強制する仕様
        // どおりに修正する。
        song.ensure_ids();
        // v20: load() drains the legacy per-clip `name` into
        // `clip_content_names` via `ensure_clip_contents`. Apply the same
        // normalization to the original so the round-trip assert compares
        // like-with-like (idempotent — load runs it once more on read).
        song.ensure_clip_contents();
        save(&path, &song).unwrap();
        assert_eq!(load(&path).unwrap(), song);
    }

    #[test]
    fn load_v6_clip_content_struct_form_deserializes_as_midi_variant() {
        // v6 saves stored `ClipContent` as a flat struct
        // `{ "notes": [...] }`. v7 promotes `ClipContent` to an enum
        // `Midi(MidiContent) | Audio(AudioContent)` with
        // `#[serde(untagged)]` so the legacy struct form deserialises
        // straight into `Midi(MidiContent { notes })`.
        let dir = tempdir().unwrap();
        let path = dir.path().join("v6.daw");
        let v6_json = r#"{
            "version": 6,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "next_track_id": 2,
                "next_content_id": 2,
                "clip_contents": {
                    "1": {
                        "notes": [
                            {"start_beat": 0.0, "duration_beats": 1.0, "pitch": 60, "velocity": 100}
                        ]
                    }
                },
                "tracks": [
                    {
                        "id": 1,
                        "name": "Lead",
                        "volume": 1.0,
                        "pan": 0.0,
                        "next_clip_id": 2,
                        "clips": [
                            {
                                "id": 1,
                                "name": "C",
                                "start_beat": 0.0,
                                "length_beats": 4.0,
                                "content_id": 1
                            }
                        ]
                    }
                ]
            }
        }"#;
        fs::write(&path, v6_json).unwrap();
        let song = load(&path).expect("v6 must forward-migrate to v7");
        let content = song
            .clip_contents
            .get(&1)
            .expect("v6 content_id 1 must round-trip");
        let notes = content
            .notes()
            .expect("legacy struct form must deserialise as Midi variant");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 60);
        // audio_sources defaults to empty for v6 files.
        assert!(song.audio_sources.is_empty());
        assert!(song.next_audio_source_id >= 1);
    }

    #[test]
    fn load_v5_migrates_clip_notes_to_clip_contents() {
        // v5 saves stored notes per-`Clip` directly. After load, the
        // legacy `notes` vector must be drained into `clip_contents` and
        // a fresh `content_id` allocated.
        let dir = tempdir().unwrap();
        let path = dir.path().join("v5.daw");
        let v5_json = r#"{
            "version": 5,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": [
                    {
                        "id": 1,
                        "name": "Lead",
                        "volume": 1.0,
                        "pan": 0.0,
                        "next_clip_id": 2,
                        "clips": [
                            {
                                "id": 1,
                                "name": "C",
                                "start_beat": 0.0,
                                "length_beats": 4.0,
                                "notes": [
                                    {"start_beat": 0.0, "duration_beats": 1.0, "pitch": 60, "velocity": 100}
                                ]
                            }
                        ]
                    }
                ]
            }
        }"#;
        fs::write(&path, v5_json).unwrap();
        let song = load(&path).expect("v5 must forward-migrate");
        let clip = &song.tracks[0].clips[0];
        assert_ne!(clip.content_id, 0, "ensure_clip_contents must allocate");
        assert!(
            clip.notes.is_empty(),
            "legacy notes must be drained on migration"
        );
        let content = song
            .clip_contents
            .get(&clip.content_id)
            .expect("content_id must have an entry after migration");
        let notes = content.notes().expect("legacy Clip.notes must migrate to Midi variant");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 60);
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        save(&path, &Song::default()).unwrap();
        assert!(path.exists());
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn save_overwrites_existing_file_atomically() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        let mut song = Song::default();
        save(&path, &song).unwrap();

        song.bpm = 140.0;
        save(&path, &song).unwrap();

        assert_eq!(load(&path).unwrap().bpm, 140.0);
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn load_rejects_newer_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("future.daw");
        let future = ProjectFile {
            version: CURRENT_VERSION + 1,
            song: Song::default(),
        };
        fs::write(&path, serde_json::to_string(&future).unwrap()).unwrap();

        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("newer"), "unexpected error: {err}");
    }

    #[test]
    fn load_accepts_v4_with_default_routing_fields() {
        // v4 saves had no `kind` / `parent_group_id` /
        // `reported_latency_samples` keys on each `Track`. Loading must
        // succeed and fill those fields with their serde defaults
        // (Audio / None / 0).
        let dir = tempdir().unwrap();
        let path = dir.path().join("v4.daw");
        let v4_json = r#"{
            "version": 4,
            "song": {
                "bpm": 140.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": [
                    {
                        "id": 1,
                        "name": "Lead",
                        "volume": 0.85,
                        "pan": 0.0,
                        "next_clip_id": 1
                    }
                ]
            }
        }"#;
        fs::write(&path, v4_json).unwrap();
        let song = load(&path).expect("v4 must forward-migrate");
        assert_eq!(song.bpm, 140.0);
        assert_eq!(song.tracks.len(), 1);
        let t = &song.tracks[0];
        assert_eq!(t.parent_group_id, None);
        assert_eq!(t.reported_latency_samples, 0);
    }

    #[test]
    fn load_accepts_v18_with_default_group_transform() {
        // v18 saves had no `group_transform` key on each `Track`. Loading
        // must succeed and fill it with the serde default (`None`), proving
        // the v19 field is forward-compatible (enum 末尾追加 = forward-migrate
        // のみ)。See `docs/plan_tachie_group_transform.md` §4.5.
        let dir = tempdir().unwrap();
        let path = dir.path().join("v18.daw");
        let v18_json = r#"{
            "version": 18,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": [
                    {
                        "id": 1,
                        "name": "Char A",
                        "volume": 1.0,
                        "pan": 0.0,
                        "next_clip_id": 1,
                        "color": [0.5, 0.5, 0.5]
                    }
                ]
            }
        }"#;
        fs::write(&path, v18_json).unwrap();
        let song = load(&path).expect("v18 must forward-migrate");
        assert_eq!(song.tracks.len(), 1);
        assert_eq!(song.tracks[0].group_transform, None);
    }

    #[test]
    fn load_rejects_legacy_row_based_version() {
        // Version 1 was the row-based format; we no longer support it.
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.daw");
        fs::write(
            &path,
            r#"{"version":1,"song":{"bpm":120.0,"time_sig":[4,4],"length_beats":64.0}}"#,
        )
        .unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(err.contains("retired"), "unexpected error: {err}");
    }

    #[test]
    fn load_rejects_invalid_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("invalid.daw");
        fs::write(&path, "not valid json {").unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn load_fails_for_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.daw");
        assert!(load(&path).is_err());
    }

    #[test]
    fn tmp_path_appends_tmp_suffix() {
        assert_eq!(
            tmp_path(Path::new("project.daw")),
            PathBuf::from("project.daw.tmp")
        );
        assert_eq!(tmp_path(Path::new("noext")), PathBuf::from("noext.tmp"));
    }
}
