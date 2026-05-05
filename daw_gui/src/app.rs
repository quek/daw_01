//! Bitwig-style DAW GUI state.
//!
//! 状態は 3 つに分けて持つ:
//!   1. **song** — `Track → Clip → Note` のツリー。あらゆる編集で mutate し、
//!      Play / clip-edit のたびに plugin_host へ push する。
//!   2. **selection** — 選択中の track / clip / notes。inspector・piano roll・
//!      lyric panel の入力源。
//!   3. **view state** — zoom / scroll / playhead / peak meter。
//!
//! gui_01 (daw-ui) は immediate-mode + `Edit<M>` クロージャ方式:
//! - 状態は plain mutable field
//! - 派生は method (`pub fn track_headers(&self) -> Vec<TrackHeader>` 等)
//! - background thread → UI event は `EventLoopProxy<AppEvent>` 経由

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const UNDO_LIMIT: usize = 200;

use common::model::{Clip, InstrumentSource, Note, Song, Track};
use common::plugin_db::PluginDatabase;
use common::plugin_format::PluginFormat;
use common::protocol::{MainToChild, PluginSlot, SlotState};
use tokio::sync::mpsc::UnboundedSender;
use winit::event_loop::EventLoopProxy;

#[derive(Debug, Clone, PartialEq)]
pub struct TrackMixEntry {
    pub index: u32,
    pub name: String,
    pub volume: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    /// raw linear amplitude (`-1.0..=1.0` のうち post-smoothing peak)。
    /// `Ui::level_meter` は内部で dB 変換するので、view 側ではこのまま渡す。
    pub peak_l_raw: f32,
    pub peak_r_raw: f32,
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
            peak_l_raw: 0.0,
            peak_r_raw: 0.0,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ClipRef {
    pub track: u32,
    pub clip: u32,
}

pub const ARRANGE_PX_PER_BEAT: f32 = 24.0;
pub const ARRANGE_TRACK_HEIGHT: f32 = 56.0;
pub const DEFAULT_NOTE_DURATION: f64 = 0.25;
pub const DEFAULT_CLIP_LENGTH: f64 = 4.0;

pub struct AppData {
    // -------- Song / file --------
    pub song: Song,
    pub file_path: Option<PathBuf>,

    // -------- Selection --------
    pub selected_track: u32,
    pub selected_clip: Option<ClipRef>,
    pub selected_clips: Vec<ClipRef>,
    pub selected_notes: Vec<u32>,

    // -------- View state --------
    pub bottom_panel: u8,
    pub arrange_zoom_x: f32,
    pub arrange_scroll_beat: f32,
    /// arrangement の 1 track row 高さ (px)。Alt+wheel で 16..96 に縦ズーム。
    /// default は `ARRANGE_TRACK_HEIGHT`。
    pub arrange_track_row_h: f32,
    pub pianoroll_zoom_x: f32,
    pub pianoroll_zoom_y: f32,
    pub pianoroll_top_pitch: u8,
    pub pianoroll_scroll_beat: f32,
    pub pianoroll_notes_generation: u64,

    // -------- Playback / metering --------
    pub is_playing: bool,
    pub is_looping: bool,
    pub playhead_beat: Option<f32>,
    pub master_gain: f32,
    pub peak_l_display: f32,
    pub peak_r_display: f32,
    pub peak_l_norm: f32,
    pub peak_r_norm: f32,

    // -------- Plugin database / picker --------
    pub plugin_db: Option<Arc<PluginDatabase>>,
    pub plugin_picker_entries: Vec<PluginPickEntry>,
    pub plugin_picker_visible: Vec<PluginPickEntry>,
    pub is_plugin_picker_open: bool,
    pub plugin_picker_target: PickerTarget,

    // -------- Save flow / IPC --------
    pub pending_save_path: Option<PathBuf>,
    pub audio_tx: Option<UnboundedSender<MainToChild>>,
    pub plugin_tx: Option<UnboundedSender<MainToChild>>,
    #[cfg(windows)]
    pub plugin_host_windows:
        HashMap<(u32, PluginSlot), crate::view::plugin_embed::PluginHostWindow>,

    // -------- Mixer --------
    pub track_peak_display: Vec<(f32, f32)>,

    // -------- Background workers --------
    pub synth_result: Arc<Mutex<Vec<common::voicevox::SynthResult>>>,
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    pub is_rescanning: bool,
    pub status_message: String,

    pub track_rename_idx: Option<u32>,
    pub track_rename_text: String,

    pub undo_stack: VecDeque<Song>,
    pub redo_stack: VecDeque<Song>,

    pub is_help_open: bool,

    pub recent_files: common::recent::RecentFiles,

    pub is_dirty: bool,
    pub last_autosave: std::time::Instant,
    pub is_dragging: bool,
    pub midi_input_label: String,

    pub step_cursor_beat: f64,
    pub step_size_beats: f64,

    /// 背景スレッド (autosave / playhead poll / MIDI / IPC bridge / VOICEVOX
    /// 合成 / plugin DB rescan) からメインスレッドへ AppEvent を送るためのプロキシ。
    pub event_proxy: EventLoopProxy<AppEvent>,
}

impl AppData {
    pub fn new(
        audio_tx: UnboundedSender<MainToChild>,
        plugin_tx: UnboundedSender<MainToChild>,
        // 将来的な auto-select 用に予約。現在は song に反映していない。
        _clap_plugin_path: Option<PathBuf>,
        plugin_db: Option<Arc<PluginDatabase>>,
        event_proxy: EventLoopProxy<AppEvent>,
    ) -> Self {
        let song = Song {
            tracks: vec![Track {
                name: "Track 1".into(),
                ..Track::default()
            }],
            ..Song::default()
        };
        let initial_peak_display = vec![(0.0, 0.0); song.tracks.len()];
        let plugin_picker_entries = plugin_db
            .as_ref()
            .map(|db| {
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
                v
            })
            .unwrap_or_default();

        Self {
            song,
            file_path: None,
            selected_track: 0,
            selected_clip: None,
            selected_clips: Vec::new(),
            selected_notes: Vec::new(),
            bottom_panel: 0,
            arrange_zoom_x: ARRANGE_PX_PER_BEAT,
            arrange_scroll_beat: 0.0,
            arrange_track_row_h: ARRANGE_TRACK_HEIGHT,
            pianoroll_zoom_x: 64.0,
            pianoroll_zoom_y: 14.0,
            pianoroll_top_pitch: 84, // C6
            pianoroll_scroll_beat: 0.0,
            pianoroll_notes_generation: 0,
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
            pending_save_path: None,
            audio_tx: Some(audio_tx),
            plugin_tx: Some(plugin_tx),
            #[cfg(windows)]
            plugin_host_windows: HashMap::new(),
            track_peak_display: initial_peak_display,
            synth_result: Arc::new(Mutex::new(Vec::new())),
            rescan_result: Arc::new(Mutex::new(None)),
            is_rescanning: false,
            status_message: String::new(),
            track_rename_idx: None,
            track_rename_text: String::new(),
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            is_help_open: false,
            recent_files: load_recent_files(),
            is_dirty: false,
            last_autosave: std::time::Instant::now(),
            is_dragging: false,
            midi_input_label: String::new(),
            step_cursor_beat: 0.0,
            step_size_beats: DEFAULT_NOTE_DURATION,
            event_proxy,
        }
    }

