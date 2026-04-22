use std::path::{Path, PathBuf};

use common::model::{Clip, InstrumentSource, Note, NoteEvent, Row, Song, Track};
use common::protocol::MainToChild;
use tokio::sync::mpsc::UnboundedSender;
use vizia::prelude::*;

#[derive(Lens)]
pub struct AppData {
    pub song: Song,
    pub file_path: Option<PathBuf>,
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
        Self {
            song: Song::default(),
            file_path: None,
            audio_tx: Some(audio_tx),
            plugin_tx: Some(plugin_tx),
        }
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
    AddVocalTrack,
    RemoveLastTrack,
}

impl Model for AppData {
    fn event(&mut self, _cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _| {
            if let WindowEvent::WindowClose = window_event {
                tracing::info!("window close requested");
            }
        });
        event.map(|app_event, _| match app_event {
            AppEvent::New => self.action_new(),
            AppEvent::Open => self.action_open(),
            AppEvent::Save => self.action_save(),
            AppEvent::SaveAs => self.action_save_as(),
            AppEvent::Play => {
                // Push the current Song to plugin_host so it can schedule events.
                self.send_plugin(MainToChild::LoadSong(self.song.clone()));
                self.send_audio(MainToChild::Play);
                self.send_plugin(MainToChild::Play);
            }
            AppEvent::Stop => {
                self.send_audio(MainToChild::Stop);
                self.send_plugin(MainToChild::Stop);
            }
            AppEvent::AddVocalTrack => self.action_add_vocal_track(),
            AppEvent::RemoveLastTrack => self.action_remove_last_track(),
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
