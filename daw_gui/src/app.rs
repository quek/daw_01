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

use common::model::{
    AudioContent, AudioEvent, Clip, ClipContent, InstrumentSource, MidiContent, Note, Song, Track,
};
use common::plugin_db::PluginDatabase;
use common::plugin_format::PluginFormat;
use common::protocol::{MainToChild, PluginSlot, SlotState};
use tokio::sync::mpsc::UnboundedSender;

use crate::audio_source_cache::AudioSourceCache;
use crate::dispatcher::{BackgroundDispatcher, JobDispatcher};
use crate::import_audio;

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

/// Audio event 単位 field の inspector 表示用 read snapshot (Phase 2 PR1)。
/// `selected_clip` が `ClipContent::Audio` の clip を指していて、 中に
/// 少なくとも 1 event があれば `inspector_audio_event_summary()` が
/// `Some` を返す。 view (`track_inspector::draw`) はこれを使って
/// "Audio Event" section を出し、 toggle / dropdown 操作を `target`
/// に向けて発火する。 Phase 1 で 1 clip = 1 event 前提なので first event
/// を代表値として表示する。
#[derive(Debug, Clone, Copy)]
pub struct InspectorAudioEventSummary {
    /// 編集 AppEvent (`SetClipReversed` 等) の宛先 clip。
    pub target: ClipRef,
    pub reversed: bool,
    pub muted: bool,
    pub stretch_mode: common::model::StretchMode,
    pub fade_in_curve: common::model::FadeCurve,
    pub fade_out_curve: common::model::FadeCurve,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
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
    /// Decoded sample buffers for `Song.audio_sources`, keyed by
    /// `AudioSourceId`. Filled lazily on import (Phase 1 PR3). The
    /// audio engine maintains its own independent cache — file-backed
    /// sources are decoded twice (once per process) to keep IPC lean
    /// (`docs/plan_audio_clip.md` §6.1 / §8.3).
    pub audio_source_cache: AudioSourceCache,
    /// Snapped mouse hover beat inside the arrangement canvas. `None`
    /// outside the canvas. `arrangement_view::draw` updates it every
    /// frame using the current `SnapConfig`. Used by Split (E) so the
    /// split lands at the user's pointer (REAPER edit-cursor flavour)
    /// instead of the playhead (`docs/plan_audio_clip.md` §3.3).
    pub arrangement_hover_beat: Option<f64>,
    /// Same as above but **without** snap applied. Used by Alt+E
    /// (split with snap temporarily disabled).
    pub arrangement_hover_beat_raw: Option<f64>,
    /// `(track, clip)` index pair for the clip the mouse is currently
    /// over (or `None` outside any clip). Lets Split work without an
    /// explicit selection — hover over a clip, press `E`, and that
    /// clip is split. Falls back to the existing `selected_clips`
    /// when no clip is under the cursor.
    pub arrangement_hover_clip: Option<ClipRef>,

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
    /// `(track_id, PluginSlot)` → 現在 plugin_host に load されている
    /// plugin の情報。 Undo/Redo の reconcile (`reconcile_plugins_with_song`)
    /// で「Song の各 slot の plugin が host 側と一致しているか」 を slot
    /// 粒度で diff するために使う。 [`Self::track_plugin_ids`] が track
    /// 単位の plugin_id 集合だけを持つのに対し、 こちらは slot ごとの
    /// 詳細 (どの slot にどの plugin string id) まで track する。
    ///
    /// 更新タイミング: `SlotPluginLoaded` 受信時に insert、
    /// `SlotPluginUnloaded` 受信時に reverse-lookup retain、
    /// 削除系編集の `_inner` 関数内で track / slot 単位で remove。
    pub loaded_slots: std::collections::HashMap<(u32, PluginSlot), LoadedSlotInfo>,
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
    /// `Some(target)` で Audio Editor (= clip ダブルクリックで開く波形
    /// 編集 view) が開いている。 bottom_panel の Piano Roll タブが
    /// audio_editor view に切り替わる (`docs/plan_audio_clip.md` §3.10
    /// 「piano_roll の領域を流用」)。 `None` なら通常の Piano Roll が
    /// 表示される。 audio clip ダブルクリックで `Some` 化、 Esc / Audio
    /// Editor close で `None` に戻る。
    pub audio_editor_clip: Option<ClipRef>,
    /// Audio Editor で選択中の event index (`audio_editor_clip` の clip
    /// 内 events Vec への index)。 PR-D 段階 1 で導入: multi-event clip
    /// で個別 event を選択 → Duplicate / Delete / Inspector 編集の
    /// target にする。 `None` で「未選択」 (= clip 自体は開いてる)、
    /// `Some(0)` がデフォルト (= first event)。 audio_editor_clip が None
    /// になったら自動的に None (= editor 閉じたら selection も消える)。
    pub audio_editor_selected_event: Option<usize>,
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
    /// 現在 plugin_host へ `RequestAllStates` を投げて返答待ち中の理由。
    /// `None` の間は新規 request を発行できる。 `Some` の間に来た新規
    /// request は fallback (state 同期なし) で即時実行する。
    /// 詳細は [`PendingStateRequest`] / [`DeferredEdit`]。
    pub pending_state_request: Option<PendingStateRequest>,
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

    // ---- Audio event 数値 field 編集 buffer (Phase 2 PR2) ---------------
    /// 現 buffer がどの clip 用にロードされているか。 `selected_clip` が
    /// 変わったら view 側が `AppEvent::ResyncClipEditBuffers(target)` を
    /// 発火して `resync_clip_audio_event_edit_buffers` で再生成。 `None`
    /// は「未ロード」 (= 起動直後 / clip 未選択)。 編集 buffer の中身が
    /// この target の現値と整合する保証はないが (= ユーザー入力中はズレる)、
    /// commit / resync で必ず書き戻す。
    pub clip_edit_buffer_target: Option<ClipRef>,
    /// `AudioEvent.gain_db` 入力欄の編集中文字列 (`{:.1}` で format)。
    pub clip_gain_db_edit_text: String,
    /// `AudioEvent.pan` 入力欄の編集中文字列 (`{:.2}`、 -1.0 .. 1.0)。
    pub clip_pan_edit_text: String,
    /// `AudioEvent.pitch_semitones` 入力欄の編集中文字列 (`{:+.1}`、
    /// -96.0 .. 96.0、 Bitwig spec §3.6)。
    pub clip_pitch_edit_text: String,
    /// `AudioEvent.fade_in_beats` 入力欄の編集中文字列 (`{:.3}`、
    /// 0.0 .. clip.length_beats で clamp、 Bitwig spec §3.5)。
    pub clip_fade_in_edit_text: String,
    /// `AudioEvent.fade_out_beats` 入力欄の編集中文字列 (`{:.3}`、
    /// 0.0 .. clip.length_beats で clamp)。
    pub clip_fade_out_edit_text: String,

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
            audio_source_cache: AudioSourceCache::new(),
            arrangement_hover_beat: None,
            arrangement_hover_beat_raw: None,
            arrangement_hover_clip: None,
            selected_track_ids: Vec::new(),
            collapsed_groups: std::collections::HashSet::new(),
            track_plugin_ids: std::collections::HashMap::new(),
            loaded_slots: std::collections::HashMap::new(),
            plugin_latencies: std::collections::HashMap::new(),
            selected_clip: None,
            selected_clips: Vec::new(),
            selected_notes: Vec::new(),
            bottom_panel: 0,
            audio_editor_clip: None,
            audio_editor_selected_event: None,
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
            pending_state_request: None,
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
            clip_edit_buffer_target: None,
            clip_gain_db_edit_text: String::new(),
            clip_pan_edit_text: String::new(),
            clip_pitch_edit_text: String::new(),
            clip_fade_in_edit_text: String::new(),
            clip_fade_out_edit_text: String::new(),
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
        // PR4.5 diagnostic: if any chain plugin has a non-empty
        // sidechain_sources, log the resolved current_source values once
        // per inspector_chain rebuild. Helps catch UI ↔ model state
        // mismatches (= dropdown shows "—" but model has Some(id)).
        let any_wired = entries.iter().any(|e| e.current_source.is_some())
            || track
                .midi_fx_chain
                .iter()
                .chain(track.fx_chain.iter())
                .chain(track.instrument.as_ref())
                .any(|p| !p.sidechain_sources.is_empty());
        if any_wired {
            // Dump raw model state alongside entries so we can see the
            // exact values UI is displaying. trace! to avoid frame-rate
            // spam at default log levels; enable with RUST_LOG=trace.
            let raw: Vec<(u8, u32, String, Vec<Option<u32>>)> = track
                .midi_fx_chain
                .iter()
                .enumerate()
                .map(|(i, p)| (0u8, i as u32, p.plugin_id.clone(), p.sidechain_sources.clone()))
                .chain(track.instrument.as_ref().map(|p| (1u8, 0u32, p.plugin_id.clone(), p.sidechain_sources.clone())))
                .chain(track.fx_chain.iter().enumerate().map(|(i, p)| (2u8, i as u32, p.plugin_id.clone(), p.sidechain_sources.clone())))
                .collect();
            tracing::trace!(
                cursor_track_id = track.id,
                ?raw,
                ?entries,
                "sidechain_entries: rebuilt for cursor track"
            );
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

    /// Audio event field の inspector 表示用ライト read snapshot。
    /// 選択 clip (`selected_clip`) が `ClipContent::Audio` で、 中に少なくとも
    /// 1 event ある場合に `Some` を返す。 それ以外 (no selection / MIDI clip
    /// / Vocal clip / 空 events) は `None`。 Phase 1 では 1 clip 1 event 前提
    /// なので first event の field を「clip 全体の field」 として表示する。
    /// 編集 AppEvent (`SetClipReversed` / `SetClipMuted` / `SetClipStretchMode`)
    /// は全 event に同じ値を broadcast するので、 multi-event clip でも
    /// view は first event を「代表値」 として見せれば編集後に整合が取れる。
    pub fn inspector_audio_event_summary(&self) -> Option<InspectorAudioEventSummary> {
        let cref = self.selected_clip?;
        let track = self.song.tracks.get(cref.track as usize)?;
        let clip = track.clips.get(cref.clip as usize)?;
        let common::model::ClipContent::Audio(audio) =
            self.song.clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        // PR-D 段階 2: audio_editor が同じ clip を開いていて event を
        // 選択中なら、 そちらの event を Inspector の target にする。
        // multi-event clip でも個別 event を編集可能。 audio_editor が
        // 閉じている / 別 clip を開いている / 選択中 event idx が範囲外
        // なら first event (= Phase 2 PR1-3 と同じ既存挙動)。
        let event_idx = if self.audio_editor_clip == Some(cref) {
            self.audio_editor_selected_event.unwrap_or(0)
        } else {
            0
        };
        let event = audio.events.get(event_idx).or(audio.events.first())?;
        Some(InspectorAudioEventSummary {
            target: cref,
            reversed: event.reversed,
            muted: event.muted,
            stretch_mode: event.stretch_mode,
            fade_in_curve: event.fade_in_curve,
            fade_out_curve: event.fade_out_curve,
        })
    }

    /// PR-D 段階 2: Audio Editor の event 選択を `delta` (= +1 / -1) 分
    /// 進める / 戻す helper。 wrap-around (= 末尾 +1 で 0 に戻る、 0
    /// -1 で末尾)。 events が空 / audio_editor_clip が None のときは
    /// `None`、 1 event のときは Some(0) (= 動かない)。 root.rs から
    /// shortcut handler 経由で呼ばれて `SelectAudioEditorEvent` の
    /// 引数を組み立てる用。
    pub fn next_audio_editor_event_idx(&self, delta: i32) -> Option<usize> {
        let target = self.audio_editor_clip?;
        let track = self.song.tracks.get(target.track as usize)?;
        let clip = track.clips.get(target.clip as usize)?;
        let common::model::ClipContent::Audio(audio) =
            self.song.clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        let n = audio.events.len();
        if n == 0 {
            return None;
        }
        let cur = self.audio_editor_selected_event.unwrap_or(0).min(n - 1);
        let n_i = n as i32;
        let next = (cur as i32).wrapping_add(delta).rem_euclid(n_i);
        Some(next as usize)
    }

    /// PR-D 段階 2: set_clip_audio_event_* 系 helper の broadcast 範囲を
    /// 決める。 audio_editor が `target` clip を開いていて event を
    /// 選択中なら、 当該 event 1 つだけ更新 (= multi-event clip の個別
    /// 編集)。 そうでなければ全 event に broadcast (= Phase 2 PR1-3 の
    /// 既存挙動、 1 clip 1 event 前提なので broadcast = first event 編集)。
    /// 引数 `n_events` は当該 ClipContent::Audio の events 長 (= 呼び出し
    /// 前に immutable get で取得)。
    fn audio_event_target_indices(
        &self,
        target: ClipRef,
        n_events: usize,
    ) -> std::ops::Range<usize> {
        if self.audio_editor_clip == Some(target)
            && let Some(idx) = self.audio_editor_selected_event
            && idx < n_events
        {
            idx..(idx + 1)
        } else {
            0..n_events
        }
    }

    /// PR-D 段階 2 の集約 helper: `target` clip の `ClipContent::Audio`
    /// 内、 `audio_event_target_indices` で決まる範囲の event 群に
    /// closure `f` を適用 + sync。 audio_editor で個別 event 選択中なら
    /// その 1 つだけ、 そうでなければ全 event を更新する。 戻り値は
    /// 「実際に何らかの event を更新したか」 (= caller が edit buffer
    /// resync を呼ぶかの判断に使う)。
    fn mutate_audio_events_in_clip<F>(&mut self, target: ClipRef, mut f: F) -> bool
    where
        F: FnMut(&mut common::model::AudioEvent),
    {
        let Some(content_id) = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return false;
        };
        let n_events = match self.song.clip_contents.get(&content_id) {
            Some(common::model::ClipContent::Audio(a)) => a.events.len(),
            _ => return false,
        };
        let range = self.audio_event_target_indices(target, n_events);
        if range.is_empty() {
            return false;
        }
        if let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        {
            for event in &mut audio.events[range] {
                f(event);
            }
            self.sync_song_to_plugin_host();
            true
        } else {
            false
        }
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
        // Undo / Redo は plugin_host / audio engine の plugin
        // load 状態に直接 IPC を発行しないので、 ここで Song と
        // `track_plugin_ids` を diff して同期させる。 さもなければ
        // 「Bass track 削除 → Undo で track は復活するが plugin は
        // load されない (= 音が出ない)」 となる。
        self.reconcile_plugins_with_song();
        self.resync_song_edit_texts();
        self.pianoroll_notes_generation += 1;
    }

    /// `handle_event` の冒頭で push_undo_snapshot を auto する対象 event。
    ///
    /// **plugin が削除される event (`DeleteTrack` / `UngroupTracks` /
    /// `RemoveSlot`) はここに含めない**。 これらは dispatcher 側で
    /// `RequestAllStates` を経由してから push_undo_snapshot するため、
    /// auto push と重複してしまう。 plugin state 同期付き Undo の
    /// 詳細は [`PendingStateRequest`] / [`DeferredEdit`] を参照。
    fn is_undoable(event: &AppEvent) -> bool {
        matches!(
            event,
            AppEvent::New
                | AppEvent::AddVocalTrack
                | AppEvent::AddInstrumentTrack
                | AppEvent::GroupSelectedTracks { .. }
                | AppEvent::SetTrackParent { .. }
                | AppEvent::RemoveLastTrack
                | AppEvent::CommitRenameTrack
                | AppEvent::CreateClip { .. }
                | AppEvent::ResizeClip { .. }
                | AppEvent::DeleteSelectedClip
                | AppEvent::DuplicateClipShared { .. }
                | AppEvent::DuplicateClipUnique { .. }
                | AppEvent::CloneClipsLinked(_)
                | AppEvent::CloneClipsIndependent(_)
                | AppEvent::MakeClipUnique(_)
                | AppEvent::SplitClipAtPlayhead { .. }
                | AppEvent::GlueSelectedClips
                | AppEvent::SetClipReversed { .. }
                | AppEvent::SetClipMuted { .. }
                | AppEvent::SetClipStretchMode { .. }
                | AppEvent::CommitClipGainEdit
                | AppEvent::CommitClipPanEdit
                | AppEvent::CommitClipPitchEdit
                | AppEvent::SetClipGainDb { .. }
                | AppEvent::SetClipPan { .. }
                | AppEvent::SetClipPitchSemitones { .. }
                | AppEvent::CommitClipFadeInEdit
                | AppEvent::CommitClipFadeOutEdit
                | AppEvent::SetClipFadeInBeats { .. }
                | AppEvent::SetClipFadeOutBeats { .. }
                | AppEvent::SetClipFadeInCurve { .. }
                | AppEvent::SetClipFadeOutCurve { .. }
                | AppEvent::AutoFadeSelectedClips
                | AppEvent::AutoCrossfadeSelectedClips
                | AppEvent::ToggleClipReversed(_)
                | AppEvent::BounceClipInPlace(_)
                | AppEvent::SetClipGainDbBatch(_)
                | AppEvent::SetClipFadeBeatsBatch(_)
                | AppEvent::SetClipFadeCurveBatch(_)
                | AppEvent::DuplicateAudioEditorEvent
                | AppEvent::SetAudioEventStart { .. }
                | AppEvent::SetAudioEventTrim { .. }
                | AppEvent::AddAudioEventFromFile { .. }
                | AppEvent::DeleteAudioEvent { .. }
                | AppEvent::ImportAudio { .. }
                | AppEvent::AddNote { .. }
                | AppEvent::ResizeNote { .. }
                | AppEvent::ResizeNotes(_)
                | AppEvent::SetNotePositions(_)
                | AppEvent::DeleteSelectedNotes
                | AppEvent::SetNoteLyrics { .. }
                | AppEvent::SetNoteVelocities(_)
                | AppEvent::SetTrackSpeaker { .. }
                | AppEvent::QuantizeSelectedNotes(_)
                | AppEvent::SelectPluginFromDb(_)
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
        let notes = self.song.clip_notes(clip);
        let mut copied: Vec<Note> = self
            .selected_notes
            .iter()
            .filter_map(|i| notes.get(*i as usize).cloned())
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
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        let mut new_indices = Vec::with_capacity(clipboard.len());
        for src in &clipboard {
            let mut n = src.clone();
            n.start_beat += anchor;
            new_indices.push(notes.len() as u32);
            notes.push(n);
        }
        self.selected_notes = new_indices;
        self.sync_song_to_plugin_host();
        self.status_message = format!("貼り付け: {count} ノート");
    }

    fn set_note_velocity(&mut self, note_idx: u32, velocity: u8) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        let Some(note) = notes.get_mut(note_idx as usize) else {
            return;
        };
        note.velocity = velocity;
        self.sync_song_to_plugin_host();
    }

