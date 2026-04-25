//! Bitwig-style DAW GUI state.
//!
//! The app state has three top-level pieces:
//!   1. The **song** — a tree of `Track → Clip → Note`. Mutated by every
//!      edit; pushed to `daw_plugin_host` on `Play`.
//!   2. **Selection state** — what track / clip / notes the user has
//!      currently picked. Drives the inspector, the piano roll's
//!      contents, and the lyric panel.
//!   3. **View state** — zoom, scroll, playhead, peak meters. Lens-bound
//!      so views can render without taking a callback through `cx`.
//!
//! Drag interactions live inside the views (arrangement / piano roll).
//! Each view tracks the in-progress drag locally and only emits a single
//! commit event (`MoveClip`, `MoveNote`, etc.) on `MouseUp`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::model::{Clip, InstrumentSource, Note, Song, Track};
use common::plugin_db::PluginDatabase;
use common::plugin_format::PluginFormat;
use common::protocol::{MainToChild, PluginSlot, SlotState};
use tokio::sync::mpsc::UnboundedSender;
use vizia::prelude::*;

/// Per-track mixer strip row bound to the bottom-panel mixer view.
/// Rebuilt from `song.tracks` whenever the track list or a mixer parameter
/// changes; the `peak_*_norm` fields are refreshed on every UI tick from
/// the shmem-published post-fader peaks.
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
    pub features: Vec<String>,
    pub format_label: String,
}

/// A single slot on the inspected track's chain.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Data)]
pub enum PickerTarget {
    Instrument,
    Fx,
    #[allow(dead_code)]
    MidiFx,
}

/// Drop-target for the inspector + piano roll.
///
/// `Track`-only is selected by clicking a track header; `(Track, Clip)` is
/// selected by clicking a clip in the arrangement view, which also drives
/// the piano roll's contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Data, Default)]
pub struct ClipRef {
    pub track: u32,
    pub clip: u32,
}

/// Render-friendly snapshot of one clip on the timeline. Rebuilt whenever
/// the song changes; the arrangement view binds to the `Vec<ClipBox>` lens
/// and renders each entry as a coloured rectangle.
#[derive(Debug, Clone, PartialEq, Data)]
pub struct ClipBox {
    pub track: u32,
    pub clip: u32,
    pub name: String,
    pub start_beat: f32,
    pub length_beats: f32,
    pub selected: bool,
}

/// Render-friendly snapshot of one note inside the currently selected clip.
/// Rebuilt whenever the clip's note list or selection changes; the piano
/// roll binds to the `Vec<NoteBox>` lens.
#[derive(Debug, Clone, PartialEq, Data)]
pub struct NoteBox {
    pub note: u32,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub pitch: u8,
    pub lyric: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Data)]
pub struct TrackHeader {
    pub index: u32,
    pub name: String,
    pub muted: bool,
    pub solo: bool,
    pub selected: bool,
}

/// Width of the arrangement view at 1× zoom expressed in pixels per beat.
/// Mirrors Meadowlark's `POINTS_PER_BEAT = 100.0`.
pub const ARRANGE_PX_PER_BEAT: f32 = 24.0;
/// Vertical pixels per arrangement track row.
pub const ARRANGE_TRACK_HEIGHT: f32 = 56.0;
/// Default note duration (in beats) when the user double-clicks empty
/// piano-roll space to place a new note. 1/4 beat = 1/16 note at 4/4.
pub const DEFAULT_NOTE_DURATION: f64 = 0.25;
/// Default clip length (in beats) when the user double-clicks empty
/// arrangement space to create a new clip.
pub const DEFAULT_CLIP_LENGTH: f64 = 4.0;

#[derive(Lens)]
pub struct AppData {
    pub song: Song,
    pub file_path: Option<PathBuf>,

    // -------- Selection -----------------------------------------------------
    /// Track that the inspector + plugin picker target. Always valid against
    /// `song.tracks` (clamped on track removal). Rebuilt from the
    /// arrangement view's "selected track header" interaction.
    pub selected_track: u32,
    /// Currently selected clip. `None` means "no clip selected" — the piano
    /// roll then shows an empty placeholder.
    pub selected_clip: Option<ClipRef>,
    /// Notes selected within `selected_clip`. Indices into the clip's
    /// `notes` vector. Multi-select isn't wired up in v1 but the field is
    /// a `Vec` so it's ready for it.
    pub selected_notes: Vec<u32>,

    // -------- View state ----------------------------------------------------
    /// Bottom panel selector: `0 = Mixer`, `1 = Piano Roll`. Bound to a
    /// `TabView` selected_index lens.
    pub bottom_panel: u8,
    /// Horizontal zoom on the arrangement view (px / beat).
    pub arrange_zoom_x: f32,
    /// Beat at the left edge of the arrangement view. Mouse wheel +
    /// Shift+wheel pan this; Ctrl+wheel zooms `arrange_zoom_x` instead.
    pub arrange_scroll_beat: f32,
    /// Horizontal zoom on the piano roll (px / beat).
    pub pianoroll_zoom_x: f32,
    /// Vertical zoom on the piano roll (px / semitone).
    pub pianoroll_zoom_y: f32,
    /// MIDI pitch shown at the top edge of the piano roll. Decreasing
    /// scrolls down (towards lower pitches).
    pub pianoroll_top_pitch: u8,
    /// Beat at the left edge of the piano roll.
    pub pianoroll_scroll_beat: f32,

