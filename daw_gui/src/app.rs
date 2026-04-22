use std::path::{Path, PathBuf};

use common::model::Song;
use vizia::prelude::*;

#[derive(Lens, Default)]
pub struct AppData {
    pub song: Song,
    pub file_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppEvent {
    New,
    Open,
    Save,
    SaveAs,
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
        });
    }
}

impl AppData {
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
