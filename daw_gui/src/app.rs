use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::model::{Clip, InstrumentSource, Note, NoteEvent, Row, Song, Track};
use common::plugin_db::PluginDatabase;
use common::plugin_format::PluginFormat;
use common::protocol::{MainToChild, PluginSlot, SlotState};
use tokio::sync::mpsc::UnboundedSender;
use vizia::prelude::*;

/// Lightweight lens-friendly copy of a plugin database entry. Carries only
/// what the picker list needs to render and select; path lookup happens
/// via `plugin_db.find_by_id` when the user picks.
/// Per-track mixer strip row bound to the new MixerStripsView. Rebuilt
/// from `song.tracks` whenever the track list or a mixer parameter
/// changes; the `peak_*_norm` fields are refreshed on every UI tick
/// from the shmem-published post-fader peaks.
#[derive(Debug, Clone, PartialEq, Data)]
pub struct TrackMixEntry {
    pub index: u32,
    pub name: String,
    pub volume: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    pub peak_l_norm: f32,
    pub peak_r_norm: f32,
}

impl Default for TrackMixEntry {
    fn default() -> Self {
        Self {
            index: u32::MAX,
            name: String::new(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            peak_l_norm: 0.0,
            peak_r_norm: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Data)]
pub struct PluginPickEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
    /// Features from the CLAP descriptor — used by the picker's filter
    /// bar to show only instrument/audio-effect/note-effect plugins.
    pub features: Vec<String>,
    /// Short label ("CLAP" / "VST3") rendered as a badge in the picker row.
    /// Kept as a `String` (not `PluginFormat`) so Vizia's `Data` derive
    /// works without a foreign-type `impl Data` on `PluginFormat`.
    pub format_label: String,
}

/// A single slot on the visible track's chain, used by the Track Inspector
/// list view. Rebuilt from `song.tracks[0]` whenever the chain changes.
///
/// `slot_kind` / `slot_index` reproduce `PluginSlot` in Data-safe primitive
/// form: 0 = MidiFx(idx), 1 = Instrument, 2 = Fx(idx).
#[derive(Debug, Clone, PartialEq, Eq, Data)]
pub struct ChainEntry {
    pub slot_kind: u8,
    pub slot_index: u32,
    pub section_label: String,
    pub plugin_name: String,
}

impl ChainEntry {
    #[allow(dead_code)]
    pub fn to_plugin_slot(&self) -> PluginSlot {
        match self.slot_kind {
            0 => PluginSlot::MidiFx(self.slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(self.slot_index),
        }
    }
}

/// Kind of slot the Plugin Picker is currently adding to. Drives the
/// feature filter and the destination slot when the user picks a plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Data)]
pub enum PickerTarget {
    Instrument,
    Fx,
    #[allow(dead_code)]
    MidiFx,
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
    /// Plugin database (path + id lookup). Shared, read-mostly. `None`
    /// before the initial scan finishes.
    #[lens(ignore)]
    pub plugin_db: Option<Arc<PluginDatabase>>,
    /// Lens-visible copy of the database entries for the plugin-picker UI.
    /// Derived once from `plugin_db` at construction time.
    pub plugin_picker_entries: Vec<PluginPickEntry>,
    /// Subset of `plugin_picker_entries` filtered by the current
    /// `plugin_picker_target` feature; rebuilt when the picker opens so
    /// `+ Add Instrument` only shows instruments, etc.
    pub plugin_picker_visible: Vec<PluginPickEntry>,
    /// Whether the plugin-picker overlay is visible.
    pub is_plugin_picker_open: bool,
    /// What the user is adding when they open the picker — drives the
    /// picker's feature filter and the destination slot for the selection.
    pub plugin_picker_target: PickerTarget,
    /// Chain entries for the track the Inspector is currently viewing
    /// (driven by `cursor_track`). Rebuilt whenever that track's
    /// instrument / fx / midi-fx chain changes, or the cursor moves.
    pub inspector_chain: Vec<ChainEntry>,
    /// Display label for the selected track (e.g. "Track 1") shown in the
    /// Inspector heading. Refreshed alongside `inspector_chain`.
    pub selected_track_label: String,
    /// Name of the loaded instrument on the selected track, or a placeholder.
    /// Convenience mirror for UI display; primary storage is
    /// `song.tracks[cursor_track].instrument`.
    pub instrument_label: String,
    /// When non-`None`, a save is in flight waiting for the plugin state
    /// reply; once `AllStatesReceived` arrives the data is written to this
    /// path.
    #[lens(ignore)]
    pub pending_save_path: Option<PathBuf>,
    #[lens(ignore)]
    pub audio_tx: Option<UnboundedSender<MainToChild>>,
    #[lens(ignore)]
    pub plugin_tx: Option<UnboundedSender<MainToChild>>,
    /// Live host-owned container windows per loaded plugin GUI, keyed by
    /// (track, slot). Each entry is one open editor; multiple editors can
    /// be visible simultaneously.
    #[cfg(windows)]
    #[lens(ignore)]
    pub plugin_host_windows:
        HashMap<(u32, PluginSlot), crate::view::plugin_embed::PluginHostWindow>,

    /// Renoise-style mixer strip row: one entry per song track, rebuilt
    /// on song mutation and refreshed on every UI tick with post-fader
    /// peaks from the shmem bridge.
    pub track_mix: Vec<TrackMixEntry>,
    /// Raw linear-amplitude peak already decayed with the mixer's
    /// release curve; parallel array to `song.tracks`. Kept out of the
    /// lens-visible struct so decay state isn't exposed to the UI —
    /// it's baked into `track_mix[i].peak_*_norm` each tick.
    #[lens(ignore)]
    pub track_peak_display: Vec<(f32, f32)>,

    /// Drop-off slot for background VOICEVOX synthesis results. Worker
    /// thread writes here, then emits `VocalSynthCompleted`.
    #[lens(ignore)]
    pub synth_result: Arc<Mutex<Vec<common::voicevox::SynthResult>>>,

