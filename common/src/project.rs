use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::{CURRENT_VERSION, ProjectFile, Song};

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
    if project.version < CURRENT_VERSION {
        anyhow::bail!(
            "project file {} uses legacy version {}; the row-based format \
             was retired in version {}. Re-create the project in the \
             current free-time-note format.",
            path.display(),
            project.version,
            CURRENT_VERSION
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
        assert!(err.contains("legacy"), "unexpected error: {err}");
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