    // -------- Derived snapshots (毎フレーム計算; cache が必要なら view 側で持つ) -----

    pub fn bpm(&self) -> f32 {
        self.song.bpm
    }

    pub fn track_mix(&self) -> Vec<TrackMixEntry> {
        self.song
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (l, r) = self.track_peak_display.get(i).copied().unwrap_or((0.0, 0.0));
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
                    peak_l_raw: l,
                    peak_r_raw: r,
                }
            })
            .collect()
    }

    pub fn selected_lyric(&self) -> String {
        let Some(n_idx) = self.selected_notes.first().copied() else {
            return String::new();
        };
        let Some(r) = self.selected_clip else {
            return String::new();
        };
        let Some(track) = self.song.tracks.get(r.track as usize) else {
            return String::new();
        };
        let Some(clip) = track.clips.get(r.clip as usize) else {
            return String::new();
        };
        let Some(note) = clip.notes.get(n_idx as usize) else {
            return String::new();
        };
        note.lyric.clone().unwrap_or_default()
    }

    pub fn selected_track_label(&self) -> String {
        let sel = self.selected_track;
        self.song
            .tracks
            .get(sel as usize)
            .map(|t| {
                if t.name.is_empty() {
                    format!("Track {}", sel + 1)
                } else {
                    t.name.clone()
                }
            })
            .unwrap_or_else(|| format!("Track {}", sel + 1))
    }

    pub fn inspector_chain(&self) -> Vec<ChainEntry> {
        let Some(track) = self.song.tracks.get(self.selected_track as usize) else {
            return Vec::new();
        };
        let mut chain: Vec<ChainEntry> = Vec::new();
        for (i, p) in track.midi_fx_chain.iter().enumerate() {
            chain.push(ChainEntry {
                slot_kind: 0,
                slot_index: i as u32,
                section_label: "MIDI FX".into(),
                plugin_name: resolve_plugin_name(&self.plugin_db, &p.plugin_id),
            });
        }
        if let Some(inst) = track.instrument.as_ref() {
            chain.push(ChainEntry {
                slot_kind: 1,
                slot_index: 0,
                section_label: "Instrument".into(),
                plugin_name: resolve_plugin_name(&self.plugin_db, &inst.plugin_id),
            });
        }
        for (i, p) in track.fx_chain.iter().enumerate() {
            chain.push(ChainEntry {
                slot_kind: 2,
                slot_index: i as u32,
                section_label: "FX".into(),
                plugin_name: resolve_plugin_name(&self.plugin_db, &p.plugin_id),
            });
        }
        chain
    }

    // -------- Undo/Redo ----------------------------------------------------

    fn push_undo_snapshot(&mut self) {
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(self.song.clone());
        self.redo_stack.clear();
    }

    fn undo(&mut self) {
        let Some(prev) = self.undo_stack.pop_back() else {
            return;
        };
        let current = std::mem::replace(&mut self.song, prev);
        self.redo_stack.push_back(current);
        self.after_undo_redo();
    }

    fn redo(&mut self) {
        let Some(next) = self.redo_stack.pop_back() else {
            return;
        };
        let current = std::mem::replace(&mut self.song, next);
        self.undo_stack.push_back(current);
        self.after_undo_redo();
    }

    fn after_undo_redo(&mut self) {
        // selected_clip が undo 後も存在するなら維持、消えていれば None。
        // (常に None にすると undo のたびにピアノロールがプレースホルダに戻ってしまう)
        if let Some(r) = self.selected_clip
            && self
                .song
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .is_none()
        {
            self.selected_clip = None;
        }
        self.selected_clips.retain(|r| {
            self.song
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .is_some()
        });
        // note の index は undo で容易にずれるため、安全側で clear する。
        self.selected_notes.clear();
        self.track_rename_idx = None;
        self.track_rename_text.clear();
        let track_max = self.song.tracks.len().saturating_sub(1) as u32;
        let is_empty = self.song.tracks.is_empty();
        if is_empty {
            self.selected_track = 0;
        } else if self.selected_track > track_max {
            self.selected_track = track_max;
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        self.pianoroll_notes_generation += 1;
    }

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
                | AppEvent::ResizeNotes(_)
                | AppEvent::SetNotePositions(_)
                | AppEvent::DeleteSelectedNotes
                | AppEvent::SetSelectedNoteLyric(_)
                | AppEvent::QuantizeSelectedNotes(_)
                | AppEvent::SelectPluginFromDb(_)
                | AppEvent::RemoveSlot { .. }
        )
    }

    /// 選択中ノートを JSON シリアライズ。OS clipboard 経由で root.rs から
    /// `Ui::set_clipboard_text` に渡される。何も copy できない (選択無し /
    /// クリップ未選択 / シリアライズ失敗) 場合は `None`。
    /// 戻り値は `(json, note_count)` 。`note_count` は呼び出し側で status_message
    /// 表示等に使う (ここで status_message を書かないのは `&self` で済ませるため)。
    pub fn copy_selected_notes_as_json(&self) -> Option<(String, usize)> {
        let r = self.selected_clip?;
        if self.selected_notes.is_empty() {
            return None;
        }
        let track = self.song.tracks.get(r.track as usize)?;
        let clip = track.clips.get(r.clip as usize)?;
        let mut copied: Vec<Note> = self
            .selected_notes
            .iter()
            .filter_map(|i| clip.notes.get(*i as usize).cloned())
            .collect();
        if copied.is_empty() {
            return None;
        }
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
        let json = serde_json::to_string(&copied).ok()?;
        Some((json, count))
    }

    /// OS clipboard から取得した text を `Vec<Note>` として deserialize し、
    /// 既存の paste ロジックで貼り付ける。他アプリの text が来た場合は何もしない。
    pub fn paste_notes_from_json(&mut self, json: &str) {
        let Ok(clipboard) = serde_json::from_str::<Vec<Note>>(json) else {
            return;
        };
        if clipboard.is_empty() {
            return;
        }
        let Some(r) = self.selected_clip else {
            self.status_message = "貼り付け先のクリップが選択されていません".to_string();
            return;
        };
        let playhead = self.playhead_beat;
        let anchor = if let Some(playhead) = playhead {
            let clip_start = self
                .song
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0);
            (playhead as f64 - clip_start).max(0.0)
        } else {
            0.0
        };
        let count = clipboard.len();
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(r.clip as usize) else {
            return;
        };
        let mut new_indices = Vec::with_capacity(clipboard.len());
        for src in &clipboard {
            let mut n = src.clone();
            n.start_beat += anchor;
            new_indices.push(clip.notes.len() as u32);
            clip.notes.push(n);
        }
        self.selected_notes = new_indices;
        self.sync_song_to_plugin_host();
        self.status_message = format!("貼り付け: {count} ノート");
    }

    fn set_note_velocity(&mut self, note_idx: u32, velocity: u8) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(r.clip as usize) else {
            return;
        };
        let Some(note) = clip.notes.get_mut(note_idx as usize) else {
            return;
        };
        note.velocity = velocity;
        self.sync_song_to_plugin_host();
    }

    fn quantize_selected_notes(&mut self, div: u8) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let div = div.max(1) as f64;
        let snap = |b: f64| (b * div).round() / div;
        let selected = self.selected_notes.clone();
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
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
        self.sync_song_to_plugin_host();
        self.pianoroll_notes_generation += 1;
    }

    fn resize_track_peak_display(&mut self) {
        let n = self.song.tracks.len();
        self.track_peak_display.resize(n, (0.0, 0.0));
    }
}

