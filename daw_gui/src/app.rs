use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::model::{Clip, InstrumentSource, Note, NoteEvent, Row, Song, Track};
use common::plugin_db::PluginDatabase;
use common::protocol::MainToChild;
use tokio::sync::mpsc::UnboundedSender;
use vizia::prelude::*;

/// Lightweight lens-friendly copy of a plugin database entry. Carries only
/// what the picker list needs to render and select; path lookup happens
/// via `plugin_db.find_by_id` when the user picks.
#[derive(Debug, Clone, PartialEq, Eq, Data)]
pub struct PluginPickEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
}

#[derive(Lens)]
pub struct AppData {
    pub song: Song,
    pub file_path: Option<PathBuf>,
    pub cursor_row: u32,
    pub cursor_track: u32,
    /// Cached header (track names + column labels) for the tracker grid.
    /// Single Label at the top of ArrangementView; recomputed on `song` or
    /// `cursor_track` change.
    pub tracker_header: String,
    /// Cached per-row rendering of the tracker grid. Each entry is one row
    /// of the grid. Refreshed on `song`, `cursor_row`, or `cursor_track`
    /// change so cursor indicators (`>`, `[…]`) stay in sync.
    pub tracker_rows: Vec<String>,
    /// Template note used when placing into an empty cell (sing_like_coding
    /// style). Updated whenever the user edits a NoteOn cell.
    pub last_note: Note,
    /// Tracks whether playback is currently active so PlayToggle can flip.
    /// Flipped only by explicit user Play/Stop; auto-stop in plugin_host is
    /// not yet mirrored back here.
    pub is_playing: bool,
    /// Loop the current clip when it reaches the end instead of auto-stopping.
    /// Session-only state; not persisted to `.daw`.
    pub is_looping: bool,
    /// Tracker row currently being played back. `None` when no playback is
    /// happening (sentinel `u64::MAX` published by plugin_host, or no clip).
    /// Used by ArrangementView to highlight the sounding row.
    pub playhead_row: Option<u32>,
    /// Master output gain applied inside daw_audio (linear, 0.0..=1.0).
    /// Session state; not persisted to `.daw`.
    pub master_gain: f32,
    /// Smoothed peak levels (linear, 0.0..=1.0) for the left/right meter.
    /// Updated on every UI tick using `common::meter::update_peak` so the
    /// meter snaps up instantly and falls exponentially.
    pub peak_l_display: f32,
    pub peak_r_display: f32,
    /// Same peaks converted to `[0, 1]` for the meter-fill height binding.
    /// Recomputed together with `peak_*_display` so the view only needs a
    /// single Lens.
    pub peak_l_norm: f32,
    pub peak_r_norm: f32,
    /// Stable CLAP descriptor ID of the loaded plugin (e.g. `com.vital.Vital`).
    /// Persisted in `.daw`; the authoritative copy is
    /// `song.clap_plugin_id` (we mirror it here for Lens binding).
    pub clap_plugin_id: Option<String>,
    /// Path of the currently loaded CLAP plugin (derived from the database
    /// at runtime; not persisted). Kept for diagnostic logging.
    pub clap_plugin_path: Option<PathBuf>,
    /// Cached display label ("<name>" or "(no plugin)") for the inspector.
    pub clap_plugin_label: String,
    /// Plugin database (path + id lookup). Shared, read-mostly. `None`
    /// before the initial scan finishes.
    #[lens(ignore)]
    pub plugin_db: Option<Arc<PluginDatabase>>,
    /// Lens-visible copy of the database entries for the plugin-picker UI.
    /// Derived once from `plugin_db` at construction time.
    pub plugin_picker_entries: Vec<PluginPickEntry>,
    /// Whether the plugin-picker overlay is visible.
    pub is_plugin_picker_open: bool,
    /// When non-`None`, a save is in flight waiting for the plugin state
    /// reply; once `ChildToMain::PluginState` arrives the data is written
    /// to this path.
    #[lens(ignore)]
    pub pending_save_path: Option<PathBuf>,
    /// Whether the plugin's GUI window is currently open. Flips on button
    /// press / IPC callbacks.
    pub is_gui_open: bool,
    #[lens(ignore)]
    pub audio_tx: Option<UnboundedSender<MainToChild>>,
    #[lens(ignore)]
    pub plugin_tx: Option<UnboundedSender<MainToChild>>,
    /// Live handle to the host-owned container window while a plugin GUI is
    /// embedded. Kept in AppData so it outlives any one event handler and
    /// is destroyed deterministically when we close the GUI.
    #[cfg(windows)]
    #[lens(ignore)]
    pub plugin_host_window: Option<crate::view::plugin_embed::PluginHostWindow>,
}

