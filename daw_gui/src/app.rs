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

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Maximum number of undo snapshots kept in memory. Each snapshot is a
/// full `Song` clone, so the practical ceiling is bounded by song size
/// rather than this constant; 200 covers a long editing session.
const UNDO_LIMIT: usize = 200;

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
#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPickEntry {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub features: Vec<String>,
    pub format_label: String,
}

/// A single slot on the inspected track's chain.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ClipRef {
    pub track: u32,
    pub clip: u32,
}

/// Render-friendly snapshot of one clip on the timeline. Rebuilt whenever
/// the song changes; the arrangement view binds to the `Vec<ClipBox>` lens
/// and renders each entry as a coloured rectangle.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct NoteBox {
    pub note: u32,
    pub start_beat: f32,
    pub duration_beats: f32,
    pub pitch: u8,
    pub velocity: u8,
    pub lyric: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq)]
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

pub struct AppData {
    pub song: Signal<Song>,
    pub file_path: Signal<Option<PathBuf>>,

    // -------- Selection -----------------------------------------------------
    /// Track that the inspector + plugin picker target. Always valid against
    /// `song.tracks` (clamped on track removal).
    pub selected_track: Signal<u32>,
    /// Most recently clicked clip — drives the piano roll's contents and
    /// any "primary" indicator. `None` means no clip is selected.
    pub selected_clip: Signal<Option<ClipRef>>,
    /// Full selection set. Always contains `selected_clip` as its last
    /// entry when non-empty. Bulk operations (delete / drag-move) iterate
    /// this instead of the primary alone.
    pub selected_clips: Signal<Vec<ClipRef>>,
    /// Notes selected within `selected_clip`. Indices into the clip's
    /// `notes` vector.
    pub selected_notes: Signal<Vec<u32>>,

    // -------- View state ----------------------------------------------------
    /// Bottom panel selector: `0 = Mixer`, `1 = Piano Roll`.
    pub bottom_panel: Signal<u8>,
    /// Horizontal zoom on the arrangement view (px / beat).
    pub arrange_zoom_x: Signal<f32>,
    /// Beat at the left edge of the arrangement view.
    pub arrange_scroll_beat: Signal<f32>,
    /// Horizontal zoom on the piano roll (px / beat).
    pub pianoroll_zoom_x: Signal<f32>,
    /// Vertical zoom on the piano roll (px / semitone).
    pub pianoroll_zoom_y: Signal<f32>,
    /// MIDI pitch shown at the top edge of the piano roll.
    pub pianoroll_top_pitch: Signal<u8>,
    /// Beat at the left edge of the piano roll.
    pub pianoroll_scroll_beat: Signal<f32>,
    /// Loop band start/end as `f32`, derived from `song.loop_*_beat` so the
    /// arrangement view can render without binding to `Song` directly.
    pub loop_start_beat: Memo<f32>,
    pub loop_end_beat: Memo<f32>,

    /// `song.bpm` projected as a Memo so the transport view can render
    /// without binding to the entire `Song`.
    pub bpm: Memo<f32>,

    // -------- Reactive snapshots derived from `song` + selection ----------
    /// Per-track header strip on the left of the arrangement view.
    pub track_headers: Memo<Vec<TrackHeader>>,
    /// Mirror of `song.tracks.len()` for slot-visibility derivations.
    pub track_count: Memo<u32>,
    /// All clips in the song, flattened for the arrangement view.
    pub clip_boxes: Memo<Vec<ClipBox>>,
    /// Notes in `selected_clip` (empty if no clip selected).
    pub note_boxes: Memo<Vec<NoteBox>>,
    /// Lyric of the first selected note, or empty when nothing is selected.
    /// The lyric panel binds an editable Textbox to this.
    pub selected_lyric: Memo<String>,

    // -------- Playback / metering ------------------------------------------
    pub is_playing: Signal<bool>,
    pub is_looping: Signal<bool>,
    /// Current playhead in beats (relative to song origin). `None` when the
    /// audio thread published the "not playing" sentinel.
    pub playhead_beat: Signal<Option<f32>>,
    pub master_gain: Signal<f32>,
    pub peak_l_display: Signal<f32>,
    pub peak_r_display: Signal<f32>,
    pub peak_l_norm: Signal<f32>,
    pub peak_r_norm: Signal<f32>,

    // -------- Plugin database / picker -------------------------------------
    pub plugin_db: Option<Arc<PluginDatabase>>,
    pub plugin_picker_entries: Signal<Vec<PluginPickEntry>>,
    pub plugin_picker_visible: Signal<Vec<PluginPickEntry>>,
    pub is_plugin_picker_open: Signal<bool>,
    pub plugin_picker_target: Signal<PickerTarget>,
    pub inspector_chain: Memo<Vec<ChainEntry>>,
    pub selected_track_label: Memo<String>,

    // -------- Save flow / IPC ----------------------------------------------
    pub pending_save_path: Option<PathBuf>,
    pub audio_tx: Option<UnboundedSender<MainToChild>>,
    pub plugin_tx: Option<UnboundedSender<MainToChild>>,
    #[cfg(windows)]
    pub plugin_host_windows:
        HashMap<(u32, PluginSlot), crate::view::plugin_embed::PluginHostWindow>,

    // -------- Mixer ---------------------------------------------------------
    pub track_mix: Memo<Vec<TrackMixEntry>>,
    pub track_peak_display: Signal<Vec<(f32, f32)>>,

    // -------- Background workers -------------------------------------------
    pub synth_result: Arc<Mutex<Vec<common::voicevox::SynthResult>>>,
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    pub is_rescanning: Signal<bool>,
    pub status_message: Signal<String>,

    /// Inline rename state. `Some(track_idx)` means the track header for
    /// that track is currently in edit mode.
    pub track_rename_idx: Signal<Option<u32>>,
    pub track_rename_text: Signal<String>,

    /// Undo / redo history. Each entry is a full `Song` snapshot.
    pub undo_stack: VecDeque<Song>,
    pub redo_stack: VecDeque<Song>,

    /// Note clipboard. Notes are stored with `start_beat` already normalised
    /// so the earliest note is at 0 — paste then offsets every entry by the
    /// target position.
    pub note_clipboard: Vec<Note>,

    /// True while the keybindings cheat-sheet overlay is visible.
    pub is_help_open: Signal<bool>,

    /// "Open Recent" entries persisted to LocalAppData. The signal
    /// `recent_paths_display` mirrors `recent_files.paths` as plain strings.
    pub recent_files: common::recent::RecentFiles,
    pub recent_paths_display: Signal<Vec<String>>,

    /// True after any song-mutating edit, false right after the song is
    /// successfully saved / loaded / replaced.
    pub is_dirty: Signal<bool>,
    /// Last instant we wrote `<file_path>.autosave.daw`.
    pub last_autosave: std::time::Instant,

    /// `true` while a clip or note is being dragged.
    pub is_dragging: Signal<bool>,

    /// Display label for the active MIDI input device — empty when no
    /// device is connected.
    pub midi_input_label: Signal<String>,

    /// Step-input cursor: the next beat where an incoming MIDI NoteOn drops
    /// a note.
    pub step_cursor_beat: f64,
    /// How far the step cursor advances per dropped note.
    pub step_size_beats: f64,
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
        let song_data = Song {
            tracks: vec![Track {
                name: "Track 1".into(),
                ..Track::default()
            }],
            ..Song::default()
        };
        let initial_peak_display = vec![(0.0, 0.0); song_data.tracks.len()];
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

        // Reactive state — create the upstream Signals first so the
        // derived Memos can capture them. `Signal<T>: Copy`, so the moves
        // into Memo closures don't prevent us from also placing the
        // Signals in the returned `Self`.
        let song = Signal::new(song_data);
        let selected_track: Signal<u32> = Signal::new(0);
        let selected_clip: Signal<Option<ClipRef>> = Signal::new(None);
        let selected_clips: Signal<Vec<ClipRef>> = Signal::new(Vec::new());
        let selected_notes: Signal<Vec<u32>> = Signal::new(Vec::new());
        let track_peak_display: Signal<Vec<(f32, f32)>> = Signal::new(initial_peak_display);

        // Scalar projections.
        let bpm = song.map(|s: &Song| s.bpm);
        let track_count = song.map(|s: &Song| s.tracks.len() as u32);
        let loop_start_beat = song.map(|s: &Song| s.loop_start_beat as f32);
        let loop_end_beat = song.map(|s: &Song| s.loop_end_beat as f32);

