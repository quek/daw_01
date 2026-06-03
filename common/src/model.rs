use std::collections::HashMap;
use std::path::PathBuf;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::plugin_format::PluginFormat;
use crate::protocol::PluginSlot;
use crate::scale::ScaleChange;

/// `21` 口パク (lip-sync): vocal track に `lipsync_target_track: Option<u32>`
/// (口パク画像を焼き込む立ち絵 group 内 image track の id)、口 track に
/// `mouth_map: Option<MouthMap>` (口形状 7 種 → ImageSourceId)、`Clip` に
/// `auto_lipsync: bool` (自動生成 clip 印、再生成で全置換) が追加される。VOICEVOX
/// の phoneme タイミングから口画像を `ImageEvent` 列として生成する派生データで、
/// SSoT は vocal の notes+lyric + `mouth_map`。v20 `.daw` files still load — 全 field
/// が `#[serde(default)]` で forward-migrate (binding / map は `None`、`auto_lipsync`
/// は `false`)。See `docs/plan_pakupaku.md`.
///
/// `20` 共有クリップ名: `Song.clip_content_names: HashMap<ContentId, String>`
/// が追加。 同 `content_id` を共有する全 clip の表示名をここで 1 実体共有し、
/// 片方を rename すると linked clip 全部に連動する。 legacy per-clip
/// `Clip.name` / `AutomationClip.name` は deserialize-only に降格し、
/// `Song::ensure_clip_contents` が load 時に map へ drain する (v5→v6 の
/// `Clip.notes` 移管と同 idiom)。 v19 `.daw` files still load —
/// `clip_content_names` defaults to empty で、 各 clip の legacy `name` から
/// backfill される (共有 content は最初に見た非空名を採用)。
/// See `docs/plan_clip_shared_name.md`.
///
/// `19` 立ち絵 group transform: `Track.group_transform: Option<GroupTransform>`
/// (位置 X/Y・回転・非一様スケール ScaleX/ScaleY・任意アンカー AnchorX/AnchorY・Opacity の
/// 2D affine。AE の Transform プロパティ群と同構成) と
/// `AutomationTarget::GroupTransform(GroupTransformParam)` が追加される。親グループトラック
/// (= 子が `parent_group_id` で指すトラック) が合成済み立ち絵 1 枚にかける transform で、
/// 純粋に visual (daw_audio は評価しない)。v18 `.daw` files still load — `group_transform`
/// defaults to `None` (per `#[serde(default)]`)、appended enum variant も forward-compatible。
/// See `docs/plan_tachie_group_transform.md`.
///
/// Previously:
///   `18` Track / Clip color: `Track.color: Option<[f32; 3]>` and
/// `Clip.color: Option<[f32; 3]>` are added (RGB, opaque). For a track,
/// `None` means "derive a stable palette color from the track id"
/// (auto-assignment; reorder-stable because it keys off the id, not the
/// index) and `Some(rgb)` is a user override. For a clip, `None` means
/// "inherit the owning track's effective color" and `Some(rgb)` is a
/// per-clip override; resetting a clip back to `None` is the Ableton-style
/// "match track color" action. v17 `.daw` files still load — both fields
/// default to `None` (per `#[serde(default)]`), i.e. tracks render their
/// derived palette color and clips inherit. The color is a model value
/// only; the renderer-side `daw_ui_renderer::Color` conversion and the
/// palette live in `daw_gui` (view layer). See
/// `docs/plan_track_clip_color.md`.
///
///   `17` Aux send / return: `Track.sends: Vec<Send>` is added — each
/// `Send` is a parallel, gain-scaled copy of the track's signal routed
/// into a destination "return" track's input bus (the source's own
/// signal still reaches its parent / master untouched). v16 `.daw`
/// files still load — `sends` defaults to empty (per `#[serde(default)]`,
/// i.e. no sends). The destination is any existing track (Reaper /
/// Ardour unified bus model); a "return" is *derived* (a track that has
/// incoming sends), not a distinct `TrackKind`. See
/// `docs/plan_routing_graph.md`.
///
/// Bumped to `13` for Image overlay (PiP): `Song.image_sources` pool +
/// `next_image_source_id`, and `ClipContent::Image(ImageContent {
/// events: Vec<ImageEvent> })` variant are added. v12 `.daw` files
/// still load — `image_sources` defaults to empty (per
/// `#[serde(default)]`), `next_image_source_id` defaults to `0`. The
/// new `Image` variant under `#[serde(untagged)]` is disambiguated
/// from `Audio` / `Video` by the disjoint required field `opacity`
/// inside `ImageEvent` (= absent from both `AudioEvent` and
/// `VideoEvent`), and `deny_unknown_fields` on each variant's content
/// struct prevents accidental wide-match. See `docs/plan_image_overlay.md`.
///
/// Previously:
///   `12` Video editing: `Track.kind: TrackKind { Audio, Video }`
///   discriminator, `Song.video_sources` pool +
///   `next_video_source_id` + `video_resolution` + `video_framerate`,
///   and `ClipContent::Video(VideoContent { events: Vec<VideoEvent> })`
///   variant are added. v11 `.daw` files still load — `Track.kind`
///   defaults to `Audio` (per `#[serde(default)]`), `video_sources` is
///   empty, `video_resolution` defaults to `(1920, 1080)`, and
///   `video_framerate` defaults to `30.0`. `ClipContent::Video` is
///   distinguished from `Audio` under `#[serde(untagged)]` by the
///   disjoint required-field pair `source_start_micros` (Video) vs
///   `source_start_frames` (Audio) inside the inner event struct — a
///   JSON missing one's required field falls through to the other.
///   See `docs/plan_video.md`.
///
///   `11` Scale &amp; Root: `Song.scale_changes: Vec<ScaleChange>` is
///   added. v10 `.daw` files still load — the field defaults to an
///   empty Vec (per `#[serde(default)]`), which is the "Scale feature
///   OFF / chromatic" mode and matches the legacy behavior exactly.
///   See `docs/plan_scale.html`.
///
///   `8` parameter automation: `Track.automation_lanes` is added
///   (per-target lane with a default value, an enabled toggle and
///   clip-shaped point lists) and `ClipContent` gains an
///   `Automation(AutomationContent { points })` variant. v7 `.daw`
///   files still load — `automation_lanes` defaults to empty (per
///   `#[serde(default)]`), and existing `Midi` / `Audio` variants of
///   `ClipContent` are unaffected because the new `Automation` variant
///   has a disjoint field set (`points` vs `notes` / `events`) under
///   `#[serde(untagged)]`. See `docs/plan_automation.md`.
///
///   `7` audio clip / WAV import (`ClipContent` enum `{ Midi, Audio }`
///   and `Song.audio_sources`); `6` shared/linked clip (notes moved
///   into `Song.clip_contents` keyed by `Clip.content_id`, REAPER
///   pooled MIDI model); `5` routing graph + plugin latency cache;
///   `4` per-`Clip` `volume` moved onto `Track::volume`; `3` was a
///   brief detour.
pub const CURRENT_VERSION: u32 = 21;

/// Stable id for shared clip content (notes). Allocated by
/// `Song::alloc_content_id` and referenced by `Clip::content_id`.
/// `0` is the "未採番" sentinel — `Song::ensure_clip_contents` reassigns
/// any zero-valued `content_id` on load.
pub type ContentId = u32;

/// Serde adapter for `Option<Vec<u8>>` that writes binary data as base64 in
/// JSON (and other human-readable formats). Bincode bypasses this and uses
/// native length-prefixed bytes via the `Encode`/`Decode` derives.
pub mod base64_opt {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => ser.serialize_some(&STANDARD.encode(b)),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let s: Option<String> = Option::deserialize(de)?;
        match s {
            Some(s) => STANDARD
                .decode(s.as_bytes())
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    pub song: Song,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Song {
    pub bpm: f32,
    pub time_sig: (u8, u8),
    pub length_beats: f64,
    #[serde(default)]
    pub tracks: Vec<Track>,
    /// User-defined playback loop region (beats). When `loop_end_beat <=
    /// loop_start_beat` (e.g. both zero — the default for new / older
    /// projects), the engine falls back to looping over the full song
    /// content envelope.
    #[serde(default)]
    pub loop_start_beat: f64,
    #[serde(default)]
    pub loop_end_beat: f64,
    /// Stable id allocator for `Track`. Bumped each time a new track is
    /// created; never reused even after deletion. `0` is reserved as
    /// "未採番" sentinel — assigned the first available id at allocation
    /// time.
    #[serde(default)]
    pub next_track_id: u32,
    /// Shared clip content store. Each `Clip.content_id` references one
    /// entry here; multiple clips with the same `content_id` share the
    /// same `notes` (linked / pooled clips, REAPER pooled MIDI model).
    /// Entries with refcount == 0 are GC'd by `Song::gc_clip_contents`
    /// before save.
    #[serde(default)]
    pub clip_contents: HashMap<ContentId, ClipContent>,
    /// Stable id allocator for `ContentId`. `0` is the sentinel; valid
    /// allocations start at `1`.
    #[serde(default)]
    pub next_content_id: ContentId,
    /// v20: shared clip display name, keyed by `ContentId`. Every clip
    /// sharing a `content_id` (linked clips) shares the same name — rename
    /// one and all update. This is the SSoT for clip names; the legacy
    /// per-clip `Clip.name` / `AutomationClip.name` fields are
    /// deserialize-only and drained into this map by
    /// `Song::ensure_clip_contents` on load (mirroring the v5→v6
    /// `Clip.notes` → `clip_contents` migration). Lifecycle follows
    /// `clip_contents`: `gc_clip_contents` prunes dead ids here too.
    /// v19 files forward-migrate to a map backfilled from `Clip.name`.
    #[serde(default)]
    pub clip_content_names: HashMap<ContentId, String>,
    /// Pool of imported audio file references (WAV / generated). Each
    /// entry is keyed by `AudioSourceId` and shared by every
    /// `AudioEvent.source_id` that points at it. Decoded sample buffers
    /// are NOT stored here — only metadata (path / sample_rate / channels
    /// / frames). The actual buffers are decoded independently in each
    /// process (GUI / audio engine) from the path. Entries with refcount
    /// == 0 are GC'd by `Song::gc_audio_sources` before save.
    #[serde(default)]
    pub audio_sources: HashMap<AudioSourceId, AudioSource>,
    /// Stable id allocator for `AudioSourceId`. `0` is the sentinel; valid
    /// allocations start at `1`.
    #[serde(default)]
    pub next_audio_source_id: AudioSourceId,
    /// Phase 5 (`docs/plan_automation.md` §10 Phase 5): song-level
    /// automation lanes (`AutomationTarget::SongTempo` /
    /// `SongTimeSigNumerator`)。 master lane に相当し、 track ではなく
    /// Song 自身に紐付く。 既存 `Track.automation_lanes` と同 schema
    /// (= 同 `AutomationLane` struct を再利用) を使い、 clip 内 points も
    /// `clip_contents` map を共有する。 audio engine は SongTempo lane
    /// を per-buffer 評価して `playhead → beat` 換算に使う (Step 5.2)。
    /// 未設定なら従来通り `Song.bpm` を constant tempo として使う。
    #[serde(default)]
    pub song_lanes: Vec<AutomationLane>,
    /// Stable id allocator for `AutomationLane` ids in `song_lanes`。 0 は
    /// "未採番" sentinel、 1 から採番。
    #[serde(default)]
    pub next_song_lane_id: u32,
    /// Phase 7 B1-M Step 2-3 (`docs/plan_b1_vst3_completion.md`): MIDI Learn の
    /// CC → param バインディング table。 GUI 側で「MIDI Learn」 button 経由
    /// で user が CC を bind、 audio engine 側は使わない (= GUI の
    /// `handle_midi_control_change` が lookup → set_track_volume 等の既存
    /// path で値送信する)。 Project save 対象 (= 起動間で永続化)。 v9 file は
    /// 空 Vec で forward-migrate。
    #[serde(default)]
    pub midi_bindings: Vec<MidiBinding>,
    /// Phase 7 B5 (`docs/plan_scale.html`): タイムライン上の root + scale 変化点。
    /// `beat` 昇順で保持 (= `scale_at(beat)` が rev-find で動く invariant)。
    /// 空 Vec なら Scale 機能 OFF (chromatic 互換、 既存 project と完全互換)。
    /// 単一キーの楽曲なら `beat = 0` の event 1 件、 転調は 2 件目以降を追加。
    /// v10 file は `#[serde(default)]` で空 Vec で forward-migrate。
    #[serde(default)]
    pub scale_changes: Vec<ScaleChange>,
    /// v12 (`docs/plan_video.md` §2.3): pool of imported video file
    /// references, keyed by `VideoSourceId`. Decoded frames are NOT
    /// stored here — only metadata (path / width / height / framerate /
    /// duration / codec). Frames are decoded on demand by daw_gui's
    /// video worker thread. Entries with refcount == 0 are GC'd by
    /// `Song::gc_video_sources` before save. v11 file forward-migrates
    /// to an empty map.
    #[serde(default)]
    pub video_sources: HashMap<VideoSourceId, VideoSource>,
    /// v12: stable id allocator for `VideoSourceId`. `0` is the
    /// sentinel; valid allocations start at `1`. v11 file forward-
    /// migrates to `0`, then `ensure_video_source_ids` lifts it.
    #[serde(default)]
    pub next_video_source_id: VideoSourceId,
    /// v12 (`docs/plan_video.md` §2.3): project-level video output
    /// resolution `(width, height)` in pixels. Drives preview window
    /// scale + render output dimensions. All imports are letterboxed
    /// onto this canvas (preview composites at this size; render
    /// encodes at this size). v11 file forward-migrates to
    /// `(1920, 1080)` (= 1080p default).
    #[serde(default = "default_video_resolution")]
    pub video_resolution: (u32, u32),
    /// v12: project-level video output framerate in Hz. v11 file
    /// forward-migrates to `30.0`.
    #[serde(default = "default_video_framerate")]
    pub video_framerate: f32,
    /// v13 (`docs/plan_image_overlay.md` §2.3): pool of imported image
    /// file references (PNG / JPEG / WebP / static), keyed by
    /// `ImageSourceId`. Decoded BGRA8 bytes are NOT stored here — only
    /// metadata (path / width / height / format). The bytes are
    /// decoded once at import time and uploaded to a GPU
    /// `TextureHandle` cached by `PreviewWindowState`. Entries with
    /// refcount == 0 are GC'd by `Song::gc_image_sources` before save.
    /// v12 file forward-migrates to an empty map.
    #[serde(default)]
    pub image_sources: HashMap<ImageSourceId, ImageSource>,
    /// v13: stable id allocator for `ImageSourceId`. `0` is the
    /// sentinel; valid allocations start at `1`. v12 file forward-
    /// migrates to `0`, then `ensure_image_source_ids` lifts it.
    #[serde(default)]
    pub next_image_source_id: ImageSourceId,
    /// master bus の audio fx chain。 通常 track の `Track.fx_chain` と同 schema
    /// (= 同 `PluginInstance` を再利用)。 master は instrument / midi_fx を持たず、
    /// audio fx のみ持つ (master bus に instrument / arpeggiator は無意味)。 automation の
    /// `song_lanes` と同じく「master 固有データは Track ではなく Song 直下に置く」
    /// 既存パターン (`automation_lane_by_key_mut` 参照) の踏襲。 audio engine は全
    /// track mix 後・metronome 前に `(MASTER_TRACK_ID, PluginSlot::Fx(i))` keying で
    /// 直列 process する。 旧 file は `#[serde(default)]` で空 Vec に forward-migrate。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub master_fx_chain: Vec<PluginInstance>,
}

fn default_video_resolution() -> (u32, u32) {
    (1920, 1080)
}

fn default_video_framerate() -> f32 {
    30.0
}

impl Default for Song {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            time_sig: (4, 4),
            length_beats: 64.0,
            tracks: Vec::new(),
            loop_start_beat: 0.0,
            loop_end_beat: 0.0,
            next_track_id: 1,
            clip_contents: HashMap::new(),
            next_content_id: 1,
            clip_content_names: HashMap::new(),
            audio_sources: HashMap::new(),
            next_audio_source_id: 1,
            song_lanes: Vec::new(),
            next_song_lane_id: 1,
            midi_bindings: Vec::new(),
            scale_changes: Vec::new(),
            video_sources: HashMap::new(),
            next_video_source_id: 1,
            video_resolution: default_video_resolution(),
            video_framerate: default_video_framerate(),
            image_sources: HashMap::new(),
            next_image_source_id: 1,
            master_fx_chain: Vec::new(),
        }
    }
}

/// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn binding 1 件 (= CC → target)。
/// `channel = 16` は any-channel (= channel-agnostic、 全 16 channel にマッチ)。
/// 同じ `(channel, controller)` の重複は許容しない (= GUI 側 handler が
/// 新規 bind 時に既存 entry を replace する)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MidiBinding {
    /// MIDI channel 0..15、 または 16 = any-channel (= channel 無視で match)。
    pub channel: u8,
    /// MIDI CC 番号 0..127。
    pub controller: u8,
    /// CC 値が変化したときに更新する parameter。
    pub target: BindingTarget,
}

/// Phase 7 B1-M Step 2-4 (2026-05-13): MIDI Learn の bind 先。 段階 2 で
/// TrackVolume / TrackPan / SongTempo の 3 種、 段階 4 で PluginParam を
/// 追加。 `PluginParam` の actual injection (= GUI → audio thread → plugin
/// host → IParameterChanges で plugin に送信) は IPC + RT 安全性整備が
/// 大規模なため extended scope (別フェーズ)、 段階 4 では「データ型 +
/// tracing 経由の警告」 のみで bind だけは可能。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum BindingTarget {
    /// `Track.volume` (0.0..=1.0、 CC 0..127 を linear マップ)。
    TrackVolume(u32),
    /// `Track.pan` (-1.0..=1.0、 CC 0..127 を `value*2/127 - 1` で linear マップ)。
    TrackPan(u32),
    /// `Song.bpm` (60.0..=180.0、 CC 0..127 を linear マップ)。 SongTempo
    /// curve とは独立 (= curve がある場合は curve が優先、 CC は base bpm を
    /// 動かすイメージ)。
    SongTempo,
    /// Phase 7 B1-M Step 4: plugin parameter bind。 `track` / `slot` で
    /// plugin instance を特定、 `param_id` は format ごと (CLAP `clap_id` /
    /// VST3 `ParamID`)。 actual な injection は extended scope (= GUI →
    /// audio thread → plugin host への RT-safe IPC + IParameterChanges /
    /// CLAP_EVENT_PARAM_VALUE injection)。 段階 4 では bind data の永続化
    /// と GUI side `apply_midi_value_to_target` での tracing log まで。
    PluginParam {
        track: u32,
        slot: crate::protocol::PluginSlot,
        param_id: u32,
    },
}

impl Song {
    /// Allocate a new stable track id, bumping the song-level counter.
    pub fn alloc_track_id(&mut self) -> u32 {
        let id = self.next_track_id.max(1);
        self.next_track_id = id + 1;
        id
    }

    /// Phase 5: allocate a new song-level automation lane id (`song_lanes`)。
    /// `next_song_lane_id` を bump して返す。
    pub fn alloc_song_lane_id(&mut self) -> u32 {
        let id = self.next_song_lane_id.max(1);
        self.next_song_lane_id = id + 1;
        id
    }

    /// Phase 7 B5 (`docs/plan_scale.html`): 指定 beat における active な
    /// `ScaleChange` を返す。 該当 event が無ければ `None` (= Scale 機能 OFF /
    /// chromatic 扱い)。 `scale_changes` は beat 昇順 invariant 前提で、
    /// `rev().find()` で「該当 beat 直前の最新 event」 を取る。
    pub fn scale_at(&self, beat: f64) -> Option<&ScaleChange> {
        self.scale_changes
            .iter()
            .rev()
            .find(|c| c.beat <= beat)
    }

    /// Phase 7 B5: `scale_changes` を beat 昇順に保つ。 同 beat の
    /// duplicate は許容 (上書きするかは caller 判断)。 scale_changes を
    /// 変更したあと (event 追加 / move) に呼ぶ。
    pub fn ensure_scale_changes_sorted(&mut self) {
        self.scale_changes
            .sort_by(|a, b| a.beat.partial_cmp(&b.beat).unwrap_or(std::cmp::Ordering::Equal));
    }

    /// Phase 5: find a song-level lane (mutable) by id。 Track の
    /// `lane_by_id_mut` と同 idiom。
    pub fn song_lane_by_id_mut(&mut self, lane_id: u32) -> Option<&mut AutomationLane> {
        self.song_lanes.iter_mut().find(|l| l.id == lane_id)
    }

    /// Phase 5: find a song-level lane (immutable) by id.
    pub fn song_lane_by_id(&self, lane_id: u32) -> Option<&AutomationLane> {
        self.song_lanes.iter().find(|l| l.id == lane_id)
    }

    /// Phase 5: find a song-level lane (immutable) whose target matches.
    /// SongTempo / SongTimeSigNumerator は同 song に最大 1 lane の前提
    /// (= multi-lane で同 target に複数置く意味がない、 Bitwig も 1 lane)。
    pub fn song_lane_by_target(&self, target: &AutomationTarget) -> Option<&AutomationLane> {
        self.song_lanes.iter().find(|l| &l.target == target)
    }

    /// Phase 5 Step 5.1 (`docs/plan_automation.md` §10、 gui_01 #034): track と
    /// master row を統一的に走査する mut accessor。 `track_id == MASTER_TRACK_ID`
    /// なら `song_lanes` を、 そうでなければ該当 track の `automation_lanes`
    /// を引く。 全 automation EditRequest handler から呼ばれる。
    pub fn automation_lane_by_key_mut(
        &mut self,
        track_id: u32,
        lane_id: u32,
    ) -> Option<&mut AutomationLane> {
        if track_id == MASTER_TRACK_ID {
            self.song_lane_by_id_mut(lane_id)
        } else {
            self.track_by_id_mut(track_id)
                .and_then(|t| t.lane_by_id_mut(lane_id))
        }
    }

    /// Phase 5 Step 5.1: read-only counterpart of `automation_lane_by_key_mut`。
    pub fn automation_lane_by_key(
        &self,
        track_id: u32,
        lane_id: u32,
    ) -> Option<&AutomationLane> {
        if track_id == MASTER_TRACK_ID {
            self.song_lane_by_id(lane_id)
        } else {
            self.track_by_id(track_id).and_then(|t| t.lane_by_id(lane_id))
        }
    }

    /// track と master row を統一的に走査する fx chain accessor。
    /// `track_id == MASTER_TRACK_ID` なら `master_fx_chain` を、 そうでなければ
    /// 該当 track の `fx_chain` を引く。 `automation_lane_by_key` と同 idiom
    /// (master 固有データは Song 直下、 sentinel 分岐で透過アクセス)。 plugin
    /// install / Inspector / chain 操作 handler から呼ぶ。
    pub fn fx_chain_by_track_id(&self, track_id: u32) -> Option<&[PluginInstance]> {
        if track_id == MASTER_TRACK_ID {
            Some(&self.master_fx_chain)
        } else {
            self.track_by_id(track_id).map(|t| t.fx_chain.as_slice())
        }
    }

    /// read-write counterpart of `fx_chain_by_track_id`。
    pub fn fx_chain_by_track_id_mut(
        &mut self,
        track_id: u32,
    ) -> Option<&mut Vec<PluginInstance>> {
        if track_id == MASTER_TRACK_ID {
            Some(&mut self.master_fx_chain)
        } else {
            self.track_by_id_mut(track_id).map(|t| &mut t.fx_chain)
        }
    }

    /// Re-assign stable ids to all tracks / clips after loading an older
    /// project file (or any save predating the id schema). Idempotent:
    /// records that already have non-zero ids are left untouched, and
    /// `next_*_id` counters are bumped above the highest seen id.
    ///
    /// PR4.5 sidechain regression fix: when a track's id changes here,
    /// every reference to the old id (= other tracks' `parent_group_id`
    /// and per-plugin `sidechain_sources` entries) is remapped to the new
    /// id. Without this remap, a saved project that used `id == 0` as a
    /// sentinel for the first track would, on load, lose all its sidechain
    /// wiring (the references would dangle, `compile_schedule` silently
    /// skips dangling refs, and the user sees no sidechain signal).
    pub fn ensure_ids(&mut self) {
        // Pass 1: assign fresh ids to sentinel tracks, recording the
        // (old_id → new_id) remap so refs can be patched in pass 2.
        let mut id_remap: std::collections::HashMap<u32, u32> =
            std::collections::HashMap::new();
        for track in &mut self.tracks {
            if track.id == 0 {
                let new_id = self.next_track_id.max(1);
                self.next_track_id = new_id + 1;
                id_remap.insert(0, new_id);
                track.id = new_id;
            } else if track.id >= self.next_track_id {
                self.next_track_id = track.id + 1;
            }
            track.ensure_clip_ids();
            track.ensure_lane_ids();
        }
        if self.next_track_id == 0 {
            self.next_track_id = 1;
        }

        // Pass 2: patch every reference to a remapped id. Multi-sentinel
        // cases (= more than one track started with id 0) collapse to the
        // *last* remap entry inserted for key 0 above, which is fine for
        // the typical "one sentinel for the first track" case. Anything
        // else was already malformed before save.
        if id_remap.is_empty() {
            return;
        }
        for track in &mut self.tracks {
            if let Some(pid) = track.parent_group_id
                && let Some(&new_pid) = id_remap.get(&pid)
            {
                track.parent_group_id = Some(new_pid);
            }
            for send in &mut track.sends {
                if let Some(&new_dest) = id_remap.get(&send.dest_track_id) {
                    send.dest_track_id = new_dest;
                }
            }
            let remap_chain = |chain: &mut [PluginInstance]| {
                for p in chain.iter_mut() {
                    for src in p.sidechain_sources.iter_mut() {
                        if let Some(old_id) = *src
                            && let Some(&new_id) = id_remap.get(&old_id)
                        {
                            *src = Some(new_id);
                        }
                    }
                }
            };
            remap_chain(&mut track.midi_fx_chain);
            if let Some(inst) = track.instrument.as_mut() {
                for src in inst.sidechain_sources.iter_mut() {
                    if let Some(old_id) = *src
                        && let Some(&new_id) = id_remap.get(&old_id)
                    {
                        *src = Some(new_id);
                    }
                }
            }
            remap_chain(&mut track.fx_chain);
        }

        // master bus の fx chain も track fx_chain と同じく sidechain_sources を
        // remap する。 master fx が他 track を sidechain source に取るケースに備える
        // (track ループ内 `remap_chain` closure は loop scope なので再利用不可、
        // ここで open-code)。
        for p in self.master_fx_chain.iter_mut() {
            for src in p.sidechain_sources.iter_mut() {
                if let Some(old_id) = *src
                    && let Some(&new_id) = id_remap.get(&old_id)
                {
                    *src = Some(new_id);
                }
            }
        }

        // Phase 5: song-level lane の id も同様に採番。 sentinel (0) のみ
        // 上書き、 既存非 0 id は触らず counter を bump するだけ。
        for lane in &mut self.song_lanes {
            if lane.id == 0 {
                let new_id = self.next_song_lane_id.max(1);
                self.next_song_lane_id = new_id + 1;
                lane.id = new_id;
            } else if lane.id >= self.next_song_lane_id {
                self.next_song_lane_id = lane.id + 1;
            }
            // lane 内 clip ids も担保 (Track の ensure_lane_ids 同 idiom、
            // ただし song_lanes は track field を持たないので per-lane で展開)
            for clip in &mut lane.clips {
                if clip.id == 0 {
                    let new_id = lane.next_clip_id.max(1);
                    lane.next_clip_id = new_id + 1;
                    clip.id = new_id;
                } else if clip.id >= lane.next_clip_id {
                    lane.next_clip_id = clip.id + 1;
                }
            }
            if lane.next_clip_id == 0 {
                lane.next_clip_id = 1;
            }
        }
        if self.next_song_lane_id == 0 {
            self.next_song_lane_id = 1;
        }
    }