    /// Drop-off slot for a background plugin-database rescan. The worker
    /// thread scans, persists the result to the on-disk cache, stashes the
    /// fresh `PluginDatabase` here, and then emits
    /// `AppEvent::PluginDbRescanCompleted` so the UI thread picks it up.
    /// Kept out of any lens — the UI reacts to rescan via the event, not
    /// by observing this field.
    #[lens(ignore)]
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    /// True while a rescan is in flight; lens-visible so the picker can
    /// show a "Rescanning..." label instead of letting the user hammer the
    /// button.
    pub is_rescanning: bool,
    /// Status message shown in the status bar. Updated by synthesis /
    /// rescan / errors to give the user feedback on background tasks.
    pub status_message: String,
    /// True while the lyric inline editor is open. Bound by the overlay
    /// `Binding` that shows/hides the `Textbox`.
    /// First visible track index in the tracker + mixer view. Adjusted
    /// automatically when the cursor moves beyond the visible window.
    pub visible_track_start: u32,
    /// Number of tracks that fit side-by-side in the arrangement area.
    /// MVP: fixed at 6; a future version should derive this from the
    /// actual pixel width of the arrangement panel.
    pub visible_track_count: u32,
    /// `(start, end)` range of visible track indices, used by the mixer
    /// strip visibility lens. Updated alongside `visible_track_start`.
    pub mixer_visible_range: (u32, u32),
    pub lyric_editing: bool,
    /// Current text in the lyric editor `Textbox`.
    pub lyric_edit_text: String,
}

impl AppData {
    pub fn new(
        audio_tx: UnboundedSender<MainToChild>,
        plugin_tx: UnboundedSender<MainToChild>,
        // Path reserved for future auto-select; currently not wired to song.
        _clap_plugin_path: Option<PathBuf>,
        plugin_db: Option<Arc<PluginDatabase>>,
    ) -> Self {
        // Mixer view shows a permanent master strip even when the user
        // has added zero tracks; that keeps Vizia's `List` draw path off
        // its zero-item panic path.
        let song = Song::default();
        let tracker_header =
            crate::view::arrangement::render_tracker_header(&song, 0, 0, 6);
        let tracker_rows =
            crate::view::arrangement::render_tracker_rows(&song, 0, 0, 0, 6);
        // Pre-compute anything that borrows `song` so the Self literal
        // below can move it in field-declaration order without tripping
        // the borrow checker.
        let initial_mix = initial_track_mix(&song);
        let initial_peak_display = vec![(0.0, 0.0); song.tracks.len()];
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
                        features: e.features.clone(),
                        format_label: e.format.as_str().to_string(),
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
            plugin_db,
            plugin_picker_entries,
            plugin_picker_visible: Vec::new(),
            is_plugin_picker_open: false,
            plugin_picker_target: PickerTarget::Instrument,
            inspector_chain: Vec::new(),
            selected_track_label: "Track 1".to_string(),
            instrument_label: "(no instrument)".to_string(),
            pending_save_path: None,
            audio_tx: Some(audio_tx),
            plugin_tx: Some(plugin_tx),
            #[cfg(windows)]
            plugin_host_windows: HashMap::new(),
            track_mix: initial_mix,
            track_peak_display: initial_peak_display,
            synth_result: Arc::new(Mutex::new(Vec::new())),
            rescan_result: Arc::new(Mutex::new(None)),
            is_rescanning: false,
            status_message: String::new(),
            visible_track_start: 0,
            visible_track_count: 6,
            mixer_visible_range: (0, 6),
            lyric_editing: false,
            lyric_edit_text: String::new(),
        }
    }