    // -------- Cached lens-bound snapshots -----------------------------------
    /// Per-track header strip on the left of the arrangement view.
    pub track_headers: Vec<TrackHeader>,
    /// Mirror of `song.tracks.len()` for slot-visibility lenses.
    pub track_count: u32,
    /// All clips in the song, flattened for the arrangement view.
    pub clip_boxes: Vec<ClipBox>,
    /// Notes in `selected_clip` (empty if no clip selected).
    pub note_boxes: Vec<NoteBox>,
    /// Lyric of the first selected note, or empty when nothing is selected.
    /// The lyric panel binds an editable Textbox to this.
    pub selected_lyric: String,

    // -------- Playback / metering ------------------------------------------
    pub is_playing: bool,
    pub is_looping: bool,
    /// Current playhead in beats (relative to song origin). `None` when the
    /// audio thread published the "not playing" sentinel.
    pub playhead_beat: Option<f32>,
    pub master_gain: f32,
    pub peak_l_display: f32,
    pub peak_r_display: f32,
    pub peak_l_norm: f32,
    pub peak_r_norm: f32,

    // -------- Plugin database / picker -------------------------------------
    #[lens(ignore)]
    pub plugin_db: Option<Arc<PluginDatabase>>,
    pub plugin_picker_entries: Vec<PluginPickEntry>,
    pub plugin_picker_visible: Vec<PluginPickEntry>,
    pub is_plugin_picker_open: bool,
    pub plugin_picker_target: PickerTarget,
    pub inspector_chain: Vec<ChainEntry>,
    pub selected_track_label: String,
    pub instrument_label: String,

    // -------- Save flow / IPC ----------------------------------------------
    #[lens(ignore)]
    pub pending_save_path: Option<PathBuf>,
    #[lens(ignore)]
    pub audio_tx: Option<UnboundedSender<MainToChild>>,
    #[lens(ignore)]
    pub plugin_tx: Option<UnboundedSender<MainToChild>>,
    #[cfg(windows)]
    #[lens(ignore)]
    pub plugin_host_windows:
        HashMap<(u32, PluginSlot), crate::view::plugin_embed::PluginHostWindow>,

    // -------- Mixer ---------------------------------------------------------
    pub track_mix: Vec<TrackMixEntry>,
    #[lens(ignore)]
    pub track_peak_display: Vec<(f32, f32)>,

    // -------- Background workers -------------------------------------------
    #[lens(ignore)]
    pub synth_result: Arc<Mutex<Vec<common::voicevox::SynthResult>>>,
    #[lens(ignore)]
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    pub is_rescanning: bool,
    pub status_message: String,

    /// Inline rename state. `Some(track_idx)` means the track header for
    /// that track is currently in edit mode and should render a Textbox
    /// bound to `track_rename_text` instead of the regular click button.
    pub track_rename_idx: Option<u32>,
    pub track_rename_text: String,
}

