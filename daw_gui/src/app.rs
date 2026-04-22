use std::path::{Path, PathBuf};

use common::model::{Clip, InstrumentSource, Note, NoteEvent, Row, Song, Track};
use common::protocol::MainToChild;
use tokio::sync::mpsc::UnboundedSender;
use vizia::prelude::*;

#[derive(Lens)]
pub struct AppData {
    pub song: Song,
    pub file_path: Option<PathBuf>,
    pub cursor_row: u32,
    pub cursor_track: u32,
    /// Cached textual rendering of the tracker grid. Recomputed whenever
    /// `song`, `cursor_row`, or `cursor_track` change via `refresh_tracker_text`.
    pub tracker_text: String,
    /// Template note used when placing into an empty cell (sing_like_coding
    /// style). Updated whenever the user edits a NoteOn cell.
    pub last_note: Note,
    /// Tracks whether playback is currently active so PlayToggle can flip.
    /// Flipped only by explicit user Play/Stop; auto-stop in plugin_host is
    /// not yet mirrored back here.
    pub is_playing: bool,
    #[lens(ignore)]
    pub audio_tx: Option<UnboundedSender<MainToChild>>,
    #[lens(ignore)]
    pub plugin_tx: Option<UnboundedSender<MainToChild>>,
}

impl AppData {
    pub fn new(
        audio_tx: UnboundedSender<MainToChild>,
        plugin_tx: UnboundedSender<MainToChild>,
    ) -> Self {
        let song = Song::default();
        let tracker_text = crate::view::arrangement::render_tracker_text(&song, 0, 0);
        Self {
            song,
            file_path: None,
            cursor_row: 0,
            cursor_track: 0,
            tracker_text,
            last_note: Note {
                key: 60,
                velocity: 100,
            },
            is_playing: false,
            audio_tx: Some(audio_tx),
            plugin_tx: Some(plugin_tx),
        }
    }

    fn refresh_tracker_text(&mut self) {
        self.tracker_text = crate::view::arrangement::render_tracker_text(
            &self.song,
            self.cursor_row,
            self.cursor_track,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppEvent {
    New,
    Open,
    Save,
    SaveAs,
    Play,
    Stop,
    PlayToggle,
    AddVocalTrack,
    RemoveLastTrack,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    NoteOff,
    NoteClear,
    TransposeSemi(i8),
    TransposeOctave(i8),
}

impl Model for AppData {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _| {
            if let WindowEvent::WindowClose = window_event {
                tracing::info!("window close requested");
            }
        });
        event.map(|app_event, _| {
            let mut dirty = true;
            match app_event {
                AppEvent::New => self.action_new(),
                AppEvent::Open => self.action_open(),
                AppEvent::Save => {
                    self.action_save();
                    dirty = false;
                }
                AppEvent::SaveAs => {
                    self.action_save_as();
                    dirty = false;
                }
                AppEvent::Play => {
                    self.play();
                    dirty = false;
                }
                AppEvent::Stop => {
                    self.stop();
                    dirty = false;
                }
                AppEvent::PlayToggle => {
                    if self.is_playing {
                        self.stop();
                    } else {
                        self.play();
                    }
                    dirty = false;
                }
                AppEvent::AddVocalTrack => self.action_add_vocal_track(),
                AppEvent::RemoveLastTrack => self.action_remove_last_track(),
                AppEvent::CursorLeft => self.move_cursor_track(-1),
                AppEvent::CursorRight => self.move_cursor_track(1),
                AppEvent::CursorUp => self.move_cursor_row(-1),
                AppEvent::CursorDown => self.move_cursor_row(1),
                AppEvent::NoteOff => self.edit_cell(|row| row.note = Some(NoteEvent::Off)),
                AppEvent::NoteClear => self.edit_cell(|row| row.note = None),
                AppEvent::TransposeSemi(d) => self.apply_transpose(*d as i16),
                AppEvent::TransposeOctave(d) => self.apply_transpose(*d as i16 * 12),
            }
            if dirty {
                self.refresh_tracker_text();
            }
        });
    }
}

impl AppData {
    fn send_audio(&self, msg: MainToChild) {
        tracing::info!(?msg, "sending to audio");
        let Some(tx) = self.audio_tx.as_ref() else {
            tracing::warn!("audio sender is not configured");
            return;
        };
        if let Err(e) = tx.send(msg) {
            tracing::error!(error = %e, "failed to enqueue audio command");
        }
    }

    fn send_plugin(&self, msg: MainToChild) {
        tracing::info!(?msg, "sending to plugin_host");
        let Some(tx) = self.plugin_tx.as_ref() else {
            tracing::warn!("plugin sender is not configured");
            return;
        };
        if let Err(e) = tx.send(msg) {
            tracing::error!(error = %e, "failed to enqueue plugin command");
        }
    }

    fn action_new(&mut self) {
        self.song = Song::default();
        self.file_path = None;
        tracing::info!("new project");
    }

    fn action_open(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("daw", &["daw"])
            .pick_file()
        else {
            return;
        };
        match common::project::load(&path) {
            Ok(song) => {
                tracing::info!(path = %path.display(), "loaded project");
                self.song = song;
                self.file_path = Some(path);
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project")
            }
        }
    }