/// 一部 variant は将来の機能 (rename UI / quantize / autosave / piano-roll
/// shortcut 等) で使う予定なので、現時点で未参照でも残す。新規 variant 追加時に
/// 既存の event handler と一貫性を保つため、enum 全体に `#[allow(dead_code)]`
/// を付ける。
#[allow(dead_code)]
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
    PushUndoSnapshot,
    QuantizeSelectedNotes(u8),
    SetNoteVelocity { note: u32, velocity: u8 },
    AddVocalTrack,
    AddInstrumentTrack,
    RemoveLastTrack,
    DeleteTrack(u32),
    MoveTrackUp(u32),
    MoveTrackDown(u32),
    /// 新順での `Track.id` 列で `song.tracks` を並び替える (drag&drop reorder)。
    /// order に含まれない track はそのまま末尾に残す。
    ReorderTracks(Vec<u32>),
    SelectTrack(u32),
    BeginRenameTrack(u32),
    RenameTrackChanged(String),
    CommitRenameTrack,
    CancelRenameTrack,
    ToggleHelp,
    CloseHelp,
    OpenRecent(PathBuf),
    AutosaveTick,
    BeginDrag,
    EndDrag,
    MidiNoteOn { pitch: u8, velocity: u8 },
    MidiNoteOff { pitch: u8 },
    MidiInputOpened(Option<String>),

    // -------- Bottom panel -------------------------------------------------
    SelectBottomPanel(u8),

    // -------- Arrangement / clip operations -------------------------------
    SelectClip { target: ClipRef, additive: bool },
    SetClipSelection(Vec<ClipRef>),
    ClearSelection,
    ResizeClip { target: ClipRef, length: f64 },
    SetClipPositions(Vec<(ClipRef, f64)>),
    CreateClip { track: u32, start_beat: f64 },
    DeleteSelectedClip,

    // -------- Piano roll / note operations --------------------------------
    SelectNote { note: u32, additive: bool },
    ClearNoteSelection,
    AddNote {
        track: u32,
        clip: u32,
        start_beat: f64,
        duration: f64,
        pitch: u8,
    },
    SetNotePositions(Vec<(u32, f64, u8)>),
    SetNoteSelection(Vec<u32>),
    ResizeNote {
        track: u32,
        clip: u32,
        note: u32,
        duration: f64,
    },
    ResizeNotes(Vec<(u32, f64, f64)>),
    DeleteSelectedNotes,
    SetSelectedNoteLyric(String),

    // -------- Plugin picker / chain ---------------------------------------
    OpenPluginPickerFor(PickerTarget),
    ClosePluginPicker,
    SelectPluginFromDb(String),
    ToggleSlotGui { slot_kind: u8, slot_index: u32 },
    RemoveSlot { slot_kind: u8, slot_index: u32 },
    /// inspector chain (MIDI FX → Instrument → FX を一列にした list) の reorder。
    /// `order` は新順での旧 chain index 列。section 跨ぎ (slot_kind が変わる移動)
    /// は handler 側で拒否する。
    ReorderInspectorChain(Vec<usize>),
    SetMasterGain(f32),

    // -------- IPC events from plugin_host ---------------------------------
    Tick { samples: u64, peak_l: f32, peak_r: f32 },
    GuiOpenedFromChild { track: u32, slot: PluginSlot, width: u32, height: u32 },
    GuiRequestResizeFromChild { track: u32, slot: PluginSlot, width: u32, height: u32 },
    GuiClosedFromChild { track: u32, slot: PluginSlot },
    SlotPluginLoadedFromChild { track: u32, slot: PluginSlot, id: String, name: String },
    AllStatesReceived(Vec<SlotState>),
    RescanPluginDb,
    PluginDbRescanCompleted,

    // -------- Scroll / zoom -----------------------------------------------
    SetArrangeScroll(f32),
    SetArrangeZoom(f32),
    SetArrangeTrackRowH(f32),
    SetPianoRollScrollX(f32),
    SetPianoRollTopPitch(u8),
    SetPianoRollZoomX(f32),
    SetPianoRollZoomY(f32),
    SetLoopRange { start: f64, end: f64 },

    // -------- Mixer -------------------------------------------------------
    SetTrackVolume { track: u32, amp: f32 },
    SetTrackPan { track: u32, pan: f32 },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    TrackPeaksTick(Vec<(f32, f32)>),

    // -------- VOICEVOX ----------------------------------------------------
    SynthesizeVocal,
    VocalSynthCompleted,

    // -------- WAV export -------------------------------------------------
    ExportWav,
    ExportWavComplete { error: Option<String> },
}

