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

    let project = ProjectFile {
        version: CURRENT_VERSION,
        song: song.clone(),
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
    Ok(project.song)
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Clip, InstrumentSource, Note, Track};
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
        let song = Song {
            tracks: vec![Track {
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
                }],
                ..Track::default()
            }],
            ..Song::default()
        };
        save(&path, &song).unwrap();
        assert_eq!(load(&path).unwrap(), song);
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