    fn action_save(&mut self) {
        if let Some(path) = self.file_path.clone() {
            self.save_to(&path);
        } else {
            self.action_save_as();
        }
    }

    fn action_save_as(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("daw", &["daw"])
            .save_file()
        else {
            return;
        };
        if self.save_to(&path) {
            self.file_path = Some(path);
        }
    }

    fn action_add_vocal_track(&mut self) {
        let index = self.song.tracks.len() + 1;
        let track = Track {
            name: format!("Track {index}"),
            source: InstrumentSource::Vocal {
                speaker_id: 3,
                style_name: "ノーマル".into(),
            },
            fx_chain: vec![],
            volume: 1.0,
            pan: 0.0,
            clips: vec![demo_clip()],
        };
        self.song.tracks.push(track);
        tracing::info!(index, "added vocal track");
    }

    fn action_remove_last_track(&mut self) {
        if let Some(track) = self.song.tracks.pop() {
            tracing::info!(name = %track.name, "removed last track");
        }
        self.clamp_cursor();
    }

    fn move_cursor_track(&mut self, delta: i32) {
        let max = self.song.tracks.len().saturating_sub(1) as i64;
        let next = (self.cursor_track as i64 + delta as i64).clamp(0, max.max(0));
        self.cursor_track = next as u32;
    }

    fn move_cursor_row(&mut self, delta: i32) {
        let max = self
            .song
            .tracks
            .get(self.cursor_track as usize)
            .and_then(|t| t.clips.first())
            .map(|c| c.rows.len().saturating_sub(1))
            .unwrap_or(0) as i64;
        let next = (self.cursor_row as i64 + delta as i64).clamp(0, max.max(0));
        self.cursor_row = next as u32;
    }

    fn play(&mut self) {
        self.send_plugin(MainToChild::LoadSong(self.song.clone()));
        self.send_audio(MainToChild::Play);
        self.send_plugin(MainToChild::Play);
        self.is_playing = true;
    }

    fn stop(&mut self) {
        self.send_audio(MainToChild::Stop);
        self.send_plugin(MainToChild::Stop);
        self.is_playing = false;
    }

    fn edit_cell<F: FnOnce(&mut Row)>(&mut self, f: F) {
        let Some(track) = self.song.tracks.get_mut(self.cursor_track as usize) else {
            tracing::warn!("no track at cursor");
            return;
        };
        let Some(clip) = track.clips.first_mut() else {
            tracing::warn!("no clip on track");
            return;
        };
        while clip.rows.len() <= self.cursor_row as usize {
            clip.rows.push(Row::default());
        }
        f(&mut clip.rows[self.cursor_row as usize]);
    }

    /// sing_like_coding 流: 空セルなら last_note をそのまま配置、既存 NoteOn なら
    /// key に delta を加算して last_note を更新。NoteOff には効果なし。
    fn apply_transpose(&mut self, delta: i16) {
        let last_note = self.last_note;
        let mut updated_note: Option<Note> = None;
        self.edit_cell(|row| match &row.note {
            None | Some(NoteEvent::Off) => {
                // Empty cell or Off row: place last_note as-is (overwrites Off).
                row.note = Some(NoteEvent::On(last_note));
            }
            Some(NoteEvent::On(n)) => {
                let new_key = (i16::from(n.key) + delta).clamp(0, 127) as u8;
                let new_note = Note {
                    key: new_key,
                    velocity: n.velocity,
                };
                row.note = Some(NoteEvent::On(new_note));
                updated_note = Some(new_note);
            }
        });
        if let Some(n) = updated_note {
            self.last_note = n;
        }
    }

    fn clamp_cursor(&mut self) {
        let track_max = self.song.tracks.len().saturating_sub(1) as u32;
        self.cursor_track = self.cursor_track.min(track_max);
        let row_max = self
            .song
            .tracks
            .get(self.cursor_track as usize)
            .and_then(|t| t.clips.first())
            .map(|c| c.rows.len().saturating_sub(1))
            .unwrap_or(0) as u32;
        self.cursor_row = self.cursor_row.min(row_max);
    }

    fn save_to(&self, path: &Path) -> bool {
        match common::project::save(path, &self.song) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "saved project");
                true
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to save project");
                false
            }
        }
    }
}

fn demo_clip() -> Clip {
    let note = |key, lyric: &str| Row {
        note: Some(NoteEvent::On(Note {
            key,
            velocity: 100,
        })),
        lyric: Some(lyric.into()),
        ..Default::default()
    };
    let mut rows = vec![
        note(60, "こ"),
        Row::default(),
        note(62, "ん"),
        Row::default(),
        note(64, "に"),
        Row::default(),
        note(65, "ち"),
        Row::default(),
        note(67, "は"),
        Row::default(),
        Row {
            note: Some(NoteEvent::Off),
            ..Default::default()
        },
    ];
    while rows.len() < 16 {
        rows.push(Row::default());
    }
    Clip {
        name: "こんにちは".into(),
        start_beat: 0.0,
        length_beats: 4.0,
        rows_per_beat: 4,
        rows,
    }
}