    pub fn track_index_by_id(&self, track_id: u32) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == track_id)
    }

    pub fn track_by_id(&self, track_id: u32) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == track_id)
    }

    pub fn track_by_id_mut(&mut self, track_id: u32) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == track_id)
    }

    /// Allocate a fresh `ContentId`, bumping the song-level counter.
    pub fn alloc_content_id(&mut self) -> ContentId {
        let id = self.next_content_id.max(1);
        self.next_content_id = id + 1;
        id
    }

    /// Shared clip name for a `ContentId` (SSoT, v20+). Empty string if
    /// the content has no name. All clips sharing `content_id` resolve
    /// the same name through here.
    pub fn content_name(&self, content_id: ContentId) -> &str {
        self.clip_content_names
            .get(&content_id)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Set the shared name for a `ContentId`. Renames every linked clip
    /// (= every clip sharing this `content_id`) at once — the single
    /// write point for clip rename.
    pub fn set_content_name(&mut self, content_id: ContentId, name: String) {
        self.clip_content_names.insert(content_id, name);
    }

    /// Allocate a fresh `ContentId`, insert its `content` payload and its
    /// shared `name` together. Use at every fresh-clip creation site so
    /// name + content never desync. Returns the new id.
    pub fn alloc_content(&mut self, content: ClipContent, name: String) -> ContentId {
        let id = self.alloc_content_id();
        self.clip_contents.insert(id, content);
        if !name.is_empty() {
            self.clip_content_names.insert(id, name);
        }
        id
    }

    /// Fork a `ContentId` into an independent copy: deep-clone its
    /// content payload AND its shared name under a fresh id. Use at every
    /// independent-copy / Make-Unique site. Returns the new id. The
    /// source content/name are left untouched.
    pub fn fork_content(&mut self, src: ContentId) -> ContentId {
        let content = self.clip_contents.get(&src).cloned().unwrap_or_default();
        let name = self.clip_content_names.get(&src).cloned();
        let id = self.alloc_content_id();
        self.clip_contents.insert(id, content);
        if let Some(name) = name {
            self.clip_content_names.insert(id, name);
        }
        id
    }

    /// Migrate v5 `.daw` files: legacy `Clip.notes` (deserialize-only)
    /// gets moved into `clip_contents` keyed by a freshly allocated
    /// `content_id`. Idempotent — clips that already have non-zero
    /// `content_id` and an empty `notes` vector are left alone.
    ///
    /// Also assigns fresh `content_id` to clips with `content_id == 0`
    /// (sentinel) and ensures every referenced `content_id` has an
    /// entry in `clip_contents` (creating an empty one if missing —
    /// shouldn't happen in practice but keeps the invariant cheap).
    pub fn ensure_clip_contents(&mut self) {
        // Collect all live content_ids first so we can bump the counter
        // above the highest one before allocating new ids for sentinels.
        // Walks both main `clips` and every `automation_lanes[].clips`.
        let mut max_seen: ContentId = 0;
        for track in &self.tracks {
            for clip in &track.clips {
                if clip.content_id != 0 {
                    max_seen = max_seen.max(clip.content_id);
                }
            }
            for lane in &track.automation_lanes {
                for clip in &lane.clips {
                    if clip.content_id != 0 {
                        max_seen = max_seen.max(clip.content_id);
                    }
                }
            }
        }
        if self.next_content_id <= max_seen {
            self.next_content_id = max_seen + 1;
        }
        if self.next_content_id == 0 {
            self.next_content_id = 1;
        }

        for t_idx in 0..self.tracks.len() {
            for c_idx in 0..self.tracks[t_idx].clips.len() {
                let needs_new_id = self.tracks[t_idx].clips[c_idx].content_id == 0;
                let has_legacy_notes = !self.tracks[t_idx].clips[c_idx].notes.is_empty();
                if needs_new_id {
                    let new_id = self.alloc_content_id();
                    self.tracks[t_idx].clips[c_idx].content_id = new_id;
                }
                let cid = self.tracks[t_idx].clips[c_idx].content_id;
                // v19→v20: drain legacy per-clip name into the shared name
                // map (first non-empty wins for a shared content_id). Keeps
                // the in-memory `Clip.name` invariant empty.
                let legacy_name = std::mem::take(&mut self.tracks[t_idx].clips[c_idx].name);
                if !legacy_name.is_empty() {
                    self.clip_content_names.entry(cid).or_insert(legacy_name);
                }
                if has_legacy_notes {
                    let notes =
                        std::mem::take(&mut self.tracks[t_idx].clips[c_idx].notes);
                    self.clip_contents
                        .entry(cid)
                        .and_modify(|c| {
                            // Two clips both carrying legacy notes for
                            // the same migrated content_id is impossible
                            // (v5 stored notes per-clip; migration emits
                            // a fresh content_id per clip), so just
                            // overwrite if it ever happens. Promote any
                            // existing Audio variant back to Midi (also
                            // shouldn't happen, but keep the invariant).
                            *c = ClipContent::Midi(MidiContent {
                                notes: notes.clone(),
                            });
                        })
                        .or_insert_with(|| {
                            ClipContent::Midi(MidiContent { notes })
                        });
                } else {
                    // Ensure an entry exists for every referenced
                    // content_id so lookups never have to handle the
                    // missing case.
                    self.clip_contents.entry(cid).or_default();
                }
            }
            for l_idx in 0..self.tracks[t_idx].automation_lanes.len() {
                let lane_clip_count =
                    self.tracks[t_idx].automation_lanes[l_idx].clips.len();
                for c_idx in 0..lane_clip_count {
                    let needs_new_id =
                        self.tracks[t_idx].automation_lanes[l_idx].clips[c_idx].content_id
                            == 0;
                    if needs_new_id {
                        let new_id = self.alloc_content_id();
                        self.tracks[t_idx].automation_lanes[l_idx].clips[c_idx]
                            .content_id = new_id;
                    }
                    let cid = self.tracks[t_idx].automation_lanes[l_idx].clips[c_idx]
                        .content_id;
                    // v19→v20: drain legacy automation-clip name too.
                    let legacy_name = std::mem::take(
                        &mut self.tracks[t_idx].automation_lanes[l_idx].clips[c_idx].name,
                    );
                    if !legacy_name.is_empty() {
                        self.clip_content_names.entry(cid).or_insert(legacy_name);
                    }
                    // Automation clips have no legacy in-place payload
                    // (v8-introduced) — just make sure the content
                    // store has an entry so audio thread / GUI lookups
                    // never miss. Default is `Midi(empty)`; writers
                    // promote to `Automation` on first edit.
                    self.clip_contents.entry(cid).or_insert_with(|| {
                        ClipContent::Automation(AutomationContent::default())
                    });
                }
            }
        }
    }

    /// Refcount of a `ContentId` = number of clips across all tracks
    /// referencing it, **including automation clips** inside
    /// `Track.automation_lanes`. Used by the GUI to switch the visual
    /// style between "shared" (>=2) and "regular" (==1) and by GC.
    pub fn clip_content_refcount(&self, content_id: ContentId) -> usize {
        let main_clips = self
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .filter(|c| c.content_id == content_id)
            .count();
        let auto_clips = self
            .tracks
            .iter()
            .flat_map(|t| t.automation_lanes.iter())
            .flat_map(|l| l.clips.iter())
            .filter(|c| c.content_id == content_id)
            .count();
        main_clips + auto_clips
    }

    /// Resolve a `Clip`'s shared notes via its `content_id`. Returns
    /// an empty slice if `content_id` doesn't have an entry (e.g. a
    /// freshly-constructed clip before `ensure_clip_contents` ran).
    /// Used everywhere that previously read `clip.notes` directly.
    pub fn clip_notes(&self, clip: &Clip) -> &[Note] {
        self.clip_contents
            .get(&clip.content_id)
            .and_then(|c| c.notes())
            .unwrap_or(&[])
    }

    /// Mutable lookup for the notes of a clip identified by `(track_idx,
    /// clip_idx)`. Resolves `content_id` and returns a mutable reference
    /// to the shared `notes` vector. Returns `None` if the indices are
    /// out of range, the `content_id` has no entry, or the entry is an
    /// `Audio` variant.
    pub fn notes_in_clip_mut(
        &mut self,
        track_idx: usize,
        clip_idx: usize,
    ) -> Option<&mut Vec<Note>> {
        let content_id = self.tracks.get(track_idx)?.clips.get(clip_idx)?.content_id;
        self.clip_contents
            .get_mut(&content_id)
            .and_then(|c| c.notes_mut())
    }

    /// Drop `clip_contents` entries that no clip references. Called
    /// before save so disk files stay tidy. In-memory we keep zero-ref
    /// entries around briefly (e.g. between a delete and the next
    /// frame) — Undo restores from the snapshot regardless.
    ///
    /// Walks both the main per-track `clips` and every
    /// `automation_lanes[].clips` entry — automation clips share the
    /// same content store as MIDI / audio clips.
    pub fn gc_clip_contents(&mut self) {
        let mut live: std::collections::HashSet<ContentId> = self
            .tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(|c| c.content_id)
            .collect();
        for track in &self.tracks {
            for lane in &track.automation_lanes {
                for clip in &lane.clips {
                    live.insert(clip.content_id);
                }
            }
        }
        self.clip_contents.retain(|id, _| live.contains(id));
        // Shared names follow content lifecycle: drop names whose
        // content_id no longer has any referencing clip.
        self.clip_content_names.retain(|id, _| live.contains(id));
    }

    /// Allocate a fresh `AudioSourceId`, bumping the song-level counter.
    pub fn alloc_audio_source_id(&mut self) -> AudioSourceId {
        let id = self.next_audio_source_id.max(1);
        self.next_audio_source_id = id + 1;
        id
    }

    /// Refcount of an `AudioSourceId` = total `AudioEvent.source_id`
    /// references across every audio `ClipContent` in the song. Used by
    /// `gc_audio_sources` and Inspector display. `Video` clips do not
    /// reference AudioSource directly — the auto-extracted WAV is wired
    /// via the paired audio track's `AudioEvent`, which is counted here
    /// like any other audio reference.
    pub fn audio_source_refcount(&self, source_id: AudioSourceId) -> usize {
        self.clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Audio(a) => Some(a.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Automation(_)
                | ClipContent::Video(_)
                | ClipContent::Image(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .filter(|ev| ev.source_id == source_id)
            .count()
    }

    /// Drop `audio_sources` entries no `AudioEvent` references. Mirrors
    /// `gc_clip_contents` — called before save so the on-disk pool stays
    /// tidy. In-memory entries with refcount=0 are kept briefly so
    /// Undo can restore them.
    pub fn gc_audio_sources(&mut self) {
        let live: std::collections::HashSet<AudioSourceId> = self
            .clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Audio(a) => Some(a.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Automation(_)
                | ClipContent::Video(_)
                | ClipContent::Image(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .map(|ev| ev.source_id)
            .collect();
        self.audio_sources.retain(|id, _| live.contains(id));
    }

    /// Re-assign fresh `AudioSourceId` to any source whose id is the
    /// `0` sentinel (and bump `next_audio_source_id` above the highest
    /// seen). Idempotent — sources with non-zero ids are left untouched.
    /// Mirrors `ensure_clip_contents` semantics.
    pub fn ensure_audio_source_ids(&mut self) {
        let mut max_seen: AudioSourceId = 0;
        for id in self.audio_sources.keys() {
            if *id != 0 {
                max_seen = max_seen.max(*id);
            }
        }
        if self.next_audio_source_id <= max_seen {
            self.next_audio_source_id = max_seen + 1;
        }
        if self.next_audio_source_id == 0 {
            self.next_audio_source_id = 1;
        }
        // Re-key any AudioSource currently held under id 0. AudioEvent
        // references to id 0 are NOT remapped — those remain dangling
        // (= "missing source") which is the correct UX for unresolved
        // imports. Callers that mint a fresh AudioSource should always
        // go through `alloc_audio_source_id` and avoid sentinel 0.
        if let Some(orphan) = self.audio_sources.remove(&0) {
            let new_id = self.alloc_audio_source_id();
            self.audio_sources.insert(new_id, orphan);
        }
    }

    /// v12 (`docs/plan_video.md` §2.4): allocate a fresh
    /// `VideoSourceId`, bumping the song-level counter. Mirrors
    /// `alloc_audio_source_id`.
    pub fn alloc_video_source_id(&mut self) -> VideoSourceId {
        let id = self.next_video_source_id.max(1);
        self.next_video_source_id = id + 1;
        id
    }

    /// v12: refcount of a `VideoSourceId` = total `VideoEvent.source_id`
    /// references across every `Video` `ClipContent` in the song. Used
    /// by `gc_video_sources` and (future) inspector display.
    pub fn video_source_refcount(&self, source_id: VideoSourceId) -> usize {
        self.clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Video(v) => Some(v.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Audio(_)
                | ClipContent::Automation(_)
                | ClipContent::Image(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .filter(|ev| ev.source_id == source_id)
            .count()
    }

    /// v12: drop `video_sources` entries no `VideoEvent` references.
    /// Mirrors `gc_audio_sources` — called before save so the on-disk
    /// pool stays tidy. In-memory entries with refcount==0 are kept
    /// briefly so Undo can restore them.
    pub fn gc_video_sources(&mut self) {
        let live: std::collections::HashSet<VideoSourceId> = self
            .clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Video(v) => Some(v.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Audio(_)
                | ClipContent::Automation(_)
                | ClipContent::Image(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .map(|ev| ev.source_id)
            .collect();
        self.video_sources.retain(|id, _| live.contains(id));
    }

    /// v12: re-assign fresh `VideoSourceId` to any source whose id is
    /// the `0` sentinel and bump `next_video_source_id` above the
    /// highest seen. Mirrors `ensure_audio_source_ids` semantics; v11
    /// files load with all-default fields so this only matters once
    /// v12 sources start being saved with sentinel ids (= shouldn't
    /// happen in practice, but the invariant is cheap to enforce).
    pub fn ensure_video_source_ids(&mut self) {
        let mut max_seen: VideoSourceId = 0;
        for id in self.video_sources.keys() {
            if *id != 0 {
                max_seen = max_seen.max(*id);
            }
        }
        if self.next_video_source_id <= max_seen {
            self.next_video_source_id = max_seen + 1;
        }
        if self.next_video_source_id == 0 {
            self.next_video_source_id = 1;
        }
        if let Some(orphan) = self.video_sources.remove(&0) {
            let new_id = self.alloc_video_source_id();
            self.video_sources.insert(new_id, orphan);
        }
    }

    /// v13 (`docs/plan_image_overlay.md` §2.4): allocate a fresh
    /// `ImageSourceId`, bumping the song-level counter. Mirrors
    /// `alloc_video_source_id`.
    pub fn alloc_image_source_id(&mut self) -> ImageSourceId {
        let id = self.next_image_source_id.max(1);
        self.next_image_source_id = id + 1;
        id
    }

    /// v13: refcount of an `ImageSourceId` = total `ImageEvent.source_id`
    /// references across every `Image` `ClipContent` in the song. Used
    /// by `gc_image_sources` and (future) inspector display.
    pub fn image_source_refcount(&self, source_id: ImageSourceId) -> usize {
        self.clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Image(i) => Some(i.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Audio(_)
                | ClipContent::Automation(_)
                | ClipContent::Video(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .filter(|ev| ev.source_id == source_id)
            .count()
    }

    /// v13: drop `image_sources` entries no `ImageEvent` references.
    /// Mirrors `gc_video_sources`.
    pub fn gc_image_sources(&mut self) {
        let live: std::collections::HashSet<ImageSourceId> = self
            .clip_contents
            .values()
            .filter_map(|c| match c {
                ClipContent::Image(i) => Some(i.events.iter()),
                ClipContent::Midi(_)
                | ClipContent::Audio(_)
                | ClipContent::Automation(_)
                | ClipContent::Video(_)
                | ClipContent::Text(_) => None,
            })
            .flatten()
            .map(|ev| ev.source_id)
            .collect();
        self.image_sources.retain(|id, _| live.contains(id));
    }

    /// v13: re-assign fresh `ImageSourceId` to any source whose id is
    /// the `0` sentinel and bump `next_image_source_id` above the
    /// highest seen. Mirrors `ensure_video_source_ids` semantics.
    pub fn ensure_image_source_ids(&mut self) {
        let mut max_seen: ImageSourceId = 0;
        for id in self.image_sources.keys() {
            if *id != 0 {
                max_seen = max_seen.max(*id);
            }
        }
        if self.next_image_source_id <= max_seen {
            self.next_image_source_id = max_seen + 1;
        }
        if self.next_image_source_id == 0 {
            self.next_image_source_id = 1;
        }
        if let Some(orphan) = self.image_sources.remove(&0) {
            let new_id = self.alloc_image_source_id();
            self.image_sources.insert(new_id, orphan);
        }
    }
}

/// A track owns a full CLAP signal chain in three sections:
///
/// 1. `midi_fx_chain` — note-effect plugins (arpeggiator / quantizer / ...)
///    processed in order, piping out_events into the next plugin's in_events.
/// 2. `instrument` — the note→audio plugin (receives the MIDI FX output).
///    `None` when the track has no instrument yet.
/// 3. `fx_chain` — audio-effect plugins (compressor / reverb / ...) applied
///    to the instrument's audio output in order.
///
/// Clips on the track feed the MIDI FX chain at the top of the buffer. The
/// final audio flows into the parent — either a `Group` track (when
/// `parent_group_id == Some(id)`) or the master bus (when `None`).
///
/// v16 (`docs/plan_text_overlay.md`): 旧 `kind: TrackKind { Audio, Video }`
/// を廃止し、 全 track が unified に audio path + visual composite path 両方
/// を保持する (= REAPER 流、 同 track 上で audio / midi / video / image /
/// text clip を混在可能)。 旧 Video track は v16 migration で audio
/// defaults (instrument: None / fx_chain: vec![] / volume: 1.0 / pan: 0.0
/// / armed: false / source: None) を自動補完し、 mixer / engine path に
/// 静かに参加する (= 音は出ないが mute / volume 操作は可)。 旧 v15 file
/// の `kind` field は serde が未知 field として捨てる (= deny_unknown_fields
/// が無いため tolerant)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Track {
    /// Stable id assigned by `Song::alloc_track_id`. `0` is "未採番"
    /// sentinel — reassigned by `Song::ensure_ids` when loading an older
    /// file. Persists across track add/remove and reorder; arrangement
    /// widget addresses tracks by this id, not by index.
    #[serde(default)]
    pub id: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instrument: Option<PluginInstance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub midi_fx_chain: Vec<PluginInstance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fx_chain: Vec<PluginInstance>,
    pub volume: f32,
    pub pan: f32,
    /// Track silenced by the user. Additive with the global solo rule (see
    /// `solo` below): `effective_mute = muted || (any_solo_on && !solo)`.
    #[serde(default)]
    pub muted: bool,
    /// When any track has `solo == true`, tracks that don't are silenced
    /// for the duration of playback (classic mixer-strip behaviour).
    #[serde(default)]
    pub solo: bool,
    /// Phase 7 B4 (`docs/plan_b4_midi.md` §3.1): Record-arm 状態。 armed track
    /// のみが MIDI input (および将来の audio input) を受け取り、 録音中は
    /// 該当 track の MIDI clip に note が書き込まれる。 業界標準 (Bitwig /
    /// Live / Reaper) と同 idiom (= 排他性なし、 任意数の track を同時 armed
    /// にできる)。 v8 file は `false` で forward-migrate (serde default)。
    #[serde(default)]
    pub armed: bool,
    /// Future use: VOICEVOX speaker / style etc. Kept distinct from the
    /// `instrument` slot because it selects a rendering backend, not a CLAP
    /// plugin.
    #[serde(default)]
    pub source: InstrumentSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<Clip>,
    /// Per-track stable id allocator for `Clip`. Bumped each time a new
    /// clip is created on this track.
    #[serde(default)]
    pub next_clip_id: u32,
    /// Parent group track id. `None` ⇒ this track feeds the master bus
    /// directly. Any track can act as a "group" — that role is derived
    /// from whether other tracks point at this one's id, not stored on
    /// the track itself (Reaper's folder-track model). Forms a tree of
    /// arbitrary depth; cycles are rejected by the graph compiler.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_group_id: Option<u32>,
    /// Aux sends from this track to other (return / bus) tracks. Each
    /// `Send` is a *parallel* gain-scaled copy of this track's signal
    /// summed into the destination track's input bus — the source's own
    /// signal still flows to its parent / master untouched. Empty for
    /// tracks with no sends. A "return" track is not a distinct kind: it
    /// is derived (a track that has incoming sends), exactly like a
    /// "group" is derived from incoming `parent_group_id`. See `Send` /
    /// `docs/plan_routing_graph.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<Send>,
    /// Most recent plugin-reported latency for this track, populated by
    /// the plugin host via the CLAP `latency` extension and cached on
    /// the model so the GUI can display it and the routing graph can
    /// recompile PDC compensation. Not user-editable.
    #[serde(default)]
    pub reported_latency_samples: u32,
    /// Per-target automation lanes attached to this track. Each lane
    /// carries a `default_value` (used outside any clip / when
    /// `enabled = false`) and a list of `AutomationClip` whose
    /// `content_id` resolves into `Song.clip_contents` like MIDI /
    /// Audio clips do. Order is the display order in the inspector
    /// and arrangement (drag-reorderable). Empty for tracks without
    /// any automation. See `docs/plan_automation.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation_lanes: Vec<AutomationLane>,
    /// Per-track stable id allocator for `AutomationLane`. Bumped each
    /// time a new lane is created; never reused even after deletion.
    /// `0` is the sentinel — `Track::ensure_lane_ids` reassigns it on
    /// load.
    #[serde(default)]
    pub next_lane_id: u32,
    /// v18 (`docs/plan_track_clip_color.md`): user-facing track color
    /// (RGB, opaque). `None` ⇒ the view layer derives a stable palette
    /// color from `id` (auto-assignment, reorder-stable). `Some(rgb)` ⇒
    /// explicit user override. The color carries no audio/engine meaning;
    /// only `daw_gui` reads it (arrangement header tint + clip inherit).
    /// v17 files forward-migrate to `None` (= derived palette color).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// v19 (`docs/plan_tachie_group_transform.md`): 親グループトラックが合成済み
    /// 立ち絵 (子パーツを z 順に 1 枚へ合成したもの) にかける 2D affine + opacity。
    /// `None` ⇒ transform 無し (= identity、立ち絵グループでない通常 / audio
    /// グループ)。`Some` ⇒ 位置/回転/非一様スケール/任意アンカー/opacity。純粋に
    /// visual で daw_audio は読まない (group の役割は `parent_group_id` 由来で派生、
    /// inspector / 合成は §5.6 `group_has_visual_content` で gate)。v18 files
    /// forward-migrate to `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_transform: Option<GroupTransform>,
    /// v21 (`docs/plan_pakupaku.md`): 口パク出力先。`Some(track_id)` ⇒ この
    /// vocal track の notes+歌詞から生成した口画像 `ImageEvent` 列を、指定の
    /// 口 track (= 立ち絵 group 内の子 image track) へ焼き込む。設定が arm に
    /// あたり、notes/歌詞/`mouth_map` 変更で自動再生成される (派生データ)。
    /// vocal track 以外では意味を持たない。v20 files forward-migrate to `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lipsync_target_track: Option<u32>,
    /// v21 (`docs/plan_pakupaku.md`): 口形状 → `ImageSourceId` のマッピング。
    /// 口 track (= `lipsync_target_track` が指す側) に持たせ、生成時に各 phoneme
    /// の口形状をこの表で画像へ解決する。`None` ⇒ 未設定 (口パク未割当)。
    /// v20 files forward-migrate to `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouth_map: Option<MouthMap>,
}

/// Where a `Send` taps the source track's signal chain.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode,
)]
pub enum SendMode {
    /// After the track's volume / pan fader (the post-fader scratch).
    /// The send level tracks the source fader — the standard choice for
    /// reverb / delay returns where the wet should follow the dry.
    #[default]
    PostFader,
    /// After the fx chain but before the volume / pan fader. The send is
    /// independent of the source fader — for cue / parallel sends. (A
    /// pre-FX raw tap is the sidechain feature's job, not a send.)
    PreFader,
}

/// A single aux send: a parallel, gain-scaled copy of a track's signal
/// routed to another track that acts as a return / bus. Mirrors Ardour
/// `InternalSend` / a REAPER track send. The source track's main output
/// is unaffected; the copy is summed into `dest_track_id`'s input bus
/// before that destination's fx chain runs. The send level is
/// automatable via `AutomationTarget::TrackBuiltin(TrackBuiltinParam::
/// SendGain { send_idx })`, where `send_idx` is this send's index in
/// `Track::sends`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Send {
    /// Stable `Track::id` of the destination (return / bus) track.
    pub dest_track_id: u32,
    /// Linear send gain (`0.0` = silent, `1.0` = unity, up to `2.0` =
    /// +6 dB to match the volume-fader range). Automatable.
    pub gain: f32,
    /// Tap point on the source track's signal chain.
    pub mode: SendMode,
    /// Per-send mute. `false` keeps the wiring but silences the send.
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub enum InstrumentSource {
    #[default]
    None,
    Vocal { speaker_id: u32, style_name: String },
    Vst3 { path: PathBuf },
    BuiltinSynth,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            instrument: None,
            midi_fx_chain: Vec::new(),
            fx_chain: Vec::new(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            armed: false,
            source: InstrumentSource::None,
            clips: Vec::new(),
            next_clip_id: 1,
            parent_group_id: None,
            sends: Vec::new(),
            reported_latency_samples: 0,
            automation_lanes: Vec::new(),
            next_lane_id: 1,
            color: None,
            group_transform: None,
            lipsync_target_track: None,
            mouth_map: None,
        }
    }
}

/// v19 (`docs/plan_tachie_group_transform.md` §4.1): 親グループトラックが合成済み
/// 立ち絵 1 枚にかける 2D affine + opacity。AE の Transform プロパティ群
/// (Anchor / Position / Scale / Rotation / Opacity) と同構成。合成式は列ベクトル
/// 左乗算で `M_local = T(pos+anchor)·R(rot)·S(sx,sy)·T(-anchor)`、親子は
/// `M_world = M_parent·M_local` (トップダウン)。Opacity だけは行列に乗せず合成済み
/// quad の alpha に適用 (AE 準拠)。値は plain 単位 (automation lane の正規化は
/// `crate::automation::plain_to_norm` 参照)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct GroupTransform {
    /// 位置 X (normalized project 座標、0..1 が preview 幅)。アンカー基準位置への
    /// オフセット (AE 同様、`pos = 0` でアンカーが home に留まる)。
    pub x: f32,
    /// 位置 Y。
    pub y: f32,
    /// 2D 回転 (radians、clockwise positive)。アンカーを旋回中心とする。
    pub rotation_radians: f32,
    /// 水平スケール倍率 (`1.0` = 等倍)。アンカー中心。非一様可 (`scale_y` と独立)。
    pub scale_x: f32,
    /// 垂直スケール倍率。
    pub scale_y: f32,
    /// アンカー X (合成キャンバスの normalized 0..1、`0.5` = 中央)。回転・スケール
    /// 共通の中心。
    pub anchor_x: f32,
    /// アンカー Y。
    pub anchor_y: f32,
    /// 全体不透明度 (0..1)。transform 行列には乗せず、合成済みグループ quad の
    /// alpha に適用 (AE 準拠)。子個別の opacity は合成前に各子へ焼き込まれる。
    pub opacity: f32,
}

impl Default for GroupTransform {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            rotation_radians: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            anchor_x: 0.5,
            anchor_y: 0.5,
            opacity: 1.0,
        }
    }
}

impl Track {
    /// Allocate a new stable clip id, bumping the per-track counter.
    pub fn alloc_clip_id(&mut self) -> u32 {
        let id = self.next_clip_id.max(1);
        self.next_clip_id = id + 1;
        id
    }

    /// Re-assign stable ids to all clips. Idempotent (clips with non-zero
    /// ids are left alone, counter is bumped above the max seen).
    pub fn ensure_clip_ids(&mut self) {
        for clip in &mut self.clips {
            if clip.id == 0 {
                clip.id = self.next_clip_id.max(1);
                self.next_clip_id = clip.id + 1;
            } else if clip.id >= self.next_clip_id {
                self.next_clip_id = clip.id + 1;
            }
        }
        if self.next_clip_id == 0 {
            self.next_clip_id = 1;
        }
    }

    pub fn clip_index_by_id(&self, clip_id: u32) -> Option<usize> {
        self.clips.iter().position(|c| c.id == clip_id)
    }

    pub fn clip_by_id(&self, clip_id: u32) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == clip_id)
    }

    pub fn clip_by_id_mut(&mut self, clip_id: u32) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == clip_id)
    }

    /// Allocate a new stable lane id, bumping the per-track counter.
    pub fn alloc_lane_id(&mut self) -> u32 {
        let id = self.next_lane_id.max(1);
        self.next_lane_id = id + 1;
        id
    }

    /// Re-assign stable ids to all automation lanes and the clips
    /// inside each lane. Idempotent (lanes / clips with non-zero ids
    /// are left alone, counters are bumped above the max seen).
    pub fn ensure_lane_ids(&mut self) {
        for lane in &mut self.automation_lanes {
            if lane.id == 0 {
                lane.id = self.next_lane_id.max(1);
                self.next_lane_id = lane.id + 1;
            } else if lane.id >= self.next_lane_id {
                self.next_lane_id = lane.id + 1;
            }
            lane.ensure_clip_ids();
        }
        if self.next_lane_id == 0 {
            self.next_lane_id = 1;
        }
    }

    pub fn lane_index_by_id(&self, lane_id: u32) -> Option<usize> {
        self.automation_lanes.iter().position(|l| l.id == lane_id)
    }

    pub fn lane_by_id(&self, lane_id: u32) -> Option<&AutomationLane> {
        self.automation_lanes.iter().find(|l| l.id == lane_id)
    }

    pub fn lane_by_id_mut(&mut self, lane_id: u32) -> Option<&mut AutomationLane> {
        self.automation_lanes.iter_mut().find(|l| l.id == lane_id)
    }
}

