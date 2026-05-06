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

use crate::dispatcher::{BackgroundDispatcher, JobDispatcher};

/// `plan_track_removal_ipc` の出力。 順序が deadlock 防止に必須なので
/// テスト可能な enum で表現する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackRemovalIpc {
    /// daw_audio engine に `MainToChild::ClosePluginShmem { plugin_id }`
    /// を送る (use-after-free deadlock 防止のため RemoveTrack より先)。
    CloseAudioShmem { plugin_id: u32 },
    /// daw_plugin_host に `MainToChild::RemoveTrack { track }` を送る
    /// (plugin chain の proper teardown)。
    RemoveTrackFromPluginHost { track_id: u32 },
}

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
    /// `kind == Group` のとき mixer strip / arrangement で別色表示し、
    /// 子トラックを束ねる sub-mix bus として識別する。
    pub is_group: bool,
    /// このトラックの depth (parent_group_id を辿った段数)。 0 = master 直下、
    /// 1 = 1 段ネスト、… mixer strip / arrangement view が階層インデント描画に使う。
    pub depth: u8,
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
            is_group: false,
            depth: 0,
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

/// Per-plugin sidechain wiring entry shown in the inspector. One row per
/// chain plugin (MIDI FX / Instrument / Fx); the `current_source` field
/// is the value of `PluginInstance::sidechain_sources[0]` (port 0; the
/// inspector only exposes the first aux input port for now).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidechainEntry {
    pub track_id: u32,
    pub slot_kind: u8,
    pub slot_index: u32,
    pub plugin_name: String,
    pub current_source: Option<u32>,
}

/// Sidechain source picker choice: `None` = "—" (disconnected),
/// `Some(track_id)` = a specific track. Self-track is filtered out by
/// the picker because feeding a track its own output into a sidechain
/// creates a feedback loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidechainSourceChoice {
    pub label: String,
    pub track_id: Option<u32>,
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
    /// Track multi-selection (Ableton Live / Reaper 互換)。 末尾要素 =
    /// 「最後にクリックした anchor」 = カーソル相当。 widget 側 (gui_01
    /// arrangement) からは `selected_tracks: &[u32]` として渡す。 id
    /// ベース (Track::id) で持ち、 track 並び替えでも安定。
    pub selected_track_ids: Vec<u32>,
    /// 折り畳み中の group track id 集合。 group 自身が `kind == Group`
    /// (= 子を持つ) かつこの set に含まれていれば子孫の row を hide。
    pub collapsed_groups: std::collections::HashSet<u32>,
    /// `track_id → 現在ロード済の plugin_id 列`。 plugin_host から
    /// `SlotPluginLoaded` を受信したときに register、 `SlotPluginUnloaded`
    /// で drain。 `RemoveTrack` を plugin_host に送る前に audio engine
    /// に直接 `ClosePluginShmem` を発射して plugin_refs / slot_to_plugin_id
    /// を空にし、 plugin destroy 中の use-after-free (`pd.prepare()` で
    /// unmapped shmem を踏む → audio worker が AV で silent terminate
    /// → all_done 永久 wait) を防ぐ。 daw_gui が plugin_id を保持する
    /// ための単一 source of truth。
    pub track_plugin_ids: std::collections::HashMap<u32, Vec<u32>>,
    /// PR3.3 PDC: `plugin_id → reported latency samples`。 plugin_host から
    /// `ChildToMain::PluginLatencyChanged` を受信して更新、
    /// `SlotPluginUnloaded` で drop。 各 track の累積 latency は
    /// `track_plugin_ids[track_id].iter().map(|pid| plugin_latencies[pid]).sum()`
    /// で計算して `Track::reported_latency_samples` に書く。 これが
    /// `LoadSong` で daw_audio に渡って `compile_schedule` の PDC 補償に
    /// 反映される (chain 内の plugin が直列に latency を加算する Ardour 流)。
    pub plugin_latencies: std::collections::HashMap<u32, u32>,
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
    /// FL Studio の smart length 互換: 直近に作成 / リサイズ / クリック選択した
    /// ノートの長さ (拍)。次の新規追加時のデフォルト長として使う。session 内
    /// in-memory のみ、永続化はしない。`add_note` / `resize_notes` /
    /// `SetNoteSelection` ハンドラで更新。
    pub last_note_duration_beats: f64,

    // -------- Grid snap state --------
    /// piano_roll の Snap on/off (Snap toggle / `G` キー)。
    pub pianoroll_snap_enabled: bool,
    /// `view::snap::SNAP_LABELS` の index。`view::snap::choice_to_mode` で SnapMode に変換。
    pub pianoroll_snap_choice: u8,
    pub arrange_snap_enabled: bool,
    pub arrange_snap_choice: u8,
    /// auto-fit (`X` キー / `Fit` ボタン / SelectClip 経由) で参照する piano_roll
    /// grid 領域サイズ (px)。`view::root` / `view::bottom_panel` が piano_roll タブ
    /// 描画時に毎フレーム書き込む。0 は「未測定」フラグ扱い (auto-fit を skip)。
    pub last_pianoroll_grid_size: (f32, f32),
    /// 同様に arrangement の lanes 領域サイズ (px)。
    pub last_arrange_canvas_size: (f32, f32),

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

    // -------- Plugin load tracking (A7 race-condition fix) -----------
    /// `(track, slot)` pairs we've sent `SetSlotPlugin` for but haven't
    /// yet received `SlotPluginLoaded` back. While non-empty, Play is
    /// queued so the audio engine doesn't dispatch silent buffers for
    /// tracks whose plugins are still being loaded.
    pub pending_plugin_loads: std::collections::HashSet<(u32, PluginSlot)>,
    /// `play()` was called while `pending_plugin_loads` was non-empty;
    /// re-fire it once the last `SlotPluginLoaded` arrives.
    pub pending_play: bool,

    // -------- Background workers --------
    pub synth_result: Arc<Mutex<Vec<common::voicevox::SynthResult>>>,
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    /// VOICEVOX engine `/singers` の結果。 起動時に background thread が
    /// `AppEvent::SingersLoaded` で投入する。 engine 未起動 / fetch 失敗時は
    /// 空のまま (Track Inspector の dropdown は default singer のみ表示)。
    pub singers: Vec<common::voicevox::VoiceVoxSinger>,
    /// VOICEVOX 合成結果 in-memory cache (process lifetime のみ)。 Synth ボタン
    /// 押下時に各 clip の content_hash + singer_id を key に lookup → hit なら
    /// HTTP call をスキップ。 永続化は将来 Phase。
    pub voicevox_cache: Arc<Mutex<common::voicevox_cache::VoiceVoxCache>>,
    /// VOICEVOX engine の auto-kill 用 Job dispatcher。
    /// production は `Win32JobDispatcher` (`JobHandle::assign_std` ラップ)、
    /// test は `NoopJobDispatcher`。 trait DI により AppData::new の
    /// 引数は OS-API 抽象だけで完結する。
    pub voicevox_job: Arc<dyn JobDispatcher>,
    /// VOICEVOX engine 起動を 1 度だけ trigger するためのフラグ。 lazy 起動:
    /// 起動時 auto-launch せず、 Vocal track 選択 / Synth ボタン押下等で初めて
    /// `ensure_voicevox_engine()` が `true` にして background spawn する。
    pub voicevox_launch_attempted: bool,
    pub is_rescanning: bool,
    pub status_message: String,

    pub track_rename_idx: Option<u32>,
    pub track_rename_text: String,

    /// Transport BPM 入力欄の編集中文字列。 commit (Enter) で parse + clamp +
    /// `song.bpm` に反映、 song を切り替える際 (open / new / undo / redo) は
    /// `resync_song_edit_texts` で formatted な現値に書き戻す。
    pub bpm_edit_text: String,
    /// Transport time_sig numerator 入力欄の編集中文字列。 同上。
    pub time_sig_num_edit_text: String,

    pub undo_stack: VecDeque<Song>,
    pub redo_stack: VecDeque<Song>,

    pub is_help_open: bool,

    pub recent_files: common::recent::RecentFiles,

    pub is_dirty: bool,
    pub last_autosave: std::time::Instant,
    /// Crash-recovery session id (uuid v4)。 起動時に AppData::new で 1 回生成、
    /// 未保存プロジェクトの autosave file 名 (`<id>.autosave.daw`) と
    /// `on_shutdown` での cleanup target に使う。
    pub recovery_session_id: String,
    /// 起動時 recovery_dir scan + Open 時 sidecar 検出で蓄積される復元候補。
    /// `recovery_modal` が空でない間 modal を出す。
    pub recovery_candidates: Vec<PathBuf>,
    /// `recovery_candidates` を modal に出すかどうか (Dismiss で false)。
    pub show_recovery_modal: bool,
    pub is_dragging: bool,
    pub midi_input_label: String,

    pub step_cursor_beat: f64,
    pub step_size_beats: f64,

    /// 背景スレッド (autosave / playhead poll / MIDI / IPC bridge / VOICEVOX
    /// 合成 / plugin DB rescan) からメインスレッドへ `AppEvent` を送るための
    /// dispatcher。 production は `WinitDispatcher` (winit `EventLoopProxy`
    /// ラップ)、 test は `RecordingDispatcher` (Mutex<Vec> に蓄積)。
    pub event_proxy: Arc<dyn BackgroundDispatcher>,
}

