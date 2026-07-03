use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::model::{CURRENT_VERSION, ProjectFile, Song, ViewState};

/// Result of `load_project`: the normalized song plus the optional GUI view
/// state. `view` is `None` for legacy files / files saved without
/// view state — callers fall back to their default (fit-to-content) behavior.
#[derive(Debug, Clone)]
pub struct LoadedProject {
    pub song: Song,
    pub view: Option<ViewState>,
}

/// Oldest project-file version `load` will accept. Versions below this
/// (currently `1` = the retired row-based format) are rejected with a
/// "re-create the project" error. Versions in `[MIN_LOADABLE_VERSION,
/// CURRENT_VERSION)` are accepted and forward-migrated via
/// `#[serde(default)]` on any new fields — fine because every field
/// added since v2 has a sensible default and no reinterpretation of
/// existing data.
const MIN_LOADABLE_VERSION: u32 = 2;

/// v26 で `builtin.video.subtitle` device が text overlay の表示ゲートになった
/// (`docs/plan_voicevox_talk.md` §6)。`migrate_text_overlay_to_subtitle_device` は
/// このバージョン未満の保存ファイルにだけ適用する。
const SUBTITLE_DEVICE_VERSION: u32 = 26;

/// v27 で `Clip.muted` / `Note.muted` が mute の SSoT になった。
/// `migrate_per_event_mute_to_clip_mute` はこのバージョン未満の保存ファイルにだけ適用する。
const CLIP_MUTE_VERSION: u32 = 27;

/// Save without GUI view state (legacy callers / tests / headless `--script`).
/// Delegates to `save_project` with `view = None`.
pub fn save(path: impl AsRef<Path>, song: &Song) -> Result<()> {
    save_project(path, song, None)
}