        // Vec snapshots for Lists / Canvas binds.
        let track_headers = Memo::new(move |_| {
            let s = song.get();
            let sel = selected_track.get();
            s.tracks
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
                    selected: i as u32 == sel,
                })
                .collect()
        });

        let clip_boxes = Memo::new(move |_| {
            let s = song.get();
            let selected = selected_clips.get();
            s.tracks
                .iter()
                .enumerate()
                .flat_map(|(t_idx, t)| {
                    let selected = selected.clone();
                    t.clips.iter().enumerate().map(move |(c_idx, c)| {
                        let r = ClipRef {
                            track: t_idx as u32,
                            clip: c_idx as u32,
                        };
                        ClipBox {
                            track: t_idx as u32,
                            clip: c_idx as u32,
                            name: c.name.clone(),
                            start_beat: c.start_beat as f32,
                            length_beats: c.length_beats as f32,
                            selected: selected.contains(&r),
                        }
                    })
                })
                .collect()
        });

        let note_boxes = Memo::new(move |_| {
            let s = song.get();
            let clip_ref = selected_clip.get();
            let selected = selected_notes.get();
            match clip_ref {
                Some(ClipRef { track, clip }) => s
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
                                velocity: n.velocity,
                                lyric: n.lyric.clone().unwrap_or_default(),
                                selected: selected.contains(&(i as u32)),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            }
        });

        let track_mix = Memo::new(move |_| {
            let s = song.get();
            let peaks = track_peak_display.get();
            s.tracks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let (l, r) = peaks.get(i).copied().unwrap_or((0.0, 0.0));
                    TrackMixEntry {
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
                        peak_l_norm: common::meter::db_to_norm(common::meter::linear_to_db(l)),
                        peak_r_norm: common::meter::db_to_norm(common::meter::linear_to_db(r)),
                    }
                })
                .collect()
        });

        let selected_lyric = Memo::new(move |_| {
            let song = song.get();
            let clip = selected_clip.get();
            let notes = selected_notes.get();
            notes
                .first()
                .copied()
                .and_then(|n_idx| {
                    let r = clip?;
                    let track = song.tracks.get(r.track as usize)?;
                    let clip = track.clips.get(r.clip as usize)?;
                    let note = clip.notes.get(n_idx as usize)?;
                    Some(note.lyric.clone().unwrap_or_default())
                })
                .unwrap_or_default()
        });

        let selected_track_label = Memo::new(move |_| {
            let s = song.get();
            let sel = selected_track.get();
            s.tracks
                .get(sel as usize)
                .map(|t| {
                    if t.name.is_empty() {
                        format!("Track {}", sel + 1)
                    } else {
                        t.name.clone()
                    }
                })
                .unwrap_or_else(|| format!("Track {}", sel + 1))
        });

        let plugin_db_for_chain = plugin_db.clone();
        let inspector_chain = Memo::new(move |_| {
            let s = song.get();
            let sel = selected_track.get();
            let Some(track) = s.tracks.get(sel as usize) else {
                return Vec::new();
            };
            let mut chain: Vec<ChainEntry> = Vec::new();
            for (i, p) in track.midi_fx_chain.iter().enumerate() {
                chain.push(ChainEntry {
                    slot_kind: 0,
                    slot_index: i as u32,
                    section_label: "MIDI FX".into(),
                    plugin_name: resolve_plugin_name(&plugin_db_for_chain, &p.plugin_id),
                });
            }
            if let Some(inst) = track.instrument.as_ref() {
                chain.push(ChainEntry {
                    slot_kind: 1,
                    slot_index: 0,
                    section_label: "Instrument".into(),
                    plugin_name: resolve_plugin_name(&plugin_db_for_chain, &inst.plugin_id),
                });
            }
            for (i, p) in track.fx_chain.iter().enumerate() {
                chain.push(ChainEntry {
                    slot_kind: 2,
                    slot_index: i as u32,
                    section_label: "FX".into(),
                    plugin_name: resolve_plugin_name(&plugin_db_for_chain, &p.plugin_id),
                });
            }
            chain
        });

        Self {
            song,
            file_path: Signal::new(None),
            selected_track,
            selected_clip,
            selected_clips,
            selected_notes,
            bottom_panel: Signal::new(0),
            arrange_zoom_x: Signal::new(ARRANGE_PX_PER_BEAT),
            arrange_scroll_beat: Signal::new(0.0),
            pianoroll_zoom_x: Signal::new(64.0),
            pianoroll_zoom_y: Signal::new(14.0),
            pianoroll_top_pitch: Signal::new(84), // C6
            pianoroll_scroll_beat: Signal::new(0.0),
            loop_start_beat,
            loop_end_beat,
            bpm,
            track_headers,
            track_count,
            clip_boxes,
            note_boxes,
            selected_lyric,
            is_playing: Signal::new(false),
            is_looping: Signal::new(false),
            playhead_beat: Signal::new(None),
            master_gain: Signal::new(1.0),
            peak_l_display: Signal::new(0.0),
            peak_r_display: Signal::new(0.0),
            peak_l_norm: Signal::new(0.0),
            peak_r_norm: Signal::new(0.0),
            plugin_db,
            plugin_picker_entries: Signal::new(plugin_picker_entries),
            plugin_picker_visible: Signal::new(Vec::new()),
            is_plugin_picker_open: Signal::new(false),
            plugin_picker_target: Signal::new(PickerTarget::Instrument),
            inspector_chain,
            selected_track_label,
            pending_save_path: None,
            audio_tx: Some(audio_tx),
            plugin_tx: Some(plugin_tx),
            #[cfg(windows)]
            plugin_host_windows: HashMap::new(),
            track_mix,
            track_peak_display,
            synth_result: Arc::new(Mutex::new(Vec::new())),
            rescan_result: Arc::new(Mutex::new(None)),
            is_rescanning: Signal::new(false),
            status_message: Signal::new(String::new()),
            track_rename_idx: Signal::new(None),
            track_rename_text: Signal::new(String::new()),
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            note_clipboard: Vec::new(),
            is_help_open: Signal::new(false),
            recent_files: load_recent_files(),
            recent_paths_display: Signal::new(load_recent_files_display()),
            is_dirty: Signal::new(false),
            last_autosave: std::time::Instant::now(),
            is_dragging: Signal::new(false),
            midi_input_label: Signal::new(String::new()),
            step_cursor_beat: 0.0,
            step_size_beats: DEFAULT_NOTE_DURATION,
        }
    }

    /// Record the current `song` onto the undo stack. Drops the redo
    /// stack — Bitwig's behaviour: any new edit invalidates the redo
    /// branch the user could have replayed otherwise.
    fn push_undo_snapshot(&mut self) {
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(self.song.get_untracked());
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        let Some(prev) = self.undo_stack.pop_back() else {
            return;
        };
        let current = self.song.get_untracked();
        self.song.set(prev);
        self.redo_stack.push_back(current);
        self.after_undo_redo();
    }

    fn redo(&mut self) {
        let Some(next) = self.redo_stack.pop_back() else {
            return;
        };
        let current = self.song.get_untracked();
        self.song.set(next);
        self.undo_stack.push_back(current);
        self.after_undo_redo();
    }

    fn after_undo_redo(&mut self) {
        // Selection might point at a clip / note that no longer exists;
        // clamp to safe defaults rather than try to migrate.
        self.selected_clip.set(None);
        self.selected_clips.update(|v| v.clear());
        self.selected_notes.update(|v| v.clear());
        self.track_rename_idx.set(None);
        self.track_rename_text.set(String::new());
        let (track_max, is_empty) = self
            .song
            .with_untracked(|s| (s.tracks.len().saturating_sub(1) as u32, s.tracks.is_empty()));
        if is_empty {
            self.selected_track.set(0);
        } else if self.selected_track.get_untracked() > track_max {
            self.selected_track.set(track_max);
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    /// Returns true when the given event is a discrete song-mutation that
    /// should record a single undo snapshot. Drag operations (which fire
    /// an event per mouse-move) are excluded — the view emits a separate
    /// `PushUndoSnapshot` once at MouseDown to capture pre-drag state.
    fn is_undoable(event: &AppEvent) -> bool {
        matches!(
            event,
            AppEvent::New
                | AppEvent::AddVocalTrack
                | AppEvent::AddInstrumentTrack
                | AppEvent::RemoveLastTrack
                | AppEvent::CommitRenameTrack
                | AppEvent::CreateClip { .. }
                | AppEvent::ResizeClip { .. }
                | AppEvent::DeleteSelectedClip
                | AppEvent::AddNote { .. }
                | AppEvent::ResizeNote { .. }
                | AppEvent::DeleteSelectedNotes
                | AppEvent::SetSelectedNoteLyric(_)
                | AppEvent::PasteNotes
                | AppEvent::QuantizeSelectedNotes(_)
                | AppEvent::SelectPluginFromDb(_)
                | AppEvent::RemoveSlot { .. }
        )
    }

    fn copy_selected_notes(&mut self) {
        let Some(r) = self.selected_clip.get_untracked() else {
            return;
        };
        let selected = self.selected_notes.get_untracked();
        if selected.is_empty() {
            return;
        }
        let mut copied: Vec<Note> = self.song.with_untracked(|s| {
            let Some(track) = s.tracks.get(r.track as usize) else {
                return Vec::new();
            };
            let Some(clip) = track.clips.get(r.clip as usize) else {
                return Vec::new();
            };
            selected
                .iter()
                .filter_map(|i| clip.notes.get(*i as usize).cloned())
                .collect()
        });
        if copied.is_empty() {
            return;
        }
        // Normalise so the earliest selected note sits at beat 0; paste
        // then re-offsets every entry by the destination position.
        let earliest = copied
            .iter()
            .map(|n| n.start_beat)
            .fold(f64::INFINITY, f64::min);
        if earliest.is_finite() {
            for n in &mut copied {
                n.start_beat -= earliest;
            }
        }
        let count = copied.len();
        self.note_clipboard = copied;
        self.status_message.set(format!("コピー: {count} ノート"));
    }

    fn paste_notes(&mut self) {
        if self.note_clipboard.is_empty() {
            return;
        }
        let Some(r) = self.selected_clip.get_untracked() else {
            self.status_message
                .set("貼り付け先のクリップが選択されていません".to_string());
            return;
        };
        // Anchor at the playhead beat (relative to clip), or beat 0 when
        // not playing.
        let playhead = self.playhead_beat.get_untracked();
        let clipboard = self.note_clipboard.clone();
        let new_indices = self.song.try_update(|s| {
            let anchor = if let Some(playhead) = playhead {
                let clip_start = s
                    .tracks
                    .get(r.track as usize)
                    .and_then(|t| t.clips.get(r.clip as usize))
                    .map(|c| c.start_beat)
                    .unwrap_or(0.0);
                (playhead as f64 - clip_start).max(0.0)
            } else {
                0.0
            };
            let Some(track) = s.tracks.get_mut(r.track as usize) else {
                return Vec::new();
            };
            let Some(clip) = track.clips.get_mut(r.clip as usize) else {
                return Vec::new();
            };
            let mut new_indices = Vec::with_capacity(clipboard.len());
            for src in &clipboard {
                let mut n = src.clone();
                n.start_beat += anchor;
                new_indices.push(clip.notes.len() as u32);
                clip.notes.push(n);
            }
            new_indices
        });
        if let Some(new_indices) = new_indices {
            self.selected_notes.set(new_indices);
            self.sync_song_to_plugin_host();
            self.status_message
                .set(format!("貼り付け: {} ノート", clipboard.len()));
        }
    }

    fn set_note_velocity(&mut self, note_idx: u32, velocity: u8) {
        let Some(r) = self.selected_clip.get_untracked() else {
            return;
        };
        self.song.update(|s| {
            let Some(track) = s.tracks.get_mut(r.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get_mut(r.clip as usize) else {
                return;
            };
            let Some(note) = clip.notes.get_mut(note_idx as usize) else {
                return;
            };
            note.velocity = velocity;
        });
        self.sync_song_to_plugin_host();
    }

    fn quantize_selected_notes(&mut self, div: u8) {
        let Some(r) = self.selected_clip.get_untracked() else {
            return;
        };
        let selected = self.selected_notes.get_untracked();
        let div = div.max(1) as f64;
        let snap = |b: f64| (b * div).round() / div;
        self.song.update(|s| {
            let Some(track) = s.tracks.get_mut(r.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get_mut(r.clip as usize) else {
                return;
            };
            for &i in &selected {
                if let Some(n) = clip.notes.get_mut(i as usize) {
                    n.start_beat = snap(n.start_beat).max(0.0);
                }
            }
        });
        self.sync_song_to_plugin_host();
    }

    /// Resize `track_peak_display` so its length matches `song.tracks`.
    /// Called whenever a track is added/removed; `track_mix` Memo will
    /// pick up the new length via its dependency on `track_peak_display`
    /// (and on `song`).
    fn resize_track_peak_display(&mut self) {
        let n = self.song.with_untracked(|s| s.tracks.len());
        self.track_peak_display
            .update(|disp| disp.resize(n, (0.0, 0.0)));
    }
}

#[derive(Debug, Clone, PartialEq)]
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
    Undo,
    Redo,
    /// Take an undo snapshot of the current song. Used by views before
    /// starting a multi-frame drag (clip / note move) so the whole drag
    /// collapses into a single undo step.
    PushUndoSnapshot,
    /// Copy currently selected notes onto the clipboard (positions
    /// normalised so the earliest note sits at beat 0).
    CopySelectedNotes,
    /// Paste clipboard notes into the selected clip, offset by either
    /// the playhead position or the start of the clip if no playhead.
    PasteNotes,
    /// Quantize selected notes' `start_beat` to the nearest `1/div`-beat
    /// grid. `div=4` means 1/4 beat = 16th-note grid.
    QuantizeSelectedNotes(u8),
    /// Set the velocity of one note (0..=127). Emitted by the velocity
    /// lane while the user drags a bar.
    SetNoteVelocity {
        note: u32,
        velocity: u8,
    },
    AddVocalTrack,
    AddInstrumentTrack,
    RemoveLastTrack,
    /// Remove an arbitrary track by index. Existing plugin chains on
    /// later tracks shift down by one to keep the index space contiguous.
    DeleteTrack(u32),
    /// Swap a track with its neighbour above (no-op when already at top).
    MoveTrackUp(u32),
    /// Swap a track with its neighbour below (no-op at bottom).
    MoveTrackDown(u32),
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
    /// Toggle the keybindings cheat-sheet overlay.
    ToggleHelp,
    /// Force-close the help overlay (Esc).
    CloseHelp,
    /// Open a project from the recent-files list. The path is the
    /// absolute disk path stored when the user last loaded / saved it.
    OpenRecent(PathBuf),
    /// Periodic autosave tick fired by the GUI's timer thread. Triggers
    /// `<file_path>.autosave.daw` when the song is dirty and a path is
    /// known; otherwise a no-op.
    AutosaveTick,
    /// Mouse drag started in arrangement / piano roll. Sets
    /// `is_dragging` so the LoadSong IPC is deferred to the matching
    /// `EndDrag` rather than fired per mouse-move frame.
    BeginDrag,
    /// Mouse drag finished. Flushes one final LoadSong with the
    /// committed state.
    EndDrag,
    /// MIDI NoteOn observed on the active input port. Treated as
    /// step-input: drops a new note in the selected clip at
    /// `step_cursor_beat` and advances the cursor by `step_size_beats`.
    MidiNoteOn { pitch: u8, velocity: u8 },
    /// MIDI NoteOff — currently a no-op in step-input mode (note duration
    /// is fixed by the step grid). Reserved for future record-input
    /// modes that capture true on/off times.
    MidiNoteOff { pitch: u8 },
    /// Background thread reports the MIDI input that was opened on
    /// startup (or when the user re-scans devices). `None` clears the
    /// connection; `Some(name)` populates the device label.
    MidiInputOpened(Option<String>),

    // -------- Bottom panel -------------------------------------------------
    SelectBottomPanel(u8),

    // -------- Arrangement / clip operations -------------------------------
    /// Select a clip. `additive` extends the existing selection (Shift+click);
    /// otherwise the prior selection is replaced.
    SelectClip {
        target: ClipRef,
        additive: bool,
    },
    /// Replace the current clip selection with the given set. Used by
    /// marquee box-select on the arrangement.
    SetClipSelection(Vec<ClipRef>),
    ClearSelection,
    /// Resize the clip to `length` beats.
    ResizeClip {
        target: ClipRef,
        length: f64,
    },
    /// Bulk move of every entry: `(ClipRef, new_start_beat)`. Used
    /// when the user drags one clip with several selected — every clip in
    /// the selection slides by the same delta.
    SetClipPositions(Vec<(ClipRef, f64)>),
    /// Create a new empty clip on `track` starting at `start_beat`. Length
    /// defaults to `DEFAULT_CLIP_LENGTH`.
    CreateClip {
        track: u32,
        start_beat: f64,
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
        start_beat: f64,
        duration: f64,
        pitch: u8,
    },
    /// Bulk move every entry: `(note_idx, new_start_beat, new_pitch)`.
    /// Used when the user drags one note with several selected.
    SetNotePositions(Vec<(u32, f64, u8)>),
    /// Replace the note selection set. Used by piano roll marquee.
    SetNoteSelection(Vec<u32>),
    ResizeNote {
        track: u32,
        clip: u32,
        note: u32,
        duration: f64,
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
    SetMasterGain(f32),

    // -------- IPC events from plugin_host ---------------------------------
    Tick { samples: u64, peak_l: f32, peak_r: f32 },
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
    /// Update arrangement scroll (beat at left edge).
    SetArrangeScroll(f32),
    /// Update arrangement horizontal zoom (px/beat).
    SetArrangeZoom(f32),
    /// Update piano roll horizontal scroll (beat at left edge).
    SetPianoRollScrollX(f32),
    /// Update piano roll top pitch (highest pitch shown at top).
    SetPianoRollTopPitch(u8),
    /// Update piano roll horizontal zoom (px/beat).
    SetPianoRollZoomX(f32),
    /// Update piano roll vertical zoom (px/semitone).
    SetPianoRollZoomY(f32),
    /// Set the user-defined playback loop region. Pass `start == end` (e.g.
    /// both zero) to clear the user range and fall back to the song
    /// content envelope.
    SetLoopRange { start: f64, end: f64 },

    // -------- Mixer -------------------------------------------------------
    SetTrackVolume {
        track: u32,
        amp: f32,
    },
    SetTrackPan {
        track: u32,
        pan: f32,
    },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    TrackPeaksTick(Vec<(f32, f32)>),

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
            // One snapshot per discrete edit. Drag-driven events skip
            // this and rely on the view's MouseDown-time
            // `PushUndoSnapshot` so the full drag is a single undo step.
            if Self::is_undoable(app_event) {
                self.push_undo_snapshot();
            }

            match app_event {
                AppEvent::New => self.action_new(),
                AppEvent::Open => self.action_open(),
                AppEvent::Save => {
                    self.action_save();
                }
                AppEvent::SaveAs => {
                    self.action_save_as();
                }
                AppEvent::Play => {
                    self.play();
                }
                AppEvent::Stop => {
                    self.stop();
                }
                AppEvent::PlayToggle => {
                    if self.is_playing.get_untracked() {
                        self.stop();
                    } else {
                        self.play();
                    }
                }
                AppEvent::ToggleLoop => {
                    self.toggle_loop();
                }
                AppEvent::Undo => self.undo(),
                AppEvent::Redo => self.redo(),
                AppEvent::PushUndoSnapshot => {
                    self.push_undo_snapshot();
                }
                AppEvent::CopySelectedNotes => {
                    self.copy_selected_notes();
                }
                AppEvent::PasteNotes => self.paste_notes(),
                AppEvent::QuantizeSelectedNotes(div) => {
                    self.quantize_selected_notes(*div);
                }
                AppEvent::SetNoteVelocity { note, velocity } => {
                    self.set_note_velocity(*note, *velocity);
                }
                AppEvent::AddVocalTrack => self.action_add_vocal_track(),
                AppEvent::AddInstrumentTrack => self.action_add_instrument_track(),
                AppEvent::RemoveLastTrack => self.action_remove_last_track(),
                AppEvent::DeleteTrack(idx) => self.delete_track(*idx),
                AppEvent::MoveTrackUp(idx) => self.swap_tracks(*idx, idx.saturating_sub(1)),
                AppEvent::MoveTrackDown(idx) => self.swap_tracks(*idx, *idx + 1),
                AppEvent::SelectTrack(idx) => self.select_track(*idx),
                AppEvent::BeginRenameTrack(idx) => {
                    self.begin_rename_track(*idx);
                }
                AppEvent::RenameTrackChanged(text) => {
                    self.track_rename_text.set(text.clone());
                }
                AppEvent::CommitRenameTrack => self.commit_rename_track(),
                AppEvent::CancelRenameTrack => {
                    self.track_rename_idx.set(None);
                    self.track_rename_text.set(String::new());
                }
                AppEvent::ToggleHelp => {
                    self.is_help_open.update(|b| *b = !*b);
                }
                AppEvent::CloseHelp => {
                    self.is_help_open.set(false);
                }
                AppEvent::OpenRecent(path) => {
                    self.action_open_path(path.clone());
                }
                AppEvent::AutosaveTick => {
                    self.maybe_autosave();
                }
                AppEvent::BeginDrag => {
                    self.is_dragging.set(true);
                }
                AppEvent::EndDrag => {
                    self.is_dragging.set(false);
                    let song = self.song.get_untracked();
                    self.send_plugin(MainToChild::LoadSong(song));
                }
                AppEvent::MidiNoteOn { pitch, velocity } => {
                    self.handle_midi_note_on(*pitch, *velocity);
                }
                AppEvent::MidiNoteOff { pitch: _ } => {
                    // Step-input doesn't track note ends — durations are
                    // fixed by `step_size_beats`.
                }
                AppEvent::MidiInputOpened(name) => {
                    let label = name.clone().unwrap_or_default();
                    self.midi_input_label.set(label.clone());
                    if name.is_some() {
                        self.status_message.set(format!("MIDI 入力: {label}"));
                    }
                }
                AppEvent::SelectBottomPanel(p) => {
                    self.bottom_panel.set(*p);
                }
                AppEvent::SelectClip { target, additive } => {
                    self.select_clip(*target, *additive);
                }
                AppEvent::SetClipSelection(targets) => {
                    self.set_clip_selection(targets.clone());
                }
                AppEvent::ClearSelection => {
                    self.selected_clip.set(None);
                    self.selected_clips.update(|v| v.clear());
                    self.selected_notes.update(|v| v.clear());
                }
                AppEvent::ResizeClip { target, length } => {
                    self.resize_clip(*target, *length);
                }
                AppEvent::SetClipPositions(entries) => {
                    self.set_clip_positions(entries);
                }
                AppEvent::CreateClip { track, start_beat } => {
                    self.create_clip(*track, *start_beat);
                }
                AppEvent::DeleteSelectedClip => self.delete_selected_clip(),
                AppEvent::SelectNote { note, additive } => {
                    self.select_note(*note, *additive);
                }
                AppEvent::ClearNoteSelection => self.selected_notes.update(|v| v.clear()),
                AppEvent::AddNote {
                    track,
                    clip,
                    start_beat,
                    duration,
                    pitch,
                } => {
                    self.add_note(*track, *clip, *start_beat, *duration, *pitch);
                }
                AppEvent::ResizeNote { track, clip, note, duration } => {
                    self.resize_note(*track, *clip, *note, *duration);
                }
                AppEvent::SetNotePositions(entries) => {
                    self.set_note_positions(entries);
                }
                AppEvent::SetNoteSelection(targets) => {
                    self.selected_notes.set(targets.clone());
                }
                AppEvent::DeleteSelectedNotes => self.delete_selected_notes(),
                AppEvent::SetSelectedNoteLyric(text) => {
                    self.set_selected_note_lyric(text.clone());
                }
                AppEvent::OpenPluginPickerFor(target) => {
                    self.plugin_picker_target.set(*target);
                    self.refresh_picker_visible();
                    self.is_plugin_picker_open.set(true);
                }
                AppEvent::ClosePluginPicker => {
                    self.is_plugin_picker_open.set(false);
                }
                AppEvent::RescanPluginDb => {
                    self.begin_rescan(cx);
                }
                AppEvent::PluginDbRescanCompleted => {
                    self.finish_rescan();
                }
                AppEvent::SetArrangeScroll(scroll) => {
                    self.arrange_scroll_beat.set(scroll.max(0.0));
                }
                AppEvent::SetArrangeZoom(zoom) => {
                    self.arrange_zoom_x.set(zoom.clamp(2.0, 400.0));
                }
                AppEvent::SetPianoRollScrollX(scroll) => {
                    self.pianoroll_scroll_beat.set(scroll.max(0.0));
                }
                AppEvent::SetPianoRollTopPitch(p) => {
                    self.pianoroll_top_pitch.set((*p).clamp(11, 127));
                }
                AppEvent::SetPianoRollZoomX(zoom) => {
                    self.pianoroll_zoom_x.set(zoom.clamp(8.0, 400.0));
                }
                AppEvent::SetPianoRollZoomY(zoom) => {
                    self.pianoroll_zoom_y.set(zoom.clamp(6.0, 40.0));
                }
                AppEvent::SetLoopRange { start, end } => {
                    self.set_loop_range(*start, *end);
                }
                AppEvent::SelectPluginFromDb(id) => {
                    self.select_plugin_from_db(id.clone());
                }
                AppEvent::ToggleSlotGui {
                    slot_kind,
                    slot_index,
                } => {
                    self.toggle_slot_gui(*slot_kind, *slot_index);
                }
                AppEvent::RemoveSlot {
                    slot_kind,
                    slot_index,
                } => {
                    self.remove_slot(*slot_kind, *slot_index);
                }
                AppEvent::SetMasterGain(amp) => {
                    self.set_master_gain(*amp);
                }
                AppEvent::Tick { samples, peak_l, peak_r } => {
                    self.on_tick(*samples, *peak_l, *peak_r);
                }
                AppEvent::GuiOpenedFromChild {
                    track,
                    slot,
                    width,
                    height,
                } => {
                    self.on_gui_opened(*track, *slot, *width, *height);
                }
                AppEvent::GuiRequestResizeFromChild {
                    track,
                    slot,
                    width,
                    height,
                } => {
                    self.on_gui_request_resize(*track, *slot, *width, *height);
                }
                AppEvent::GuiClosedFromChild { track, slot } => {
                    self.on_gui_closed(*track, *slot);
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
                }
                AppEvent::AllStatesReceived(entries) => {
                    self.on_all_states_from_child(entries.clone());
                }
                AppEvent::SetTrackVolume { track, amp } => {
                    self.set_track_volume(*track, *amp);
                }
                AppEvent::SetTrackPan { track, pan } => {
                    self.set_track_pan(*track, *pan);
                }
                AppEvent::ToggleTrackMute(track) => {
                    self.toggle_track_mute(*track);
                }
                AppEvent::ToggleTrackSolo(track) => {
                    self.toggle_track_solo(*track);
                }
                AppEvent::TrackPeaksTick(peaks) => {
                    self.on_track_peaks_tick(peaks);
                }
                AppEvent::ExportWav => {
                    self.action_export_wav();
                }
                AppEvent::ExportWavComplete { error } => {
                    if let Some(e) = error {
                        self.status_message.set(format!("WAV 書き出し失敗: {e}"));
                    } else {
                        self.status_message.set("WAV 書き出し完了".to_string());
                    }
                }
                AppEvent::SynthesizeVocal => {
                    self.status_message.set("VOICEVOX 合成中...".to_string());
                    self.begin_vocal_synth(cx);
                }
                AppEvent::VocalSynthCompleted => {
                    self.finish_vocal_synth();
                }
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
    /// move / clip drag etc.) are heard immediately during playback,
    /// and mark the project as dirty so the autosave timer knows to fire.
    ///
    /// During a drag the LoadSong IPC is suppressed — sending a full
    /// song snapshot 60× per second is wasted bandwidth, and `EndDrag`
    /// flushes the final committed state.
    fn sync_song_to_plugin_host(&mut self) {
        self.is_dirty.set(true);
        if self.is_dragging.get_untracked() {
            return;
        }
        let song = self.song.get_untracked();
        self.send_plugin(MainToChild::LoadSong(song));
    }

    // -------- File ----------------------------------------------------------

    fn action_new(&mut self) {
        self.song.set(Song::default());
        self.file_path.set(None);
        self.selected_track.set(0);
        self.selected_clip.set(None);
        self.selected_notes.update(|v| v.clear());
        self.resize_track_peak_display();
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
        self.action_open_path(path);
    }

    /// Shared implementation for the File→Open dialog and the recent-
    /// files menu. Loads the project, primes plugin chains, and bumps
    /// `path` to the front of the recent list.
    fn action_open_path(&mut self, path: PathBuf) {
        match common::project::load(&path) {
            Ok(song) => {
                tracing::info!(path = %path.display(), "loaded project");
                self.restore_plugin_from_song(&song);
                self.song.set(song);
                self.file_path.set(Some(path.clone()));
                self.selected_track.set(0);
                self.selected_clip.set(None);
                self.selected_notes.update(|v| v.clear());
                self.resize_track_peak_display();
                self.sync_song_to_plugin_host();
                self.is_dirty.set(false);
                self.push_recent(path);
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project");
                self.status_message.set(format!("Open 失敗: {e:#}"));
            }
        }
    }

    fn push_recent(&mut self, path: PathBuf) {
        self.recent_files.push(path);
        let display: Vec<String> = self
            .recent_files
            .paths
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        self.recent_paths_display.set(display);
        if let Some(disk) = common::recent::default_path()
            && let Err(e) = common::recent::save(&disk, &self.recent_files)
        {
            tracing::warn!(
                error = ?e,
                path = %disk.display(),
                "failed to persist recent files"
            );
        }
    }

    /// Periodic autosave hook. Writes `<file_path>.autosave.daw` when the
    /// song is dirty and we have a known on-disk location; rate-limited
    /// to once every 60 seconds so big drag operations don't hammer the
    /// disk.
    fn maybe_autosave(&mut self) {
        if !self.is_dirty.get_untracked() {
            return;
        }
        let Some(orig) = self.file_path.get_untracked() else {
            return;
        };
        if self.last_autosave.elapsed() < std::time::Duration::from_secs(60) {
            return;
        }
        let mut autosave_path = orig.clone();
        let mut name = autosave_path
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        name.push(".autosave.daw");
        autosave_path.set_file_name(name);
        let result = self
            .song
            .with_untracked(|s| common::project::save(&autosave_path, s));
        match result {
            Ok(()) => {
                tracing::info!(path = %autosave_path.display(), "autosaved");
                self.last_autosave = std::time::Instant::now();
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    path = %autosave_path.display(),
                    "autosave failed"
                );
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
        if let Some(path) = self.file_path.get_untracked() {
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
        let has_plugin = self.song.with_untracked(|s| {
            s.tracks.iter().any(|t| {
                t.instrument.is_some() || !t.fx_chain.is_empty() || !t.midi_fx_chain.is_empty()
            })
        });
        if has_plugin {
            self.pending_save_path = Some(path);
            self.send_plugin(MainToChild::RequestAllStates);
        } else {
            self.finish_save(path, Vec::new());
        }
    }

    fn finish_save(&mut self, path: PathBuf, states: Vec<SlotState>) {
        self.song.update(|song| {
            for s in &states {
                let Some(track) = song.tracks.get_mut(s.track as usize) else {
                    tracing::warn!(track = s.track, ?s.slot, "save: track not found in model");
                    continue;
                };
                match s.slot {
                    PluginSlot::Instrument => {
                        if let Some(inst) = track.instrument.as_mut() {
                            inst.state = s.data.clone();
                        }
                    }
                    PluginSlot::Fx(i) => {
                        if let Some(p) = track.fx_chain.get_mut(i as usize) {
                            p.state = s.data.clone();
                        }
                    }
                    PluginSlot::MidiFx(i) => {
                        if let Some(p) = track.midi_fx_chain.get_mut(i as usize) {
                            p.state = s.data.clone();
                        }
                    }
                }
            }
        });
        if self.save_to(&path) {
            self.file_path.set(Some(path));
        }
    }

    fn save_to(&mut self, path: &Path) -> bool {
        let result = self
            .song
            .with_untracked(|s| common::project::save(path, s));
        match result {
            Ok(()) => {
                tracing::info!(path = %path.display(), "saved project");
                self.is_dirty.set(false);
                self.push_recent(path.to_path_buf());
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
        let song = self.song.get_untracked();
        self.send_plugin(MainToChild::LoadSong(song));
        self.send_audio(MainToChild::Play);
        self.send_plugin(MainToChild::Play);
        self.is_playing.set(true);
    }

    fn stop(&mut self) {
        self.send_audio(MainToChild::Stop);
        self.send_plugin(MainToChild::Stop);
        self.is_playing.set(false);
        self.playhead_beat.set(None);
    }

    fn toggle_loop(&mut self) {
        let new_val = !self.is_looping.get_untracked();
        self.is_looping.set(new_val);
        self.send_plugin(MainToChild::SetLoop(new_val));
    }

    fn set_loop_range(&mut self, start: f64, end: f64) {
        // Normalise: a degenerate range (end <= start) clears the user
        // region so the engine falls back to song-bounds looping.
        let (start, end) = if end > start {
            (start.max(0.0), end.max(0.0))
        } else {
            (0.0, 0.0)
        };
        self.song.update(|s| {
            s.loop_start_beat = start;
            s.loop_end_beat = end;
        });
        self.sync_song_to_plugin_host();
    }

    // -------- Track operations ---------------------------------------------

    fn delete_track(&mut self, idx: u32) {
        let len = self.song.with_untracked(|s| s.tracks.len());
        if idx as usize >= len {
            return;
        }
        // Stash the snapshot for undo before mutating.
        self.push_undo_snapshot();
        // Close any open plugin GUIs for the doomed track first so
        // PluginHostWindow drops on the GUI thread (not after RemoveTrack
        // races back over IPC).
        #[cfg(windows)]
        {
            self.plugin_host_windows.retain(|&(t, _), _| t != idx);
        }
        self.song.update(|s| {
            s.tracks.remove(idx as usize);
        });
        self.send_plugin(MainToChild::RemoveTrack { track: idx });
        // Selection / clip-selection cleanup. Anything that pointed past
        // the deleted track shifts down by one.
        if let Some(r) = self.selected_clip.get_untracked() {
            if r.track == idx {
                self.selected_clip.set(None);
                self.selected_notes.update(|v| v.clear());
            } else if r.track > idx {
                self.selected_clip.set(Some(ClipRef {
                    track: r.track - 1,
                    clip: r.clip,
                }));
            }
        }
        let cur_track = self.selected_track.get_untracked();
        let new_track = if cur_track == idx {
            idx.saturating_sub(1)
        } else if cur_track > idx {
            cur_track - 1
        } else {
            cur_track
        };
        let max = self
            .song
            .with_untracked(|s| s.tracks.len().saturating_sub(1) as u32);
        self.selected_track.set(new_track.min(max));
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    fn swap_tracks(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        let n = self.song.with_untracked(|s| s.tracks.len() as u32);
        if a >= n || b >= n {
            return;
        }
        self.push_undo_snapshot();
        self.song.update(|s| {
            s.tracks.swap(a as usize, b as usize);
        });
        self.send_plugin(MainToChild::SwapTracks { a, b });
        // Track-relative selection state follows the move.
        if let Some(r) = self.selected_clip.get_untracked() {
            self.selected_clip.set(Some(ClipRef {
                track: if r.track == a {
                    b
                } else if r.track == b {
                    a
                } else {
                    r.track
                },
                clip: r.clip,
            }));
        }
        let cur = self.selected_track.get_untracked();
        if cur == a {
            self.selected_track.set(b);
        } else if cur == b {
            self.selected_track.set(a);
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    fn select_track(&mut self, idx: u32) {
        let n = self.song.with_untracked(|s| s.tracks.len() as u32);
        if idx >= n {
            return;
        }
        if self.selected_track.get_untracked() != idx {
            self.selected_track.set(idx);
        }
    }

    fn begin_rename_track(&mut self, idx: u32) {
        let Some(name) = self
            .song
            .with_untracked(|s| s.tracks.get(idx as usize).map(|t| t.name.clone()))
        else {
            return;
        };
        self.track_rename_text.set(name);
        self.track_rename_idx.set(Some(idx));
    }

    fn commit_rename_track(&mut self) {
        let Some(idx) = self.track_rename_idx.get_untracked() else {
            return;
        };
        self.track_rename_idx.set(None);
        let new_name = self
            .track_rename_text
            .with_untracked(|s| s.trim().to_string());
        self.track_rename_text.set(String::new());
        if new_name.is_empty() {
            // An empty name is meaningless — treat as cancel.
            return;
        }
        self.song.update(|s| {
            if let Some(track) = s.tracks.get_mut(idx as usize) {
                track.name = new_name;
            }
        });
        self.sync_song_to_plugin_host();
    }

    fn ensure_first_track(&mut self) {
        let need = self.song.with_untracked(|s| s.tracks.is_empty());
        if need {
            self.song.update(|s| {
                s.tracks.push(Track {
                    name: "Track 1".into(),
                    ..Track::default()
                });
            });
            self.resize_track_peak_display();
        }
    }

    fn action_add_vocal_track(&mut self) {
        let index = self.song.with_untracked(|s| s.tracks.len() + 1);
        let track = Track {
            name: format!("Track {index}"),
            source: InstrumentSource::Vocal {
                speaker_id: common::voicevox::DEFAULT_SINGER_ID,
                style_name: "ノーマル".into(),
            },
            clips: vec![demo_clip()],
            ..Track::default()
        };
        self.song.update(|s| s.tracks.push(track));
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(index, "added vocal track");
    }

    fn action_add_instrument_track(&mut self) {
        let index = self.song.with_untracked(|s| s.tracks.len() + 1);
        let track = Track {
            name: format!("Track {index}"),
            source: InstrumentSource::None,
            clips: Vec::new(),
            ..Track::default()
        };
        self.song.update(|s| s.tracks.push(track));
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(index, "added instrument track");
    }

    fn action_remove_last_track(&mut self) {
        let len = self.song.with_untracked(|s| s.tracks.len());
        if len == 0 {
            return;
        }
        let removed_idx = (len - 1) as u32;
        let removed_name = self.song.try_update(|s| s.tracks.pop().map(|t| t.name));
        if let Some(Some(name)) = removed_name {
            tracing::info!(
                index = removed_idx,
                name = %name,
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
        let (new_max, is_empty) = self
            .song
            .with_untracked(|s| (s.tracks.len().saturating_sub(1) as u32, s.tracks.is_empty()));
        let cur = self.selected_track.get_untracked();
        if is_empty {
            self.selected_track.set(0);
        } else if cur > new_max {
            self.selected_track.set(new_max);
        }
        self.selected_clips
            .update(|v| v.retain(|c| c.track != removed_idx));
        if let Some(r) = self.selected_clip.get_untracked()
            && r.track == removed_idx
        {
            let last = self.selected_clips.with_untracked(|v| v.last().copied());
            self.selected_clip.set(last);
            self.selected_notes.update(|v| v.clear());
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    // -------- Clip operations ----------------------------------------------

    /// Handle a MIDI NoteOn from the active input port. Step-input mode:
    /// drops a fresh note in the selected clip at `step_cursor_beat`,
    /// then advances the cursor by `step_size_beats`. When no clip is
    /// selected the event is ignored (no fallback target).
    fn handle_midi_note_on(&mut self, pitch: u8, velocity: u8) {
        let Some(target) = self.selected_clip.get_untracked() else {
            return;
        };
        let cursor = self.step_cursor_beat;
        let step = self.step_size_beats;
        let new_idx = self.song.try_update(|s| {
            let track = s.tracks.get_mut(target.track as usize)?;
            let clip = track.clips.get_mut(target.clip as usize)?;
            // Wrap the cursor when it walks past the end of the clip so a
            // user can keep playing without having to manually reset.
            let cursor = if cursor >= clip.length_beats {
                0.0
            } else {
                cursor
            };
            let new_idx = clip.notes.len() as u32;
            clip.notes.push(common::model::Note {
                start_beat: cursor,
                duration_beats: step,
                pitch,
                velocity,
                lyric: None,
            });
            Some((new_idx, cursor + step))
        });
        if let Some(Some((new_idx, next_cursor))) = new_idx {
            self.selected_notes.set(vec![new_idx]);
            self.step_cursor_beat = next_cursor;
            self.sync_song_to_plugin_host();
        }
    }

    fn select_clip(&mut self, target: ClipRef, additive: bool) {
        let mut clips = self.selected_clips.get_untracked();
        if additive {
            // Toggle behaviour: clicking an already-selected clip removes
            // it from the set. Bitwig does the same for Shift-click.
            if let Some(pos) = clips.iter().position(|c| *c == target) {
                clips.remove(pos);
            } else {
                clips.push(target);
            }
        } else {
            clips = vec![target];
        }
        let primary = clips.last().copied();
        self.selected_clips.set(clips);
        self.selected_clip.set(primary);
        self.selected_notes.update(|v| v.clear());
        // Picking a different clip resets the step-input cursor so MIDI
        // notes start at beat 0 of the new clip rather than wherever
        // the previous one left off.
        self.step_cursor_beat = 0.0;
        if let Some(r) = primary {
            self.select_track(r.track);
        }
    }

    fn set_clip_selection(&mut self, targets: Vec<ClipRef>) {
        let primary = targets.last().copied();
        self.selected_clips.set(targets);
        self.selected_clip.set(primary);
        self.selected_notes.update(|v| v.clear());
        self.step_cursor_beat = 0.0;
        if let Some(r) = primary {
            self.select_track(r.track);
        }
    }

    fn set_clip_positions(&mut self, entries: &[(ClipRef, f64)]) {
        self.song.update(|s| {
            for (target, beat) in entries {
                let new_start = beat.max(0.0);
                if let Some(track) = s.tracks.get_mut(target.track as usize)
                    && let Some(clip) = track.clips.get_mut(target.clip as usize)
                {
                    clip.start_beat = new_start;
                }
            }
        });
        self.sync_song_to_plugin_host();
    }

    fn resize_clip(&mut self, target: ClipRef, new_length_beats: f64) {
        // Don't shrink to zero — Bitwig keeps a minimum of one bar; we use
        // 1/16 as a softer floor so VOICEVOX clips can be tight.
        let new_length_beats = new_length_beats.max(0.0625);
        self.song.update(|s| {
            if let Some(track) = s.tracks.get_mut(target.track as usize)
                && let Some(clip) = track.clips.get_mut(target.clip as usize)
            {
                clip.length_beats = new_length_beats;
            }
        });
        self.sync_song_to_plugin_host();
    }

    fn create_clip(&mut self, track_idx: u32, start_beat: f64) {
        let start_beat = start_beat.max(0.0);
        let new_idx = self.song.try_update(|s| {
            let track = s.tracks.get_mut(track_idx as usize)?;
            let new_idx = track.clips.len() as u32;
            track.clips.push(Clip {
                name: format!("Clip {}", track.clips.len() + 1),
                start_beat,
                length_beats: DEFAULT_CLIP_LENGTH,
                notes: Vec::new(),
            });
            Some(new_idx)
        });
        if let Some(Some(new_idx)) = new_idx {
            let r = ClipRef {
                track: track_idx,
                clip: new_idx,
            };
            self.selected_clip.set(Some(r));
            self.selected_clips.set(vec![r]);
            self.selected_notes.update(|v| v.clear());
            self.select_track(track_idx);
            self.sync_song_to_plugin_host();
        }
    }

    fn delete_selected_clip(&mut self) {
        let mut targets = self.selected_clips.get_untracked();
        if targets.is_empty() {
            return;
        }
        self.selected_clips.set(Vec::new());
        // Sort by (track ASC, clip DESC) so within each track the higher
        // indices are removed first — that keeps the lower ones valid.
        targets.sort_by(|a, b| a.track.cmp(&b.track).then(b.clip.cmp(&a.clip)));
        self.song.update(|s| {
            for target in &targets {
                if let Some(track) = s.tracks.get_mut(target.track as usize)
                    && (target.clip as usize) < track.clips.len()
                {
                    track.clips.remove(target.clip as usize);
                }
            }
        });
        self.selected_clip.set(None);
        self.selected_notes.update(|v| v.clear());
        // Indices may have shifted — drop any stale primary; user has to
        // click again to re-pick a clip.
        self.sync_song_to_plugin_host();
    }

    // -------- Note operations ----------------------------------------------

    fn select_note(&mut self, note: u32, additive: bool) {
        self.selected_notes.update(|v| {
            if !additive {
                v.clear();
            }
            if !v.contains(&note) {
                v.push(note);
            }
        });
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
        let new_idx = self.song.try_update(|s| {
            let track = s.tracks.get_mut(track_idx as usize)?;
            let clip = track.clips.get_mut(clip_idx as usize)?;
            let new_idx = clip.notes.len() as u32;
            clip.notes.push(Note {
                start_beat,
                duration_beats: duration,
                pitch,
                velocity: 100,
                lyric: None,
            });
            Some(new_idx)
        });
        let Some(Some(new_idx)) = new_idx else {
            return;
        };
        let r = ClipRef {
            track: track_idx,
            clip: clip_idx,
        };
        self.selected_clip.set(Some(r));
        self.selected_clips.update(|v| {
            if !v.contains(&r) {
                *v = vec![r];
            }
        });
        self.selected_notes.set(vec![new_idx]);
        self.sync_song_to_plugin_host();
    }


    fn set_note_positions(&mut self, entries: &[(u32, f64, u8)]) {
        let Some(r) = self.selected_clip.get_untracked() else {
            return;
        };
        self.song.update(|s| {
            let Some(track) = s.tracks.get_mut(r.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get_mut(r.clip as usize) else {
                return;
            };
            for &(idx, beat, pitch) in entries {
                let Some(note) = clip.notes.get_mut(idx as usize) else {
                    continue;
                };
                note.start_beat = beat.max(0.0);
                note.pitch = pitch;
            }
        });
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
        self.song.update(|s| {
            let Some(track) = s.tracks.get_mut(track_idx as usize) else {
                return;
            };
            let Some(clip) = track.clips.get_mut(clip_idx as usize) else {
                return;
            };
            let Some(note) = clip.notes.get_mut(note_idx as usize) else {
                return;
            };
            note.duration_beats = new_duration;
        });
        self.sync_song_to_plugin_host();
    }

    fn delete_selected_notes(&mut self) {
        let Some(r) = self.selected_clip.get_untracked() else {
            return;
        };
        let mut indices = self.selected_notes.get_untracked();
        if indices.is_empty() {
            return;
        }
        // Sort indices descending so each removal stays valid.
        indices.sort_unstable_by(|a, b| b.cmp(a));
        self.song.update(|s| {
            let Some(track) = s.tracks.get_mut(r.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get_mut(r.clip as usize) else {
                return;
            };
            for i in &indices {
                let i = *i as usize;
                if i < clip.notes.len() {
                    clip.notes.remove(i);
                }
            }
        });
        self.selected_notes.update(|v| v.clear());
        self.sync_song_to_plugin_host();
    }

    fn set_selected_note_lyric(&mut self, lyric: String) {
        let Some(r) = self.selected_clip.get_untracked() else {
            return;
        };
        let selected = self.selected_notes.get_untracked();
        let trimmed = lyric.trim();
        let value = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self.song.update(|s| {
            let Some(track) = s.tracks.get_mut(r.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get_mut(r.clip as usize) else {
                return;
            };
            for &i in &selected {
                if let Some(n) = clip.notes.get_mut(i as usize) {
                    n.lyric = value.clone();
                }
            }
        });
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
        _name: String,
    ) {
        let track_idx = track as usize;
        self.ensure_first_track();
        self.song.update(|s| {
            let Some(t) = s.tracks.get_mut(track_idx) else {
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
                        plugin_id: id.clone(),
                        format,
                        state,
                    });
                }
                PluginSlot::Fx(i) => {
                    let i = i as usize;
                    let (existing_state, format) = t
                        .fx_chain
                        .get(i)
                        .map(|p| (p.state.clone(), p.format))
                        .unwrap_or((None, PluginFormat::Clap));
                    let inst = common::model::PluginInstance {
                        plugin_id: id.clone(),
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
                        plugin_id: id.clone(),
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
        });
    }

    fn toggle_slot_gui(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        let track = self.selected_track.get_untracked();
        #[cfg(windows)]
        {
            if self.plugin_host_windows.contains_key(&(track, slot)) {
                self.send_plugin(MainToChild::CloseSlotGui { track, slot });
                return;
            }
            let label = self
                .song
                .with_untracked(|s| {
                    s.tracks
                        .get(track as usize)
                        .and_then(|t| self.slot_ref_name(t, slot))
                })
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
        let track_idx = self.selected_track.get_untracked();
        self.send_plugin(MainToChild::RemoveSlotPlugin {
            track: track_idx,
            slot,
        });
        self.song.update(|s| {
            if let Some(track) = s.tracks.get_mut(track_idx as usize) {
                match slot {
                    PluginSlot::Instrument => track.instrument = None,
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
        });
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
            self.song.with_untracked(|s| {
                common::timing::playhead_to_beat(
                    Some(s),
                    common::audio_bridge::SAMPLE_RATE,
                    playhead_samples,
                )
                .map(|b| b as f32)
            })
        };
        if next_beat != self.playhead_beat.get_untracked() {
            self.playhead_beat.set(next_beat);
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
        let new_l = common::meter::update_peak(self.peak_l_display.get_untracked(), peak_l_raw, RELEASE);
        let new_r = common::meter::update_peak(self.peak_r_display.get_untracked(), peak_r_raw, RELEASE);
        self.peak_l_display.set(new_l);
        self.peak_r_display.set(new_r);
        self.peak_l_norm
            .set(common::meter::db_to_norm(common::meter::linear_to_db(new_l)));
        self.peak_r_norm
            .set(common::meter::db_to_norm(common::meter::linear_to_db(new_r)));
    }

    fn set_master_gain(&mut self, gain: f32) {
        let clamped = gain.clamp(0.0, 1.0);
        self.master_gain.set(clamped);
        self.send_audio(MainToChild::SetMasterGain(clamped));
    }

    // -------- Plugin picker -----------------------------------------------

    fn select_plugin_from_db(&mut self, id: String) {
        self.is_plugin_picker_open.set(false);
        let Some(db) = self.plugin_db.clone() else {
            tracing::warn!(id, "plugin_db not available");
            return;
        };
        let Some(entry) = db.find_by_id(&id) else {
            tracing::error!(id, "picked plugin id not in database");
            return;
        };
        let path = entry.path.clone();
        let entry_id = entry.id.clone();
        let entry_format = entry.format;
        self.ensure_first_track();
        let track_idx = self.selected_track.get_untracked();
        let target = self.plugin_picker_target.get_untracked();
        let dest_slot = self.song.with_untracked(|s| match target {
            PickerTarget::Instrument => PluginSlot::Instrument,
            PickerTarget::Fx => {
                let next = s
                    .tracks
                    .get(track_idx as usize)
                    .map(|t| t.fx_chain.len() as u32)
                    .unwrap_or(0);
                PluginSlot::Fx(next)
            }
            PickerTarget::MidiFx => {
                let next = s
                    .tracks
                    .get(track_idx as usize)
                    .map(|t| t.midi_fx_chain.len() as u32)
                    .unwrap_or(0);
                PluginSlot::MidiFx(next)
            }
        });
        self.send_plugin(MainToChild::SetSlotPlugin {
            track: track_idx,
            slot: dest_slot,
            format: entry_format,
            path,
            plugin_id: entry_id.clone(),
            initial_state: None,
        });
        self.song.update(|s| {
            if let Some(track) = s.tracks.get_mut(track_idx as usize) {
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
        });
    }

    // -------- VOICEVOX -----------------------------------------------------

    fn begin_vocal_synth(&self, cx: &mut EventContext) {
        let song = self.song.get_untracked();
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
            let msg = if errors.is_empty() {
                "合成結果なし（Vocal トラックがないか VOICEVOX が応答しません）".to_string()
            } else {
                format!("合成エラー: {}", errors.join("; "))
            };
            self.status_message.set(msg);
            return;
        }

        let ok_results: Vec<_> = results.iter().filter(|r| r.error.is_none()).collect();
        let err_count = results.len() - ok_results.len();
        let msg = if err_count > 0 {
            let first_err = results
                .iter()
                .find_map(|r| r.error.as_deref())
                .unwrap_or("不明");
            format!(
                "合成: {} 成功, {} 失敗 ({})",
                ok_results.len(),
                err_count,
                first_err
            )
        } else {
            format!("合成完了 — {} クリップ。Play で再生", ok_results.len())
        };
        self.status_message.set(msg);

        let song_snapshot = self.song.get_untracked();
        for r in &ok_results {
            let clip_start_beat = song_snapshot
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0);
            let samples_per_beat =
                common::audio_bridge::SAMPLE_RATE as f64 * 60.0 / song_snapshot.bpm as f64;
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
        if self.is_rescanning.get_untracked() {
            return;
        }
        self.is_rescanning.set(true);
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
        self.is_rescanning.set(false);
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
        self.song.update(|s| {
            if let Some(t) = s.tracks.get_mut(track as usize) {
                t.volume = v;
            }
        });
        self.send_plugin(MainToChild::SetTrackVolume { track, volume: v });
    }

    fn set_track_pan(&mut self, track: u32, pan: f32) {
        let p = pan.clamp(-1.0, 1.0);
        self.song.update(|s| {
            if let Some(t) = s.tracks.get_mut(track as usize) {
                t.pan = p;
            }
        });
        self.send_plugin(MainToChild::SetTrackPan { track, pan: p });
    }

    fn toggle_track_mute(&mut self, track: u32) {
        let muted = self.song.try_update(|s| {
            let t = s.tracks.get_mut(track as usize)?;
            t.muted = !t.muted;
            Some(t.muted)
        });
        let Some(Some(muted)) = muted else {
            return;
        };
        self.send_plugin(MainToChild::SetTrackMuted { track, muted });
    }

    fn toggle_track_solo(&mut self, track: u32) {
        let solo = self.song.try_update(|s| {
            let t = s.tracks.get_mut(track as usize)?;
            t.solo = !t.solo;
            Some(t.solo)
        });
        let Some(Some(solo)) = solo else {
            return;
        };
        self.send_plugin(MainToChild::SetTrackSolo { track, solo });
    }

    fn on_track_peaks_tick(&mut self, peaks: &[(f32, f32)]) {
        const RELEASE: f32 = 0.85;
        let n = self.song.with_untracked(|s| s.tracks.len());
        self.track_peak_display.update(|disp| {
            if disp.len() != n {
                disp.resize(n, (0.0, 0.0));
            }
            for (i, d) in disp.iter_mut().enumerate() {
                let (l, r) = peaks.get(i).copied().unwrap_or((0.0, 0.0));
                d.0 = common::meter::update_peak(d.0, l, RELEASE);
                d.1 = common::meter::update_peak(d.1, r, RELEASE);
            }
        });
    }

    fn rebuild_picker_entries(&mut self) {
        let Some(db) = self.plugin_db.as_ref() else {
            self.plugin_picker_entries.set(Vec::new());
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
        self.plugin_picker_entries.set(v);
    }

    fn refresh_picker_visible(&mut self) {
        let feature_key: &str = match self.plugin_picker_target.get_untracked() {
            PickerTarget::Instrument => "instrument",
            PickerTarget::Fx => "audio-effect",
            PickerTarget::MidiFx => "note-effect",
        };
        let visible: Vec<PluginPickEntry> = self.plugin_picker_entries.with_untracked(|entries| {
            entries
                .iter()
                .filter(|e| e.features.iter().any(|f| f == feature_key))
                .cloned()
                .collect()
        });
        self.plugin_picker_visible.set(visible);
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
        self.status_message.set("WAV 書き出し中...".to_string());
        let song = self.song.get_untracked();
        self.send_plugin(MainToChild::LoadSong(song));
        self.send_plugin(MainToChild::ExportWav { path });
    }
}

// ---------------------------------------------------------------------------
// Free standing helpers
// ---------------------------------------------------------------------------

/// Load the persisted "Open Recent" list from LocalAppData. Failures are
/// logged at debug — the menu just shows up empty when the file is
/// missing or corrupt, which is the expected first-launch behaviour.
fn load_recent_files() -> common::recent::RecentFiles {
    let Some(path) = common::recent::default_path() else {
        return Default::default();
    };
    match common::recent::load(&path) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = ?e, path = %path.display(), "no recent.json");
            Default::default()
        }
    }
}

fn load_recent_files_display() -> Vec<String> {
    load_recent_files()
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect()
}

/// Free-standing variant of `AppData::resolve_name`, usable inside
/// `Memo::new` closures (which can't borrow `&self`).
fn resolve_plugin_name(plugin_db: &Option<Arc<PluginDatabase>>, plugin_id: &str) -> String {
    plugin_db
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