impl AppData {
    pub fn new(
        audio_tx: UnboundedSender<MainToChild>,
        plugin_tx: UnboundedSender<MainToChild>,
        // 将来的な auto-select 用に予約。現在は song に反映していない。
        _clap_plugin_path: Option<PathBuf>,
        plugin_db: Option<Arc<PluginDatabase>>,
        event_proxy: Arc<dyn BackgroundDispatcher>,
        voicevox_job: Arc<dyn JobDispatcher>,
    ) -> Self {
        let song = Song {
            tracks: vec![Track {
                name: "Track 1".into(),
                ..Track::default()
            }],
            ..Song::default()
        };
        let initial_peak_display = vec![(0.0, 0.0); song.tracks.len()];
        let initial_bpm = song.bpm;
        let initial_time_sig_num = song.time_sig.0;
        let recovery_candidates = common::recovery::scan_recovery_files();
        let show_recovery_modal = !recovery_candidates.is_empty();
        if show_recovery_modal {
            tracing::info!(
                count = recovery_candidates.len(),
                "recovery candidates found at startup"
            );
        }
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
            selected_track_ids: Vec::new(),
            collapsed_groups: std::collections::HashSet::new(),
            track_plugin_ids: std::collections::HashMap::new(),
            plugin_latencies: std::collections::HashMap::new(),
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
            last_note_duration_beats: DEFAULT_NOTE_DURATION,
            pianoroll_snap_enabled: true,
            pianoroll_snap_choice: crate::view::snap::CHOICE_PIANOROLL_DEFAULT,
            arrange_snap_enabled: true,
            arrange_snap_choice: crate::view::snap::CHOICE_ARRANGE_DEFAULT,
            last_pianoroll_grid_size: (0.0, 0.0),
            last_arrange_canvas_size: (0.0, 0.0),
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
            pending_plugin_loads: std::collections::HashSet::new(),
            pending_play: false,
            synth_result: Arc::new(Mutex::new(Vec::new())),
            rescan_result: Arc::new(Mutex::new(None)),
            singers: Vec::new(),
            voicevox_cache: Arc::new(Mutex::new(common::voicevox_cache::VoiceVoxCache::new())),
            voicevox_job,
            voicevox_launch_attempted: false,
            is_rescanning: false,
            status_message: String::new(),
            track_rename_idx: None,
            track_rename_text: String::new(),
            bpm_edit_text: format!("{initial_bpm:.1}"),
            time_sig_num_edit_text: initial_time_sig_num.to_string(),
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            is_help_open: false,
            recent_files: load_recent_files(),
            is_dirty: false,
            last_autosave: std::time::Instant::now(),
            recovery_session_id: common::recovery::new_session_id(),
            recovery_candidates,
            show_recovery_modal,
            is_dragging: false,
            midi_input_label: String::new(),
            step_cursor_beat: 0.0,
            step_size_beats: DEFAULT_NOTE_DURATION,
            event_proxy,
        }
    }

    // -------- Derived snapshots (毎フレーム計算; cache が必要なら view 側で持つ) -----

    /// 「カーソル相当」 = `selected_track_ids` の末尾要素。 `None` の
    /// ときは選択ゼロ (まだ何もクリックしていない / 全 track 削除直後)。
    pub fn cursor_track_id(&self) -> Option<u32> {
        self.selected_track_ids.last().copied()
    }

    /// カーソル track の `song.tracks` 内 index。 selection は id ベース
    /// なので、 track 並び替え後でも index は再評価される。
    pub fn cursor_track_index(&self) -> Option<usize> {
        let id = self.cursor_track_id()?;
        self.song.tracks.iter().position(|t| t.id == id)
    }

    /// 単一カーソル選択にする。 multi-select を使う UI 側からは
    /// `selected_track_ids = vec![id]` を直接書く方が自然なので、 これは
    /// 既存の「index で選択しなおす」 旧フローを id ベースに変換する
    /// 互換ヘルパ。 当面は呼び出し側がない (Phase 2 移行中) ので
    /// dead_code を許容。
    #[allow(dead_code)]
    pub fn set_cursor_track_index(&mut self, idx: usize) {
        if let Some(t) = self.song.tracks.get(idx) {
            self.selected_track_ids = vec![t.id];
        }
    }

    /// A track acts as a "group" iff at least one other track points
    /// at it via `parent_group_id`. The role is purely derived — there
    /// is no `Track::kind` field. SSOT (CLAUDE.md).
    pub fn is_group_track(&self, track_id: u32) -> bool {
        self.song
            .tracks
            .iter()
            .any(|t| t.parent_group_id == Some(track_id))
    }

    /// Walk a track's `parent_group_id` chain to count how many group
    /// hops sit between it and the master bus. Saturated at 32 to keep
    /// pathological cycles (which the schedule compiler also rejects)
    /// from looping forever in the GUI's derived snapshot.
    pub fn compute_track_depth(&self, track: &common::model::Track) -> u8 {
        let mut cursor = track.parent_group_id;
        let mut depth: u8 = 0;
        let mut hops = 0;
        while let Some(pid) = cursor {
            depth = depth.saturating_add(1);
            hops += 1;
            if hops > 32 {
                break;
            }
            cursor = self.song.track_by_id(pid).and_then(|t| t.parent_group_id);
        }
        depth
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
                    is_group: self.is_group_track(t.id),
                    depth: self.compute_track_depth(t),
                }
            })
            .collect()
    }

    pub fn selected_track_label(&self) -> String {
        let n_selected = self.selected_track_ids.len();
        if n_selected > 1 {
            return format!("{n_selected} tracks selected");
        }
        match self.cursor_track_index() {
            Some(idx) => self
                .song
                .tracks
                .get(idx)
                .map(|t| {
                    if t.name.is_empty() {
                        format!("Track {}", idx + 1)
                    } else {
                        t.name.clone()
                    }
                })
                .unwrap_or_else(|| format!("Track {}", idx + 1)),
            None => "(no track)".into(),
        }
    }

    /// Per-plugin sidechain wiring entries shown in the inspector. One
    /// entry per chain plugin (MidiFx / Instrument / Fx); each carries
    /// the plugin's current `sidechain_sources[0]` value (port 0; PR4
    /// only exposes the first aux input port through the inspector). The
    /// track picker UI maps `None` → "—" and `Some(track_id)` → the
    /// track's name. Self-track is filtered out by the picker because
    /// feeding a track its own output into a sidechain creates a
    /// feedback loop the schedule compiler catches with `GraphError::Cycle`.
    pub fn sidechain_entries(&self) -> Vec<SidechainEntry> {
        let Some(track) = self
            .cursor_track_index()
            .and_then(|i| self.song.tracks.get(i))
        else {
            return Vec::new();
        };
        let mut entries: Vec<SidechainEntry> = Vec::new();
        for (i, p) in track.midi_fx_chain.iter().enumerate() {
            entries.push(SidechainEntry {
                track_id: track.id,
                slot_kind: 0,
                slot_index: i as u32,
                plugin_name: resolve_plugin_name(&self.plugin_db, &p.plugin_id),
                current_source: p.sidechain_sources.first().copied().flatten(),
            });
        }
        if let Some(inst) = track.instrument.as_ref() {
            entries.push(SidechainEntry {
                track_id: track.id,
                slot_kind: 1,
                slot_index: 0,
                plugin_name: resolve_plugin_name(&self.plugin_db, &inst.plugin_id),
                current_source: inst.sidechain_sources.first().copied().flatten(),
            });
        }
        for (i, p) in track.fx_chain.iter().enumerate() {
            entries.push(SidechainEntry {
                track_id: track.id,
                slot_kind: 2,
                slot_index: i as u32,
                plugin_name: resolve_plugin_name(&self.plugin_db, &p.plugin_id),
                current_source: p.sidechain_sources.first().copied().flatten(),
            });
        }
        entries
    }

    /// Sidechain source picker choices: "—" (None) followed by every
    /// track in the song **except** the cursor track itself.
    pub fn sidechain_source_choices(&self) -> Vec<SidechainSourceChoice> {
        let cursor_id = self.cursor_track_id();
        let mut choices: Vec<SidechainSourceChoice> = Vec::with_capacity(self.song.tracks.len() + 1);
        choices.push(SidechainSourceChoice {
            label: "—".into(),
            track_id: None,
        });
        for t in &self.song.tracks {
            if Some(t.id) == cursor_id {
                continue;
            }
            choices.push(SidechainSourceChoice {
                label: format!("{} (id {})", t.name, t.id),
                track_id: Some(t.id),
            });
        }
        choices
    }

    pub fn inspector_chain(&self) -> Vec<ChainEntry> {
        let Some(idx) = self.cursor_track_index() else {
            return Vec::new();
        };
        let Some(track) = self.song.tracks.get(idx) else {
            return Vec::new();
        };
        let mut chain: Vec<ChainEntry> = Vec::new();
        // Reaper folder model: every track (group or not) can carry
        // MIDI FX / instrument / audio FX, so show all rows uniformly.
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
        // selected_track_ids: undo で track が消えていたら除外。 残りが
        // 空なら「最後の track をカーソル」 にフォールバック (UI が
        // 完全選択ゼロでフリーズしないため)。
        let live_ids: std::collections::HashSet<u32> =
            self.song.tracks.iter().map(|t| t.id).collect();
        self.selected_track_ids.retain(|id| live_ids.contains(id));
        if self.selected_track_ids.is_empty()
            && let Some(last) = self.song.tracks.last()
        {
            self.selected_track_ids.push(last.id);
        }
        // collapsed_groups も track が消えていたら除外。
        self.collapsed_groups.retain(|id| live_ids.contains(id));
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        self.resync_song_edit_texts();
        self.pianoroll_notes_generation += 1;
    }

    fn is_undoable(event: &AppEvent) -> bool {
        matches!(
            event,
            AppEvent::New
                | AppEvent::AddVocalTrack
                | AppEvent::AddInstrumentTrack
                | AppEvent::GroupSelectedTracks { .. }
                | AppEvent::UngroupTracks { .. }
                | AppEvent::SetTrackParent { .. }
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
                | AppEvent::SetNoteLyrics { .. }
                | AppEvent::SetTrackSpeaker { .. }
                | AppEvent::QuantizeSelectedNotes(_)
                | AppEvent::SelectPluginFromDb(_)
                | AppEvent::RemoveSlot { .. }
                | AppEvent::CommitBpmEdit
                | AppEvent::CommitTimeSigNumEdit
                | AppEvent::SetSongTimeSigDenominator(_)
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
    /// Transport BPM 入力欄の文字列が変わった (commit ではなく途中入力)。
    /// Undo 対象外。
    BpmEditChanged(String),
    /// BPM 入力欄で Enter (commit)。 parse + clamp(1.0..=400.0) + Song.bpm 反映 +
    /// `bpm_edit_text` を formatted な現値に書き戻す。 Undo 対象。
    CommitBpmEdit,
    /// time_sig numerator 入力欄の文字列が変わった。 Undo 対象外。
    TimeSigNumEditChanged(String),
    /// numerator 入力欄で Enter (commit)。 parse + clamp(1..=32) + 反映。 Undo 対象。
    CommitTimeSigNumEdit,
    /// time_sig denominator dropdown で選択された (2/4/8/16 のみ valid)。 Undo 対象。
    SetSongTimeSigDenominator(u8),
    Undo,
    Redo,
    PushUndoSnapshot,
    QuantizeSelectedNotes(u8),
    SetNoteVelocity { note: u32, velocity: u8 },
    AddVocalTrack,
    AddInstrumentTrack,
    /// Group the selected tracks under a fresh group track. Mirrors
    /// Ableton Live's Cmd/Ctrl+G: the existing tracks become children
    /// (their `parent_group_id` is set), and the new group is inserted
    /// just *before* the highest-positioned selected track (= 一番上の
    /// 選択 track の直前 / 子の上にヘッダー)。 `track_ids` must be
    /// non-empty — Live forbids empty groups and so do we. If a
    /// selected track is itself a group (= already has children), it
    /// ends up nested under the new group (Live behaviour, depth
    /// unbounded).
    GroupSelectedTracks {
        track_ids: Vec<u32>,
    },
    /// Ungroup the selected group tracks. Children are reparented to
    /// the group's own parent (master or upper group), then the group
    /// track itself is removed. The group's `fx_chain` is lost
    /// (Ableton Live convention). Non-group tracks in the selection
    /// are silently ignored.
    UngroupTracks {
        track_ids: Vec<u32>,
    },
    /// Reparent a track. `track_id` becomes a child of `parent_id` (or
    /// a top-level track when `parent_id == None`). The graph compiler
    /// rejects the edit (silently keeping the old parent) if it would
    /// produce a cycle.
    SetTrackParent {
        track_id: u32,
        parent_id: Option<u32>,
    },
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
    /// Recovery modal で「復元」 を押した。 候補 .autosave.daw を読み込み、
    /// candidates から remove + 元 file 削除。 sidecar 復元なら file_path は
    /// 元 .daw、 recovery_dir 復元なら file_path = None (新規プロジェクト扱い)。
    RecoveryRestore(PathBuf),
    /// Recovery modal で「破棄」 を押した。 該当 .autosave.daw を削除 +
    /// candidates から remove。
    RecoveryDiscard(PathBuf),
    /// Recovery modal を閉じる (候補は次回起動時にも見える)。
    RecoveryDismiss,
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
    /// gui_01 #017 (M14 Phase 59) で piano_roll widget が L キー → Enter
    /// commit 時に発行する歌詞分配バッチ。 各 `(note_id, lyric)` を指定
    /// `clip_ref` 内で更新。 widget が空文字列を `None` に正規化済みなので
    /// daw_01 側で `is_empty` 判定不要 (None = 歌詞削除)。 1 batch = 1 undo。
    SetNoteLyrics {
        clip_ref: ClipRef,
        lyrics: Vec<(u32, Option<String>)>,
    },

    // -------- Plugin picker / chain ---------------------------------------
    OpenPluginPickerFor(PickerTarget),
    ClosePluginPicker,
    SelectPluginFromDb(String),
    ToggleSlotGui { slot_kind: u8, slot_index: u32 },
    RemoveSlot { slot_kind: u8, slot_index: u32 },
    /// PR4 sidechain: wire / unwire the sidechain source for a plugin's
    /// aux input port. `track_id` + (slot_kind, slot_index) identifies
    /// the plugin instance; `port` selects the aux input port on that
    /// plugin (0 = first sidechain bus); `source` is `Some(track_id)`
    /// to wire from a track, or `None` to disconnect.
    SetSidechainSource {
        track_id: u32,
        slot_kind: u8,
        slot_index: u32,
        port: u8,
        source: Option<u32>,
    },
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
    SlotPluginLoadedFromChild {
        track: u32,
        slot: PluginSlot,
        id: String,
        name: String,
        /// plugin_host が割り振った session-unique な plugin instance id。
        /// daw_gui 側は `track_plugin_ids` に登録して、 後続の delete /
        /// ungroup で先に ClosePluginShmem を audio に直接送るのに使う。
        plugin_id: u32,
    },
    /// plugin_host が plugin destroy したことの通知。 `track_plugin_ids`
    /// から該当 plugin_id を取り除き、 もし audio engine 側で未削除
    /// (= daw_gui が直接 ClosePluginShmem を先送りしていない経路で
    /// 来た場合) なら ClosePluginShmem を audio に転送する。
    SlotPluginUnloadedFromChild {
        plugin_id: u32,
    },
    /// PR3.3: plugin_host から forward された 「plugin が報告した latency」 通知。
    /// `plugin_latencies` に積んで track の累積 latency を再計算、 song を
    /// 更新して LoadSong を daw_audio に再送 (compile_schedule で PDC 反映)。
    PluginLatencyChangedFromChild {
        plugin_id: u32,
        samples: u32,
    },
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

    // -------- Grid snap ---------------------------------------------------
    SetPianoRollSnapEnabled(bool),
    SetPianoRollSnapChoice(u8),
    SetArrangeSnapEnabled(bool),
    SetArrangeSnapChoice(u8),
    TogglePianoRollSnap,
    ToggleArrangeSnap,
    /// `1` キー (Ableton Live "Narrow Grid" 互換): snap unit を 1 段細かく。
    NarrowPianoRollGrid,
    NarrowArrangeGrid,
    /// `2` キー (Widen Grid): snap unit を 1 段粗く。
    WidenPianoRollGrid,
    WidenArrangeGrid,
    /// `3` キー (Toggle Triplet): Straight ↔ Triplet (div は維持)。
    TogglePianoRollTriplet,
    ToggleArrangeTriplet,
    /// `X` キー / "Fit" ボタン / SelectClip 経由の auto-fit zoom。
    /// piano_roll は selected_clip のノート bbox に、arrangement は全 clip に fit。
    FitPianoRollToClip,
    FitArrangeToContent,

    // -------- Mixer -------------------------------------------------------
    SetTrackVolume { track: u32, amp: f32 },
    SetTrackPan { track: u32, pan: f32 },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    TrackPeaksTick(Vec<(f32, f32)>),

    // -------- VOICEVOX ----------------------------------------------------
    SynthesizeVocal,
    VocalSynthCompleted,
    /// VOICEVOX engine `/singers` の取得結果。 起動時 background thread が
    /// 1 度発行する。 失敗時は空 Vec で送る。
    SingersLoaded(Vec<common::voicevox::VoiceVoxSinger>),
    /// Track Inspector の Vocal speaker dropdown で選択された singer。
    /// `track.source` を `InstrumentSource::Vocal { speaker_id, style_name }`
    /// に書き換える。 Vocal 以外の track に対しては no-op。
    SetTrackSpeaker {
        track: u32,
        speaker_id: u32,
        style_name: String,
    },

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
            AppEvent::BpmEditChanged(s) => {
                self.bpm_edit_text = s;
            }
            AppEvent::CommitBpmEdit => {
                self.commit_bpm_edit();
            }
            AppEvent::TimeSigNumEditChanged(s) => {
                self.time_sig_num_edit_text = s;
            }
            AppEvent::CommitTimeSigNumEdit => {
                self.commit_time_sig_num_edit();
            }
            AppEvent::SetSongTimeSigDenominator(den) => {
                self.set_song_time_sig_denominator(den);
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
            AppEvent::GroupSelectedTracks { track_ids } => {
                self.action_group_selected_tracks(&track_ids);
            }
            AppEvent::UngroupTracks { track_ids } => {
                self.action_ungroup_tracks(&track_ids);
            }
            AppEvent::SetTrackParent { track_id, parent_id } => {
                self.action_set_track_parent(track_id, parent_id);
            }
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
            AppEvent::RecoveryRestore(path) => {
                self.restore_recovery(path);
            }
            AppEvent::RecoveryDiscard(path) => {
                self.discard_recovery(path);
            }
            AppEvent::RecoveryDismiss => {
                self.show_recovery_modal = false;
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
                if let Some(&last_idx) = self.selected_notes.last()
                    && let Some(r) = self.selected_clip
                    && let Some(note) = self
                        .song
                        .tracks
                        .get(r.track as usize)
                        .and_then(|t| t.clips.get(r.clip as usize))
                        .and_then(|c| c.notes.get(last_idx as usize))
                {
                    self.last_note_duration_beats = note.duration_beats.max(0.0625);
                }
            }
            AppEvent::DeleteSelectedNotes => self.delete_selected_notes(),
            AppEvent::SetNoteLyrics { clip_ref, lyrics } => {
                self.set_note_lyrics(clip_ref, &lyrics);
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
            AppEvent::SetSidechainSource {
                track_id,
                slot_kind,
                slot_index,
                port,
                source,
            } => {
                self.set_sidechain_source(track_id, slot_kind, slot_index, port, source);
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
            AppEvent::SlotPluginLoadedFromChild { track, slot, id, name, plugin_id } => {
                self.on_plugin_loaded_from_child(track, slot, id, name, plugin_id);
            }
            AppEvent::SlotPluginUnloadedFromChild { plugin_id } => {
                self.on_plugin_unloaded_from_child(plugin_id);
            }
            AppEvent::PluginLatencyChangedFromChild { plugin_id, samples } => {
                self.on_plugin_latency_changed(plugin_id, samples);
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
            AppEvent::SingersLoaded(singers) => {
                tracing::info!(
                    count = singers.len(),
                    "VOICEVOX singers loaded"
                );
                self.singers = singers;
            }
            AppEvent::SetTrackSpeaker { track, speaker_id, style_name } => {
                self.set_track_speaker(track, speaker_id, style_name);
            }
            AppEvent::SetPianoRollSnapEnabled(b) => {
                self.pianoroll_snap_enabled = b;
            }
            AppEvent::SetPianoRollSnapChoice(c) => {
                self.pianoroll_snap_choice = clamp_snap_choice(c);
            }
            AppEvent::SetArrangeSnapEnabled(b) => {
                self.arrange_snap_enabled = b;
            }
            AppEvent::SetArrangeSnapChoice(c) => {
                self.arrange_snap_choice = clamp_snap_choice(c);
            }
            AppEvent::TogglePianoRollSnap => {
                self.pianoroll_snap_enabled = !self.pianoroll_snap_enabled;
            }
            AppEvent::ToggleArrangeSnap => {
                self.arrange_snap_enabled = !self.arrange_snap_enabled;
            }
            AppEvent::NarrowPianoRollGrid => {
                self.pianoroll_snap_choice =
                    crate::view::snap::narrow_choice(self.pianoroll_snap_choice);
            }
            AppEvent::NarrowArrangeGrid => {
                self.arrange_snap_choice =
                    crate::view::snap::narrow_choice(self.arrange_snap_choice);
            }
            AppEvent::WidenPianoRollGrid => {
                self.pianoroll_snap_choice =
                    crate::view::snap::widen_choice(self.pianoroll_snap_choice);
            }
            AppEvent::WidenArrangeGrid => {
                self.arrange_snap_choice =
                    crate::view::snap::widen_choice(self.arrange_snap_choice);
            }
            AppEvent::TogglePianoRollTriplet => {
                self.pianoroll_snap_choice =
                    crate::view::snap::toggle_triplet_choice(self.pianoroll_snap_choice);
            }
            AppEvent::ToggleArrangeTriplet => {
                self.arrange_snap_choice =
                    crate::view::snap::toggle_triplet_choice(self.arrange_snap_choice);
            }
            AppEvent::FitPianoRollToClip => {
                self.fit_piano_roll_to_clip();
            }
            AppEvent::FitArrangeToContent => {
                self.fit_arrange_to_content();
            }
        }
    }
}

fn clamp_snap_choice(c: u8) -> u8 {
    let max = (crate::view::snap::SNAP_LABELS.len() - 1) as u8;
    c.min(max)
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

    pub(crate) fn sync_song_to_plugin_host(&mut self) {
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
        self.selected_track_ids.clear();
        self.collapsed_groups.clear();
        self.selected_clip = None;
        self.selected_notes.clear();
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        self.resync_song_edit_texts();
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
        // Recursive open を防ぐ: autosave file を直接開いた場合は弾く
        // (RecoveryRestore で開くべきもの)。
        if common::recovery::is_autosave_file(&path) {
            self.status_message = format!(
                "autosave ファイルは Recovery modal から復元してください: {}",
                path.display()
            );
            return;
        }
        match common::project::load(&path) {
            Ok(mut song) => {
                tracing::info!(path = %path.display(), "loaded project");
                song.ensure_ids();
                self.restore_plugin_from_song(&song);
                self.song = song;
                self.file_path = Some(path.clone());
                self.selected_track_ids.clear();
                self.collapsed_groups.clear();
                self.selected_clip = None;
                self.selected_notes.clear();
                self.resize_track_peak_display();
                self.sync_song_to_plugin_host();
                self.resync_song_edit_texts();
                self.is_dirty = false;
                // sidecar 検出: 前回のセッションが正常終了せず、 同 file の
                // autosave が残っているなら recovery modal に追加。 ユーザーが
                // 「復元」 で sidecar に切り替えられる。
                let sidecar = common::recovery::sidecar_for(&path);
                if sidecar.exists() && !self.recovery_candidates.contains(&sidecar) {
                    tracing::info!(
                        sidecar = %sidecar.display(),
                        "sidecar autosave detected on open"
                    );
                    self.recovery_candidates.push(sidecar);
                    self.show_recovery_modal = true;
                }
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
        if self.last_autosave.elapsed() < std::time::Duration::from_secs(60) {
            return;
        }

        // 保存先決定: file_path Some なら sidecar、 None なら recovery_dir。
        let autosave_path = match self.file_path.as_ref() {
            Some(orig) => common::recovery::sidecar_for(orig),
            None => {
                if let Err(e) = common::recovery::ensure_recovery_dir() {
                    tracing::warn!(error = ?e, "failed to create recovery dir");
                    return;
                }
                let Some(p) = common::recovery::recovery_path_for_session(
                    &self.recovery_session_id,
                ) else {
                    tracing::warn!(
                        "could not resolve recovery path (no LOCALAPPDATA?)"
                    );
                    return;
                };
                p
            }
        };

        match common::project::save(&autosave_path, &self.song) {
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

    /// Recovery modal で「復元」 を押した処理。 sidecar 形式 (`<x>.daw.autosave.daw`)
    /// なら元 `<x>.daw` を file_path にセット、 recovery_dir 内 (`<uuid>.autosave.daw`)
    /// なら file_path = None (新規プロジェクト扱い、 ユーザーが Save As)。
    fn restore_recovery(&mut self, autosave_path: PathBuf) {
        let Ok(mut song) = common::project::load(&autosave_path) else {
            tracing::error!(
                path = %autosave_path.display(),
                "failed to load recovery file"
            );
            self.status_message =
                format!("復元失敗: {}", autosave_path.display());
            return;
        };
        song.ensure_ids();
        self.restore_plugin_from_song(&song);
        self.song = song;
        self.file_path = common::recovery::original_file_for_sidecar(&autosave_path);
        self.selected_track_ids.clear();
        self.collapsed_groups.clear();
        self.selected_clip = None;
        self.selected_notes.clear();
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        self.resync_song_edit_texts();
        self.is_dirty = false;
        let _ = std::fs::remove_file(&autosave_path);
        self.recovery_candidates.retain(|p| p != &autosave_path);
        if self.recovery_candidates.is_empty() {
            self.show_recovery_modal = false;
        }
        tracing::info!(
            recovered_to = ?self.file_path,
            "recovery restored"
        );
    }

    /// Recovery modal で「破棄」 を押した処理。 file 削除 + candidates から外す。
    fn discard_recovery(&mut self, autosave_path: PathBuf) {
        if let Err(e) = std::fs::remove_file(&autosave_path) {
            tracing::warn!(
                error = ?e,
                path = %autosave_path.display(),
                "failed to remove recovery file"
            );
        }
        self.recovery_candidates.retain(|p| p != &autosave_path);
        if self.recovery_candidates.is_empty() {
            self.show_recovery_modal = false;
        }
    }

    /// アプリ正常終了時 (`WindowEvent::CloseRequested`) に呼ぶ cleanup。
    /// 自セッションで作った recovery file (sidecar / recovery_dir 両方) を削除。
    /// recovery file が無ければ no-op。 削除失敗は warn でログのみ。
    pub fn on_shutdown(&self) {
        // 自セッションの recovery_dir file
        if let Some(p) = common::recovery::recovery_path_for_session(
            &self.recovery_session_id,
        ) && p.exists()
            && let Err(e) = std::fs::remove_file(&p)
        {
            tracing::warn!(
                error = ?e,
                path = %p.display(),
                "failed to remove recovery file on shutdown"
            );
        }
        // sidecar (file_path が Some なら)
        if let Some(orig) = self.file_path.as_ref() {
            let side = common::recovery::sidecar_for(orig);
            if side.exists()
                && let Err(e) = std::fs::remove_file(&side)
            {
                tracing::warn!(
                    error = ?e,
                    path = %side.display(),
                    "failed to remove sidecar on shutdown"
                );
            }
        }
    }

    fn restore_plugin_from_song(&mut self, song: &Song) {
        let Some(db) = self.plugin_db.clone() else {
            tracing::warn!("plugin database not loaded; cannot resolve plugin ids");
            return;
        };
        // PR2.1: send `Track::id` (not Vec position) so the plugin host
        // keys its chains by id from the start.
        let mut to_send: Vec<(u32, PluginSlot, common::model::PluginInstance)> = Vec::new();
        for track in song.tracks.iter() {
            let t = track.id;
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
            self.track_pending_load(track, slot);
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
        // A7: if any plugin is still in the SetSlotPlugin →
        // SlotPluginLoaded round-trip (its `OpenPluginShmem` may not
        // have reached the audio engine yet), queue the Play so every
        // track starts on the same buffer once registration completes.
        // Without this the just-loaded tracks render silent for the
        // first few buffers / first loop.
        if !self.pending_plugin_loads.is_empty() {
            self.pending_play = true;
            self.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.pending_plugin_loads.len()
            );
            return;
        }
        let song = self.song.clone();
        self.send_audio(MainToChild::LoadSong(song));
        self.send_audio(MainToChild::Play);
        self.is_playing = true;
    }

    /// A7: register a `(track, slot)` we just sent `SetSlotPlugin` for,
    /// and — if playback is currently running — pause it until the last
    /// `SlotPluginLoaded` arrives. Without the pause, plugins loaded
    /// while playing render silent until the audio engine's
    /// `OpenPluginShmem` register catches up (typically several buffers
    /// or a loop wrap behind).
    fn track_pending_load(&mut self, track: u32, slot: PluginSlot) {
        if self.pending_plugin_loads.is_empty() && self.is_playing {
            self.send_audio(MainToChild::Stop);
            self.is_playing = false;
            self.pending_play = true;
        }
        self.pending_plugin_loads.insert((track, slot));
        if self.pending_play {
            self.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.pending_plugin_loads.len()
            );
        }
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

        // When deleting a Group track, Live recursively removes its
        // entire subtree (children + nested groups) so dangling
        // `parent_group_id` references don't survive. Collect the full
        // subtree of stable ids, then resolve them to current indices
        // and remove from highest to lowest so earlier indices stay
        // valid during the loop.
        let target_id = self.song.tracks[idx as usize].id;
        let subtree_ids = self.collect_track_subtree_ids(target_id);
        let mut subtree_idxs: Vec<u32> = subtree_ids
            .iter()
            .filter_map(|id| self.song.track_index_by_id(*id))
            .map(|i| i as u32)
            .collect();
        subtree_idxs.sort_unstable();
        subtree_idxs.dedup();

        // PR2.1 race-fix: 順序を「song update → LoadSong → plugin
        // destroy → RemoveTrack」 に固定する。 song update を先に送ら
        // ないと、 audio thread が古い schedule (削除対象 track の
        // ProcessTrack / ProcessGroupFx を含む) で destroyed plugin に
        // dispatch して deadlock する。
        // (a) snapshot を取って順次 song.tracks.remove
        let mut snapshots: Vec<(u32, common::model::Track)> =
            Vec::with_capacity(subtree_idxs.len());
        for &i in subtree_idxs.iter().rev() {
            let removed_id = self.song.tracks[i as usize].id;
            let snapshot = self.song.tracks[i as usize].clone();
            #[cfg(windows)]
            {
                self.plugin_host_windows.retain(|&(t, _), _| t != removed_id);
            }
            self.song.tracks.remove(i as usize);
            snapshots.push((removed_id, snapshot));
        }
        // (b) LoadSong で audio engine を新 schedule に
        self.sync_song_to_plugin_host();
        // (c) **重要 (deadlock 防止)**: RemoveTrack 送信前に daw_audio
        // に直接 ClosePluginShmem を送って plugin_refs から stale entry
        // を消す。 plugin_host の `plugin_shmems.remove` で shmem を
        // unmap した直後、 audio worker が `pd.prepare()` で unmapped
        // memory を読み AV → silent terminate → all_done 永久 wait
        // を防ぐため。
        for (removed_id, _snapshot) in snapshots {
            if let Some(pids) = self.track_plugin_ids.remove(&removed_id) {
                for pid in pids {
                    self.send_audio(MainToChild::ClosePluginShmem { plugin_id: pid });
                }
            }
            self.send_plugin(MainToChild::RemoveTrack { track: removed_id });
        }

        // selected_clip: if its track was deleted, clear; otherwise
        // shift down by the number of deleted tracks above it.
        if let Some(r) = self.selected_clip {
            if subtree_idxs.contains(&r.track) {
                self.selected_clip = None;
                self.selected_notes.clear();
            } else {
                let drops_above = subtree_idxs.iter().filter(|&&i| i < r.track).count() as u32;
                self.selected_clip = Some(ClipRef {
                    track: r.track - drops_above,
                    clip: r.clip,
                });
            }
        }

        // selected_track_ids: subtree に含まれていた id を全て除外。
        // 残りが空なら直近の生存 track にフォールバック (UI 完全選択
        // ゼロを避ける)。
        let subtree_ids_set: std::collections::HashSet<u32> = subtree_ids.iter().copied().collect();
        self.selected_track_ids
            .retain(|id| !subtree_ids_set.contains(id));
        if self.selected_track_ids.is_empty()
            && let Some(t) = self.song.tracks.last()
        {
            self.selected_track_ids.push(t.id);
        }
        // collapsed_groups からも消えた id を除外。
        self.collapsed_groups
            .retain(|id| !subtree_ids_set.contains(id));
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    /// Return `root_id` plus every descendant track that points at it
    /// (directly or transitively) via `parent_group_id`. Used by
    /// `delete_track` when removing a Group: the whole subtree is
    /// dropped together (Live convention) so no orphan references
    /// survive. Cycle-safe via a hop limit.
    fn collect_track_subtree_ids(&self, root_id: u32) -> Vec<u32> {
        let mut result = vec![root_id];
        let mut frontier = vec![root_id];
        let mut hops = 0;
        while !frontier.is_empty() {
            hops += 1;
            if hops > self.song.tracks.len() + 1 {
                tracing::error!(
                    root_id,
                    "collect_track_subtree_ids: cycle detected, aborting BFS"
                );
                break;
            }
            let mut next = Vec::new();
            for &pid in &frontier {
                for t in &self.song.tracks {
                    if t.parent_group_id == Some(pid) && !result.contains(&t.id) {
                        result.push(t.id);
                        next.push(t.id);
                    }
                }
            }
            frontier = next;
        }
        result
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
        // PR2.1: plugin_host の chains は `Track::id` ベースなので、
        // Vec position swap は通知不要。 SwapTracks IPC は削除済。
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
        // selected_track_ids は id ベースなので track の index swap で
        // 自動的に追従する (id は変わらないため再マッピング不要)。
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
            .get(self.cursor_track_index().unwrap_or(0))
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

        // selected_track_ids は id ベースなので、 reorder 後も自動的に
        // 整合 (id は変わらず、 song.tracks の Vec 内 index が変わるだけ
        // で `cursor_track_index` が再評価される)。 selected_track_id
        // 局所変数は不要。
        let _ = selected_track_id;
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

        // PR2.1: plugin_host の chains は `Track::id` ベースなので、
        // Vec position の reorder は通知不要。 ReorderTracks IPC は
        // 削除済。 LoadSong (sync_song_to_plugin_host) で song_store
        // のみ新順序に同期する。
        let _ = index_order;
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    /// 単独選択する (index ベース、 旧 API 互換)。 新 multi-select API
    /// (gui_01 #016) からは `SelectTrack { next, modifier, .. }` 経由で
    /// `selected_track_ids` を直接書き込む。
    fn select_track(&mut self, idx: u32) {
        let Some(t) = self.song.tracks.get(idx as usize) else {
            return;
        };
        let id = t.id;
        if self.selected_track_ids.as_slice() != [id] {
            self.selected_track_ids = vec![id];
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

    /// Ableton Live's Cmd/Ctrl+G: wrap the listed `track_ids` in a
    /// fresh `kind == Group` track. The selected tracks become the
    /// group's children (their `parent_group_id` is rewritten), and
    /// the new group is inserted just after the highest-indexed
    /// selected track so it visually sits at the top of its
    /// children's "block" (Live convention). Empty selections are
    /// silently ignored — Live forbids empty groups.
    ///
    /// Already-grouped tracks keep their old parent's settings; only
    /// their immediate parent pointer is rewritten. If a selected
    /// track was a group itself, it ends up nested under the new
    /// group (depth unbounded).
    fn action_group_selected_tracks(&mut self, track_ids: &[u32]) {
        if track_ids.is_empty() {
            tracing::info!("group request ignored: empty selection");
            return;
        }
        // De-duplicate while preserving the first-appearance order.
        let mut child_ids: Vec<u32> = Vec::with_capacity(track_ids.len());
        for &id in track_ids {
            if !child_ids.contains(&id) {
                child_ids.push(id);
            }
        }
        // Validate all ids exist before mutating anything.
        if child_ids.iter().any(|id| self.song.track_by_id(*id).is_none()) {
            tracing::warn!(?child_ids, "group request: stale track id, abort");
            return;
        }
        // 仕様 §4: 「選択トラックのうち、 index が最も小さいものの
        // 直前」 に新グループを挿入 (= 一番上の選択 track の上)。
        // Live 互換、 視覚的には「子の上にヘッダー行」。
        let top_child_idx = child_ids
            .iter()
            .filter_map(|id| self.song.track_index_by_id(*id))
            .min()
            .unwrap_or(self.song.tracks.len());
        // Inherit the common parent of the selection if every selected
        // track shared the same `parent_group_id` — preserves Live's
        // behaviour of grouping inside a group keeps you in the parent.
        let common_parent = {
            let first_parent = self
                .song
                .track_by_id(child_ids[0])
                .and_then(|t| t.parent_group_id);
            if child_ids.iter().all(|id| {
                self.song
                    .track_by_id(*id)
                    .and_then(|t| t.parent_group_id)
                    == first_parent
            }) {
                first_parent
            } else {
                None
            }
        };
        let group_id = self.song.alloc_track_id();
        let group_index = self.song.tracks.len() + 1;
        let group_track = Track {
            id: group_id,
            name: format!("Group {group_index}"),
            // Reaper folder model: a "group" is just a track that has
            // children. No dedicated kind enum — once the children's
            // `parent_group_id` is repointed below, this track auto-
            // matically becomes a group bus to the engine.
            parent_group_id: common_parent,
            source: InstrumentSource::None,
            clips: Vec::new(),
            ..Track::default()
        };
        // Repoint every selected track's parent to the new group.
        for &cid in &child_ids {
            if let Some(t) = self.song.track_by_id_mut(cid) {
                t.parent_group_id = Some(group_id);
            }
        }
        // 仕様 §4: 「一番上の選択 track の直前」 に挿入 (= 子の上に
        // ヘッダー)。 PR2.1 で plugin_host の chains を `Track::id`
        // ベースに改修した結果、 Vec::insert で既存 track の Vec
        // position が shift しても plugin chain の lookup は壊れない
        // (engine の `slot_to_plugin_id` も (track_id, slot) ベース)。
        let insert_at = top_child_idx.min(self.song.tracks.len());
        self.song.tracks.insert(insert_at, group_track);
        // 新規 group track を選択状態に (Live 互換: グループ化直後は
        // 親 group が selection cursor になる)。
        self.selected_track_ids = vec![group_id];
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(group_id, ?child_ids, "grouped tracks");
    }

    /// `action_ungroup_tracks` / `delete_track` で送る IPC 列を組み立てる
    /// pure function。 順序が必須仕様 (deadlock 防止) なので、 ロジックを
    /// ここに集約して unit test で検証する:
    ///
    /// 1. `audio: ClosePluginShmem(plugin_id)` × N — 削除対象 track が
    ///    持っていた全 plugin について先に audio engine に送る。 これに
    ///    より plugin_refs / slot_to_plugin_id から stale entry が消え、
    ///    audio worker が destroyed plugin に dispatch する race を断つ。
    /// 2. `plugin_host: RemoveTrack(track_id)` — plugin_host が chain
    ///    の Box<Plugin> を properly tear down (stop_processing →
    ///    deactivate → gui_destroy → drop) して、 shmem mapping を
    ///    unmap する。 (1) で audio 側はもう触らないので安全。
    pub fn plan_track_removal_ipc(
        track_ids: &[u32],
        track_plugin_ids: &std::collections::HashMap<u32, Vec<u32>>,
    ) -> Vec<TrackRemovalIpc> {
        let mut plan = Vec::new();
        for track_id in track_ids {
            if let Some(pids) = track_plugin_ids.get(track_id) {
                for pid in pids {
                    plan.push(TrackRemovalIpc::CloseAudioShmem { plugin_id: *pid });
                }
            }
            plan.push(TrackRemovalIpc::RemoveTrackFromPluginHost { track_id: *track_id });
        }
        plan
    }

    /// Alt+G: 選択中の group track の subtree を 1 階層持ち上げる。
    /// 仕様 §5: 子の `parent_group_id` を group の親 (master or 上位
    /// group) に向ける + group track 自体を削除。 group の `fx_chain`
    /// は失われる (Live 仕様)。 複数 group が選択されているときは深い
    /// (子) → 浅い (親) の順に処理してインデックスを安定させる。
    fn action_ungroup_tracks(&mut self, track_ids: &[u32]) {
        if track_ids.is_empty() {
            return;
        }
        // 選択された track の中から「実際に子を持つ」ものだけ ungroup
        // 対象。 通常 track が選択に混じっていても無視。
        let mut groups_to_ungroup: Vec<u32> = track_ids
            .iter()
            .copied()
            .filter(|id| self.is_group_track(*id))
            .collect();
        if groups_to_ungroup.is_empty() {
            tracing::info!(
                ?track_ids,
                "ungroup request: no group track in selection, ignored"
            );
            return;
        }
        // 深さ降順 (子から先に処理)。 同階層なら index 大きい方から。
        groups_to_ungroup.sort_by_key(|id| {
            let depth = self
                .song
                .track_by_id(*id)
                .map(|t| self.compute_track_depth(t))
                .unwrap_or(0);
            (-(depth as i32), -(self.song.track_index_by_id(*id).unwrap_or(0) as i32))
        });

        // 各 group の plugin chain snapshot を **削除前に** 取得して
        // おく (後の plugin destroy 用)。 song.tracks から group を
        // remove した後では取得できない。
        let group_snapshots: Vec<(u32, common::model::Track)> = groups_to_ungroup
            .iter()
            .filter_map(|gid| self.song.track_by_id(*gid).map(|t| (*gid, t.clone())))
            .collect();

        let mut new_selection: Vec<u32> = Vec::new();
        for group_id in &groups_to_ungroup {
            let Some(group_track) = self.song.track_by_id(*group_id) else {
                continue;
            };
            let new_parent = group_track.parent_group_id;
            for t in &mut self.song.tracks {
                if t.parent_group_id == Some(*group_id) {
                    t.parent_group_id = new_parent;
                    new_selection.push(t.id);
                }
            }
            if let Some(pos) = self.song.tracks.iter().position(|t| t.id == *group_id) {
                #[cfg(windows)]
                {
                    self.plugin_host_windows
                        .retain(|&(t, _), _| t != *group_id);
                }
                self.song.tracks.remove(pos);
            }
            self.collapsed_groups.remove(group_id);
        }

        // **song update + LoadSong を先に送る** → daw_audio engine が
        // 新 schedule (group が消えた状態) を即適用。 audio thread が
        // 古い schedule の ProcessGroupFx で destroyed plugin にアクセス
        // する race を回避する。
        self.sync_song_to_plugin_host();

        // **重要 (deadlock 防止)**: plugin_host が `tracks.mutate` で
        // chain の Box<Plugin> を drop すると `plugin_shmems.remove(&pid)`
        // で `ProcessDataHandle` も drop され、 OS が shmem mapping を
        // unmap する。 audio worker thread がその直後に `pd.prepare()`
        // で unmapped memory を読むと **access violation で worker が
        // silently terminate** し、 master の `WaitForSingleObject(all_done,
        // INFINITE)` が永久 wait → 18 秒 audio thread 完全停止。
        //
        // 対策: RemoveTrack を plugin_host に送る **前に** daw_audio に
        // 直接 ClosePluginShmem を送って `plugin_refs` / `slot_to_plugin_id`
        // から stale entry を削除させ、 audio worker が destroyed plugin
        // を dispatch しないようにする。
        let _ = group_snapshots;
        for group_id in &groups_to_ungroup {
            if let Some(pids) = self.track_plugin_ids.remove(group_id) {
                for pid in pids {
                    self.send_audio(MainToChild::ClosePluginShmem { plugin_id: pid });
                }
            }
            self.send_plugin(MainToChild::RemoveTrack { track: *group_id });
        }
        // selection: ungroup 後は元 group の子を選択 (Live 互換)。
        if !new_selection.is_empty() {
            self.selected_track_ids = new_selection;
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(?groups_to_ungroup, "ungrouped tracks");
    }

    /// Reparent `track_id` to `parent_id` (or detach to the master bus
    /// when `parent_id` is None). Any track is allowed as a parent
    /// (the "group" role is implicit — a track that has children).
    /// Validates the new parent chain doesn't contain `track_id`
    /// itself so the schedule compiler never sees a cyclic state.
    fn action_set_track_parent(&mut self, track_id: u32, parent_id: Option<u32>) {
        if Some(track_id) == parent_id {
            tracing::warn!(track_id, "ignored self-parent edit");
            return;
        }
        if let Some(pid) = parent_id {
            if self.song.track_by_id(pid).is_none() {
                tracing::warn!(track_id, parent_id = pid, "ignored: parent track not found");
                return;
            }
            // Walk the parent's chain upward looking for `track_id`. If
            // we find it, the edit would create a cycle.
            let mut cursor = Some(pid);
            let mut hops = 0u32;
            while let Some(c) = cursor {
                if c == track_id {
                    tracing::warn!(track_id, parent_id = pid, "ignored: would create a cycle");
                    return;
                }
                hops += 1;
                if hops > self.song.tracks.len() as u32 + 1 {
                    // Existing graph already has a cycle; abort to avoid an infinite loop.
                    tracing::error!("existing parent chain is cyclic; aborting reparent");
                    return;
                }
                cursor = self
                    .song
                    .track_by_id(c)
                    .and_then(|t| t.parent_group_id);
            }
        }
        let Some(track) = self.song.track_by_id_mut(track_id) else {
            tracing::warn!(track_id, "ignored: track not found");
            return;
        };
        track.parent_group_id = parent_id;
        self.sync_song_to_plugin_host();
        tracing::info!(track_id, ?parent_id, "track reparented");
    }

    fn action_remove_last_track(&mut self) {
        let len = self.song.tracks.len();
        if len == 0 {
            return;
        }
        // PR2.1: pop() の前に id を保存し、 IPC は id で送る。
        let Some(removed) = self.song.tracks.pop() else {
            return;
        };
        let removed_id = removed.id;
        tracing::info!(
            index = (len - 1) as u32,
            id = removed_id,
            name = %removed.name,
            "removed last track"
        );
        #[cfg(windows)]
        {
            self.plugin_host_windows
                .retain(|&(t, _), _| t != removed_id);
        }
        self.send_plugin(MainToChild::RemoveTrack { track: removed_id });
        // selected_track_ids は id ベース。 削除対象 track id を除外
        // (Vec の index で持つ subtree とは異なり id 直接判定)。 残りが
        // 空なら最後尾にフォールバック。
        let live_ids: std::collections::HashSet<u32> =
            self.song.tracks.iter().map(|t| t.id).collect();
        self.selected_track_ids.retain(|id| live_ids.contains(id));
        if self.selected_track_ids.is_empty()
            && let Some(t) = self.song.tracks.last()
        {
            self.selected_track_ids.push(t.id);
        }
        self.collapsed_groups.retain(|id| live_ids.contains(id));
        // `selected_clips` / `selected_clip` は ClipRef.track を Vec
        // index で持つ仕様。 末尾削除なので index = pop した位置 (= 旧 len-1)。
        let removed_idx = len as u32 - 1;
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
        // クリップが新しく primary になったらピアノロールを auto-fit。
        // 同 clip 再選択でも fit し直す (ノート編集で範囲が変わることがある)。
        if primary.is_some() {
            self.fit_piano_roll_to_clip();
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
        if primary.is_some() {
            self.fit_piano_roll_to_clip();
        }
    }

    /// 現 selected_clip のノート bounding box が piano_roll grid 領域に
    /// 収まるよう zoom_x / zoom_y / scroll_beat / top_pitch を自動調整する。
    /// ノート無しの clip は clip 全長が見える初期 zoom にフォールバック。
    /// `last_pianoroll_grid_size` が未測定 (= 0) の場合は何もしない。
    fn fit_piano_roll_to_clip(&mut self) {
        let Some(target) = self.selected_clip else { return };
        let Some(track) = self.song.tracks.get(target.track as usize) else { return };
        let Some(clip) = track.clips.get(target.clip as usize) else { return };
        let (grid_w, grid_h) = self.last_pianoroll_grid_size;
        if grid_w < 16.0 || grid_h < 16.0 {
            return;
        }

        if clip.notes.is_empty() {
            self.pianoroll_scroll_beat = 0.0;
            self.pianoroll_zoom_x =
                (grid_w / clip.length_beats.max(1.0) as f32).clamp(8.0, 400.0);
            self.pianoroll_top_pitch = 84;
            self.pianoroll_zoom_y = 14.0;
        } else {
            let min_beat = clip
                .notes
                .iter()
                .map(|n| n.start_beat)
                .fold(f64::INFINITY, f64::min);
            let max_beat = clip
                .notes
                .iter()
                .map(|n| n.start_beat + n.duration_beats)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_pitch = clip.notes.iter().map(|n| n.pitch).min().unwrap_or(60);
            let max_pitch = clip.notes.iter().map(|n| n.pitch).max().unwrap_or(60);

            let span_beats = (max_beat - min_beat + 2.0).max(1.0);
            let span_pitch = (i32::from(max_pitch) - i32::from(min_pitch) + 4).max(4);

            self.pianoroll_scroll_beat = (min_beat - 1.0).max(0.0) as f32;
            self.pianoroll_zoom_x = (f64::from(grid_w) / span_beats).clamp(8.0, 400.0) as f32;
            self.pianoroll_top_pitch = (i32::from(max_pitch) + 2).clamp(11, 127) as u8;
            self.pianoroll_zoom_y = (grid_h / span_pitch as f32).clamp(6.0, 40.0);
        }
        self.pianoroll_notes_generation = self.pianoroll_notes_generation.wrapping_add(1);
    }

    /// 全 track の全 clip が arrangement canvas に収まるよう zoom_x / scroll_beat /
    /// track_row_h を自動調整する。clip 0 個なら song.length_beats でフォールバック。
    fn fit_arrange_to_content(&mut self) {
        let (canvas_w, canvas_h) = self.last_arrange_canvas_size;
        if canvas_w < 16.0 || canvas_h < 16.0 {
            return;
        }
        let track_count = self.song.tracks.len().max(1);

        let (min_beat, max_beat) = self
            .song
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| {
                (lo.min(c.start_beat), hi.max(c.start_beat + c.length_beats))
            });
        let (min_beat, max_beat) = if min_beat.is_finite() {
            (min_beat, max_beat)
        } else {
            (0.0, self.song.length_beats.max(16.0))
        };

        let span_beats = (max_beat - min_beat + 4.0).max(4.0);
        self.arrange_scroll_beat = (min_beat - 2.0).max(0.0) as f32;
        self.arrange_zoom_x = (f64::from(canvas_w) / span_beats).clamp(2.0, 400.0) as f32;
        let row_h = (canvas_h / track_count as f32).clamp(16.0, 96.0);
        self.arrange_track_row_h = row_h;
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
        self.last_note_duration_beats = duration;
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
        if let Some(&(_, _, duration)) = entries.last() {
            self.last_note_duration_beats = duration.max(0.0625);
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

    /// Track Inspector の Vocal speaker dropdown 経由で speaker_id 変更。
    /// 対象 track が `InstrumentSource::Vocal` でなければ no-op。
    fn set_track_speaker(&mut self, track: u32, speaker_id: u32, style_name: String) {
        let Some(t) = self.song.tracks.get_mut(track as usize) else {
            return;
        };
        let common::model::InstrumentSource::Vocal {
            speaker_id: cur_id,
            style_name: cur_style,
        } = &mut t.source
        else {
            return;
        };
        if *cur_id == speaker_id && *cur_style == style_name {
            return;
        }
        *cur_id = speaker_id;
        *cur_style = style_name;
        self.sync_song_to_plugin_host();
    }

    /// gui_01 #017 (M14 Phase 59): piano_roll widget が L キー → Enter
    /// commit で発行する歌詞分配 batch を、 指定 `clip_ref` 内の note に
    /// 適用。 各 entry は `(note_index, Option<String>)`、 widget 側で空文字列
    /// は `None` に正規化済み (= 歌詞削除)。 clip_ref が無効なら no-op。
    fn set_note_lyrics(&mut self, clip_ref: ClipRef, updates: &[(u32, Option<String>)]) {
        let Some(track) = self.song.tracks.get_mut(clip_ref.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(clip_ref.clip as usize) else {
            return;
        };
        let mut changed = false;
        for (id, lyric) in updates {
            if let Some(n) = clip.notes.get_mut(*id as usize) {
                let normalised =
                    lyric.as_ref().and_then(|s| {
                        let t = s.trim();
                        if t.is_empty() { None } else { Some(t.to_string()) }
                    });
                if n.lyric != normalised {
                    n.lyric = normalised;
                    changed = true;
                }
            }
        }
        if changed {
            self.sync_song_to_plugin_host();
            self.pianoroll_notes_generation += 1;
        }
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
        track_id: u32,
        slot: PluginSlot,
        id: String,
        _name: String,
        plugin_id: u32,
    ) {
        // PR2.1: ChildToMain `track` is now a `Track::id`. Resolve to
        // a Vec position only for the local `song.tracks` mutation;
        // the plugin host stores chains by id directly.
        // plugin_id を track_plugin_ids に登録 (delete / ungroup 時の
        // ClosePluginShmem 先送りに使用、 use-after-free deadlock 防止)。
        let entry = self.track_plugin_ids.entry(track_id).or_default();
        if !entry.contains(&plugin_id) {
            entry.push(plugin_id);
        }
        self.ensure_first_track();
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
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
                    sidechain_sources: Vec::new(),
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
                    sidechain_sources: Vec::new(),
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
                    sidechain_sources: Vec::new(),
                };
                if i < t.midi_fx_chain.len() {
                    t.midi_fx_chain[i] = inst;
                } else {
                    t.midi_fx_chain.push(inst);
                }
            }
        }

        // A7: this load is done. If Play was queued waiting for the
        // last plugin to register on the audio side, fire it now.
        self.pending_plugin_loads.remove(&(track_id, slot));
        if self.pending_plugin_loads.is_empty() && self.pending_play {
            self.pending_play = false;
            self.status_message.clear();
            self.play();
        } else if !self.pending_plugin_loads.is_empty() && self.pending_play {
            self.status_message = format!(
                "プラグイン読み込み中... (残 {})",
                self.pending_plugin_loads.len()
            );
        }
    }

    /// plugin_host が plugin destroy を完了した通知を受けて、
    /// `track_plugin_ids` から該当 plugin_id を取り除く。 audio engine
    /// 側の `ClosePluginShmem` は daw_gui main.rs (`ChildToMain::
    /// SlotPluginUnloaded` ハンドラ) で先に転送済なので、 ここでは
    /// daw_gui ローカル状態のクリーンアップのみ。
    fn on_plugin_unloaded_from_child(&mut self, plugin_id: u32) {
        for entry in self.track_plugin_ids.values_mut() {
            entry.retain(|p| *p != plugin_id);
        }
        self.track_plugin_ids.retain(|_, v| !v.is_empty());
        // PR3.3: drop the latency entry for the destroyed plugin and
        // recompute every track's total since the chain shape changed.
        self.plugin_latencies.remove(&plugin_id);
        self.recompute_track_latencies();
    }

    /// PR3.3: store the new per-plugin reported latency, recompute the
    /// owning track's total (sum of all its plugin latencies), and push the
    /// updated `Song` to daw_audio so `compile_schedule` regenerates the
    /// PDC delay lines.
    fn on_plugin_latency_changed(&mut self, plugin_id: u32, samples: u32) {
        self.plugin_latencies.insert(plugin_id, samples);
        self.recompute_track_latencies();
    }

    /// Walk every `track_plugin_ids` entry, sum the plugin latencies into the
    /// matching `Track::reported_latency_samples`, and re-`sync_song_to_plugin_host`
    /// if anything changed. No-op when the totals already agree.
    fn recompute_track_latencies(&mut self) {
        let mut changed = false;
        for (track_id, plugin_ids) in &self.track_plugin_ids {
            let total: u32 = plugin_ids
                .iter()
                .map(|pid| self.plugin_latencies.get(pid).copied().unwrap_or(0))
                .sum();
            if let Some(track) = self.song.track_by_id_mut(*track_id)
                && track.reported_latency_samples != total
            {
                track.reported_latency_samples = total;
                changed = true;
            }
        }
        // Tracks with no loaded plugins should report 0 — clear any stale
        // value (e.g. the last plugin in a track was just removed).
        let track_ids_with_plugins: std::collections::HashSet<u32> =
            self.track_plugin_ids.keys().copied().collect();
        for track in &mut self.song.tracks {
            if !track_ids_with_plugins.contains(&track.id)
                && track.reported_latency_samples != 0
            {
                track.reported_latency_samples = 0;
                changed = true;
            }
        }
        if changed {
            // sync_song_to_plugin_host pushes the Song to daw_audio (the
            // schedule recompile happens inside `LocalState::refresh_schedule`
            // when it spots the new song Arc).
            self.sync_song_to_plugin_host();
        }
    }

    fn toggle_slot_gui(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        // PR2.1: plugin_host_windows / IPC は track_id ベース。
        let Some(track_idx) = self.cursor_track_index() else {
            return;
        };
        let track_id = self.song.tracks[track_idx].id;
        #[cfg(windows)]
        {
            if self.plugin_host_windows.contains_key(&(track_id, slot)) {
                self.send_plugin(MainToChild::CloseSlotGui { track: track_id, slot });
                return;
            }
            let label = self
                .song
                .tracks
                .get(track_idx)
                .and_then(|t| self.slot_ref_name(t, slot))
                .unwrap_or_else(|| "(unknown)".into());
            match crate::view::plugin_embed::PluginHostWindow::create(
                800,
                600,
                &format!("Plugin — {}", label),
            ) {
                Ok(win) => {
                    let hwnd = win.hwnd_u64();
                    self.plugin_host_windows.insert((track_id, slot), win);
                    self.send_plugin(MainToChild::OpenSlotGuiEmbedded {
                        track: track_id,
                        slot,
                        host_hwnd: hwnd,
                    });
                }
                Err(e) => tracing::error!(error = ?e, ?slot, "failed to create container"),
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (track_id, slot, slot_kind, slot_index);
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
        let track_idx = self.cursor_track_index().unwrap_or(0) as u32;
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

    /// PR4 sidechain: route a track's output into a plugin's `aux_in_port`.
    /// `source = None` disconnects. The plugin's
    /// `PluginInstance.sidechain_sources[port]` slot is created on demand;
    /// shorter vectors are extended with `None` placeholders so port `port`
    /// becomes addressable. After mutation we re-`sync_song_to_plugin_host`
    /// so `compile_schedule` regenerates the `SidechainTap` ops.
    fn set_sidechain_source(
        &mut self,
        track_id: u32,
        slot_kind: u8,
        slot_index: u32,
        port: u8,
        source: Option<u32>,
    ) {
        let Some(track) = self.song.track_by_id_mut(track_id) else {
            return;
        };
        let plugin = match slot_kind {
            0 => track.midi_fx_chain.get_mut(slot_index as usize),
            1 => track.instrument.as_mut(),
            _ => track.fx_chain.get_mut(slot_index as usize),
        };
        let Some(inst) = plugin else { return };
        let port_idx = port as usize;
        if inst.sidechain_sources.len() <= port_idx {
            inst.sidechain_sources.resize(port_idx + 1, None);
        }
        inst.sidechain_sources[port_idx] = source;
        self.sync_song_to_plugin_host();
    }

    fn remove_slot(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        let Some(track_idx) = self.cursor_track_index() else {
            return;
        };
        // PR2.1: send `Track::id` to the plugin host.
        let track_id = self.song.tracks[track_idx].id;
        self.send_plugin(MainToChild::RemoveSlotPlugin {
            track: track_id,
            slot,
        });
        // **GUI lifecycle**: plugin_host が plugin を destroy しても、
        // daw_gui 側の host HWND (`plugin_host_windows` の値) は自動で
        // 閉じない。 ここで drop することで `PluginHostWindow::Drop` が
        // `DestroyWindow` を呼んで容器ウィンドウを閉じる。 さらに
        // `Fx(slot_index)` を削除すると `Fx(slot_index+1..)` は 1 段
        // shift するので、 残った GUI window の key も再 mapping する。
        self.cleanup_slot_gui(track_id, slot);
        if let Some(track) = self.song.tracks.get_mut(track_idx) {
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

    /// `(track_id, slot)` の host window を破棄し、 同 track の
    /// 後続 Fx / MidiFx の key を 1 つずつ前にずらす (Vec::remove 後の
    /// chain index と整合させるため)。 Instrument は単一スロットなので
    /// shift 不要。 Windows 専用 (Linux では `plugin_host_windows` 自体
    /// を持たない)。
    #[cfg(windows)]
    fn cleanup_slot_gui(&mut self, track_id: u32, slot: PluginSlot) {
        // 対象スロットの host window を drop (= DestroyWindow)。
        self.plugin_host_windows.remove(&(track_id, slot));
        match slot {
            PluginSlot::Instrument => {}
            PluginSlot::Fx(removed_idx) => {
                self.shift_slot_gui_keys(track_id, removed_idx, true);
            }
            PluginSlot::MidiFx(removed_idx) => {
                self.shift_slot_gui_keys(track_id, removed_idx, false);
            }
        }
    }

    #[cfg(not(windows))]
    fn cleanup_slot_gui(&mut self, _track_id: u32, _slot: PluginSlot) {}

    /// Fx (`is_fx == true`) または MidiFx の index >= `removed_idx + 1`
    /// なエントリを 1 つずつ前にずらす。
    #[cfg(windows)]
    fn shift_slot_gui_keys(&mut self, track_id: u32, removed_idx: u32, is_fx: bool) {
        let mut moves: Vec<(PluginSlot, PluginSlot)> = Vec::new();
        for &(t, slot) in self.plugin_host_windows.keys() {
            if t != track_id {
                continue;
            }
            match (is_fx, slot) {
                (true, PluginSlot::Fx(i)) if i > removed_idx => {
                    moves.push((PluginSlot::Fx(i), PluginSlot::Fx(i - 1)));
                }
                (false, PluginSlot::MidiFx(i)) if i > removed_idx => {
                    moves.push((PluginSlot::MidiFx(i), PluginSlot::MidiFx(i - 1)));
                }
                _ => {}
            }
        }
        // 前方から順に move (低 index 側を先に詰める)。
        moves.sort_by_key(|(from, _)| match from {
            PluginSlot::Fx(i) | PluginSlot::MidiFx(i) => *i,
            PluginSlot::Instrument => 0,
        });
        for (from, to) in moves {
            if let Some(win) = self.plugin_host_windows.remove(&(track_id, from)) {
                self.plugin_host_windows.insert((track_id, to), win);
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

    /// BPM 入力欄を Enter で commit。 parse 成功なら 1.0..=400.0 に clamp して
    /// `song.bpm` に反映、 parse 失敗なら現値を維持。 どちらも edit_text を
    /// formatted な現値 (`"{:.1}"`) に書き戻して表示を整える。
    fn commit_bpm_edit(&mut self) {
        if let Ok(v) = self.bpm_edit_text.trim().parse::<f32>() {
            let clamped = v.clamp(1.0, 400.0);
            if (self.song.bpm - clamped).abs() > f32::EPSILON {
                self.song.bpm = clamped;
                self.sync_song_to_plugin_host();
            }
        }
        self.bpm_edit_text = format!("{:.1}", self.song.bpm);
    }

    /// time_sig numerator 入力欄を Enter で commit。 parse 成功なら 1..=32 に
    /// clamp、 失敗なら現値維持。 edit_text は現値の string 表現に書き戻す。
    fn commit_time_sig_num_edit(&mut self) {
        if let Ok(v) = self.time_sig_num_edit_text.trim().parse::<u8>() {
            let clamped = v.clamp(1, 32);
            if self.song.time_sig.0 != clamped {
                self.song.time_sig.0 = clamped;
                self.sync_song_to_plugin_host();
            }
        }
        self.time_sig_num_edit_text = self.song.time_sig.0.to_string();
    }

    /// time_sig denominator dropdown で選択された値を反映。 2/4/8/16 以外は無視。
    fn set_song_time_sig_denominator(&mut self, den: u8) {
        if !matches!(den, 2 | 4 | 8 | 16) {
            tracing::warn!(den, "ignoring invalid time_sig denominator");
            return;
        }
        if self.song.time_sig.1 != den {
            self.song.time_sig.1 = den;
            self.sync_song_to_plugin_host();
        }
    }

    /// `self.song` が外部要因 (open / new / undo / redo / autosave 復元 etc.) で
    /// 差し替わった後に、 transport 入力欄の表示文字列を現値に書き戻す。
    fn resync_song_edit_texts(&mut self) {
        self.bpm_edit_text = format!("{:.1}", self.song.bpm);
        self.time_sig_num_edit_text = self.song.time_sig.0.to_string();
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
        let Some(track_idx) = self.cursor_track_index() else {
            return;
        };
        // PR2.1: send `Track::id` to the plugin host (track_idx は
        // ローカルの song.tracks 操作のみで使う)。
        let track_id = self.song.tracks[track_idx].id;
        let target = self.plugin_picker_target;
        let dest_slot = match target {
            PickerTarget::Instrument => PluginSlot::Instrument,
            PickerTarget::Fx => {
                let next = self.song.tracks[track_idx].fx_chain.len() as u32;
                PluginSlot::Fx(next)
            }
            PickerTarget::MidiFx => {
                let next = self.song.tracks[track_idx].midi_fx_chain.len() as u32;
                PluginSlot::MidiFx(next)
            }
        };
        self.track_pending_load(track_id, dest_slot);
        self.send_plugin(MainToChild::SetSlotPlugin {
            track: track_id,
            slot: dest_slot,
            format: entry_format,
            path,
            plugin_id: entry_id.clone(),
            initial_state: None,
        });
        if let Some(track) = self.song.tracks.get_mut(track_idx) {
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

    fn begin_vocal_synth(&mut self) {
        let song = self.song.clone();
        let slot = Arc::clone(&self.synth_result);
        let cache_arc = Arc::clone(&self.voicevox_cache);
        let proxy = self.event_proxy.clone();
        let job = Arc::clone(&self.voicevox_job);
        // Lazy 起動の重複 spawn 防止フラグ。 1 度試行すれば以降は engine の
        // is_running 判定だけで分岐 (engine 落ちたユーザー再起動は手動で OK)。
        let need_launch = !self.voicevox_launch_attempted;
        self.voicevox_launch_attempted = true;
        std::thread::spawn(move || {
            // 1. Engine 起動 (まだ試行していなければ)
            if need_launch && !common::voicevox_engine::is_running() {
                if let Some(exe) = common::voicevox_engine::resolve_engine_path() {
                    tracing::info!(exe = %exe.display(), "lazy spawn VOICEVOX engine for synthesis");
                    match common::voicevox_engine::spawn_engine(&exe) {
                        Ok(child) => {
                            if let Err(e) = job.assign_std(&child) {
                                tracing::warn!(error = ?e, "failed to attach VOICEVOX to job");
                            }
                            // child を drop しても std::process::Child は wait
                            // しない (Windows)。 auto-kill は JobObject 経由。
                            std::mem::forget(child);
                        }
                        Err(e) => {
                            tracing::error!(error = ?e, exe = %exe.display(), "failed to spawn VOICEVOX engine");
                        }
                    }
                } else {
                    let cfg_hint = common::voicevox_engine::engine_path_config_file()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<no localappdata>".into());
                    tracing::warn!(
                        hint = %cfg_hint,
                        "VOICEVOX engine path not configured (set DAW_VOICEVOX_PATH or write the exe path to the config file)"
                    );
                }
            }
            // 2. Engine ready 待ち (60s timeout)
            if !common::voicevox_engine::wait_until_ready() {
                tracing::warn!(timeout_secs = 60, "VOICEVOX engine not ready, aborting synth");
                if let Ok(mut guard) = slot.lock() {
                    *guard = Vec::new();
                }
                proxy.send(AppEvent::VocalSynthCompleted);
                return;
            }
            // 3. Singers fetch (初回のみ意味あり、 2 回目以降も harmless)
            if let Ok(singers) = common::voicevox::fetch_singers() {
                tracing::info!(count = singers.len(), "fetched VOICEVOX singers");
                proxy.send(AppEvent::SingersLoaded(singers));
            }
            // 4. 合成 (既存パス)
            let results = match cache_arc.lock() {
                Ok(mut cache) => common::voicevox::synthesize_song(
                    &song,
                    common::voicevox::DEFAULT_SINGER_ID,
                    common::voicevox::DEFAULT_SINGER_ID,
                    &mut cache,
                ),
                Err(_) => {
                    tracing::error!("voicevox_cache mutex poisoned, skipping synth");
                    Vec::new()
                }
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = results;
            }
            proxy.send(AppEvent::VocalSynthCompleted);
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
                proxy.send(AppEvent::PluginDbRescanCompleted);
            }
            Err(e) => {
                tracing::error!(error = ?e, "plugin rescan failed");
                proxy.send(AppEvent::PluginDbRescanCompleted);
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