/// Reference to a plugin loaded on a track, with the opaque state blob the
/// plugin itself produced (CLAP `clap_plugin_state.save` or VST3
/// `IComponent::getState`). Paths are NOT stored — `(format, plugin_id)`
/// is resolved through `plugin_db::PluginDatabase` at load time, keeping
/// projects portable across machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct PluginInstance {
    /// CLAP stable id (reverse-DNS) or VST3 class UUID rendered as hex.
    pub plugin_id: String,
    /// Which backend created this plugin. Defaults to CLAP for projects
    /// saved before VST3 support existed.
    #[serde(default)]
    pub format: PluginFormat,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "base64_opt"
    )]
    pub state: Option<Vec<u8>>,
    /// PR4 sidechain: per-aux-input-port routing source. Each entry maps
    /// the plugin's `is_main=false` aux input port index to a source
    /// `Track::id`. `None` (or absent index) leaves that port silent.
    /// `Vec` length = number of aux input ports the user has hooked up;
    /// shorter than the plugin's actual port count is fine (trailing
    /// ports stay silent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidechain_sources: Vec<Option<u32>>,
}

impl PluginInstance {
    pub fn new(plugin_id: String, format: PluginFormat) -> Self {
        Self {
            plugin_id,
            format,
            state: None,
            sidechain_sources: Vec::new(),
        }
    }
}

/// A clip is a free-time container of notes positioned along the song
/// timeline. `start_beat` and `length_beats` define where the clip lives;
/// the actual notes are stored in `Song.clip_contents` keyed by
/// `content_id` so multiple clips can share the same source (REAPER
/// pooled MIDI / linked clip model).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Clip {
    /// Stable id within the owning track. `0` is "未採番" sentinel —
    /// reassigned by `Track::ensure_clip_ids` when loading. Persists across
    /// move and resize.
    #[serde(default)]
    pub id: u32,
    /// **Legacy field** (v19 まで per-clip 名の owner)。 v20+ は
    /// `Song.clip_content_names[content_id]` が SSoT (= 共有クリップ間で
    /// 名前を共有)。 load 時に `Song::ensure_clip_contents` が map へ drain
    /// して空にする。 **in-memory は常に空**、 直接書かない (rename は
    /// `Song::set_content_name` 経由)。 空なら serialize されない。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub start_beat: f64,
    pub length_beats: f64,
    /// Reference into `Song.clip_contents`. `0` is the "未採番" sentinel —
    /// reassigned by `Song::ensure_clip_contents` when loading. Multiple
    /// clips with the same `content_id` share notes (linked clips).
    #[serde(default)]
    pub content_id: ContentId,
    /// **Legacy v5 deserialize-only field**: in v5 `Clip` owned `notes`
    /// directly. v6+ stores notes in `Song.clip_contents` keyed by
    /// `content_id`. After deserialization, `Song::ensure_clip_contents`
    /// drains non-empty `notes` into `clip_contents` and clears the
    /// vector. **In-memory the field is always empty**; never write to
    /// it directly. Skipped on serialize when empty so v6 files don't
    /// emit it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    /// v18 (`docs/plan_track_clip_color.md`): per-clip color override
    /// (RGB, opaque). `None` ⇒ inherit the owning track's effective color
    /// (the default; resetting to `None` is the Ableton-style "match track
    /// color"). `Some(rgb)` ⇒ explicit per-clip override. Read only by
    /// `daw_gui` (arrangement clip fill). v17 files forward-migrate to
    /// `None` (= inherit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// v21 (`docs/plan_pakupaku.md`): 口パク自動生成 clip の印。`true` ⇒ この
    /// clip は vocal の notes+歌詞+`mouth_map` から導出された派生物で、再生成時に
    /// 口 track 上の `auto_lipsync == true` clip は全削除 → 再構築される
    /// (手編集は保持しない)。ユーザが手で置いた clip は `false` のまま温存。
    /// v20 files forward-migrate to `false`。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_lipsync: bool,
}

/// Shared content referenced by one or more `Clip`s via
/// `Clip.content_id`. Stored on `Song.clip_contents`. Carries either
/// MIDI notes (`Midi(MidiContent)`) or audio events
/// (`Audio(AudioContent)`) depending on the variant.
///
/// `#[serde(untagged)]` lets v6 `.daw` files (which serialised
/// `ClipContent` as a flat struct `{ "notes": [...] }`) deserialize
/// directly into `Midi(MidiContent { notes })` — `MidiContent.notes`
/// vs `AudioContent.events` are disjoint field sets so the dispatch
/// is unambiguous. bincode (used over IPC) ignores the serde-untagged
/// attribute and encodes the variant index as usual.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(untagged)]
pub enum ClipContent {
    Midi(MidiContent),
    Audio(AudioContent),
    Automation(AutomationContent),
    /// v12 (`docs/plan_video.md` §2.2): video clip payload. Untagged
    /// disambiguation from `Audio` works because `VideoEvent` requires
    /// `source_start_micros` while `AudioEvent` requires
    /// `source_start_frames` — neither field has a serde default, so
    /// a JSON shaped for one variant fails inner deserialization for
    /// the other and serde falls through to the matching variant.
    Video(VideoContent),
    /// v13 (`docs/plan_image_overlay.md` §2.2): image overlay (PiP)
    /// clip payload. Untagged disambiguation from `Audio` / `Video`
    /// works because `ImageEvent` requires `opacity` while neither
    /// `AudioEvent` nor `VideoEvent` has that field, and all three
    /// content structs use `deny_unknown_fields` so a JSON object
    /// carrying `opacity` fails the Audio / Video inner deserialize
    /// and falls through to `Image`.
    Image(ImageContent),
    /// v16 (`docs/plan_text_overlay.md` §2.2): text overlay (title /
    /// 字幕 / credits) clip payload。 untagged disambiguation:
    /// `TextEvent.text: String` + `font_family: String` を持ち、 他
    /// variant の inner struct には `String` の required field 無し
    /// (Image は `opacity` 数値、 Video は `source_start_micros`、
    /// Audio は `source_start_frames`、 Midi は `notes`、 Automation
    /// は `points`)。 `deny_unknown_fields` で意図しない fallthrough
    /// を防止。
    Text(TextContent),
}

impl Default for ClipContent {
    fn default() -> Self {
        ClipContent::Midi(MidiContent::default())
    }
}