impl AppData {
    pub fn new(
        audio_tx: UnboundedSender<MainToChild>,
        plugin_tx: UnboundedSender<MainToChild>,
        clap_plugin_path: Option<PathBuf>,
        plugin_db: Option<Arc<PluginDatabase>>,
    ) -> Self {
        let song = Song::default();
        let tracker_header = crate::view::arrangement::render_tracker_header(&song, 0);
        let tracker_rows = crate::view::arrangement::render_tracker_rows(&song, 0, 0);
        let clap_plugin_label = plugin_label(clap_plugin_path.as_deref());
        let plugin_picker_entries = plugin_db
            .as_ref()
            .map(|db| {
                let mut v: Vec<PluginPickEntry> = db
                    .entries
                    .iter()
                    .map(|e| PluginPickEntry {
                        id: e.id.clone(),
                        name: if e.name.is_empty() { e.id.clone() } else { e.name.clone() },
                        vendor: e.vendor.clone(),
                    })
                    .collect();
                v.sort_by_key(|e| e.name.to_lowercase());
                v
            })
            .unwrap_or_default();
        Self {
            song,
            file_path: None,
            cursor_row: 0,
            cursor_track: 0,
            tracker_header,
            tracker_rows,
            last_note: Note {
                key: 60,
                velocity: 100,
            },
            is_playing: false,
            is_looping: false,
            playhead_row: None,
            master_gain: 1.0,
            peak_l_display: 0.0,
            peak_r_display: 0.0,
            peak_l_norm: 0.0,
            peak_r_norm: 0.0,
            clap_plugin_id: None,
            clap_plugin_path,
            clap_plugin_label,
            plugin_db,
            plugin_picker_entries,
            is_plugin_picker_open: false,
            pending_save_path: None,
            is_gui_open: false,
            audio_tx: Some(audio_tx),
            plugin_tx: Some(plugin_tx),
            #[cfg(windows)]
            plugin_host_window: None,
        }
    }