impl AppData {
    /// AppEvent dispatcher。view から `Edit::mutate` 経由で、background thread
    /// から `EventLoopProxy<AppEvent>` 経由で呼ばれる。
    pub fn handle_event(&mut self, event: AppEvent) {
        if Self::is_undoable(&event) {
            self.push_undo_snapshot();
        }

        match event {
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
                if self.is_playing {
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
            AppEvent::QuantizeSelectedNotes(div) => {
                self.quantize_selected_notes(div);
            }
            AppEvent::SetNoteVelocity { note, velocity } => {
                self.set_note_velocity(note, velocity);
            }
            AppEvent::AddVocalTrack => self.action_add_vocal_track(),
            AppEvent::AddInstrumentTrack => self.action_add_instrument_track(),
            AppEvent::RemoveLastTrack => self.action_remove_last_track(),
            AppEvent::DeleteTrack(idx) => self.delete_track(idx),
            AppEvent::MoveTrackUp(idx) => self.swap_tracks(idx, idx.saturating_sub(1)),
            AppEvent::MoveTrackDown(idx) => self.swap_tracks(idx, idx + 1),
            AppEvent::ReorderTracks(order) => self.reorder_tracks(&order),
            AppEvent::SelectTrack(idx) => self.select_track(idx),
            AppEvent::BeginRenameTrack(idx) => {
                self.begin_rename_track(idx);
            }
            AppEvent::RenameTrackChanged(text) => {
                self.track_rename_text = text;
            }
            AppEvent::CommitRenameTrack => self.commit_rename_track(),
            AppEvent::CancelRenameTrack => {
                self.track_rename_idx = None;
                self.track_rename_text.clear();
            }
            AppEvent::ToggleHelp => {
                self.is_help_open = !self.is_help_open;
            }
            AppEvent::CloseHelp => {
                self.is_help_open = false;
            }
            AppEvent::OpenRecent(path) => {
                self.action_open_path(path);
            }
            AppEvent::AutosaveTick => {
                self.maybe_autosave();
            }
            AppEvent::BeginDrag => {
                self.is_dragging = true;
            }
            AppEvent::EndDrag => {
                self.is_dragging = false;
                let song = self.song.clone();
                self.send_audio(MainToChild::LoadSong(song));
            }
            AppEvent::MidiNoteOn { pitch, velocity } => {
                self.handle_midi_note_on(pitch, velocity);
            }
            AppEvent::MidiNoteOff { pitch: _ } => {
                // step-input mode は note end を追跡しない。
            }
            AppEvent::MidiInputOpened(name) => {
                let label = name.clone().unwrap_or_default();
                self.midi_input_label = label.clone();
                if name.is_some() {
                    self.status_message = format!("MIDI 入力: {label}");
                }
            }
            AppEvent::SelectBottomPanel(p) => {
                self.bottom_panel = p;
            }
            AppEvent::SelectClip { target, additive } => {
                self.select_clip(target, additive);
            }
            AppEvent::SetClipSelection(targets) => {
                self.set_clip_selection(targets);
            }
            AppEvent::ClearSelection => {
                self.selected_clip = None;
                self.selected_clips.clear();
                self.selected_notes.clear();
            }
            AppEvent::ResizeClip { target, length } => {
                self.resize_clip(target, length);
            }
            AppEvent::SetClipPositions(entries) => {
                self.set_clip_positions(&entries);
            }
            AppEvent::CreateClip { track, start_beat } => {
                self.create_clip(track, start_beat);
            }
            AppEvent::DeleteSelectedClip => self.delete_selected_clip(),
            AppEvent::SelectNote { note, additive } => {
                self.select_note(note, additive);
            }
            AppEvent::ClearNoteSelection => self.selected_notes.clear(),
            AppEvent::AddNote {
                track,
                clip,
                start_beat,
                duration,
                pitch,
            } => {
                self.add_note(track, clip, start_beat, duration, pitch);
            }
            AppEvent::ResizeNote { track, clip, note, duration } => {
                self.resize_note(track, clip, note, duration);
            }
            AppEvent::SetNotePositions(entries) => {
                self.set_note_positions(&entries);
            }
            AppEvent::ResizeNotes(entries) => {
                self.resize_notes(&entries);
            }
            AppEvent::SetNoteSelection(targets) => {
                self.selected_notes = targets;
            }
            AppEvent::DeleteSelectedNotes => self.delete_selected_notes(),
            AppEvent::SetSelectedNoteLyric(text) => {
                self.set_selected_note_lyric(text);
            }
            AppEvent::OpenPluginPickerFor(target) => {
                self.plugin_picker_target = target;
                self.refresh_picker_visible();
                self.is_plugin_picker_open = true;
            }
            AppEvent::ClosePluginPicker => {
                self.is_plugin_picker_open = false;
            }
            AppEvent::RescanPluginDb => {
                self.begin_rescan();
            }
            AppEvent::PluginDbRescanCompleted => {
                self.finish_rescan();
            }
            AppEvent::SetArrangeScroll(scroll) => {
                self.arrange_scroll_beat = scroll.max(0.0);
            }
            AppEvent::SetArrangeTrackRowH(h) => {
                self.arrange_track_row_h = h.clamp(16.0, 96.0);
            }
            AppEvent::SetArrangeZoom(zoom) => {
                self.arrange_zoom_x = zoom.clamp(2.0, 400.0);
            }
            AppEvent::SetPianoRollScrollX(scroll) => {
                self.pianoroll_scroll_beat = scroll.max(0.0);
            }
            AppEvent::SetPianoRollTopPitch(p) => {
                self.pianoroll_top_pitch = p.clamp(11, 127);
            }
            AppEvent::SetPianoRollZoomX(zoom) => {
                self.pianoroll_zoom_x = zoom.clamp(8.0, 400.0);
            }
            AppEvent::SetPianoRollZoomY(zoom) => {
                self.pianoroll_zoom_y = zoom.clamp(6.0, 40.0);
            }
            AppEvent::SetLoopRange { start, end } => {
                self.set_loop_range(start, end);
            }
            AppEvent::SelectPluginFromDb(id) => {
                self.select_plugin_from_db(id);
            }
            AppEvent::ToggleSlotGui { slot_kind, slot_index } => {
                self.toggle_slot_gui(slot_kind, slot_index);
            }
            AppEvent::RemoveSlot { slot_kind, slot_index } => {
                self.remove_slot(slot_kind, slot_index);
            }
            AppEvent::ReorderInspectorChain(order) => {
                self.reorder_inspector_chain(&order);
            }
            AppEvent::SetMasterGain(amp) => {
                self.set_master_gain(amp);
            }
            AppEvent::Tick { samples, peak_l, peak_r } => {
                self.on_tick(samples, peak_l, peak_r);
            }
            AppEvent::GuiOpenedFromChild { track, slot, width, height } => {
                self.on_gui_opened(track, slot, width, height);
            }
            AppEvent::GuiRequestResizeFromChild { track, slot, width, height } => {
                self.on_gui_request_resize(track, slot, width, height);
            }
            AppEvent::GuiClosedFromChild { track, slot } => {
                self.on_gui_closed(track, slot);
            }
            AppEvent::SlotPluginLoadedFromChild { track, slot, id, name } => {
                self.on_plugin_loaded_from_child(track, slot, id, name);
            }
            AppEvent::AllStatesReceived(entries) => {
                self.on_all_states_from_child(entries);
            }
            AppEvent::SetTrackVolume { track, amp } => {
                self.set_track_volume(track, amp);
            }
            AppEvent::SetTrackPan { track, pan } => {
                self.set_track_pan(track, pan);
            }
            AppEvent::ToggleTrackMute(track) => {
                self.toggle_track_mute(track);
            }
            AppEvent::ToggleTrackSolo(track) => {
                self.toggle_track_solo(track);
            }
            AppEvent::TrackPeaksTick(peaks) => {
                self.on_track_peaks_tick(&peaks);
            }
            AppEvent::ExportWav => {
                self.action_export_wav();
            }
            AppEvent::ExportWavComplete { error } => {
                // Either way, hand the plugins back to realtime mode
                // (we set Offline before triggering the export).
                self.send_plugin(MainToChild::SetRenderMode(
                    common::protocol::RenderMode::Realtime,
                ));
                if let Some(err) = error {
                    self.status_message = format!("WAV 書き出し失敗: {err}");
                } else {
                    self.status_message = "WAV 書き出し完了".to_string();
                }
            }
            AppEvent::SynthesizeVocal => {
                self.status_message = "VOICEVOX 合成中...".to_string();
                self.begin_vocal_synth();
            }
            AppEvent::VocalSynthCompleted => {
                self.finish_vocal_synth();
            }
        }
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

    fn sync_song_to_plugin_host(&mut self) {
        self.is_dirty = true;
        if self.is_dragging {
            return;
        }
        let song = self.song.clone();
        self.send_audio(MainToChild::LoadSong(song));
    }

    // -------- File ----------------------------------------------------------

    fn action_new(&mut self) {
        self.song = Song::default();
        self.file_path = None;
        self.selected_track = 0;
        self.selected_clip = None;
        self.selected_notes.clear();
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

    fn action_open_path(&mut self, path: PathBuf) {
        match common::project::load(&path) {
            Ok(mut song) => {
                tracing::info!(path = %path.display(), "loaded project");
                song.ensure_ids();
                self.restore_plugin_from_song(&song);
                self.song = song;
                self.file_path = Some(path.clone());
                self.selected_track = 0;
                self.selected_clip = None;
                self.selected_notes.clear();
                self.resize_track_peak_display();
                self.sync_song_to_plugin_host();
                self.is_dirty = false;
                self.push_recent(path);
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project");
                self.status_message = format!("Open 失敗: {e:#}");
            }
        }
    }

    fn push_recent(&mut self, path: PathBuf) {
        self.recent_files.push(path);
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

    fn maybe_autosave(&mut self) {
        if !self.is_dirty {
            return;
        }
        let Some(orig) = self.file_path.as_ref() else {
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
        let result = common::project::save(&autosave_path, &self.song);
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
        for s in &states {
            let Some(track) = self.song.tracks.get_mut(s.track as usize) else {
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
        if self.save_to(&path) {
            self.file_path = Some(path);
        }
    }

    fn save_to(&mut self, path: &Path) -> bool {
        match common::project::save(path, &self.song) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "saved project");
                self.is_dirty = false;
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
        let song = self.song.clone();
        self.send_audio(MainToChild::LoadSong(song));
        self.send_audio(MainToChild::Play);
        self.is_playing = true;
    }

    fn stop(&mut self) {
        self.send_audio(MainToChild::Stop);
        self.is_playing = false;
        self.playhead_beat = None;
    }

    fn toggle_loop(&mut self) {
        self.is_looping = !self.is_looping;
        self.send_audio(MainToChild::SetLoop(self.is_looping));
    }

    fn set_loop_range(&mut self, start: f64, end: f64) {
        let (start, end) = if end > start {
            (start.max(0.0), end.max(0.0))
        } else {
            (0.0, 0.0)
        };
        self.song.loop_start_beat = start;
        self.song.loop_end_beat = end;
        self.sync_song_to_plugin_host();
    }

    // -------- Track operations ---------------------------------------------

    fn delete_track(&mut self, idx: u32) {
        if idx as usize >= self.song.tracks.len() {
            return;
        }
        self.push_undo_snapshot();
        #[cfg(windows)]
        {
            self.plugin_host_windows.retain(|&(t, _), _| t != idx);
        }
        self.song.tracks.remove(idx as usize);
        self.send_plugin(MainToChild::RemoveTrack { track: idx });
        if let Some(r) = self.selected_clip {
            if r.track == idx {
                self.selected_clip = None;
                self.selected_notes.clear();
            } else if r.track > idx {
                self.selected_clip = Some(ClipRef {
                    track: r.track - 1,
                    clip: r.clip,
                });
            }
        }
        let new_track = if self.selected_track == idx {
            idx.saturating_sub(1)
        } else if self.selected_track > idx {
            self.selected_track - 1
        } else {
            self.selected_track
        };
        let max = self.song.tracks.len().saturating_sub(1) as u32;
        self.selected_track = new_track.min(max);
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    fn swap_tracks(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        let n = self.song.tracks.len() as u32;
        if a >= n || b >= n {
            return;
        }
        self.push_undo_snapshot();
        self.song.tracks.swap(a as usize, b as usize);
        self.send_plugin(MainToChild::SwapTracks { a, b });
        if let Some(r) = self.selected_clip {
            self.selected_clip = Some(ClipRef {
                track: if r.track == a {
                    b
                } else if r.track == b {
                    a
                } else {
                    r.track
                },
                clip: r.clip,
            });
        }
        if self.selected_track == a {
            self.selected_track = b;
        } else if self.selected_track == b {
            self.selected_track = a;
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    /// Drag&drop reorder。`order` は新順での `Track.id` 列。order に含まれない
    /// track は末尾に残す (gui_01 daw_prototype の流儀に合わせ防御的)。
    fn reorder_tracks(&mut self, order: &[u32]) {
        if order.is_empty() {
            return;
        }
        // 並びが変化しない場合は no-op
        let same = order.iter().enumerate().all(|(i, id)| {
            self.song.tracks.get(i).map(|t| t.id) == Some(*id)
        });
        if same && order.len() == self.song.tracks.len() {
            return;
        }
        self.push_undo_snapshot();
        let selected_track_id = self
            .song
            .tracks
            .get(self.selected_track as usize)
            .map(|t| t.id);
        let selected_clip_keys: Vec<(u32, u32)> = self
            .selected_clips
            .iter()
            .filter_map(|r| {
                let t = self.song.tracks.get(r.track as usize)?;
                let c = t.clips.get(r.clip as usize)?;
                Some((t.id, c.id))
            })
            .collect();

        // 元順序での index 列を計算 (`order[i]` の id を持つ track の旧 index)。
        // この `index_order` を `MainToChild::ReorderTracks` で 1 度送り、
        // plugin host 側で 1 回の `tracks.mutate` (= 1 回の audio thread stop/start)
        // で chains / params / vocal を新順序に並び替える。
        let index_order: Vec<u32> = order
            .iter()
            .filter_map(|id| {
                self.song
                    .tracks
                    .iter()
                    .position(|t| t.id == *id)
                    .map(|p| p as u32)
            })
            .collect();

        // song.tracks を新順序に並び替え (= 表示モデル更新)。
        let mut new_tracks = Vec::with_capacity(self.song.tracks.len());
        for id in order {
            if let Some(pos) = self.song.tracks.iter().position(|t| t.id == *id) {
                new_tracks.push(self.song.tracks.remove(pos));
            }
        }
        new_tracks.append(&mut self.song.tracks);
        self.song.tracks = new_tracks;

        // selection を id で復元
        if let Some(id) = selected_track_id
            && let Some(idx) = self.song.tracks.iter().position(|t| t.id == id)
        {
            self.selected_track = idx as u32;
        }
        let new_clips: Vec<ClipRef> = selected_clip_keys
            .iter()
            .filter_map(|(tid, cid)| {
                let t_idx = self.song.tracks.iter().position(|t| t.id == *tid)?;
                let c_idx = self.song.tracks[t_idx]
                    .clips
                    .iter()
                    .position(|c| c.id == *cid)?;
                Some(ClipRef { track: t_idx as u32, clip: c_idx as u32 })
            })
            .collect();
        self.selected_clips = new_clips.clone();
        self.selected_clip = new_clips.first().copied();

        // chains / params / vocal を 1 回の `tracks.mutate` で並び替え (ReorderTracks)。
        // 続けて `LoadSong` で `song_store` も新順序に同期 (LoadSong は params 更新と
        // song の atomic swap だけなので audio thread を止めない)。両方送らないと
        // chains は新順序、song_store は旧順序のまま → clip が違う track の chain で
        // 再生されて「クリップが入れかわる」現象が出る。
        self.send_plugin(MainToChild::ReorderTracks(index_order));
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    fn select_track(&mut self, idx: u32) {
        let n = self.song.tracks.len() as u32;
        if idx >= n {
            return;
        }
        if self.selected_track != idx {
            self.selected_track = idx;
        }
    }

    fn begin_rename_track(&mut self, idx: u32) {
        let Some(name) = self.song.tracks.get(idx as usize).map(|t| t.name.clone()) else {
            return;
        };
        self.track_rename_text = name;
        self.track_rename_idx = Some(idx);
    }

    fn commit_rename_track(&mut self) {
        let Some(idx) = self.track_rename_idx else {
            return;
        };
        self.track_rename_idx = None;
        let new_name = self.track_rename_text.trim().to_string();
        self.track_rename_text.clear();
        if new_name.is_empty() {
            return;
        }
        if let Some(track) = self.song.tracks.get_mut(idx as usize) {
            track.name = new_name;
        }
        self.sync_song_to_plugin_host();
    }

    fn ensure_first_track(&mut self) {
        if self.song.tracks.is_empty() {
            let id = self.song.alloc_track_id();
            self.song.tracks.push(Track {
                id,
                name: "Track 1".into(),
                ..Track::default()
            });
            self.resize_track_peak_display();
        }
    }

    fn action_add_vocal_track(&mut self) {
        let id = self.song.alloc_track_id();
        let index = self.song.tracks.len() + 1;
        let mut track = Track {
            id,
            name: format!("Track {index}"),
            source: InstrumentSource::Vocal {
                speaker_id: common::voicevox::DEFAULT_SINGER_ID,
                style_name: "ノーマル".into(),
            },
            ..Track::default()
        };
        let mut clip = demo_clip();
        clip.id = track.alloc_clip_id();
        track.clips.push(clip);
        self.song.tracks.push(track);
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(index, "added vocal track");
    }

    fn action_add_instrument_track(&mut self) {
        let id = self.song.alloc_track_id();
        let index = self.song.tracks.len() + 1;
        let track = Track {
            id,
            name: format!("Track {index}"),
            source: InstrumentSource::None,
            clips: Vec::new(),
            ..Track::default()
        };
        self.song.tracks.push(track);
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(index, "added instrument track");
    }

    fn action_remove_last_track(&mut self) {
        let len = self.song.tracks.len();
        if len == 0 {
            return;
        }
        let removed_idx = (len - 1) as u32;
        let removed_name = self.song.tracks.pop().map(|t| t.name);
        if let Some(name) = removed_name {
            tracing::info!(
                index = removed_idx,
                name = %name,
                "removed last track"
            );
        }
        #[cfg(windows)]
        {
            self.plugin_host_windows
                .retain(|&(t, _), _| t != removed_idx);
        }
        self.send_plugin(MainToChild::RemoveTrack { track: removed_idx });
        let new_max = self.song.tracks.len().saturating_sub(1) as u32;
        let is_empty = self.song.tracks.is_empty();
        if is_empty {
            self.selected_track = 0;
        } else if self.selected_track > new_max {
            self.selected_track = new_max;
        }
        self.selected_clips.retain(|c| c.track != removed_idx);
        if let Some(r) = self.selected_clip
            && r.track == removed_idx
        {
            self.selected_clip = self.selected_clips.last().copied();
            self.selected_notes.clear();
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    // -------- Clip / note / midi -------------------------------------------

    fn handle_midi_note_on(&mut self, pitch: u8, velocity: u8) {
        let Some(target) = self.selected_clip else {
            return;
        };
        let cursor = self.step_cursor_beat;
        let step = self.step_size_beats;
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
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
        let next_cursor = cursor + step;
        self.selected_notes = vec![new_idx];
        self.step_cursor_beat = next_cursor;
        self.sync_song_to_plugin_host();
    }

    fn select_clip(&mut self, target: ClipRef, additive: bool) {
        let mut clips = self.selected_clips.clone();
        if additive {
            if let Some(pos) = clips.iter().position(|c| *c == target) {
                clips.remove(pos);
            } else {
                clips.push(target);
            }
        } else {
            clips = vec![target];
        }
        let primary = clips.last().copied();
        self.selected_clips = clips;
        self.selected_clip = primary;
        self.selected_notes.clear();
        self.step_cursor_beat = 0.0;
        if let Some(r) = primary {
            self.select_track(r.track);
        }
    }

    fn set_clip_selection(&mut self, targets: Vec<ClipRef>) {
        let primary = targets.last().copied();
        self.selected_clips = targets;
        self.selected_clip = primary;
        self.selected_notes.clear();
        self.step_cursor_beat = 0.0;
        if let Some(r) = primary {
            self.select_track(r.track);
        }
    }

    fn set_clip_positions(&mut self, entries: &[(ClipRef, f64)]) {
        for (target, beat) in entries {
            let new_start = beat.max(0.0);
            if let Some(track) = self.song.tracks.get_mut(target.track as usize)
                && let Some(clip) = track.clips.get_mut(target.clip as usize)
            {
                clip.start_beat = new_start;
            }
        }
        self.sync_song_to_plugin_host();
    }

    fn resize_clip(&mut self, target: ClipRef, new_length_beats: f64) {
        let new_length_beats = new_length_beats.max(0.0625);
        if let Some(track) = self.song.tracks.get_mut(target.track as usize)
            && let Some(clip) = track.clips.get_mut(target.clip as usize)
        {
            clip.length_beats = new_length_beats;
        }
        self.sync_song_to_plugin_host();
    }

    fn create_clip(&mut self, track_idx: u32, start_beat: f64) {
        let start_beat = start_beat.max(0.0);
        let Some(track) = self.song.tracks.get_mut(track_idx as usize) else {
            return;
        };
        let new_clip_id = track.alloc_clip_id();
        let new_idx = track.clips.len() as u32;
        let clip_no = track.clips.len() + 1;
        track.clips.push(Clip {
            id: new_clip_id,
            name: format!("Clip {clip_no}"),
            start_beat,
            length_beats: DEFAULT_CLIP_LENGTH,
            notes: Vec::new(),
        });
        let r = ClipRef {
            track: track_idx,
            clip: new_idx,
        };
        self.selected_clip = Some(r);
        self.selected_clips = vec![r];
        self.selected_notes.clear();
        self.select_track(track_idx);
        self.sync_song_to_plugin_host();
    }

    fn delete_selected_clip(&mut self) {
        if self.selected_clips.is_empty() {
            return;
        }
        let mut targets = std::mem::take(&mut self.selected_clips);
        targets.sort_by(|a, b| a.track.cmp(&b.track).then(b.clip.cmp(&a.clip)));
        for target in &targets {
            if let Some(track) = self.song.tracks.get_mut(target.track as usize)
                && (target.clip as usize) < track.clips.len()
            {
                track.clips.remove(target.clip as usize);
            }
        }
        self.selected_clip = None;
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
        let r = ClipRef {
            track: track_idx,
            clip: clip_idx,
        };
        self.selected_clip = Some(r);
        if !self.selected_clips.contains(&r) {
            self.selected_clips = vec![r];
        }
        self.selected_notes = vec![new_idx];
        self.sync_song_to_plugin_host();
        self.pianoroll_notes_generation += 1;
    }

    fn set_note_positions(&mut self, entries: &[(u32, f64, u8)]) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
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
        self.sync_song_to_plugin_host();
        self.pianoroll_notes_generation += 1;
    }

    fn resize_notes(&mut self, entries: &[(u32, f64, f64)]) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(r.clip as usize) else {
            return;
        };
        for &(idx, start, duration) in entries {
            let Some(note) = clip.notes.get_mut(idx as usize) else {
                continue;
            };
            note.start_beat = start.max(0.0);
            note.duration_beats = duration.max(0.0625);
        }
        self.sync_song_to_plugin_host();
        self.pianoroll_notes_generation += 1;
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
        self.pianoroll_notes_generation += 1;
    }

    fn delete_selected_notes(&mut self) {
        let Some(r) = self.selected_clip else {
            return;
        };
        if self.selected_notes.is_empty() {
            return;
        }
        let mut indices = std::mem::take(&mut self.selected_notes);
        indices.sort_unstable_by(|a, b| b.cmp(a));
        if let Some(track) = self.song.tracks.get_mut(r.track as usize)
            && let Some(clip) = track.clips.get_mut(r.clip as usize)
        {
            for i in &indices {
                let i = *i as usize;
                if i < clip.notes.len() {
                    clip.notes.remove(i);
                }
            }
        }
        self.sync_song_to_plugin_host();
        self.pianoroll_notes_generation += 1;
    }

    fn set_selected_note_lyric(&mut self, lyric: String) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let trimmed = lyric.trim();
        let value = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        let selected = self.selected_notes.clone();
        let Some(track) = self.song.tracks.get_mut(r.track as usize) else {
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
        self.sync_song_to_plugin_host();
        self.pianoroll_notes_generation += 1;
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

    /// inspector chain (MIDI FX → Instrument → FX を一列で表示) の reorder。
    /// `order[i]` は新位置 i に来る旧 chain index。section 跨ぎ (slot_kind が
    /// 変わる移動) は無視 (DAW 慣習: signal flow 順序の意味が壊れる)。
    fn reorder_inspector_chain(&mut self, order: &[usize]) {
        let chain = self.inspector_chain();
        if chain.len() != order.len() {
            return;
        }
        // section 整合性チェック: 各位置の slot_kind が変わらないこと
        let same_section = order
            .iter()
            .enumerate()
            .all(|(new_i, &old_i)| {
                chain.get(old_i).map(|e| e.slot_kind)
                    == chain.get(new_i).map(|e| e.slot_kind)
            });
        if !same_section {
            return;
        }
        let track_idx = self.selected_track;
        let Some(track) = self.song.tracks.get_mut(track_idx as usize) else {
            return;
        };
        // chain の構成: [midi_fx_chain..., (instrument), fx_chain...]
        let midi_count = track.midi_fx_chain.len();
        let inst_count = usize::from(track.instrument.is_some());
        let fx_start = midi_count + inst_count;

        // MIDI FX section を新順序で並び替え
        if midi_count > 0 {
            let new_midi: Vec<_> = (0..midi_count)
                .map(|new_i| track.midi_fx_chain[order[new_i]].clone())
                .collect();
            track.midi_fx_chain = new_midi;
        }
        // FX section を新順序で並び替え
        let fx_count = track.fx_chain.len();
        if fx_count > 0 {
            let new_fx: Vec<_> = (0..fx_count)
                .map(|new_i| {
                    let chain_new_i = fx_start + new_i;
                    let chain_old_i = order[chain_new_i];
                    let fx_old_i = chain_old_i - fx_start;
                    track.fx_chain[fx_old_i].clone()
                })
                .collect();
            track.fx_chain = new_fx;
        }
        self.sync_song_to_plugin_host();
    }

    fn remove_slot(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        let track_idx = self.selected_track;
        self.send_plugin(MainToChild::RemoveSlotPlugin {
            track: track_idx,
            slot,
        });
        if let Some(track) = self.song.tracks.get_mut(track_idx as usize) {
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
    }

    fn on_all_states_from_child(&mut self, states: Vec<SlotState>) {
        let Some(path) = self.pending_save_path.take() else {
            return;
        };
        self.finish_save(path, states);
    }

    // -------- Tick / metering ----------------------------------------------

    fn on_tick(&mut self, playhead_samples: u64, peak_l_raw: f32, peak_r_raw: f32) {
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

        const RELEASE: f32 = 0.85;
        let new_l = common::meter::update_peak(self.peak_l_display, peak_l_raw, RELEASE);
        let new_r = common::meter::update_peak(self.peak_r_display, peak_r_raw, RELEASE);
        self.peak_l_display = new_l;
        self.peak_r_display = new_r;
        self.peak_l_norm = common::meter::db_to_norm(common::meter::linear_to_db(new_l));
        self.peak_r_norm = common::meter::db_to_norm(common::meter::linear_to_db(new_r));
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
    }

    // -------- VOICEVOX -----------------------------------------------------

    fn begin_vocal_synth(&self) {
        let song = self.song.clone();
        let slot = Arc::clone(&self.synth_result);
        let proxy = self.event_proxy.clone();
        std::thread::spawn(move || {
            let results = common::voicevox::synthesize_song(
                &song,
                common::voicevox::DEFAULT_SINGER_ID,
                common::voicevox::DEFAULT_SINGER_ID,
            );
            if let Ok(mut guard) = slot.lock() {
                *guard = results;
            }
            let _ = proxy.send_event(AppEvent::VocalSynthCompleted);
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
            self.status_message = msg;
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
        self.status_message = msg;

        let song_snapshot = self.song.clone();
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

            self.send_audio(MainToChild::SetVocalAudio {
                track: r.track,
                clip: r.clip,
                clip_start_samples,
                sample_rate: r.sample_rate,
                samples: r.samples.clone(),
            });
        }
    }

    // -------- Plugin DB rescan --------------------------------------------

    fn begin_rescan(&mut self) {
        if self.is_rescanning {
            return;
        }
        self.is_rescanning = true;
        let slot = Arc::clone(&self.rescan_result);
        let proxy = self.event_proxy.clone();
        std::thread::spawn(move || match common::plugin_db::scan_system() {
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
                let _ = proxy.send_event(AppEvent::PluginDbRescanCompleted);
            }
            Err(e) => {
                tracing::error!(error = ?e, "plugin rescan failed");
                let _ = proxy.send_event(AppEvent::PluginDbRescanCompleted);
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
        let msg = MainToChild::SetTrackVolume { track, volume: v };
        self.send_audio(msg);
    }

    fn set_track_pan(&mut self, track: u32, pan: f32) {
        let p = pan.clamp(-1.0, 1.0);
        if let Some(t) = self.song.tracks.get_mut(track as usize) {
            t.pan = p;
        }
        let msg = MainToChild::SetTrackPan { track, pan: p };
        self.send_audio(msg);
    }

    fn toggle_track_mute(&mut self, track: u32) {
        let Some(t) = self.song.tracks.get_mut(track as usize) else {
            return;
        };
        t.muted = !t.muted;
        let muted = t.muted;
        let msg = MainToChild::SetTrackMuted { track, muted };
        self.send_audio(msg);
    }

    fn toggle_track_solo(&mut self, track: u32) {
        let Some(t) = self.song.tracks.get_mut(track as usize) else {
            return;
        };
        t.solo = !t.solo;
        let solo = t.solo;
        let msg = MainToChild::SetTrackSolo { track, solo };
        self.send_audio(msg);
    }

    fn on_track_peaks_tick(&mut self, peaks: &[(f32, f32)]) {
        const RELEASE: f32 = 0.85;
        let n = self.song.tracks.len();
        if self.track_peak_display.len() != n {
            self.track_peak_display.resize(n, (0.0, 0.0));
        }
        for (i, d) in self.track_peak_display.iter_mut().enumerate() {
            let (l, r) = peaks.get(i).copied().unwrap_or((0.0, 0.0));
            d.0 = common::meter::update_peak(d.0, l, RELEASE);
            d.1 = common::meter::update_peak(d.1, r, RELEASE);
        }
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
        let visible: Vec<PluginPickEntry> = self
            .plugin_picker_entries
            .iter()
            .filter(|e| e.features.iter().any(|f| f == feature_key))
            .cloned()
            .collect();
        self.plugin_picker_visible = visible;
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

    /// File → Export WAV...:
    ///   1. Pick a destination via the OS file dialog.
    ///   2. Tell the plugin host to switch every plugin to
    ///      `CLAP_RENDER_OFFLINE` so plugins offering the `clap.render`
    ///      extension can use higher-quality algorithms.
    ///   3. Send `ExportWav { path }` to daw_audio, which freewheels
    ///      the song through the existing AudioWorker pool while the
    ///      CPAL callback writes silence.
    ///   4. On `ExportWavComplete` the handler flips render mode back
    ///      to Realtime (see `AppEvent::ExportWavComplete` arm).
    fn action_export_wav(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV", &["wav"])
            .save_file()
        else {
            return;
        };
        self.status_message = "WAV 書き出し中...".to_string();
        // Make sure daw_audio has the latest song snapshot before the
        // freewheel run starts.
        let song = self.song.clone();
        self.send_audio(MainToChild::LoadSong(song));
        self.send_plugin(MainToChild::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        self.send_audio(MainToChild::ExportWav { path });
    }
}

// ---------------------------------------------------------------------------
// Free standing helpers
// ---------------------------------------------------------------------------

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
        id: 1,
        name: "こんにちわ".into(),
        start_beat: 0.0,
        length_beats: 4.0,
        notes,
    }
}