    /// gui_01 #018 (M14 Phase 64): velocity lane drag の release frame で
    /// 1 batch 発行される `(note_id, new_velocity)` 列を一括適用。 widget
    /// から渡される id は piano_roll widget 上の `NoteId` (= clip 内 note
    /// index に同じ値域、 daw_01 でも u32)。 1 batch を 1 Undo step とする
    /// ため、 push_undo_snapshot は handle_event の auto push 経路に任せる
    /// (`is_undoable` で `SetNoteVelocities` を許可)。 sync_song_to_plugin_host
    /// は最後に 1 度だけ呼ぶ (毎 note 同期は無駄)。
    fn set_note_velocities(&mut self, updates: &[(u32, u8)]) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        let mut changed = false;
        for (note_idx, vel) in updates {
            if let Some(note) = notes.get_mut(*note_idx as usize) {
                note.velocity = *vel;
                changed = true;
            }
        }
        if changed {
            self.sync_song_to_plugin_host();
        }
    }

    fn quantize_selected_notes(&mut self, div: u8) {
        let Some(r) = self.selected_clip else {
            return;
        };
        let div = div.max(1) as f64;
        let snap = |b: f64| (b * div).round() / div;
        let selected = self.selected_notes.clone();
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        for &i in &selected {
            if let Some(n) = notes.get_mut(i as usize) {
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
/// `loaded_slots` の値: 1 つの (track, slot) ペアに対する load 情報。
#[derive(Debug, Clone)]
pub struct LoadedSlotInfo {
    /// session-unique numeric plugin id (= plugin_host が割り当てる u32)。
    pub plugin_id: u32,
    /// stable string id (= `PluginInstance::plugin_id` と同じ値)。
    /// reconcile の slot-level diff で「Song と host で同じ plugin が
    /// 居るか」 を判定するキー。
    pub plugin_id_str: String,
}

/// `RequestAllStates` の発行理由。 plugin_host から `AllPluginStates`
/// が返ってくるまで [`AppData::pending_state_request`] に保持し、
/// 受信時に対応する完了処理 (save または deferred edit) を実行する。
///
/// 「同時に複数の理由」 は持てない (`Option<PendingStateRequest>` が
/// 1 つのため)。 すでに `Some` のときに来た新規 request は state 同期
/// 無しで即時実行する fallback ロジックが各 dispatcher 側にある。
#[derive(Debug, Clone)]
pub enum PendingStateRequest {
    /// project save。 ファイル書き出し完了で消費される。
    Save { path: PathBuf },
    /// plugin が **削除される** 編集操作の Undo snapshot 作成。
    /// state を Song に書き込んでから [`AppData::push_undo_snapshot`]
    /// を呼ぶことで、 削除直前の knob 値等を Undo で復元できる。
    Deferred(DeferredEdit),
}

/// state 取得が完了したあとに plugin-main thread へ実行させる編集。
/// track index ではなく **stable な `track_id`** で持つので、 pending
/// 中に他の編集が track の Vec position をずらしても整合性が保たれる。
#[derive(Debug, Clone)]
pub enum DeferredEdit {
    DeleteTrack { track_id: u32 },
    UngroupTracks { track_ids: Vec<u32> },
    RemoveSlot { track_id: u32, slot: PluginSlot },
}

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
    /// gui_01 #018 (M14 Phase 64): velocity lane drag で 1 batch 更新。
    /// `selected_clip` の note を `(id, velocity)` で一括書き換え。 1 drag =
    /// 1 Undo step。
    SetNoteVelocities(Vec<(u32, u8)>),
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
    /// Clip の右端 trim (= `start_beat` 同値、 `length_beats` のみ更新) と
    /// 左端 trim (= `start_beat` を進めて `length_beats` を縮める) の両方を
    /// カバー。 audio clip の場合は handler が delta_start を計算して各
    /// `AudioEvent.event_start_in_clip_beats` / `source_start_frames` /
    /// `event_length_beats` を追従させる (Bitwig spec §3.2)。 gui_01
    /// `ResizeClipDelta` の `next_start` / `next_len` 両方をそのまま流す。
    ResizeClip {
        target: ClipRef,
        start_beat: f64,
        length: f64,
    },
    /// `(source_ref, to_track_id, next_start_beat)` のタプル列。
    /// to_track_id == source の track id なら同 track 内 move、 違えば
    /// track 跨ぎ move (clip 自体を別 track の `clips: Vec<Clip>` に移す)。
    SetClipPositions(Vec<(ClipRef, u32, f64)>),
    CreateClip { track: u32, start_beat: f64 },
    DeleteSelectedClip,
    /// 選択中 clip の末尾直後に共有コピー (linked clip) を生成 (D shortcut /
    /// `docs/plan_clip_share_clone.md` §3.2)。 source の `content_id` を
    /// そのまま新 clip にコピー。
    DuplicateClipShared { source: ClipRef },
    /// 選択中 clip の末尾直後に独立コピー (notes を deep clone + 新 ContentId)
    /// を生成 (Alt+D shortcut / §3.3)。
    DuplicateClipUnique { source: ClipRef },
    /// arrangement Ctrl+drag → release の結果。 各 entry は `(source ClipRef,
    /// to_track_id, drop_start_beat)` (snap 済み)、 元 clip は残し、 drop 位置に
    /// 共有コピー を to_track 上で生成。 (§3.4)
    CloneClipsLinked(Vec<(ClipRef, u32, f64)>),
    /// arrangement Ctrl+Shift+drag → release。 同上だが content は deep clone
    /// + 新 ContentId 採番で独立化。 (§3.5)
    CloneClipsIndependent(Vec<(ClipRef, u32, f64)>),
    /// 右クリック「Make Unique」 — 共有 clip を独立化。 refcount==1 の場合は
    /// no-op (§3.6)。
    MakeClipUnique(ClipRef),

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
    /// plugin_host で `SetSlotPlugin` の load が失敗した通知。
    /// `pending_plugin_loads` から該当 entry を解放し、 status_message に
    /// エラー表示、 `pending_play` が立っていれば flush する。
    SlotPluginLoadFailedFromChild {
        track: u32,
        slot: PluginSlot,
        plugin_id: String,
        reason: String,
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

    // -------- Audio clip import (Phase 1 PR3) ----------------------------
    /// Import one or more audio files into the song. Triggered by
    /// `arrangement` drag&drop and the File → Import Audio menu (PR3).
    /// The handler decodes each file (Phase 1: synchronous + WAV-only,
    /// `docs/plan_audio_clip.md` §7), copies it into
    /// `<project_dir>/samples/<basename>_<hash>.<ext>` (or the unsaved-
    /// project import_cache as fallback), registers an `AudioSource`,
    /// stashes the decoded buffer in `audio_source_cache`, and creates
    /// an audio clip on the first track at the current playhead.
    /// Phase 2 moves decode to a background thread so large WAVs (up
    /// to 4 GB §7.2) don't block the UI.
    ImportAudio { paths: Vec<PathBuf> },

    /// File menu → "Import Audio..." entry. Opens an `rfd` file picker
    /// (multi-select, WAV filter), then forwards the chosen paths to
    /// `AppEvent::ImportAudio`. The dialog itself is `rfd`'s native
    /// modal so we don't need our own ui state. `docs/plan_audio_clip.md`
    /// §3.1 — File menu からの import 経路。
    OpenImportAudioDialog,

    // -------- Split / Glue (Phase 1 PR7) -----------------------------------
    /// Split clip(s) at the **mouse cursor** (= `AppData
    /// .arrangement_hover_beat` snapped, or `_raw` when `snap == false`
    /// for the Alt+E variant). Falls back to the playhead when the
    /// cursor is outside the arrangement canvas. Operates on the clip
    /// the cursor is hovering over; if there is no hovered clip,
    /// falls back to `selected_clips`. Works on MIDI / Audio / Vocal
    /// clips alike (`docs/plan_audio_clip.md` §3.3.1): the back half
    /// gets a freshly-allocated `ContentId` and `notes` / `events` are
    /// partitioned by the split beat. Bound to `E` (snap on) and
    /// `Alt+E` (snap off).
    SplitClipAtPlayhead { snap: bool },

    /// Glue (Consolidate) the currently selected clips into a single
    /// clip per track. All clips must be the same kind (MIDI / Audio
    /// / Vocal) — mixed-kind selections are rejected with a status
    /// message (§3.3.2). Result clip spans `min(start_beat) .. max(end
    /// _beat)` and inherits a fresh `ContentId` carrying every event /
    /// note from the source clips with offsets re-aligned to the new
    /// clip start. Gaps between clips become silent ranges. Bound to `J`.
    GlueSelectedClips,

    // -------- Audio event field edits (Phase 2 PR1) ------------------------
    /// Toggle `AudioEvent.reversed` for every event in the selected
    /// audio clip. Non-audio clips no-op. `docs/plan_audio_clip.md`
    /// §3.8: Reverse は destructive ではなく、 再生時に source を逆方向
    /// 走査する flag。
    SetClipReversed { target: ClipRef, reversed: bool },

    /// Toggle `AudioEvent.muted` for every event in the selected audio
    /// clip. Mute は event 単位の silent flag (§3.7 / §3.9 AudioEvent
    /// 選択時 Mute toggle)、 track-mute とは独立。 Phase 1 では 1 clip 1
    /// event 前提なので「event mute = clip mute」 と同義。
    SetClipMuted { target: ClipRef, muted: bool },

    /// Set `AudioEvent.stretch_mode` for every event in the selected
    /// audio clip. Phase 1 で再生に効くのは `Raw` / `Repitch` のみ;
    /// `Stretch` / `Slice` は §3.7 に従って Raw 同等で再生される
    /// (Phase 3+ で本実装)。
    SetClipStretchMode { target: ClipRef, mode: common::model::StretchMode },

    // ---- Audio event 数値 field 編集 (Phase 2 PR2) ----------------------
    /// Inspector の `target` clip 用 edit buffer (gain / pan / pitch) を
    /// `target` の現値で再生成する。 `selected_clip` 切替や Undo/Redo /
    /// open / new で発火。 view 側でも buffer の target が現選択と
    /// 違ければ `Edit::mutate` で同 AppEvent を push する。 buffer の
    /// 中身を「現値の formatted 文字列」 に書き戻す純 sync 操作なので
    /// `is_undoable` ではない。
    ResyncClipEditBuffers(ClipRef),

    /// text_input が 1 文字毎に発行する change event。 buffer 文字列を
    /// 受け取って `clip_*_edit_text` に書き込むだけ (parse / commit はしない)。
    /// `is_undoable` ではない (= 連続 typing で undo step を量産しない、
    /// commit 系を 1 step とする)。
    ClipGainEditChanged(String),
    ClipPanEditChanged(String),
    ClipPitchEditChanged(String),

    /// text_input commit (Enter / focus 喪失) で `clip_*_edit_text` を
    /// parse して該当 field を更新、 失敗時は buffer を現値に書き戻す。
    /// `is_undoable` (= 1 commit = 1 Undo step)。 既存 `CommitBpmEdit` と
    /// 同じパターン。
    CommitClipGainEdit,
    CommitClipPanEdit,
    CommitClipPitchEdit,

    /// Programmatic な field 設定 (Inspector の commit から呼ばれる /
    /// JS test API 経由 / 将来の knob drag からも)。 全 event に
    /// broadcast (`SetClipReversed` 等と同じ semantics)。
    SetClipGainDb { target: ClipRef, gain_db: f32 },
    SetClipPan { target: ClipRef, pan: f32 },
    SetClipPitchSemitones { target: ClipRef, semitones: f32 },

    // ---- Audio event fade 編集 (Phase 2 PR3) ----------------------------
    /// Fade In length (beats) 入力欄の per-character 更新 / commit。
    /// `Clip*EditChanged` 系と同じ pattern: per-character は非 undoable、
    /// Commit (Enter / focus 喪失) で parse + clamp + 全 event broadcast、
    /// undo step を消費する。
    ClipFadeInEditChanged(String),
    ClipFadeOutEditChanged(String),
    CommitClipFadeInEdit,
    CommitClipFadeOutEdit,

    /// Fade length / curve の programmatic 設定。 `SetClipGainDb` 等と
    /// 同じ semantics で全 event に broadcast、 値は clip.length_beats
    /// で clamp (= fade が clip より長くならない)。 curve は spec §3.5
    /// の Linear / Exponential / SCurve から選択 (Inspector dropdown 経由)。
    SetClipFadeInBeats { target: ClipRef, beats: f64 },
    SetClipFadeOutBeats { target: ClipRef, beats: f64 },
    SetClipFadeInCurve { target: ClipRef, curve: common::model::FadeCurve },
    SetClipFadeOutCurve { target: ClipRef, curve: common::model::FadeCurve },

    // ---- Auto-Fade / Auto-Crossfade (Phase 2 PR5) -----------------------
    /// 全選択 audio clip に短 (≒4 ms 相当) fade を一括適用 (`docs
    /// /plan_audio_clip.md` §3.5)。 既存 fade 値は上書き。 fade 長は
    /// `0.004 * bpm / 60` beats = 4 ms 相当 (業界標準のクリック除去
    /// 用 short fade)。
    AutoFadeSelectedClips,

    /// 隣接 audio clip 間で重なり区間に crossfade を作成 (= 前 clip の
    /// 末尾 fade_out + 次 clip の先頭 fade_in を overlap 長で揃える、
    /// `docs/plan_audio_clip.md` §3.5)。 同 track 内の clip 群を
    /// start_beat 順に sort し、 ペアごとに `prev.start + prev.length >
    /// next.start` を判定 → overlap_beats を両 fade に設定。 隙間がある
    /// (= overlap が無い) ペアは no-op。
    AutoCrossfadeSelectedClips,

    // ---- Audio Editor (Phase 2 PR6, `docs/plan_audio_clip.md` §3.10) ---
    /// audio clip ダブルクリックで Audio Editor を開く。
    /// `audio_editor_clip = Some(target)` + bottom_panel を tab 1
    /// (Piano Roll 切替先) に切り替え。 ClipContent::Audio 以外を渡された
    /// 場合は no-op (status_message 出さず silent skip)。
    OpenAudioEditor(ClipRef),

    /// Audio Editor を閉じる (Esc shortcut / 切替操作経由)。
    /// `audio_editor_clip = None` に戻して bottom_panel は現在のタブ
    /// (Piano Roll) を維持。
    CloseAudioEditor,

    /// `target` clip の first event の `reversed` を反転 (= 右クリック
    /// メニュー「Reverse」 用 toggle、 `docs/plan_audio_clip.md` §3.8)。
    /// Inspector でも同 field は編集できるが、 メニューから 1 操作で
    /// 切り替えられる UX を提供。 内部的には現値を読んで
    /// `SetClipReversed` を呼ぶのと等価で、 全 event に broadcast。
    ToggleClipReversed(ClipRef),

    /// Bounce In Place (Pre-FX、 `docs/plan_audio_clip.md` §3.8)。
    /// `target` clip 内の全 events を offline mix して 1 つの WAV
    /// (stereo 32-bit float) に書き出し、 新 `AudioSource` を採番して
    /// Song.audio_sources に追加、 `ClipContent::Audio { events: [新
    /// 1 event] }` に置換する。 Pre-FX = plugin chain (instrument /
    /// fx_chain) を通さない、 source の events を mix しただけの
    /// snapshot。 同 ContentId を共有していた linked clip も同じ新
    /// content に置換される (= 既存 ContentId を上書き)。
    BounceClipInPlace(ClipRef),

    // ---- multi-clip drag batch (Phase 2 PR-B) ---------------------------
    /// gui_01 widget が multi-clip 一括 drag (= dB / fade / curve) を 1
    /// release で発行する場合、 各 delta を 1 AppEvent にまとめて 1
    /// Undo step とする。 delta 数だけ単発 AppEvent を撃つと Undo step
    /// が分散してしまう (Phase 2 PR-B、 `docs/plan_audio_followup.md` §PR-B)。
    /// 単発 `SetClipGainDb` 等は Inspector commit 経路で引き続き使用。
    SetClipGainDbBatch(Vec<(ClipRef, f32)>),
    /// `(target, edge, beats)` 列で fade length を一括設定。
    SetClipFadeBeatsBatch(Vec<(ClipRef, FadeEdgeKind, f64)>),
    /// `(target, edge, curve)` 列で fade curve を一括設定。
    SetClipFadeCurveBatch(Vec<(ClipRef, FadeEdgeKind, common::model::FadeCurve)>),

    // ---- Audio Editor event 単位編集 (Phase 2 PR-D 段階 1) -----------
    /// Audio Editor 内で event index を選択 (= clip 内 events Vec の
    /// index)。 `None` で選択解除。 `audio_editor_clip` が `None` の
    /// ときは no-op。 view state なので非 undoable。
    SelectAudioEditorEvent(Option<usize>),

    /// 現在 Audio Editor で開いている clip + 選択中 event を Duplicate
    /// (= 同 source の event を直後に複製)。 spec §3.10.2 の `Ctrl+D`
    /// 動作。 `audio_editor_clip` / `audio_editor_selected_event` が
    /// `Some` でないと no-op。 新 event は元 event の右隣 (= clip 内
    /// 位置 = `src.event_start_in_clip_beats + src.event_length_beats`)、
    /// 同 source + 同パラメータ。 clip.length_beats が足りなければ自動
    /// で伸ばす。 selection は新 event に移る。
    DuplicateAudioEditorEvent,

    // ---- Audio Editor event 単位編集 (Phase 2 PR-D 段階 3) -----------
    /// Audio Editor で event の clip 内 start position を変更
    /// (= 中央 drag 移動)。 `clip` の `event_idx` 番目の event の
    /// `event_start_in_clip_beats` を `new_start_beats` (clamp 0..) に
    /// 設定。 範囲外 / 非 audio clip / event_idx 範囲外なら no-op。
    /// clip.length_beats は新 event の終端を含むよう自動拡張。
    SetAudioEventStart {
        clip: ClipRef,
        event_idx: usize,
        new_start_beats: f64,
    },
    /// Audio Editor で event 端 trim (= 左右端 drag)。 `side == Left`
    /// なら `event_start_in_clip_beats` + `event_length_beats` +
    /// `source_start_frames` を delta で連動更新、 `side == Right` なら
    /// `event_length_beats` + `source_end_frames` を更新。 source は
    /// `audio_sources` から sample_rate を取って delta_beats → frames
    /// 変換。 clip.length_beats は必要に応じて拡張。
    SetAudioEventTrim {
        clip: ClipRef,
        event_idx: usize,
        side: AudioEventTrimSide,
        delta_beats: f64,
    },
    /// Audio Editor の空白領域に file system drag&drop された path を
    /// decode + import し、 既存 audio clip の content に新 event として
    /// `position_in_clip_beats` の位置に追加。 source 採番 + buffer cache
    /// 登録は `import_audio::import_one` 経由 (= top-level Import Audio
    /// と同 pipeline)。 失敗時は status_message にエラー、 selection は
    /// 新 event に移す。 clip.length_beats は必要に応じて拡張。
    AddAudioEventFromFile {
        clip: ClipRef,
        path: PathBuf,
        position_in_clip_beats: f64,
    },
    /// Audio Editor で event を削除 (= Delete key / context menu)。
    /// `clip` の `event_idx` 番目を `events.remove`。 残 event 0 個に
    /// なっても content は保持 (= clip の placeholder)。 selection は
    /// `event_idx` を最大に詰める (events 空なら None)。
    DeleteAudioEvent {
        clip: ClipRef,
        event_idx: usize,
    },
}

/// `*Batch` 系 AppEvent で fade in / out を区別するための marker。
/// `daw_ui_core::FadeEdge` は widget 側 type で daw_01 model 側 enum
/// に直接置けないので、 AppEvent module 内に再定義 (= bincode 経由は
/// 不要なので common::model に追加する必要なし)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeEdgeKind {
    In,
    Out,
}

/// Audio Editor の event trim 側 (左端 / 右端) marker。 `SetAudioEventTrim`
/// AppEvent 用。 left = (event_start, source_start) 連動、 right =
/// (event_length, source_end) 連動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEventTrimSide {
    Left,
    Right,
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
            AppEvent::SetNoteVelocities(updates) => {
                self.set_note_velocities(&updates);
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
            AppEvent::ResizeClip {
                target,
                start_beat,
                length,
            } => {
                self.resize_clip(target, start_beat, length);
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
            AppEvent::SlotPluginLoadFailedFromChild {
                track,
                slot,
                plugin_id,
                reason,
            } => {
                self.on_plugin_load_failed_from_child(track, slot, plugin_id, reason);
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
            AppEvent::ImportAudio { paths } => {
                self.action_import_audio(paths);
            }
            AppEvent::OpenImportAudioDialog => {
                self.action_open_import_audio_dialog();
            }
            AppEvent::SetClipReversed { target, reversed } => {
                self.set_clip_audio_event_reversed(target, reversed);
            }
            AppEvent::SetClipMuted { target, muted } => {
                self.set_clip_audio_event_muted(target, muted);
            }
            AppEvent::SetClipStretchMode { target, mode } => {
                self.set_clip_audio_event_stretch_mode(target, mode);
            }
            AppEvent::ResyncClipEditBuffers(target) => {
                self.resync_clip_audio_event_edit_buffers(target);
            }
            AppEvent::ClipGainEditChanged(s) => {
                self.clip_gain_db_edit_text = s;
            }
            AppEvent::ClipPanEditChanged(s) => {
                self.clip_pan_edit_text = s;
            }
            AppEvent::ClipPitchEditChanged(s) => {
                self.clip_pitch_edit_text = s;
            }
            AppEvent::CommitClipGainEdit => {
                self.commit_clip_gain_edit();
            }
            AppEvent::CommitClipPanEdit => {
                self.commit_clip_pan_edit();
            }
            AppEvent::CommitClipPitchEdit => {
                self.commit_clip_pitch_edit();
            }
            AppEvent::SetClipGainDb { target, gain_db } => {
                self.set_clip_audio_event_gain_db(target, gain_db);
            }
            AppEvent::SetClipPan { target, pan } => {
                self.set_clip_audio_event_pan(target, pan);
            }
            AppEvent::SetClipPitchSemitones { target, semitones } => {
                self.set_clip_audio_event_pitch_semitones(target, semitones);
            }
            AppEvent::ClipFadeInEditChanged(s) => {
                self.clip_fade_in_edit_text = s;
            }
            AppEvent::ClipFadeOutEditChanged(s) => {
                self.clip_fade_out_edit_text = s;
            }
            AppEvent::CommitClipFadeInEdit => {
                self.commit_clip_fade_in_edit();
            }
            AppEvent::CommitClipFadeOutEdit => {
                self.commit_clip_fade_out_edit();
            }
            AppEvent::SetClipFadeInBeats { target, beats } => {
                self.set_clip_audio_event_fade_in_beats(target, beats);
            }
            AppEvent::SetClipFadeOutBeats { target, beats } => {
                self.set_clip_audio_event_fade_out_beats(target, beats);
            }
            AppEvent::SetClipFadeInCurve { target, curve } => {
                self.set_clip_audio_event_fade_in_curve(target, curve);
            }
            AppEvent::SetClipFadeOutCurve { target, curve } => {
                self.set_clip_audio_event_fade_out_curve(target, curve);
            }
            AppEvent::AutoFadeSelectedClips => {
                self.auto_fade_selected_clips();
            }
            AppEvent::AutoCrossfadeSelectedClips => {
                self.auto_crossfade_selected_clips();
            }
            AppEvent::OpenAudioEditor(target) => {
                self.open_audio_editor(target);
            }
            AppEvent::CloseAudioEditor => {
                self.close_audio_editor();
            }
            AppEvent::SelectAudioEditorEvent(idx) => {
                self.audio_editor_selected_event = idx;
            }
            AppEvent::DuplicateAudioEditorEvent => {
                self.duplicate_audio_editor_event();
            }
            AppEvent::SetAudioEventStart { clip, event_idx, new_start_beats } => {
                self.set_audio_event_start(clip, event_idx, new_start_beats);
            }
            AppEvent::SetAudioEventTrim { clip, event_idx, side, delta_beats } => {
                self.set_audio_event_trim(clip, event_idx, side, delta_beats);
            }
            AppEvent::AddAudioEventFromFile { clip, path, position_in_clip_beats } => {
                self.add_audio_event_from_file(clip, path, position_in_clip_beats);
            }
            AppEvent::DeleteAudioEvent { clip, event_idx } => {
                self.delete_audio_event(clip, event_idx);
            }
            AppEvent::ToggleClipReversed(target) => {
                let cur = self.is_clip_audio_event_reversed(target);
                self.set_clip_audio_event_reversed(target, !cur);
            }
            AppEvent::BounceClipInPlace(target) => {
                self.bounce_clip_in_place(target);
            }
            AppEvent::SetClipGainDbBatch(entries) => {
                for (target, gain_db) in &entries {
                    self.set_clip_audio_event_gain_db(*target, *gain_db);
                }
            }
            AppEvent::SetClipFadeBeatsBatch(entries) => {
                for (target, edge, beats) in &entries {
                    match edge {
                        FadeEdgeKind::In => {
                            self.set_clip_audio_event_fade_in_beats(*target, *beats);
                        }
                        FadeEdgeKind::Out => {
                            self.set_clip_audio_event_fade_out_beats(*target, *beats);
                        }
                    }
                }
            }
            AppEvent::SetClipFadeCurveBatch(entries) => {
                for (target, edge, curve) in &entries {
                    match edge {
                        FadeEdgeKind::In => {
                            self.set_clip_audio_event_fade_in_curve(*target, *curve);
                        }
                        FadeEdgeKind::Out => {
                            self.set_clip_audio_event_fade_out_curve(*target, *curve);
                        }
                    }
                }
            }
            AppEvent::SplitClipAtPlayhead { snap } => {
                self.action_split_clips_at_cursor(snap);
            }
            AppEvent::GlueSelectedClips => {
                self.action_glue_selected_clips();
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
            AppEvent::DuplicateClipShared { source } => {
                self.duplicate_clip_shared(source);
            }
            AppEvent::DuplicateClipUnique { source } => {
                self.duplicate_clip_unique(source);
            }
            AppEvent::CloneClipsLinked(entries) => {
                self.clone_clips_linked(&entries);
            }
            AppEvent::CloneClipsIndependent(entries) => {
                self.clone_clips_independent(&entries);
            }
            AppEvent::MakeClipUnique(target) => {
                self.make_clip_unique(target);
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
        // PR6: project_dir も送る (audio engine は AudioSourcePath::
        // ProjectRelative を解決するために必要、 §9.2)。 send_audio は
        // 順序保証付きの IPC なので SetProjectDir → LoadSong の順で
        // 送れば audio side の LoadSong handler 内で project_dir が
        // 既に最新になっている。
        let project_dir: Option<PathBuf> = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        self.send_audio(MainToChild::SetProjectDir(project_dir));
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

    /// Undo / Redo 後に呼んで、 `Song.tracks` と plugin_host の load
    /// 状態を **slot 粒度で** diff し、 必要な IPC を発行して両者を
    /// 再同期する。
    ///
    /// Undo / Redo は `Song` の clone 入れ替えだけ行うので、 plugin_host
    /// と audio engine 側の load 状態は元に戻らない。 そのまま放置すると
    /// 「track 削除 → Undo で track 復活 → plugin が host に load されて
    /// いないので音が鳴らない」「FX 1 個追加 → Undo でも host にその FX
    /// が残り続ける」 等の UX バグになる。
    ///
    /// Phase A (stale tracks remove): `loaded_slots` にあるが
    /// `Song.tracks` には居ない `track_id` を、 `delete_track` と同じ
    /// IPC 順 (audio に `ClosePluginShmem` 先送り → plugin_host に
    /// `RemoveTrack`) で破棄する。 Redo が track 削除を進めた場合に
    /// 発動する。
    ///
    /// Phase B (per-slot diff): `Song.tracks` の各 track について
    /// [`AppData::loaded_slots`] と「Song の各 `(slot, plugin_id_str)`」
    /// を比較する。 host にあるが Song に無い slot は `RemoveSlotPlugin`、
    /// Song にあるが host に無い slot もしくは host にあるが
    /// `plugin_id_str` が違う slot は `SetSlotPlugin`。 plugin_host の
    /// SetSlotPlugin handler は同 plugin_id を同 slot に置く dedup logic
    /// を持つので、 一致 slot に改めて送信しても no-op
    /// (`SlotPluginLoaded` を再 emit するだけ)。
    ///
    /// plugin の **state** は `Song.PluginInstance::state` を
    /// `initial_state` として渡す。 直前 commit で push_undo_snapshot 前に
    /// `RequestAllStates` で最新 state を Song に書き戻しているので、
    /// 削除直前の knob 値も Undo で復元される。
    fn reconcile_plugins_with_song(&mut self) {
        // Phase A: Song に無い track を host から消す。 `loaded_slots` に
        // 1 つでも残っている track id (= host 側 plugin chain がまだ
        // ある) を見れば判定できる。
        let song_track_ids: std::collections::HashSet<u32> =
            self.song.tracks.iter().map(|t| t.id).collect();
        let stale_track_ids: std::collections::HashSet<u32> = self
            .loaded_slots
            .keys()
            .map(|(tid, _)| *tid)
            .filter(|tid| !song_track_ids.contains(tid))
            .collect();
        if !stale_track_ids.is_empty() {
            tracing::info!(
                ?stale_track_ids,
                "reconcile: removing stale tracks from plugin host"
            );
        }
        for track_id in stale_track_ids {
            // `delete_track` と同じ IPC 順序: audio engine に
            // ClosePluginShmem を先送りしてから plugin_host に
            // RemoveTrack。
            if let Some(plugin_ids) = self.track_plugin_ids.remove(&track_id) {
                for pid in plugin_ids {
                    self.send_audio(MainToChild::ClosePluginShmem { plugin_id: pid });
                }
            }
            self.send_plugin(MainToChild::RemoveTrack { track: track_id });
            // host から消す track の pending load / GUI window / slot
            // cache も掃除。
            self.pending_plugin_loads.retain(|(t, _)| *t != track_id);
            self.loaded_slots.retain(|(t, _), _| *t != track_id);
            #[cfg(windows)]
            {
                self.plugin_host_windows.retain(|&(t, _), _| t != track_id);
            }
        }

        // Phase B: 各 track について Song の slot 列と host の slot 列
        // (= `loaded_slots` の対応 entries) を diff する。
        let song = self.song.clone();
        let Some(db) = self.plugin_db.clone() else {
            // plugin DB が未ロードなら SetSlotPlugin の組み立て不可。
            // RemoveSlotPlugin 単体は db 不要だが、 Phase B はまとめて
            // skip する (= db ロード待ち)。
            if !song.tracks.is_empty() {
                tracing::warn!("reconcile: plugin database not loaded; phase B skipped");
            }
            return;
        };

        for track in &song.tracks {
            let track_id = track.id;

            // Song 側の (slot, plugin_id_str, &PluginInstance) 列。
            let mut song_slots: Vec<(PluginSlot, &common::model::PluginInstance)> = Vec::new();
            for (i, p) in track.midi_fx_chain.iter().enumerate() {
                song_slots.push((PluginSlot::MidiFx(i as u32), p));
            }
            if let Some(inst) = track.instrument.as_ref() {
                song_slots.push((PluginSlot::Instrument, inst));
            }
            for (i, p) in track.fx_chain.iter().enumerate() {
                song_slots.push((PluginSlot::Fx(i as u32), p));
            }
            let song_slot_set: std::collections::HashSet<PluginSlot> =
                song_slots.iter().map(|(s, _)| *s).collect();

            // (1) host にあるが Song に無い slot → RemoveSlotPlugin
            let host_extra_slots: Vec<PluginSlot> = self
                .loaded_slots
                .iter()
                .filter(|((tid, _), _)| *tid == track_id)
                .map(|((_, s), _)| *s)
                .filter(|s| !song_slot_set.contains(s))
                .collect();
            for slot in host_extra_slots {
                tracing::info!(track_id, ?slot, "reconcile: removing extra host slot");
                self.send_plugin(MainToChild::RemoveSlotPlugin {
                    track: track_id,
                    slot,
                });
                self.cleanup_slot_gui(track_id, slot);
                self.loaded_slots.remove(&(track_id, slot));
                self.pending_plugin_loads.remove(&(track_id, slot));
            }

            // (2) Song にあるが host に無い、 または plugin_id_str が
            // 違う slot → SetSlotPlugin
            for (slot, inst) in song_slots {
                let need_load = match self.loaded_slots.get(&(track_id, slot)) {
                    None => true,
                    Some(info) => info.plugin_id_str != inst.plugin_id,
                };
                if !need_load {
                    continue;
                }
                let Some(entry) = db.find_by_id(&inst.plugin_id) else {
                    tracing::error!(
                        id = %inst.plugin_id,
                        track = track_id,
                        ?slot,
                        "reconcile: plugin id not in database"
                    );
                    continue;
                };
                tracing::info!(
                    track_id,
                    ?slot,
                    plugin_id = %inst.plugin_id,
                    "reconcile: loading slot from song"
                );
                self.track_pending_load(track_id, slot);
                self.send_plugin(MainToChild::SetSlotPlugin {
                    track: track_id,
                    slot,
                    format: entry.format,
                    path: entry.path.clone(),
                    plugin_id: entry.id.clone(),
                    initial_state: inst.state.clone(),
                });
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

    /// Bitwig / Ableton / Logic 流: project = bundle directory。 UX として
    /// ユーザーは普通の「名前を付けて保存」 dialog でプロジェクト名
    /// (例: `wav03.daw`) を入力する。 daw_01 はその親フォルダ内に
    /// **同名のフォルダを作成** し、 中に project file (`wav03.daw`) と
    /// `samples/` (imported audio copy)、 将来 `bounce/` 等を配置する。
    /// = ユーザー入力 `<parent>/wav03.daw` → 実際の保存先は
    /// `<parent>/wav03/wav03.daw`。 これにより
    /// 「ファイル名だけ選んだら samples/ がどこに作られるか分からない」
    /// 旧挙動と「pick_folder dialog では新規フォルダを作れない」 (Windows
    /// の input 欄問題) を同時に解消する。 仕様書:
    /// `docs/plan_audio_clip.md` §5 / §13 Q2。
    fn action_save_as(&mut self) {
        let Some(picked) = rfd::FileDialog::new()
            .add_filter("daw", &["daw"])
            .set_title("プロジェクト名 / 保存先を選択 (フォルダは自動作成されます)")
            .save_file()
        else {
            return;
        };
        let Some(stem) = picked
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            self.status_message = "プロジェクト名を取得できませんでした".to_string();
            return;
        };
        let parent = picked
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let project_dir = parent.join(&stem);
        let path = project_dir.join(format!("{stem}.daw"));
        if path.exists() {
            let res = rfd::MessageDialog::new()
                .set_title("プロジェクトの上書き確認")
                .set_description(format!(
                    "{} は既に存在します。 上書きしますか？",
                    path.display()
                ))
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if res != rfd::MessageDialogResult::Yes {
                return;
            }
        }
        if let Err(e) = std::fs::create_dir_all(&project_dir) {
            self.status_message = format!(
                "プロジェクトフォルダの作成に失敗: {} ({e})",
                project_dir.display()
            );
            return;
        }
        self.begin_save(path);
    }

    /// Song 内に CLAP/VST3 plugin が 1 つでもあるか。 何も無ければ
    /// `RequestAllStates` を発行する意味が無いので、 deferred / save の
    /// dispatcher は plugin なしを早期判定して即時実行に切り替える。
    fn song_has_plugin(&self) -> bool {
        self.song.tracks.iter().any(|t| {
            t.instrument.is_some() || !t.fx_chain.is_empty() || !t.midi_fx_chain.is_empty()
        })
    }

    /// `AllPluginStates` で受け取った各 plugin の state を `Song` の
    /// 対応する `PluginInstance::state` に書き戻す。 save flow と Undo
    /// snapshot deferred path の両方で呼ばれる共通 helper。
    ///
    /// `track` の検索は Vec position ではなく **`Track::id` 一致** で
    /// 行う。 plugin_host は SlotState の `track` を `Track::id` で
    /// 詰める仕様 (PR2.1)。 旧実装は `tracks.get_mut(s.track as usize)`
    /// と Vec index で検索していたが、 deferred path で track が再
    /// 並び替わっていると壊れるため改めた。
    fn apply_plugin_states(&mut self, states: &[SlotState]) {
        for s in states {
            let Some(track) = self.song.tracks.iter_mut().find(|t| t.id == s.track) else {
                tracing::warn!(track = s.track, ?s.slot, "apply_plugin_states: track id not found");
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
    }

    /// project save の trigger。 plugin がある場合は plugin_host から
    /// 最新 state を取って Song に書き戻してから save する。 plugin が
    /// 1 つもなければ即 save。
    ///
    /// 既に `RequestAllStates` を発行中 (= 別の deferred edit の sync を
    /// 待っている) なら、 fallback として state 同期なしで即時 save する
    /// (= 古い save 挙動)。 これは「 plugin を消そうとしたタイミングで
    /// Ctrl+S」 のような狭い race のための妥協で、 実用上の問題は無い。
    fn begin_save(&mut self, path: PathBuf) {
        if self.pending_state_request.is_some() {
            tracing::warn!("begin_save: pending state request already in flight; saving without state sync");
            self.save_after_states(path);
            return;
        }
        if !self.song_has_plugin() {
            self.save_after_states(path);
            return;
        }
        self.pending_state_request = Some(PendingStateRequest::Save { path });
        self.send_plugin(MainToChild::RequestAllStates);
    }

    /// state 適用 (save flow なら `apply_plugin_states` 済み、 plugin が
    /// 無ければ no-op) のあとファイルを書き出す。
    fn save_after_states(&mut self, path: PathBuf) {
        if self.save_to(&path) {
            self.file_path = Some(path);
        }
    }

    fn save_to(&mut self, path: &Path) -> bool {
        // Phase 1 PR3: 未保存 project 中に import した audio source は
        // user import_cache に置かれている。 save 時に
        // `<project_dir>/samples/` へ move + path を ProjectRelative に
        // 書き換える (`docs/plan_audio_clip.md` §13 Q2)。 失敗しても
        // save は続行し、 missing source として扱う。
        if let Some(project_dir) = path.parent()
            && let Err(e) = import_audio::migrate_unsaved_audio_sources_into(
                &mut self.song,
                project_dir,
            )
        {
            tracing::warn!(
                error = ?e,
                path = %path.display(),
                "audio sources のうち import_cache → samples/ への移行で一部失敗"
            );
            self.status_message = format!(
                "Audio sources の samples/ 移行で一部失敗: {e}"
            );
        }
        match common::project::save(path, &self.song) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "saved project");
                self.is_dirty = false;
                self.push_recent(path.to_path_buf());
                // PR6: Save の中で audio_sources の path が
                // `Absolute(import_cache)` → `ProjectRelative(samples/)`
                // に書き換わり、 さらに project_dir も新たに確定した。
                // 両方を audio engine へ再送して
                // `AudioClipRenderer` を rebuild させる (順序保証付き
                // IPC なので SetProjectDir → LoadSong)。 file_path は
                // 呼び出し側 `save_after_states` が確定するが、
                // path.parent() を直接使えば Save 時点の project_dir
                // を正しく送れる。
                let project_dir: Option<PathBuf> =
                    path.parent().map(Path::to_path_buf);
                self.send_audio(MainToChild::SetProjectDir(project_dir));
                let song = self.song.clone();
                self.send_audio(MainToChild::LoadSong(song));
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

    /// `AppEvent::DeleteTrack` の dispatcher。 plugin が song に居る
    /// 場合は `RequestAllStates` を投げて、 受信時に最新 plugin state
    /// を Song に書き込んでから [`Self::push_undo_snapshot`] + 削除を
    /// 実行する。 これで「knob を回した状態で track 削除 → Undo」 で
    /// knob 値が復元される。 plugin 無しの song / 既に pending 中の
    /// 場合は state 同期なしで即時実行。
    fn delete_track(&mut self, idx: u32) {
        let Some(track_id) = self.song.tracks.get(idx as usize).map(|t| t.id) else {
            return;
        };
        if !self.song_has_plugin() || self.pending_state_request.is_some() {
            self.push_undo_snapshot();
            self.delete_track_inner(track_id);
            return;
        }
        self.pending_state_request = Some(PendingStateRequest::Deferred(
            DeferredEdit::DeleteTrack { track_id },
        ));
        self.send_plugin(MainToChild::RequestAllStates);
    }

    /// 実際の削除処理。 [`Self::on_all_states_from_child`] か上の
    /// dispatcher の即時 fallback path から呼ばれる。 どちらでも呼び出し
    /// 側で `push_undo_snapshot` 済みである前提なので、 ここでは push
    /// しない。
    fn delete_track_inner(&mut self, track_id: u32) {
        let Some(idx) = self.song.track_index_by_id(track_id) else {
            return;
        };
        let idx = idx as u32;
        if idx as usize >= self.song.tracks.len() {
            return;
        }

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
            // slot cache からも削除する track 由来の entry を外す。
            // SlotPluginUnloaded event の到着待ち race を狭めて、
            // reconcile が stale entry を見ないようにする防御的 cleanup。
            self.loaded_slots.retain(|(t, _), _| *t != removed_id);
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
    /// `AppEvent::UngroupTracks` の dispatcher。 group track を ungroup
    /// すると group の `fx_chain` が削除されるため、 [`delete_track`] と
    /// 同様 plugin の最新 state を取ってから Undo snapshot を取って実行
    /// する。
    fn action_ungroup_tracks(&mut self, track_ids: &[u32]) {
        if track_ids.is_empty() {
            return;
        }
        if !self.song_has_plugin() || self.pending_state_request.is_some() {
            self.push_undo_snapshot();
            self.action_ungroup_tracks_inner(track_ids);
            return;
        }
        self.pending_state_request = Some(PendingStateRequest::Deferred(
            DeferredEdit::UngroupTracks {
                track_ids: track_ids.to_vec(),
            },
        ));
        self.send_plugin(MainToChild::RequestAllStates);
    }

    fn action_ungroup_tracks_inner(&mut self, track_ids: &[u32]) {
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
                self.loaded_slots.retain(|(t, _), _| *t != *group_id);
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
        let target_track_idx = target.track as usize;
        let target_clip_idx = target.clip as usize;
        let Some(clip) = self
            .song
            .tracks
            .get(target_track_idx)
            .and_then(|t| t.clips.get(target_clip_idx))
        else {
            return;
        };
        let cursor = if cursor >= clip.length_beats {
            0.0
        } else {
            cursor
        };
        let Some(notes) = self
            .song
            .notes_in_clip_mut(target_track_idx, target_clip_idx)
        else {
            return;
        };
        let new_idx = notes.len() as u32;
        notes.push(common::model::Note {
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

        let notes = self.song.clip_notes(clip);
        if notes.is_empty() {
            self.pianoroll_scroll_beat = 0.0;
            self.pianoroll_zoom_x =
                (grid_w / clip.length_beats.max(1.0) as f32).clamp(8.0, 400.0);
            self.pianoroll_top_pitch = 84;
            self.pianoroll_zoom_y = 14.0;
        } else {
            let min_beat = notes
                .iter()
                .map(|n| n.start_beat)
                .fold(f64::INFINITY, f64::min);
            let max_beat = notes
                .iter()
                .map(|n| n.start_beat + n.duration_beats)
                .fold(f64::NEG_INFINITY, f64::max);
            let min_pitch = notes.iter().map(|n| n.pitch).min().unwrap_or(60);
            let max_pitch = notes.iter().map(|n| n.pitch).max().unwrap_or(60);

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

    fn set_clip_positions(&mut self, entries: &[(ClipRef, u32, f64)]) {
        // track 跨ぎ move: source track と to_track が異なれば clip を remove +
        // 別 track に再 push。 同 track 内なら start_beat だけ update。
        // 同 track 内で複数 entry がある場合、 高い clip_idx から処理しないと
        // 配列インデックスが先に変動してしまうので、 source.track 同一 group
        // ごとに clip_idx 降順で sort してから処理する。
        let mut entries: Vec<(ClipRef, u32, f64)> = entries.to_vec();
        entries.sort_by(|a, b| {
            a.0.track
                .cmp(&b.0.track)
                .then_with(|| b.0.clip.cmp(&a.0.clip))
        });

        let mut new_refs: Vec<(u32, u32)> = Vec::with_capacity(entries.len());
        for (source, to_track_id, new_start_beat) in entries {
            let new_start = new_start_beat.max(0.0);
            let Some(source_track_id) = self
                .song
                .tracks
                .get(source.track as usize)
                .map(|t| t.id)
            else {
                continue;
            };
            if source_track_id == to_track_id {
                if let Some(track) = self.song.tracks.get_mut(source.track as usize)
                    && let Some(clip) = track.clips.get_mut(source.clip as usize)
                {
                    clip.start_beat = new_start;
                    new_refs.push((source.track, clip.id));
                }
            } else {
                let Some(to_track_idx) =
                    self.song.track_index_by_id(to_track_id)
                else {
                    continue;
                };
                let Some(removed) =
                    self.song.tracks.get_mut(source.track as usize).and_then(|t| {
                        if (source.clip as usize) < t.clips.len() {
                            Some(t.clips.remove(source.clip as usize))
                        } else {
                            None
                        }
                    })
                else {
                    continue;
                };
                let Some(to_track) = self.song.tracks.get_mut(to_track_idx) else {
                    continue;
                };
                let new_clip_id = to_track.alloc_clip_id();
                let mut new_clip = removed;
                new_clip.id = new_clip_id;
                new_clip.start_beat = new_start;
                to_track.clips.push(new_clip);
                new_refs.push((to_track_idx as u32, new_clip_id));
            }
        }
        // ClipRef を最新の (track_idx, clip_idx) に再構築。
        self.selected_clips = new_refs
            .iter()
            .filter_map(|(t_idx, c_id)| {
                let track = self.song.tracks.get(*t_idx as usize)?;
                let c_idx = track.clips.iter().position(|c| c.id == *c_id)?;
                Some(ClipRef {
                    track: *t_idx,
                    clip: c_idx as u32,
                })
            })
            .collect();
        self.selected_clip = self.selected_clips.last().copied();
        self.sync_song_to_plugin_host();
    }

    /// Bounce In Place (Pre-FX、 `docs/plan_audio_clip.md` §3.8 / §13 Q8)。
    /// `target` clip 内の全 events を engine sample_rate で stereo mix
    /// して WAV 32-bit float ファイルに書き出し、 新 `AudioSource` を
    /// 採番して `Song.audio_sources` に insert、 `audio_source_cache` に
    /// 登録、 `ClipContent::Audio { events: [単一新 event] }` で置換、
    /// audio engine に `SetGeneratedAudio` で配信する。 同 `ContentId` を
    /// 共有していた linked clip も新 content で同期される (= `clip_contents`
    /// は `ContentId` 単位の pool)。
    ///
    /// 出力先: project_dir があれば `<project_dir>/bounce/<name>_<ts>.wav`、
    /// 未保存 project は `%LOCALAPPDATA%/daw_01/bounce_cache/<random>.wav`
    /// (= `import_cache` と同じ fallback、 save 時に `<project_dir>/bounce/`
    /// へ移動するのは将来 PR で `migrate_unsaved_audio_sources_into` 相当の
    /// helper を bounce 用にも追加)。
    ///
    /// Pre-FX なので plugin chain (instrument / fx_chain) は通さない。
    /// source の events を fade / gain / pan / pitch_ratio で mix した
    /// snapshot のみ。 plugin 効果込みの bounce は spec §3.8 "Bounce"
    /// (= 新 Clip + 新 track) で別 PR。
    fn bounce_clip_in_place(&mut self, target: ClipRef) {
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get(target.clip as usize).cloned() else {
            return;
        };
        let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get(&clip.content_id).cloned()
        else {
            self.status_message = "Bounce In Place: audio clip ではありません".into();
            return;
        };
        if audio.events.is_empty() {
            self.status_message = "Bounce In Place: events が空です".into();
            return;
        }

        let engine_sr = common::audio_bridge::SAMPLE_RATE;
        let bpm = self.song.bpm.max(1.0) as f64;
        let samples_per_beat = engine_sr as f64 * 60.0 / bpm;
        let total_frames = (clip.length_beats * samples_per_beat).max(0.0) as usize;
        if total_frames == 0 {
            self.status_message = "Bounce In Place: clip 長が 0 です".into();
            return;
        }

        // ---- mix loop (Pre-FX、 audio_clip_renderer のロジックを daw_gui
        // 側に portion-wise port。 Phase 3+ で render_audio_events を共通
        // crate に切り出して DRY 化を検討。 ここは offline 1 回きりの
        // 計算なので allocation も自由)。
        let mut mix_l = vec![0.0_f32; total_frames];
        let mut mix_r = vec![0.0_f32; total_frames];
        for event in &audio.events {
            if event.muted {
                continue;
            }
            let Some(buffer) = self.audio_source_cache.get(event.source_id) else {
                continue;
            };
            let event_start =
                (event.event_start_in_clip_beats * samples_per_beat).max(0.0) as usize;
            let event_end =
                ((event.event_start_in_clip_beats + event.event_length_beats) * samples_per_beat)
                    .max(0.0) as usize;
            let event_len = event_end.saturating_sub(event_start);
            if event_len == 0 {
                continue;
            }

            let pitch_ratio = common::audio_render::pitch_ratio_for(
                event.stretch_mode,
                buffer.sample_rate,
                engine_sr,
                event.pitch_semitones,
            );
            let gain_lin = 10f32.powf(event.gain_db / 20.0);
            let pan_rad = (event.pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
            let pan_l = pan_rad.cos();
            let pan_r = pan_rad.sin();
            let fade_in_frames =
                (event.fade_in_beats.max(0.0) * samples_per_beat).max(0.0) as u64;
            let fade_out_frames =
                (event.fade_out_beats.max(0.0) * samples_per_beat).max(0.0) as u64;
            let event_total = event_len as u64;
            let source_len = event
                .source_end_frames
                .saturating_sub(event.source_start_frames);

            let l_plane: &[f32] =
                buffer.samples.first().map(Vec::as_slice).unwrap_or(&[]);
            let r_plane: &[f32] = if buffer.channels >= 2 {
                buffer.samples.get(1).map(Vec::as_slice).unwrap_or(l_plane)
            } else {
                l_plane
            };

            for i in 0..event_len {
                let dst = event_start + i;
                if dst >= total_frames {
                    break;
                }
                let local = i as u64;
                let fade_in = common::audio_render::fade_envelope(
                    local,
                    fade_in_frames,
                    event.fade_in_curve,
                );
                let tail = event_total.saturating_sub(local + 1);
                let fade_out = common::audio_render::fade_envelope(
                    tail,
                    fade_out_frames,
                    event.fade_out_curve,
                );
                let env = fade_in * fade_out * gain_lin;
                if env == 0.0 {
                    continue;
                }
                let source_pos = i as f64 * pitch_ratio;
                let source_pos = if event.reversed {
                    source_len as f64 - 1.0 - source_pos
                } else {
                    source_pos
                };
                if source_pos < 0.0 {
                    continue;
                }
                let i0 = source_pos.floor() as i64;
                let frac = (source_pos - i0 as f64) as f32;
                if i0 < 0 {
                    continue;
                }
                let abs0 = event.source_start_frames + i0 as u64;
                let abs1 = abs0 + 1;
                if abs0 >= event.source_end_frames || abs0 >= buffer.frames {
                    continue;
                }
                let s_l0 = l_plane.get(abs0 as usize).copied().unwrap_or(0.0);
                let s_r0 = r_plane.get(abs0 as usize).copied().unwrap_or(0.0);
                let s_l1 = l_plane.get(abs1 as usize).copied().unwrap_or(s_l0);
                let s_r1 = r_plane.get(abs1 as usize).copied().unwrap_or(s_r0);
                let s_l = s_l0 + (s_l1 - s_l0) * frac;
                let s_r = s_r0 + (s_r1 - s_r0) * frac;
                let sqrt2 = std::f32::consts::SQRT_2;
                mix_l[dst] += s_l * env * pan_l * sqrt2;
                mix_r[dst] += s_r * env * pan_r * sqrt2;
            }
        }

        // ---- WAV 書き出し ----
        // file 名: clip 名を sanitize + ts8 (epoch milli の下 8 桁)。
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 % 100_000_000)
            .unwrap_or(0);
        let safe_name: String = clip
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let safe_name = if safe_name.is_empty() {
            "bounce".into()
        } else {
            safe_name
        };
        let filename = format!("{safe_name}_{ts:08}.wav");

        let project_dir = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
        let (out_path, source_path) = match project_dir.as_deref() {
            Some(dir) => {
                let bounce_dir = dir.join("bounce");
                if let Err(e) = std::fs::create_dir_all(&bounce_dir) {
                    self.status_message =
                        format!("Bounce In Place: bounce/ 作成失敗: {e}");
                    return;
                }
                let dst = bounce_dir.join(&filename);
                (
                    dst.clone(),
                    common::model::AudioSourcePath::ProjectRelative(
                        std::path::PathBuf::from("bounce").join(&filename),
                    ),
                )
            }
            None => {
                // 未保存 project は import_cache と同様、 user cache に
                // 一時書き出し。 save 時の migration helper は将来追加。
                let cache = std::env::var_os("LOCALAPPDATA")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir)
                    .join("daw_01")
                    .join("bounce_cache");
                if let Err(e) = std::fs::create_dir_all(&cache) {
                    self.status_message =
                        format!("Bounce In Place: bounce_cache/ 作成失敗: {e}");
                    return;
                }
                let dst = cache.join(&filename);
                (
                    dst.clone(),
                    common::model::AudioSourcePath::Absolute(dst.clone()),
                )
            }
        };

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: engine_sr,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = match hound::WavWriter::create(&out_path, spec) {
            Ok(w) => w,
            Err(e) => {
                self.status_message =
                    format!("Bounce In Place: WAV create 失敗: {e}");
                return;
            }
        };
        let mut writer = writer;
        for i in 0..total_frames {
            if writer.write_sample(mix_l[i]).is_err()
                || writer.write_sample(mix_r[i]).is_err()
            {
                self.status_message = "Bounce In Place: WAV 書き込み失敗".into();
                let _ = std::fs::remove_file(&out_path);
                return;
            }
        }
        if let Err(e) = writer.finalize() {
            self.status_message =
                format!("Bounce In Place: WAV finalize 失敗: {e}");
            let _ = std::fs::remove_file(&out_path);
            return;
        }

        // ---- AudioSource 採番 + Song / cache 更新 ----
        let new_source_id = self.song.alloc_audio_source_id();
        let new_source = common::model::AudioSource {
            path: source_path,
            sample_rate: engine_sr,
            channels: 2,
            frames: total_frames as u64,
            original_bpm: Some(self.song.bpm),
            root_key: None,
        };
        self.song.audio_sources.insert(new_source_id, new_source);
        let new_buffer = std::sync::Arc::new(crate::audio_source_cache::AudioSourceBuffer {
            sample_rate: engine_sr,
            channels: 2,
            frames: total_frames as u64,
            samples: vec![mix_l, mix_r],
        });
        self.audio_source_cache.insert(new_source_id, new_buffer);

        // ClipContent::Audio を 1 event 構成に置換 (= bounce 後は flat な
        // single-event clip)。 Phase 1 の 1 clip 1 event 前提と整合。
        let new_event = common::model::AudioEvent {
            source_id: new_source_id,
            event_start_in_clip_beats: 0.0,
            event_length_beats: clip.length_beats,
            source_start_frames: 0,
            source_end_frames: total_frames as u64,
            ..common::model::AudioEvent::default()
        };
        if let Some(content) = self.song.clip_contents.get_mut(&clip.content_id) {
            *content = common::model::ClipContent::Audio(common::model::AudioContent {
                events: vec![new_event],
            });
        }

        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        self.status_message = format!(
            "Bounce In Place: '{}' を {} に書き出し",
            clip.name,
            out_path.display()
        );
    }

    /// `target` clip の first event の `reversed` 値を読む。 audio で
    /// ない / event が空 / 範囲外なら `false`。 メニューの toggle 用。
    fn is_clip_audio_event_reversed(&self, target: ClipRef) -> bool {
        self.song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| {
                if let Some(common::model::ClipContent::Audio(audio)) =
                    self.song.clip_contents.get(&c.content_id)
                {
                    audio.events.first().map(|e| e.reversed)
                } else {
                    None
                }
            })
            .unwrap_or(false)
    }

    /// `AudioEvent.reversed` を更新 (`docs/plan_audio_clip.md` §3.8)。
    /// audio_editor で event を選択中なら当該 event のみ、 さもなくば
    /// 全 event に broadcast (= multi-event 対応 / 1 clip 1 event 互換、
    /// PR-D 段階 2)。
    fn set_clip_audio_event_reversed(&mut self, target: ClipRef, reversed: bool) {
        self.mutate_audio_events_in_clip(target, |e| e.reversed = reversed);
    }

    /// `AudioEvent.muted` を更新 (event 単位 silent flag、 track-mute と
    /// 独立)。 broadcast 範囲は `audio_event_target_indices` 仕様。
    fn set_clip_audio_event_muted(&mut self, target: ClipRef, muted: bool) {
        self.mutate_audio_events_in_clip(target, |e| e.muted = muted);
    }

    /// `AudioEvent.stretch_mode` を更新。 `compile_audio_schedule` が
    /// 次の LoadSong で再 compile し、 Repitch の場合は pitch_ratio の
    /// 再計算が走る。 Phase 1 で再生に効くのは Raw / Repitch のみ。
    fn set_clip_audio_event_stretch_mode(
        &mut self,
        target: ClipRef,
        mode: common::model::StretchMode,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.stretch_mode = mode);
    }

    fn set_clip_audio_event_gain_db(&mut self, target: ClipRef, gain_db: f32) {
        let gain_db = gain_db.clamp(-80.0, 24.0);
        self.mutate_audio_events_in_clip(target, |e| e.gain_db = gain_db);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    fn set_clip_audio_event_pan(&mut self, target: ClipRef, pan: f32) {
        let pan = pan.clamp(-1.0, 1.0);
        self.mutate_audio_events_in_clip(target, |e| e.pan = pan);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    fn set_clip_audio_event_pitch_semitones(&mut self, target: ClipRef, semitones: f32) {
        // Bitwig spec §3.6: Pitch range is -96 .. +96 semitones.
        let semitones = semitones.clamp(-96.0, 96.0);
        self.mutate_audio_events_in_clip(target, |e| e.pitch_semitones = semitones);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    /// `clip_*_edit_text` を `target` clip の first event の現値で
    /// 再生成する。 select 切替や Undo/Redo / open 時に呼ぶ。 target が
    /// 該当 clip を解決できない / `ClipContent::Audio` でない場合は
    /// buffer を空に戻して target を `None` 化する (= inspector で
    /// audio event section が消えている状態に整合)。
    fn resync_clip_audio_event_edit_buffers(&mut self, target: ClipRef) {
        let event_snapshot = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| {
                if let Some(common::model::ClipContent::Audio(audio)) =
                    self.song.clip_contents.get(&c.content_id)
                {
                    audio.events.first().cloned()
                } else {
                    None
                }
            });
        match event_snapshot {
            Some(ev) => {
                self.clip_edit_buffer_target = Some(target);
                self.clip_gain_db_edit_text = format!("{:.1}", ev.gain_db);
                self.clip_pan_edit_text = format!("{:.2}", ev.pan);
                self.clip_pitch_edit_text = format!("{:+.1}", ev.pitch_semitones);
                self.clip_fade_in_edit_text = format!("{:.3}", ev.fade_in_beats);
                self.clip_fade_out_edit_text = format!("{:.3}", ev.fade_out_beats);
            }
            None => {
                self.clip_edit_buffer_target = None;
                self.clip_gain_db_edit_text.clear();
                self.clip_pan_edit_text.clear();
                self.clip_pitch_edit_text.clear();
                self.clip_fade_in_edit_text.clear();
                self.clip_fade_out_edit_text.clear();
            }
        }
    }

    /// `target` clip の length_beats を fade clamp 用に取得する helper。
    /// clip が解決できなければ `None`。
    fn clip_length_beats(&self, target: ClipRef) -> Option<f64> {
        Some(
            self.song
                .tracks
                .get(target.track as usize)?
                .clips
                .get(target.clip as usize)?
                .length_beats,
        )
    }

    fn set_clip_audio_event_fade_in_beats(&mut self, target: ClipRef, beats: f64) {
        // Spec §3.5: fade は clip 内 beats、 clip 長を超えないように clamp。
        let max_beats = self.clip_length_beats(target).unwrap_or(0.0);
        let beats = beats.clamp(0.0, max_beats);
        self.mutate_audio_events_in_clip(target, |e| e.fade_in_beats = beats);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    fn set_clip_audio_event_fade_out_beats(&mut self, target: ClipRef, beats: f64) {
        let max_beats = self.clip_length_beats(target).unwrap_or(0.0);
        let beats = beats.clamp(0.0, max_beats);
        self.mutate_audio_events_in_clip(target, |e| e.fade_out_beats = beats);
        self.resync_clip_audio_event_edit_buffers(target);
    }

    fn set_clip_audio_event_fade_in_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    fn set_clip_audio_event_fade_out_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_audio_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    fn commit_clip_fade_in_edit(&mut self) {
        let Some(target) = self.clip_edit_buffer_target else {
            return;
        };
        match self.clip_fade_in_edit_text.trim().parse::<f64>() {
            Ok(v) => self.set_clip_audio_event_fade_in_beats(target, v),
            Err(_) => {
                self.status_message = format!(
                    "Fade In: '{}' を数値として解釈できません",
                    self.clip_fade_in_edit_text
                );
                self.resync_clip_audio_event_edit_buffers(target);
            }
        }
    }

    /// audio clip 判定。 `target` が指す clip が `ClipContent::Audio` か。
    /// MIDI / Vocal / 範囲外は false。 Audio Editor の open 判定で使う。
    pub fn is_audio_clip(&self, target: ClipRef) -> bool {
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return false;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return false;
        };
        matches!(
            self.song.clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Audio(_))
        )
    }

    /// audio clip ダブルクリックで Audio Editor を開く。 `target` が
    /// 非 audio (MIDI / Vocal / 範囲外) なら silent no-op。 bottom_panel
    /// を tab 1 (= 通常 Piano Roll、 audio_editor_clip is Some なら
    /// audio_editor view に切り替わる) に揃える。
    fn open_audio_editor(&mut self, target: ClipRef) {
        if !self.is_audio_clip(target) {
            return;
        }
        self.audio_editor_clip = Some(target);
        self.bottom_panel = 1;
    }

    fn close_audio_editor(&mut self) {
        self.audio_editor_clip = None;
        self.audio_editor_selected_event = None;
    }

    /// PR-D 段階 1: Audio Editor で開いている clip + 選択中 event を
    /// Duplicate (= 同 source の event を直後に複製、 spec §3.10.2 の
    /// `Ctrl+D`)。 audio_editor_clip と audio_editor_selected_event の
    /// どちらかが None なら no-op。 新 event は src.event_start +
    /// src.event_length_beats の位置に配置、 同 source / 同パラメータ。
    /// clip.length_beats は新 event の終端を超えないように自動拡張。
    /// selection は新 event index に進む。
    fn duplicate_audio_editor_event(&mut self) {
        let Some(target) = self.audio_editor_clip else {
            return;
        };
        let Some(idx) = self.audio_editor_selected_event else {
            return;
        };
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        let Some(src) = audio.events.get(idx).cloned() else {
            return;
        };
        let new_start = src.event_start_in_clip_beats + src.event_length_beats;
        let mut new_event = src.clone();
        new_event.event_start_in_clip_beats = new_start;
        let insert_at = idx + 1;
        if insert_at >= audio.events.len() {
            audio.events.push(new_event);
        } else {
            audio.events.insert(insert_at, new_event);
        }
        // clip.length_beats を必要に応じて拡張 (= 新 event の右端を含むよう
        // に)。 元 length より長くなる場合のみ更新。
        let needed = new_start + src.event_length_beats;
        if needed > clip.length_beats {
            clip.length_beats = needed;
        }
        self.audio_editor_selected_event = Some(insert_at);
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// PR-D 段階 3: Audio Editor で event の clip 内位置を変更 (= 中央
    /// drag 移動)。 `event_start_in_clip_beats` を `new_start_beats`
    /// (clamp 0..) に設定。 範囲外 / 非 audio clip / event_idx 範囲外
    /// なら no-op。 clip.length_beats は新 event 終端を含むよう自動拡張。
    fn set_audio_event_start(
        &mut self,
        target: ClipRef,
        event_idx: usize,
        new_start_beats: f64,
    ) {
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        let Some(event) = audio.events.get_mut(event_idx) else {
            return;
        };
        let new_start = new_start_beats.max(0.0);
        event.event_start_in_clip_beats = new_start;
        let needed = new_start + event.event_length_beats;
        if needed > clip.length_beats {
            clip.length_beats = needed;
        }
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// PR-D 段階 3: Audio Editor で event 端 trim (= 左右端 drag)。
    /// `side == Left` で左端 trim (= event_start_in_clip_beats +
    /// event_length_beats + source_start_frames を delta で連動)、
    /// `side == Right` で右端 trim (= event_length_beats +
    /// source_end_frames を連動)。 source の sample_rate で
    /// delta_beats → frames 変換 (bpm = self.song.bpm)。 source 境界
    /// (0..total_frames) と event_length_beats > 0 を保つ clamp 込み。
    fn set_audio_event_trim(
        &mut self,
        target: ClipRef,
        event_idx: usize,
        side: AudioEventTrimSide,
        delta_beats: f64,
    ) {
        let bpm = self.song.bpm.max(1.0) as f64;
        // source 情報を先に snapshot (= 後の mut borrow と分離)。
        let (sr_hz, total_frames) = {
            let Some(track) = self.song.tracks.get(target.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get(target.clip as usize) else {
                return;
            };
            let Some(common::model::ClipContent::Audio(audio)) =
                self.song.clip_contents.get(&clip.content_id)
            else {
                return;
            };
            let Some(event) = audio.events.get(event_idx) else {
                return;
            };
            let Some(audio_source) = self.song.audio_sources.get(&event.source_id) else {
                return;
            };
            (audio_source.sample_rate as f64, audio_source.frames)
        };
        let delta_frames = (delta_beats * 60.0 / bpm * sr_hz).round() as i64;

        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        let Some(event) = audio.events.get_mut(event_idx) else {
            return;
        };

        const MIN_LEN_BEATS: f64 = 1e-4;
        match side {
            AudioEventTrimSide::Left => {
                // delta_beats > 0 で右に縮める (= start を遅らせる)、
                // < 0 で左に伸ばす。 ただし event_length が MIN_LEN を
                // 切らないよう先に clamp。
                let max_inset = (event.event_length_beats - MIN_LEN_BEATS).max(0.0);
                let dbeats = delta_beats.clamp(
                    -event.event_start_in_clip_beats,
                    max_inset,
                );
                let dframes = (dbeats * 60.0 / bpm * sr_hz).round() as i64;
                let new_start_in_clip = event.event_start_in_clip_beats + dbeats;
                let new_length = event.event_length_beats - dbeats;
                let new_source_start = (event.source_start_frames as i64 + dframes)
                    .max(0)
                    .min(event.source_end_frames as i64) as u64;
                event.event_start_in_clip_beats = new_start_in_clip;
                event.event_length_beats = new_length.max(MIN_LEN_BEATS);
                event.source_start_frames = new_source_start;
                let _ = delta_frames;
            }
            AudioEventTrimSide::Right => {
                // delta_beats > 0 で右に伸ばす、 < 0 で縮める。 縮める
                // 側は event_length が MIN_LEN を切らないよう clamp、
                // 伸ばす側は source_end_frames が total_frames を超え
                // ないよう clamp。
                let max_grow_frames = total_frames as i64 - event.source_end_frames as i64;
                let max_grow_beats =
                    (max_grow_frames as f64) / sr_hz * bpm / 60.0;
                let min_shrink_beats = -(event.event_length_beats - MIN_LEN_BEATS).max(0.0);
                let dbeats = delta_beats.clamp(min_shrink_beats, max_grow_beats);
                let dframes = (dbeats * 60.0 / bpm * sr_hz).round() as i64;
                let new_length = event.event_length_beats + dbeats;
                let new_source_end = ((event.source_end_frames as i64 + dframes)
                    .max(event.source_start_frames as i64)
                    .min(total_frames as i64)) as u64;
                event.event_length_beats = new_length.max(MIN_LEN_BEATS);
                event.source_end_frames = new_source_end;
            }
        }

        let needed = event.event_start_in_clip_beats + event.event_length_beats;
        if needed > clip.length_beats {
            clip.length_beats = needed;
        }
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// PR-D 段階 3: Audio Editor の空白領域 file drop で新 event 追加。
    /// `import_audio::import_one` で decode + audio source 登録、 既存
    /// audio clip に新 event を `position_in_clip_beats` (clamp 0..) に
    /// 配置。 失敗時は status_message にエラー、 selection は新 event に
    /// 移す。 clip.length_beats は新 event 終端を含むよう自動拡張。
    fn add_audio_event_from_file(
        &mut self,
        target: ClipRef,
        path: PathBuf,
        position_in_clip_beats: f64,
    ) {
        if !self.is_audio_clip(target) {
            self.status_message = "Audio Editor: 対象 clip が audio ではないため event 追加できません".into();
            return;
        }
        let project_dir: Option<PathBuf> = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let imported = match import_audio::import_one(&path, project_dir.as_deref()) {
            Ok(i) => i,
            Err(e) => {
                self.status_message = format!("Audio event 追加 失敗: {}: {e}", path.display());
                return;
            }
        };
        let bpm = self.song.bpm;
        let length_beats =
            frames_to_beats(imported.buffer.frames, imported.buffer.sample_rate, bpm);
        let display_name = imported.display_name.clone();

        let source_id = self.song.alloc_audio_source_id();
        self.song.audio_sources.insert(source_id, imported.source);
        self.audio_source_cache
            .insert(source_id, imported.buffer.clone());

        let position = position_in_clip_beats.max(0.0);
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        let new_event = AudioEvent {
            source_id,
            event_start_in_clip_beats: position,
            event_length_beats: length_beats,
            source_start_frames: 0,
            source_end_frames: imported.buffer.frames,
            ..AudioEvent::default()
        };
        audio.events.push(new_event);
        let new_idx = audio.events.len() - 1;
        let needed = position + length_beats;
        if needed > clip.length_beats {
            clip.length_beats = needed;
        }
        self.audio_editor_selected_event = Some(new_idx);
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        self.status_message = format!("Audio event 追加: {display_name}");
    }

    /// PR-D 段階 3: Audio Editor で event を削除 (= Delete key /
    /// context menu)。 `events.remove(event_idx)`。 残 event 0 個でも
    /// content は保持 (= clip placeholder)。 selection は event_idx を
    /// `events.len() - 1` で詰める (events 空なら None)。
    fn delete_audio_event(&mut self, target: ClipRef, event_idx: usize) {
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        if event_idx >= audio.events.len() {
            return;
        }
        audio.events.remove(event_idx);
        let new_sel = if audio.events.is_empty() {
            None
        } else {
            Some(event_idx.min(audio.events.len() - 1))
        };
        self.audio_editor_selected_event = new_sel;
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
    }

    /// 全選択 audio clip に短 fade を一括適用 (`docs/plan_audio_clip
    /// .md` §3.5 Auto-Fade)。 fade 長は 4 ms 相当 (= `0.004 * bpm / 60`
    /// beats)、 既存値は上書き。 audio 以外の clip (MIDI / Vocal) と
    /// `selected_clip` がない場合は no-op。
    fn auto_fade_selected_clips(&mut self) {
        let bpm = self.song.bpm.max(1.0) as f64;
        let auto_fade_beats = 0.004 * bpm / 60.0; // 4 ms 相当
        let mut applied = 0usize;
        // borrow checker: target list を先に固める。
        let targets: Vec<ClipRef> = if self.selected_clips.is_empty() {
            self.selected_clip.into_iter().collect()
        } else {
            self.selected_clips.clone()
        };
        for target in targets {
            let Some(content_id) = self
                .song
                .tracks
                .get(target.track as usize)
                .and_then(|t| t.clips.get(target.clip as usize))
                .map(|c| c.content_id)
            else {
                continue;
            };
            let max_beats = self.clip_length_beats(target).unwrap_or(0.0);
            let fade_beats = auto_fade_beats.min(max_beats);
            if let Some(common::model::ClipContent::Audio(audio)) =
                self.song.clip_contents.get_mut(&content_id)
            {
                for event in &mut audio.events {
                    event.fade_in_beats = fade_beats;
                    event.fade_out_beats = fade_beats;
                }
                applied += 1;
            }
        }
        if applied > 0 {
            self.sync_song_to_plugin_host();
            // edit buffer (Inspector) も追従させる。
            if let Some(target) = self.clip_edit_buffer_target {
                self.resync_clip_audio_event_edit_buffers(target);
            }
            self.status_message = format!("Auto-Fade: {applied} 個のクリップに 4 ms fade を適用");
        } else {
            self.status_message = "Auto-Fade: 選択中の audio clip がありません".into();
        }
    }

    /// 隣接 audio clip ペアに crossfade を作成 (`docs/plan_audio_clip
    /// .md` §3.5 Auto-Crossfade)。 selected_clips のうち audio clip を
    /// track 別に集めて start_beat 順に並べ、 ペアごとに `prev_end >
    /// next_start` (= overlap 中) のみ overlap_beats を fade_out / fade_in
    /// に設定する。 隙間ペアは no-op、 完全重なり (next が prev に
    /// 内包される) はサポート対象外で skip + 警告。
    fn auto_crossfade_selected_clips(&mut self) {
        // (track_idx, clip_idx, start_beat, end_beat, content_id) を集める
        let mut entries: Vec<(u32, u32, f64, f64, u32)> = Vec::new();
        let targets: Vec<ClipRef> = if self.selected_clips.is_empty() {
            self.selected_clip.into_iter().collect()
        } else {
            self.selected_clips.clone()
        };
        for target in &targets {
            let Some(track) = self.song.tracks.get(target.track as usize) else {
                continue;
            };
            let Some(clip) = track.clips.get(target.clip as usize) else {
                continue;
            };
            let Some(common::model::ClipContent::Audio(_)) =
                self.song.clip_contents.get(&clip.content_id)
            else {
                continue;
            };
            entries.push((
                target.track,
                target.clip,
                clip.start_beat,
                clip.start_beat + clip.length_beats,
                clip.content_id,
            ));
        }
        if entries.len() < 2 {
            self.status_message =
                "Auto-Crossfade: 隣接判定には audio clip が 2 つ以上必要です".into();
            return;
        }
        // track ごとに sort して隣接ペアを抽出
        entries.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        });
        let mut applied = 0usize;
        for window in entries.windows(2) {
            let (prev_track, _, prev_start, prev_end, prev_content) = window[0];
            let (next_track, _, next_start, next_end, next_content) = window[1];
            if prev_track != next_track {
                continue;
            }
            if next_start >= prev_end {
                continue; // 隙間あり、 crossfade 対象外
            }
            if next_end <= prev_end {
                tracing::warn!(
                    prev_start, prev_end, next_start, next_end,
                    "Auto-Crossfade: next clip が prev に内包されているため skip"
                );
                continue;
            }
            let overlap = (prev_end - next_start).max(0.0);
            // prev clip の末尾 fade_out
            if let Some(common::model::ClipContent::Audio(audio)) =
                self.song.clip_contents.get_mut(&prev_content)
            {
                for event in &mut audio.events {
                    event.fade_out_beats = overlap.min(event.event_length_beats);
                }
            }
            // next clip の先頭 fade_in
            if let Some(common::model::ClipContent::Audio(audio)) =
                self.song.clip_contents.get_mut(&next_content)
            {
                for event in &mut audio.events {
                    event.fade_in_beats = overlap.min(event.event_length_beats);
                }
            }
            applied += 1;
        }
        if applied > 0 {
            self.sync_song_to_plugin_host();
            if let Some(target) = self.clip_edit_buffer_target {
                self.resync_clip_audio_event_edit_buffers(target);
            }
            self.status_message =
                format!("Auto-Crossfade: {applied} ペアに crossfade を適用");
        } else {
            self.status_message =
                "Auto-Crossfade: 重なっている隣接ペアがありません".into();
        }
    }

    fn commit_clip_fade_out_edit(&mut self) {
        let Some(target) = self.clip_edit_buffer_target else {
            return;
        };
        match self.clip_fade_out_edit_text.trim().parse::<f64>() {
            Ok(v) => self.set_clip_audio_event_fade_out_beats(target, v),
            Err(_) => {
                self.status_message = format!(
                    "Fade Out: '{}' を数値として解釈できません",
                    self.clip_fade_out_edit_text
                );
                self.resync_clip_audio_event_edit_buffers(target);
            }
        }
    }

    /// text_input commit (Enter / focus 喪失) 経路。 buffer を parse して
    /// 成功時は `set_clip_audio_event_gain_db` 経由で全 event 更新 +
    /// resync (= buffer を formatted な現値に書き戻し)、 失敗時は
    /// status_message + buffer のみ resync。 `CommitBpmEdit` と同じ pattern。
    fn commit_clip_gain_edit(&mut self) {
        let Some(target) = self.clip_edit_buffer_target else {
            return;
        };
        match self.clip_gain_db_edit_text.trim().parse::<f32>() {
            Ok(v) => self.set_clip_audio_event_gain_db(target, v),
            Err(_) => {
                self.status_message =
                    format!("Gain: '{}' を数値として解釈できません", self.clip_gain_db_edit_text);
                self.resync_clip_audio_event_edit_buffers(target);
            }
        }
    }

    fn commit_clip_pan_edit(&mut self) {
        let Some(target) = self.clip_edit_buffer_target else {
            return;
        };
        match self.clip_pan_edit_text.trim().parse::<f32>() {
            Ok(v) => self.set_clip_audio_event_pan(target, v),
            Err(_) => {
                self.status_message =
                    format!("Pan: '{}' を数値として解釈できません", self.clip_pan_edit_text);
                self.resync_clip_audio_event_edit_buffers(target);
            }
        }
    }

    fn commit_clip_pitch_edit(&mut self) {
        let Some(target) = self.clip_edit_buffer_target else {
            return;
        };
        match self.clip_pitch_edit_text.trim().parse::<f32>() {
            Ok(v) => self.set_clip_audio_event_pitch_semitones(target, v),
            Err(_) => {
                self.status_message = format!(
                    "Pitch: '{}' を数値として解釈できません",
                    self.clip_pitch_edit_text
                );
                self.resync_clip_audio_event_edit_buffers(target);
            }
        }
    }

    /// Clip の左右端 trim ハンドラ。 caller (arrangement widget) は
    /// `ResizeClipDelta { prev_start, next_start, prev_len, next_len }`
    /// から `next_start` / `next_len` を直接渡す。 ここで `delta_start =
    /// new_start_beat - prev_start_beat` を計算し、 audio clip では
    /// 各 event の clip 内位置 (`event_start_in_clip_beats`) と source 切り
    /// 出し (`source_start_frames` / `event_length_beats`) を整合させる
    /// (Bitwig 流 §3.2)。 MIDI clip では既存どおり `start_beat` /
    /// `length_beats` のみ更新。
    ///
    /// 左端 trim (delta_start > 0):
    /// - clip.start_beat += delta_start、 clip.length_beats -= delta_start (= next_len)
    /// - 各 event: clip 内 beats 軸を維持するため event_start_in_clip_beats
    ///   から delta_start を引く。 event の絶対位置 (= clip.start_beat +
    ///   event.event_start_in_clip_beats) は変わらない (= source の同位置を
    ///   そのまま再生する)
    /// - delta_start が event の途中に入った場合は event の左端を切り
    ///   詰める: event_start_in_clip_beats = 0、 event_length_beats を
    ///   削った分だけ縮める、 source_start_frames を delta_samples 進める
    ///
    /// 左端を伸ばす (delta_start < 0): event は単に右へスライド (= source
    /// は変えない、 clip 先頭の追加範囲は無音)。 source_start_frames を
    /// 負方向に動かすのは安全でない (source 開始フレームを超えると
    /// 配列範囲外) ので、 単純な後方スライドのみ。
    ///
    /// 右端 trim (delta_start == 0): 既存挙動 = length_beats を縮め、
    /// audio event を new_length_beats でクランプ。
    ///
    /// Phase 2 PR4 では Raw mode 前提で source_start_frames 計算に
    /// source.sample_rate * 60 / song.bpm を使う (= Repitch 中の左端 trim
    /// は pitch_ratio 補正が要るが将来 PR スコープ)。
    fn resize_clip(&mut self, target: ClipRef, new_start_beat: f64, new_length_beats: f64) {
        let new_length_beats = new_length_beats.max(0.0625);
        let new_start_beat = new_start_beat.max(0.0);
        let bpm = self.song.bpm.max(1.0) as f64;
        let (content_id, prev_start_beat) = {
            let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
                return;
            };
            let Some(clip) = track.clips.get_mut(target.clip as usize) else {
                return;
            };
            let prev_start_beat = clip.start_beat;
            clip.start_beat = new_start_beat;
            clip.length_beats = new_length_beats;
            (clip.content_id, prev_start_beat)
        };
        let delta_start = new_start_beat - prev_start_beat;

        // Snapshot の per-source sample_rate (event ごとに lookup できる
        // よう immutable borrow を先に切る)。 Phase 2 PR4 は Raw mode 前提。
        let audio_sources = self.song.audio_sources.clone();

        if let Some(ClipContent::Audio(audio)) = self.song.clip_contents.get_mut(&content_id) {
            for event in &mut audio.events {
                if delta_start > 0.0 {
                    // 左端 trim: event を delta_start だけ手前にずらして
                    // 絶対位置を維持する。 結果が負なら event の左端を
                    // 削る (source_start_frames を進める)。
                    let new_evt_start = event.event_start_in_clip_beats - delta_start;
                    if new_evt_start >= 0.0 {
                        event.event_start_in_clip_beats = new_evt_start;
                    } else {
                        let chopped_beats = -new_evt_start;
                        let source_sr = audio_sources
                            .get(&event.source_id)
                            .map(|s| s.sample_rate as f64)
                            .unwrap_or(48000.0);
                        let chopped_samples =
                            (chopped_beats * source_sr * 60.0 / bpm).max(0.0) as u64;
                        event.event_start_in_clip_beats = 0.0;
                        event.event_length_beats =
                            (event.event_length_beats - chopped_beats).max(0.0);
                        event.source_start_frames = event
                            .source_start_frames
                            .saturating_add(chopped_samples)
                            .min(event.source_end_frames);
                    }
                } else if delta_start < 0.0 {
                    // 左端を伸ばした: event を後方スライド。 source は
                    // 触らない (= 追加範囲は無音、 §3.2 に合致)。
                    event.event_start_in_clip_beats -= delta_start;
                }
                // 右端 trim 相当の clamp (delta_start のいずれかでも適用)
                let max_event_len =
                    (new_length_beats - event.event_start_in_clip_beats).max(0.0);
                if event.event_length_beats > max_event_len {
                    event.event_length_beats = max_event_len;
                }
            }
        }

        self.sync_song_to_plugin_host();
    }

    /// 共有コピー (D shortcut): 末尾直後 (start+length) に同サイズの clip を
    /// 1 つ生成、 `content_id` を流用。 `docs/plan_clip_share_clone.md` §3.2。
    fn duplicate_clip_shared(&mut self, source: ClipRef) {
        let Some(track) = self.song.tracks.get(source.track as usize) else {
            return;
        };
        let Some(src_clip) = track.clips.get(source.clip as usize) else {
            return;
        };
        let new_start_beat = src_clip.start_beat + src_clip.length_beats;
        let new_length = src_clip.length_beats;
        let content_id = src_clip.content_id;
        let new_name = src_clip.name.clone();
        let Some(track) = self.song.tracks.get_mut(source.track as usize) else {
            return;
        };
        let new_clip_id = track.alloc_clip_id();
        let new_idx = track.clips.len() as u32;
        track.clips.push(Clip {
            id: new_clip_id,
            name: new_name,
            start_beat: new_start_beat,
            length_beats: new_length,
            content_id,
            notes: Vec::new(),
        });
        let r = ClipRef {
            track: source.track,
            clip: new_idx,
        };
        self.selected_clip = Some(r);
        self.selected_clips = vec![r];
        self.selected_notes.clear();
        self.sync_song_to_plugin_host();
    }

    /// 独立コピー (Alt+D shortcut): 末尾直後に同サイズ、 ただし content を
    /// deep clone + 新 ContentId 採番で独立化。 §3.3。
    fn duplicate_clip_unique(&mut self, source: ClipRef) {
        let Some(track) = self.song.tracks.get(source.track as usize) else {
            return;
        };
        let Some(src_clip) = track.clips.get(source.clip as usize) else {
            return;
        };
        let new_start_beat = src_clip.start_beat + src_clip.length_beats;
        let new_length = src_clip.length_beats;
        let new_name = src_clip.name.clone();
        let src_content_id = src_clip.content_id;
        let cloned_content = self
            .song
            .clip_contents
            .get(&src_content_id)
            .cloned()
            .unwrap_or_default();
        let new_content_id = self.song.alloc_content_id();
        self.song
            .clip_contents
            .insert(new_content_id, cloned_content);
        let Some(track) = self.song.tracks.get_mut(source.track as usize) else {
            return;
        };
        let new_clip_id = track.alloc_clip_id();
        let new_idx = track.clips.len() as u32;
        track.clips.push(Clip {
            id: new_clip_id,
            name: new_name,
            start_beat: new_start_beat,
            length_beats: new_length,
            content_id: new_content_id,
            notes: Vec::new(),
        });
        let r = ClipRef {
            track: source.track,
            clip: new_idx,
        };
        self.selected_clip = Some(r);
        self.selected_clips = vec![r];
        self.selected_notes.clear();
        self.sync_song_to_plugin_host();
    }

    /// arrangement Ctrl+drag → release: 各 (source, drop_start_beat) で
    /// 共有コピーを生成。 元 clip 群はそのまま、 selected_clips は新 clip
    /// 群に置き換える (drag 後に選択が新 clip に移るのは MoveClips と同じ semantics)。
    /// §3.4。
    fn clone_clips_linked(&mut self, entries: &[(ClipRef, u32, f64)]) {
        let mut new_refs = Vec::with_capacity(entries.len());
        for &(source, to_track_id, drop_start) in entries {
            let Some(track) = self.song.tracks.get(source.track as usize) else {
                continue;
            };
            let Some(src_clip) = track.clips.get(source.clip as usize) else {
                continue;
            };
            let new_length = src_clip.length_beats;
            let new_name = src_clip.name.clone();
            let content_id = src_clip.content_id;
            let Some(to_track_idx) = self.song.track_index_by_id(to_track_id) else {
                continue;
            };
            let Some(to_track) = self.song.tracks.get_mut(to_track_idx) else {
                continue;
            };
            let new_clip_id = to_track.alloc_clip_id();
            let new_idx = to_track.clips.len() as u32;
            to_track.clips.push(Clip {
                id: new_clip_id,
                name: new_name,
                start_beat: drop_start.max(0.0),
                length_beats: new_length,
                content_id,
                notes: Vec::new(),
            });
            new_refs.push(ClipRef {
                track: to_track_idx as u32,
                clip: new_idx,
            });
        }
        if !new_refs.is_empty() {
            self.selected_clip = new_refs.last().copied();
            self.selected_clips = new_refs;
            self.selected_notes.clear();
            self.sync_song_to_plugin_host();
        }
    }

    /// arrangement Ctrl+Shift+drag → release: 各 (source, drop_start_beat)
    /// で独立コピーを生成。 §3.5。
    fn clone_clips_independent(&mut self, entries: &[(ClipRef, u32, f64)]) {
        let mut new_refs = Vec::with_capacity(entries.len());
        for &(source, to_track_id, drop_start) in entries {
            let Some(track) = self.song.tracks.get(source.track as usize) else {
                continue;
            };
            let Some(src_clip) = track.clips.get(source.clip as usize) else {
                continue;
            };
            let new_length = src_clip.length_beats;
            let new_name = src_clip.name.clone();
            let src_content_id = src_clip.content_id;
            let cloned_content = self
                .song
                .clip_contents
                .get(&src_content_id)
                .cloned()
                .unwrap_or_default();
            let new_content_id = self.song.alloc_content_id();
            self.song
                .clip_contents
                .insert(new_content_id, cloned_content);
            let Some(to_track_idx) = self.song.track_index_by_id(to_track_id) else {
                continue;
            };
            let Some(to_track) = self.song.tracks.get_mut(to_track_idx) else {
                continue;
            };
            let new_clip_id = to_track.alloc_clip_id();
            let new_idx = to_track.clips.len() as u32;
            to_track.clips.push(Clip {
                id: new_clip_id,
                name: new_name,
                start_beat: drop_start.max(0.0),
                length_beats: new_length,
                content_id: new_content_id,
                notes: Vec::new(),
            });
            new_refs.push(ClipRef {
                track: to_track_idx as u32,
                clip: new_idx,
            });
        }
        if !new_refs.is_empty() {
            self.selected_clip = new_refs.last().copied();
            self.selected_clips = new_refs;
            self.selected_notes.clear();
            self.sync_song_to_plugin_host();
        }
    }

    /// Make Unique (右クリック): 共有 clip → 独立化。 refcount==1 なら no-op。
    /// §3.6。
    fn make_clip_unique(&mut self, target: ClipRef) {
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return;
        };
        let content_id = clip.content_id;
        if self.song.clip_content_refcount(content_id) <= 1 {
            self.status_message = "すでに独立 clip です".to_string();
            return;
        }
        let cloned_content = self
            .song
            .clip_contents
            .get(&content_id)
            .cloned()
            .unwrap_or_default();
        let new_content_id = self.song.alloc_content_id();
        self.song
            .clip_contents
            .insert(new_content_id, cloned_content);
        let Some(track) = self.song.tracks.get_mut(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get_mut(target.clip as usize) else {
            return;
        };
        clip.content_id = new_content_id;
        self.sync_song_to_plugin_host();
        self.status_message = "Clip を独立化しました".to_string();
    }

    fn create_clip(&mut self, track_idx: u32, start_beat: f64) {
        let start_beat = start_beat.max(0.0);
        // Allocate the shared content slot first so the new clip points
        // at a real entry. Orphan content_ids (if track lookup below
        // fails) get reclaimed by `Song::gc_clip_contents` before save.
        let content_id = self.song.alloc_content_id();
        self.song
            .clip_contents
            .insert(content_id, ClipContent::default());
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
            content_id,
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
        let Some(notes) = self
            .song
            .notes_in_clip_mut(track_idx as usize, clip_idx as usize)
        else {
            return;
        };
        let new_idx = notes.len() as u32;
        notes.push(Note {
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
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        for &(idx, beat, pitch) in entries {
            let Some(note) = notes.get_mut(idx as usize) else {
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
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        for &(idx, start, duration) in entries {
            let Some(note) = notes.get_mut(idx as usize) else {
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
        let Some(notes) = self
            .song
            .notes_in_clip_mut(track_idx as usize, clip_idx as usize)
        else {
            return;
        };
        let Some(note) = notes.get_mut(note_idx as usize) else {
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
        if let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        {
            for i in &indices {
                let i = *i as usize;
                if i < notes.len() {
                    notes.remove(i);
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
        let Some(notes) = self
            .song
            .notes_in_clip_mut(clip_ref.track as usize, clip_ref.clip as usize)
        else {
            return;
        };
        let mut changed = false;
        for (id, lyric) in updates {
            if let Some(n) = notes.get_mut(*id as usize) {
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
        // slot 単位での load 状態 cache。 reconcile の slot-level diff
        // (Undo で同 track 内の plugin 構成が変化した場合の同期) で参照。
        self.loaded_slots.insert(
            (track_id, slot),
            LoadedSlotInfo {
                plugin_id,
                plugin_id_str: id.clone(),
            },
        );
        self.ensure_first_track();
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        // PR4.5 sidechain wiring preservation: when a plugin finishes
        // loading via SlotPluginLoaded, we replace the existing
        // PluginInstance with a fresh one carrying the resolved id +
        // saved state, but **must preserve `sidechain_sources`** —
        // otherwise wiring set by the user (or loaded from a saved .daw
        // file) gets clobbered to `Vec::new()` here, which then
        // (a) makes the inspector dropdown display "—" instead of the
        //     wired source track, and (b) propagates to daw_audio via
        //     the next LoadSong, killing the SidechainTap in
        //     `compile_schedule`. The user's symptom: dropdown empty +
        //     no ducking after Open, then everything works once they
        //     re-pick the source manually (which writes Some(id) back).
        match slot {
            PluginSlot::Instrument => {
                let (state, format, existing_sc) = t
                    .instrument
                    .as_ref()
                    .map(|i| (i.state.clone(), i.format, i.sidechain_sources.clone()))
                    .unwrap_or((None, PluginFormat::Clap, Vec::new()));
                t.instrument = Some(common::model::PluginInstance {
                    plugin_id: id,
                    format,
                    state,
                    sidechain_sources: existing_sc,
                });
            }
            PluginSlot::Fx(i) => {
                let i = i as usize;
                let (existing_state, format, existing_sc) = t
                    .fx_chain
                    .get(i)
                    .map(|p| (p.state.clone(), p.format, p.sidechain_sources.clone()))
                    .unwrap_or((None, PluginFormat::Clap, Vec::new()));
                let inst = common::model::PluginInstance {
                    plugin_id: id,
                    format,
                    state: existing_state,
                    sidechain_sources: existing_sc,
                };
                if i < t.fx_chain.len() {
                    t.fx_chain[i] = inst;
                } else {
                    t.fx_chain.push(inst);
                }
            }
            PluginSlot::MidiFx(i) => {
                let i = i as usize;
                let (existing_state, format, existing_sc) = t
                    .midi_fx_chain
                    .get(i)
                    .map(|p| (p.state.clone(), p.format, p.sidechain_sources.clone()))
                    .unwrap_or((None, PluginFormat::Clap, Vec::new()));
                let inst = common::model::PluginInstance {
                    plugin_id: id,
                    format,
                    state: existing_state,
                    sidechain_sources: existing_sc,
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

    /// plugin_host で `SetSlotPlugin` が失敗した (`load_plugin` Err か
    /// `ProcessDataHandle::create` Err) 通知を受けたときの後処理。
    ///
    /// A7 の `track_pending_load` で詰めた `pending_plugin_loads` の
    /// entry が plugin_host 側で消費されないと、 「プラグイン読み込み
    /// 中...」 status のまま `pending_play` が永久に flush されない
    /// (= 再生不能) になる。 失敗 = ロード round-trip 完了 と等価
    /// 扱いで pending を解放し、 必要なら queue Play を flush する。
    ///
    /// Song の slot は touch しない: 旧 plugin が居れば継続再生、 reconcile
    /// 由来で旧無し → slot 空のまま。 ユーザーには status_message でエラー
    /// を表示するだけ。
    fn on_plugin_load_failed_from_child(
        &mut self,
        track: u32,
        slot: PluginSlot,
        plugin_id: String,
        reason: String,
    ) {
        tracing::error!(
            track,
            ?slot,
            %plugin_id,
            %reason,
            "plugin load failed (notified by plugin host)"
        );
        self.pending_plugin_loads.remove(&(track, slot));
        // pending_play 解放: A7 と同じロジック (`on_plugin_loaded_from_child`
        // と対称)。 失敗で空になったタイミングで queue Play を flush する。
        if self.pending_plugin_loads.is_empty() && self.pending_play {
            self.pending_play = false;
            self.status_message =
                format!("プラグイン読み込み失敗: {plugin_id} ({reason})");
            self.play();
        } else if !self.pending_plugin_loads.is_empty() && self.pending_play {
            // まだ他の load が走っているなら、 残数表示を更新しつつエラーは
            // 上書き (最新の状況をユーザーに見せる)。
            self.status_message = format!(
                "プラグイン読み込み失敗: {plugin_id} ({reason}) — 残 {}",
                self.pending_plugin_loads.len()
            );
        } else {
            // pending_play は立っていない (= 再生中じゃなかった or stop 済) ので
            // 単に status にエラーを出すだけ。
            self.status_message =
                format!("プラグイン読み込み失敗: {plugin_id} ({reason})");
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
        // slot 単位 cache からも、 同 plugin_id を持つ entry を retain で外す。
        self.loaded_slots
            .retain(|_, info| info.plugin_id != plugin_id);
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

    /// `AppEvent::RemoveSlot` の dispatcher。 削除する plugin の最新
    /// state を取ってから Undo snapshot + 削除を行う。
    fn remove_slot(&mut self, slot_kind: u8, slot_index: u32) {
        let slot = match slot_kind {
            0 => PluginSlot::MidiFx(slot_index),
            1 => PluginSlot::Instrument,
            _ => PluginSlot::Fx(slot_index),
        };
        let Some(track_idx) = self.cursor_track_index() else {
            return;
        };
        let track_id = self.song.tracks[track_idx].id;

        if !self.song_has_plugin() || self.pending_state_request.is_some() {
            self.push_undo_snapshot();
            self.remove_slot_inner(track_id, slot);
            return;
        }
        self.pending_state_request = Some(PendingStateRequest::Deferred(
            DeferredEdit::RemoveSlot { track_id, slot },
        ));
        self.send_plugin(MainToChild::RequestAllStates);
    }

    fn remove_slot_inner(&mut self, track_id: u32, slot: PluginSlot) {
        let Some(track_idx) = self.song.track_index_by_id(track_id) else {
            return;
        };
        // PR2.1: send `Track::id` to the plugin host.
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
        // slot cache から該当 entry を即時削除。 SlotPluginUnloaded event
        // 到着前に reconcile が走っても stale entry を見ないようにする
        // 防御策。
        self.loaded_slots.remove(&(track_id, slot));
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

    /// `plugin_host` から `AllPluginStates` 受信。 全 plugin の最新
    /// state を Song に書き戻したあと、 [`AppData::pending_state_request`]
    /// に応じた完了処理 (save または deferred edit) を実行する。
    fn on_all_states_from_child(&mut self, states: Vec<SlotState>) {
        // Save / Deferred どちらでも Song の state は最新化したいので
        // ここで一律書き戻す。 pending が None だった場合 (= 想定外
        // タイミングの応答) でも害はない。
        self.apply_plugin_states(&states);
        let Some(req) = self.pending_state_request.take() else {
            return;
        };
        match req {
            PendingStateRequest::Save { path } => self.save_after_states(path),
            PendingStateRequest::Deferred(edit) => {
                // ここで初めて Undo snapshot を push する。 Song に
                // 最新 state が入った状態を捕まえるため (plugin が
                // 削除される編集を Undo すると knob 値が復元される)。
                self.push_undo_snapshot();
                self.execute_deferred_edit(edit);
            }
        }
    }

    /// `AllPluginStates` 受信後に呼ばれる。 deferred edit を実際に実行
    /// する。 inner 関数群は `push_undo_snapshot` を呼ばない (= 上の
    /// `on_all_states_from_child` 側で push 済みであり、 二重 push を
    /// 避けるため)。
    fn execute_deferred_edit(&mut self, edit: DeferredEdit) {
        match edit {
            DeferredEdit::DeleteTrack { track_id } => self.delete_track_inner(track_id),
            DeferredEdit::UngroupTracks { track_ids } => {
                self.action_ungroup_tracks_inner(&track_ids)
            }
            DeferredEdit::RemoveSlot { track_id, slot } => {
                self.remove_slot_inner(track_id, slot)
            }
        }
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
        // Audio event 数値 buffer も song 差し替え (open / new / undo /
        // redo) に追従。 selected_clip が無い / 範囲外 / 非 audio なら
        // resync が target を None 化してくれる。
        match self.selected_clip {
            Some(target) => self.resync_clip_audio_event_edit_buffers(target),
            None => {
                self.clip_edit_buffer_target = None;
                self.clip_gain_db_edit_text.clear();
                self.clip_pan_edit_text.clear();
                self.clip_pitch_edit_text.clear();
            }
        }
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

        // PR8: SetVocalAudio retired in favour of SetGeneratedAudio.
        // The audio engine looks the per-clip vocal buffer up by
        // `vocal_gen_id(track_id, clip_id) = (track_id << 32) | clip_id`
        // — same encoding lives in `daw_audio::engine::vocal_gen_id`.
        // Convert mono samples to planar 1-ch (`Vec<Vec<f32>>`) so the
        // generic audio buffer layout used everywhere else applies.
        let song_snapshot = self.song.clone();
        for r in &ok_results {
            let Some((track_id, clip_id)) = song_snapshot
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize).map(|c| (t.id, c.id)))
            else {
                continue;
            };
            let id = ((track_id as u64) << 32) | (clip_id as u64);
            self.send_audio(MainToChild::SetGeneratedAudio {
                id,
                sample_rate: r.sample_rate,
                channels: 1,
                samples: vec![r.samples.clone()],
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

    /// Import one or more audio files into the song (Phase 1 PR3).
    /// Synchronous — blocks the UI until decode completes (Phase 2
    /// will move this to a background thread; spec §7.4). Each file:
    ///
    /// 1. Hash + copy into `<project_dir>/samples/` (or import_cache
    ///    fallback for unsaved projects, §13 Q2).
    /// 2. Decode (WAV-only in Phase 1).
    /// 3. Allocate `AudioSourceId`, register on `Song.audio_sources`.
    /// 4. Stash decoded buffer in `audio_source_cache`.
    /// 5. Build a single `AudioEvent` covering the whole source and
    ///    wrap it in a fresh `ClipContent::Audio` content. Place a
    ///    `Clip` on the cursor track at the playhead. Phase 2 / PR4
    ///    refines drop-coordinate → (track, beat) resolution.
    ///
    /// Failures (unsupported format, oversize, decode error) surface
    /// in `status_message`; partial progress (= some files succeeded)
    /// is preserved.
    /// File menu → "Import Audio..." 経路。 `rfd` の native file picker
    /// (multi-select、 WAV filter) を開いて、 選択された path を
    /// `action_import_audio` に転送するだけのラッパ。 dialog をキャンセル
    /// した場合は no-op。 起点が違うだけで採番 / dedup / コピー / decode
    /// は drag&drop と完全に同じ pipeline。
    fn action_open_import_audio_dialog(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("WAV", &["wav"])
            .set_title("Import Audio")
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        self.action_import_audio(paths);
    }

    /// PR-D 段階 3: Audio Editor の context menu "Add From Source..."。
    /// `rfd` で 1 ファイル選択 → `AddAudioEventFromFile` に転送 (= 内部
    /// で `import_audio::import_one` 経由で decode + AudioSource 採番)。
    /// `position_in_clip_beats` は呼び出し側 (= context menu 発火位置 =
    /// 直前 event の右端) で決定。 `handle_event` 経由なので auto Undo
    /// snapshot が積まれる。
    pub fn action_open_audio_event_dialog(
        &mut self,
        target: ClipRef,
        position_in_clip_beats: f64,
    ) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("WAV", &["wav"])
            .set_title("Add Audio Event")
            .pick_file()
        else {
            return;
        };
        self.handle_event(AppEvent::AddAudioEventFromFile {
            clip: target,
            path,
            position_in_clip_beats,
        });
    }

    fn action_import_audio(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let project_dir: Option<PathBuf> = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // Phase 1 PR3: drop coordinate → track resolution は gui_01 #023
        // 待ち (`take_file_drop_in_rect` の戻り値に position を含める要望)。
        // 暫定として「最後に選択した track」 (= cursor track) に配置する。
        // 未選択時は最初の track にフォールバック。 gui_01 #023 が解決
        // したら drop 座標から track を解決する形に置き換える (PR4)。
        let target_track_idx: usize = self.cursor_track_index().unwrap_or(0);
        let start_beat_seed: f64 = self.playhead_beat.unwrap_or(0.0) as f64;
        if self.song.tracks.is_empty() {
            self.status_message =
                "Audio import: 配置先のトラックが無いため取り込めません".to_string();
            return;
        }

        let bpm = self.song.bpm;
        let mut imported_ok = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut next_start_beat = start_beat_seed.max(0.0);

        for path in paths {
            let imported = match import_audio::import_one(&path, project_dir.as_deref()) {
                Ok(i) => i,
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };

            let length_beats =
                frames_to_beats(imported.buffer.frames, imported.buffer.sample_rate, bpm);

            let source_id = self.song.alloc_audio_source_id();
            self.song.audio_sources.insert(source_id, imported.source);
            self.audio_source_cache.insert(source_id, imported.buffer.clone());

            let event = AudioEvent {
                source_id,
                event_start_in_clip_beats: 0.0,
                event_length_beats: length_beats,
                source_start_frames: 0,
                source_end_frames: imported.buffer.frames,
                ..AudioEvent::default()
            };
            let content_id = self.song.alloc_content_id();
            self.song.clip_contents.insert(
                content_id,
                ClipContent::Audio(AudioContent {
                    events: vec![event],
                }),
            );

            let display_name = imported.display_name.clone();
            let track = &mut self.song.tracks[target_track_idx];
            let new_clip_id = track.alloc_clip_id();
            track.clips.push(Clip {
                id: new_clip_id,
                name: display_name,
                start_beat: next_start_beat,
                length_beats,
                content_id,
                notes: Vec::new(),
            });
            next_start_beat += length_beats;
            imported_ok += 1;
        }

        if imported_ok > 0 {
            self.is_dirty = true;
            self.sync_song_to_plugin_host();
        }

        self.status_message = match (imported_ok, errors.is_empty()) {
            (0, false) => format!("Audio import 失敗: {}", errors.join(" / ")),
            (n, true) => format!("Audio import 完了: {n} ファイル"),
            (n, false) => format!(
                "Audio import: {n} ファイル成功、 {} 件エラー: {}",
                errors.len(),
                errors.join(" / ")
            ),
        };
    }

    /// Split clip(s) at the cursor (= mouse hover beat).
    ///
    /// If `snap` is `true`, uses the snapped beat; otherwise the raw
    /// beat (for `Alt+E` snap-temporarily-off flow). Falls back to the
    /// playhead when the cursor is outside the canvas. Targets are:
    ///
    /// 1. The clip the cursor is hovering over
    ///    (`arrangement_hover_clip`).
    /// 2. If no hover, the current `selected_clips` (multi-clip split
    ///    at the same beat).
    /// 3. If neither, surfaces a status message.
    ///
    /// The back half of each split clip receives a fresh `ContentId`
    /// (= leaves any share group, Make Unique-equivalent semantics).
    /// Works on MIDI / Audio / Vocal clips alike. See
    /// `docs/plan_audio_clip.md` §3.3 / §3.3.1.
    fn action_split_clips_at_cursor(&mut self, snap: bool) {
        let cursor: f64 = if snap {
            self.arrangement_hover_beat
                .or(self.arrangement_hover_beat_raw)
                .or_else(|| self.playhead_beat.map(|b| b as f64))
                .unwrap_or(-1.0)
        } else {
            self.arrangement_hover_beat_raw
                .or(self.arrangement_hover_beat)
                .or_else(|| self.playhead_beat.map(|b| b as f64))
                .unwrap_or(-1.0)
        };
        if cursor < 0.0 {
            self.status_message =
                "Split: マウスを arrangement に置くか再生中に E を押してください".into();
            return;
        }
        // Build targets list. Prefer hover clip, fall back to selection.
        let targets: Vec<ClipRef> = if let Some(hover) = self.arrangement_hover_clip {
            vec![hover]
        } else if !self.selected_clips.is_empty() {
            self.selected_clips.clone()
        } else {
            self.status_message =
                "Split: clip にマウスを乗せるか clip を選択してください".into();
            return;
        };
        let mut split_count = 0usize;
        let mut new_selection: Vec<ClipRef> = Vec::new();
        for src in &targets {
            if self.split_clip_at_beat(*src, cursor, &mut new_selection) {
                split_count += 1;
            }
        }
        if split_count == 0 {
            self.status_message =
                "Split: カーソルが clip 範囲外のため何も分割されませんでした".into();
            return;
        }
        if !new_selection.is_empty() {
            self.selected_clip = new_selection.last().copied();
            self.selected_clips = new_selection;
            self.selected_notes.clear();
        }
        self.status_message = format!("Split: {split_count} clip を分割しました");
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
    }

    /// Single-clip split helper. Returns `true` iff the playhead lay
    /// strictly inside the clip and the split actually happened. The
    /// new (back-half) clip is appended to `new_selection` so the
    /// caller can update the selection afterwards.
    fn split_clip_at_beat(
        &mut self,
        target: ClipRef,
        playhead: f64,
        new_selection: &mut Vec<ClipRef>,
    ) -> bool {
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return false;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return false;
        };
        let clip_start = clip.start_beat;
        let clip_len = clip.length_beats;
        let clip_end = clip_start + clip_len;
        if !(playhead > clip_start && playhead < clip_end) {
            return false; // playhead 範囲外 / 端ぴったりは split 不要
        }
        let split_offset = playhead - clip_start;
        let front_len = split_offset;
        let back_len = clip_len - split_offset;
        let src_content_id = clip.content_id;
        let src_name = clip.name.clone();
        let Some(src_content) = self.song.clip_contents.get(&src_content_id).cloned()
        else {
            return false;
        };

        // Build the back-half ClipContent by partitioning the source
        // content at `split_offset` (clip-local beats).
        let back_content = match src_content.clone() {
            ClipContent::Midi(mut midi) => {
                let mut back_notes: Vec<Note> = Vec::new();
                let mut keep_front: Vec<Note> = Vec::new();
                for note in midi.notes.drain(..) {
                    let n_start = note.start_beat;
                    let n_end = note.start_beat + note.duration_beats;
                    if n_end <= split_offset {
                        keep_front.push(note);
                    } else if n_start >= split_offset {
                        back_notes.push(Note {
                            start_beat: n_start - split_offset,
                            ..note
                        });
                    } else {
                        // Note straddles the split point — front half
                        // keeps lyric, back half is a continuation
                        // (no lyric so VOICEVOX doesn't sing it twice).
                        let front_dur = split_offset - n_start;
                        let back_dur = n_end - split_offset;
                        keep_front.push(Note {
                            start_beat: n_start,
                            duration_beats: front_dur,
                            ..note.clone()
                        });
                        back_notes.push(Note {
                            start_beat: 0.0,
                            duration_beats: back_dur,
                            lyric: None,
                            ..note
                        });
                    }
                }
                // Trim the original (front) content in place so the
                // share group keeps the front half only — but only
                // for THIS clip's content; if other clips share the
                // same `content_id` we must fork via a fresh id. We
                // always fork here for simplicity (= split always
                // promotes both halves to fresh ContentIds, which is
                // safer for shared-clip semantics).
                let mut front = MidiContent { notes: keep_front };
                front.notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
                let mut back = MidiContent { notes: back_notes };
                back.notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
                let front_id = self.song.alloc_content_id();
                self.song
                    .clip_contents
                    .insert(front_id, ClipContent::Midi(front));
                ClipContent::Midi(back)
            }
            ClipContent::Audio(mut audio) => {
                let mut back_events: Vec<AudioEvent> = Vec::new();
                let mut keep_front: Vec<AudioEvent> = Vec::new();
                for ev in audio.events.drain(..) {
                    let e_start = ev.event_start_in_clip_beats;
                    let e_end = e_start + ev.event_length_beats;
                    if e_end <= split_offset {
                        keep_front.push(ev);
                    } else if e_start >= split_offset {
                        back_events.push(AudioEvent {
                            event_start_in_clip_beats: e_start - split_offset,
                            ..ev
                        });
                    } else {
                        // Event straddles the split: split source range
                        // proportionally by the source-frame stride
                        // implied by this event's pitch_ratio is
                        // approximated as a simple linear partition
                        // (good enough for Phase 1 default Raw mode
                        // where source beats == clip beats × bpm).
                        let frac_front = (split_offset - e_start) / ev.event_length_beats;
                        let total_src = ev
                            .source_end_frames
                            .saturating_sub(ev.source_start_frames);
                        let split_src_offset =
                            (total_src as f64 * frac_front).round() as u64;
                        let mid_src_frame = ev.source_start_frames + split_src_offset;
                        let mut front_ev = ev.clone();
                        front_ev.event_length_beats = split_offset - e_start;
                        front_ev.source_end_frames = mid_src_frame;
                        keep_front.push(front_ev);
                        back_events.push(AudioEvent {
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: e_end - split_offset,
                            source_start_frames: mid_src_frame,
                            ..ev
                        });
                    }
                }
                let front = AudioContent { events: keep_front };
                let back = AudioContent { events: back_events };
                let front_id = self.song.alloc_content_id();
                self.song
                    .clip_contents
                    .insert(front_id, ClipContent::Audio(front));
                ClipContent::Audio(back)
            }
        };

        // Allocate fresh ContentIds for both halves (front was just
        // inserted into clip_contents above with a placeholder id —
        // we now rewrite the clip's content_id to point at it).
        // Strategy: walk back the last alloc'd id we just inserted.
        // The id list above used `alloc_content_id()` so the most
        // recent one is `next_content_id - 1`.
        let front_content_id = self.song.next_content_id.saturating_sub(1);
        let back_content_id = self.song.alloc_content_id();
        self.song
            .clip_contents
            .insert(back_content_id, back_content);

        // Mutate the clip in place: front half stays as `clip`
        // (length / content_id rewritten), and a new clip for the
        // back half is appended on the same track.
        let track = &mut self.song.tracks[target.track as usize];
        {
            let clip_mut = &mut track.clips[target.clip as usize];
            clip_mut.length_beats = front_len;
            clip_mut.content_id = front_content_id;
        }
        let new_clip_id = track.alloc_clip_id();
        let new_idx = track.clips.len() as u32;
        track.clips.push(Clip {
            id: new_clip_id,
            name: src_name,
            start_beat: clip_start + front_len,
            length_beats: back_len,
            content_id: back_content_id,
            notes: Vec::new(),
        });
        new_selection.push(target);
        new_selection.push(ClipRef {
            track: target.track,
            clip: new_idx,
        });
        true
    }

    /// Glue (Consolidate) the currently selected clips into one clip
    /// per track. Mixed-kind selections (MIDI + Audio etc.) are
    /// rejected with a status message. See `docs/plan_audio_clip.md`
    /// §3.3 / §3.3.2.
    fn action_glue_selected_clips(&mut self) {
        if self.selected_clips.len() < 2 {
            self.status_message = format!(
                "Glue: 2 つ以上の clip を選択してください (現在 {} 個)",
                self.selected_clips.len()
            );
            return;
        }

        // Group selected clips by track.
        let mut by_track: std::collections::BTreeMap<u32, Vec<ClipRef>> =
            std::collections::BTreeMap::new();
        for r in &self.selected_clips {
            by_track.entry(r.track).or_default().push(*r);
        }

        let mut new_refs: Vec<ClipRef> = Vec::new();
        let mut glued_count = 0usize;
        let mut had_mixed_kind = false;

        for (track_idx, mut refs) in by_track {
            if refs.len() < 2 {
                continue;
            }
            // Sort by start_beat ascending (clip indices may differ).
            refs.sort_by(|a, b| {
                let ta = self
                    .song
                    .tracks
                    .get(a.track as usize)
                    .and_then(|t| t.clips.get(a.clip as usize))
                    .map(|c| c.start_beat)
                    .unwrap_or(f64::INFINITY);
                let tb = self
                    .song
                    .tracks
                    .get(b.track as usize)
                    .and_then(|t| t.clips.get(b.clip as usize))
                    .map(|c| c.start_beat)
                    .unwrap_or(f64::INFINITY);
                ta.total_cmp(&tb)
            });

            // Detect mixed kinds.
            let mut kind_audio: Option<bool> = None;
            for r in &refs {
                let Some(track) = self.song.tracks.get(r.track as usize) else {
                    continue;
                };
                let Some(clip) = track.clips.get(r.clip as usize) else {
                    continue;
                };
                let Some(content) = self.song.clip_contents.get(&clip.content_id)
                else {
                    continue;
                };
                let is_audio = matches!(content, ClipContent::Audio(_));
                match kind_audio {
                    None => kind_audio = Some(is_audio),
                    Some(prev) if prev != is_audio => {
                        had_mixed_kind = true;
                        break;
                    }
                    _ => {}
                }
            }
            if had_mixed_kind {
                continue;
            }
            let is_audio_kind = kind_audio.unwrap_or(false);

            // Compute combined range + collect content fragments.
            let mut combined_start = f64::INFINITY;
            let mut combined_end = f64::NEG_INFINITY;
            let mut combined_name = String::new();
            #[derive(Default)]
            struct Fragments {
                midi_notes: Vec<Note>,
                audio_events: Vec<AudioEvent>,
            }
            let mut frags = Fragments::default();

            for r in &refs {
                let Some(track) = self.song.tracks.get(r.track as usize) else {
                    continue;
                };
                let Some(clip) = track.clips.get(r.clip as usize) else {
                    continue;
                };
                let s = clip.start_beat;
                let e = s + clip.length_beats;
                if combined_name.is_empty() {
                    combined_name = clip.name.clone();
                }
                combined_start = combined_start.min(s);
                combined_end = combined_end.max(e);
                let Some(content) = self.song.clip_contents.get(&clip.content_id)
                else {
                    continue;
                };
                let offset_into_combined = s - combined_start;
                match content {
                    ClipContent::Midi(midi) => {
                        for note in &midi.notes {
                            frags.midi_notes.push(Note {
                                start_beat: note.start_beat + offset_into_combined,
                                ..note.clone()
                            });
                        }
                    }
                    ClipContent::Audio(audio) => {
                        for ev in &audio.events {
                            frags.audio_events.push(AudioEvent {
                                event_start_in_clip_beats: ev.event_start_in_clip_beats
                                    + offset_into_combined,
                                ..ev.clone()
                            });
                        }
                    }
                }
            }
            if !combined_start.is_finite() || !combined_end.is_finite() {
                continue;
            }

            // Re-walk to fix offsets now that we know combined_start.
            // (The first pass used a tentative `combined_start` that
            // updated as we iterated; re-shift everything by the
            // delta between the first clip's start and the actual
            // combined_start. In sorted order they should already
            // match since clips are sorted by start_beat and
            // combined_start = first clip's start, so the no-op case
            // is the common one — but be defensive.)

            let combined_len = combined_end - combined_start;
            let new_content_id = self.song.alloc_content_id();
            let new_content = if is_audio_kind {
                ClipContent::Audio(AudioContent {
                    events: frags.audio_events,
                })
            } else {
                let mut notes = frags.midi_notes;
                notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
                ClipContent::Midi(MidiContent { notes })
            };
            self.song.clip_contents.insert(new_content_id, new_content);

            // Remove source clips (descending index to keep earlier
            // indices stable).
            let track = &mut self.song.tracks[track_idx as usize];
            let mut indices: Vec<usize> =
                refs.iter().map(|r| r.clip as usize).collect();
            indices.sort_unstable();
            indices.dedup();
            for &idx in indices.iter().rev() {
                if idx < track.clips.len() {
                    track.clips.remove(idx);
                }
            }
            // Append the merged clip.
            let new_clip_id = track.alloc_clip_id();
            let new_idx = track.clips.len() as u32;
            track.clips.push(Clip {
                id: new_clip_id,
                name: combined_name,
                start_beat: combined_start,
                length_beats: combined_len,
                content_id: new_content_id,
                notes: Vec::new(),
            });
            new_refs.push(ClipRef {
                track: track_idx,
                clip: new_idx,
            });
            glued_count += 1;
        }

        if had_mixed_kind {
            tracing::warn!("Glue rejected: mixed kinds");
            self.status_message =
                "Glue: MIDI / Audio / Vocal clip が混在しているため Glue できません".into();
            return;
        }
        if glued_count == 0 {
            tracing::warn!("Glue: glued_count==0 (no track had 2+ clips)");
            self.status_message =
                "Glue: 同じ track 上で 2 つ以上の clip を選択してください".into();
            return;
        }

        tracing::info!(glued_count, ?new_refs, "Glue completed");
        self.selected_clip = new_refs.last().copied();
        self.selected_clips = new_refs;
        self.selected_notes.clear();
        self.status_message = format!("Glue: {glued_count} 箇所を結合しました");
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
    }
}

// ---------------------------------------------------------------------------
// Free standing helpers
// ---------------------------------------------------------------------------

/// frames @ source_sr → beats @ project bpm. Used to size newly
/// imported audio clips so the visual length matches the file
/// duration at the project's current tempo.
fn frames_to_beats(frames: u64, sample_rate: u32, bpm: f32) -> f64 {
    if sample_rate == 0 || bpm <= 0.0 {
        return 0.0;
    }
    let secs = frames as f64 / sample_rate as f64;
    secs * (bpm as f64) / 60.0
}


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
        // Sentinel: caller is expected to allocate a content_id and
        // move `notes` into `Song.clip_contents` via `ensure_clip_contents`
        // (or by constructing the clip via `Song::create_empty_clip` +
        // pushing notes into the content store directly).
        content_id: 0,
        notes,
    }
}