    fn refresh_tracker_text(&mut self) {
        self.tracker_header =
            crate::view::arrangement::render_tracker_header(&self.song, self.cursor_track);
        self.tracker_rows = crate::view::arrangement::render_tracker_rows(
            &self.song,
            self.cursor_row,
            self.cursor_track,
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// Open the plugin picker overlay (DB-backed selection).
    OpenPluginPicker,
    /// Close the picker without changing selection.
    ClosePluginPicker,
    /// User picked a plugin from the picker; carries its stable id.
    SelectPluginFromDb(String),
    ToggleLoop,
    /// Master gain change from the fader. Carried as `f32::to_bits` because
    /// `f32` doesn't implement `Eq`/`Hash` and `AppEvent` needs both for the
    /// Keymap API.
    SetMasterGain(u32),
    /// Periodic UI tick carrying the latest `(playhead_samples, peak_l, peak_r)`
    /// from shmem. Peaks are f32::to_bits so the event can still derive
    /// Eq/Hash. `u64::MAX` playhead is the "not playing" sentinel published
    /// by plugin_host.
    Tick(u64, u32, u32),
    /// User clicked the "Open / Close Plugin GUI" toggle in the inspector.
    TogglePluginGui,
    /// Plugin confirmed that its GUI has been embedded at `width × height`.
    /// Forwarded by the `ChildToMain` receiver loop from daw_plugin_host.
    GuiOpenedFromChild { width: u32, height: u32 },
    /// Plugin requested a resize via `clap_host_gui.request_resize`.
    GuiRequestResizeFromChild { width: u32, height: u32 },
    /// Plugin signalled its GUI was closed (host-callback `closed`).
    GuiClosedFromChild,
    /// Plugin-host confirmed a successful load and reported the ID/name of
    /// the descriptor that actually came up.
    PluginLoadedFromChild { id: String, name: String },
    /// Reply to `RequestPluginState`. Payload is `None` when the plugin
    /// does not implement the state extension.
    PluginStateReceived(Option<Vec<u8>>),
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
                AppEvent::OpenPluginPicker => {
                    self.is_plugin_picker_open = true;
                    dirty = false;
                }
                AppEvent::ClosePluginPicker => {
                    self.is_plugin_picker_open = false;
                    dirty = false;
                }
                AppEvent::SelectPluginFromDb(id) => {
                    self.select_plugin_from_db(id.clone());
                    dirty = false;
                }
                AppEvent::ToggleLoop => {
                    self.toggle_loop();
                    dirty = false;
                }
                AppEvent::SetMasterGain(bits) => {
                    self.set_master_gain(f32::from_bits(*bits));
                    dirty = false;
                }
                AppEvent::Tick(playhead_samples, peak_l_bits, peak_r_bits) => {
                    self.on_tick(
                        *playhead_samples,
                        f32::from_bits(*peak_l_bits),
                        f32::from_bits(*peak_r_bits),
                    );
                    dirty = false;
                }
                AppEvent::TogglePluginGui => {
                    self.toggle_plugin_gui();
                    dirty = false;
                }
                AppEvent::GuiOpenedFromChild { width, height } => {
                    self.on_gui_opened(*width, *height);
                    dirty = false;
                }
                AppEvent::GuiRequestResizeFromChild { width, height } => {
                    self.on_gui_request_resize(*width, *height);
                    dirty = false;
                }
                AppEvent::GuiClosedFromChild => {
                    self.on_gui_closed();
                    dirty = false;
                }
                AppEvent::PluginLoadedFromChild { id, name } => {
                    self.on_plugin_loaded_from_child(id.clone(), name.clone());
                    dirty = false;
                }
                AppEvent::PluginStateReceived(state) => {
                    self.on_plugin_state_from_child(state.clone());
                    dirty = false;
                }
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
                // Resolve persisted plugin id → path via the database, then
                // send SetClapPlugin with initial_state so the plugin comes
                // back up with the same settings.
                self.restore_plugin_from_song(&song);
                self.song = song;
                self.file_path = Some(path);
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project")
            }
        }
    }

    /// Looks up the persisted plugin id on track 0 in the database, sends
    /// `SetClapPlugin` with the restored state, and updates the local
    /// mirrors. Silently skips when the id is missing from the DB so the
    /// project can still open (user can pick a plugin manually).
    ///
    /// NOTE: MVP wires the "active" plugin to `tracks[0].instrument`.
    /// Multi-track support is coming in a follow-up.
    fn restore_plugin_from_song(&mut self, song: &Song) {
        let Some(inst) = song
            .tracks
            .first()
            .and_then(|t| t.instrument.as_ref())
        else {
            tracing::info!("project has no CLAP plugin on track 0; nothing to restore");
            self.clap_plugin_id = None;
            self.clap_plugin_path = None;
            self.clap_plugin_label = plugin_label(None);
            return;
        };
        let Some(db) = self.plugin_db.as_deref() else {
            tracing::warn!(id = %inst.plugin_id, "plugin database not loaded; cannot resolve id");
            return;
        };
        let Some(entry) = db.find_by_id(&inst.plugin_id) else {
            tracing::error!(id = %inst.plugin_id, "plugin id not in database");
            return;
        };
        tracing::info!(
            id = %entry.id,
            path = %entry.path.display(),
            has_state = inst.state.is_some(),
            "restoring CLAP plugin from project"
        );
        self.send_plugin(MainToChild::SetSlotPlugin {
            track: 0,
            slot: common::protocol::PluginSlot::Instrument,
            path: entry.path.clone(),
            plugin_id: entry.id.clone(),
            initial_state: inst.state.clone(),
        });
        self.clap_plugin_id = Some(entry.id.clone());
        self.clap_plugin_path = Some(entry.path.clone());
        self.clap_plugin_label = plugin_label_from_entry(entry);
    }

    fn action_save(&mut self) {
        if let Some(path) = self.file_path.clone() {
            self.begin_save(path);
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
        self.begin_save(path);
    }

    /// Kicks off the two-step save: request plugin state asynchronously and
    /// stash the target path. `on_plugin_state_from_child` finishes the save
    /// when the reply arrives.
    fn begin_save(&mut self, path: PathBuf) {
        if self.clap_plugin_id.is_some() {
            self.pending_save_path = Some(path);
            self.send_plugin(MainToChild::RequestAllStates);
        } else {
            // No plugin loaded: write immediately without a state round-trip.
            self.finish_save(path, None);
        }
    }

    fn finish_save(&mut self, path: PathBuf, plugin_state: Option<Vec<u8>>) {
        // Snapshot plugin id/state into Track[0].instrument (MVP single-plugin
        // binding) before serializing. When there is no track yet, there's
        // nothing to persist.
        if let (Some(id), Some(track)) =
            (self.clap_plugin_id.clone(), self.song.tracks.get_mut(0))
        {
            track.instrument = Some(common::model::PluginInstance {
                plugin_id: id,
                state: plugin_state,
            });
        }
        if self.save_to(&path) {
            self.file_path = Some(path);
        }
    }

    fn toggle_loop(&mut self) {
        self.is_looping = !self.is_looping;
        tracing::info!(on = self.is_looping, "loop toggled");
        self.send_plugin(MainToChild::SetLoop(self.is_looping));
    }

    /// Toggle the plugin editor window. Opening allocates a host-owned Win32
    /// container (`PluginHostWindow`), sends its HWND to daw_plugin_host, and
    /// relies on `ChildToMain::GuiOpened` to confirm. Closing sends
    /// `CloseGui`; the `GuiClosedFromChild` callback finalizes Drop.
    #[cfg(windows)]
    fn toggle_plugin_gui(&mut self) {
        if self.is_gui_open {
            // Close flow: send IPC, wait for GuiClosedFromChild to tear down.
            self.send_plugin(MainToChild::CloseSlotGui {
                track: 0,
                slot: common::protocol::PluginSlot::Instrument,
            });
            return;
        }
        match crate::view::plugin_embed::PluginHostWindow::create(
            800,
            600,
            &format!("Plugin — {}", self.clap_plugin_label),
        ) {
            Ok(win) => {
                let hwnd = win.hwnd_u64();
                self.plugin_host_window = Some(win);
                tracing::info!(hwnd, "created plugin host window, requesting embed");
                self.send_plugin(MainToChild::OpenSlotGuiEmbedded {
                    track: 0,
                    slot: common::protocol::PluginSlot::Instrument,
                    host_hwnd: hwnd,
                });
                self.is_gui_open = true;
            }
            Err(e) => {
                tracing::error!(error = ?e, "failed to create plugin host window");
            }
        }
    }

    #[cfg(not(windows))]
    fn toggle_plugin_gui(&mut self) {
        tracing::warn!("plugin GUI embedding is only implemented on Windows");
    }

    /// Handler for `ChildToMain::GuiOpened`: resize our container so the
    /// client area matches the plugin's preferred size, avoiding the
    /// clipped-UI problem.
    #[cfg(windows)]
    fn on_gui_opened(&mut self, width: u32, height: u32) {
        tracing::info!(width, height, "plugin GUI opened");
        if let Some(win) = &self.plugin_host_window {
            win.set_client_size(width, height);
        }
    }

    #[cfg(not(windows))]
    fn on_gui_opened(&mut self, _width: u32, _height: u32) {}

    /// Handler for `ChildToMain::GuiRequestResize`: plugin wants to grow /
    /// shrink. Resize our container first, then echo `ResizeGui` back so the
    /// plugin-main thread calls `plugin.gui.set_size(w, h)` on its end.
    #[cfg(windows)]
    fn on_gui_request_resize(&mut self, width: u32, height: u32) {
        tracing::info!(width, height, "plugin requested GUI resize");
        if let Some(win) = &self.plugin_host_window {
            win.set_client_size(width, height);
        }
        self.send_plugin(MainToChild::ResizeSlotGui {
            track: 0,
            slot: common::protocol::PluginSlot::Instrument,
            width,
            height,
        });
    }

    #[cfg(not(windows))]
    fn on_gui_request_resize(&mut self, _width: u32, _height: u32) {}

    /// Handler for `ChildToMain::GuiClosed`: plugin confirms tear-down, drop
    /// our container so Drop destroys the HWND.
    #[cfg(windows)]
    fn on_gui_closed(&mut self) {
        tracing::info!("plugin GUI closed (from child)");
        self.plugin_host_window = None;
        self.is_gui_open = false;
    }

    #[cfg(not(windows))]
    fn on_gui_closed(&mut self) {
        self.is_gui_open = false;
    }

    /// plugin_host reported that a SetClapPlugin succeeded. Sync the id and
    /// display label with reality.
    fn on_plugin_loaded_from_child(&mut self, id: String, name: String) {
        tracing::info!(id = %id, name = %name, "plugin_host confirmed plugin loaded");
        self.clap_plugin_label = if name.is_empty() { id.clone() } else { name };
        self.clap_plugin_id = Some(id);
    }

    /// `ChildToMain::PluginState` reply. When a save was pending, finalize
    /// it; otherwise the reply is unsolicited and ignored.
    fn on_plugin_state_from_child(&mut self, state: Option<Vec<u8>>) {
        tracing::info!(
            has_state = state.is_some(),
            pending = self.pending_save_path.is_some(),
            "plugin state reply received"
        );
        if let Some(path) = self.pending_save_path.take() {
            self.finish_save(path, state);
        }
    }

    /// Process a periodic UI tick that carries the latest `playhead_samples`
    /// and raw L/R peak amplitudes published to shmem by daw_audio and
    /// daw_plugin_host.
    ///
    /// Updates:
    /// - `playhead_row`: tracker-row highlight (None when not playing).
    /// - `peak_{l,r}_display`: fast-attack / exponential-release peak tracker.
    /// - `peak_{l,r}_norm`: dB-space normalized fill for the meter bar.
    ///
    /// `playhead_samples == u64::MAX` is the sentinel meaning "not playing".
    fn on_tick(&mut self, playhead_samples: u64, peak_l_raw: f32, peak_r_raw: f32) {
        // --- Playhead row highlight --------------------------------------
        let next_row = if playhead_samples == u64::MAX {
            None
        } else {
            common::timing::playhead_to_row(
                Some(&self.song),
                common::audio_bridge::SAMPLE_RATE,
                playhead_samples,
            )
        };
        if next_row != self.playhead_row {
            self.playhead_row = next_row;
        }

        // --- Plugin GUI ✕-button bridge ----------------------------------
        // Runs on the Vizia main thread so it can safely send IPC. Converts
        // the async WNDPROC signal into our synchronous close flow.
        #[cfg(windows)]
        if self.is_gui_open
            && let Some(win) = &self.plugin_host_window
            && win.take_close_request()
        {
            tracing::info!("plugin host window ✕ clicked; forwarding to CloseGui");
            self.send_plugin(MainToChild::CloseSlotGui {
                track: 0,
                slot: common::protocol::PluginSlot::Instrument,
            });
        }

        // --- Peak meter --------------------------------------------------
        // At 30 Hz, 0.85 per-tick decay ≈ -24 dB/s release (fairly standard).
        const RELEASE: f32 = 0.85;
        self.peak_l_display =
            common::meter::update_peak(self.peak_l_display, peak_l_raw, RELEASE);
        self.peak_r_display =
            common::meter::update_peak(self.peak_r_display, peak_r_raw, RELEASE);
        self.peak_l_norm = common::meter::db_to_norm(common::meter::linear_to_db(
            self.peak_l_display,
        ));
        self.peak_r_norm = common::meter::db_to_norm(common::meter::linear_to_db(
            self.peak_r_display,
        ));
    }

    /// Applies a master-gain change from the slider: clamp to [0,1], update
    /// local state, and forward to daw_audio over the control pipe.
    fn set_master_gain(&mut self, gain: f32) {
        let clamped = gain.clamp(0.0, 1.0);
        self.master_gain = clamped;
        self.send_audio(MainToChild::SetMasterGain(clamped));
    }

    /// User picked a plugin from the DB-backed picker overlay. Resolves the
    /// path from the database and swaps in the plugin (no state to restore:
    /// this is an explicit "new plugin" flow).
    fn select_plugin_from_db(&mut self, id: String) {
        self.is_plugin_picker_open = false;
        let Some(db) = self.plugin_db.as_deref() else {
            tracing::warn!(id, "plugin_db not available");
            return;
        };
        let Some(entry) = db.find_by_id(&id) else {
            tracing::error!(id, "picked plugin id not in database (stale picker?)");
            return;
        };
        let path = entry.path.clone();
        tracing::info!(id = %entry.id, path = %path.display(), "user picked plugin");
        self.send_plugin(MainToChild::SetSlotPlugin {
            track: 0,
            slot: common::protocol::PluginSlot::Instrument,
            path: path.clone(),
            plugin_id: entry.id.clone(),
            initial_state: None,
        });
        self.clap_plugin_label = plugin_label_from_entry(entry);
        self.clap_plugin_path = Some(path);
        self.clap_plugin_id = Some(entry.id.clone());
        // Starting fresh — drop any persisted state from a previous plugin.
        if let Some(track) = self.song.tracks.get_mut(0) {
            track.instrument = Some(common::model::PluginInstance::new(entry.id.clone()));
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
            clips: vec![demo_clip()],
            ..Track::default()
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

fn plugin_label(path: Option<&Path>) -> String {
    match path {
        Some(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string()),
        None => "(no plugin)".into(),
    }
}

fn plugin_label_from_entry(entry: &common::plugin_db::PluginEntry) -> String {
    if entry.name.is_empty() {
        entry.id.clone()
    } else {
        entry.name.clone()
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