    fn refresh_tracker_text(&mut self) {
        self.tracker_header = crate::view::arrangement::render_tracker_header(
            &self.song,
            self.cursor_track,
            self.visible_track_start,
            self.visible_track_count,
        );
        self.tracker_rows = crate::view::arrangement::render_tracker_rows(
            &self.song,
            self.cursor_row,
            self.cursor_track,
            self.visible_track_start,
            self.visible_track_count,
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
    /// Open the plugin picker overlay targeting a specific slot kind.
    OpenPluginPickerFor(PickerTarget),
    /// Close the picker without changing selection.
    ClosePluginPicker,
    /// User picked a plugin from the picker; carries its stable id.
    SelectPluginFromDb(String),
    /// Toggle GUI for a specific chain slot (from a row's [GUI] button).
    ToggleSlotGui { slot_kind: u8, slot_index: u32 },
    /// Remove the plugin at a given chain slot.
    RemoveSlot { slot_kind: u8, slot_index: u32 },
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
    /// Plugin confirmed that its GUI has been embedded at `width × height`.
    /// Forwarded by the `ChildToMain` receiver loop from daw_plugin_host;
    /// the (track, slot) addresses which open container window is the target.
    GuiOpenedFromChild {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    /// Plugin requested a resize via `clap_host_gui.request_resize`.
    GuiRequestResizeFromChild {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    /// Plugin signalled its GUI was closed (host-callback `closed`).
    GuiClosedFromChild { track: u32, slot: PluginSlot },
    /// Plugin-host confirmed a successful load and reported the ID/name of
    /// the descriptor that actually came up. Routed to the (track, slot)
    /// that issued the `SetSlotPlugin` so the right model entry is updated.
    SlotPluginLoadedFromChild {
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
    },
    /// Reply to `RequestAllStates`. Each entry pairs a (track, slot) with the
    /// plugin's serialized state (or `None` if the plugin doesn't implement
    /// the state extension). Triggers `finish_save` when a save is pending.
    AllStatesReceived(Vec<SlotState>),
    /// User asked for a full plugin re-scan (CLAP + VST3). The scan itself
    /// runs on a background thread; when it finishes,
    /// `PluginDbRescanCompleted` is posted.
    RescanPluginDb,
    /// Background rescan finished. The new `PluginDatabase` lives in
    /// `AppData::rescan_result`; this event carries no payload because
    /// `AppEvent` needs `Eq + Hash` and `PluginDatabase` does not.
    PluginDbRescanCompleted,
    /// Mixer strip slider drag / button toggle.
    SetTrackVolume { track: u32, bits: u32 },
    SetTrackPan { track: u32, bits: u32 },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    /// Per-track post-fader peaks sampled from the shmem bridge on the UI
    /// poll thread. Carries `f32::to_bits` pairs so the event stays
    /// `Eq + Hash`.
    TrackPeaksTick(Vec<(u32, u32)>),
    /// Synthesize all vocal clips on all vocal tracks via VOICEVOX.
    /// Triggered by the `v` shortcut key. Runs on a background thread;
    /// completion posts `VocalSynthCompleted`.
    SynthesizeVocal,
    /// Background vocal synth finished. Results are in `synth_result`.
    VocalSynthCompleted,
    /// Export the entire song to a WAV file. Triggered by Ctrl+E or
    /// the File menu.
    ExportWav,
    /// plugin_host finished the WAV export.
    ExportWavComplete { error: Option<String> },
    /// `i` key: open the inline lyric editor for the current row.
    StartLyricEdit,
    /// Textbox on_edit callback.
    LyricEditChanged(String),
    /// Enter in the lyric editor: commit current text, move to next row.
    SubmitLyricEdit,
    /// Esc in the lyric editor: discard and close.
    CancelLyricEdit,
}

impl Model for AppData {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _| {
            if let WindowEvent::WindowClose = window_event {
                tracing::info!("window close requested");
            }
        });
        event.map(|app_event, _| {
            let mut dirty = true;

            // While the lyric editor is open, only process lyric-related
            // events. All other keymap shortcuts (h/j/k/l cursor, Play,
            // etc.) are blocked so they don't move the cursor or trigger
            // actions behind the modal.
            if self.lyric_editing {
                match app_event {
                    AppEvent::LyricEditChanged(text) => {
                        self.lyric_edit_text = text.clone();
                    }
                    AppEvent::SubmitLyricEdit => {
                        self.submit_lyric_edit();
                    }
                    AppEvent::CancelLyricEdit => {
                        self.lyric_editing = false;
                    }
                    _ => {}
                }
                return;
            }

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
                AppEvent::OpenPluginPickerFor(target) => {
                    self.plugin_picker_target = *target;
                    self.refresh_picker_visible();
                    self.is_plugin_picker_open = true;
                    dirty = false;
                }
                AppEvent::ClosePluginPicker => {
                    self.is_plugin_picker_open = false;
                    dirty = false;
                }
                AppEvent::RescanPluginDb => {
                    self.begin_rescan(cx);
                    dirty = false;
                }
                AppEvent::PluginDbRescanCompleted => {
                    self.finish_rescan();
                    dirty = false;
                }
                AppEvent::SelectPluginFromDb(id) => {
                    self.select_plugin_from_db(id.clone());
                    dirty = false;
                }
                AppEvent::ToggleSlotGui {
                    slot_kind,
                    slot_index,
                } => {
                    self.toggle_slot_gui(*slot_kind, *slot_index);
                    dirty = false;
                }
                AppEvent::RemoveSlot {
                    slot_kind,
                    slot_index,
                } => {
                    self.remove_slot(*slot_kind, *slot_index);
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
                AppEvent::GuiOpenedFromChild {
                    track,
                    slot,
                    width,
                    height,
                } => {
                    self.on_gui_opened(*track, *slot, *width, *height);
                    dirty = false;
                }
                AppEvent::GuiRequestResizeFromChild {
                    track,
                    slot,
                    width,
                    height,
                } => {
                    self.on_gui_request_resize(*track, *slot, *width, *height);
                    dirty = false;
                }
                AppEvent::GuiClosedFromChild { track, slot } => {
                    self.on_gui_closed(*track, *slot);
                    dirty = false;
                }
                AppEvent::SlotPluginLoadedFromChild {
                    track,
                    slot,
                    id,
                    name,
                } => {
                    self.on_plugin_loaded_from_child(*track, *slot, id.clone(), name.clone());
                    dirty = false;
                }
                AppEvent::AllStatesReceived(entries) => {
                    self.on_all_states_from_child(entries.clone());
                    dirty = false;
                }
                AppEvent::SetTrackVolume { track, bits } => {
                    self.set_track_volume(*track, f32::from_bits(*bits));
                    dirty = false;
                }
                AppEvent::SetTrackPan { track, bits } => {
                    self.set_track_pan(*track, f32::from_bits(*bits));
                    dirty = false;
                }
                AppEvent::ToggleTrackMute(track) => {
                    self.toggle_track_mute(*track);
                    dirty = false;
                }
                AppEvent::ToggleTrackSolo(track) => {
                    self.toggle_track_solo(*track);
                    dirty = false;
                }
                AppEvent::TrackPeaksTick(peaks) => {
                    self.on_track_peaks_tick(peaks);
                    dirty = false;
                }
                AppEvent::ExportWav => {
                    self.action_export_wav();
                    dirty = false;
                }
                AppEvent::ExportWavComplete { error } => {
                    if let Some(e) = error {
                        self.status_message = format!("WAV 書き出し失敗: {e}");
                    } else {
                        self.status_message = "WAV 書き出し完了".to_string();
                    }
                    dirty = false;
                }
                AppEvent::SynthesizeVocal => {
                    self.status_message = "VOICEVOX 合成中...".to_string();
                    self.begin_vocal_synth(cx);
                    dirty = false;
                }
                AppEvent::VocalSynthCompleted => {
                    self.finish_vocal_synth();
                    dirty = false;
                }
                AppEvent::StartLyricEdit => {
                    self.start_lyric_edit();
                    dirty = false;
                }
                // LyricEditChanged / SubmitLyricEdit / CancelLyricEdit
                // are handled in the early-return block above when
                // lyric_editing is true.
                AppEvent::LyricEditChanged(_)
                | AppEvent::SubmitLyricEdit
                | AppEvent::CancelLyricEdit => {
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
        self.cursor_track = 0;
        self.refresh_inspector_chain();
        self.rebuild_track_mix();
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
                // Resolve persisted plugin ids → paths via the database, then
                // send SetSlotPlugin with initial_state for every plugin on
                // every track so they come back with the same settings.
                self.restore_plugin_from_song(&song);
                self.song = song;
                self.file_path = Some(path);
                self.cursor_track = 0;
                self.refresh_inspector_chain();
                self.rebuild_track_mix();
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project")
            }
        }
    }

    /// Resolves every plugin id on every track (MIDI FX → Instrument → FX)
    /// via the database and re-sends them with their persisted state.
    /// Chain-order replay matters because plugin_host installs FX / MIDI FX
    /// into the next free index on each track, so we must replay in the
    /// same order the entries were saved in.
    fn restore_plugin_from_song(&mut self, song: &Song) {
        let Some(db) = self.plugin_db.clone() else {
            tracing::warn!("plugin database not loaded; cannot resolve plugin ids");
            return;
        };
        // Snapshot every (track, slot, instance) triple so we can mutate
        // self while iterating. Track order comes from the song's Vec which
        // already matches the user's arrangement.
        let mut to_send: Vec<(u32, PluginSlot, common::model::PluginInstance)> = Vec::new();
        for (track_idx, track) in song.tracks.iter().enumerate() {
            let t = track_idx as u32;
            for (i, p) in track.midi_fx_chain.iter().enumerate() {
                to_send.push((t, PluginSlot::MidiFx(i as u32), p.clone()));
            }
            if let Some(inst) = track.instrument.as_ref() {
                to_send.push((t, PluginSlot::Instrument, inst.clone()));
            }
            for (i, p) in track.fx_chain.iter().enumerate() {
                to_send.push((t, PluginSlot::Fx(i as u32), p.clone()));
            }
        }
        for (track, slot, inst) in to_send {
            let Some(entry) = db.find_by_id(&inst.plugin_id) else {
                tracing::error!(id = %inst.plugin_id, track, ?slot, "plugin id not in database");
                continue;
            };
            tracing::info!(
                track,
                ?slot,
                id = %entry.id,
                path = %entry.path.display(),
                has_state = inst.state.is_some(),
                "restoring plugin from project"
            );
            self.send_plugin(MainToChild::SetSlotPlugin {
                track,
                slot,
                format: entry.format,
                path: entry.path.clone(),
                plugin_id: entry.id.clone(),
                initial_state: inst.state.clone(),
            });
        }
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

    /// Kicks off the two-step save: request plugin states asynchronously and
    /// stash the target path. `on_all_states_from_child` finishes the save
    /// when the reply arrives.
    fn begin_save(&mut self, path: PathBuf) {
        let has_plugin = self.song.tracks.first().is_some_and(|t| {
            t.instrument.is_some() || !t.fx_chain.is_empty() || !t.midi_fx_chain.is_empty()
        });
        if has_plugin {
            self.pending_save_path = Some(path);
            self.send_plugin(MainToChild::RequestAllStates);
        } else {
            // No plugin loaded: write immediately without a state round-trip.
            self.finish_save(path, Vec::new());
        }
    }

    /// Distributes the captured `SlotState` entries across the model's
    /// instrument / fx / midi-fx slots, then writes the project file. Slots
    /// whose plugin was unloaded between request and reply are silently
    /// skipped (the state has nowhere to live).
    fn finish_save(&mut self, path: PathBuf, states: Vec<SlotState>) {
        for s in states {
            let Some(track) = self.song.tracks.get_mut(s.track as usize) else {
                tracing::warn!(track = s.track, ?s.slot, "save: track not found in model");
                continue;
            };
            match s.slot {
                PluginSlot::Instrument => {
                    if let Some(inst) = track.instrument.as_mut() {
                        inst.state = s.data;
                    }
                }
                PluginSlot::Fx(i) => {
                    if let Some(p) = track.fx_chain.get_mut(i as usize) {
                        p.state = s.data;
                    }
                }
                PluginSlot::MidiFx(i) => {
                    if let Some(p) = track.midi_fx_chain.get_mut(i as usize) {
                        p.state = s.data;
                    }
                }
            }
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

    /// Handler for `ChildToMain::SlotGuiOpened`: plugin confirmed embed, so
    /// resize our container's client area to match its preferred size.
    #[cfg(windows)]
    fn on_gui_opened(&mut self, track: u32, slot: PluginSlot, width: u32, height: u32) {
        tracing::info!(track, ?slot, width, height, "plugin GUI opened");
        if let Some(win) = self.plugin_host_windows.get(&(track, slot)) {
            win.set_client_size(width, height);
        }
    }

    #[cfg(not(windows))]
    fn on_gui_opened(&mut self, _track: u32, _slot: PluginSlot, _width: u32, _height: u32) {}

    /// Handler for `ChildToMain::SlotGuiRequestResize`: plugin asked to
    /// resize. Update our container first, then echo `ResizeSlotGui` back
    /// so the plugin-main thread runs `gui.set_size(w, h)`.
    #[cfg(windows)]
    fn on_gui_request_resize(&mut self, track: u32, slot: PluginSlot, width: u32, height: u32) {
        tracing::info!(track, ?slot, width, height, "plugin requested GUI resize");
        if let Some(win) = self.plugin_host_windows.get(&(track, slot)) {
            win.set_client_size(width, height);
        }
        self.send_plugin(MainToChild::ResizeSlotGui {
            track,
            slot,
            width,
            height,
        });
    }

    #[cfg(not(windows))]
    fn on_gui_request_resize(
        &mut self,
        _track: u32,
        _slot: PluginSlot,
        _width: u32,
        _height: u32,
    ) {
    }

    /// Handler for `ChildToMain::SlotGuiClosed`: plugin confirms tear-down,
    /// drop our container so Drop destroys the HWND.
    #[cfg(windows)]
    fn on_gui_closed(&mut self, track: u32, slot: PluginSlot) {
        tracing::info!(track, ?slot, "plugin GUI closed (from child)");
        self.plugin_host_windows.remove(&(track, slot));
    }

    #[cfg(not(windows))]
    fn on_gui_closed(&mut self, _track: u32, _slot: PluginSlot) {}

    /// plugin_host reported that a `SetSlotPlugin` succeeded. Updates the
    /// (track, slot) target with the actual id/name reported by the host so
    /// the model and Inspector reflect what is really loaded.
    fn on_plugin_loaded_from_child(
        &mut self,
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
    ) {
        tracing::info!(track, ?slot, %id, %name, "plugin_host confirmed plugin loaded");
        let label = if name.is_empty() { id.clone() } else { name };
        let track_idx = track as usize;
        self.ensure_first_track();
        let Some(t) = self.song.tracks.get_mut(track_idx) else {
            tracing::warn!(track, "plugin loaded for missing track");
            return;
        };
        match slot {
            PluginSlot::Instrument => {
                // Preserve any state that was attached optimistically; the
                // load itself may have come with `initial_state`. Format is
                // also carried over from the optimistic entry.
                let (state, format) = t
                    .instrument
                    .as_ref()
                    .map(|i| (i.state.clone(), i.format))
                    .unwrap_or((None, PluginFormat::Clap));
                t.instrument = Some(common::model::PluginInstance {
                    plugin_id: id,
                    format,
                    state,
                });
                if track == self.cursor_track {
                    self.instrument_label = label;
                }
            }
            PluginSlot::Fx(i) => {
                let i = i as usize;
                let (existing_state, format) = t
                    .fx_chain
                    .get(i)
                    .map(|p| (p.state.clone(), p.format))
                    .unwrap_or((None, PluginFormat::Clap));
                let inst = common::model::PluginInstance {
                    plugin_id: id,
                    format,
                    state: existing_state,
                };
                if i < t.fx_chain.len() {
                    t.fx_chain[i] = inst;
                } else {
                    t.fx_chain.push(inst);
                }
            }
            PluginSlot::MidiFx(i) => {
                let i = i as usize;
                let (existing_state, format) = t
                    .midi_fx_chain
                    .get(i)
                    .map(|p| (p.state.clone(), p.format))
                    .unwrap_or((None, PluginFormat::Clap));
                let inst = common::model::PluginInstance {
                    plugin_id: id,
                    format,
                    state: existing_state,
                };
                if i < t.midi_fx_chain.len() {
                    t.midi_fx_chain[i] = inst;
                } else {
                    t.midi_fx_chain.push(inst);
                }
            }
        }
        // Refresh the Inspector only when the loaded slot belongs to the
        // track the user is currently viewing.
        if track == self.cursor_track {
            self.refresh_inspector_chain();
        }
    }

    /// Opens or closes the GUI for the given chain slot. Open state lives in
    /// `plugin_host_windows`: presence = open, absence = closed. Multiple
    /// slots can be open at once, each with its own top-level container.
    fn toggle_slot_gui(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        // The Inspector operates on the selected track. Match plugin_host's
        // addressing by sending the same `cursor_track` through IPC.
        let track = self.cursor_track;
        #[cfg(windows)]
        {
            if self.plugin_host_windows.contains_key(&(track, slot)) {
                self.send_plugin(MainToChild::CloseSlotGui { track, slot });
                return;
            }
            let label = self
                .song
                .tracks
                .get(track as usize)
                .and_then(|t| self.slot_ref_name(t, slot))
                .unwrap_or_else(|| "(unknown)".into());
            match crate::view::plugin_embed::PluginHostWindow::create(
                800,
                600,
                &format!("Plugin — {}", label),
            ) {
                Ok(win) => {
                    let hwnd = win.hwnd_u64();
                    self.plugin_host_windows.insert((track, slot), win);
                    self.send_plugin(MainToChild::OpenSlotGuiEmbedded {
                        track,
                        slot,
                        host_hwnd: hwnd,
                    });
                }
                Err(e) => tracing::error!(error = ?e, ?slot, "failed to create container"),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (track, slot, slot_kind, slot_index);
        }
    }

    #[cfg(windows)]
    fn slot_ref_name(&self, track: &Track, slot: PluginSlot) -> Option<String> {
        let id = match slot {
            PluginSlot::Instrument => track.instrument.as_ref().map(|i| i.plugin_id.as_str())?,
            PluginSlot::Fx(i) => track.fx_chain.get(i as usize).map(|p| p.plugin_id.as_str())?,
            PluginSlot::MidiFx(i) => track
                .midi_fx_chain
                .get(i as usize)
                .map(|p| p.plugin_id.as_str())?,
        };
        Some(self.resolve_name(id))
    }

    /// Remove the plugin at the given chain slot on the currently selected
    /// track. Sends `RemoveSlotPlugin` to the host and mirrors the change in
    /// the model so the Inspector refreshes immediately.
    fn remove_slot(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        let track_idx = self.cursor_track;
        tracing::info!(track = track_idx, ?slot, "removing plugin slot");
        self.send_plugin(MainToChild::RemoveSlotPlugin { track: track_idx, slot });
        if let Some(track) = self.song.tracks.get_mut(track_idx as usize) {
            match slot {
                PluginSlot::Instrument => {
                    track.instrument = None;
                    self.instrument_label = "(no instrument)".into();
                }
                PluginSlot::Fx(i) => {
                    let i = i as usize;
                    if i < track.fx_chain.len() {
                        track.fx_chain.remove(i);
                    }
                }
                PluginSlot::MidiFx(i) => {
                    let i = i as usize;
                    if i < track.midi_fx_chain.len() {
                        track.midi_fx_chain.remove(i);
                    }
                }
            }
        }
        self.refresh_inspector_chain();
    }

    /// `ChildToMain::AllPluginStates` reply. Each entry is dispatched to its
    /// own (track, slot) inside the model before the project file is written.
    /// Unsolicited replies (no pending save) are ignored silently.
    fn on_all_states_from_child(&mut self, states: Vec<SlotState>) {
        tracing::info!(
            count = states.len(),
            pending = self.pending_save_path.is_some(),
            "all plugin states reply received"
        );
        let Some(path) = self.pending_save_path.take() else {
            return;
        };
        self.finish_save(path, states);
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
                self.cursor_track,
                common::audio_bridge::SAMPLE_RATE,
                playhead_samples,
            )
        };
        if next_row != self.playhead_row {
            self.playhead_row = next_row;
        }

        // --- Plugin GUI ✕-button bridge ----------------------------------
        // Runs on the Vizia main thread so it can safely send IPC. Converts
        // the async WNDPROC signal into our synchronous close flow. Each
        // container polls independently so multi-GUI sessions close the
        // right editor.
        #[cfg(windows)]
        {
            let mut to_close: Vec<(u32, PluginSlot)> = Vec::new();
            for (&(track, slot), win) in &self.plugin_host_windows {
                if win.take_close_request() {
                    to_close.push((track, slot));
                }
            }
            for (track, slot) in to_close {
                tracing::info!(track, ?slot, "plugin host window ✕ clicked; forwarding to CloseSlotGui");
                self.send_plugin(MainToChild::CloseSlotGui { track, slot });
            }
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
    /// path from the database and adds the plugin to the currently selected
    /// track's chain (no state to restore: this is an explicit "new plugin"
    /// flow).
    fn select_plugin_from_db(&mut self, id: String) {
        self.is_plugin_picker_open = false;
        // Pull everything we need out of the DB before starting mutations
        // (self.plugin_db is an Arc, so we work with a cloned snapshot).
        let Some(db) = self.plugin_db.clone() else {
            tracing::warn!(id, "plugin_db not available");
            return;
        };
        let Some(entry) = db.find_by_id(&id) else {
            tracing::error!(id, "picked plugin id not in database (stale picker?)");
            return;
        };
        let path = entry.path.clone();
        let entry_label = plugin_label_from_entry(entry);
        let entry_id = entry.id.clone();
        let entry_format = entry.format;
        self.ensure_first_track();
        let track_idx = self.cursor_track;
        // Pick the destination slot based on what the user was adding,
        // using the selected track's current chain lengths for FX / MIDI FX
        // append positions.
        let target = self.plugin_picker_target;
        let dest_slot = match target {
            PickerTarget::Instrument => PluginSlot::Instrument,
            PickerTarget::Fx => {
                let next = self
                    .song
                    .tracks
                    .get(track_idx as usize)
                    .map(|t| t.fx_chain.len() as u32)
                    .unwrap_or(0);
                PluginSlot::Fx(next)
            }
            PickerTarget::MidiFx => {
                let next = self
                    .song
                    .tracks
                    .get(track_idx as usize)
                    .map(|t| t.midi_fx_chain.len() as u32)
                    .unwrap_or(0);
                PluginSlot::MidiFx(next)
            }
        };
        tracing::info!(
            track = track_idx,
            id = %entry_id,
            ?dest_slot,
            path = %path.display(),
            "user picked plugin"
        );
        self.send_plugin(MainToChild::SetSlotPlugin {
            track: track_idx,
            slot: dest_slot,
            format: entry_format,
            path,
            plugin_id: entry_id.clone(),
            initial_state: None,
        });
        // Update song model optimistically; the final id/name lands via the
        // `SlotPluginLoaded` callback from plugin_host.
        if let Some(track) = self.song.tracks.get_mut(track_idx as usize) {
            match dest_slot {
                PluginSlot::Instrument => {
                    track.instrument = Some(common::model::PluginInstance::new(
                        entry_id.clone(),
                        entry_format,
                    ));
                }
                PluginSlot::Fx(_) => {
                    track.fx_chain.push(common::model::PluginInstance::new(
                        entry_id.clone(),
                        entry_format,
                    ));
                }
                PluginSlot::MidiFx(_) => {
                    track.midi_fx_chain.push(common::model::PluginInstance::new(
                        entry_id.clone(),
                        entry_format,
                    ));
                }
            }
        }
        if matches!(dest_slot, PluginSlot::Instrument) {
            self.instrument_label = entry_label;
        }
        self.refresh_inspector_chain();
    }

    /// Rebuild `inspector_chain`, `selected_track_label`, and
    /// `instrument_label` from the track at `cursor_track`. Called after any
    /// chain mutation on that track, or when the cursor moves to a
    /// different track.
    fn refresh_inspector_chain(&mut self) {
        let mut out: Vec<ChainEntry> = Vec::new();
        let track_idx = self.cursor_track as usize;
        let Some(track) = self.song.tracks.get(track_idx) else {
            self.inspector_chain = out;
            self.selected_track_label = format!("Track {}", self.cursor_track + 1);
            self.instrument_label = "(no instrument)".into();
            return;
        };
        self.selected_track_label = if track.name.is_empty() {
            format!("Track {}", self.cursor_track + 1)
        } else {
            track.name.clone()
        };
        self.instrument_label = match track.instrument.as_ref() {
            Some(inst) => self.resolve_name(&inst.plugin_id),
            None => "(no instrument)".into(),
        };
        for (i, p) in track.midi_fx_chain.iter().enumerate() {
            out.push(ChainEntry {
                slot_kind: 0,
                slot_index: i as u32,
                section_label: "MIDI FX".into(),
                plugin_name: self.resolve_name(&p.plugin_id),
            });
        }
        if let Some(inst) = track.instrument.as_ref() {
            out.push(ChainEntry {
                slot_kind: 1,
                slot_index: 0,
                section_label: "Instrument".into(),
                plugin_name: self.resolve_name(&inst.plugin_id),
            });
        }
        for (i, p) in track.fx_chain.iter().enumerate() {
            out.push(ChainEntry {
                slot_kind: 2,
                slot_index: i as u32,
                section_label: "FX".into(),
                plugin_name: self.resolve_name(&p.plugin_id),
            });
        }
        self.inspector_chain = out;
    }

    fn action_export_wav(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV", &["wav"])
            .save_file()
        else {
            return;
        };
        self.status_message = "WAV 書き出し中...".to_string();
        // Send the song + path to plugin_host for offline render.
        self.send_plugin(MainToChild::LoadSong(self.song.clone()));
        self.send_plugin(MainToChild::ExportWav { path });
    }

    /// Open the lyric editor for the current cursor row, pre-filling
    /// the textbox with any existing lyric.
    fn start_lyric_edit(&mut self) {
        let track_idx = self.cursor_track as usize;
        let row_idx = self.cursor_row as usize;
        let existing = self
            .song
            .tracks
            .get(track_idx)
            .and_then(|t| t.clips.first())
            .and_then(|c| c.rows.get(row_idx))
            .and_then(|r| r.lyric.clone())
            .unwrap_or_default();
        self.lyric_edit_text = existing;
        self.lyric_editing = true;
    }

    /// Commit the lyric editor text to the current row, advance the
    /// cursor, and keep editing (Enter = next row).
    fn submit_lyric_edit(&mut self) {
        let track_idx = self.cursor_track as usize;
        let row_idx = self.cursor_row as usize;
        if let Some(track) = self.song.tracks.get_mut(track_idx)
            && let Some(clip) = track.clips.first_mut()
        {
            while clip.rows.len() <= row_idx {
                clip.rows.push(Row::default());
            }
            let text = self.lyric_edit_text.trim().to_string();
            clip.rows[row_idx].lyric = if text.is_empty() { None } else { Some(text) };
        }
        // Refresh tracker display so the lyric appears immediately.
        self.refresh_tracker_text();
        // Advance cursor and keep editing the next row.
        self.move_cursor_row(1);
        self.start_lyric_edit();
    }

    /// Synthesize all vocal tracks in the background. Results are
    /// delivered via `VocalSynthCompleted` → `finish_vocal_synth`.
    fn begin_vocal_synth(&self, cx: &mut EventContext) {
        let song = self.song.clone();
        let slot = Arc::clone(&self.synth_result);
        cx.spawn(move |proxy| {
            tracing::info!("VOICEVOX synthesis starting");
            let results = common::voicevox::synthesize_song(
            &song,
            common::voicevox::DEFAULT_SINGER_ID,
            common::voicevox::DEFAULT_SINGER_ID,
        );
            tracing::info!(count = results.len(), "VOICEVOX synthesis finished");
            if let Ok(mut guard) = slot.lock() {
                *guard = results;
            }
            let _ = proxy.emit(AppEvent::VocalSynthCompleted);
        });
    }

    /// Take synthesis results and forward the rendered audio to
    /// plugin_host via IPC so the audio thread can mix it in.
    fn finish_vocal_synth(&mut self) {
        let results: Vec<common::voicevox::SynthResult> = self
            .synth_result
            .lock()
            .ok()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();

        if results.is_empty() {
            tracing::warn!("vocal synth produced no results");
            // Pull last error from the synth result slot (errors are
            // appended as empty-samples entries with an error field).
            let errors: Vec<String> = self
                .synth_result
                .lock()
                .ok()
                .map(|g| g.iter().filter_map(|r| r.error.clone()).collect())
                .unwrap_or_default();
            if errors.is_empty() {
                self.status_message =
                    "合成結果なし（Vocal トラックがないか VOICEVOX が応答しません）".to_string();
            } else {
                self.status_message = format!("合成エラー: {}", errors.join("; "));
            }
            return;
        }

        let ok_results: Vec<_> = results.iter().filter(|r| r.error.is_none()).collect();
        let err_count = results.len() - ok_results.len();
        if err_count > 0 {
            let first_err = results.iter().find_map(|r| r.error.as_deref()).unwrap_or("不明");
            self.status_message = format!(
                "合成: {} 成功, {} 失敗 ({})",
                ok_results.len(),
                err_count,
                first_err
            );
        } else {
            self.status_message = format!("合成完了 — {} クリップ。Play で再生", ok_results.len());
        }

        for r in &ok_results {
            // Compute the absolute sample offset for the clip's start
            // beat. This is where the audio thread should begin playing
            // the rendered buffer.
            let clip_start_beat = self
                .song
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0);
            let samples_per_beat =
                common::audio_bridge::SAMPLE_RATE as f64 * 60.0 / self.song.bpm as f64;
            let clip_start_samples = (clip_start_beat * samples_per_beat).max(0.0) as u64;

            tracing::info!(
                track = r.track,
                clip = r.clip,
                len = r.samples.len(),
                clip_start_samples,
                "sending vocal audio to plugin_host"
            );
            self.send_plugin(MainToChild::SetVocalAudio {
                track: r.track,
                clip: r.clip,
                clip_start_samples,
                sample_rate: r.sample_rate,
                samples: r.samples.clone(),
            });
        }
    }

    /// Kick off a background plugin rescan. Safe to call twice — if a
    /// scan is already running, the second request is ignored so the
    /// worker stays single-threaded (no race on `rescan_result`).
    fn begin_rescan(&mut self, cx: &mut EventContext) {
        if self.is_rescanning {
            tracing::info!("plugin rescan already in flight; ignoring");
            return;
        }
        self.is_rescanning = true;
        let slot = Arc::clone(&self.rescan_result);
        cx.spawn(move |proxy| {
            tracing::info!("plugin_db rescan starting");
            match common::plugin_db::scan_system() {
                Ok(db) => {
                    tracing::info!(
                        count = db.entries.len(),
                        "plugin_db rescan completed"
                    );
                    if let Some(cache) = common::plugin_db::default_cache_path()
                        && let Err(e) = db.save_to_file(&cache)
                    {
                        tracing::warn!(
                            error = ?e,
                            path = %cache.display(),
                            "failed to persist rescanned plugin_db"
                        );
                    }
                    if let Ok(mut guard) = slot.lock() {
                        *guard = Some(db);
                    }
                    let _ = proxy.emit(AppEvent::PluginDbRescanCompleted);
                }
                Err(e) => {
                    tracing::error!(error = ?e, "plugin rescan failed");
                    // Still post completion so the UI clears its
                    // "rescanning" state; the result slot stays empty, so
                    // `finish_rescan` treats it as a no-op.
                    let _ = proxy.emit(AppEvent::PluginDbRescanCompleted);
                }
            }
        });
    }

    /// Pull the freshly-scanned database out of `rescan_result` and swap
    /// it in. Called from the UI thread via `PluginDbRescanCompleted`.
    fn finish_rescan(&mut self) {
        self.is_rescanning = false;
        let Some(new_db) = self.rescan_result.lock().ok().and_then(|mut g| g.take()) else {
            return;
        };
        let new_db = Arc::new(new_db);
        tracing::info!(count = new_db.entries.len(), "applied rescanned plugin_db");
        self.plugin_db = Some(new_db);
        self.rebuild_picker_entries();
        self.refresh_picker_visible();
    }

    /// Apply a volume fader change. Updates the model, the lens-visible
    /// mixer row, and forwards to plugin_host so the audio thread picks
    /// up the new value on its next buffer.
    fn set_track_volume(&mut self, track: u32, volume: f32) {
        let v = volume.clamp(0.0, 1.0);
        if let Some(t) = self.song.tracks.get_mut(track as usize) {
            t.volume = v;
        }
        if let Some(entry) = self.track_mix.iter_mut().find(|e| e.index == track) {
            entry.volume = v;
        }
        self.send_plugin(MainToChild::SetTrackVolume { track, volume: v });
    }

    fn set_track_pan(&mut self, track: u32, pan: f32) {
        let p = pan.clamp(-1.0, 1.0);
        if let Some(t) = self.song.tracks.get_mut(track as usize) {
            t.pan = p;
        }
        if let Some(entry) = self.track_mix.iter_mut().find(|e| e.index == track) {
            entry.pan = p;
        }
        self.send_plugin(MainToChild::SetTrackPan { track, pan: p });
    }

    fn toggle_track_mute(&mut self, track: u32) {
        let Some(t) = self.song.tracks.get_mut(track as usize) else {
            return;
        };
        t.muted = !t.muted;
        let muted = t.muted;
        if let Some(entry) = self.track_mix.iter_mut().find(|e| e.index == track) {
            entry.muted = muted;
        }
        self.send_plugin(MainToChild::SetTrackMuted { track, muted });
    }

    fn toggle_track_solo(&mut self, track: u32) {
        let Some(t) = self.song.tracks.get_mut(track as usize) else {
            return;
        };
        t.solo = !t.solo;
        let solo = t.solo;
        if let Some(entry) = self.track_mix.iter_mut().find(|e| e.index == track) {
            entry.solo = solo;
        }
        self.send_plugin(MainToChild::SetTrackSolo { track, solo });
    }

    /// Per-tick post-fader peak integration. Runs the same
    /// fast-attack/slow-release curve the master meter uses, then maps
    /// linear → dB → 0..1 for the lens-visible entries.
    fn on_track_peaks_tick(&mut self, peaks: &[(u32, u32)]) {
        const RELEASE: f32 = 0.85;
        let n = self.song.tracks.len();
        if self.track_peak_display.len() != n {
            self.track_peak_display.resize(n, (0.0, 0.0));
        }
        for (i, display) in self.track_peak_display.iter_mut().enumerate() {
            let (l_bits, r_bits) = peaks.get(i).copied().unwrap_or((0u32, 0u32));
            let l = f32::from_bits(l_bits);
            let r = f32::from_bits(r_bits);
            display.0 = common::meter::update_peak(display.0, l, RELEASE);
            display.1 = common::meter::update_peak(display.1, r, RELEASE);
        }
        // Iterate by index to avoid overlapping borrows. We route through
        // `as_slice()` so `get(usize)` picks the `[T]` inherent method
        // rather than the blanket `vizia::Res::get(&impl DataContext)`
        // Vizia adds to any `Vec<T>` in scope.
        let display = self.track_peak_display.as_slice();
        let updates: Vec<(usize, f32, f32)> = (0..self.track_mix.len())
            .map(|i| {
                let (l, r) = display.get(i).copied().unwrap_or((0.0, 0.0));
                (i, l, r)
            })
            .collect();
        for (i, l, r) in updates {
            if let Some(entry) = self.track_mix.get_mut(i) {
                entry.peak_l_norm = common::meter::db_to_norm(common::meter::linear_to_db(l));
                entry.peak_r_norm = common::meter::db_to_norm(common::meter::linear_to_db(r));
            }
        }
    }

    /// Rebuild `track_mix` from `song.tracks`. Call whenever the number or
    /// order of tracks (or any of their mixer fields via undo/LoadSong)
    /// changes. Peak fields start at zero and are refreshed by the next
    /// tick.
    fn rebuild_track_mix(&mut self) {
        self.track_mix = self
            .song
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackMixEntry {
                index: i as u32,
                name: if t.name.is_empty() {
                    format!("Track {}", i + 1)
                } else {
                    t.name.clone()
                },
                volume: t.volume,
                pan: t.pan,
                muted: t.muted,
                solo: t.solo,
                peak_l_norm: 0.0,
                peak_r_norm: 0.0,
            })
            .collect();
        self.track_peak_display
            .resize(self.song.tracks.len(), (0.0, 0.0));
    }

    /// Rebuild the flat `plugin_picker_entries` list from `plugin_db`.
    /// Called after a rescan to keep the picker in sync with the new DB.
    fn rebuild_picker_entries(&mut self) {
        let Some(db) = self.plugin_db.as_ref() else {
            self.plugin_picker_entries.clear();
            return;
        };
        let mut v: Vec<PluginPickEntry> = db
            .entries
            .iter()
            .map(|e| PluginPickEntry {
                id: e.id.clone(),
                name: if e.name.is_empty() {
                    e.id.clone()
                } else {
                    e.name.clone()
                },
                vendor: e.vendor.clone(),
                features: e.features.clone(),
                format_label: e.format.as_str().to_string(),
            })
            .collect();
        v.sort_by_key(|e| e.name.to_lowercase());
        self.plugin_picker_entries = v;
    }

    /// Rebuild `plugin_picker_visible` from `plugin_picker_entries` using
    /// the current `plugin_picker_target` feature as the filter key.
    fn refresh_picker_visible(&mut self) {
        let feature_key: &str = match self.plugin_picker_target {
            PickerTarget::Instrument => "instrument",
            PickerTarget::Fx => "audio-effect",
            PickerTarget::MidiFx => "note-effect",
        };
        self.plugin_picker_visible = self
            .plugin_picker_entries
            .iter()
            .filter(|e| e.features.iter().any(|f| f == feature_key))
            .cloned()
            .collect();
    }

    fn resolve_name(&self, plugin_id: &str) -> String {
        self.plugin_db
            .as_deref()
            .and_then(|db| db.find_by_id(plugin_id))
            .map(|e| if e.name.is_empty() { plugin_id.to_string() } else { e.name.clone() })
            .unwrap_or_else(|| plugin_id.to_string())
    }

    fn ensure_first_track(&mut self) {
        if self.song.tracks.is_empty() {
            self.song.tracks.push(Track {
                name: "Track 1".into(),
                clips: vec![demo_clip()],
                ..Track::default()
            });
            self.rebuild_track_mix();
        }
    }

    fn action_add_vocal_track(&mut self) {
        let index = self.song.tracks.len() + 1;
        let track = Track {
            name: format!("Track {index}"),
            source: InstrumentSource::Vocal {
                speaker_id: common::voicevox::DEFAULT_SINGER_ID,
                style_name: "ノーマル".into(),
            },
            clips: vec![demo_clip()],
            ..Track::default()
        };
        self.song.tracks.push(track);
        self.rebuild_track_mix();
        tracing::info!(index, "added vocal track");
    }

    fn action_remove_last_track(&mut self) {
        if self.song.tracks.is_empty() {
            return;
        }
        let removed_idx = (self.song.tracks.len() - 1) as u32;
        if let Some(track) = self.song.tracks.pop() {
            tracing::info!(
                index = removed_idx,
                name = %track.name,
                "removed last track"
            );
        }
        // Close any plugin editor windows that belong to this track before
        // the host tears down its chain — Drop on PluginHostWindow destroys
        // the HWND.
        #[cfg(windows)]
        {
            self.plugin_host_windows
                .retain(|&(t, _), _| t != removed_idx);
        }
        // Notify plugin_host so it drops the whole chain; otherwise its
        // audio thread keeps rendering the removed track.
        self.send_plugin(MainToChild::RemoveTrack { track: removed_idx });
        self.clamp_cursor();
        self.refresh_inspector_chain();
        self.rebuild_track_mix();
    }

    fn move_cursor_track(&mut self, delta: i32) {
        let max = self.song.tracks.len().saturating_sub(1) as i64;
        let next = (self.cursor_track as i64 + delta as i64).clamp(0, max.max(0));
        if next as u32 != self.cursor_track {
            self.cursor_track = next as u32;
            self.ensure_cursor_visible();
            // Selected-track change → Inspector must show the new track's
            // chain and labels.
            self.refresh_inspector_chain();
        }
    }

    /// Adjust `visible_track_start` so `cursor_track` is within the
    /// visible window. Called after every cursor-track change.
    fn ensure_cursor_visible(&mut self) {
        let ct = self.cursor_track;
        let count = self.visible_track_count;
        if ct < self.visible_track_start {
            self.visible_track_start = ct;
        } else if ct >= self.visible_track_start + count {
            self.visible_track_start = ct - count + 1;
        }
        self.mixer_visible_range = (
            self.visible_track_start,
            self.visible_track_start + count,
        );
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

#[allow(dead_code)]
fn plugin_label(path: Option<&Path>) -> String {
    match path {
        Some(p) => p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.display().to_string()),
        None => "(no plugin)".into(),
    }
}

/// Freestanding variant of `AppData::rebuild_track_mix` usable from
/// `AppData::new` before `self` exists. Populates the lens-visible mixer
/// rows from a `Song` with zeroed peak meters.
fn initial_track_mix(song: &Song) -> Vec<TrackMixEntry> {
    song.tracks
        .iter()
        .enumerate()
        .map(|(i, t)| TrackMixEntry {
            index: i as u32,
            name: if t.name.is_empty() {
                format!("Track {}", i + 1)
            } else {
                t.name.clone()
            },
            volume: t.volume,
            pan: t.pan,
            muted: t.muted,
            solo: t.solo,
            peak_l_norm: 0.0,
            peak_r_norm: 0.0,
        })
        .collect()
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
        note(67, "わ"),
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
        name: "こんにちわ".into(),
        start_beat: 0.0,
        length_beats: 4.0,
        rows_per_beat: 4,
        rows,
    }
}