impl AppData {
    pub fn new(
        audio_tx: UnboundedSender<MainToChild>,
        plugin_tx: UnboundedSender<MainToChild>,
        // Path reserved for future auto-select; currently not wired to song.
        _clap_plugin_path: Option<PathBuf>,
        plugin_db: Option<Arc<PluginDatabase>>,
    ) -> Self {
        // Start with a single empty instrument track so the arrangement
        // and mixer never go through the 0→N transition (Vizia's morphorm
        // layout can produce non-invertible matrices when a list flips
        // from empty to non-empty mid-frame; CLAUDE.md draw.rs:35 panic).
        let song = Song {
            tracks: vec![Track {
                name: "Track 1".into(),
                ..Track::default()
            }],
            ..Song::default()
        };
        let track_count = song.tracks.len() as u32;
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
            selected_track: 0,
            selected_clip: None,
            selected_notes: Vec::new(),
            bottom_panel: 0,
            arrange_zoom_x: ARRANGE_PX_PER_BEAT,
            arrange_scroll_beat: 0.0,
            pianoroll_zoom_x: 64.0,
            pianoroll_zoom_y: 14.0,
            pianoroll_top_pitch: 84, // C6
            pianoroll_scroll_beat: 0.0,
            track_headers: Vec::new(),
            track_count,
            clip_boxes: Vec::new(),
            note_boxes: Vec::new(),
            selected_lyric: String::new(),
            is_playing: false,
            is_looping: false,
            playhead_beat: None,
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
            track_rename_idx: None,
            track_rename_text: String::new(),
        }
    }

    /// Recompute every cached lens-visible snapshot (`track_headers`,
    /// `clip_boxes`, `note_boxes`, `selected_lyric`, `track_count`) from
    /// `song` + selection state.
    fn refresh_caches(&mut self) {
        self.track_count = self.song.tracks.len() as u32;
        self.track_headers = self
            .song
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackHeader {
                index: i as u32,
                name: if t.name.is_empty() {
                    format!("Track {}", i + 1)
                } else {
                    t.name.clone()
                },
                muted: t.muted,
                solo: t.solo,
                selected: i as u32 == self.selected_track,
            })
            .collect();
        let selected_clip = self.selected_clip;
        self.clip_boxes = self
            .song
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(t_idx, t)| {
                t.clips
                    .iter()
                    .enumerate()
                    .map(move |(c_idx, c)| ClipBox {
                        track: t_idx as u32,
                        clip: c_idx as u32,
                        name: c.name.clone(),
                        start_beat: c.start_beat as f32,
                        length_beats: c.length_beats as f32,
                        selected: selected_clip
                            == Some(ClipRef {
                                track: t_idx as u32,
                                clip: c_idx as u32,
                            }),
                    })
            })
            .collect();
        // Notes for the currently selected clip.
        let selected_notes = &self.selected_notes;
        self.note_boxes = match self.selected_clip {
            Some(ClipRef { track, clip }) => self
                .song
                .tracks
                .get(track as usize)
                .and_then(|t| t.clips.get(clip as usize))
                .map(|c| {
                    c.notes
                        .iter()
                        .enumerate()
                        .map(|(i, n)| NoteBox {
                            note: i as u32,
                            start_beat: n.start_beat as f32,
                            duration_beats: n.duration_beats as f32,
                            pitch: n.pitch,
                            lyric: n.lyric.clone().unwrap_or_default(),
                            selected: selected_notes.contains(&(i as u32)),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        };
        self.selected_lyric = self
            .selected_notes
            .first()
            .copied()
            .and_then(|n_idx| {
                let r = self.selected_clip?;
                let track = self.song.tracks.get(r.track as usize)?;
                let clip = track.clips.get(r.clip as usize)?;
                let note = clip.notes.get(n_idx as usize)?;
                Some(note.lyric.clone().unwrap_or_default())
            })
            .unwrap_or_default();
    }
}

/// Unpack an `f64` carried inside an `AppEvent` variant. AppEvent needs
/// `Eq + Hash`, which f64 lacks; senders use `f64::to_bits` and the
/// handler reads them back through here.
fn from_f64_bits(b: u64) -> f64 {
    f64::from_bits(b)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AppEvent {
    // -------- File / playback ---------------------------------------------
    New,
    Open,
    Save,
    SaveAs,
    Play,
    Stop,
    PlayToggle,
    ToggleLoop,
    AddVocalTrack,
    AddInstrumentTrack,
    RemoveLastTrack,
    SelectTrack(u32),
    /// Open the inline rename editor for the given track. Empties out any
    /// previously open editor first.
    BeginRenameTrack(u32),
    /// Textbox `on_edit` callback while renaming.
    RenameTrackChanged(String),
    /// Enter key — apply the new name to the song model.
    CommitRenameTrack,
    /// Esc key — discard the in-progress edit.
    CancelRenameTrack,

    // -------- Bottom panel -------------------------------------------------
    SelectBottomPanel(u8),

    // -------- Arrangement / clip operations -------------------------------
    SelectClip(ClipRef),
    ClearSelection,
    /// `start_beat_bits` = `f64::to_bits` of the new clip start.
    MoveClip {
        target: ClipRef,
        start_beat_bits: u64,
    },
    /// `length_bits` = `f64::to_bits` of the new clip length.
    ResizeClip {
        target: ClipRef,
        length_bits: u64,
    },
    /// Create a new empty clip on `track` starting at `start_beat`. Length
    /// defaults to `DEFAULT_CLIP_LENGTH`.
    CreateClip {
        track: u32,
        start_beat_bits: u64,
    },
    DeleteSelectedClip,

    // -------- Piano roll / note operations --------------------------------
    SelectNote {
        note: u32,
        additive: bool,
    },
    ClearNoteSelection,
    AddNote {
        track: u32,
        clip: u32,
        start_beat_bits: u64,
        duration_bits: u64,
        pitch: u8,
    },
    MoveNote {
        track: u32,
        clip: u32,
        note: u32,
        start_beat_bits: u64,
        pitch: u8,
    },
    ResizeNote {
        track: u32,
        clip: u32,
        note: u32,
        duration_bits: u64,
    },
    DeleteSelectedNotes,
    SetSelectedNoteLyric(String),

    // -------- Plugin picker / chain ---------------------------------------
    OpenPluginPickerFor(PickerTarget),
    ClosePluginPicker,
    SelectPluginFromDb(String),
    ToggleSlotGui {
        slot_kind: u8,
        slot_index: u32,
    },
    RemoveSlot {
        slot_kind: u8,
        slot_index: u32,
    },
    SetMasterGain(u32),

    // -------- IPC events from plugin_host ---------------------------------
    Tick(u64, u32, u32),
    GuiOpenedFromChild {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    GuiRequestResizeFromChild {
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    },
    GuiClosedFromChild {
        track: u32,
        slot: PluginSlot,
    },
    SlotPluginLoadedFromChild {
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
    },
    AllStatesReceived(Vec<SlotState>),
    RescanPluginDb,
    PluginDbRescanCompleted,

    // -------- Scroll / zoom -----------------------------------------------
    /// Update arrangement scroll (beat at left edge). Bits = `f32::to_bits`.
    SetArrangeScroll(u32),
    /// Update arrangement horizontal zoom (px/beat). Bits = `f32::to_bits`.
    SetArrangeZoom(u32),
    /// Update piano roll horizontal scroll (beat at left edge).
    SetPianoRollScrollX(u32),
    /// Update piano roll top pitch (highest pitch shown at top).
    SetPianoRollTopPitch(u8),
    /// Update piano roll horizontal zoom (px/beat).
    SetPianoRollZoomX(u32),
    /// Update piano roll vertical zoom (px/semitone).
    SetPianoRollZoomY(u32),

    // -------- Mixer -------------------------------------------------------
    SetTrackVolume {
        track: u32,
        bits: u32,
    },
    SetTrackPan {
        track: u32,
        bits: u32,
    },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    TrackPeaksTick(Vec<(u32, u32)>),

    // -------- VOICEVOX ----------------------------------------------------
    SynthesizeVocal,
    VocalSynthCompleted,

    // -------- Export ------------------------------------------------------
    ExportWav,
    ExportWavComplete {
        error: Option<String>,
    },
}

impl Model for AppData {
    fn event(&mut self, cx: &mut EventContext, event: &mut Event) {
        event.map(|window_event, _| {
            if let WindowEvent::WindowClose = window_event {
                tracing::info!("window close requested");
            }
        });
        event.map(|app_event, _| {
            // `dirty` decides whether `refresh_caches` runs at the end of
            // the handler. Most edits set it true; pure metering / IPC
            // replies leave it false to avoid pointless re-renders.
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
                AppEvent::ToggleLoop => {
                    self.toggle_loop();
                    dirty = false;
                }
                AppEvent::AddVocalTrack => self.action_add_vocal_track(),
                AppEvent::AddInstrumentTrack => self.action_add_instrument_track(),
                AppEvent::RemoveLastTrack => self.action_remove_last_track(),
                AppEvent::SelectTrack(idx) => self.select_track(*idx),
                AppEvent::BeginRenameTrack(idx) => {
                    self.begin_rename_track(*idx);
                    dirty = false;
                }
                AppEvent::RenameTrackChanged(text) => {
                    self.track_rename_text = text.clone();
                    dirty = false;
                }
                AppEvent::CommitRenameTrack => self.commit_rename_track(),
                AppEvent::CancelRenameTrack => {
                    self.track_rename_idx = None;
                    self.track_rename_text.clear();
                    dirty = false;
                }
                AppEvent::SelectBottomPanel(p) => {
                    self.bottom_panel = *p;
                    dirty = false;
                }
                AppEvent::SelectClip(target) => self.select_clip(Some(*target)),
                AppEvent::ClearSelection => {
                    self.selected_clip = None;
                    self.selected_notes.clear();
                }
                AppEvent::MoveClip {
                    target,
                    start_beat_bits,
                } => {
                    self.move_clip(*target, from_f64_bits(*start_beat_bits));
                }
                AppEvent::ResizeClip {
                    target,
                    length_bits,
                } => self.resize_clip(*target, from_f64_bits(*length_bits)),
                AppEvent::CreateClip {
                    track,
                    start_beat_bits,
                } => self.create_clip(*track, from_f64_bits(*start_beat_bits)),
                AppEvent::DeleteSelectedClip => self.delete_selected_clip(),
                AppEvent::SelectNote { note, additive } => {
                    self.select_note(*note, *additive);
                }
                AppEvent::ClearNoteSelection => self.selected_notes.clear(),
                AppEvent::AddNote {
                    track,
                    clip,
                    start_beat_bits,
                    duration_bits,
                    pitch,
                } => {
                    self.add_note(
                        *track,
                        *clip,
                        from_f64_bits(*start_beat_bits),
                        from_f64_bits(*duration_bits),
                        *pitch,
                    );
                }
                AppEvent::MoveNote {
                    track,
                    clip,
                    note,
                    start_beat_bits,
                    pitch,
                } => self.move_note(
                    *track,
                    *clip,
                    *note,
                    from_f64_bits(*start_beat_bits),
                    *pitch,
                ),
                AppEvent::ResizeNote {
                    track,
                    clip,
                    note,
                    duration_bits,
                } => self.resize_note(*track, *clip, *note, from_f64_bits(*duration_bits)),
                AppEvent::DeleteSelectedNotes => self.delete_selected_notes(),
                AppEvent::SetSelectedNoteLyric(text) => {
                    self.set_selected_note_lyric(text.clone());
                }
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
                AppEvent::SetArrangeScroll(bits) => {
                    self.arrange_scroll_beat = f32::from_bits(*bits).max(0.0);
                    dirty = false;
                }
                AppEvent::SetArrangeZoom(bits) => {
                    self.arrange_zoom_x = f32::from_bits(*bits).clamp(2.0, 400.0);
                    dirty = false;
                }
                AppEvent::SetPianoRollScrollX(bits) => {
                    self.pianoroll_scroll_beat = f32::from_bits(*bits).max(0.0);
                    dirty = false;
                }
                AppEvent::SetPianoRollTopPitch(p) => {
                    self.pianoroll_top_pitch = (*p).clamp(11, 127);
                    dirty = false;
                }
                AppEvent::SetPianoRollZoomX(bits) => {
                    self.pianoroll_zoom_x = f32::from_bits(*bits).clamp(8.0, 400.0);
                    dirty = false;
                }
                AppEvent::SetPianoRollZoomY(bits) => {
                    self.pianoroll_zoom_y = f32::from_bits(*bits).clamp(6.0, 40.0);
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
                    self.on_plugin_loaded_from_child(
                        *track,
                        *slot,
                        id.clone(),
                        name.clone(),
                    );
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
            }

            if dirty {
                self.refresh_caches();
            }
        });
    }
}

impl AppData {
    // -------- IPC -----------------------------------------------------------

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

    /// Push the current song to plugin_host so live edits (note add /
    /// move / clip drag etc.) are heard immediately during playback.
    /// `Play` already does this on each press, but pressing Play once and
    /// then editing for a while wouldn't refresh the audio thread's view
    /// of the song without this hook.
    fn sync_song_to_plugin_host(&self) {
        self.send_plugin(MainToChild::LoadSong(self.song.clone()));
    }

    // -------- File ----------------------------------------------------------

    fn action_new(&mut self) {
        self.song = Song::default();
        self.file_path = None;
        self.selected_track = 0;
        self.selected_clip = None;
        self.selected_notes.clear();
        self.refresh_inspector_chain();
        self.rebuild_track_mix();
        self.sync_song_to_plugin_host();
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
                self.restore_plugin_from_song(&song);
                self.song = song;
                self.file_path = Some(path);
                self.selected_track = 0;
                self.selected_clip = None;
                self.selected_notes.clear();
                self.refresh_inspector_chain();
                self.rebuild_track_mix();
                self.sync_song_to_plugin_host();
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project");
                self.status_message = format!("Open 失敗: {e:#}");
            }
        }
    }

    /// Resolves every plugin id on every track (MIDI FX → Instrument → FX)
    /// via the database and re-sends them with their persisted state.
    fn restore_plugin_from_song(&mut self, song: &Song) {
        let Some(db) = self.plugin_db.clone() else {
            tracing::warn!("plugin database not loaded; cannot resolve plugin ids");
            return;
        };
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

    fn begin_save(&mut self, path: PathBuf) {
        let has_plugin = self.song.tracks.iter().any(|t| {
            t.instrument.is_some() || !t.fx_chain.is_empty() || !t.midi_fx_chain.is_empty()
        });
        if has_plugin {
            self.pending_save_path = Some(path);
            self.send_plugin(MainToChild::RequestAllStates);
        } else {
            self.finish_save(path, Vec::new());
        }
    }

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

    // -------- Playback -----------------------------------------------------

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
        self.playhead_beat = None;
    }

    fn toggle_loop(&mut self) {
        self.is_looping = !self.is_looping;
        self.send_plugin(MainToChild::SetLoop(self.is_looping));
    }

    // -------- Track operations ---------------------------------------------

    fn select_track(&mut self, idx: u32) {
        if idx >= self.song.tracks.len() as u32 {
            return;
        }
        if self.selected_track != idx {
            self.selected_track = idx;
            self.refresh_inspector_chain();
        }
    }

    fn begin_rename_track(&mut self, idx: u32) {
        let Some(track) = self.song.tracks.get(idx as usize) else {
            return;
        };
        self.track_rename_text = track.name.clone();
        self.track_rename_idx = Some(idx);
    }

    fn commit_rename_track(&mut self) {
        let Some(idx) = self.track_rename_idx.take() else {
            return;
        };
        let new_name = self.track_rename_text.trim().to_string();
        self.track_rename_text.clear();
        if new_name.is_empty() {
            // An empty name is meaningless — treat as cancel.
            return;
        }
        if let Some(track) = self.song.tracks.get_mut(idx as usize) {
            track.name = new_name.clone();
        }
        if let Some(entry) = self.track_mix.iter_mut().find(|e| e.index == idx) {
            entry.name = new_name;
        }
        if idx == self.selected_track {
            self.refresh_inspector_chain();
        }
        self.sync_song_to_plugin_host();
    }

    fn ensure_first_track(&mut self) {
        if self.song.tracks.is_empty() {
            self.song.tracks.push(Track {
                name: "Track 1".into(),
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
        self.sync_song_to_plugin_host();
        tracing::info!(index, "added vocal track");
    }

    fn action_add_instrument_track(&mut self) {
        let index = self.song.tracks.len() + 1;
        let track = Track {
            name: format!("Track {index}"),
            source: InstrumentSource::None,
            clips: Vec::new(),
            ..Track::default()
        };
        self.song.tracks.push(track);
        self.rebuild_track_mix();
        self.sync_song_to_plugin_host();
        tracing::info!(index, "added instrument track");
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
        // the host tears down its chain.
        #[cfg(windows)]
        {
            self.plugin_host_windows
                .retain(|&(t, _), _| t != removed_idx);
        }
        self.send_plugin(MainToChild::RemoveTrack { track: removed_idx });
        // Clamp selection to remaining tracks.
        let new_max = self.song.tracks.len().saturating_sub(1) as u32;
        if self.song.tracks.is_empty() {
            self.selected_track = 0;
        } else if self.selected_track > new_max {
            self.selected_track = new_max;
        }
        if let Some(r) = self.selected_clip
            && r.track == removed_idx
        {
            self.selected_clip = None;
            self.selected_notes.clear();
        }
        self.refresh_inspector_chain();
        self.rebuild_track_mix();
        self.sync_song_to_plugin_host();
    }

    // -------- Clip operations ----------------------------------------------

    fn select_clip(&mut self, target: Option<ClipRef>) {
        self.selected_clip = target;
        self.selected_notes.clear();
        if let Some(r) = target {
            self.select_track(r.track);
        }
    }

    fn move_clip(&mut self, target: ClipRef, new_start_beat: f64) {
        let new_start_beat = new_start_beat.max(0.0);
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        clip.start_beat = new_start_beat;
        self.sync_song_to_plugin_host();
    }

    fn resize_clip(&mut self, target: ClipRef, new_length_beats: f64) {
        // Don't shrink to zero — Bitwig keeps a minimum of one bar; we use
        // 1/16 as a softer floor so VOICEVOX clips can be tight.
        let new_length_beats = new_length_beats.max(0.0625);
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        clip.length_beats = new_length_beats;
        self.sync_song_to_plugin_host();
    }

    fn create_clip(&mut self, track_idx: u32, start_beat: f64) {
        let start_beat = start_beat.max(0.0);
        let Some(track) = self.song.tracks.get_mut(track_idx as usize) else {
            return;
        };
        let new_clip = Clip {
            name: format!("Clip {}", track.clips.len() + 1),
            start_beat,
            length_beats: DEFAULT_CLIP_LENGTH,
            notes: Vec::new(),
        };
        let new_idx = track.clips.len() as u32;
        track.clips.push(new_clip);
        self.selected_clip = Some(ClipRef {
            track: track_idx,
            clip: new_idx,
        });
        self.selected_notes.clear();
        self.select_track(track_idx);
        self.sync_song_to_plugin_host();
    }

    fn delete_selected_clip(&mut self) {
        let Some(r) = self.selected_clip.take() else {
            return;
        };
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
            return;
        };
        if (r.clip as usize) < track.clips.len() {
            track.clips.remove(r.clip as usize);
        }
        self.selected_notes.clear();
        self.sync_song_to_plugin_host();
    }

    // -------- Note operations ----------------------------------------------

    fn select_note(&mut self, note: u32, additive: bool) {
        if !additive {
            self.selected_notes.clear();
        }
        if !self.selected_notes.contains(&note) {
            self.selected_notes.push(note);
        }
    }

    fn add_note(
        &mut self,
        track_idx: u32,
        clip_idx: u32,
        start_beat: f64,
        duration: f64,
        pitch: u8,
    ) {
        let start_beat = start_beat.max(0.0);
        let duration = duration.max(0.0625);
        let Some(track) = self.song.tracks.get_mut(track_idx as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(clip_idx as usize) else {
            return;
        };
        let new_idx = clip.notes.len() as u32;
        clip.notes.push(Note {
            start_beat,
            duration_beats: duration,
            pitch,
            velocity: 100,
            lyric: None,
        });
        self.selected_clip = Some(ClipRef {
            track: track_idx,
            clip: clip_idx,
        });
        self.selected_notes = vec![new_idx];
        self.sync_song_to_plugin_host();
    }

    fn move_note(
        &mut self,
        track_idx: u32,
        clip_idx: u32,
        note_idx: u32,
        new_start_beat: f64,
        new_pitch: u8,
    ) {
        let new_start_beat = new_start_beat.max(0.0);
        let Some(track) = self.song.tracks.get_mut(track_idx as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(clip_idx as usize) else {
            return;
        };
        let Some(note) = clip.notes.get_mut(note_idx as usize) else {
            return;
        };
        note.start_beat = new_start_beat;
        note.pitch = new_pitch;
        self.sync_song_to_plugin_host();
    }

    fn resize_note(
        &mut self,
        track_idx: u32,
        clip_idx: u32,
        note_idx: u32,
        new_duration: f64,
    ) {
        let new_duration = new_duration.max(0.0625);
        let Some(track) = self.song.tracks.get_mut(track_idx as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(clip_idx as usize) else {
            return;
        };
        let Some(note) = clip.notes.get_mut(note_idx as usize) else {
            return;
        };
        note.duration_beats = new_duration;
        self.sync_song_to_plugin_host();
    }

    fn delete_selected_notes(&mut self) {
        let Some(r) = self.selected_clip else { return };
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(r.clip as usize) else {
            return;
        };
        // Sort indices descending so each removal stays valid.
        let mut indices = self.selected_notes.clone();
        indices.sort_unstable_by(|a, b| b.cmp(a));
        for i in indices {
            let i = i as usize;
            if i < clip.notes.len() {
                clip.notes.remove(i);
            }
        }
        self.selected_notes.clear();
        self.sync_song_to_plugin_host();
    }

    fn set_selected_note_lyric(&mut self, lyric: String) {
        let Some(r) = self.selected_clip else { return };
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(r.clip as usize) else {
            return;
        };
        let trimmed = lyric.trim();
        let value = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        for &i in &self.selected_notes {
            if let Some(n) = clip.notes.get_mut(i as usize) {
                n.lyric = value.clone();
            }
        }
        self.sync_song_to_plugin_host();
    }

    // -------- Plugin GUI bridge --------------------------------------------

    #[cfg(windows)]
    fn on_gui_opened(&mut self, track: u32, slot: PluginSlot, width: u32, height: u32) {
        if let Some(win) = self.plugin_host_windows.get(&(track, slot)) {
            win.set_client_size(width, height);
        }
    }

    #[cfg(not(windows))]
    fn on_gui_opened(&mut self, _track: u32, _slot: PluginSlot, _width: u32, _height: u32) {}

    #[cfg(windows)]
    fn on_gui_request_resize(
        &mut self,
        track: u32,
        slot: PluginSlot,
        width: u32,
        height: u32,
    ) {
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

    #[cfg(windows)]
    fn on_gui_closed(&mut self, track: u32, slot: PluginSlot) {
        self.plugin_host_windows.remove(&(track, slot));
    }

    #[cfg(not(windows))]
    fn on_gui_closed(&mut self, _track: u32, _slot: PluginSlot) {}

    fn on_plugin_loaded_from_child(
        &mut self,
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
    ) {
        let label = if name.is_empty() { id.clone() } else { name };
        let track_idx = track as usize;
        self.ensure_first_track();
        let Some(t) = self.song.tracks.get_mut(track_idx) else {
            return;
        };
        match slot {
            PluginSlot::Instrument => {
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
                if track == self.selected_track {
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
        if track == self.selected_track {
            self.refresh_inspector_chain();
        }
    }

    fn toggle_slot_gui(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        let track = self.selected_track;
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

    fn remove_slot(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        let track_idx = self.selected_track;
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

    fn on_all_states_from_child(&mut self, states: Vec<SlotState>) {
        let Some(path) = self.pending_save_path.take() else {
            return;
        };
        self.finish_save(path, states);
    }

    // -------- Tick / metering ----------------------------------------------

    fn on_tick(&mut self, playhead_samples: u64, peak_l_raw: f32, peak_r_raw: f32) {
        // Playhead in beats for the arrangement view's red line.
        let next_beat = if playhead_samples == u64::MAX {
            None
        } else {
            common::timing::playhead_to_beat(
                Some(&self.song),
                common::audio_bridge::SAMPLE_RATE,
                playhead_samples,
            )
            .map(|b| b as f32)
        };
        if next_beat != self.playhead_beat {
            self.playhead_beat = next_beat;
        }

        // ✕-button bridge for embedded plugin windows.
        #[cfg(windows)]
        {
            let mut to_close: Vec<(u32, PluginSlot)> = Vec::new();
            for (&(track, slot), win) in &self.plugin_host_windows {
                if win.take_close_request() {
                    to_close.push((track, slot));
                }
            }
            for (track, slot) in to_close {
                self.send_plugin(MainToChild::CloseSlotGui { track, slot });
            }
        }

        // Peak meter — fast attack, exponential release. 0.85/tick at 30 Hz
        // is roughly -24 dB/s.
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

    fn set_master_gain(&mut self, gain: f32) {
        let clamped = gain.clamp(0.0, 1.0);
        self.master_gain = clamped;
        self.send_audio(MainToChild::SetMasterGain(clamped));
    }

    // -------- Plugin picker -----------------------------------------------

    fn select_plugin_from_db(&mut self, id: String) {
        self.is_plugin_picker_open = false;
        let Some(db) = self.plugin_db.clone() else {
            tracing::warn!(id, "plugin_db not available");
            return;
        };
        let Some(entry) = db.find_by_id(&id) else {
            tracing::error!(id, "picked plugin id not in database");
            return;
        };
        let path = entry.path.clone();
        let entry_label = plugin_label_from_entry(entry);
        let entry_id = entry.id.clone();
        let entry_format = entry.format;
        self.ensure_first_track();
        let track_idx = self.selected_track;
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
        self.send_plugin(MainToChild::SetSlotPlugin {
            track: track_idx,
            slot: dest_slot,
            format: entry_format,
            path,
            plugin_id: entry_id.clone(),
            initial_state: None,
        });
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

    fn refresh_inspector_chain(&mut self) {
        let mut out: Vec<ChainEntry> = Vec::new();
        let track_idx = self.selected_track as usize;
        let Some(track) = self.song.tracks.get(track_idx) else {
            self.inspector_chain = out;
            self.selected_track_label = format!("Track {}", self.selected_track + 1);
            self.instrument_label = "(no instrument)".into();
            return;
        };
        self.selected_track_label = if track.name.is_empty() {
            format!("Track {}", self.selected_track + 1)
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

    // -------- VOICEVOX -----------------------------------------------------

    fn begin_vocal_synth(&self, cx: &mut EventContext) {
        let song = self.song.clone();
        let slot = Arc::clone(&self.synth_result);
        cx.spawn(move |proxy| {
            let results = common::voicevox::synthesize_song(
                &song,
                common::voicevox::DEFAULT_SINGER_ID,
                common::voicevox::DEFAULT_SINGER_ID,
            );
            if let Ok(mut guard) = slot.lock() {
                *guard = results;
            }
            let _ = proxy.emit(AppEvent::VocalSynthCompleted);
        });
    }

    fn finish_vocal_synth(&mut self) {
        let results: Vec<common::voicevox::SynthResult> = self
            .synth_result
            .lock()
            .ok()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();

        if results.is_empty() {
            let errors: Vec<String> = self
                .synth_result
                .lock()
                .ok()
                .map(|g| g.iter().filter_map(|r| r.error.clone()).collect())
                .unwrap_or_default();
            self.status_message = if errors.is_empty() {
                "合成結果なし（Vocal トラックがないか VOICEVOX が応答しません）".to_string()
            } else {
                format!("合成エラー: {}", errors.join("; "))
            };
            return;
        }

        let ok_results: Vec<_> = results.iter().filter(|r| r.error.is_none()).collect();
        let err_count = results.len() - ok_results.len();
        if err_count > 0 {
            let first_err =
                results.iter().find_map(|r| r.error.as_deref()).unwrap_or("不明");
            self.status_message = format!(
                "合成: {} 成功, {} 失敗 ({})",
                ok_results.len(),
                err_count,
                first_err
            );
        } else {
            self.status_message =
                format!("合成完了 — {} クリップ。Play で再生", ok_results.len());
        }

        for r in &ok_results {
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

            self.send_plugin(MainToChild::SetVocalAudio {
                track: r.track,
                clip: r.clip,
                clip_start_samples,
                sample_rate: r.sample_rate,
                samples: r.samples.clone(),
            });
        }
    }

    // -------- Plugin DB rescan --------------------------------------------

    fn begin_rescan(&mut self, cx: &mut EventContext) {
        if self.is_rescanning {
            return;
        }
        self.is_rescanning = true;
        let slot = Arc::clone(&self.rescan_result);
        cx.spawn(move |proxy| match common::plugin_db::scan_system() {
            Ok(db) => {
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
                let _ = proxy.emit(AppEvent::PluginDbRescanCompleted);
            }
        });
    }

    fn finish_rescan(&mut self) {
        self.is_rescanning = false;
        let Some(new_db) = self.rescan_result.lock().ok().and_then(|mut g| g.take()) else {
            return;
        };
        let new_db = Arc::new(new_db);
        self.plugin_db = Some(new_db);
        self.rebuild_picker_entries();
        self.refresh_picker_visible();
    }

    // -------- Mixer --------------------------------------------------------

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
        let display = self.track_peak_display.as_slice();
        let updates: Vec<(usize, f32, f32)> = (0..self.track_mix.len())
            .map(|i| {
                let (l, r) = display.get(i).copied().unwrap_or((0.0, 0.0));
                (i, l, r)
            })
            .collect();
        for (i, l, r) in updates {
            if let Some(entry) = self.track_mix.get_mut(i) {
                entry.peak_l_norm =
                    common::meter::db_to_norm(common::meter::linear_to_db(l));
                entry.peak_r_norm =
                    common::meter::db_to_norm(common::meter::linear_to_db(r));
            }
        }
    }

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
            .map(|e| {
                if e.name.is_empty() {
                    plugin_id.to_string()
                } else {
                    e.name.clone()
                }
            })
            .unwrap_or_else(|| plugin_id.to_string())
    }

    fn action_export_wav(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV", &["wav"])
            .save_file()
        else {
            return;
        };
        self.status_message = "WAV 書き出し中...".to_string();
        self.send_plugin(MainToChild::LoadSong(self.song.clone()));
        self.send_plugin(MainToChild::ExportWav { path });
    }
}

// ---------------------------------------------------------------------------
// Free standing helpers
// ---------------------------------------------------------------------------

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

/// Demo clip preset used by `Add Vocal Track`. Five-note "こんにちは" line
/// at quarter-note spacing in the C major scale.
fn demo_clip() -> Clip {
    let lyrics = ["こ", "ん", "に", "ち", "わ"];
    let pitches = [60u8, 62, 64, 65, 67];
    let notes = (0..5)
        .map(|i| Note {
            start_beat: i as f64 * 0.5,
            duration_beats: 0.5,
            pitch: pitches[i],
            velocity: 100,
            lyric: Some(lyrics[i].into()),
        })
        .collect();
    Clip {
        name: "こんにちわ".into(),
        start_beat: 0.0,
        length_beats: 4.0,
        notes,
    }
}