/// Save a project, optionally embedding GUI view state. `view` is
/// written as `ProjectFile.view` (a sibling of `song`), so the Song / IPC
/// layout is untouched. Atomic write via tmp → rename.
pub fn save_project(
    path: impl AsRef<Path>,
    song: &Song,
    view: Option<&ViewState>,
) -> Result<()> {
    let path = path.as_ref();
    let tmp = tmp_path(path);

    // Normalize for save (GC orphan content / audio / video / image
    // source-pool entries) so disk files stay tidy. Working on a clone —
    // caller's in-memory Song is not mutated.
    let mut song = song.clone();
    song.normalize_for_save();
    let project = ProjectFile {
        version: CURRENT_VERSION,
        song,
        view: view.cloned(),
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

/// 旧 `InstrumentSource::Vocal { speaker_id, style_name }`
/// (= JSON object `{"Vocal": {...}}`) を unit `Vocal` (= JSON string
/// `"Vocal"`) へ移行し、 旧トラック声をそのトラックの全 clip に焼き込む。
/// 声は per-clip (`Clip::speaker_id` 等) が SSoT になったため。
///
/// - 既に `speaker_id` を持つ clip (= 新形式) は尊重して上書きしない。
/// - `singer_name` は旧データに無いので空のまま (= `/singers` 取得後に
///   app 側が speaker_id から逆引きして埋める)。
/// - 新形式ファイル (source が既に string `"Vocal"` 等) は no-op。
fn migrate_vocal_source_to_clips(value: &mut serde_json::Value) {
    let Some(tracks) = value
        .get_mut("song")
        .and_then(|s| s.get_mut("tracks"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    for track in tracks {
        // 旧形式判定: source == { "Vocal": { speaker_id, style_name } }。
        let Some(vocal) = track
            .get("source")
            .and_then(|s| s.get("Vocal"))
            .filter(|v| v.is_object())
        else {
            continue;
        };
        let old_speaker = vocal
            .get("speaker_id")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let old_style = vocal
            .get("style_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        // source を unit "Vocal" に置換。
        track["source"] = serde_json::Value::String("Vocal".to_string());
        // 全 clip へ焼き込み (新形式 clip は触らない)。
        let Some(clips) = track
            .get_mut("clips")
            .and_then(serde_json::Value::as_array_mut)
        else {
            continue;
        };
        for clip in clips {
            let Some(obj) = clip.as_object_mut() else {
                continue;
            };
            if obj.contains_key("speaker_id") {
                continue;
            }
            if old_speaker != 0 {
                obj.insert("speaker_id".to_string(), serde_json::json!(old_speaker));
            }
            if !old_style.is_empty() {
                obj.insert("style_name".to_string(), serde_json::json!(old_style));
            }
        }
    }
}

/// (v26) device-gated text overlay 移行 (`docs/plan_voicevox_talk.md` §6)。
/// v25 以前は `ClipContent::Text` がトラック非依存で常時 overlay 表示されていた。
/// v26 で `builtin.video.subtitle` device が表示ゲートになったため、旧プロジェクトの
/// 「Text clip を 1 つ以上持つトラック」へ字幕デバイスを auto-insert して見た目を保つ。
/// idempotent (既に字幕デバイスを持つトラックは no-op)。**version-gate 前提** — v26 以降の
/// プロジェクトには適用しない (= ユーザーが字幕デバイスを抜いた「喋るが映さない」
/// トラックを誤って表示化しない)。caller が `project.version < SUBTITLE_DEVICE_VERSION` で gate。
fn migrate_text_overlay_to_subtitle_device(song: &mut Song) {
    use crate::model::ClipContent;
    // clip_contents は tracks と別 borrow になるので、Text な content_id を先に集める。
    let text_content_ids: std::collections::HashSet<crate::model::ContentId> = song
        .clip_contents
        .iter()
        .filter(|(_, c)| matches!(c, ClipContent::Text(_)))
        .map(|(id, _)| *id)
        .collect();
    for track in &mut song.tracks {
        let has_text_clip = track
            .clips
            .iter()
            .any(|c| text_content_ids.contains(&c.content_id));
        if has_text_clip && !track.has_subtitle_device() {
            track.devices.push(crate::model::PluginInstance::with_ports(
                crate::plugin_db::SUBTITLE_ID.to_string(),
                crate::plugin_format::PluginFormat::Builtin,
                crate::port_config::PortConfig {
                    has_video_input: true,
                    has_video_output: true,
                    ..Default::default()
                },
            ));
        }
    }
}

/// (v27) per-event mute → clip-level mute 統合。v26 以前は clip の mute を
/// audio / image / video / text event の `muted` フラグ (inspector の "Mute" トグルが clip 内
/// 全 event を mute) で表現していた。v27 で `Clip.muted` を SSoT に一本化したので、旧プロジェクトの
/// 「event が muted な content」を `Clip.muted = true` へ畳み込み、event 側の `muted` は false に戻す。
/// 共有 content (linked clip) は 1 度 false 化すれば全 clip に効く。idempotent。**version-gate 前提** —
/// v27 以降のプロジェクトの event.muted は触らない (将来 per-event mute UI 用に温存)。caller が
/// `project.version < CLIP_MUTE_VERSION` で gate。
fn migrate_per_event_mute_to_clip_mute(song: &mut Song) {
    use crate::model::{ClipContent, ContentId};
    // content_id 単位で「event が 1 つでも muted だったか」を判定しつつ event.muted を false に戻す。
    let mut muted_contents: std::collections::HashSet<ContentId> = std::collections::HashSet::new();
    for (id, content) in song.clip_contents.iter_mut() {
        let any_muted = match content {
            ClipContent::Audio(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Image(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Video(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Text(c) => {
                let m = c.events.iter().any(|e| e.muted);
                c.events.iter_mut().for_each(|e| e.muted = false);
                m
            }
            ClipContent::Midi(_) | ClipContent::Automation(_) => false,
        };
        if any_muted {
            muted_contents.insert(*id);
        }
    }
    if muted_contents.is_empty() {
        return;
    }
    for track in &mut song.tracks {
        for clip in &mut track.clips {
            if muted_contents.contains(&clip.content_id) {
                clip.muted = true;
            }
        }
    }
}

/// Load just the song (legacy callers / tests / headless `--script`).
/// Delegates to `load_project` and drops the view state.
pub fn load(path: impl AsRef<Path>) -> Result<Song> {
    Ok(load_project(path)?.song)
}

/// Load a project including optional GUI view state. The returned
/// `song` is fully normalized (same as `load`); `view` is `None` for legacy
/// files / files saved without view state.
pub fn load_project(path: impl AsRef<Path>) -> Result<LoadedProject> {
    let path = path.as_ref();
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    // per-clip 声移行: 旧 `InstrumentSource::Vocal { speaker_id,
    // style_name }` (JSON object) を unit `Vocal` (JSON string) に変換し、
    // 旧トラック声をそのトラックの全 clip へ焼き込んでから deserialize する
    // (= 本 deserialize は unit `Vocal` だけ見れば良い、 声は per-clip が SSoT)。
    let mut value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse project JSON from {}", path.display()))?;
    migrate_vocal_source_to_clips(&mut value);
    let project: ProjectFile = serde_json::from_value(value)
        .with_context(|| format!("failed to deserialize project from {}", path.display()))?;
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
    let view = project.view;
    let mut song = project.song;
    // 各 migration は「その挙動が導入されたバージョン」未満のファイルにだけ適用する。
    // `< CURRENT_VERSION` で gate すると、version を bump するたびに一つ前のバージョンで
    // 保存されたファイルへ migration が誤再適用される (例: v26 で字幕デバイスを意図的に
    // 抜いた「喋るが映さない」トラックが load のたびに再表示化される)。
    //
    // (v26) 旧プロジェクト (= device-gated text overlay 以前) の Text 持ちトラックへ
    // 字幕デバイスを補い、表示を保つ。normalize の前に挿すことで、追加 device も
    // 他 device と同じ正規化 (aux migration 等) を通る。
    if project.version < SUBTITLE_DEVICE_VERSION {
        migrate_text_overlay_to_subtitle_device(&mut song);
    }
    // (v27) 旧 per-event mute を `Clip.muted` へ畳み込む。v27 以降の `event.muted` は
    // 温存 (将来の per-event mute UI 用)。
    if project.version < CLIP_MUTE_VERSION {
        migrate_per_event_mute_to_clip_mute(&mut song);
    }
    // Re-establish every invariant the codebase assumes about a loaded
    // song in one SSoT call: value-range sanity (bpm/time_sig/length/loop
    // — defends downstream divisors against 0/NaN from corrupt files),
    // v5→v6/v6→v7 content & source-id migration, stable id assignment
    // (track/clip/parent_group_id consistency), and the scale-change /
    // automation-point sort invariants. Idempotent — safe if a caller
    // (e.g. `daw_gui::app::open_project`) re-runs it.
    song.normalize_after_load();
    Ok(LoadedProject { song, view })
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".tmp");
    PathBuf::from(os)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Clip, ClipContent, InstrumentSource, MidiContent, Note, TextContent, TextEvent, Track,
    };
    use crate::model::{AudioEditorViewState, ClipKey, FollowMode, PianoRollViewState, ViewState};
    use tempfile::tempdir;

    /// per-clip view + globals を含む代表的な `ViewState`。
    fn sample_view_state() -> ViewState {
        ViewState {
            arrange_zoom_x: 37.5,
            arrange_scroll_beat: 12.0,
            arrange_track_top: 48.0,
            arrange_track_row_h: 72.0,
            arrange_header_w: 200.0,
            track_row_overrides: [(1u32, 64u16), (3, 120)].into_iter().collect(),
            expanded_automation_tracks: vec![2, 5],
            master_row_automation_expanded: true,
            arrange_follow: FollowMode::Page,
            arrange_snap_enabled: false,
            arrange_snap_choice: 4,
            pianoroll_snap_enabled: true,
            pianoroll_snap_choice: 2,
            piano_roll_fold: true,
            snap_on_draw: true,
            snap_live_input: false,
            bottom_panel: 1,
            selected_clip: Some(ClipKey { track_id: 2, clip_id: 1 }),
            selected_clips: vec![
                ClipKey { track_id: 1, clip_id: 1 },
                ClipKey { track_id: 2, clip_id: 1 },
            ],
            piano_roll_views: vec![
                (
                    ClipKey { track_id: 1, clip_id: 1 },
                    PianoRollViewState { zoom_x: 120.0, zoom_y: 22.0, top_pitch: 72, scroll_beat: 3.0 },
                ),
                (
                    ClipKey { track_id: 2, clip_id: 1 },
                    PianoRollViewState { zoom_x: 16.0, zoom_y: 8.0, top_pitch: 96, scroll_beat: 0.0 },
                ),
            ],
            audio_editor_views: vec![(
                ClipKey { track_id: 3, clip_id: 7 },
                AudioEditorViewState { start_beat: 1.5, len_beats: 8.0 },
            )],
        }
    }

    /// ViewState は save_project → load_project で完全に往復する。
    #[test]
    fn save_project_roundtrips_view_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("with_view.daw");
        let view = sample_view_state();
        save_project(&path, &Song::default(), Some(&view)).unwrap();
        let loaded = load_project(&path).unwrap();
        assert_eq!(
            loaded.view.as_ref(),
            Some(&view),
            "view state が save/load で完全往復する"
        );
    }

    /// `view` キーを持たない旧ファイルは `view == None` で読め、
    /// 従来挙動 (fit-to-content) にフォールバックできる。
    #[test]
    fn legacy_file_loads_with_none_view() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.daw");
        write_project_with_version(&path, &Song::default(), CURRENT_VERSION);
        let loaded = load_project(&path).unwrap();
        assert_eq!(loaded.view, None);
    }

    /// 旧 `save` (= `save_project(.., None)` への委譲) は view を書かない。
    #[test]
    fn plain_save_writes_no_view() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("no_view.daw");
        save(&path, &Song::default()).unwrap();
        assert_eq!(load_project(&path).unwrap().view, None);
    }

    /// version `v` の project file を `path` に書く (song は Rust で組んで serialize)。
    /// device-gated text overlay 移行 (v26) の version-gate を試すための helper。
    fn write_project_with_version(path: &Path, song: &Song, v: u32) {
        let song_value = serde_json::to_value(song).unwrap();
        let project = serde_json::json!({ "version": v, "song": song_value });
        std::fs::write(path, serde_json::to_string(&project).unwrap()).unwrap();
    }

    /// Text clip 1 つを持つ非 vocal トラック 1 本だけの song (字幕デバイス無し)。
    fn song_with_one_text_clip() -> Song {
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Text(TextContent {
                events: vec![TextEvent {
                    text: "こんにちは".into(),
                    event_length_beats: 8.0,
                    ..TextEvent::default()
                }],
            }),
        );
        let mut track = Track {
            id: 1,
            name: "Subs".into(),
            ..Track::default()
        };
        track.clips.push(Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: cid,
            ..Clip::default()
        });
        song.tracks.push(track);
        song
    }

    /// (v27): event 単位 mute だった clip を `Clip.muted` へ畳み込み、
    /// event 側の `muted` を false に戻す。
    #[test]
    fn migrate_per_event_mute_folds_into_clip_and_clears_event() {
        let mut song = song_with_one_text_clip();
        let cid = song.tracks[0].clips[0].content_id;
        if let Some(ClipContent::Text(t)) = song.clip_contents.get_mut(&cid) {
            t.events[0].muted = true;
        }
        migrate_per_event_mute_to_clip_mute(&mut song);
        assert!(
            song.tracks[0].clips[0].muted,
            "event が muted な clip は Clip.muted = true に畳み込まれる"
        );
        let Some(ClipContent::Text(t)) = song.clip_contents.get(&cid) else {
            panic!("expected Text content");
        };
        assert!(!t.events[0].muted, "畳み込み後は event.muted = false に戻る");
    }

    /// 元々 mute されていない clip は migration で変化しない。
    #[test]
    fn migrate_per_event_mute_leaves_unmuted_clip_untouched() {
        let mut song = song_with_one_text_clip();
        migrate_per_event_mute_to_clip_mute(&mut song);
        assert!(!song.tracks[0].clips[0].muted);
    }

    #[test]
    fn migrate_v25_text_overlay_adds_subtitle_device() {
        // v25 (= device-gated text 以前) は Text overlay がトラック非依存で常時表示。
        // load 時に Text 持ちトラックへ字幕デバイスが補われ、表示が保たれる。
        let dir = tempdir().unwrap();
        let path = dir.path().join("old.daw");
        write_project_with_version(&path, &song_with_one_text_clip(), 25);
        let song = load(&path).unwrap();
        assert!(
            song.tracks[0].has_subtitle_device(),
            "v25 の Text 持ちトラックへ字幕デバイスが auto-insert される"
        );
    }

    #[test]
    fn migrate_skips_when_subtitle_device_already_present() {
        // 既に字幕デバイスを持つ v25 トラックは二重挿入しない (idempotent)。
        let dir = tempdir().unwrap();
        let path = dir.path().join("old_with_dev.daw");
        let mut song = song_with_one_text_clip();
        song.tracks[0]
            .devices
            .push(crate::model::PluginInstance::with_ports(
                crate::plugin_db::SUBTITLE_ID.to_string(),
                crate::plugin_format::PluginFormat::Builtin,
                crate::port_config::PortConfig {
                    has_video_input: true,
                    has_video_output: true,
                    ..Default::default()
                },
            ));
        write_project_with_version(&path, &song, 25);
        let loaded = load(&path).unwrap();
        let n = loaded.tracks[0]
            .devices
            .iter()
            .filter(|d| d.plugin_id == crate::plugin_db::SUBTITLE_ID)
            .count();
        assert_eq!(n, 1, "字幕デバイスは二重挿入されない");
    }

    /// (talk) 実機 end-to-end 検証用の fixture 生成 (`docs/plan_voicevox_talk.md` §7)。
    /// VOICEVOX device 付きトラック + 読み上げテキストの Text clip を 1 本持つ .daw を
    /// `target/talk_fixture.daw` に書く。`daw_gui --script` がこれを load → exportWav し、
    /// 出力 WAV の非無音判定で「full pipeline で実際に喋る」を headless 検証する。
    /// 通常 test では不要なので `#[ignore]` (= 明示実行で再生成)。
    #[test]
    #[ignore = "fixture generator: writes target/talk_fixture.daw"]
    fn gen_talk_fixture() {
        use crate::model::{PluginInstance, TextContent, TextEvent};
        use crate::port_config::PortConfig;
        let mut song = Song { bpm: 120.0, length_beats: 16.0, ..Default::default() };
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Text(TextContent {
                events: vec![TextEvent {
                    text: "こんにちは。これは読み上げのテストです。".into(),
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 8.0,
                    ..TextEvent::default()
                }],
            }),
        );
        let mut track = Track {
            id: 1,
            name: "Talk".into(),
            ..Track::default()
        };
        // VOICEVOX builtin (instrument: note_in → audio_out)。= is_voicevox_vocal。
        track.devices.push(PluginInstance::with_ports(
            crate::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
            crate::plugin_format::PluginFormat::Builtin,
            PortConfig {
                has_note_input: true,
                has_audio_output: true,
                ..Default::default()
            },
        ));
        track.clips.push(Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 8.0,
            content_id: cid,
            speaker_id: crate::voicevox::DEFAULT_TALK_SPEAKER_ID,
            ..Clip::default()
        });
        song.tracks.push(track);
        let path = Path::new(r"F:\dev\daw_01\target\talk_fixture.daw");
        save(path, &song).unwrap();
        eprintln!("wrote talk fixture: {}", path.display());
    }

    #[test]
    fn no_migration_for_current_version_text_clip() {
        // v26 (= CURRENT) の Text 持ちトラックは migration 対象外。字幕デバイスを
        // 抜いた「喋るが映さない」トラックを誤って表示化しない。
        let dir = tempdir().unwrap();
        let path = dir.path().join("new.daw");
        write_project_with_version(&path, &song_with_one_text_clip(), CURRENT_VERSION);
        let song = load(&path).unwrap();
        assert!(
            !song.tracks[0].has_subtitle_device(),
            "新規 (v26) の Text 持ちトラックには字幕デバイスを補わない"
        );
    }

    #[test]
    fn save_and_load_default_song() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("project.daw");
        let song = Song::default();
        save(&path, &song).unwrap();
        let mut loaded = load(&path).unwrap();
        // v24: default の project_id は 0 (未採番)。save は 0 のまま書き、
        // load が `ensure_project_id` で採番するので loaded.project_id != 0。それ以外は
        // 一致するはず。project_id を 0 に戻してから残りを比較する。
        assert_ne!(loaded.project_id, 0, "load で project_id が採番される");
        loaded.project_id = 0;
        assert_eq!(loaded, song);
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
                        muted: false,
                    },
                    Note {
                        start_beat: 1.0,
                        duration_beats: 0.5,
                        pitch: 62,
                        velocity: 100,
                        lyric: Some("ん".into()),
                        muted: false,
                    },
                ],
            }),
        );
        song.tracks.push(Track {
            id: 1,
            name: "Vocal".into(),
            source: InstrumentSource::Vocal,
            clips: vec![Clip {
                id: 1,
                name: "こんにちは".into(),
                start_beat: 0.0,
                length_beats: 16.0,
                content_id: cid,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
                muted: false,
                speaker_id: 3061,
                singer_name: "中国うさぎ".into(),
                style_name: "ノーマル".into(),
                talk: None,
            }],
            ..Track::default()
        });
        // load() runs `normalize_after_load` (ensure_ids + ensure_clip_
        // contents + sanitize + sorts). Apply the same normalization to
        // the original so the round-trip assert compares like-with-like
        // (idempotent — load runs it once more on read).
        song.normalize_after_load();
        save(&path, &song).unwrap();
        assert_eq!(load(&path).unwrap(), song);
    }

    #[test]
    fn migrate_old_vocal_source_bakes_voice_into_clips() {
        // 旧形式: track.source = {"Vocal": {speaker_id, style_name}}、
        // clip は声フィールド無し。 migration は source を unit "Vocal" に変換し、
        // 旧トラック声を全 clip へ焼き込む (新形式 clip = 既に speaker_id 持ちは尊重)。
        let mut value = serde_json::json!({
            "version": 2,
            "song": {
                "tracks": [{
                    "source": { "Vocal": { "speaker_id": 3061, "style_name": "へろへろ" } },
                    "clips": [
                        { "id": 1 },
                        { "id": 2, "speaker_id": 7, "style_name": "あまあま" }
                    ]
                }]
            }
        });
        migrate_vocal_source_to_clips(&mut value);
        let track = &value["song"]["tracks"][0];
        // source は unit "Vocal" (JSON string) に。
        assert_eq!(track["source"], serde_json::json!("Vocal"));
        // clip 1 (声無し): 旧トラック声を焼き込み。
        assert_eq!(track["clips"][0]["speaker_id"], 3061);
        assert_eq!(track["clips"][0]["style_name"], "へろへろ");
        // clip 2 (既に speaker_id 持ち): 尊重して上書きしない。
        assert_eq!(track["clips"][1]["speaker_id"], 7);
        assert_eq!(track["clips"][1]["style_name"], "あまあま");
    }

    #[test]
    fn migrate_is_noop_for_new_unit_vocal_source() {
        // 新形式 (source が既に string "Vocal") は no-op。
        let mut value = serde_json::json!({
            "version": 2,
            "song": { "tracks": [{ "source": "Vocal", "clips": [{ "id": 1 }] }] }
        });
        let before = value.clone();
        migrate_vocal_source_to_clips(&mut value);
        assert_eq!(value, before);
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
            view: None,
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
    fn load_v23_assigns_project_id_and_persists() {
        // v23 saves had no `project_id`. Loading must succeed and assign a fresh
        // non-zero id (`ensure_project_id`), and re-saving must persist it so the
        // next load returns the SAME id (`docs/plan_fixme_33_clipboard.md`).
        let dir = tempdir().unwrap();
        let path = dir.path().join("v23.daw");
        let v23_json = r#"{
            "version": 23,
            "song": {
                "bpm": 120.0,
                "time_sig": [4, 4],
                "length_beats": 64.0,
                "tracks": []
            }
        }"#;
        fs::write(&path, v23_json).unwrap();
        let song = load(&path).expect("v23 must forward-migrate");
        assert_ne!(song.project_id, 0, "load で project_id が採番される");
        let id = song.project_id;
        // re-save → re-load で同一 id が保たれる (ensure_project_id は非 0 を上書きしない)。
        save(&path, &song).unwrap();
        let reloaded = load(&path).unwrap();
        assert_eq!(reloaded.project_id, id, "save/load で project_id が保たれる");
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