impl ClipContent {
    /// Borrow the notes slice if this is a `Midi` variant. `Audio` /
    /// `Automation` / `Video` variants return `None`. Used by
    /// `Song::clip_notes` and other helpers that previously read
    /// `clip.notes` directly.
    pub fn notes(&self) -> Option<&[Note]> {
        match self {
            ClipContent::Midi(m) => Some(m.notes.as_slice()),
            ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the notes vec for a `Midi` variant. Other
    /// variants return `None`.
    pub fn notes_mut(&mut self) -> Option<&mut Vec<Note>> {
        match self {
            ClipContent::Midi(m) => Some(&mut m.notes),
            ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the audio events slice if this is an `Audio` variant.
    pub fn audio_events(&self) -> Option<&[AudioEvent]> {
        match self {
            ClipContent::Audio(a) => Some(a.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the events vec for an `Audio` variant.
    pub fn audio_events_mut(&mut self) -> Option<&mut Vec<AudioEvent>> {
        match self {
            ClipContent::Audio(a) => Some(&mut a.events),
            ClipContent::Midi(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the automation point slice if this is an `Automation`
    /// variant. Other variants return `None`.
    pub fn automation_points(&self) -> Option<&[AutomationPoint]> {
        match self {
            ClipContent::Automation(a) => Some(a.points.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the automation point vec for an `Automation`
    /// variant.
    pub fn automation_points_mut(&mut self) -> Option<&mut Vec<AutomationPoint>> {
        match self {
            ClipContent::Automation(a) => Some(&mut a.points),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the video events slice if this is a `Video` variant. v12
    /// (`docs/plan_video.md` §2.2).
    pub fn video_events(&self) -> Option<&[VideoEvent]> {
        match self {
            ClipContent::Video(v) => Some(v.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the events vec for a `Video` variant. v12.
    pub fn video_events_mut(&mut self) -> Option<&mut Vec<VideoEvent>> {
        match self {
            ClipContent::Video(v) => Some(&mut v.events),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Image(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the image events slice if this is an `Image` variant. v13
    /// (`docs/plan_image_overlay.md` §2.2).
    pub fn image_events(&self) -> Option<&[ImageEvent]> {
        match self {
            ClipContent::Image(i) => Some(i.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Mutably borrow the events vec for an `Image` variant. v13.
    pub fn image_events_mut(&mut self) -> Option<&mut Vec<ImageEvent>> {
        match self {
            ClipContent::Image(i) => Some(&mut i.events),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Text(_) => None,
        }
    }

    /// Borrow the text events slice if this is a `Text` variant.
    /// v16 (`docs/plan_text_overlay.md` §2.2).
    pub fn text_events(&self) -> Option<&[TextEvent]> {
        match self {
            ClipContent::Text(t) => Some(t.events.as_slice()),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_) => None,
        }
    }

    /// Mutably borrow the events vec for a `Text` variant. v16.
    pub fn text_events_mut(&mut self) -> Option<&mut Vec<TextEvent>> {
        match self {
            ClipContent::Text(t) => Some(&mut t.events),
            ClipContent::Midi(_)
            | ClipContent::Audio(_)
            | ClipContent::Automation(_)
            | ClipContent::Video(_)
            | ClipContent::Image(_) => None,
        }
    }
}

/// MIDI clip content — a bag of notes positioned in clip-local beats.
///
/// `deny_unknown_fields` is required: `ClipContent` is `#[serde(untagged)]`
/// so the deserializer tries each variant in order until one succeeds.
/// Without `deny_unknown_fields`, a JSON object with only an `events`
/// or `points` key would happily deserialize into `MidiContent { notes:
/// vec![] }` (because every field has a default), making it impossible
/// to disambiguate variants. With `deny_unknown_fields`, only the
/// matching variant succeeds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct MidiContent {
    /// Notes are in arbitrary order — readers that care about time
    /// order must sort by `Note::start_beat`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
}

/// Audio clip content — an ordered list of audio events that play
/// within the clip. Bitwig "Clip ⊃ Audio Events" hierarchy
/// ([docs/plan_audio_clip.md](../../docs/plan_audio_clip.md)). Events
/// can overlap (mixed) or sit side by side; clip-internal layout is
/// defined by each event's `event_start_in_clip_beats` /
/// `event_length_beats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct AudioContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<AudioEvent>,
}

/// Stable id for an entry in `Song.audio_sources`. `0` is the "未採番"
/// sentinel — `Song::ensure_audio_source_ids` reassigns it on load.
pub type AudioSourceId = u32;

/// Reference to an imported audio file (WAV / FLAC / generated). Path
/// resolution is governed by `AudioSourcePath`. Sample buffers are NOT
/// stored on the model — each process (GUI / audio engine) decodes the
/// file independently from the path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioSource {
    pub path: AudioSourcePath,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    /// BPM detected from WAV cue chunks / ACID metadata. Used by
    /// `StretchMode::Repitch` / `Stretch` to translate to project BPM.
    /// `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_bpm: Option<f32>,
    /// MIDI key the loop was recorded at — relevant for sample-based
    /// instruments. `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_key: Option<u8>,
}

/// Path resolution strategy for an `AudioSource`. Normal imports
/// produce `ProjectRelative` after copying the file into
/// `<project_dir>/samples/<basename>_<hash8>.<ext>`. `Absolute` is
/// reserved for the unsaved-project import-cache fallback (and a
/// future "link to external sample" mode). `Generated` is used by
/// VOICEVOX and other in-memory synthesised audio with no file on
/// disk; the `id` is the same one carried by
/// `MainToChild::SetGeneratedAudio`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum AudioSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
    Generated { id: u64 },
}

/// Stable id for an entry in `Song.video_sources`. `0` is the "未採番"
/// sentinel — `Song::ensure_video_source_ids` reassigns it on load.
/// v12 (`docs/plan_video.md` §2.3).
pub type VideoSourceId = u32;

/// Reference to an imported video file (mp4 / mov / mkv / webm). Path
/// resolution mirrors `AudioSource` — normal imports copy the file into
/// `<project_dir>/samples/<basename>_<hash8>.<ext>` and store
/// `ProjectRelative`. The decoded frames are NOT stored on the model:
/// each frame is decoded on demand by `daw_gui`'s video worker thread
/// from the path (= same SSoT pattern as `AudioSource`). The audio
/// stream is extracted to a sibling `.wav` at import time and exposed
/// via `audio_source_id` for the auto-generated pair audio track. v12
/// (`docs/plan_video.md` §2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct VideoSource {
    pub path: VideoSourcePath,
    /// Native pixel width / height as reported by the decoder. Project
    /// preview scales these to `Song.video_resolution`.
    pub width: u32,
    pub height: u32,
    /// Frames per second as reported by the decoder (= source FPS, not
    /// project FPS). Used for thumbnail seek and frame timing. Variable
    /// framerate (VFR) sources report their nominal FPS here; MVP
    /// assumes CFR.
    pub framerate: f32,
    /// Total duration in microseconds (= libav `AV_TIME_BASE` units).
    pub duration_micros: u64,
    /// FFmpeg codec name (`"h264"` / `"hevc"` / `"vp9"` / `"av1"` etc.).
    /// Free-form string; consumers use it only for display and
    /// diagnostics.
    pub codec: String,
    /// AudioSource holding the audio stream extracted from the video at
    /// import time. `None` when the source video had no audio stream or
    /// extraction was skipped. The audio is NOT played back through this
    /// link — `daw_audio` plays from `AudioEvent.source_id` in the
    /// auto-generated pair audio track. This back-reference exists for
    /// diagnostics and future "re-extract audio" operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source_id: Option<AudioSourceId>,
}

/// Path resolution strategy for a `VideoSource`. Mirrors
/// `AudioSourcePath` minus the `Generated` variant — video frames are
/// always backed by an on-disk file. v12 (`docs/plan_video.md` §2.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum VideoSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
}

/// One playable audio event inside an `AudioContent`. Maps a slice of
/// an `AudioSource` (`source_*_frames`) to a position in the clip
/// (`event_start_in_clip_beats` + `event_length_beats`) and applies
/// per-event playback parameters (gain / pan / pitch / fade / stretch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioEvent {
    pub source_id: AudioSourceId,
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    pub source_start_frames: u64,
    pub source_end_frames: u64,

    pub gain_db: f32,
    pub pan: f32,
    pub pitch_semitones: f32,
    pub formant_semitones: f32,

    pub stretch_mode: StretchMode,

    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,

    pub reversed: bool,
    pub muted: bool,

    /// Auto-detected transient frames (sample units). Phase 4+; empty
    /// in Phase 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub onsets: Vec<u64>,
    /// User-placed beat markers for `StretchMode::Stretch`. Phase 3+;
    /// empty in Phase 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub beat_markers: Vec<BeatMarker>,
}

impl Default for AudioEvent {
    fn default() -> Self {
        Self {
            source_id: 0,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            source_start_frames: 0,
            source_end_frames: 0,
            gain_db: 0.0,
            pan: 0.0,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            stretch_mode: StretchMode::Raw,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
            reversed: false,
            muted: false,
            onsets: Vec::new(),
            beat_markers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum StretchMode {
    #[default]
    Raw,
    Repitch,
    Stretch,
    Slice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum FadeCurve {
    #[default]
    Linear,
    Exponential,
    SCurve,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct BeatMarker {
    /// Position inside the source file (sample frames).
    pub source_frame: u64,
    /// Position inside the event (event-local beats) where the source
    /// frame is locked to land.
    pub locked_beat: f64,
}

// =============================================================================
// Video (v12, docs/plan_video.md §2.2)
// =============================================================================

/// Video clip content — an ordered list of video events that play within
/// the clip. Mirrors the Bitwig-style `Clip ⊃ Event` hierarchy used by
/// `AudioContent`; each `VideoEvent` maps a slice of a `VideoSource` to a
/// position in the clip. Events on the same clip can overlap (= the
/// preview composite alpha-blends them per #043 wgpu pipeline) or sit
/// side by side (= split clip).
///
/// `#[serde(deny_unknown_fields)]` is required so the `#[serde(untagged)]`
/// dispatch on `ClipContent` distinguishes `Audio` vs `Video` when the
/// outer field name (`events`) collides — disjoint inner required fields
/// (`source_start_frames` vs `source_start_micros`) handle the actual
/// disambiguation, but denying unknowns here prevents a future field
/// addition from accidentally widening the match.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct VideoContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<VideoEvent>,
}

/// One playable video event inside a `VideoContent`. Maps a slice of a
/// `VideoSource` (`source_*_micros`) to a position in the clip
/// (`event_start_in_clip_beats` + `event_length_beats`) and applies
/// per-event playback parameters (mute / fade).
///
/// **Required-field invariant**: `source_start_micros` MUST stay as a
/// required (no `#[serde(default)]`) field of distinct name from any
/// required field of `AudioEvent`. The untagged `ClipContent` dispatch
/// relies on this to disambiguate Video vs Audio JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct VideoEvent {
    pub source_id: VideoSourceId,
    /// Clip-local beat at which the event starts.
    pub event_start_in_clip_beats: f64,
    /// Duration of the event in clip-local beats. The source range
    /// (`source_end_micros - source_start_micros`) maps onto this
    /// duration at project tempo; tempo changes are not interpolated
    /// for MVP (= CFR assumption).
    pub event_length_beats: f64,
    /// Source-relative start position in microseconds (libav
    /// `AV_TIME_BASE` units). Disjoint from `AudioEvent`'s
    /// `source_start_frames` so untagged `ClipContent` can dispatch
    /// unambiguously.
    pub source_start_micros: u64,
    pub source_end_micros: u64,

    /// When `true` the event renders as a solid clear color (= black
    /// frame, no `VideoSource` decode). Useful for "blank" placeholders
    /// without removing the event.
    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

impl Default for VideoEvent {
    fn default() -> Self {
        Self {
            source_id: 0,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            source_start_micros: 0,
            source_end_micros: 0,
            muted: false,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        }
    }
}

// =============================================================================
// Image overlay / PiP (v13, docs/plan_image_overlay.md §2)
// =============================================================================

/// Stable id for an imported image source. `0` is the "未採番" sentinel.
/// v13 (`docs/plan_image_overlay.md` §2.1).
pub type ImageSourceId = u32;

/// Reference to an imported image file (PNG / JPEG / WebP / static
/// BMP / TIFF / TGA / GIF-static). Path resolution mirrors
/// `VideoSource` — normal imports copy the file into
/// `<project_dir>/images/<basename>_<hash8>.<ext>` and store
/// `ProjectRelative`. The decoded BGRA8 buffer is NOT stored on the
/// model: each image is decoded once at import time (= the `image`
/// crate returns a `RgbaImage`, daw_gui reorders to BGRA8 + uploads
/// to a GPU `TextureHandle` cached on the preview window for the
/// lifetime of the project). v13 (`docs/plan_image_overlay.md` §2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ImageSource {
    pub path: ImageSourcePath,
    /// Native pixel width / height as reported by the decoder. PiP
    /// rect (`ImageEvent.x/y/w/h`) is normalized so width/height are
    /// only used for aspect-fit fallback and metadata display.
    pub width: u32,
    pub height: u32,
    /// `image::ImageFormat` debug string (`"Png"` / `"Jpeg"` /
    /// `"WebP"` / etc.). Free-form, consumer uses for diagnostics
    /// only.
    pub format: String,
}

/// Path resolution strategy for an `ImageSource`. Mirrors
/// `VideoSourcePath`. v13 (`docs/plan_image_overlay.md` §2.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ImageSourcePath {
    ProjectRelative(PathBuf),
    Absolute(PathBuf),
}

/// Image clip content — an ordered list of image events that display
/// within the clip. Mirrors `VideoContent`'s shape so the existing
/// clip / event UX (Split / Glue / drag move / trim / fade in/out)
/// applies uniformly. Multiple events on the same clip can overlap
/// (= the preview composite alpha-blends them, top-event-wins by
/// emit order) or sit side by side (= splittable PiP montage).
///
/// `#[serde(deny_unknown_fields)]` is required so `#[serde(untagged)]`
/// `ClipContent` distinguishes `Image` vs `Audio` / `Video`: the
/// disjoint required field is `ImageEvent.opacity`, absent from both
/// `AudioEvent` and `VideoEvent`. Denying unknowns prevents a future
/// field addition from widening the match unexpectedly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct ImageContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<ImageEvent>,
}

/// One playable image event inside an `ImageContent`. Maps an
/// `ImageSource` to a position in the clip
/// (`event_start_in_clip_beats` + `event_length_beats`) and a PiP
/// rect in normalized 0-1 preview coordinates.
///
/// **Required-field invariant**: `opacity` MUST stay as a required
/// (no `#[serde(default)]`) field of distinct name from any required
/// field of `AudioEvent` and `VideoEvent`. The untagged `ClipContent`
/// dispatch relies on this to disambiguate Image vs Audio / Video
/// JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ImageEvent {
    pub source_id: ImageSourceId,
    /// Clip-local beat at which the event starts.
    pub event_start_in_clip_beats: f64,
    /// Duration of the event in clip-local beats. Image is static so
    /// the source has no inherent duration — the user freely extends
    /// the event by drag-trim.
    pub event_length_beats: f64,

    /// PiP rect in normalized preview-window coordinates. `(0.0, 0.0)`
    /// is the top-left corner of the preview window, `(1.0, 1.0)` is
    /// the bottom-right. `(x, y)` is the top-left of the image's
    /// rect, `(w, h)` is its width / height. Example:
    /// `(0.0, 0.0, 1.0, 1.0)` fills the entire preview; `(0.7, 0.0,
    /// 0.3, 0.3)` lands a 30%×30% logo in the top-right corner.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,

    /// Overall transparency (0.0 = fully transparent, 1.0 = fully
    /// opaque). Multiplied with the fade envelope (= the image
    /// crossfades and the user-set base opacity stack).
    ///
    /// **JSON disambiguation required field** — see struct doc.
    pub opacity: f32,

    /// v15 (`docs/plan_image_automation.md` rotation): rect 中心を旋回
    /// 中心とする 2D 回転 (radians、 clockwise positive)。 `0.0` =
    /// 軸並行 (互換)、 `±π` = 180°、 範囲は実用上 `-π..=π` で wrap。
    /// gui_01 #047 で `TexturedQuad.rotation_radians` が landing 次第
    /// preview / render passes に wire される。 lane override も同単位。
    #[serde(default)]
    pub rotation_radians: f32,

    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

impl Default for ImageEvent {
    fn default() -> Self {
        Self {
            source_id: 0,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            // PiP rect defaults to "full screen" so a freshly-dropped
            // image immediately shows something visible; the user can
            // shrink/move it in the inspector or preview drag handle.
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            opacity: 1.0,
            rotation_radians: 0.0,
            muted: false,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        }
    }
}

/// 口形状クラス (lip-sync, v21、`docs/plan_pakupaku.md`)。VOICEVOX phoneme を
/// この 7 種へ畳む。母音 a/i/u/e/o、撥音 N (ん)、閉口 Closed (cl 促音 / pau
/// ポーズ / 子音で続く母音が無い場合 / 未割当時の fallback)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub enum MouthShape {
    A,
    I,
    U,
    E,
    O,
    N,
    Closed,
}

/// 口形状 → `ImageSourceId` のマッピング (lip-sync, v21)。`0` = 未割当
/// sentinel。口 track (= vocal の `lipsync_target_track` が指す image track) に
/// `Track.mouth_map` として持たせる。各 slot には通常の image import で
/// `Song.image_sources` に登録した口画像の id を割り当てる (id 参照のみを保持し、
/// 画像実体はプール 1 箇所が SSoT)。
#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode,
)]
pub struct MouthMap {
    pub a: ImageSourceId,
    pub i: ImageSourceId,
    pub u: ImageSourceId,
    pub e: ImageSourceId,
    pub o: ImageSourceId,
    pub n: ImageSourceId,
    pub closed: ImageSourceId,
}

impl MouthMap {
    /// その口形状に割り当てられた id (未割当なら `0`)。
    pub fn get(&self, shape: MouthShape) -> ImageSourceId {
        match shape {
            MouthShape::A => self.a,
            MouthShape::I => self.i,
            MouthShape::U => self.u,
            MouthShape::E => self.e,
            MouthShape::O => self.o,
            MouthShape::N => self.n,
            MouthShape::Closed => self.closed,
        }
    }

    /// 描画に使う id を解決する。slot が未割当 (`0`) なら閉口へ fallback し、
    /// 閉口も未割当なら `0` (= 描画なし) を返す。
    pub fn resolve(&self, shape: MouthShape) -> ImageSourceId {
        let id = self.get(shape);
        if id != 0 { id } else { self.closed }
    }

    /// いずれかの slot に割当がある (= 口パクを生成する意味がある)。
    pub fn is_configured(&self) -> bool {
        [self.a, self.i, self.u, self.e, self.o, self.n, self.closed]
            .iter()
            .any(|&id| id != 0)
    }
}

/// Text clip content — `docs/plan_text_overlay.md` §2.2 (v16)。 単一行
/// の text overlay。 1 clip = 1 text、 複数行は禁止 (\n を含む `text`
/// は描画時に最初の改行で truncate するか、 model 側で reject する)。
///
/// `#[serde(deny_unknown_fields)]` は `ClipContent` の `#[serde(untagged)]`
/// dispatch のため必須。 `TextEvent.text` 等の disjoint required field で
/// Audio / Video / Image / Automation / MIDI と判別される。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct TextContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<TextEvent>,
}

/// `TextEvent.align` 用 enum。 horizontal alignment 3 選択
/// (`docs/plan_text_overlay.md` §1.5)。 vertical は単一行 text のため
/// 常に center (= box の縦中央 baseline) 固定。
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// One playable text event inside a `TextContent` (`docs/plan_text_overlay.md`
/// §2.2)。 単一行 text、 PiP rect + font + color + outline + shadow + rotation
/// 等の描画属性を持つ。 image PiP の `(x, y, w, h)` は project resolution の
/// letterbox 内 normalized 0..=1 で展開される (= 画像 PiP と同 idiom、 window
/// resize で aspect 維持)。
///
/// **JSON disambiguation required field**: `text: String` と
/// `font_family: String` の同時保持で他 variant と disjoint。 ただし
/// `ImageEvent` も `String` field を間接保持していないので衝突無し。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct TextEvent {
    /// 表示する文字列 (単一行、 UTF-8、 改行禁止)。
    pub text: String,
    /// system font 名 (例 `"Yu Gothic"` / `""` で default)。 glyphon が
    /// 解決失敗時は fallback chain で代替。
    pub font_family: String,
    /// project resolution 基準 px (= 1920x1080 で 48 px なら 48.0)。
    pub font_size_px: f32,
    /// 塗り色 RGBA (0.0..=1.0)。
    pub fill_color: [f32; 4],
    /// アウトライン色 RGBA。 `outline_width_px == 0.0` ならアウトライン無効。
    pub outline_color: [f32; 4],
    /// アウトライン太さ (project resolution 基準 px、 0.0 で無効)。
    pub outline_width_px: f32,
    /// ドロップシャドウ色 RGBA。 `shadow_offset == (0, 0)` && `shadow_blur
    /// == 0.0` && color alpha == 0.0 のとき shadow 無し。
    pub shadow_color: [f32; 4],
    /// シャドウオフセット (project resolution 基準 px、 (dx, dy))。
    pub shadow_offset_px: (f32, f32),
    /// シャドウぼかし半径 (project resolution 基準 px、 0.0 で hard shadow)。
    pub shadow_blur_px: f32,
    /// horizontal alignment (vertical は単一行 text で center 固定)。
    pub align: TextAlign,
    /// Clip-local beat (image / audio event と同 idiom)。
    pub event_start_in_clip_beats: f64,
    pub event_length_beats: f64,
    /// PiP rect in normalized 0-1 letterbox coordinates (image と同 idiom)。
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 全体 opacity (0..=1)。 fade envelope と multiply。
    pub opacity: f32,
    /// box 中心を旋回中心とする 2D 回転 (radians、 clockwise positive)。
    pub rotation_radians: f32,
    pub muted: bool,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

impl Default for TextEvent {
    fn default() -> Self {
        Self {
            text: String::new(),
            font_family: String::new(),
            font_size_px: 64.0,
            fill_color: [1.0, 1.0, 1.0, 1.0],
            outline_color: [0.0, 0.0, 0.0, 1.0],
            outline_width_px: 0.0,
            shadow_color: [0.0, 0.0, 0.0, 0.5],
            shadow_offset_px: (0.0, 0.0),
            shadow_blur_px: 0.0,
            align: TextAlign::Center,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            // 既定 PiP rect = 「中央付近の横帯」 (= 標準 title 位置)。
            x: 0.0,
            y: 0.4,
            w: 1.0,
            h: 0.2,
            opacity: 1.0,
            rotation_radians: 0.0,
            muted: false,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        }
    }
}

/// A free-time note inside a clip. `start_beat` is relative to the clip
/// start; `duration_beats` is the note length. `pitch` is a MIDI key
/// (0..=127), `velocity` is 0..=127. `lyric` is attached for VOICEVOX
/// singing synthesis and is `None` for purely instrumental tracks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Note {
    pub start_beat: f64,
    pub duration_beats: f64,
    pub pitch: u8,
    pub velocity: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lyric: Option<String>,
}

// =============================================================================
// Automation
// =============================================================================
//
// See `docs/plan_automation.md` for the full design. The summary:
//
// - Each `Track` carries `automation_lanes: Vec<AutomationLane>`. A lane has a
//   `target` (track-builtin volume / pan / mute, or a plugin parameter), a
//   `default_value` used outside any clip, and a list of `AutomationClip`.
// - `AutomationClip` is positioned along the track timeline and references a
//   `ContentId` in `Song.clip_contents` — the same shared store MIDI / Audio
//   clips use, so linked / independent copy machinery transparently applies.
// - `ClipContent::Automation(AutomationContent { points })` stores the actual
//   curve data. `#[serde(untagged)]` dispatch on `ClipContent` picks the
//   variant based on the disjoint field set (`notes` / `events` / `points`).

/// Phase 5 Step 5.1 (`docs/plan_automation.md` §10、 gui_01 #034): master row
/// 由来の automation lane を identify する sentinel track id。 widget crate
/// (`daw_ui_core::arrangement::MASTER_TRACK_ID`) と同値で mirror、 grep で
/// 両 crate を追跡可能にする。 `AutomationLaneKey { track: MASTER_TRACK_ID,
/// lane }` で master lane を表現、 EditRequest dispatch 側で
/// `track == MASTER_TRACK_ID` で `Song.song_lanes` か `Track.automation_lanes`
/// かを分岐する規約。 値は `u32::MAX` (= 通常 track id が 2^32 - 1 まで到達
/// する現実的なシナリオは無い)。
pub const MASTER_TRACK_ID: u32 = u32::MAX;

/// What an `AutomationLane` automates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum AutomationTarget {
    /// Built-in track parameter (volume / pan / mute / send).
    TrackBuiltin(TrackBuiltinParam),
    /// Plugin parameter on this track. `slot` identifies which plugin
    /// inside the track's chain. `param_id` is the CLAP `clap_id` /
    /// VST3 `ParamID` (both `u32`); the format is recovered through
    /// `Track.{instrument,midi_fx_chain[idx],fx_chain[idx]}.format`.
    PluginParam {
        slot: PluginSlot,
        param_id: u32,
    },
    /// Song-wide parameters. Lanes targeting these only make sense on
    /// a designated "master" track. M5 scope.
    SongTempo,
    SongTimeSigNumerator,
    /// v14: image track 上の PiP 数値 (x / y / w / h / opacity)。 lane
    /// の時間軸は track-global beats、 値域 0.0..=1.0。 image clip が
    /// 存在する時間範囲だけ lane 値が画像 PiP rect / opacity に適用さ
    /// れる (= `ImageEvent.field` を override)。 同 track の全 image
    /// clip が同一 lane で駆動される (`docs/plan_image_automation.md`
    /// §1.1 / §1.2)。
    ImageBuiltin(ImageBuiltinParam),
    /// v16 (`docs/plan_text_overlay.md` §2.3): text overlay の各 field を
    /// automation。 計 23 lane (位置 4 + 形 3 + fill RGBA + outline RGBA + width +
    /// shadow RGBA + offset xy + blur)。 image と同じく track-level、 text clip が
    /// 存在する時間範囲だけ lane 値が `TextEvent.<field>` を override。
    TextBuiltin(TextBuiltinParam),
    /// v19 (`docs/plan_tachie_group_transform.md` §4.3): 親グループトラックの
    /// 2D affine + opacity を automation する。`TrackBuiltin` (volume/pan) と同じ
    /// **クリップ非依存のトラックレベルパラメータ** — image/text clip の有無に
    /// 関係なく、グループが子を描画している間ずっと適用される。純粋に visual で
    /// daw_audio は評価しない (`daw_audio/src/automation.rs` の `_ => continue`)。
    GroupTransform(GroupTransformParam),
}

/// Built-in track parameter selector for `AutomationTarget::TrackBuiltin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum TrackBuiltinParam {
    Volume,
    Pan,
    Mute,
    /// Aux send level for `Track::sends[send_idx]` (linear, `0.0..=2.0`).
    /// `send_idx` is the send's position inside the track's `sends`
    /// array. See `Send` / `docs/plan_routing_graph.md`.
    SendGain { send_idx: u8 },
}

/// v16 (`docs/plan_text_overlay.md` §2.3): text overlay の各 field
/// selector。 計 23 variants で TextEvent 全描画属性 + 位置 + 形を
/// automation 可能。 lane の値は plain (= TextEvent field と同単位)、
/// normalize 経路 (= UI 表示の 0..=1) は target ごとに plain_to_norm で
/// 定義 (Color channel は 0..=1 そのまま、 size / offset / blur は
/// project px なので plain そのまま使用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum TextBuiltinParam {
    /// 位置 / サイズ (normalized 0..=1、 image と同 idiom)
    X, Y, W, H,
    /// 形 (Opacity / Rotation は image と同 idiom、 FontSize は px)
    Opacity, Rotation, FontSize,
    /// 塗り色 RGBA (各 channel 0..=1)
    FillR, FillG, FillB, FillA,
    /// アウトライン RGBA + Width (px)
    OutlineR, OutlineG, OutlineB, OutlineA, OutlineWidth,
    /// ドロップシャドウ RGBA + Offset XY + Blur (px)
    ShadowR, ShadowG, ShadowB, ShadowA, ShadowOffsetX, ShadowOffsetY, ShadowBlur,
}

/// v14: image track の PiP 数値 field selector (`docs/plan_image_automation
/// .md` §2.1)。 `AutomationTarget::ImageBuiltin` の payload。 v15 で
/// `Rotation` を追加 (= 2D 回転、 radians 単位)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum ImageBuiltinParam {
    /// PiP rect 左上 X (normalized 0..=1)。
    X,
    /// PiP rect 左上 Y (normalized 0..=1)。
    Y,
    /// PiP rect width (normalized 0..=1)。
    W,
    /// PiP rect height (normalized 0..=1)。
    H,
    /// 透明度 (0..=1)。 fade envelope と multiply、 さらに ImageEvent.
    /// opacity と multiply される (lane 経路 = override、 fade は重畳)。
    Opacity,
    /// v15: 2D 回転 (radians、 rect 中心が旋回中心、 clockwise positive)。
    /// 実用範囲 `-π..=π`、 範囲外は描画時に modulo 2π で正規化。 normalize
    /// 0..=1 は `(plain + π) / (2π)` mapping (Pan -1..=1 と同 idiom)。
    Rotation,
}

/// v19 (`docs/plan_tachie_group_transform.md` §4.3): `AutomationTarget::
/// GroupTransform` の field selector。`ImageBuiltinParam` と同じ Copy tag enum。
/// 正規化 (UI の 0..=1) は target ごとに `crate::automation::plain_to_norm` で
/// 定義: X/Y/AnchorX/AnchorY/Opacity は 0..=1 恒等、Rotation は Pan idiom
/// `(plain+π)/(2π)`、ScaleX/ScaleY は 0.1..10 の log space。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode)]
pub enum GroupTransformParam {
    X,
    Y,
    Rotation,
    ScaleX,
    ScaleY,
    AnchorX,
    AnchorY,
    Opacity,
}

/// Per-segment interpolation between two adjacent automation points.
/// The `curve` is an *incoming* attribute on a point — i.e. the curve
/// describing the line from the previous point to *this* one. The
/// first point's `curve` is unused.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, Encode, Decode,
)]
pub enum AutomationCurve {
    /// Step jump. The previous point's value holds until this point,
    /// then snaps to the new value.
    Hold,
    /// Straight line from the previous point to this one.
    #[default]
    Linear,
    /// 2D cubic Bezier。 `tension` は -1.0..=1.0、 数式 SSoT は
    /// [`crate::automation::apply_curve`] の `eval_bezier`。
    /// 制御点 x は固定 (1/3, 2/3)、 y は対角線と end-hold の lerp:
    /// `tension = 0.0` で 4 制御点が対角線上 → 直線 (Linear 等価)、
    /// `tension = +1.0` で滑らかな S 字 (両端緩い)、
    /// `tension = -1.0` で inverse S 字 (overshoot 系)。
    Bezier { tension: f32 },
    /// Exponential / power curve. `bend` is `-1.0..=1.0`: `0.0` is
    /// linear, positive values hold near the start and ramp toward the
    /// end, negative values invert. `value = a + (b - a) * u^(2^bend)`.
    Exponential { bend: f32 },
}

/// One control point inside an `AutomationContent`. Ordered by
/// `time_beat` ascending; insertion code MUST keep this invariant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AutomationPoint {
    /// Clip-local beat (`0.0` = clip start, `Clip.length_beats` = clip
    /// end). Negative or out-of-range values are clamped on read; the
    /// editor should never produce them.
    pub time_beat: f64,
    /// Plain (non-normalized) value in the target's native units. For
    /// volume that's `0.0..=2.0` (or whatever the GUI exposes), for a
    /// CLAP plugin parameter it's `min_value..=max_value`. The audio
    /// engine converts to `0.0..=1.0` per format right before sending
    /// to the plugin.
    pub value: f64,
    /// Interpolation strategy for the line *into* this point from the
    /// previous one. The first point's curve is meaningless.
    pub curve: AutomationCurve,
}

impl Default for AutomationPoint {
    fn default() -> Self {
        Self {
            time_beat: 0.0,
            value: 0.0,
            curve: AutomationCurve::Linear,
        }
    }
}

/// Phase 4 (`docs/plan_automation.md` §6): automation recording mode
/// selected from the transport bar 4-way toggle. Bitwig / Ableton Live
/// / Reaper の慣例に従う。 session-only (project 保存対象外、 起動時
/// `Read`)。 audio thread もこの enum を読んで recording lane の
/// curve eval をバイパスする予定 (Phase 4 Step C+)。
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub enum RecordingMode {
    /// Curve を読むだけ (default)。 knob 操作は `lane.default_value`
    /// を更新するのみで、 point は生成されない。
    #[default]
    Read,
    /// knob を触っている間だけ点を打ち、 release で curve に戻る
    /// (Bitwig / Live `Touch`)。
    Touch,
    /// 1 度触れたら playback 停止まで上書きし続ける (`Latch`)。
    Latch,
    /// playback 再生中ずっと knob 値で curve を上書きする
    /// (`Write` = overdub)。
    Write,
}

/// `ClipContent::Automation` payload. The actual curve sits inside the
/// shared content store (`Song.clip_contents`) so multiple
/// `AutomationClip`s with the same `content_id` share the curve
/// (linked-clip pattern, mirroring MIDI clips).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(deny_unknown_fields)]
pub struct AutomationContent {
    /// Sorted by `AutomationPoint::time_beat` ascending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<AutomationPoint>,
}

/// One automation lane attached to a `Track`. Each lane targets one
/// parameter (`AutomationTarget`) and contains a list of clips holding
/// the actual point data plus a `default_value` used everywhere the
/// clips don't cover (gaps, before the first clip, after the last,
/// or whenever `enabled = false`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AutomationLane {
    /// Stable id within the owning track. `0` is the "未採番" sentinel —
    /// reassigned by `Track::ensure_lane_ids` on load.
    #[serde(default)]
    pub id: u32,
    pub target: AutomationTarget,
    /// Constant value used outside any clip and whenever
    /// `enabled = false`. Two-way bound to the track inspector knob:
    /// twisting the knob edits this field, and editing this field
    /// updates the knob display. Stored in the target's plain units
    /// (same convention as `AutomationPoint::value`).
    pub default_value: f64,
    /// When `false` the entire lane is bypassed: the target is driven
    /// purely by `default_value` and the curve is rendered greyed-out
    /// in the arrangement (Bitwig "Disable Automation" / Reaper
    /// "Bypass envelope").
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When `false` the lane row is hidden in the arrangement (still
    /// listed in the inspector). Independent of `enabled` — a lane
    /// can be active but visually collapsed away.
    #[serde(default = "default_true")]
    pub visible: bool,
    /// Lane row height in pixels. Default 60. User-resizable in
    /// Phase 1+.
    #[serde(default = "default_lane_height_px")]
    pub height_px: u16,
    /// Automation clips placed along the track timeline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clips: Vec<AutomationClip>,
    /// Per-lane stable id allocator for `AutomationClip`. `0` is the
    /// sentinel; valid allocations start at `1`.
    #[serde(default)]
    pub next_clip_id: u32,
}

fn default_true() -> bool {
    true
}

fn default_lane_height_px() -> u16 {
    60
}

impl AutomationLane {
    pub fn new(target: AutomationTarget, default_value: f64) -> Self {
        Self {
            id: 0,
            target,
            default_value,
            enabled: true,
            visible: true,
            height_px: default_lane_height_px(),
            clips: Vec::new(),
            next_clip_id: 1,
        }
    }

    /// Allocate a new stable clip id within this lane.
    pub fn alloc_clip_id(&mut self) -> u32 {
        let id = self.next_clip_id.max(1);
        self.next_clip_id = id + 1;
        id
    }

    /// Re-assign stable ids to all clips inside the lane. Idempotent.
    pub fn ensure_clip_ids(&mut self) {
        for clip in &mut self.clips {
            if clip.id == 0 {
                clip.id = self.next_clip_id.max(1);
                self.next_clip_id = clip.id + 1;
            } else if clip.id >= self.next_clip_id {
                self.next_clip_id = clip.id + 1;
            }
        }
        if self.next_clip_id == 0 {
            self.next_clip_id = 1;
        }
    }

    pub fn clip_index_by_id(&self, clip_id: u32) -> Option<usize> {
        self.clips.iter().position(|c| c.id == clip_id)
    }

    pub fn clip_by_id(&self, clip_id: u32) -> Option<&AutomationClip> {
        self.clips.iter().find(|c| c.id == clip_id)
    }

    pub fn clip_by_id_mut(&mut self, clip_id: u32) -> Option<&mut AutomationClip> {
        self.clips.iter_mut().find(|c| c.id == clip_id)
    }
}

/// One automation clip inside an `AutomationLane`. Same shape as `Clip`
/// (id / start / length / shared content via `content_id`) — the
/// payload variant just happens to be `ClipContent::Automation`. Two
/// `AutomationClip`s sharing a `content_id` are linked (REAPER pooled
/// MIDI / linked-clip pattern), mirroring MIDI/Audio clips.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AutomationClip {
    /// Stable id within the owning lane. `0` is the sentinel —
    /// reassigned by `AutomationLane::ensure_clip_ids` on load.
    #[serde(default)]
    pub id: u32,
    /// **Legacy field**: v20+ は `Song.clip_content_names[content_id]` が
    /// SSoT。 `Clip.name` と同じく load 時に map へ drain される。
    /// 空なら serialize されない。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub start_beat: f64,
    pub length_beats: f64,
    /// Reference into `Song.clip_contents`. `0` is the "未採番" sentinel
    /// (reassigned by `Song::ensure_clip_contents` on load). Multiple
    /// clips with the same `content_id` share their curve. The
    /// referenced `ClipContent` MUST be the `Automation` variant —
    /// loaders log a warning and treat foreign variants as empty.
    #[serde(default)]
    pub content_id: ContentId,
}

// ---------------------------------------------------------------------------
// Stable address keys (gui_01 #028 §11.2 と 1:1 対応)
// ---------------------------------------------------------------------------
//
// Edit-request 系 (`AppEvent::MoveAutomationPoints`, `MoveAutomationClips` 等)
// で使う "どの track のどの lane のどの clip / point" を指す構造化キー。
// 旧案の `(track_id, lane_id, clip_id, point_idx)` 4-tuple をフラットに渡す
// より、 hit-test と Edit/Undo 構築の両側で型違反を compile error で検出
// できる利点がある。

/// Address of an `AutomationLane` inside the song.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct AutomationLaneKey {
    pub track: u32,
    pub lane: u32,
}

/// Address of an `AutomationClip` (= one clip inside one lane).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct AutomationClipKey {
    pub track: u32,
    pub lane: u32,
    pub clip: u32,
}

impl AutomationClipKey {
    /// Drop the clip part to address the owning lane.
    #[inline]
    pub fn lane_key(self) -> AutomationLaneKey {
        AutomationLaneKey {
            track: self.track,
            lane: self.lane,
        }
    }
}

/// Address of one `AutomationPoint` inside a clip. `point_idx` is **only
/// valid within the same frame** — point add / delete renumbers indices,
/// so a drag session that spans frames must keep the previous index in
/// the session struct, not in this key.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct AutomationPointKey {
    pub clip: AutomationClipKey,
    pub point_idx: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_roundtrip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(value).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn song_default_roundtrip() {
        let song = Song::default();
        assert_eq!(json_roundtrip(&song), song);
    }

    /// Regression test for sidechain pipeline: when `ensure_ids()` rewrites
    /// a `track.id == 0` sentinel into a fresh id, every reference to that
    /// old id (= `sidechain_sources` entries and `parent_group_id`) must be
    /// remapped too. Otherwise the references dangle, `compile_schedule`
    /// silently skips them (treating dangling sidechain sources as
    /// `continue`), and the user sees no sidechain signal even though the
    /// dropdown is wired correctly.
    ///
    /// Setup:
    ///   Track Kick id=0 (sentinel) → after ensure_ids gets id=2
    ///   Track Bass id=1 with fx[0].sidechain_sources=[Some(0)] (= Kick)
    ///                    parent_group_id = Some(0) (= Kick)
    /// Expected after ensure_ids:
    ///   Bass.fx[0].sidechain_sources == [Some(2)]
    ///   Bass.parent_group_id == Some(2)
    #[test]
    fn ensure_ids_remaps_sidechain_sources_and_parent_group_id() {
        use crate::plugin_format::PluginFormat;

        let mut song = Song {
            bpm: 120.0,
            time_sig: (4, 4),
            length_beats: 64.0,
            tracks: vec![
                Track {
                    id: 0, // sentinel — will be replaced by ensure_ids
                    name: "Kick".into(),
                    ..Track::default()
                },
                Track {
                    id: 1,
                    name: "Bass".into(),
                    parent_group_id: Some(0), // points at Kick's old sentinel id
                    fx_chain: vec![PluginInstance {
                        plugin_id: "test.compressor".into(),
                        format: PluginFormat::Vst3,
                        state: None,
                        sidechain_sources: vec![Some(0)], // points at Kick
                    }],
                    ..Track::default()
                },
            ],
            next_track_id: 2,
            ..Song::default()
        };

        song.ensure_ids();

        // Kick got rebased.
        let kick = &song.tracks[0];
        assert_ne!(kick.id, 0, "ensure_ids should replace sentinel id 0");
        let new_kick_id = kick.id;

        // Bass kept its id but its references must be remapped.
        let bass = &song.tracks[1];
        assert_eq!(bass.id, 1);
        assert_eq!(
            bass.parent_group_id,
            Some(new_kick_id),
            "parent_group_id pointing at sentinel must be remapped to the new id"
        );
        assert_eq!(
            bass.fx_chain[0].sidechain_sources,
            vec![Some(new_kick_id)],
            "sidechain_sources pointing at sentinel must be remapped to the new id"
        );
    }

    #[test]
    fn project_file_roundtrip() {
        let pf = ProjectFile {
            version: CURRENT_VERSION,
            song: Song::default(),
        };
        assert_eq!(json_roundtrip(&pf), pf);
    }

    #[test]
    fn empty_note_serializes_as_minimal_object() {
        // velocity 0 / pitch 0 / start 0 / duration 0 — lyric None is
        // skipped via `skip_serializing_if`, the rest are required fields.
        assert_eq!(
            serde_json::to_string(&Note::default()).unwrap(),
            r#"{"start_beat":0.0,"duration_beats":0.0,"pitch":0,"velocity":0}"#
        );
    }

    #[test]
    fn note_with_lyric_serializes_compactly() {
        let note = Note {
            start_beat: 0.5,
            duration_beats: 1.0,
            pitch: 60,
            velocity: 100,
            lyric: Some("こ".into()),
        };
        assert_eq!(
            serde_json::to_string(&note).unwrap(),
            r#"{"start_beat":0.5,"duration_beats":1.0,"pitch":60,"velocity":100,"lyric":"こ"}"#
        );
        assert_eq!(json_roundtrip(&note), note);
    }

    #[test]
    fn vocal_clip_roundtrip() {
        let song = Song {
            tracks: vec![Track {
                name: "Vocal".into(),
                source: InstrumentSource::Vocal {
                    speaker_id: 3,
                    style_name: "ノーマル".into(),
                },
                clips: vec![Clip {
                    id: 1,
                    name: "こんにちは".into(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    content_id: 0,
                    notes: vec![
                        Note {
                            start_beat: 0.0,
                            duration_beats: 1.0,
                            pitch: 60,
                            velocity: 100,
                            lyric: Some("こ".into()),
                        },
                        Note {
                            start_beat: 1.5,
                            duration_beats: 0.5,
                            pitch: 62,
                            velocity: 100,
                            lyric: Some("ん".into()),
                        },
                    ],
                    color: None,
                    auto_lipsync: false,
                }],
                ..Track::default()
            }],
            ..Song::default()
        };
        assert_eq!(json_roundtrip(&song), song);
    }

    #[test]
    fn current_version_is_pinned() {
        // Bumped to 20 for shared clip names: `Song.clip_content_names`
        // (`HashMap<ContentId, String>`) is added and the legacy per-clip
        // `Clip.name` / `AutomationClip.name` are drained into it on load.
        // v19 files forward-migrate via `#[serde(default)]` (empty map) +
        // `ensure_clip_contents` backfill. Pinning the constant catches
        // accidental rollback. See `docs/plan_clip_shared_name.md`.
        assert_eq!(CURRENT_VERSION, 21);
    }

    #[test]
    fn v19_clip_names_drain_into_shared_map_and_rename_is_group_wide() {
        // Two linked clips (same content_id) carrying legacy v19 per-clip
        // names. `ensure_clip_contents` drains the first non-empty name
        // into the shared `clip_content_names` map and clears `Clip.name`.
        let mut song = Song {
            tracks: vec![Track {
                id: 1,
                clips: vec![
                    Clip {
                        id: 1,
                        name: "Verse".into(),
                        length_beats: 4.0,
                        content_id: 7,
                        ..Clip::default()
                    },
                    Clip {
                        id: 2,
                        name: "Verse".into(),
                        start_beat: 4.0,
                        length_beats: 4.0,
                        content_id: 7,
                        ..Clip::default()
                    },
                ],
                ..Track::default()
            }],
            ..Song::default()
        };
        song.ensure_clip_contents();

        // Legacy per-clip names are drained to empty; the shared map owns it.
        assert_eq!(song.tracks[0].clips[0].name, "");
        assert_eq!(song.tracks[0].clips[1].name, "");
        assert_eq!(song.content_name(7), "Verse");

        // Renaming via the shared map renames the whole linked group: both
        // clips resolve the same name through their shared content_id.
        song.set_content_name(7, "Chorus".into());
        let cid0 = song.tracks[0].clips[0].content_id;
        let cid1 = song.tracks[0].clips[1].content_id;
        assert_eq!(song.content_name(cid0), "Chorus");
        assert_eq!(song.content_name(cid1), "Chorus");

        // fork_content copies the name under a fresh id, then diverges
        // independently of the source group.
        let forked = song.fork_content(7);
        assert_ne!(forked, 7);
        assert_eq!(song.content_name(forked), "Chorus");
        song.set_content_name(forked, "Bridge".into());
        assert_eq!(song.content_name(forked), "Bridge");
        assert_eq!(song.content_name(7), "Chorus");

        // GC drops names whose content_id is no longer referenced by any
        // clip (the fork has no clip pointing at it).
        song.gc_clip_contents();
        assert_eq!(song.content_name(7), "Chorus");
        assert!(!song.clip_content_names.contains_key(&forked));
    }

    #[test]
    fn v17_track_and_clip_load_forward_with_none_color() {
        // A v17 .daw file (no `color` key on Track / Clip) must load with
        // `color == None` (= derived palette / inherit), proving the v18
        // field is `#[serde(default)]`.
        let v17_json = r#"{
            "id": 3,
            "name": "Lead",
            "volume": 1.0,
            "pan": 0.0,
            "next_clip_id": 2,
            "clips": [
                {
                    "id": 1,
                    "name": "C",
                    "start_beat": 0.0,
                    "length_beats": 4.0,
                    "content_id": 1
                }
            ]
        }"#;
        let track: Track = serde_json::from_str(v17_json).unwrap();
        assert_eq!(track.color, None);
        assert_eq!(track.clips[0].color, None);
    }

    #[test]
    fn track_and_clip_color_bincode_round_trip() {
        // v18 color fields survive a bincode encode/decode (the IPC + on-disk
        // path). `None` and `Some` both round-trip.
        let cfg = bincode::config::standard();
        let track = Track {
            id: 9,
            color: Some([0.25, 0.5, 0.75]),
            clips: vec![
                Clip { id: 1, color: None, ..Clip::default() },
                Clip { id: 2, color: Some([0.1, 0.2, 0.3]), ..Clip::default() },
            ],
            ..Track::default()
        };
        let bytes = bincode::encode_to_vec(&track, cfg).unwrap();
        let (decoded, _): (Track, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(decoded.color, Some([0.25, 0.5, 0.75]));
        assert_eq!(decoded.clips[0].color, None);
        assert_eq!(decoded.clips[1].color, Some([0.1, 0.2, 0.3]));
    }

    #[test]
    fn v4_track_loads_forward_with_default_routing_fields() {
        // A v4 .daw file (no `parent_group_id` / `reported_latency_samples`
        // keys) must round-trip through serde_json into a v5 `Track`
        // with defaulted graph fields.
        let v4_json = r#"{
            "id": 7,
            "name": "Lead",
            "volume": 0.9,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
        let track: Track = serde_json::from_str(v4_json).unwrap();
        assert_eq!(track.id, 7);
        assert_eq!(track.parent_group_id, None);
        assert_eq!(track.reported_latency_samples, 0);
    }

    #[test]
    fn track_with_parent_group_id_roundtrip() {
        // The "group" role is implicit (track 1 here ends up acting as
        // a group because track 2 points at it via parent_group_id).
        // No explicit `kind` field exists.
        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Drums".into(),
                    parent_group_id: None,
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Kick".into(),
                    parent_group_id: Some(1),
                    ..Track::default()
                },
            ],
            ..Song::default()
        };
        let restored: Song = json_roundtrip(&song);
        assert_eq!(restored, song);
    }

    // ====================================================================
    // Aux send / return (v17) — `Track.sends: Vec<Send>`
    // ====================================================================

    #[test]
    fn v16_track_loads_with_empty_sends() {
        // A v16 .daw file has no `sends` key; forward-migration via
        // `#[serde(default)]` must populate an empty Vec.
        let v16_json = r#"{
            "id": 5,
            "name": "Vocal",
            "volume": 1.0,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
        let track: Track = serde_json::from_str(v16_json).unwrap();
        assert_eq!(track.id, 5);
        assert!(track.sends.is_empty());
    }

    #[test]
    fn track_with_sends_roundtrips_through_serde_and_bincode() {
        // Vocal sends post-fader to a Reverb return and pre-fader (muted)
        // to a Delay return. Both serde (save) and bincode (IPC) must
        // preserve the sends exactly.
        let song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Vocal".into(),
                    sends: vec![
                        Send {
                            dest_track_id: 2,
                            gain: 0.5,
                            mode: SendMode::PostFader,
                            enabled: true,
                        },
                        Send {
                            dest_track_id: 3,
                            gain: 1.0,
                            mode: SendMode::PreFader,
                            enabled: false,
                        },
                    ],
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Reverb".into(),
                    ..Track::default()
                },
                Track {
                    id: 3,
                    name: "Delay".into(),
                    ..Track::default()
                },
            ],
            ..Song::default()
        };

        assert_eq!(json_roundtrip(&song), song, "serde (save) must preserve sends");

        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&song, cfg).unwrap();
        let (decoded, _): (Song, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(decoded, song, "bincode (IPC) must preserve sends");
    }

    #[test]
    fn ensure_ids_remaps_send_dest_track_id() {
        // A Vocal track sends to a Reverb return whose id is the `0`
        // sentinel. After ensure_ids rebases the return, the send's
        // `dest_track_id` must follow — otherwise the send dangles and
        // `compile_schedule` silently drops it (no reverb).
        let mut song = Song {
            tracks: vec![
                Track {
                    id: 0, // sentinel — Reverb return, rebased by ensure_ids
                    name: "Reverb".into(),
                    ..Track::default()
                },
                Track {
                    id: 1,
                    name: "Vocal".into(),
                    sends: vec![Send {
                        dest_track_id: 0, // points at Reverb's sentinel id
                        gain: 0.5,
                        mode: SendMode::PostFader,
                        enabled: true,
                    }],
                    ..Track::default()
                },
            ],
            next_track_id: 2,
            ..Song::default()
        };

        song.ensure_ids();

        let new_reverb_id = song.tracks[0].id;
        assert_ne!(new_reverb_id, 0, "ensure_ids should replace sentinel id 0");
        assert_eq!(
            song.tracks[1].sends[0].dest_track_id,
            new_reverb_id,
            "send dest pointing at the sentinel must be remapped to the new id"
        );
    }

    // ====================================================================
    // Automation (v8) — `Track.automation_lanes` + `ClipContent::Automation`
    // ====================================================================

    #[test]
    fn v7_track_loads_with_empty_automation_lanes() {
        // A v7 .daw file has no `automation_lanes` / `next_lane_id` keys.
        // Forward-migration via `#[serde(default)]` must populate empty
        // Vec / 0 without losing other fields.
        let v7_json = r#"{
            "id": 3,
            "name": "Lead",
            "volume": 0.9,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
        let track: Track = serde_json::from_str(v7_json).unwrap();
        assert_eq!(track.id, 3);
        assert!(track.automation_lanes.is_empty());
        assert_eq!(track.next_lane_id, 0);
    }

    #[test]
    fn ensure_lane_ids_assigns_sentinel() {
        // Lane id 0 (sentinel) gets a fresh id; non-zero lane ids are
        // left alone but bump `next_lane_id` above the highest seen.
        let mut track = Track {
            id: 1,
            name: "T".into(),
            automation_lanes: vec![
                AutomationLane {
                    id: 0,
                    ..AutomationLane::new(
                        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                        1.0,
                    )
                },
                AutomationLane {
                    id: 5,
                    ..AutomationLane::new(
                        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan),
                        0.0,
                    )
                },
            ],
            next_lane_id: 0,
            ..Track::default()
        };
        track.ensure_lane_ids();
        // Sentinel got reassigned; counter is bumped above max seen.
        assert_ne!(track.automation_lanes[0].id, 0);
        assert_eq!(track.automation_lanes[1].id, 5);
        assert!(track.next_lane_id > 5);
    }

    #[test]
    fn automation_clip_content_roundtrip() {
        // A song with one automation lane + one clip + one point
        // round-trips through serde_json bit-for-bit. Exercises
        // `ClipContent::Automation` untagged dispatch.
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        time_beat: 0.0,
                        value: 0.5,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        time_beat: 4.0,
                        value: 1.0,
                        curve: AutomationCurve::Bezier { tension: 0.25 },
                    },
                ],
            }),
        );
        song.tracks.push(Track {
            id: 1,
            name: "T".into(),
            automation_lanes: vec![AutomationLane {
                id: 1,
                clips: vec![AutomationClip {
                    id: 1,
                    name: "auto1".into(),
                    start_beat: 0.0,
                    length_beats: 4.0,
                    content_id: cid,
                }],
                next_clip_id: 2,
                ..AutomationLane::new(
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                    0.85,
                )
            }],
            next_lane_id: 2,
            ..Track::default()
        });

        let restored: Song = json_roundtrip(&song);
        assert_eq!(restored, song);
        assert!(matches!(
            restored.clip_contents[&cid],
            ClipContent::Automation(_)
        ));
    }

    #[test]
    fn automation_clip_counts_toward_clip_content_refcount() {
        // Same `content_id` shared by a MIDI clip *and* an automation
        // clip should refcount as 2 — `clip_content_refcount` walks
        // both `Track.clips` and `automation_lanes[].clips`.
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent::default()),
        );
        song.tracks.push(Track {
            id: 1,
            name: "T".into(),
            clips: vec![Clip {
                id: 1,
                name: "main".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
            }],
            automation_lanes: vec![AutomationLane {
                id: 1,
                clips: vec![AutomationClip {
                    id: 1,
                    name: "auto1".into(),
                    start_beat: 0.0,
                    length_beats: 4.0,
                    content_id: cid,
                }],
                ..AutomationLane::new(
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                    1.0,
                )
            }],
            ..Track::default()
        });
        assert_eq!(song.clip_content_refcount(cid), 2);
    }

    #[test]
    fn gc_clip_contents_keeps_automation_clip_references() {
        // A content_id only referenced by an automation clip must
        // survive `gc_clip_contents` — earlier impl walked only
        // `Track.clips` and would drop it.
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent::default()),
        );
        song.tracks.push(Track {
            id: 1,
            name: "T".into(),
            automation_lanes: vec![AutomationLane {
                id: 1,
                clips: vec![AutomationClip {
                    id: 1,
                    name: "auto1".into(),
                    start_beat: 0.0,
                    length_beats: 4.0,
                    content_id: cid,
                }],
                ..AutomationLane::new(
                    AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                    1.0,
                )
            }],
            ..Track::default()
        });
        song.gc_clip_contents();
        assert!(song.clip_contents.contains_key(&cid));
    }

    #[test]
    fn automation_target_hashes_distinguish_variants() {
        // Targets are used as HashMap keys (e.g. last-touched param
        // bookkeeping). Same-shape variants with different payloads
        // must produce different hashes.
        use std::collections::HashSet;
        let mut s = HashSet::new();
        s.insert(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume));
        s.insert(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan));
        s.insert(AutomationTarget::PluginParam {
            slot: PluginSlot::Instrument,
            param_id: 7,
        });
        s.insert(AutomationTarget::PluginParam {
            slot: PluginSlot::Fx(0),
            param_id: 7,
        });
        assert_eq!(s.len(), 4);
    }

    // ====================================================================
    // Video (v12) — `Track.kind`, `Song.video_sources`,
    // `ClipContent::Video`, project-level resolution / framerate.
    // See `docs/plan_video.md`.
    // ====================================================================

    #[test]
    fn v11_track_loads_forward_with_default_kind() {
        // A v11 `.daw` file has no `kind` key on `Track`. Forward-
        // migration via `#[serde(default)]` must populate `Audio`.
        let v11_json = r#"{
            "id": 4,
            "name": "Lead",
            "volume": 0.9,
            "pan": 0.0,
            "next_clip_id": 1
        }"#;
        let track: Track = serde_json::from_str(v11_json).unwrap();
        assert_eq!(track.id, 4);
    }

    #[test]
    fn v11_song_loads_forward_with_default_video_fields() {
        // A v11 `.daw` file has no `video_sources` / `next_video_source_id`
        // / `video_resolution` / `video_framerate` keys. Forward-migration
        // via `#[serde(default)]` must populate empty / 1080p / 30fps.
        let v11_json = r#"{
            "bpm": 120.0,
            "time_sig": [4, 4],
            "length_beats": 64.0
        }"#;
        let song: Song = serde_json::from_str(v11_json).unwrap();
        assert!(song.video_sources.is_empty());
        assert_eq!(song.next_video_source_id, 0);
        assert_eq!(song.video_resolution, (1920, 1080));
        assert!((song.video_framerate - 30.0).abs() < f32::EPSILON);
    }

    #[test]
    fn video_track_with_clip_content_roundtrip() {
        // A song with one video track + one video clip + one event
        // round-trips through serde_json bit-for-bit. Exercises
        // `ClipContent::Video` untagged dispatch against the existing
        // `Midi` / `Audio` / `Automation` variants.
        let mut song = Song::default();
        let vsrc_id = song.alloc_video_source_id();
        song.video_sources.insert(
            vsrc_id,
            VideoSource {
                path: VideoSourcePath::ProjectRelative("samples/clip.mp4".into()),
                width: 1920,
                height: 1080,
                framerate: 30.0,
                duration_micros: 10_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: vsrc_id,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 4.0,
                    source_start_micros: 0,
                    source_end_micros: 2_000_000,
                    muted: false,
                    fade_in_beats: 0.25,
                    fade_out_beats: 0.5,
                    fade_in_curve: FadeCurve::Linear,
                    fade_out_curve: FadeCurve::SCurve,
                }],
            }),
        );
        song.tracks.push(Track {
            id: 1,
            name: "Vid".into(),
            clips: vec![Clip {
                id: 1,
                name: "intro".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
            }],
            next_clip_id: 2,
            ..Track::default()
        });

        let restored: Song = json_roundtrip(&song);
        assert_eq!(restored, song);
        assert!(matches!(
            restored.clip_contents[&cid],
            ClipContent::Video(_)
        ));
    }

    #[test]
    fn untagged_dispatch_disambiguates_audio_vs_video_events() {
        // Regression test: `ClipContent::Audio` and `ClipContent::Video`
        // both serialize their inner list under `"events"`, so the
        // untagged dispatch falls back to inner-struct required-field
        // presence. AudioEvent requires `source_start_frames`,
        // VideoEvent requires `source_start_micros`; a JSON shaped for
        // one variant must NOT silently match the other.
        let audio_json = r#"{
            "events": [{
                "source_id": 1,
                "event_start_in_clip_beats": 0.0,
                "event_length_beats": 1.0,
                "source_start_frames": 0,
                "source_end_frames": 44100,
                "gain_db": 0.0,
                "pan": 0.0,
                "pitch_semitones": 0.0,
                "formant_semitones": 0.0,
                "stretch_mode": "Raw",
                "fade_in_beats": 0.0,
                "fade_out_beats": 0.0,
                "fade_in_curve": "Linear",
                "fade_out_curve": "Linear",
                "reversed": false,
                "muted": false
            }]
        }"#;
        let video_json = r#"{
            "events": [{
                "source_id": 1,
                "event_start_in_clip_beats": 0.0,
                "event_length_beats": 1.0,
                "source_start_micros": 0,
                "source_end_micros": 1000000,
                "muted": false,
                "fade_in_beats": 0.0,
                "fade_out_beats": 0.0,
                "fade_in_curve": "Linear",
                "fade_out_curve": "Linear"
            }]
        }"#;
        let audio: ClipContent = serde_json::from_str(audio_json).unwrap();
        let video: ClipContent = serde_json::from_str(video_json).unwrap();
        assert!(matches!(audio, ClipContent::Audio(_)));
        assert!(matches!(video, ClipContent::Video(_)));
    }

    #[test]
    fn alloc_video_source_id_bumps_counter() {
        let mut song = Song::default();
        let a = song.alloc_video_source_id();
        let b = song.alloc_video_source_id();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(song.next_video_source_id, 3);
    }

    #[test]
    fn video_source_refcount_counts_events() {
        let mut song = Song::default();
        let vid = song.alloc_video_source_id();
        song.video_sources.insert(
            vid,
            VideoSource {
                path: VideoSourcePath::Absolute("/tmp/v.mp4".into()),
                width: 640,
                height: 480,
                framerate: 30.0,
                duration_micros: 1_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let cid_a = song.alloc_content_id();
        song.clip_contents.insert(
            cid_a,
            ClipContent::Video(VideoContent {
                events: vec![
                    VideoEvent {
                        source_id: vid,
                        ..VideoEvent::default()
                    },
                    VideoEvent {
                        source_id: vid,
                        ..VideoEvent::default()
                    },
                ],
            }),
        );
        let cid_b = song.alloc_content_id();
        song.clip_contents.insert(
            cid_b,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: vid,
                    ..VideoEvent::default()
                }],
            }),
        );
        assert_eq!(song.video_source_refcount(vid), 3);
    }

    #[test]
    fn gc_video_sources_drops_orphans() {
        let mut song = Song::default();
        let live_id = song.alloc_video_source_id();
        let orphan_id = song.alloc_video_source_id();
        song.video_sources.insert(
            live_id,
            VideoSource {
                path: VideoSourcePath::Absolute("/tmp/live.mp4".into()),
                width: 640,
                height: 480,
                framerate: 30.0,
                duration_micros: 1_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        song.video_sources.insert(
            orphan_id,
            VideoSource {
                path: VideoSourcePath::Absolute("/tmp/orphan.mp4".into()),
                width: 640,
                height: 480,
                framerate: 30.0,
                duration_micros: 1_000_000,
                codec: "h264".into(),
                audio_source_id: None,
            },
        );
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: live_id,
                    ..VideoEvent::default()
                }],
            }),
        );

        song.gc_video_sources();
        assert!(song.video_sources.contains_key(&live_id));
        assert!(!song.video_sources.contains_key(&orphan_id));
    }

    // =========================================================================
    // Image overlay (v13, docs/plan_image_overlay.md §P1 invariants)
    // =========================================================================

    #[test]
    fn clipcontent_untagged_image_dispatches_via_opacity_field() {
        // The disambiguator for the new Image variant is the required
        // `opacity` field on `ImageEvent`. Audio / Video JSON shaped
        // without `opacity` must NOT silently match Image, and Image
        // JSON shaped with `opacity` must NOT match Audio / Video
        // (deny_unknown_fields on Content structs ensures the latter).
        let image_json = r#"{
            "events": [{
                "source_id": 1,
                "event_start_in_clip_beats": 0.0,
                "event_length_beats": 4.0,
                "x": 0.1,
                "y": 0.1,
                "w": 0.3,
                "h": 0.3,
                "opacity": 1.0,
                "muted": false,
                "fade_in_beats": 0.0,
                "fade_out_beats": 0.0,
                "fade_in_curve": "Linear",
                "fade_out_curve": "Linear"
            }]
        }"#;
        let image: ClipContent = serde_json::from_str(image_json).unwrap();
        assert!(matches!(image, ClipContent::Image(_)));
    }

    #[test]
    fn alloc_image_source_id_bumps_counter() {
        let mut song = Song::default();
        let a = song.alloc_image_source_id();
        let b = song.alloc_image_source_id();
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(song.next_image_source_id, 3);
    }

    #[test]
    fn image_source_refcount_counts_events_across_clips() {
        let mut song = Song::default();
        let img = song.alloc_image_source_id();
        song.image_sources.insert(
            img,
            ImageSource {
                path: ImageSourcePath::Absolute("/tmp/logo.png".into()),
                width: 256,
                height: 256,
                format: "Png".into(),
            },
        );
        let cid_a = song.alloc_content_id();
        song.clip_contents.insert(
            cid_a,
            ClipContent::Image(ImageContent {
                events: vec![
                    ImageEvent {
                        source_id: img,
                        ..ImageEvent::default()
                    },
                    ImageEvent {
                        source_id: img,
                        ..ImageEvent::default()
                    },
                ],
            }),
        );
        let cid_b = song.alloc_content_id();
        song.clip_contents.insert(
            cid_b,
            ClipContent::Image(ImageContent {
                events: vec![ImageEvent {
                    source_id: img,
                    ..ImageEvent::default()
                }],
            }),
        );
        assert_eq!(song.image_source_refcount(img), 3);
    }

    #[test]
    fn gc_image_sources_drops_orphans() {
        let mut song = Song::default();
        let live_id = song.alloc_image_source_id();
        let orphan_id = song.alloc_image_source_id();
        song.image_sources.insert(
            live_id,
            ImageSource {
                path: ImageSourcePath::Absolute("/tmp/live.png".into()),
                width: 256,
                height: 256,
                format: "Png".into(),
            },
        );
        song.image_sources.insert(
            orphan_id,
            ImageSource {
                path: ImageSourcePath::Absolute("/tmp/orphan.png".into()),
                width: 256,
                height: 256,
                format: "Png".into(),
            },
        );
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Image(ImageContent {
                events: vec![ImageEvent {
                    source_id: live_id,
                    ..ImageEvent::default()
                }],
            }),
        );

        song.gc_image_sources();
        assert!(song.image_sources.contains_key(&live_id));
        assert!(!song.image_sources.contains_key(&orphan_id));
    }

    #[test]
    fn v12_forward_migrates_image_fields_to_default() {
        // v12 file (= no image_sources / next_image_source_id keys)
        // must deserialize cleanly into v13 Song with default-empty
        // image pool and next_id == 0.
        let v12_song_json = serde_json::json!({
            "bpm": 120.0,
            "time_sig": [4, 4],
            "length_beats": 64.0,
        });
        let song: Song = serde_json::from_value(v12_song_json).unwrap();
        assert!(song.image_sources.is_empty());
        assert_eq!(song.next_image_source_id, 0);
    }
}
