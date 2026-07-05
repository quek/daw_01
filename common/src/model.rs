use std::collections::HashMap;
use std::path::PathBuf;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::plugin_format::PluginFormat;
use crate::scale::ScaleChange;

/// `28` ビュー状態の保存: `ProjectFile.view: Option<ViewState>` 追加。
/// ズーム / スクロール / 行高 / スナップ設定等の表示状態を `Song` の **兄弟**として
/// 同梱する (= Song / IPC は無改変、`ViewState` は serde 専用で IPC を渡らない)。
/// piano roll / audio editor は per-clip (`ClipKey` keyed) 記憶。旧ファイルは
/// `#[serde(default)]` で `view == None` → 従来どおり fit-to-content にフォールバック。
///
/// `27` クリップ / ノートのミュート: `Clip.muted: bool` (= 全 content type
/// 共通の clip-level mute の SSoT) と `Note.muted: bool` (note 単位 mute) が追加される。
/// `true` の clip / note は再生 / 書き出しから除外され、GUI は dim + 斜線ハッチで表示する。
/// `q` ショートカットと inspector の "Mute" トグルが SSoT としてここを読み書きする。v26
/// 以前は per-event mute (`AudioEvent`/`ImageEvent`/`VideoEvent`/`TextEvent` の `muted`) で
/// clip mute を表現していたので、load 時に `project::migrate_per_event_mute_to_clip_mute` が
/// 「event が muted な clip」を `Clip.muted = true` へ畳み込み、event 側を false に戻す
/// (version-gate)。両 field とも `#[serde(default)]` で v26 以前は `false` に forward-migrate。
///
/// `26` 字幕デバイスゲート + VOICEVOX トーク (`docs/plan_voicevox_talk.md`):
/// `Clip.talk: Option<TalkParams>` 追加 (= 読み上げ全体スケール、`#[serde(default)]`)。
/// テキストオーバーレイ表示が `builtin.video.subtitle` device の有無で gate される
/// ようになり、v25 以前 (= 全トラック常時表示) の `.daw` は load 時に Text 持ち
/// トラックへ字幕デバイスを auto-insert して表示を保つ (`project::migrate_text_overlay_
/// to_subtitle_device`、version-gate)。新規 .daw は migration 対象外なので「喋るが
/// 映さない」(VOICEVOX device のみ) を表現できる。
///
/// `24` プロジェクト識別子 (`docs/plan_fixme_33_clipboard.md`):
/// `Song.project_id: u64` が追加される。New で 1 度採番し Save/Load で保持する
/// document 固有の安定 ID で、クリップボード round-trip 時に「同一プロジェクト由来か」を
/// 判定して clip/track paste のリンク共有 (同一) / 独立コピー (別) を分岐する。v23 `.daw`
/// files still load — `project_id` は `#[serde(default)]` で `0` になり、`Song::ensure_project_id`
/// (`normalize_after_load` 内) が load 時に採番する (`0` は未採番 sentinel)。
///
/// `23` 単一デバイスチェーン (`docs/plan_linear_chain.md`): 役割別 3 chain
/// (`Track.{instrument, midi_fx_chain, fx_chain}`) を 1 本の
/// `Track.devices: Vec<PluginInstance>` へ統合し、各 `PluginInstance` に
/// `ports: PortConfig` を持たせる (役割は保持せず ports から位置導出)。
/// `AutomationTarget::PluginParam { slot } → { device_index }`。v22 `.daw`
/// files still load — 旧 3 fields は deserialize-only (`rename` で旧 field 名を
/// 受ける) に降格し、load 時 (`Song::ensure_ids` → `Track::flatten_legacy_devices`)
/// に `midi_fx_chain ++ instrument? ++ fx_chain` の順で `devices` へ平坦化、
/// automation lane の旧 `slot` も同順序で `device_index` へ写像する。新規 save は
/// `devices` のみ (旧 fields は `skip_serializing`)。
///
/// `22` 画像 source の元ファイル名: `ImageSource.name: String` (import 元
/// ファイルの元名、 拡張子込み) が追加される。 on-disk `path` は content
/// addressing のため `<sanitized_stem>_<hash8>.<ext>` に sanitize / hash
/// され、 日本語名が `_` に潰れて inspector / 口パク mapping ドロップダウンで
/// 区別できなかったのを、 表示用 SSoT として別途保持する。 v21 `.daw` files
/// still load — `name` は `#[serde(default)]` で空文字になり、 consumer は
/// 空なら `path.file_name()` に fallback する。 See `docs/plan_image_overlay.md`.
///
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
/// `25` 映像 Transform device: 「動かす変形」をチェーン上の
/// `builtin.video.transform` 配置 device に一本化。値・automation・変調は既存
/// `GroupTransform` 系のまま (破壊的な値 migration 無し)。`ensure_ids` が旧
/// `group_transform` 持ちトラックに Transform device を補い (idempotent)、
/// `resolve_track_transform` が device-gate で配置を効かせる。v24 `.daw` files still
/// load — device 追加は additive で forward/backward compatible。See `docs/plan_video_fx.md` §5。
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
/// Bumped to `29` for stable-id addressing (`docs/plan_arch_refactor.md` §1):
/// `PluginInstance.id: u64` (Song-global `next_device_id` allocator)、
/// `Send.id: u32` (per-track `next_send_id`)、note / audio event / automation
/// point の要素 id (per-content allocator)。`AutomationTarget::PluginParam` /
/// `BindingTarget::PluginParam` は `device_index` → `device_id`、
/// `TrackBuiltinParam::SendGain` は `send_idx` → `send_id` に移行 — 旧 file の
/// positional 値は deserialize 専用 legacy field に載り、`Song::ensure_ids` が
/// id へ写像する。v28 以前の `.daw` はすべて load 可能。
pub const CURRENT_VERSION: u32 = 30;

/// Stable id for shared clip content (notes). Allocated by
/// `Song::alloc_content_id` and referenced by `Clip::content_id`.
/// `0` is the "未採番" sentinel — `Song::ensure_clip_contents` reassigns
/// any zero-valued `content_id` on load.
pub type ContentId = u32;

/// Serde adapter for `Option<Arc<[u8]>>` that writes binary data as base64 in
/// JSON (and other human-readable formats). Bincode bypasses this and uses
/// native length-prefixed bytes via the `Encode`/`Decode` derives.
///
/// D2 (r.md #8): bulk binary フィールド (plugin `state` / `ara_archive`) は
/// `Arc<[u8]>` で保持する。 これらは undo の編集対象ではない (= 同じ bytes が
/// 全 undo snapshot で共有可能) ので、 `push_undo_snapshot` の `Song::clone` が
/// MB 級の plugin/ARA データを毎回コピーする代わりに refcount bump で済む。
/// wire 形式は base64 文字列 (serde) / length-prefixed bytes (bincode の
/// `Arc<[u8]>` impl は内側 slice をそのまま符号化) のまま不変なので、 既存
/// プロジェクト / IPC との互換は保たれる。
pub mod base64_opt {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};
    use std::sync::Arc;

    pub fn serialize<S: Serializer>(
        bytes: &Option<Arc<[u8]>>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => ser.serialize_some(&STANDARD.encode(b.as_ref())),
            None => ser.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<Arc<[u8]>>, D::Error> {
        let s: Option<String> = Option::deserialize(de)?;
        match s {
            Some(s) => STANDARD
                .decode(s.as_bytes())
                .map(|v| Some(Arc::from(v)))
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    pub version: u32,
    pub song: Song,
    /// v28: GUI の表示状態 (ズーム / スクロール / 行高 / スナップ等)。
    /// `Song` の兄弟として同梱し、開き直しで「閉じたときの見た目」を復元する。
    /// `None` = 旧ファイル / view 未保存 → loader 側で fit-to-content にフォールバック。
    /// **serde 専用** (= `bincode::Encode/Decode` を付けない) で IPC を渡らないことを
    /// 型レベルで保証する (`ViewState` 参照)。
    #[serde(default)]
    pub view: Option<ViewState>,
}

/// piano roll 1 クリップ分の表示状態。`AppData.piano_roll_views`
/// (live SSoT) と `ViewState.piano_roll_views` (永続化) の両方で `ClipKey` 単位に
/// 保持する。`Default` は `AppData::new` / `fit_piano_roll_to_clip` の既定と一致。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PianoRollViewState {
    /// 横ズーム (px / beat)。clamp 8..=400。
    pub zoom_x: f32,
    /// 縦ズーム (px / semitone)。clamp 6..=40。
    pub zoom_y: f32,
    /// 表示上端のピッチ (MIDI note)。clamp 11..=127。
    pub top_pitch: u8,
    /// 横スクロール (clip-local beats、`>= 0`)。
    pub scroll_beat: f32,
}

impl Default for PianoRollViewState {
    fn default() -> Self {
        Self {
            zoom_x: 64.0,
            zoom_y: 14.0,
            top_pitch: 84, // C6
            scroll_beat: 0.0,
        }
    }
}

/// audio editor 1 クリップ分の表示状態 (clip-relative beats)。
/// `len_beats == 0.0` は「未設定」扱い (= クリップ全体表示にフォールバック)。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct AudioEditorViewState {
    /// 表示開始位置 (clip 始端からの offset、beats)。
    pub start_beat: f64,
    /// 表示 span (beats)。`0.0` = クリップ全体。
    pub len_beats: f64,
}

/// 再生中にアレンジビューがプレイヘッドを追従スクロールする方式 (Ableton の
/// Follow Behavior 相当)。`Alt+F` で `Off → Scroll → Page → Off` と循環し、
/// トランスポートのドロップダウンでも直接選べる。`AppData` (live SSoT) が保持し、
/// `ViewState` でプロジェクト単位に保存する (snap 設定と同じ idiom、IPC は渡らない)。
/// 再生中にユーザーが手動で横スクロール / ズームすると `Off` に落ちる
/// (ユーザー選択の挙動)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FollowMode {
    /// 追従しない。
    Off,
    /// 連続スクロール: プレイヘッドを画面中央に固定し、背景を滑らかに流す
    /// (Logic / Pro Tools 風、Ableton の "Scroll")。新規 / 旧 .daw の既定。
    #[default]
    Scroll,
    /// ページめくり: プレイヘッドが可視右端を越えたらビューを 1 ページ進め、
    /// プレイヘッドを左端から再び走らせる (Ableton の "Page")。
    Page,
}

/// プロジェクトに同梱する GUI 表示状態のスナップショット。
/// `AppData` (live SSoT) から save 時に `snapshot_view_state` で作り、load 時に
/// `restore_view_state` で流し込む。**serde 専用** (bincode derive 無し) ＝ IPC を渡らない。
/// `ClipKey` は struct で JSON の map key にできないため、per-clip view は
/// `Vec<(ClipKey, _)>` で持つ。各フィールドは `#[serde(default)]` で前方互換。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ViewState {
    // ---- Arrangement (タイムラインは曲に 1 つ = グローバル) ----
    #[serde(default)]
    pub arrange_zoom_x: f32,
    #[serde(default)]
    pub arrange_scroll_beat: f32,
    #[serde(default)]
    pub arrange_track_top: f32,
    #[serde(default)]
    pub arrange_track_row_h: f32,
    #[serde(default)]
    pub arrange_header_w: f32,
    /// per-track の行高 override (track_id → px)。
    #[serde(default)]
    pub track_row_overrides: HashMap<u32, u16>,
    /// automation lane を展開中の track_id (順序非依存、save 時に sort)。
    #[serde(default)]
    pub expanded_automation_tracks: Vec<u32>,
    #[serde(default)]
    pub master_row_automation_expanded: bool,
    /// 再生中プレイヘッド追従スクロールの方式 (Alt+F で循環)。旧 .daw は
    /// フィールド欠落 → `FollowMode::default()` (= Page) で読まれる。
    #[serde(default)]
    pub arrange_follow: FollowMode,
    // ---- Snap / grid / piano roll モード ----
    #[serde(default)]
    pub arrange_snap_enabled: bool,
    #[serde(default)]
    pub arrange_snap_choice: u8,
    #[serde(default)]
    pub pianoroll_snap_enabled: bool,
    #[serde(default)]
    pub pianoroll_snap_choice: u8,
    #[serde(default)]
    pub piano_roll_fold: bool,
    #[serde(default)]
    pub snap_on_draw: bool,
    #[serde(default)]
    pub snap_live_input: bool,
    // ---- 下部パネル ----
    #[serde(default)]
    pub bottom_panel: u8,
    // ---- クリップ選択 (= 開き直したとき「編集していたクリップ」を復元する) ----
    /// 選択 anchor (= ピアノロール / インスペクタが表示するクリップ)。
    #[serde(default)]
    pub selected_clip: Option<ClipKey>,
    /// 選択集合。
    #[serde(default)]
    pub selected_clips: Vec<ClipKey>,
    // ---- per-clip view (Ableton Live / Bitwig 流) ----
    #[serde(default)]
    pub piano_roll_views: Vec<(ClipKey, PianoRollViewState)>,
    #[serde(default)]
    pub audio_editor_views: Vec<(ClipKey, AudioEditorViewState)>,
}

/// Arranger セクション (曲のパート =
/// Intro / Aメロ / サビ …)。全トラックを縦断する時間レンジ + 名前 + 色で、`Song.sections`
/// に保持する。位置 (`start_beat`) が並び順の SSoT (別途 order index は持たない)。
/// `start_beat` 昇順・互いに非交差 (重複なし、隙間は許容) を `Song::normalize_sections`
/// で保つ。帯を動かす / 並べ替えると範囲内の全 clip + automation + tempo + 拍子 + key が
/// 一緒に動く破壊的アレンジャー (Studio One モデル) の位置メタデータ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Section {
    /// Song 内で安定な id (`Song::alloc_section_id` で採番、`0` は sentinel)。
    pub id: u32,
    /// 表示名 (Intro / Aメロ / サビ …)。自動命名 + 自由 rename。
    pub name: String,
    /// 帯の塗り色 (RGB、`0.0..=1.0`)。
    pub color: [f32; 3],
    /// 開始拍 (song-absolute)。
    pub start_beat: f64,
    /// 長さ (拍)。`end = start_beat + len_beats`。
    pub len_beats: f64,
}

impl Section {
    /// 終端拍 (= `start_beat + len_beats`)。
    pub fn end_beat(&self) -> f64 {
        self.start_beat + self.len_beats
    }
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
    /// v29: stable id allocator for `PluginInstance::id` (device 安定 id)。
    /// track devices と `master_fx_chain` が共有する Song-global 採番。`0` は
    /// "未採番" sentinel — `ensure_ids` が load 時に採番する。device の
    /// addressing (IPC / automation / MIDI binding / plugin host bookkeeping /
    /// shmem 名 / worker dispatch) はすべてこの id で行い、chain 内 index は
    /// 表示順序のみに使う (`docs/plan_arch_refactor.md` §1)。
    #[serde(default)]
    pub next_device_id: u64,
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
    /// master bus の audio fx chain。 通常 track の `Track.devices` と同 schema
    /// (= 同 `PluginInstance` を再利用)。 master は audio fx のみ持つ (= 音源境界
    /// なしの単一 Vec、 master bus に instrument / arpeggiator は無意味)。 automation の
    /// `song_lanes` と同じく「master 固有データは Track ではなく Song 直下に置く」
    /// 既存パターン (`automation_lane_by_key_mut` 参照) の踏襲。 audio engine は全
    /// track mix 後・metronome 前に `(MASTER_TRACK_ID, PluginSlot::Fx(i))` keying で
    /// 直列 process する。 旧 file は `#[serde(default)]` で空 Vec に forward-migrate。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub master_fx_chain: Vec<PluginInstance>,
    /// v24: プロジェクト固有の安定 ID。New で 1 度採番、Save/Load で保持。
    /// クリップボード round-trip で「同一プロジェクト由来か」を判定し、clip/track paste の
    /// リンク共有 (同一) / 独立コピー (別) を分岐する。`0` は未採番 sentinel —
    /// load 時に `0` なら `Song::ensure_project_id` が採番する (旧 file forward-migration)。
    #[serde(default)]
    pub project_id: u64,
    /// 曲のパートを表す Arranger セクション。
    /// `start_beat` 昇順・互いに非交差 (重複なし、隙間は許容) の invariant を
    /// `normalize_sections` で保つ。旧 file は空 Vec で forward-migrate。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<Section>,
    /// Stable id allocator for `Section`。 `0` は "未採番" sentinel、 `1` から採番。
    #[serde(default)]
    pub next_section_id: u32,
    /// docs/plan_modulation.md §1: 共有モジュレーション源 (sidechain +
    /// エンベロープフォロワー) の唯一の store。 `AuxInputRoute` / `ModRouting`
    /// から `ModSource.id` で参照される。 旧 file は空 Vec で forward-migrate。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mod_sources: Vec<ModSource>,
    /// Stable id allocator for `ModSource`。 `0` は "未採番" sentinel、 `1` から採番。
    #[serde(default)]
    pub next_mod_source_id: u32,
    /// **song-level lane 非依存モジュレーション** (`docs/plan_modulation_routing_redesign.md`
    /// §2): `SongTempo` / `SongTimeSigNumerator` 等の song-wide param を変調する
    /// `ModRouting`。track 内 param は `Track.mod_routings`、song-wide はこちら
    /// (`song_lanes` と同じ「master 固有データは Song 直下」流儀)。空 Vec で変調なし。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub song_mod_routings: Vec<ModRouting>,
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
            next_device_id: 1,
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
            project_id: 0,
            sections: Vec::new(),
            next_section_id: 1,
            mod_sources: Vec::new(),
            next_mod_source_id: 1,
            song_mod_routings: Vec::new(),
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

/// MIDI Learn の bind 先。 TrackVolume / TrackPan / SongTempo / PluginParam。
/// CC 受信時は `apply_midi_value_to_target` が各 target に値を反映する
/// (PluginParam は param range で value_real に変換し inspector knob と同じ
/// lane-default 経路で plugin host へ、 r.md #8 B2)。 transport の Learn button が
/// 「直近に触った param」 を bind する (touch + learn)。
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
    /// plugin parameter bind (r.md #8 B2)。 `track` + `device_index` で
    /// linear device chain 上の plugin instance を特定 (`AutomationTarget::
    /// PluginParam` と同じ addressing)、 `param_id` は format ごと
    /// (CLAP `clap_id` / VST3 `ParamID`)。 CC 受信時は `apply_midi_value_to_target`
    /// が param range で value_real に変換し、 inspector knob と同じ lane-default
    /// 経路で plugin host へ反映する。
    PluginParam {
        track: u32,
        /// v29: 安定 device id (`PluginInstance::id`)。`0` は未解決 sentinel。
        #[serde(default)]
        device_id: u64,
        param_id: u32,
        /// v28 以前 migration 用 (deserialize 専用)。旧 save は chain 内
        /// positional index を持つ。`Song::ensure_ids` が device_id へ写像。
        #[serde(default, rename = "device_index", skip_serializing)]
        legacy_device_index: Option<u32>,
        /// v22 以前 migration 用 (deserialize 専用)。 旧 save は `slot: PluginSlot`
        /// を持つ。 `Song::ensure_ids` が flatten 前に legacy_device_index へ写像する
        /// (r.md #8 M7: 旧 project の PluginParam binding があると device_index 欠落で
        /// project 全体の deserialize が失敗していたのを是正)。
        #[serde(default, rename = "slot", skip_serializing)]
        legacy_slot: Option<crate::protocol::PluginSlot>,
    },
}

impl Song {
    /// Allocate a new stable track id, bumping the song-level counter.
    pub fn alloc_track_id(&mut self) -> u32 {
        // `u32::MAX` is reserved as `MASTER_TRACK_ID`; clamp the usable
        // range to `[1, MASTER_TRACK_ID - 1]` so we never hand out the
        // sentinel, and `saturating_add` keeps the counter from wrapping
        // back to the `0` sentinel on exhaustion.
        let id = self.next_track_id.clamp(1, MASTER_TRACK_ID - 1);
        self.next_track_id = id.saturating_add(1);
        id
    }

    /// v29: 新規 device (`PluginInstance`) 用の Song-global 安定 id を採番
    /// する。 track devices / master_fx_chain 共用。
    pub fn alloc_device_id(&mut self) -> u64 {
        let id = self.next_device_id.max(1);
        self.next_device_id = id.saturating_add(1);
        id
    }

    /// Phase 5: allocate a new song-level automation lane id (`song_lanes`)。
    /// `next_song_lane_id` を bump して返す。
    pub fn alloc_song_lane_id(&mut self) -> u32 {
        let id = self.next_song_lane_id.max(1);
        self.next_song_lane_id = id.saturating_add(1);
        id
    }

    /// allocate a new stable `Section` id, bumping `next_section_id`。
    /// `0` は "未採番" sentinel なので最低 `1` から返す。
    pub fn alloc_section_id(&mut self) -> u32 {
        let id = self.next_section_id.max(1);
        self.next_section_id = id.saturating_add(1);
        id
    }

    /// docs/plan_modulation.md §1: allocate a new stable `ModSource` id,
    /// bumping `next_mod_source_id`。 `0` は "未採番" sentinel なので最低 `1` から返す。
    pub fn alloc_mod_source_id(&mut self) -> u32 {
        let id = self.next_mod_source_id.max(1);
        self.next_mod_source_id = id.saturating_add(1);
        id
    }

    /// `sections` の invariant を回復する: `start_beat` 昇順、互いに非交差
    /// (重複なし、隙間は許容)、`len_beats > 0`。セクションを追加 / 移動 / リサイズした
    /// あとに呼ぶ。重複は「先に始まる方を優先」 (= 後発の `start_beat` を直前 section の
    /// `end_beat` までクランプして隙間化) して解消し、長さが `0` 以下になった section は
    /// 破棄する。idempotent。
    pub fn normalize_sections(&mut self) {
        self.sections.sort_by(|a, b| {
            a.start_beat
                .partial_cmp(&b.start_beat)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut prev_end = f64::NEG_INFINITY;
        for s in &mut self.sections {
            if s.start_beat < prev_end {
                let end = s.end_beat();
                s.start_beat = prev_end;
                s.len_beats = (end - prev_end).max(0.0);
            }
            prev_end = s.end_beat();
        }
        self.sections.retain(|s| s.len_beats > f64::EPSILON);
    }

    /// タイムライン全体の ripple シフト。
    /// `from_beat` 以降の全ての時間位置を `delta` だけずらす (結果は `0.0` 以上に clamp)。
    /// 破壊的セクション移動の close (`delta < 0` = 範囲を詰める) / open (`delta > 0` =
    /// 範囲を空ける) プリミティブ。 対象は全トラックの clip 位置、各トラックと `song_lanes`
    /// の automation clip 位置、`scale_changes`、`sections`、loop 範囲、`length_beats`。
    /// clip 内の note / event / point は clip-local なので動かさない (clip 位置だけずらせば
    /// 中身は付いてくる = 歌声キャッシュ key 不変、再合成不要)。 シフト後に scale / sections
    /// の invariant を復元する。
    pub fn ripple_timeline(&mut self, from_beat: f64, delta: f64) {
        fn shift(b: &mut f64, from: f64, delta: f64) {
            if *b >= from {
                *b = (*b + delta).max(0.0);
            }
        }
        for t in &mut self.tracks {
            for c in &mut t.clips {
                shift(&mut c.start_beat, from_beat, delta);
            }
            for lane in &mut t.automation_lanes {
                for c in &mut lane.clips {
                    shift(&mut c.start_beat, from_beat, delta);
                }
            }
        }
        for lane in &mut self.song_lanes {
            for c in &mut lane.clips {
                shift(&mut c.start_beat, from_beat, delta);
            }
        }
        for sc in &mut self.scale_changes {
            shift(&mut sc.beat, from_beat, delta);
        }
        for s in &mut self.sections {
            shift(&mut s.start_beat, from_beat, delta);
        }
        shift(&mut self.loop_start_beat, from_beat, delta);
        shift(&mut self.loop_end_beat, from_beat, delta);
        if self.length_beats >= from_beat {
            self.length_beats = (self.length_beats + delta).max(0.0);
        }
        self.ensure_scale_changes_sorted();
        self.normalize_sections();
    }

    /// セクション帯を `dest_start` へ
    /// 破壊的に移動し、 曲構成を組み替える (Studio One 流の能動アレンジャー)。 帯の範囲
    /// `[a, b)` 内の全トラック clip + automation + `song_lanes` automation + `scale_changes`
    /// を帯と一緒に取り出し、 `[a,b)` を ripple-close で詰め、 `dest_start` に ripple-open で
    /// 空けて落とし直す。 他セクション / 他 clip は ripple で前後に流れる。 戻り値は移動が
    /// 起きたか。
    ///
    /// 境界をまたぐ clip は移動前に `split_clips_at(a)` / `split_clips_at(b)` で分割するので、
    /// 帯範囲ぴったりの content だけが追従する (Studio One の split-at-boundary)。 残りの
    /// content / 他セクションは ripple で前後に流れる。
    pub fn move_section(&mut self, section_id: u32, dest_start: f64) -> bool {
        let Some(sec) = self.sections.iter().find(|s| s.id == section_id).cloned() else {
            return false;
        };
        let (a, len) = (sec.start_beat, sec.len_beats);
        let b = a + len;
        let dest_start = dest_start.max(0.0);
        if len <= 0.0 || (dest_start - a).abs() < f64::EPSILON {
            return false;
        }
        let in_range = |start: f64| start >= a && start < b;

        // 0. 境界をまたぐ clip を a / b で分割し、 以降の membership 抽出を正確にする。
        self.split_clips_at(a);
        self.split_clips_at(b);

        // 1. 範囲内の content を取り出し、 帯先頭 (a) 基準のローカル位置に正規化。
        let mut taken_clips: Vec<(u32, Clip)> = Vec::new();
        for t in &mut self.tracks {
            let mut i = 0;
            while i < t.clips.len() {
                if in_range(t.clips[i].start_beat) {
                    let mut c = t.clips.remove(i);
                    c.start_beat -= a;
                    taken_clips.push((t.id, c));
                } else {
                    i += 1;
                }
            }
        }
        let mut taken_auto: Vec<(u32, u32, AutomationClip)> = Vec::new();
        for t in &mut self.tracks {
            let tid = t.id;
            for lane in &mut t.automation_lanes {
                let lid = lane.id;
                let mut i = 0;
                while i < lane.clips.len() {
                    if in_range(lane.clips[i].start_beat) {
                        let mut c = lane.clips.remove(i);
                        c.start_beat -= a;
                        taken_auto.push((tid, lid, c));
                    } else {
                        i += 1;
                    }
                }
            }
        }
        let mut taken_song_auto: Vec<(u32, AutomationClip)> = Vec::new();
        for lane in &mut self.song_lanes {
            let lid = lane.id;
            let mut i = 0;
            while i < lane.clips.len() {
                if in_range(lane.clips[i].start_beat) {
                    let mut c = lane.clips.remove(i);
                    c.start_beat -= a;
                    taken_song_auto.push((lid, c));
                } else {
                    i += 1;
                }
            }
        }
        let mut taken_scales: Vec<ScaleChange> = Vec::new();
        self.scale_changes.retain(|sc| {
            if in_range(sc.beat) {
                let mut s = *sc;
                s.beat -= a;
                taken_scales.push(s);
                false
            } else {
                true
            }
        });
        // 帯自身も取り出す (ripple では動かさず、 後で dest に置き直す)。
        self.sections.retain(|s| s.id != section_id);

        // 2. `[a,b)` を詰める (close)。
        self.ripple_timeline(b, -len);
        // 3. 詰めた後の座標系での落とし先。
        let dest2 = if dest_start >= b {
            dest_start - len
        } else if dest_start <= a {
            dest_start
        } else {
            a
        };
        // 4. 落とし先に `len` ぶん空ける (open)。
        self.ripple_timeline(dest2, len);

        // 5. 取り出した content を `dest2` 基準で戻す。
        for (tid, mut c) in taken_clips {
            c.start_beat += dest2;
            if let Some(t) = self.tracks.iter_mut().find(|t| t.id == tid) {
                t.clips.push(c);
            }
        }
        for (tid, lid, mut c) in taken_auto {
            c.start_beat += dest2;
            if let Some(l) = self
                .tracks
                .iter_mut()
                .find(|t| t.id == tid)
                .and_then(|t| t.automation_lanes.iter_mut().find(|l| l.id == lid))
            {
                l.clips.push(c);
            }
        }
        for (lid, mut c) in taken_song_auto {
            c.start_beat += dest2;
            if let Some(l) = self.song_lanes.iter_mut().find(|l| l.id == lid) {
                l.clips.push(c);
            }
        }
        for mut s in taken_scales {
            s.beat += dest2;
            self.scale_changes.push(s);
        }
        // 6. 帯を dest2 に置き直す。
        self.sections.push(Section {
            id: sec.id,
            name: sec.name,
            color: sec.color,
            start_beat: dest2,
            len_beats: len,
        });

        self.ensure_scale_changes_sorted();
        self.ensure_automation_points_sorted();
        self.normalize_sections();
        true
    }

    /// セクション帯を `dest_start` に複製
    /// 挿入する (Ctrl+drag、 ripple-insert)。 範囲 `[a,b)` 内の clip / automation を **linked**
    /// (= `content_id` 共有、 REAPER pooled idiom) で複製し、 clip id だけ新規採番。 `dest_start`
    /// 以降を `len` ぶん右へ ripple して空けてから複製を落とす。 元の content は残す。 新しい
    /// セクション id を採番して返す (`Some(new_id)`)。 `move_section` / `delete_section_range`
    /// と同じく境界 `a` / `b` で `split_clips_at` してから `start_beat ∈ [a,b)` membership で複製する
    /// ので、 境界をまたぐ clip も範囲内ぶんだけ正しく複製される。
    pub fn duplicate_section(&mut self, section_id: u32, dest_start: f64) -> Option<u32> {
        let sec = self.sections.iter().find(|s| s.id == section_id).cloned()?;
        let (a, len) = (sec.start_beat, sec.len_beats);
        let b = a + len;
        let dest_start = dest_start.max(0.0);
        if len <= 0.0 {
            return None;
        }
        let in_range = |start: f64| start >= a && start < b;

        // 0. 境界をまたぐ clip を a / b で分割 (move / delete-range と同じ split-at-boundary)。
        //    これをしないと境界跨ぎ clip が membership から漏れ、 複製で境界の音が欠落する。
        self.split_clips_at(a);
        self.split_clips_at(b);

        // 1. 範囲内 content の複製 (linked: content_id 共有、 clip id 新規採番、 a 基準ローカル)。
        let mut copies_clips: Vec<(u32, Clip)> = Vec::new();
        for t in &mut self.tracks {
            let tid = t.id;
            let srcs: Vec<Clip> = t.clips.iter().filter(|c| in_range(c.start_beat)).cloned().collect();
            for mut c in srcs {
                let id = t.next_clip_id.max(1);
                t.next_clip_id = id + 1;
                c.id = id;
                c.start_beat -= a;
                copies_clips.push((tid, c));
            }
        }
        let mut copies_auto: Vec<(u32, u32, AutomationClip)> = Vec::new();
        for t in &mut self.tracks {
            let tid = t.id;
            for lane in &mut t.automation_lanes {
                let lid = lane.id;
                let srcs: Vec<AutomationClip> =
                    lane.clips.iter().filter(|c| in_range(c.start_beat)).cloned().collect();
                for mut c in srcs {
                    let id = lane.next_clip_id.max(1);
                    lane.next_clip_id = id + 1;
                    c.id = id;
                    c.start_beat -= a;
                    copies_auto.push((tid, lid, c));
                }
            }
        }
        let mut copies_song_auto: Vec<(u32, AutomationClip)> = Vec::new();
        for lane in &mut self.song_lanes {
            let lid = lane.id;
            let srcs: Vec<AutomationClip> =
                lane.clips.iter().filter(|c| in_range(c.start_beat)).cloned().collect();
            for mut c in srcs {
                let id = lane.next_clip_id.max(1);
                lane.next_clip_id = id + 1;
                c.id = id;
                c.start_beat -= a;
                copies_song_auto.push((lid, c));
            }
        }
        let mut copies_scales: Vec<ScaleChange> = self
            .scale_changes
            .iter()
            .filter(|sc| in_range(sc.beat))
            .map(|sc| {
                let mut s = *sc;
                s.beat -= a;
                s
            })
            .collect();

        // 2. dest に len ぶん空ける (insert)。
        self.ripple_timeline(dest_start, len);

        // 3. 複製を dest_start 基準で挿入。
        for (tid, mut c) in copies_clips {
            c.start_beat += dest_start;
            if let Some(t) = self.tracks.iter_mut().find(|t| t.id == tid) {
                t.clips.push(c);
            }
        }
        for (tid, lid, mut c) in copies_auto {
            c.start_beat += dest_start;
            if let Some(l) = self
                .tracks
                .iter_mut()
                .find(|t| t.id == tid)
                .and_then(|t| t.automation_lanes.iter_mut().find(|l| l.id == lid))
            {
                l.clips.push(c);
            }
        }
        for (lid, mut c) in copies_song_auto {
            c.start_beat += dest_start;
            if let Some(l) = self.song_lanes.iter_mut().find(|l| l.id == lid) {
                l.clips.push(c);
            }
        }
        for sc in &mut copies_scales {
            sc.beat += dest_start;
        }
        self.scale_changes.append(&mut copies_scales);

        // 4. 新セクションを採番して挿入。
        let new_id = self.alloc_section_id();
        self.sections.push(Section {
            id: new_id,
            name: sec.name,
            color: sec.color,
            start_beat: dest_start,
            len_beats: len,
        });

        self.ensure_scale_changes_sorted();
        self.ensure_automation_points_sorted();
        self.normalize_sections();
        Some(new_id)
    }

    /// セクション帯だけ削除する (内容は温存、 Studio One の Backspace 相当)。
    /// 削除できたら `true`。
    pub fn delete_section(&mut self, section_id: u32) -> bool {
        let before = self.sections.len();
        self.sections.retain(|s| s.id != section_id);
        self.sections.len() != before
    }

    /// セクションの**時間範囲ごと**削除して
    /// 詰める (Studio One の "Delete Range" 相当、 破壊的)。 境界を分割してから範囲内の全
    /// content を消し、 `[a,b)` を ripple-close で詰める。 削除できたら `true`。
    pub fn delete_section_range(&mut self, section_id: u32) -> bool {
        let Some(sec) = self.sections.iter().find(|s| s.id == section_id).cloned() else {
            return false;
        };
        let (a, len) = (sec.start_beat, sec.len_beats);
        let b = a + len;
        if len <= 0.0 {
            return false;
        }
        self.split_clips_at(a);
        self.split_clips_at(b);
        let in_range = |s: f64| s >= a && s < b;
        for t in &mut self.tracks {
            t.clips.retain(|c| !in_range(c.start_beat));
            for lane in &mut t.automation_lanes {
                lane.clips.retain(|c| !in_range(c.start_beat));
            }
        }
        for lane in &mut self.song_lanes {
            lane.clips.retain(|c| !in_range(c.start_beat));
        }
        self.scale_changes.retain(|sc| !in_range(sc.beat));
        self.sections.retain(|s| s.id != section_id);
        self.ripple_timeline(b, -len);
        self.ensure_scale_changes_sorted();
        self.normalize_sections();
        true
    }

    /// 全トラック clip / track automation clip /
    /// `song_lanes` clip のうち `beat` を**厳密にまたぐ** (`start < beat < start+len`) ものを
    /// 2 つに分割する。 左 clip は元 `content_id` を保持して長さを `beat` まで詰め (= content の
    /// 先頭部分のみ再生)、 右 clip は `cut = beat - start` ぶん左シフトした **fork content** を
    /// 新規採番して持つ (元 content は pooled で他 clip が共有するため不変)。 セクション移動の
    /// 前にこれを境界 `a` / `b` で呼ぶと、 以降の「`start_beat ∈ [a,b)`」 membership 抽出が
    /// 境界跨ぎ clip でも正確になる。 歌声 clip も MIDI として分割され、 右断片は note 集合が
    /// 変わるのでキャッシュ key が変化し自動で再合成される。
    pub fn split_clips_at(&mut self, beat: f64) {
        for ti in 0..self.tracks.len() {
            let mut i = 0;
            while i < self.tracks[ti].clips.len() {
                let (start, len, cid) = {
                    let c = &self.tracks[ti].clips[i];
                    (c.start_beat, c.length_beats, c.content_id)
                };
                if start < beat && beat < start + len {
                    let cut = beat - start;
                    let right_cid = self.fork_content_shifted_left(cid, cut);
                    let right_id = self.tracks[ti].alloc_clip_id();
                    let mut right = self.tracks[ti].clips[i].clone();
                    right.id = right_id;
                    right.content_id = right_cid;
                    right.start_beat = beat;
                    right.length_beats = len - cut;
                    self.tracks[ti].clips[i].length_beats = cut;
                    self.tracks[ti].clips.insert(i + 1, right);
                    i += 2;
                } else {
                    i += 1;
                }
            }
            for li in 0..self.tracks[ti].automation_lanes.len() {
                let mut j = 0;
                while j < self.tracks[ti].automation_lanes[li].clips.len() {
                    let (start, len, cid) = {
                        let c = &self.tracks[ti].automation_lanes[li].clips[j];
                        (c.start_beat, c.length_beats, c.content_id)
                    };
                    if start < beat && beat < start + len {
                        let cut = beat - start;
                        let right_cid = self.fork_content_shifted_left(cid, cut);
                        let lane = &mut self.tracks[ti].automation_lanes[li];
                        let right_id = lane.next_clip_id.max(1);
                        lane.next_clip_id = right_id + 1;
                        let mut right = lane.clips[j].clone();
                        right.id = right_id;
                        right.content_id = right_cid;
                        right.start_beat = beat;
                        right.length_beats = len - cut;
                        lane.clips[j].length_beats = cut;
                        lane.clips.insert(j + 1, right);
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
            }
        }
        for li in 0..self.song_lanes.len() {
            let mut j = 0;
            while j < self.song_lanes[li].clips.len() {
                let (start, len, cid) = {
                    let c = &self.song_lanes[li].clips[j];
                    (c.start_beat, c.length_beats, c.content_id)
                };
                if start < beat && beat < start + len {
                    let cut = beat - start;
                    let right_cid = self.fork_content_shifted_left(cid, cut);
                    let lane = &mut self.song_lanes[li];
                    let right_id = lane.next_clip_id.max(1);
                    lane.next_clip_id = right_id + 1;
                    let mut right = lane.clips[j].clone();
                    right.id = right_id;
                    right.content_id = right_cid;
                    right.start_beat = beat;
                    right.length_beats = len - cut;
                    lane.clips[j].length_beats = cut;
                    lane.clips.insert(j + 1, right);
                    j += 2;
                } else {
                    j += 1;
                }
            }
        }
    }

    /// `content_id` の content を clip-local で `cut` 左シフトした新 content を
    /// 採番して返す (clip 分割の右側用)。 元 content は不変 (pooled 共有のため)。 linked clip
    /// 名も引き継ぐ。
    fn fork_content_shifted_left(&mut self, content_id: ContentId, cut: f64) -> ContentId {
        let mut content = self.clip_contents.get(&content_id).cloned().unwrap_or_default();
        Self::shift_content_left(&mut content, cut);
        let new_id = self.alloc_content_id();
        self.clip_contents.insert(new_id, content);
        if let Some(name) = self.clip_content_names.get(&content_id).cloned() {
            self.clip_content_names.insert(new_id, name);
        }
        new_id
    }

    /// clip content を clip-local で `cut` 拍ぶん左へずらす。 `cut` より前で完全に
    /// 終わる event/note/point は落とし、 `cut` をまたぐものは先頭 `0` にクランプして長さを
    /// 詰める。 audio は trim 分を `source_start_frames` に按分換算して進める (非ストレッチ前提の
    /// 線形近似)。 automation は `cut` 前の point を落とす (境界値の補間 point 挿入は次段)。
    fn shift_content_left(content: &mut ClipContent, cut: f64) {
        macro_rules! shift_overlay {
            ($events:expr) => {{
                $events.retain_mut(|ev| {
                    let new_end = (ev.event_start_in_clip_beats + ev.event_length_beats) - cut;
                    if new_end <= 0.0 {
                        return false;
                    }
                    let new_start = (ev.event_start_in_clip_beats - cut).max(0.0);
                    ev.event_start_in_clip_beats = new_start;
                    ev.event_length_beats = new_end - new_start;
                    true
                });
            }};
        }
        match content {
            ClipContent::Midi(m) => {
                m.notes.retain_mut(|n| {
                    let new_end = (n.start_beat + n.duration_beats) - cut;
                    if new_end <= 0.0 {
                        return false;
                    }
                    let new_start = (n.start_beat - cut).max(0.0);
                    n.start_beat = new_start;
                    n.duration_beats = new_end - new_start;
                    true
                });
            }
            ClipContent::Audio(a) => {
                a.events.retain_mut(|ev| {
                    let new_end = (ev.event_start_in_clip_beats + ev.event_length_beats) - cut;
                    if new_end <= 0.0 {
                        return false;
                    }
                    let new_start = ev.event_start_in_clip_beats - cut;
                    if new_start < 0.0 {
                        let trimmed = -new_start;
                        let total = ev.event_length_beats.max(f64::EPSILON);
                        let frames = ev.source_end_frames.saturating_sub(ev.source_start_frames);
                        let adv = (trimmed / total * frames as f64) as u64;
                        ev.source_start_frames = ev.source_start_frames.saturating_add(adv);
                        ev.event_start_in_clip_beats = 0.0;
                        ev.event_length_beats = new_end;
                    } else {
                        ev.event_start_in_clip_beats = new_start;
                    }
                    true
                });
            }
            ClipContent::Automation(a) => {
                for p in &mut a.points {
                    p.time_beat -= cut;
                }
                a.points.retain(|p| p.time_beat >= 0.0);
            }
            ClipContent::Video(c) => shift_overlay!(c.events),
            ClipContent::Image(c) => shift_overlay!(c.events),
            ClipContent::Text(c) => shift_overlay!(c.events),
        }
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

    /// Re-establish the time-ascending sort invariant on every automation
    /// curve. `automation::evaluate_clip` binary-searches `points` assuming
    /// `time_beat` ascending; a hand-edited / corrupt `.daw` whose order is
    /// scrambled would otherwise return silently wrong values (which the
    /// audio thread reads via `lane_value_at`). Idempotent.
    pub fn ensure_automation_points_sorted(&mut self) {
        for content in self.clip_contents.values_mut() {
            if let ClipContent::Automation(a) = content {
                a.points.sort_by(|x, y| {
                    x.time_beat
                        .partial_cmp(&y.time_beat)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }
    }

    /// Clamp persisted scalar fields into valid ranges. The persistence
    /// layer is the trust boundary where data crosses back in from disk /
    /// IPC, so this is the single place that defends every downstream
    /// divisor (`samples_per_beat = sr*60/bpm`, `tsig_denom`, ...) against
    /// `0` / negative / `NaN` values from a corrupt or hand-edited file.
    /// `NaN` slips past naive `x <= 0.0` guards (`NaN <= 0.0` is `false`),
    /// so every check is written as `!is_finite() || out_of_range`.
    /// Idempotent.
    pub fn sanitize_ranges(&mut self) {
        if !self.bpm.is_finite() {
            self.bpm = 120.0;
        } else {
            self.bpm = self.bpm.clamp(1.0, 1000.0);
        }
        // Numerator 1..=32, denominator must be a power-of-two beat unit.
        self.time_sig.0 = self.time_sig.0.clamp(1, 32);
        if !matches!(self.time_sig.1, 1 | 2 | 4 | 8 | 16) {
            self.time_sig.1 = 4;
        }
        for v in [&mut self.length_beats, &mut self.loop_start_beat, &mut self.loop_end_beat] {
            if !(v.is_finite() && *v >= 0.0) {
                *v = 0.0;
            }
        }
        if !(self.video_framerate.is_finite() && self.video_framerate > 0.0) {
            self.video_framerate = default_video_framerate();
        }
        if self.video_resolution.0 == 0 || self.video_resolution.1 == 0 {
            self.video_resolution = default_video_resolution();
        }
    }

    /// Single entry point for all pre-save normalization. GC orphan
    /// content / source-pool entries so the on-disk file stays tidy and
    /// every persisted `content_id` / source id is still referenced. Call
    /// on a clone from `project::save` (does not mutate the live song).
    pub fn normalize_for_save(&mut self) {
        self.gc_clip_contents();
        self.gc_audio_sources();
        self.gc_video_sources();
        self.gc_image_sources();
    }

    /// Single entry point for all post-load normalization. Re-establishes
    /// every invariant the rest of the codebase assumes about a freshly
    /// loaded song — value-range sanity, content / source migration, stable
    /// ids, and sort order — so `project::load`'s return value is always
    /// self-consistent regardless of how the file was produced. Idempotent.
    /// v24: `project_id == 0` (未採番 / 旧 file / `Song::default`) なら
    /// 新規採番する。既に非 0 なら触らない (idempotent) ので、New で採番済みの song に
    /// `normalize_after_load` を再走させても上書きしない。uuid v4 の下位 64bit を使う
    /// (別起動・別マシンでも衝突しない。`0` sentinel は引き直す)。
    pub fn ensure_project_id(&mut self) {
        if self.project_id == 0 {
            self.project_id = uuid::Uuid::new_v4().as_u128() as u64;
            if self.project_id == 0 {
                self.project_id = 1;
            }
        }
    }

    pub fn normalize_after_load(&mut self) {
        self.ensure_project_id();
        self.sanitize_ranges();
        self.ensure_clip_contents();
        self.ensure_audio_source_ids();
        self.ensure_video_source_ids();
        self.ensure_image_source_ids();
        self.ensure_ids();
        self.ensure_scale_changes_sorted();
        self.ensure_automation_points_sorted();
        self.ensure_overlay_event_coverage();
    }

    /// overlay clip (image / video / text) は「clip 長 = 表示長」が
    /// 不変条件。 単一 (または末尾) の event がその clip 長に届かないと、 clip
    /// 範囲内でも event 範囲を抜けて途中で消える (= clip を伸ばしたが event が
    /// 追従していない既存 .daw を自動修復する)。
    ///
    /// 各 content を、 それを参照する **最長** clip の長さまで届くよう
    /// extend-only で覆う ([`ClipContent::ensure_event_covers_clip`])。 linked
    /// clip でより短い clip があっても、 その clip は自分の clip 範囲 gate で
    /// clamp されるので安全。 idempotent。 Audio / Midi / Automation は no-op。
    pub fn ensure_overlay_event_coverage(&mut self) {
        // content ごとに、 それを参照する clip 長の最大値を集める。
        let mut max_len: HashMap<ContentId, f64> = HashMap::new();
        for track in &self.tracks {
            for clip in &track.clips {
                let e = max_len.entry(clip.content_id).or_insert(0.0);
                if clip.length_beats > *e {
                    *e = clip.length_beats;
                }
            }
        }
        for (cid, len) in max_len {
            if let Some(content) = self.clip_contents.get_mut(&cid) {
                content.ensure_event_covers_clip(len);
            }
        }
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

    /// track と master row を統一的に走査する device chain accessor。
    /// `track_id == MASTER_TRACK_ID` なら `master_fx_chain` を、 そうでなければ
    /// 該当 track の単一 `devices` chain を引く。 `automation_lane_by_key` と同
    /// idiom (master 固有データは Song 直下、 sentinel 分岐で透過アクセス)。
    /// plugin install / Inspector / chain 操作 handler から呼ぶ。
    ///
    /// v23: 非 master track は役割別 3 chain を `devices` に統合済みなので、
    /// 旧 `fx_chain` ではなく chain 全体 (`devices`) を返す。master_fx_chain は
    /// もともと単一 Vec (= 音源境界なしの全 audio FX) なのでそのまま。
    pub fn fx_chain_by_track_id(&self, track_id: u32) -> Option<&[PluginInstance]> {
        if track_id == MASTER_TRACK_ID {
            Some(&self.master_fx_chain)
        } else {
            self.track_by_id(track_id).map(|t| t.devices.as_slice())
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
            self.track_by_id_mut(track_id).map(|t| &mut t.devices)
        }
    }

    /// Re-assign stable ids to all tracks / clips after loading an older
    /// project file (or any save predating the id schema). Idempotent:
    /// records that already have non-zero ids are left untouched, and
    /// `next_*_id` counters are bumped above the highest seen id.
    ///
    /// PR4.5 sidechain regression fix: when a track's id changes here,
    /// every reference to the old id (= other tracks' `parent_group_id`
    /// and per-plugin `aux_inputs` tap sources) is remapped to the new
    /// id. Without this remap, a saved project that used `id == 0` as a
    /// sentinel for the first track would, on load, lose all its sidechain
    /// wiring (the references would dangle, `compile_schedule` silently
    /// skips dangling refs, and the user sees no sidechain signal).
    /// v29 migration: `AutomationTarget` 内の旧 positional 参照
    /// (`legacy_device_index` / `legacy_send_idx`) を安定 id (`device_id` /
    /// `send_id`) へ写像する。 新形式 (legacy = None) は no-op、 範囲外
    /// index は sentinel (0) のまま残す (= 「解決不能な参照」 として
    /// 消費側が無視できる)。
    fn remap_target_ids(target: &mut AutomationTarget, device_ids: &[u64], send_ids: &[u32]) {
        match target {
            AutomationTarget::PluginParam {
                device_id,
                legacy_device_index,
                ..
            } => {
                if let Some(idx) = legacy_device_index.take()
                    && *device_id == 0
                    && let Some(&id) = device_ids.get(idx as usize)
                {
                    *device_id = id;
                }
            }
            AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                send_id,
                legacy_send_idx,
            }) => {
                if let Some(idx) = legacy_send_idx.take()
                    && *send_id == 0
                    && let Some(&id) = send_ids.get(idx as usize)
                {
                    *send_id = id;
                }
            }
            _ => {}
        }
    }

    pub fn ensure_ids(&mut self) {
        // v23 migration: 旧 3-split (midi_fx_chain / instrument / fx_chain) を
        // 単一 `devices` へ平坦化し、automation lane の旧 slot を device_index へ
        // 写像する。新形式 (devices 既存) は no-op。pass 1/2 の前に実行する必要が
        // ある (pass 2 の sidechain remap は `devices` を走査するため)。
        //
        // M7 (r.md #8): BindingTarget::PluginParam の旧 `slot` を device_index へ
        // 写像する。 flatten が legacy 3-split を消費する **前** に行う (automation
        // lane の legacy_slot は flatten 内で処理される)。 新形式 (legacy_slot=None)
        // は no-op。 旧 project の PluginParam MIDI binding が device_index 欠落で
        // deserialize 全体を落としていたのを是正。
        for binding in &mut self.midi_bindings {
            let BindingTarget::PluginParam {
                track,
                legacy_device_index,
                legacy_slot,
                ..
            } = &mut binding.target
            else {
                continue;
            };
            let Some(slot) = legacy_slot.take() else {
                continue;
            };
            if let Some(t) = self.tracks.iter().find(|t| t.id == *track) {
                use crate::protocol::PluginSlot;
                let n_midi = t.legacy_midi_fx_chain.len() as u32;
                let has_inst = t.legacy_instrument.is_some() as u32;
                // v29: index はまだ positional。 後段の remap pass が
                // device_id へ写像する。
                *legacy_device_index = Some(match slot {
                    PluginSlot::MidiFx(i) => i,
                    PluginSlot::Instrument => n_midi,
                    PluginSlot::Fx(i) => n_midi + has_inst + i,
                });
            }
        }
        for track in &mut self.tracks {
            track.flatten_legacy_devices();
        }

        // docs/plan_modulation.md §8: 旧 `sidechain_sources` を `aux_inputs` へ
        // lift する。 device は flatten 済みなので全 PluginInstance を走査する。
        // id_remap guard より前 (= sentinel track の有無に関わらず) 必ず走る。
        for track in &mut self.tracks {
            for p in track.devices.iter_mut() {
                p.migrate_legacy_aux();
            }
        }
        for p in self.master_fx_chain.iter_mut() {
            p.migrate_legacy_aux();
        }

        // (v25): 旧 `group_transform` を持つトラックにチェーン上の
        // Transform 配置 device を補う。これで「動かす変形」がチェーンの 1 device
        // として現れ、`resolve_track_transform` の device-gate で効く（device を抜けば
        // 変換が無効）。値・automation・変調は GroupTransform 系のまま（破壊的な値
        // migration は不要）。idempotent（device 既存 / group_transform 無しは no-op）。
        for track in &mut self.tracks {
            let has_transform = track
                .devices
                .iter()
                .any(|d| d.plugin_id == crate::video_fx::TRANSFORM_ID);
            if track.group_transform.is_some() && !has_transform {
                track.devices.push(PluginInstance::with_ports(
                    crate::video_fx::TRANSFORM_ID.to_string(),
                    crate::plugin_format::PluginFormat::Builtin,
                    crate::port_config::PortConfig {
                        has_video_input: true,
                        has_video_output: true,
                        ..Default::default()
                    },
                ));
            }
        }

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

        // Phase 5: song-level lane の id も同様に採番。 sentinel (0) のみ
        // 上書き、 既存非 0 id は触らず counter を bump するだけ。
        // (review) id_remap guard より **前** に置く — sentinel track が無い通常
        // ロードでも lane / mod_source の採番・counter 正規化は必要。
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

        // docs/plan_modulation.md §8: mod_source id も song_lanes と同様に採番。
        // sentinel (0) のみ上書き、 既存非 0 id は触らず counter を bump する。
        for ms in &mut self.mod_sources {
            if ms.id == 0 {
                let new_id = self.next_mod_source_id.max(1);
                self.next_mod_source_id = new_id + 1;
                ms.id = new_id;
            } else if ms.id >= self.next_mod_source_id {
                self.next_mod_source_id = ms.id + 1;
            }
        }
        if self.next_mod_source_id == 0 {
            self.next_mod_source_id = 1;
        }

        // v29: device 安定 id (`PluginInstance::id`) を採番する。 track devices
        // と master_fx_chain が Song-global の `next_device_id` を共有。
        // sentinel (0) のみ上書き、 既存非 0 id は counter を bump するだけ
        // (他 allocator と同 idiom)。
        {
            fn alloc_dev(p: &mut PluginInstance, next: &mut u64) {
                if p.id == 0 {
                    let new_id = (*next).max(1);
                    *next = new_id + 1;
                    p.id = new_id;
                } else if p.id >= *next {
                    *next = p.id + 1;
                }
            }
            let mut next = self.next_device_id;
            for track in &mut self.tracks {
                for p in track.devices.iter_mut() {
                    alloc_dev(p, &mut next);
                }
            }
            for p in self.master_fx_chain.iter_mut() {
                alloc_dev(p, &mut next);
            }
            self.next_device_id = next.max(1);
        }

        // v29: send 安定 id (`Send::id`) を per-track 採番する。
        for track in &mut self.tracks {
            track.ensure_send_ids();
        }

        // v29: content 内要素 (note / audio event / automation point) の
        // 安定 id を採番する (選択・undo 後の選択復元を positional index
        // でなく id でアドレスするため)。
        for content in self.clip_contents.values_mut() {
            content.ensure_element_ids();
        }

        // v29: 旧 positional addressing (`PluginParam.device_index` /
        // `SendGain.send_idx`) を安定 id へ写像する。 device_index は
        // 「同 track の devices chain 内 index」、 song_lanes /
        // song_mod_routings の PluginParam は master_fx_chain の index。
        {
            let master_ids: Vec<u64> = self.master_fx_chain.iter().map(|p| p.id).collect();
            for track in &mut self.tracks {
                let dev_ids: Vec<u64> = track.devices.iter().map(|p| p.id).collect();
                let send_ids: Vec<u32> = track.sends.iter().map(|s| s.id).collect();
                for lane in &mut track.automation_lanes {
                    Self::remap_target_ids(&mut lane.target, &dev_ids, &send_ids);
                }
                for routing in &mut track.mod_routings {
                    Self::remap_target_ids(&mut routing.target, &dev_ids, &send_ids);
                }
            }
            for lane in &mut self.song_lanes {
                Self::remap_target_ids(&mut lane.target, &master_ids, &[]);
            }
            for routing in &mut self.song_mod_routings {
                Self::remap_target_ids(&mut routing.target, &master_ids, &[]);
            }
            // MIDI binding は任意 track の device を指せるので per-binding で
            // track を解決してから写像する。
            let track_devs: std::collections::HashMap<u32, Vec<u64>> = self
                .tracks
                .iter()
                .map(|t| (t.id, t.devices.iter().map(|p| p.id).collect()))
                .collect();
            for binding in &mut self.midi_bindings {
                if let BindingTarget::PluginParam {
                    track,
                    device_id,
                    legacy_device_index,
                    ..
                } = &mut binding.target
                    && let Some(idx) = legacy_device_index.take()
                    && *device_id == 0
                    && let Some(ids) = track_devs.get(track)
                    && let Some(&id) = ids.get(idx as usize)
                {
                    *device_id = id;
                }
            }
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
            // v23: 役割別 3 chain は単一 `devices` に統合済み。各 device の
            // aux_inputs tap の source_track / aux_outputs の dest_track を
            // 1 ループで remap する (パラアウト dest も sentinel→新 id に追従)。
            for p in track.devices.iter_mut() {
                for route in p.aux_inputs.iter_mut().flatten() {
                    if let Some(&new_id) = id_remap.get(&route.tap.source_track) {
                        route.tap.source_track = new_id;
                    }
                }
                for route in p.aux_outputs.iter_mut().flatten() {
                    if let Some(&new_id) = id_remap.get(&route.dest_track) {
                        route.dest_track = new_id;
                    }
                }
            }
        }

        // master bus の fx chain も track fx_chain と同じく aux_inputs tap /
        // aux_outputs dest を remap する。 master fx が他 track を sidechain
        // source に取る / パラアウト先に取るケースに備える (track ループ内
        // closure は loop scope なので再利用不可、 ここで open-code)。
        for p in self.master_fx_chain.iter_mut() {
            for route in p.aux_inputs.iter_mut().flatten() {
                if let Some(&new_id) = id_remap.get(&route.tap.source_track) {
                    route.tap.source_track = new_id;
                }
            }
            for route in p.aux_outputs.iter_mut().flatten() {
                if let Some(&new_id) = id_remap.get(&route.dest_track) {
                    route.dest_track = new_id;
                }
            }
        }

        // docs/plan_modulation.md §8: mod_source の tap も track id remap に追従する
        // (mod_source.id は track id ではないので不変、 tap.source_track のみ)。
        for ms in self.mod_sources.iter_mut() {
            // generator (LFO/Random/MSEG/Steps) は tap を持たない。 follower のみ remap。
            if let Some(tap) = ms.follower_tap_mut()
                && let Some(&new_id) = id_remap.get(&tap.source_track)
            {
                tap.source_track = new_id;
            }
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

    /// Effective silence for the VISUAL layer (preview + export). A track's
    /// image / video / text clips are hidden when this returns `true`.
    ///
    /// Mirrors the audio engine's effective-mute semantics, but resolves
    /// group-ancestry mute explicitly because the video pipeline has no
    /// routing graph to propagate a muted group down to its children:
    ///
    /// - **Mute**: the track itself OR any ancestor reached via
    ///   `parent_group_id` is `muted` — a muted group hides its whole
    ///   subtree, exactly what the audio engine does topologically by
    ///   dropping the muted group from the master mix.
    /// - **Solo**: when any track is soloed, the track is hidden unless it
    ///   is solo-audible (see [`Song::track_solo_audible`]). Soloing a GROUP
    ///   keeps its whole subtree visible (folder-solo, as in Ableton / Reaper)
    ///   and soloing a CHILD keeps its ancestor groups visible.
    ///
    /// Cycle-safe: `parent_group_id` walks are hop-capped at `tracks.len()`.
    pub fn track_visually_silenced(&self, track_id: u32) -> bool {
        if self.track_by_id(track_id).is_none() {
            return true;
        }
        // (1) self-or-ancestor mute (a muted group hides its subtree).
        let mut cur = Some(track_id);
        let mut hops = 0usize;
        while let Some(id) = cur {
            if hops > self.tracks.len() {
                break;
            }
            let Some(t) = self.track_by_id(id) else { break };
            if t.muted {
                return true;
            }
            cur = t.parent_group_id;
            hops += 1;
        }
        // (2) solo rule (mirrors audio exactly).
        let any_solo = self.tracks.iter().any(|t| t.solo);
        if !any_solo {
            return false;
        }
        !self.track_solo_audible(track_id)
    }

    /// True if `track_id` should be seen/heard under an active solo. A track
    /// is solo-audible iff anything in its vertical group lineage is soloed:
    /// itself, ANY ANCESTOR (soloing a group shows its children — folder
    /// solo), or ANY DESCENDANT (soloing a child keeps its ancestor groups
    /// visible). Cycle-safe via `tracks.len()` hop caps.
    fn track_solo_audible(&self, track_id: u32) -> bool {
        // self or any ANCESTOR soloed → folder solo shows the subtree.
        if self.track_by_id(track_id).is_some_and(|t| t.solo) || self.ancestor_soloed(track_id) {
            return true;
        }
        // any DESCENDANT (child chain) soloed → keep this ancestor group on.
        self.tracks.iter().any(|c| {
            c.solo && {
                let mut cur = c.parent_group_id;
                let mut hops = 0usize;
                loop {
                    let Some(pid) = cur else { break false };
                    if hops > self.tracks.len() {
                        break false;
                    }
                    if pid == track_id {
                        break true;
                    }
                    cur = self.track_by_id(pid).and_then(|p| p.parent_group_id);
                    hops += 1;
                }
            }
        })
    }

    /// True if any ANCESTOR group of `track_id` (walked via `parent_group_id`,
    /// excluding the track itself) is soloed. This is the **folder-solo** rule
    /// shared by the audio engine and the video compositor: soloing a group
    /// keeps its whole subtree audible / visible (Ableton / Reaper folder solo).
    /// RT-safe (no heap / lock); hop-capped at `tracks.len()` for cycle safety.
    pub fn ancestor_soloed(&self, track_id: u32) -> bool {
        let mut cur = self.track_by_id(track_id).and_then(|t| t.parent_group_id);
        let mut hops = 0usize;
        while let Some(pid) = cur {
            if hops > self.tracks.len() {
                break;
            }
            let Some(t) = self.track_by_id(pid) else { break };
            if t.solo {
                return true;
            }
            cur = t.parent_group_id;
            hops += 1;
        }
        false
    }

    /// True if any track points at `track_id` as its parent group (= it acts
    /// as a group / folder bus). RT-safe scan, no alloc.
    pub fn track_has_children(&self, track_id: u32) -> bool {
        self.tracks.iter().any(|t| t.parent_group_id == Some(track_id))
    }

    /// True if any track has an enabled aux send whose destination is
    /// `track_id` (= it acts as a return bus). RT-safe scan, no alloc.
    pub fn track_receives_send(&self, track_id: u32) -> bool {
        self.tracks
            .iter()
            .any(|t| t.sends.iter().any(|s| s.dest_track_id == track_id))
    }

    /// パラアウト (`docs/plan_paraout.md`): true if any plugin (on any track or
    /// the master fx chain) routes one of its aux outputs to `track_id` (= it
    /// acts as a parallel-out destination bus). RT-safe scan, no alloc. Such a
    /// track is summed + FX'd in pass 2 (`run_group_fx_chain`), so the audio
    /// engine skips its own device chain in pass 1 (like a group / return) to
    /// avoid double-processing stateful FX.
    pub fn track_receives_paraout(&self, track_id: u32) -> bool {
        self.tracks
            .iter()
            .flat_map(|t| t.devices.iter())
            .chain(self.master_fx_chain.iter())
            .any(|p| {
                p.aux_outputs
                    .iter()
                    .flatten()
                    .any(|r| r.dest_track == track_id)
            })
    }

    /// Allocate a fresh `ContentId`, bumping the song-level counter.
    pub fn alloc_content_id(&mut self) -> ContentId {
        let id = self.next_content_id.max(1);
        self.next_content_id = id.saturating_add(1);
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
    /// project BPM が `old_bpm` → `new_bpm` に変わったとき、`StretchMode::Raw`
    /// の audio clip を「実時間 (秒) 固定」で tempo に追従させる。Raw は source を
    /// 元速度で鳴らす定義 (= Ableton Warp-off / Bitwig Raw) なので、tempo が変わると
    /// 拍数で測った長さが変わる: BPM を倍にすると同じ秒数が倍の拍を占めるので、
    /// グリッド上で 2 倍の長さに伸びる (`r.md` #7)。Stretch / Repitch / Slice は
    /// 拍固定 (granular / tape で追従) なので対象外。
    ///
    /// 対象は「参照する `ClipContent::Audio` の **全 event が Raw**」な clip のみ。
    /// その content の event 拍量 (`event_start_in_clip_beats` / `event_length_beats`
    /// / fade) と、参照する各 clip の `length_beats` を `new_bpm / old_bpm` 倍する。
    /// `Clip.start_beat` は拍位置に固定 (= テンポを変えても同じ小節から始まり、右へ
    /// 伸びる)。content は pool 共有なので一度だけスケールし、参照する全 linked clip
    /// の length をスケールする (audio clip は track 上にのみ置かれるので
    /// automation_lanes / song_lanes は走査不要)。
    ///
    /// 秒固定の数学的定義: `secs = beats * 60 / bpm` を不変に保つ ⟺
    /// `beats_new = beats_old * (new_bpm / old_bpm)`。退化入力 (bpm <= 0 / 非有限 /
    /// 比 1.0) は no-op。Raw clip を 1 つ以上スケールしたら `true` を返す
    /// (= 呼び出し側が再生 window 追従のため再 compile を送る合図)。
    pub fn rescale_raw_clips_for_bpm(&mut self, old_bpm: f32, new_bpm: f32) -> bool {
        if old_bpm <= 0.0 || new_bpm <= 0.0 || !old_bpm.is_finite() || !new_bpm.is_finite() {
            return false;
        }
        let ratio = f64::from(new_bpm) / f64::from(old_bpm);
        if (ratio - 1.0).abs() < f64::EPSILON {
            return false;
        }
        // 1. Raw content (= 非空かつ全 event が Raw な Audio content) の event 拍量を
        //    秒固定スケール。pool 走査なので共有 content も一度だけ。Raw と判定した
        //    content の id を集めて、後段の clip 長スケールに使う。
        let mut raw_content_ids: std::collections::HashSet<ContentId> =
            std::collections::HashSet::new();
        for (&cid, content) in self.clip_contents.iter_mut() {
            let ClipContent::Audio(audio) = content else {
                continue;
            };
            if audio.events.is_empty()
                || !audio
                    .events
                    .iter()
                    .all(|e| e.stretch_mode == StretchMode::Raw)
            {
                continue;
            }
            for event in &mut audio.events {
                event.event_start_in_clip_beats *= ratio;
                event.event_length_beats *= ratio;
                event.fade_in_beats *= ratio;
                event.fade_out_beats *= ratio;
            }
            raw_content_ids.insert(cid);
        }
        if raw_content_ids.is_empty() {
            return false;
        }
        // 2. Raw content を参照する clip の length_beats をスケール (start_beat は固定)。
        for track in &mut self.tracks {
            for clip in &mut track.clips {
                if raw_content_ids.contains(&clip.content_id) {
                    clip.length_beats *= ratio;
                }
            }
        }
        true
    }

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
        for lane in &self.song_lanes {
            for clip in &lane.clips {
                if clip.content_id != 0 {
                    max_seen = max_seen.max(clip.content_id);
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
                                ..Default::default()
                            });
                        })
                        .or_insert_with(|| {
                            ClipContent::Midi(MidiContent {
                                notes,
                                ..Default::default()
                            })
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
        // Song-level automation lanes share the same content store but are
        // not reached by the per-track walk above. Reassign sentinel ids,
        // drain legacy names, and ensure an entry exists — mirroring the
        // `automation_lanes` handling so SongTempo / TimeSig curves resolve
        // instead of silently falling back to empty.
        for l_idx in 0..self.song_lanes.len() {
            let lane_clip_count = self.song_lanes[l_idx].clips.len();
            for c_idx in 0..lane_clip_count {
                if self.song_lanes[l_idx].clips[c_idx].content_id == 0 {
                    let new_id = self.alloc_content_id();
                    self.song_lanes[l_idx].clips[c_idx].content_id = new_id;
                }
                let cid = self.song_lanes[l_idx].clips[c_idx].content_id;
                let legacy_name =
                    std::mem::take(&mut self.song_lanes[l_idx].clips[c_idx].name);
                if !legacy_name.is_empty() {
                    self.clip_content_names.entry(cid).or_insert(legacy_name);
                }
                self.clip_contents.entry(cid).or_insert_with(|| {
                    ClipContent::Automation(AutomationContent::default())
                });
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
        // Song-level lanes share the same content store, so they must be
        // counted too — otherwise a shared tempo curve reads refcount 0
        // and the GUI / GC treat it as unreferenced.
        let song_lane_clips = self
            .song_lanes
            .iter()
            .flat_map(|l| l.clips.iter())
            .filter(|c| c.content_id == content_id)
            .count();
        main_clips + auto_clips + song_lane_clips
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

    /// `track_id` の send (安定 id = `send_id`) を削除し、 その send を狙う
    /// SendGain automation lane / mod routing を除去する。 v29 で id
    /// addressing になったため、 残る send への参照は**無変更のまま正しい**
    /// (positional 時代の「後続 index を詰める」 reindex 儀式は不要になった —
    /// r.md #8 A5 で実際に壊れた class の構造的解消)。
    /// 削除成功で `true`、 track 不在 / id 不在なら `false`。
    pub fn remove_track_send(&mut self, track_id: u32, send_id: u32) -> bool {
        let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pos) = t.sends.iter().position(|s| s.id == send_id) else {
            return false;
        };
        t.sends.remove(pos);
        let targets_send = |target: &AutomationTarget| {
            matches!(
                target,
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_id: sid, .. })
                    if *sid == send_id
            )
        };
        let before = t.automation_lanes.len();
        t.automation_lanes.retain(|lane| !targets_send(&lane.target));
        let dropped = t.automation_lanes.len() != before;
        t.mod_routings.retain(|r| !targets_send(&r.target));
        if dropped {
            self.gc_clip_contents();
        }
        true
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
        // Song-level automation lanes (SongTempo / TimeSig master lanes)
        // share the same `clip_contents` store. Without walking them, a
        // tempo-automation curve's `content_id` is judged dead and GC'd
        // before save, losing the whole curve on next load.
        for lane in &self.song_lanes {
            for clip in &lane.clips {
                live.insert(clip.content_id);
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
        self.next_audio_source_id = id.saturating_add(1);
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
        self.next_video_source_id = id.saturating_add(1);
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
        self.next_image_source_id = id.saturating_add(1);
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

/// v23 (`docs/plan_linear_chain.md`): a track owns **one** linear CLAP
/// signal chain, `devices: Vec<PluginInstance>`. Roles (MIDI FX / instrument
/// / audio FX) are **not classified at all** — the engine simply connects
/// ports serially (Reaper 流): the track's notes flow into every device with a
/// note input, the track's audio (clips) flows into every device with an audio
/// input, and each device's note / audio outputs feed the next. The behaviour
/// (whether a device acts as a generator, instrument, or effect) emerges from
/// its own `PortConfig` + processing, not a stored or derived label. The final
/// audio flows into the parent — either a `Group` track (when
/// `parent_group_id == Some(id)`) or the master bus (when `None`). Reorder /
/// insert / remove just permute the Vec; nothing else to re-key.
///
/// Older files used three role-keyed fields (`midi_fx_chain` / `instrument`
/// / `fx_chain`); they deserialize into the private `legacy_*` slots and
/// `Track::flatten_legacy_devices` (run from `Song::ensure_ids`) flattens
/// them into `devices` in `midi_fx ++ instrument? ++ fx` order.
///
/// v16 (`docs/plan_text_overlay.md`): 旧 `kind: TrackKind { Audio, Video }`
/// を廃止し、 全 track が unified に audio path + visual composite path 両方
/// を保持する (= REAPER 流、 同 track 上で audio / midi / video / image /
/// text clip を混在可能)。 旧 Video track は v16 migration で audio
/// defaults (instrument: None / fx_chain: vec![] / volume: 1.0 / pan: 0.0
/// / armed: false / source: None) を自動補完し、 mixer / engine path に
/// 静かに参加する (v23 以降は空 `devices` に相当)。 旧 v15 file の `kind`
/// field は serde が未知 field として捨てる (= deny_unknown_fields が無いため
/// tolerant)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Track {
    /// Stable id assigned by `Song::alloc_track_id`. `0` is "未採番"
    /// sentinel — reassigned by `Song::ensure_ids` when loading an older
    /// file. Persists across track add/remove and reorder; arrangement
    /// widget addresses tracks by this id, not by index.
    #[serde(default)]
    pub id: u32,
    pub name: String,
    /// v23: 1 本の線形デバイスチェーン。役割は保持せず ports から位置導出。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<PluginInstance>,
    // pub(crate): public API には出さないが、同 crate の sibling module
    // (project.rs / timing.rs 等) が `..Track::default()` の functional-update
    // 構文で Track を組むため、module-private では E0451 になる。migration
    // 専用なので model module 外から直接読む用途は無い。
    #[serde(default, rename = "midi_fx_chain", skip_serializing)]
    pub(crate) legacy_midi_fx_chain: Vec<PluginInstance>,
    #[serde(default, rename = "instrument", skip_serializing)]
    pub(crate) legacy_instrument: Option<PluginInstance>,
    #[serde(default, rename = "fx_chain", skip_serializing)]
    pub(crate) legacy_fx_chain: Vec<PluginInstance>,
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
    /// v29: per-track stable id allocator for `Send::id`。`0` は sentinel、
    /// `1` から採番。
    #[serde(default)]
    pub next_send_id: u32,
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
    /// **lane 非依存モジュレーション** (`docs/plan_modulation_routing_redesign.md`
    /// §2): この track 内の param (`TrackBuiltin` / `PluginParam` / `ImageBuiltin`
    /// / `TextBuiltin` / `GroupTransform`) を変調する `ModRouting` の集合。各
    /// routing の `target` が param を直接指すので automation lane は不要。空 Vec
    /// で変調なし (旧 file は forward-migrate)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mod_routings: Vec<ModRouting>,
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
/// SendGain { send_id })`, addressed by this send's stable `id` (v29 —
/// positional index addressing は廃止)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct Send {
    /// v29: track 内で安定な send id (`Track.next_send_id` 採番、`0` = 未採番
    /// sentinel — `Track::ensure_send_ids` が load 時に採番)。automation
    /// (`TrackBuiltinParam::SendGain`) と IPC は positional index でなく
    /// この id でアドレスする。
    #[serde(default)]
    pub id: u32,
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
    /// 「このトラックは VOICEVOX builtin で歌わせる」印。
    /// 声 (singer + style) は per-clip (`Clip::speaker_id` 等) が SSoT で、
    /// トラックは声を持たない unit marker。旧プロジェクト
    /// (`Vocal { speaker_id, style_name }`) は `project::load` の JSON
    /// 前処理で旧トラック声を全 clip へ焼き込んでから unit `Vocal` に
    /// 移行する (`migrate_vocal_source_to_clips`)。
    Vocal,
    Vst3 { path: PathBuf },
    BuiltinSynth,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            devices: Vec::new(),
            legacy_midi_fx_chain: Vec::new(),
            legacy_instrument: None,
            legacy_fx_chain: Vec::new(),
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
            next_send_id: 1,
            reported_latency_samples: 0,
            automation_lanes: Vec::new(),
            next_lane_id: 1,
            mod_routings: Vec::new(),
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
    /// このトラックが VOICEVOX で歌う vocal トラックか。 SSoT は
    /// 「builtin VOICEVOX device を実際に持つか」。 旧 `InstrumentSource::Vocal`
    /// marker は device 挿入と別管理で out-of-sync になり得る (旧プロジェクトで
    /// source=None + device の実例あり) ため、 装置の実在を真実として判定する。
    pub fn is_voicevox_vocal(&self) -> bool {
        self.devices.iter().any(|d| {
            d.format == crate::plugin_format::PluginFormat::Builtin
                && d.plugin_id == crate::plugin_db::BUILTIN_ID_VOICEVOX
        })
    }

    /// (talk) このトラックが字幕(テキスト表示)デバイスを持つか。SSoT は
    /// 「builtin 字幕 device を実際に持つか」(`is_voicevox_vocal` と同思想)。
    /// `true` のときだけ、このトラック上の `ClipContent::Text` clip が画面に
    /// overlay 表示される (`docs/plan_voicevox_talk.md` §2、`text_compose` が gate)。
    pub fn has_subtitle_device(&self) -> bool {
        self.devices.iter().any(|d| {
            d.format == crate::plugin_format::PluginFormat::Builtin
                && d.plugin_id == crate::plugin_db::SUBTITLE_ID
        })
    }

    /// パラアウト (`docs/plan_paraout.md`): the device-chain split point for a
    /// **group-with-instrument** track. `Some(k)` when at least one device has
    /// a routed aux output (= this track is a parallel-out source); `k` is the
    /// index **one past** the last such device. The audio engine runs the
    /// "instrument prefix" `devices[0..k]` in pass 1 (producing the main signal
    /// and the aux outputs) and the "bus FX suffix" `devices[k..]` in pass 2 on
    /// the summed bus (own main plus routed children). `None` when no device
    /// routes an aux output (a plain leaf / pure group). Derived purely from the
    /// explicit routing data (`aux_outputs`), never a role heuristic.
    pub fn paraout_split_device(&self) -> Option<u32> {
        let mut last: Option<u32> = None;
        for (i, d) in self.devices.iter().enumerate() {
            if d.aux_outputs.iter().any(Option::is_some) {
                last = Some(i as u32);
            }
        }
        last.map(|i| i + 1)
    }

    /// パラアウト (`docs/plan_paraout.md`): true if this track's instrument
    /// routes its **main** output (parallel-out port 0) to a child track. When
    /// true the parent is a "pure splitter": its main signal goes to its own
    /// child (the first part / Out 1), so the engine must NOT keep main in the
    /// parent's scratch — the parent sums ALL children via a clearing `Mix`.
    /// When false (port 0 unrouted) the parent keeps its main as its own bus
    /// signal and sums children on top (instrument-bus, `MixAdditive`). Decided
    /// purely from explicit routing data (`aux_outputs[0]`), no role heuristic.
    pub fn paraout_main_to_child(&self) -> bool {
        self.devices
            .iter()
            .any(|d| matches!(d.aux_outputs.first(), Some(Some(_))))
    }

    /// Allocate a new stable clip id, bumping the per-track counter.
    pub fn alloc_clip_id(&mut self) -> u32 {
        let id = self.next_clip_id.max(1);
        self.next_clip_id = id.saturating_add(1);
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
        self.next_lane_id = id.saturating_add(1);
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

    /// v23 migration: 旧 3-split を devices へ平坦化 (midi_fx ++ instrument? ++ fx) し、
    /// automation lane の旧 slot を device_index へ写像する。新形式 (devices 既存) は no-op。
    /// v29: send 安定 id を採番する。 sentinel (0) のみ上書き、 既存非 0 id
    /// は counter を bump するだけ。 `Song::ensure_ids` が load 時に呼ぶ。
    pub(crate) fn ensure_send_ids(&mut self) {
        for send in &mut self.sends {
            if send.id == 0 {
                let new_id = self.next_send_id.max(1);
                self.next_send_id = new_id + 1;
                send.id = new_id;
            } else if send.id >= self.next_send_id {
                self.next_send_id = send.id + 1;
            }
        }
        if self.next_send_id == 0 {
            self.next_send_id = 1;
        }
    }

    /// v29: 新規 send 用の安定 id を採番する。
    pub fn alloc_send_id(&mut self) -> u32 {
        let id = self.next_send_id.max(1);
        self.next_send_id = id.saturating_add(1);
        id
    }

    fn flatten_legacy_devices(&mut self) {
        if !self.devices.is_empty()
            || (self.legacy_midi_fx_chain.is_empty()
                && self.legacy_instrument.is_none()
                && self.legacy_fx_chain.is_empty())
        {
            return;
        }
        let n_midi = self.legacy_midi_fx_chain.len();
        let has_inst = self.legacy_instrument.is_some();
        let to_index = |slot: crate::protocol::PluginSlot| -> u32 {
            use crate::protocol::PluginSlot;
            match slot {
                PluginSlot::MidiFx(i) => i,
                PluginSlot::Instrument => n_midi as u32,
                PluginSlot::Fx(i) => (n_midi + has_inst as usize) as u32 + i,
            }
        };
        for lane in &mut self.automation_lanes {
            if let AutomationTarget::PluginParam {
                legacy_device_index,
                legacy_slot,
                ..
            } = &mut lane.target
                && let Some(slot) = legacy_slot.take()
            {
                // v29: slot → index はまだ positional。 後段の
                // `Song::ensure_ids` の remap pass が device_id へ写像する。
                *legacy_device_index = Some(to_index(slot));
            }
        }
        let mut devices = Vec::new();
        devices.append(&mut self.legacy_midi_fx_chain);
        if let Some(inst) = self.legacy_instrument.take() {
            devices.push(inst);
        }
        devices.append(&mut self.legacy_fx_chain);
        self.devices = devices;
    }
}

// =====================================================================
// Modulation routing (sidechain + envelope follower) — docs/plan_modulation.md
// =====================================================================
//
// 「音をどこから取るか」の唯一の真実 = `AudioTap`。 消費者は 2 つだけで、
// どちらも AudioTap の "使い方" であって新しいルートではない:
//   - Consumer A: `AuxInputRoute` → プラグイン aux 入力 (旧 sidechain を吸収)
//   - Consumer B: `ModSource` の envelope follower → param 変調 (`ModRouting`)

/// タップ点。 track の音をどの段で拾うか (plan §6, Q4)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum TapPoint {
    /// device chain 適用前の素の音 (`pre_fx_l/r` snapshot として捕捉。
    /// engine.rs `resolve_tap_buf` / `any_pre_fx_tap` が消費)。
    PreFx,
    /// device chain 適用後・ fader 前 (`PreFaderScratch`)。
    PostFx,
    /// fader 後 (`TrackScratch`)。 旧 sidechain の既定。
    #[default]
    PostFader,
}

/// 「音をどこから取るか」 の SSoT。 source track + タップ点。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct AudioTap {
    /// 音源となる `Track::id`。
    pub source_track: u32,
    /// どの段で拾うか。 旧 file は `PostFader` に forward-migrate。
    #[serde(default)]
    pub tap_point: TapPoint,
}

impl AudioTap {
    /// 旧 sidechain (= 常に post-fader) からの lift / 既定構築。
    pub fn post_fader(source_track: u32) -> Self {
        Self {
            source_track,
            tap_point: TapPoint::PostFader,
        }
    }
}

/// Consumer A: プラグイン aux 入力ルート。 旧 `sidechain_sources` を置換し、
/// 生音声を `pd.buffer_aux_in[port]` に staging する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct AuxInputRoute {
    pub tap: AudioTap,
}

impl AuxInputRoute {
    /// 旧 sidechain (常に post-fader) からの lift / 既定構築。
    pub fn post_fader(source_track: u32) -> Self {
        Self {
            tap: AudioTap::post_fader(source_track),
        }
    }
}

/// Consumer B: プラグイン aux **出力** ルート (パラアウト、`docs/plan_paraout.md`)。
/// `AuxInputRoute` の対称。プラグインの `is_main=false` な出力ポート 1 本を
/// どのトラックへ流すかを表す。`PluginInstance::aux_outputs[port_idx]` に格納し、
/// `daw_plugin_host` が `pd.buffer_aux_out[port]` に書いた音を engine の
/// `NodeOp::ParallelOutTap` が `dest_track` の入力へ加算する。サイドチェインと
/// 違いタップ点 (PreFx/PostFx/PostFader) は無い (出力は常に dest の入力へ入る)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct AuxOutputRoute {
    /// この aux 出力ポートの行き先 `Track::id`。
    pub dest_track: u32,
}

impl AuxOutputRoute {
    pub fn to_track(dest_track: u32) -> Self {
        Self { dest_track }
    }
}

/// エンベロープフォロワーの検出モード (plan §3)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum FollowerMode {
    /// ピーク (max|L|,|R|)。
    #[default]
    Peak,
    /// RMS (sqrt(½(L²+R²)))。
    Rms,
}

/// キック抽出用の帯域フィルタ (plan §3, Q3)。 検出前に source 音へ適用。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct BandFilter {
    /// ハイパス cutoff (Hz)。
    pub hp_hz: f32,
    /// ローパス cutoff (Hz)。
    pub lp_hz: f32,
}

/// フォロワー解析パラメータ。 状態 (env/biquad) はエンジン所有リングに置き、
/// ここには設定のみ (RT-safe・ Copy、 plan §10)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct FollowerConfig {
    pub mode: FollowerMode,
    /// 検出前に全波整流するか。
    pub rectify: bool,
    pub attack_ms: f32,
    pub release_ms: f32,
    /// 検出前ゲイン。
    pub gain: f32,
    /// キック抽出等の帯域制限。 `None` で全帯域。
    #[serde(default)]
    pub band_filter: Option<BandFilter>,
}

impl Default for FollowerConfig {
    fn default() -> Self {
        Self {
            mode: FollowerMode::Peak,
            rectify: true,
            attack_ms: 1.0,
            release_ms: 100.0,
            gain: 1.0,
            band_filter: None,
        }
    }
}

/// source rack の色割当 palette (Bitwig 流、 作成順に循環)。
pub const MOD_SOURCE_PALETTE: [[f32; 3]; 8] = [
    [0.30, 0.69, 1.00], // 青
    [1.00, 0.55, 0.26], // 橙
    [0.45, 0.85, 0.45], // 緑
    [0.95, 0.45, 0.75], // 桃
    [0.80, 0.65, 1.00], // 紫
    [1.00, 0.85, 0.30], // 黄
    [0.40, 0.85, 0.85], // 水
    [0.95, 0.45, 0.45], // 赤
];

fn default_mod_color() -> [f32; 3] {
    MOD_SOURCE_PALETTE[0]
}

/// 共有モジュレーション源 (1 source → 多 params、 plan Q2)。 `Song.mod_sources`
/// が route の唯一の所有者。 `id` で `ModRouting.source_id` から参照される。
/// envelope follower 専用から **generator 種別** (`kind`) へ一般化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ModSource {
    /// 安定 id。 `0` は "未採番" sentinel、 `ensure_ids` が採番。
    #[serde(default)]
    pub id: u32,
    /// `docs/plan_modulation_routing_redesign.md` §6: このソースが**帰属する
    /// トラック** (Bitwig 流: モジュレーターはトラックに属する)。ソースは `id` で
    /// グローバル参照され続ける (= 他トラックの param も変調できる) が、inspector
    /// では帰属トラックの下にだけ列挙する。master 帰属は `MASTER_TRACK_ID`。
    /// `0` = legacy (どこにも表示しない。未リリース機能なので該当データは無い)。
    #[serde(default)]
    pub owner_track_id: u32,
    /// source 識別色 (depth リング/rack 用)。 作成時に palette から循環割当。
    #[serde(default = "default_mod_color")]
    pub color: [f32; 3],
    /// 変調器種別 (envelope follower / LFO / Random / MSEG / Steps)。
    #[serde(default)]
    pub kind: ModSourceKind,
}

impl ModSource {
    /// 作成順 `index` に対応する palette 色。
    pub fn palette_color(index: usize) -> [f32; 3] {
        MOD_SOURCE_PALETTE[index % MOD_SOURCE_PALETTE.len()]
    }

    /// envelope follower のときだけ `(tap, follower)` を返す (generator は `None`)。
    pub fn follower(&self) -> Option<(&AudioTap, &FollowerConfig)> {
        if let ModSourceKind::EnvelopeFollower { tap, follower } = &self.kind {
            Some((tap, follower))
        } else {
            None
        }
    }

    /// envelope follower の tap を可変借用 (generator は `None`)。
    pub fn follower_tap_mut(&mut self) -> Option<&mut AudioTap> {
        if let ModSourceKind::EnvelopeFollower { tap, .. } = &mut self.kind {
            Some(tap)
        } else {
            None
        }
    }
}

/// 変調の極性 (plan Q5)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum Polarity {
    /// 0..=1 を `depth*s` で加算。
    #[default]
    Unipolar,
    /// -1..=1 を `depth*(2s-1)` で加算。
    Bipolar,
}

/// Consumer B: param への変調 edge (加算スタック、 plan Q5)。
/// **lane 非依存** — `Track.mod_routings` / `Song.song_mod_routings` に置かれ、
/// `target` が指す param を `source_id` のフォロワー値で変調する
/// (`docs/plan_modulation_routing_redesign.md` §2)。Bitwig と同じく automation
/// レーンの有無に関係なく変調できる。base (= 変調前の値) は当該 target に
/// automation lane があればその値、無ければモデルの現在値 (ノブ / plugin param
/// 値キャッシュ)。`AutomationTarget` を内包するため `Copy` ではない。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct ModRouting {
    /// 変調先 param。lane と独立に param を直接指す。
    pub target: AutomationTarget,
    /// → `Song.mod_sources[*].id`。
    pub source_id: u32,
    /// target の *正規化* 領域での量 (-1..=1)。
    pub depth: f32,
    #[serde(default)]
    pub polarity: Polarity,
}

// =====================================================================
// Generator modulators (LFO / Random / MSEG / Steps) — docs/plan_fixme_56_modulators.md
// =====================================================================
//
// `ModSource` を envelope follower 専用から **generator 種別** へ一般化する。
// envelope follower は audio 入力に依存し engine ring が `env` を
// 算出するが、generator (LFO/Random/MSEG/Steps) は **`song_beat` の純粋関数** で
// audio に依存しない。よって ring 不要・状態レス・全経路 (RT preview / 音声書き出し
// / video export) で同一関数 → drift ゼロ・bounce 完全再現。評価は
// `common::modulators::generator_scalar`。出力は常に unipolar 0..=1 で、極性は
// 後段の `ModRouting.polarity` が担う (SSoT、 follower と同じ契約)。

/// 全 generator 共通の rate。 free-running な絶対周波数か、 tempo-synced な音価。
/// free でも壁時計でなく **song 秒** で評価するので決定論的 (plan §0)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ModRate {
    /// transport 非同期の絶対周波数 (Hz)。 phase = `frac(song_secs * hz + phase0)`。
    Free { hz: f32 },
    /// 音価同期。 `period_beats = 4.0 * numerator / denominator`
    /// (1/4=(1,4)→1拍, 1bar=(1,1)→4拍, 1/8三連=(1,12), 付点1/4=(3,8))。
    Sync { numerator: u32, denominator: u32 },
}

impl Default for ModRate {
    fn default() -> Self {
        // 1/4 note。
        ModRate::Sync {
            numerator: 1,
            denominator: 4,
        }
    }
}

/// タイムライン変調器の retrigger。 per-note 鍵盤文脈は無いので、 壁時計・再生
/// イベント基準は採らない (決定論を壊す)。 phase は常に song 位置の関数 (plan §0)。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum RetriggerMode {
    /// phase = f(song_beat) を連続評価。 既定 (Bitwig "Sync" 相当)。 loop 跨ぎでも一致。
    #[default]
    FreeRun,
    /// phase = f(song_beat - anchor_beat)。 clip / loop 開始等の beat 基準。 MSEG OneShot 用。
    FromBeat { anchor_beat: f64 },
}

/// LFO 波形。 phase 0..=1 → unipolar 0..=1。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub enum LfoShape {
    #[default]
    Sine,
    Triangle,
    /// 上昇ノコギリ (ramp)。
    SawUp,
    /// 下降ノコギリ。
    SawDown,
    /// 矩形 (duty 50%)。
    Square,
    /// 可変 duty パルス。 `width` 0..=1。
    Pulse {
        width: f32,
    },
}

/// 周期波 LFO。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct LfoConfig {
    pub shape: LfoShape,
    pub rate: ModRate,
    /// cycle 内の開始オフセット 0..=1。
    pub phase: f32,
    pub retrigger: RetriggerMode,
}

impl Default for LfoConfig {
    fn default() -> Self {
        Self {
            shape: LfoShape::Sine,
            rate: ModRate::default(),
            phase: 0.0,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// 乱数変調器。 `seed` を保存し `hash(seed, step)` の純関数にして **オフライン再現**
/// を保証する (Vital は mt19937 をグローバル seed で非再現、 plan §0)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct RandomConfig {
    pub rate: ModRate,
    /// Bitwig の Stepped↔Smoothed 連続モーフ。 `0.0` = 完全階段 (sample & hold)、
    /// `1.0` = 隣接 step を smoothstep 補間 (滑らかな乱数)、 中間は両者の lerp。
    pub smooth: f32,
    /// 決定論のための seed (source 作成時に採番・保存。 UI で re-roll 可)。
    pub seed: u64,
    pub retrigger: RetriggerMode,
}

impl Default for RandomConfig {
    fn default() -> Self {
        Self {
            rate: ModRate::default(),
            // 既定は完全 smoothed (旧 RandomMode::Smooth 相当)。
            smooth: 1.0,
            seed: 0,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// MSEG の 1 ブレークポイント。 `time`/`value` は 0..=1、 `time` は単調増加で
/// `points[0].time == 0.0`。 `curve` は次セグメントへの tension (-1..=1、 0=linear、
/// +=凸、 -=凹)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MsegPoint {
    pub time: f32,
    pub value: f32,
    pub curve: f32,
}

/// MSEG の 1 周の再生モード (per-note sustain はタイムライン文脈で無効なので除外)。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum MsegPlayMode {
    /// 1 周のみ (FromBeat anchor から)。
    OneShot,
    /// 連続ループ。
    #[default]
    Loop,
    /// 折り返しループ (forward → backward)。
    PingPong,
}

/// 自由描画できる多段エンベロープ (Bitwig Curves/Segments、 Vital LineGenerator 相当)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MsegConfig {
    /// 時刻昇順の breakpoint 列 (`points[0].time == 0.0`、 不変条件)。
    pub points: Vec<MsegPoint>,
    /// 1 周の長さ。
    pub rate: ModRate,
    pub play_mode: MsegPlayMode,
    pub retrigger: RetriggerMode,
}

impl Default for MsegConfig {
    fn default() -> Self {
        // 既定 = 三角 (0,0)-(0.5,1)-(1,0)。
        Self {
            points: vec![
                MsegPoint {
                    time: 0.0,
                    value: 0.0,
                    curve: 0.0,
                },
                MsegPoint {
                    time: 0.5,
                    value: 1.0,
                    curve: 0.0,
                },
                MsegPoint {
                    time: 1.0,
                    value: 0.0,
                    curve: 0.0,
                },
            ],
            rate: ModRate::default(),
            play_mode: MsegPlayMode::Loop,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// ステップシーケンサの進行方向。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Encode, Decode,
)]
pub enum StepsDirection {
    #[default]
    Forward,
    Backward,
    /// 端で折り返し。
    PingPong,
}

/// ステップシーケンサ (Bitwig Steps 相当)。 各 step の値 (0..=1) を順に出す。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct StepsConfig {
    /// step 値 0..=1 (最低 1 個)。
    pub values: Vec<f32>,
    /// 1 周 (全 step) の長さ。
    pub rate: ModRate,
    pub direction: StepsDirection,
    /// step 間の slew (0=階段、 >0 で隣接 step を補間)。
    pub slew: f32,
    pub retrigger: RetriggerMode,
}

impl Default for StepsConfig {
    fn default() -> Self {
        // 既定 = 8 step の上昇階段 (一目で sequencer と分かる)。
        Self {
            values: (0..8).map(|i| i as f32 / 7.0).collect(),
            rate: ModRate::default(),
            direction: StepsDirection::Forward,
            slew: 0.0,
            retrigger: RetriggerMode::FreeRun,
        }
    }
}

/// `ModSource` の変調器種別 (plan §1)。 envelope follower は既存を内包し、
/// generator 4 種を追加。 `Vec` を持つため `Copy` 不可 (`ModRouting` と同じ)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum ModSourceKind {
    /// 他トラック音声のエンベロープフォロワー (既存基盤)。
    EnvelopeFollower {
        tap: AudioTap,
        follower: FollowerConfig,
    },
    Lfo(LfoConfig),
    Random(RandomConfig),
    Mseg(MsegConfig),
    Steps(StepsConfig),
}

impl Default for ModSourceKind {
    fn default() -> Self {
        ModSourceKind::EnvelopeFollower {
            tap: AudioTap::post_fader(0),
            follower: FollowerConfig::default(),
        }
    }
}

impl ModSourceKind {
    /// generator 共通の rate (follower は `None`)。
    pub fn rate_mut(&mut self) -> Option<&mut ModRate> {
        match self {
            ModSourceKind::Lfo(c) => Some(&mut c.rate),
            ModSourceKind::Random(c) => Some(&mut c.rate),
            ModSourceKind::Mseg(c) => Some(&mut c.rate),
            ModSourceKind::Steps(c) => Some(&mut c.rate),
            ModSourceKind::EnvelopeFollower { .. } => None,
        }
    }

    /// generator 共通の retrigger (follower は `None`)。
    pub fn retrigger_mut(&mut self) -> Option<&mut RetriggerMode> {
        match self {
            ModSourceKind::Lfo(c) => Some(&mut c.retrigger),
            ModSourceKind::Random(c) => Some(&mut c.retrigger),
            ModSourceKind::Mseg(c) => Some(&mut c.retrigger),
            ModSourceKind::Steps(c) => Some(&mut c.retrigger),
            ModSourceKind::EnvelopeFollower { .. } => None,
        }
    }

    /// 短い種別ラベル (UI rack ヘッダ用)。
    pub fn short_label(&self) -> &'static str {
        match self {
            ModSourceKind::EnvelopeFollower { .. } => "Follow",
            ModSourceKind::Lfo(_) => "LFO",
            ModSourceKind::Random(_) => "Rand",
            ModSourceKind::Mseg(_) => "MSEG",
            ModSourceKind::Steps(_) => "Steps",
        }
    }
}

/// Reference to a plugin loaded on a track, with the opaque state blob the
/// plugin itself produced (CLAP `clap_plugin_state.save` or VST3
/// `IComponent::getState`). Paths are NOT stored — `(format, plugin_id)`
/// is resolved through `plugin_db::PluginDatabase` at load time, keeping
/// projects portable across machines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstance {
    /// v29: Song-global の安定 device id (`Song.next_device_id` 採番、`0` =
    /// 未採番 sentinel)。IPC / automation / plugin host bookkeeping / shmem 名
    /// / worker dispatch のアドレスはすべてこの id。chain 内 index は表示順序
    /// のみ (`docs/plan_arch_refactor.md` §1)。
    #[serde(default)]
    pub id: u64,
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
    /// D2 (r.md #8): `Arc<[u8]>` で保持し undo snapshot 間で共有 (= `Song::clone`
    /// が plugin state を毎回コピーしない)。 plugin が serialize した不透明 state で
    /// undo の編集対象ではないので共有して安全。
    pub state: Option<std::sync::Arc<[u8]>>,
    /// Consumer A (旧 sidechain、 docs/plan_modulation.md §1): aux 入力ポート
    /// ごとのルート。 各 entry は plugin の `is_main=false` aux input port
    /// index → `AudioTap`。 `None` (or 不足 index) はそのポートを無音に。
    /// `Vec` 長 = user が配線した aux port 数 (plugin の実 port 数より短くて
    /// よい — 末尾は無音)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aux_inputs: Vec<Option<AuxInputRoute>>,
    /// deserialize 専用 migration: 旧 save の `sidechain_sources:
    /// Vec<Option<u32>>`。 `ensure_ids` が `migrate_legacy_aux` で
    /// `Some(id) → AuxInputRoute{tap:{id, PostFader}}` に lift して
    /// `aux_inputs` を埋める。 serialize はしない (`legacy_slot` /
    /// `legacy_*_chain` と同 idiom)。 ロード後は常に空。 `pub` は外部クレートが
    /// `..PluginInstance::with_ports(..)` (FRU) で instance を組めるようにする
    /// ためで、 設定値に意味は無い (= migrate 後 drain される)。
    #[serde(default, rename = "sidechain_sources", skip_serializing)]
    pub legacy_aux_sources: Vec<Option<u32>>,
    /// Consumer B (パラアウト、 docs/plan_paraout.md): aux **出力**ポートごとの
    /// ルート。 各 entry は plugin の `is_main=false` aux output port index →
    /// `AuxOutputRoute { dest_track }`。 `None` (or 不足 index) はそのポートを
    /// どこにも流さない (= 業界標準: 未振分け aux 出力は無音)。 `Vec` 長 = user が
    /// 配線した aux port 数 (plugin の実 port 数より短くてよい)。 旧 file には
    /// 無いので `#[serde(default)]` で forward-migrate (空)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aux_outputs: Vec<Option<AuxOutputRoute>>,
    /// パラアウト (docs/plan_paraout.md): how many `is_main=false` audio output
    /// ports this plugin actually declares (reported by the plugin host at load
    /// via `SlotPluginLoaded`, cached here so it survives reorder and is known
    /// on project reopen). The GUI uses it to know how many child tracks to
    /// create on "explode" and how many routing rows to show. `0` = the common
    /// single-output plugin. Distinct from `aux_outputs.len()` (= how many
    /// ports the user has wired). daw_audio ignores it (it routes via
    /// `aux_outputs` + the plugin host's `aux_out_active`).
    #[serde(default)]
    pub aux_output_count: u8,
    /// v23: この device の port 構成。役割導出の入力。
    #[serde(default)]
    pub ports: crate::port_config::PortConfig,
    /// (r.md #5 ARA2) ARA ドキュメントアーカイブ = プラグインがシリアライズした
    /// 編集状態 (Melodyne のピッチ修正等)。ホストが instance ごとに保持し、
    /// プロジェクトには base64 で保存、ロード時にプラグインへ送り返して編集を
    /// 復元する。`state` (CLAP/VST3 own state) とは独立。
    #[serde(default, skip_serializing_if = "Option::is_none", with = "base64_opt")]
    /// D2 (r.md #8): `Arc<[u8]>` で保持し undo snapshot 間で共有 (Melodyne 等の
    /// ARA アーカイブは MB 級で undo の編集対象でないため)。
    pub ara_archive: Option<std::sync::Arc<[u8]>>,
}

impl PluginInstance {
    pub fn new(plugin_id: String, format: PluginFormat) -> Self {
        Self {
            id: 0,
            plugin_id,
            format,
            state: None,
            aux_inputs: Vec::new(),
            legacy_aux_sources: Vec::new(),
            aux_outputs: Vec::new(),
            aux_output_count: 0,
            ports: crate::port_config::PortConfig::default(),
            ara_archive: None,
        }
    }

    pub fn with_ports(
        plugin_id: String,
        format: PluginFormat,
        ports: crate::port_config::PortConfig,
    ) -> Self {
        Self {
            id: 0,
            plugin_id,
            format,
            state: None,
            aux_inputs: Vec::new(),
            legacy_aux_sources: Vec::new(),
            aux_outputs: Vec::new(),
            aux_output_count: 0,
            ports,
            ara_archive: None,
        }
    }

    /// 旧 `sidechain_sources` (deserialize 専用 `legacy_aux_sources`) を
    /// `aux_inputs` に lift する。 `ensure_ids` が load 時に各 instance へ呼ぶ。
    /// idempotent (lift 後 / 新形式は no-op、 legacy を drain するだけ)。
    pub(crate) fn migrate_legacy_aux(&mut self) {
        if self.aux_inputs.is_empty() && !self.legacy_aux_sources.is_empty() {
            self.aux_inputs = std::mem::take(&mut self.legacy_aux_sources)
                .into_iter()
                .map(|opt| opt.map(AuxInputRoute::post_fader))
                .collect();
        } else {
            self.legacy_aux_sources = Vec::new();
        }
    }
}

/// wire (bincode / IPC) 表現は手書きで、`state` / `ara_archive` の MB 級 blob を
/// **構造的に除外**する (`docs/plan_arch_refactor.md` §2)。ドキュメント
/// (serde / JSON 保存) は両フィールドを base64 で保持し、blob が必要な IPC
/// 操作は専用メッセージ (`SetSlotPlugin.initial_state` /
/// `SetupAraDocument.archive` / `AllPluginStates`) が個別に運ぶ。これで
/// `LoadSong` は plugin state / ARA アーカイブの肥大に依らず常に小さく、
/// 16MB wire 上限に構造的に到達しない。encode / decode の field 順は一致
/// させること (id → plugin_id → format → aux_inputs → aux_outputs →
/// aux_output_count → ports)。
impl bincode::Encode for PluginInstance {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> Result<(), bincode::error::EncodeError> {
        self.id.encode(encoder)?;
        self.plugin_id.encode(encoder)?;
        self.format.encode(encoder)?;
        self.aux_inputs.encode(encoder)?;
        self.aux_outputs.encode(encoder)?;
        self.aux_output_count.encode(encoder)?;
        self.ports.encode(encoder)
    }
}

impl<Ctx> bincode::Decode<Ctx> for PluginInstance {
    fn decode<D: bincode::de::Decoder<Context = Ctx>>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        Ok(Self {
            id: bincode::Decode::decode(decoder)?,
            plugin_id: bincode::Decode::decode(decoder)?,
            format: bincode::Decode::decode(decoder)?,
            state: None,
            aux_inputs: bincode::Decode::decode(decoder)?,
            legacy_aux_sources: Vec::new(),
            aux_outputs: bincode::Decode::decode(decoder)?,
            aux_output_count: bincode::Decode::decode(decoder)?,
            ports: bincode::Decode::decode(decoder)?,
            ara_archive: None,
        })
    }
}

impl<'de, Ctx> bincode::BorrowDecode<'de, Ctx> for PluginInstance {
    fn borrow_decode<D: bincode::de::BorrowDecoder<'de, Context = Ctx>>(
        decoder: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        <Self as bincode::Decode<Ctx>>::decode(decoder)
    }
}

/// serde `skip_serializing_if` 用: `u32` が 0 か。`Clip::speaker_id`
/// の「未採番は serialize しない」に使う。
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// (talk) VOICEVOX 読み上げの全体スケール。`ClipContent::Text` clip が VOICEVOX
/// デバイス付きトラックに居るとき、その clip の全 `TextEvent` をこの 1 声・1 スケールで
/// 読み上げる。値は VOICEVOX `audio_query` 応答 JSON の同名フィールドへ patch してから
/// `/synthesis` に渡す (`docs/plan_voicevox_talk.md` §3.1)。VOICEVOX talk UI の
/// 話速 / 音高 / 抑揚 / 音量 に対応。`Clip::talk == None` は「全部既定」を意味する。
/// 声 (talk style) は別フィールド `Clip::speaker_id` を流用する (Text clip では talk
/// style id、MIDI clip では sing style id と解釈し、content 種別で分岐する)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct TalkParams {
    /// 話速 (speedScale)。`1.0` = 等速。VOICEVOX 推奨範囲 0.5..=2.0。
    pub speed_scale: f32,
    /// 音高 (pitchScale)。`0.0` = 既定。VOICEVOX 推奨範囲 -0.15..=0.15。
    pub pitch_scale: f32,
    /// 抑揚 (intonationScale)。`1.0` = 既定。`0.0` で棒読み。
    pub intonation_scale: f32,
    /// 音量 (volumeScale)。`1.0` = 等倍。
    pub volume_scale: f32,
}

impl Default for TalkParams {
    fn default() -> Self {
        Self {
            speed_scale: 1.0,
            pitch_scale: 0.0,
            intonation_scale: 1.0,
            volume_scale: 1.0,
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
    /// clip 全体のミュート (= MIDI / audio / video / image / 字幕 / 歌唱
    /// すべての content type 共通の clip-level mute の SSoT)。`true` で再生・書き出しから
    /// この clip を除外し、GUI は dim + 斜線ハッチで「ミュート中」を表示する。`q`
    /// ショートカット (選択 clip / カーソル直下 clip を toggle) と各 content inspector の
    /// "Mute" トグルがここを唯一の source として読み書きする。`Track.muted` とは独立で、
    /// 再生時は `track.muted || clip.muted` で合成される。v26 以前の per-event mute は
    /// `project::migrate_per_event_mute_to_clip_mute` で本フラグへ畳み込まれる。v26 以前は
    /// `false` に forward-migrate。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    /// この clip の VOICEVOX 歌唱声 = `/frame_synthesis` の speaker
    /// (= `/singers` の歌唱 style id)。clip 単位で独立・焼き込み (前の clip の
    /// 声を後で変えても後続に波及しない)。`0` = 未採番 (= 合成時に
    /// `voicevox::DEFAULT_SINGER_ID` へフォールバック)。vocal track 上の MIDI
    /// clip でのみ意味を持つ (他 content type では未使用)。旧プロジェクトは
    /// `project::load` の migration で旧トラック声を焼き込む。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub speaker_id: u32,
    /// 表示用キャラ名 (例: "中国うさぎ")。`/singers` 未取得でも
    /// inspector が現在の声を出せるよう焼き込む。空なら一覧取得後に
    /// `speaker_id` から逆引きして埋める。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub singer_name: String,
    /// 表示用スタイル名 (例: "ノーマル" / "へろへろ")。同上。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub style_name: String,
    /// (talk) VOICEVOX 読み上げの全体スケール (話速/音高/抑揚/音量)。
    /// `ClipContent::Text` clip が VOICEVOX デバイス付きトラックに居るときだけ意味を
    /// 持つ (`docs/plan_voicevox_talk.md`)。`None` = 全既定。声 (talk style) は
    /// `Clip::speaker_id` を流用する。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub talk: Option<TalkParams>,
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
// v30 (arch-refactor §10): 明示 `type` タグで variant を判別する (旧 `#[serde(untagged)]` は
// content 型数の 2 乗で silent-misparse リスクがあり、空 content が `{}` で型消失していた)。
// 旧 untagged ファイル (v<30) は `project.rs` の `migrate_clip_content_add_tag` が load 時に
// `type` を注入して変換する。bincode (IPC) は serde tag を無視し variant index を使うので
// wire 互換は不変。
#[serde(tag = "type")]
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
    /// overlay content (image / video / text) の末尾 (`event_start`
    /// 最大) event を、 その end が `clip_length_beats` に届くよう extend する
    /// (extend-only)。 単一 event なら clip 全長を覆い「clip 長 = 表示長」を保証。
    /// 縮めはしない (linked clip / `event > clip` の無害な不整合や多 event の
    /// 前方タイルは温存)。 Audio / Midi / Automation は時間軸 gate を持たないので
    /// no-op。
    pub fn ensure_event_covers_clip(&mut self, clip_length_beats: f64) {
        macro_rules! extend_last {
            ($events:expr) => {{
                if let Some(ev) = $events.iter_mut().max_by(|a, b| {
                    a.event_start_in_clip_beats
                        .total_cmp(&b.event_start_in_clip_beats)
                }) {
                    let needed = (clip_length_beats - ev.event_start_in_clip_beats).max(0.0);
                    if ev.event_length_beats < needed {
                        ev.event_length_beats = needed;
                    }
                }
            }};
        }
        match self {
            ClipContent::Image(c) => extend_last!(c.events),
            ClipContent::Video(c) => extend_last!(c.events),
            ClipContent::Text(c) => extend_last!(c.events),
            _ => {}
        }
    }

    /// v29: 要素 (note / audio event / automation point) の安定 id を採番
    /// する。 sentinel (0) のみ上書き、 既存非 0 id は counter を bump する
    /// だけ。 Video / Image / Text の event は単一 event 中心の運用で
    /// 選択集合を持たないため対象外。
    pub fn ensure_element_ids(&mut self) {
        fn alloc(id: &mut u32, next: &mut u32) {
            if *id == 0 {
                let new_id = (*next).max(1);
                *next = new_id + 1;
                *id = new_id;
            } else if *id >= *next {
                *next = *id + 1;
            }
        }
        match self {
            ClipContent::Midi(m) => {
                for n in &mut m.notes {
                    alloc(&mut n.id, &mut m.next_note_id);
                }
                if m.next_note_id == 0 {
                    m.next_note_id = 1;
                }
            }
            ClipContent::Audio(a) => {
                for e in &mut a.events {
                    alloc(&mut e.id, &mut a.next_event_id);
                }
                if a.next_event_id == 0 {
                    a.next_event_id = 1;
                }
            }
            ClipContent::Automation(a) => {
                for p in &mut a.points {
                    alloc(&mut p.id, &mut a.next_point_id);
                }
                if a.next_point_id == 0 {
                    a.next_point_id = 1;
                }
            }
            ClipContent::Video(_) | ClipContent::Image(_) | ClipContent::Text(_) => {}
        }
    }

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
pub struct MidiContent {
    /// Notes are in arbitrary order — readers that care about time
    /// order must sort by `Note::start_beat`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    /// v29: `Note::id` の per-content allocator。`0` は sentinel、`1` から。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub next_note_id: u32,
}

impl MidiContent {
    /// 新規 note 用の安定 id を採番する。
    pub fn alloc_note_id(&mut self) -> u32 {
        let id = self.next_note_id.max(1);
        self.next_note_id = id.saturating_add(1);
        id
    }
}

/// Audio clip content — an ordered list of audio events that play
/// within the clip. Bitwig "Clip ⊃ Audio Events" hierarchy
/// ([docs/plan_audio_clip.md](../../docs/plan_audio_clip.md)). Events
/// can overlap (mixed) or sit side by side; clip-internal layout is
/// defined by each event's `event_start_in_clip_beats` /
/// `event_length_beats`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AudioContent {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<AudioEvent>,
    /// v29: `AudioEvent::id` の per-content allocator。`0` は sentinel。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub next_event_id: u32,
}

impl AudioContent {
    /// 新規 audio event 用の安定 id を採番する。
    pub fn alloc_event_id(&mut self) -> u32 {
        let id = self.next_event_id.max(1);
        self.next_event_id = id.saturating_add(1);
        id
    }
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
    /// project FPS)。 **メタデータ専用** (import 時に記録、 情報表示・診断用)。
    /// フレーム選択は frame index でなく source の microsecond timestamp
    /// (`VideoEvent.source_*_micros` → MF / libav の **time-based seek**) で行い、
    /// decoder が PTS で正しいフレームを返すため、 VFR ソースでもフレームタイミングは
    /// 正しい (= この nominal FPS には依存しない)。 出力 export の刻みは別途
    /// `Song.video_framerate` (constant output FPS) を使う。
    /// (r.md #8 A7: コードを辿ると frame timing は時間ベースで VFR-correct。 旧
    /// 「MVP assumes CFR」 コメントは誤解だったので訂正。)
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
    /// v29: content 内で安定な event id (`AudioContent.next_event_id` 採番、
    /// `0` = 未採番 sentinel)。選択・undo 後の選択復元は positional index
    /// でなくこの id でアドレスする。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub id: u32,
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

    /// Auto-detected transient frame positions (`source_start_frames` 起点、
    /// `StretchMode::Slice` の slice trigger 位置)。 Slice 切替時に daw_gui が
    /// `common::onset::detect_onsets` で検出して埋める (r.md #8 B1)。 空 = 未検出で
    /// source 全体が 1 slice (= Raw 等価)。
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
            id: 0,
            source_id: 0,
            event_start_in_clip_beats: 0.0,
            event_length_beats: 0.0,
            source_start_frames: 0,
            source_end_frames: 0,
            gain_db: 0.0,
            pan: 0.0,
            pitch_semitones: 0.0,
            formant_semitones: 0.0,
            // 新規 audio clip は既定で tempo 追従 (= MIDI clip と同じく
            // project bpm 変更に拍を固定して伸縮、 ピッチ保持の granular)。
            // ワンショット等で追従させたくない場合は inspector の
            // stretch-mode セレクタで Raw に切り替える (= Bitwig Raw /
            // Ableton Warp-off 相当)。 enum `StretchMode::#[default]` は
            // Raw のまま (= このフィールドの deserialize default には
            // 使われず、 保存済みプロジェクトの mode は格納値を維持)。
            stretch_mode: StretchMode::Stretch,
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
    /// import 元ファイルの元の名前 (拡張子込み、 sanitize / content-hash
    /// 前)。 inspector / 口パク mapping ドロップダウン等、 image source を
    /// 直接列挙する UI の表示用 SSoT。 on-disk `path` は content addressing
    /// のため `<sanitized_stem>_<hash8>.<ext>` に変形され、 日本語名は
    /// `_` に潰れて区別不能になるので、 元名はここに別途保持する。 v21
    /// 以前の `.daw` は未保持なので `#[serde(default)]` で空文字になり、
    /// consumer は空なら `path.file_name()` に fallback する。
    #[serde(default)]
    pub name: String,
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
    /// v29: content 内で安定な note id (`MidiContent.next_note_id` 採番、`0`
    /// = 未採番 sentinel — `ClipContent::ensure_element_ids` が load 時に
    /// 採番)。選択・undo 後の選択復元は positional index でなくこの id で
    /// アドレスする。linked clip は content を共有するので id も共有される。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub id: u32,
    pub start_beat: f64,
    pub duration_beats: f64,
    pub pitch: u8,
    pub velocity: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lyric: Option<String>,
    /// note 単位のミュート。`true` でこの note を再生・書き出しから除外し
    /// (歌唱 note も含む)、piano roll は dim + 斜線ハッチで「ミュート中」を表示する。`q`
    /// ショートカット (選択 note / カーソル直下 note を toggle) が読み書きする。linked clip は
    /// content (= notes) を共有するので、note mute も linked clip 間で共有される。v26 以前は
    /// `false` に forward-migrate。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
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
    /// Plugin parameter on this track. `device_id` は安定 device id
    /// (`PluginInstance::id`)。`param_id` is the CLAP `clap_id` / VST3
    /// `ParamID` (both `u32`); the format is recovered by resolving
    /// `device_id` in the track's `devices` chain.
    PluginParam {
        /// v29: 安定 device id。`0` は未解決 sentinel (ensure_ids が写像)。
        #[serde(default)]
        device_id: u64,
        param_id: u32,
        /// v28 以前 migration 用 (deserialize 専用)。旧 save は chain 内
        /// positional index を持つ。`Song::ensure_ids` が device_id へ写像。
        #[serde(default, rename = "device_index", skip_serializing)]
        legacy_device_index: Option<u32>,
        /// v22 以前 migration 用 (deserialize 専用)。旧 save は `slot` を持つ。
        #[serde(default, rename = "slot", skip_serializing)]
        legacy_slot: Option<crate::protocol::PluginSlot>,
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
    /// Aux send level (linear, `0.0..=2.0`) for the send with stable id
    /// `send_id` inside this track's `sends`. v29 で positional `send_idx`
    /// から安定 id に移行 — 旧 file の index は `legacy_send_idx` に載り
    /// `Song::ensure_ids` が id へ写像する。See `Send` /
    /// `docs/plan_routing_graph.md`.
    SendGain {
        #[serde(default)]
        send_id: u32,
        /// v28 以前 migration 用 (deserialize 専用)。
        #[serde(default, rename = "send_idx", skip_serializing)]
        legacy_send_idx: Option<u8>,
    },
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
    /// v29: content 内で安定な point id (`AutomationContent.next_point_id`
    /// 採番、`0` = 未採番 sentinel)。選択・undo 後の選択復元は positional
    /// index でなくこの id でアドレスする。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub id: u32,
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
            id: 0,
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
pub struct AutomationContent {
    /// Sorted by `AutomationPoint::time_beat` ascending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub points: Vec<AutomationPoint>,
    /// v29: `AutomationPoint::id` の per-content allocator。`0` は sentinel。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub next_point_id: u32,
}

impl AutomationContent {
    /// 新規 automation point 用の安定 id を採番する。
    pub fn alloc_point_id(&mut self) -> u32 {
        let id = self.next_point_id.max(1);
        self.next_point_id = id.saturating_add(1);
        id
    }
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
    // NOTE: モジュレーション routing は lane を離れ `Track.mod_routings` /
    // `Song.song_mod_routings` に移動した (`docs/plan_modulation_routing_redesign.md`)。
    // 旧 file の `mod_routings` キーは serde の unknown-field 無視で読み捨てられる。
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
        self.next_clip_id = id.saturating_add(1);
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

/// Address of a regular (MIDI / Audio / Video / Image / Text) clip inside the
/// song: owning `Track::id` + `Clip::id`. Index 非依存なので track 並べ替え /
/// undo を跨いでも同じ clip を指す (= 選択 SSoT 用)。 フィールド名を
/// `track_id` / `clip_id` にしてあるのは、 index ベースの
/// `ClipRef { track, clip }` (daw_gui) と取り違えた誤用 (`key.track as usize`
/// で index 化する事故) を compile error で弾くため。 gui_01::ClipKey
/// (widget 層、 serde/bincode 無し) とは別物。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Encode, Decode,
)]
pub struct ClipKey {
    pub track_id: u32,
    pub clip_id: u32,
}

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

    // ---- Arranger Section model ----

    fn mk_section(id: u32, start: f64, len: f64) -> Section {
        Section {
            id,
            name: format!("S{id}"),
            color: [0.5, 0.5, 0.5],
            start_beat: start,
            len_beats: len,
        }
    }

    #[test]
    fn section_end_beat_is_start_plus_len() {
        assert_eq!(mk_section(1, 4.0, 8.0).end_beat(), 12.0);
    }

    #[test]
    fn alloc_section_id_skips_zero_and_increments() {
        let mut song = Song::default();
        assert_eq!(song.alloc_section_id(), 1);
        assert_eq!(song.alloc_section_id(), 2);
        // `0` sentinel が入っていても 1 から採番。
        song.next_section_id = 0;
        assert_eq!(song.alloc_section_id(), 1);
    }

    #[test]
    fn normalize_sections_sorts_disjoint_by_start() {
        let mut song = Song {
            sections: vec![mk_section(1, 8.0, 4.0), mk_section(2, 0.0, 4.0), mk_section(3, 4.0, 4.0)],
            ..Default::default()
        };
        song.normalize_sections();
        let ids: Vec<u32> = song.sections.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![2, 3, 1]);
        assert_eq!(song.sections[0].start_beat, 0.0);
        assert_eq!(song.sections[0].len_beats, 4.0);
    }

    #[test]
    fn normalize_sections_resolves_overlap_by_clamping_later_start() {
        // [0,6) と [4,8) が重複 → 後発の start を直前 end(6) までクランプ → [6,8)。
        let mut song = Song {
            sections: vec![mk_section(1, 0.0, 6.0), mk_section(2, 4.0, 4.0)],
            ..Default::default()
        };
        song.normalize_sections();
        assert_eq!(song.sections.len(), 2);
        assert_eq!((song.sections[0].start_beat, song.sections[0].len_beats), (0.0, 6.0));
        assert_eq!((song.sections[1].start_beat, song.sections[1].len_beats), (6.0, 2.0));
    }

    #[test]
    fn normalize_sections_drops_zero_negative_and_fully_overlapped() {
        let mut song = Song {
            sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 0.0), mk_section(3, 8.0, -1.0)],
            ..Default::default()
        };
        song.normalize_sections();
        assert_eq!(song.sections.iter().map(|s| s.id).collect::<Vec<_>>(), vec![1]);

        // [0,10) が [2,4) を完全に覆う → 後発は len 0 になり drop。
        let mut song = Song {
            sections: vec![mk_section(1, 0.0, 10.0), mk_section(2, 2.0, 2.0)],
            ..Default::default()
        };
        song.normalize_sections();
        assert_eq!(song.sections.len(), 1);
        assert_eq!(song.sections[0].id, 1);
    }

    #[test]
    fn section_survives_json_roundtrip() {
        let s = mk_section(7, 12.0, 4.0);
        assert_eq!(json_roundtrip(&s), s);
    }

    #[test]
    fn ripple_timeline_open_shifts_everything_after_from_beat_right() {
        let mut song = Song {
            length_beats: 16.0,
            loop_start_beat: 8.0,
            loop_end_beat: 12.0,
            sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 8.0, 4.0)],
            tracks: vec![Track {
                clips: vec![
                    Clip { start_beat: 0.0, length_beats: 4.0, ..Default::default() },
                    Clip { start_beat: 8.0, length_beats: 4.0, ..Default::default() },
                ],
                ..Track::default()
            }],
            ..Default::default()
        };

        // beat 4 に 4 拍挿入 (open) → `>= 4` が +4。
        song.ripple_timeline(4.0, 4.0);

        assert_eq!(song.tracks[0].clips[0].start_beat, 0.0); // 4 未満は不変
        assert_eq!(song.tracks[0].clips[1].start_beat, 12.0); // 8 → 12
        assert_eq!(song.sections[0].start_beat, 0.0);
        assert_eq!(song.sections[1].start_beat, 12.0);
        assert_eq!((song.loop_start_beat, song.loop_end_beat), (12.0, 16.0));
        assert_eq!(song.length_beats, 20.0);
    }

    #[test]
    fn move_section_reorders_content_with_ripple() {
        let mut song = Song {
            length_beats: 12.0,
            sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0), mk_section(3, 8.0, 4.0)],
            tracks: vec![Track {
                id: 1,
                clips: vec![
                    Clip { id: 1, start_beat: 0.0, length_beats: 4.0, ..Default::default() }, // Intro
                    Clip { id: 2, start_beat: 4.0, length_beats: 4.0, ..Default::default() }, // Verse
                    Clip { id: 3, start_beat: 8.0, length_beats: 4.0, ..Default::default() }, // Chorus
                ],
                ..Track::default()
            }],
            ..Default::default()
        };

        // Chorus ([8,12), id=3) を Verse の前 (dest=4) へ移動。
        assert!(song.move_section(3, 4.0));

        // セクションは Intro[0,4) / Chorus[4,8) / Verse[8,12) に組み替わる (start 昇順)。
        let secs: Vec<(u32, f64, f64)> =
            song.sections.iter().map(|s| (s.id, s.start_beat, s.len_beats)).collect();
        assert_eq!(secs, vec![(1, 0.0, 4.0), (3, 4.0, 4.0), (2, 8.0, 4.0)]);

        // clip も帯に追従: clip1→0, clip3→4, clip2→8。
        let mut clips: Vec<(u32, f64)> =
            song.tracks[0].clips.iter().map(|c| (c.id, c.start_beat)).collect();
        clips.sort_by_key(|c| c.0);
        assert_eq!(clips, vec![(1, 0.0), (2, 8.0), (3, 4.0)]);
    }

    #[test]
    fn duplicate_section_inserts_linked_copy_with_ripple() {
        let mut song = Song {
            length_beats: 8.0,
            sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0)],
            next_section_id: 3, // 既存 id 1,2 の続き (実運用は alloc 経由で採番される)
            tracks: vec![Track {
                id: 1,
                clips: vec![
                    Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: 7, ..Default::default() },
                    Clip { id: 2, start_beat: 4.0, length_beats: 4.0, content_id: 8, ..Default::default() },
                ],
                next_clip_id: 3,
                ..Track::default()
            }],
            ..Default::default()
        };

        // section1 ([0,4), content_id 7) を 2 つの間 (dest=4) に複製。
        let new_id = song.duplicate_section(1, 4.0).unwrap();
        assert_eq!(new_id, 3);

        // section1[0,4) / copy[4,8) / section2[8,12) の 3 つ。
        assert_eq!(song.sections.len(), 3);
        let mut starts: Vec<f64> = song.sections.iter().map(|s| s.start_beat).collect();
        starts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(starts, vec![0.0, 4.0, 8.0]);

        // clip: 元 clip1(0,c7) / 複製(4,c7 linked) / 元 clip2 は 4→8 へ ripple(8,c8)。
        let mut clips: Vec<(f64, u32)> =
            song.tracks[0].clips.iter().map(|c| (c.start_beat, c.content_id)).collect();
        clips.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert_eq!(clips, vec![(0.0, 7), (4.0, 7), (8.0, 8)]);
        // 複製は新しい clip id (>= 3)。
        assert!(song.tracks[0].clips.iter().any(|c| c.start_beat == 4.0 && c.id >= 3));
    }

    #[test]
    fn split_clips_at_forks_straddling_clip_content() {
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![
                    Note { id: 1, start_beat: 0.0, duration_beats: 1.0, pitch: 60, velocity: 100, lyric: None, muted: false },
                    Note { id: 2, start_beat: 3.0, duration_beats: 1.0, pitch: 62, velocity: 100, lyric: None, muted: false },
                ],
                next_note_id: 3,
            }),
        );
        let track = Track {
            id: 1,
            clips: vec![Clip { id: 1, start_beat: 2.0, length_beats: 4.0, content_id: cid, ..Default::default() }],
            next_clip_id: 2,
            ..Track::default()
        };
        song.tracks = vec![track];

        // clip [2,6) を beat 4 (clip-local 2.0) で分割。
        song.split_clips_at(4.0);

        let mut cs: Vec<(f64, f64, u32)> = song.tracks[0]
            .clips
            .iter()
            .map(|c| (c.start_beat, c.length_beats, c.content_id))
            .collect();
        cs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert_eq!(cs.len(), 2);
        // 左 [2,4) は元 content を保持、 右 [4,6) は fork content。
        assert_eq!((cs[0].0, cs[0].1, cs[0].2), (2.0, 2.0, cid));
        assert_eq!((cs[1].0, cs[1].1), (4.0, 2.0));
        let right_cid = cs[1].2;
        assert_ne!(right_cid, cid);

        // 右 content の note は cut(2.0) 左シフト (3.0→1.0)、 0.0 の note は drop。
        let ClipContent::Midi(m) = &song.clip_contents[&right_cid] else {
            panic!("midi")
        };
        assert_eq!(m.notes.len(), 1);
        assert_eq!(m.notes[0].start_beat, 1.0);
        assert_eq!(m.notes[0].pitch, 62);

        // 元 content は pooled で不変 (2 note のまま)。
        let ClipContent::Midi(orig) = &song.clip_contents[&cid] else {
            panic!("midi")
        };
        assert_eq!(orig.notes.len(), 2);
    }

    #[test]
    fn delete_section_removes_band_only_keeping_content() {
        let mut song = Song {
            sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0)],
            tracks: vec![Track {
                id: 1,
                clips: vec![Clip { id: 1, start_beat: 0.0, length_beats: 4.0, ..Default::default() }],
                ..Track::default()
            }],
            ..Default::default()
        };
        assert!(song.delete_section(1));
        assert_eq!(song.sections.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(song.tracks[0].clips.len(), 1); // 内容は温存
        assert!(!song.delete_section(999)); // 不在は false
    }

    #[test]
    fn delete_section_range_removes_content_and_ripples() {
        let mut song = Song {
            length_beats: 12.0,
            sections: vec![mk_section(1, 0.0, 4.0), mk_section(2, 4.0, 4.0), mk_section(3, 8.0, 4.0)],
            tracks: vec![Track {
                id: 1,
                clips: vec![
                    Clip { id: 1, start_beat: 0.0, length_beats: 4.0, ..Default::default() },
                    Clip { id: 2, start_beat: 4.0, length_beats: 4.0, ..Default::default() },
                    Clip { id: 3, start_beat: 8.0, length_beats: 4.0, ..Default::default() },
                ],
                ..Track::default()
            }],
            ..Default::default()
        };
        // 真ん中 section2 [4,8) を範囲ごと削除 → clip2 消滅、 clip3 と section3 が 8→4 へ詰まる。
        assert!(song.delete_section_range(2));
        let mut secs: Vec<(u32, f64)> = song.sections.iter().map(|s| (s.id, s.start_beat)).collect();
        secs.sort_by_key(|s| s.0);
        assert_eq!(secs, vec![(1, 0.0), (3, 4.0)]);
        let mut clips: Vec<(u32, f64)> =
            song.tracks[0].clips.iter().map(|c| (c.id, c.start_beat)).collect();
        clips.sort_by_key(|c| c.0);
        assert_eq!(clips, vec![(1, 0.0), (3, 4.0)]);
        assert_eq!(song.length_beats, 8.0);
    }

    #[test]
    fn move_section_noop_when_dest_equals_start() {
        let mut song = Song {
            sections: vec![mk_section(1, 0.0, 4.0)],
            ..Default::default()
        };
        assert!(!song.move_section(1, 0.0));
        assert!(!song.move_section(99, 8.0)); // 存在しない id
    }

    #[test]
    fn ripple_timeline_close_shifts_left_and_shrinks_length() {
        let mut song = Song {
            length_beats: 16.0,
            tracks: vec![Track {
                clips: vec![Clip { start_beat: 12.0, length_beats: 4.0, ..Default::default() }],
                ..Track::default()
            }],
            ..Default::default()
        };

        // beat 8 以降を 4 拍詰める (close) → `>= 8` が -4。
        song.ripple_timeline(8.0, -4.0);

        assert_eq!(song.tracks[0].clips[0].start_beat, 8.0); // 12 → 8
        assert_eq!(song.length_beats, 12.0);
    }

    #[test]
    fn ensure_image_video_event_coverage_extends_short_event() {
        // clip=48 / image event=32 → event を 48 まで extend して
        // clip 範囲内で途中消失しないようにする (= 既存 .daw の load 修復)。
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Image(ImageContent {
                events: vec![ImageEvent {
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 32.0,
                    ..ImageEvent::default()
                }],
            }),
        );
        let tid = song.alloc_track_id();
        let mut track = Track { id: tid, ..Track::default() };
        let clip_id = track.alloc_clip_id();
        track.clips.push(Clip {
            id: clip_id,
            start_beat: 0.0,
            length_beats: 48.0,
            content_id: cid,
            ..Clip::default()
        });
        song.tracks.push(track);

        song.ensure_overlay_event_coverage();
        let ClipContent::Image(c) = &song.clip_contents[&cid] else {
            panic!("expected image content");
        };
        assert_eq!(c.events[0].event_length_beats, 48.0);

        // idempotent: 2 回目で値は変わらない。
        song.ensure_overlay_event_coverage();
        let ClipContent::Image(c) = &song.clip_contents[&cid] else {
            panic!("expected image content");
        };
        assert_eq!(c.events[0].event_length_beats, 48.0);
    }

    #[test]
    fn ensure_image_video_event_coverage_extend_only_across_linked_clips() {
        // 同 content を len 8 と len 48 の 2 clip が共有 → 最長 48 まで extend。
        // 短い clip は自分の clip 範囲 gate で clamp されるので event は縮めない。
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Image(ImageContent {
                events: vec![ImageEvent {
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 4.0,
                    ..ImageEvent::default()
                }],
            }),
        );
        let tid = song.alloc_track_id();
        let mut track = Track { id: tid, ..Track::default() };
        for (start, len) in [(0.0_f64, 8.0_f64), (16.0, 48.0)] {
            let clip_id = track.alloc_clip_id();
            track.clips.push(Clip {
                id: clip_id,
                start_beat: start,
                length_beats: len,
                content_id: cid,
                ..Clip::default()
            });
        }
        song.tracks.push(track);

        song.ensure_overlay_event_coverage();
        let ClipContent::Image(c) = &song.clip_contents[&cid] else {
            panic!("expected image content");
        };
        assert_eq!(c.events[0].event_length_beats, 48.0);
    }

    #[test]
    fn ensure_overlay_event_coverage_extends_text_clip() {
        // (Text 版): クレジット text clip @0+48 だが event_length=4 →
        // bar2 (beat4) で event 範囲を抜けて消える。event を 48 まで extend する。
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Text(TextContent {
                events: vec![TextEvent {
                    text: "クレジット".into(),
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 4.0,
                    ..TextEvent::default()
                }],
            }),
        );
        let tid = song.alloc_track_id();
        let mut track = Track { id: tid, ..Track::default() };
        let clip_id = track.alloc_clip_id();
        track.clips.push(Clip {
            id: clip_id,
            start_beat: 0.0,
            length_beats: 48.0,
            content_id: cid,
            ..Clip::default()
        });
        song.tracks.push(track);

        song.ensure_overlay_event_coverage();
        let ClipContent::Text(c) = &song.clip_contents[&cid] else {
            panic!("expected text content");
        };
        assert_eq!(c.events[0].event_length_beats, 48.0);
    }

    #[test]
    fn song_default_roundtrip() {
        let song = Song::default();
        assert_eq!(json_roundtrip(&song), song);
    }

    /// `track_visually_silenced` は audio engine の effective-mute と同じ意味論を
    /// video 層に再現する: グループ親の mute は subtree を隠し、 solo は audio と
    /// 一致 (グループを solo すると非 solo の子は隠れる / 子を solo すると親 group
    /// は見える)。
    #[test]
    fn track_visually_silenced_mute_and_solo_semantics() {
        // t10 = group, t11 = t10 の子, t12 = 独立。
        let mk = |id: u32, parent: Option<u32>| Track {
            id,
            parent_group_id: parent,
            ..Track::default()
        };
        let mut song = Song {
            tracks: vec![mk(10, None), mk(11, Some(10)), mk(12, None)],
            ..Default::default()
        };

        // baseline: 何も silenced でない。
        assert!(!song.track_visually_silenced(11));
        assert!(!song.track_visually_silenced(12));

        // グループ (祖先) の mute が子を隠す。
        song.track_by_id_mut(10).unwrap().muted = true;
        assert!(song.track_visually_silenced(11), "child of muted group hidden");
        assert!(!song.track_visually_silenced(12), "unrelated track unaffected");
        song.track_by_id_mut(10).unwrap().muted = false;

        // 自身の mute。
        song.track_by_id_mut(12).unwrap().muted = true;
        assert!(song.track_visually_silenced(12));
        song.track_by_id_mut(12).unwrap().muted = false;

        // leaf を solo: それだけ可視、 他は隠れる。
        song.track_by_id_mut(12).unwrap().solo = true;
        assert!(!song.track_visually_silenced(12), "soloed track visible");
        assert!(song.track_visually_silenced(11), "non-soloed hidden under solo");
        song.track_by_id_mut(12).unwrap().solo = false;

        // グループを solo: 配下の子も可視 (folder solo、 Ableton/Reaper 準拠)。
        song.track_by_id_mut(10).unwrap().solo = true;
        assert!(!song.track_visually_silenced(10), "soloed group itself visible");
        assert!(
            !song.track_visually_silenced(11),
            "child of soloed group visible (folder solo)"
        );
        assert!(song.track_visually_silenced(12), "unrelated hidden under solo");
        assert!(song.ancestor_soloed(11), "child sees soloed ancestor group");
        assert!(!song.ancestor_soloed(12), "unrelated has no soloed ancestor");
        song.track_by_id_mut(10).unwrap().solo = false;

        // 子を solo: その祖先 group は可視のまま (has_soloed_contributor 相当)。
        song.track_by_id_mut(11).unwrap().solo = true;
        assert!(!song.track_visually_silenced(11), "soloed child visible");
        assert!(!song.track_visually_silenced(10), "ancestor of soloed child visible");
        assert!(song.track_visually_silenced(12), "unrelated hidden under solo");
    }

    /// Regression test for sidechain pipeline: when `ensure_ids()` rewrites
    /// a `track.id == 0` sentinel into a fresh id, every reference to that
    /// old id (= `aux_inputs` tap source + `parent_group_id`) must be
    /// remapped too. Otherwise the references dangle, `compile_schedule`
    /// silently skips them (treating dangling sidechain sources as
    /// `continue`), and the user sees no sidechain signal even though the
    /// dropdown is wired correctly.
    ///
    /// Setup:
    ///   Track Kick id=0 (sentinel) → after ensure_ids gets id=2
    ///   Track Bass id=1 with fx[0].aux_inputs=[post_fader(0)] (= Kick)
    ///                    parent_group_id = Some(0) (= Kick)
    /// Expected after ensure_ids:
    ///   Bass.fx[0].aux_inputs == [post_fader(2)]
    ///   Bass.parent_group_id == Some(2)
    #[test]
    fn ensure_ids_remaps_aux_inputs_and_parent_group_id() {
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
                    // v23: 役割別 chain は廃止。device chain (`devices`) に直接置く。
                    devices: vec![PluginInstance {
                        // points at Kick (sentinel id 0)
                        aux_inputs: vec![Some(AuxInputRoute::post_fader(0))],
                        ..PluginInstance::with_ports(
                            "test.compressor".into(),
                            PluginFormat::Vst3,
                            crate::port_config::PortConfig::default(),
                        )
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
            bass.devices[0].aux_inputs,
            vec![Some(AuxInputRoute::post_fader(new_kick_id))],
            "aux_inputs tap pointing at sentinel must be remapped to the new id"
        );
    }

    /// パラアウト (docs/plan_paraout.md): a plugin's `aux_outputs` destination
    /// pointing at a sentinel id (0) must be remapped by `ensure_ids` to the
    /// child's freshly assigned id — the symmetric counterpart of the
    /// `aux_inputs` remap above. Without this, a saved project's parallel-out
    /// routing breaks the moment ids are rebased on load.
    #[test]
    fn ensure_ids_remaps_aux_outputs_dest() {
        use crate::plugin_format::PluginFormat;

        let mut song = Song {
            bpm: 120.0,
            time_sig: (4, 4),
            length_beats: 64.0,
            tracks: vec![
                Track {
                    id: 0, // sentinel — becomes the new child id
                    name: "Snare".into(),
                    ..Track::default()
                },
                Track {
                    id: 1,
                    name: "Drums".into(),
                    devices: vec![PluginInstance {
                        // aux output routed at Snare (sentinel id 0)
                        aux_outputs: vec![Some(AuxOutputRoute::to_track(0))],
                        aux_output_count: 1,
                        ..PluginInstance::with_ports(
                            "test.drum_sampler".into(),
                            PluginFormat::Clap,
                            crate::port_config::PortConfig::default(),
                        )
                    }],
                    ..Track::default()
                },
            ],
            next_track_id: 2,
            ..Song::default()
        };

        song.ensure_ids();

        let snare_id = song.tracks[0].id;
        assert_ne!(snare_id, 0, "ensure_ids should replace sentinel id 0");
        assert_eq!(
            song.tracks[1].devices[0].aux_outputs,
            vec![Some(AuxOutputRoute::to_track(snare_id))],
            "aux_outputs dest pointing at sentinel must be remapped to the new id"
        );
    }

    /// パラアウト (docs/plan_paraout.md): aux_outputs / aux_output_count は JSON
    /// (セーブファイル) と bincode (IPC) の両方で往復しても保持される。 None
    /// (未振分けポート) を挟んだ疎なルートも壊れないこと。
    #[test]
    fn plugin_instance_aux_outputs_survive_json_and_bincode_round_trip() {
        use crate::plugin_format::PluginFormat;

        let inst = PluginInstance {
            aux_outputs: vec![
                Some(AuxOutputRoute::to_track(7)),
                None,
                Some(AuxOutputRoute::to_track(9)),
            ],
            aux_output_count: 3,
            ..PluginInstance::new("test.drum_sampler".into(), PluginFormat::Clap)
        };

        // JSON (save file)
        let via_json = json_roundtrip(&inst);
        assert_eq!(via_json.aux_outputs, inst.aux_outputs);
        assert_eq!(via_json.aux_output_count, 3);

        // bincode (IPC)
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(&inst, cfg).unwrap();
        let (via_bincode, _): (PluginInstance, usize) =
            bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(via_bincode.aux_outputs, inst.aux_outputs);
        assert_eq!(via_bincode.aux_output_count, 3);
    }

    /// パラアウト (docs/plan_paraout.md): aux_outputs / aux_output_count フィールドを
    /// 持たない旧セーブファイルは `#[serde(default)]` で空にフォワード migrate
    /// される (機能追加前の .daw が壊れない)。
    #[test]
    fn plugin_instance_without_aux_output_fields_forward_migrates_to_empty() {
        use crate::plugin_format::PluginFormat;

        // aux_outputs を持つ instance を JSON 化 → aux_output 系キーを削除して
        // 旧形式を模し → deserialize で空に migrate されることを確認。
        let inst = PluginInstance {
            aux_outputs: vec![Some(AuxOutputRoute::to_track(7))],
            aux_output_count: 2,
            ..PluginInstance::new("test.drum_sampler".into(), PluginFormat::Clap)
        };
        let mut v = serde_json::to_value(&inst).unwrap();
        let obj = v.as_object_mut().unwrap();
        assert!(
            obj.contains_key("aux_outputs"),
            "non-empty aux_outputs must serialize (skip_serializing_if guards only the empty case)"
        );
        obj.remove("aux_outputs");
        obj.remove("aux_output_count");

        let migrated: PluginInstance = serde_json::from_value(v).unwrap();
        assert!(
            migrated.aux_outputs.is_empty(),
            "missing aux_outputs field must forward-migrate to empty"
        );
        assert_eq!(
            migrated.aux_output_count, 0,
            "missing aux_output_count field must forward-migrate to 0"
        );
    }

    /// docs/plan_modulation.md §8: a v-old project that stored sidechain
    /// wiring under the legacy `sidechain_sources: Vec<Option<u32>>` JSON key
    /// must `ensure_ids`-migrate into `aux_inputs` as `PostFader` taps, with
    /// the legacy field drained. No track sentinel here, so the migration
    /// must run even when the id-remap pass is a no-op.
    #[test]
    fn ensure_ids_migrates_legacy_sidechain_sources_to_aux_inputs() {
        use crate::plugin_format::PluginFormat;

        let mut song = Song {
            tracks: vec![
                Track {
                    id: 1,
                    name: "Kick".into(),
                    ..Track::default()
                },
                Track {
                    id: 2,
                    name: "Bass".into(),
                    devices: vec![PluginInstance {
                        // emulate a deserialized old project: legacy field set,
                        // aux_inputs empty.
                        legacy_aux_sources: vec![Some(1), None],
                        ..PluginInstance::with_ports(
                            "test.compressor".into(),
                            PluginFormat::Vst3,
                            crate::port_config::PortConfig::default(),
                        )
                    }],
                    ..Track::default()
                },
            ],
            next_track_id: 3,
            ..Song::default()
        };

        song.ensure_ids();

        let bass = &song.tracks[1];
        assert_eq!(
            bass.devices[0].aux_inputs,
            vec![Some(AuxInputRoute::post_fader(1)), None],
            "legacy sidechain_sources must lift to PostFader aux_inputs"
        );
        assert!(
            bass.devices[0].legacy_aux_sources.is_empty(),
            "legacy field must be drained after migration"
        );
    }

    /// docs/plan_modulation.md §8: `mod_sources` get stable ids assigned
    /// (sentinel 0 → fresh) and their `tap.source_track` follows a track id
    /// remap, exactly like `aux_inputs` taps.
    #[test]
    fn ensure_ids_assigns_mod_source_ids_and_remaps_tap() {
        let mut song = Song {
            tracks: vec![
                Track {
                    id: 0, // sentinel — rebased by ensure_ids
                    name: "Kick".into(),
                    ..Track::default()
                },
                Track {
                    id: 1,
                    name: "Bass".into(),
                    ..Track::default()
                },
            ],
            next_track_id: 2,
            mod_sources: vec![ModSource {
                id: 0, // sentinel — assigned by ensure_ids
                owner_track_id: 0,
                color: ModSource::palette_color(0),
                kind: ModSourceKind::EnvelopeFollower {
                    tap: AudioTap::post_fader(0), // points at Kick's sentinel id
                    follower: FollowerConfig::default(),
                },
            }],
            ..Song::default()
        };

        song.ensure_ids();

        let new_kick_id = song.tracks[0].id;
        assert_ne!(new_kick_id, 0, "Kick sentinel rebased");
        assert_ne!(song.mod_sources[0].id, 0, "mod_source id assigned");
        assert_eq!(
            song.mod_sources[0].follower().unwrap().0.source_track,
            new_kick_id,
            "mod_source tap.source_track must follow the track id remap"
        );
    }

    /// v23 migration: a v22 track with the legacy role-keyed chains
    /// (`midi_fx_chain` / `instrument` / `fx_chain`) flattens into a single
    /// `devices` Vec in `midi_fx ++ instrument? ++ fx` order, and the
    /// automation lanes' legacy `slot` remaps to the equivalent
    /// `device_index`. `ensure_ids` drives the flattening (via
    /// `flatten_legacy_devices`) before the id pass.
    #[test]
    fn ensure_ids_flattens_legacy_chains_and_remaps_lane_slots() {
        use crate::plugin_format::PluginFormat;
        use crate::protocol::PluginSlot;

        let plug = |id: &str| PluginInstance::new(id.into(), PluginFormat::Clap);
        // Layout: 2 MIDI FX, 1 instrument, 2 audio FX.
        //   index:  0=arp 1=quant | 2=synth | 3=comp 4=reverb
        let mut track = Track {
            id: 1,
            name: "Lead".into(),
            legacy_midi_fx_chain: vec![plug("arp"), plug("quant")],
            legacy_instrument: Some(plug("synth")),
            legacy_fx_chain: vec![plug("comp"), plug("reverb")],
            automation_lanes: vec![
                AutomationLane {
                    id: 1,
                    ..AutomationLane::new(
                        AutomationTarget::PluginParam {
                            device_id: 0, // resolved via legacy_slot → remap pass
                            param_id: 5,
                            legacy_device_index: None,
                            legacy_slot: Some(PluginSlot::Instrument),
                        },
                        0.0,
                    )
                },
                AutomationLane {
                    id: 2,
                    ..AutomationLane::new(
                        AutomationTarget::PluginParam {
                            device_id: 0,
                            param_id: 9,
                            legacy_device_index: None,
                            legacy_slot: Some(PluginSlot::Fx(1)),
                        },
                        0.0,
                    )
                },
                AutomationLane {
                    id: 3,
                    ..AutomationLane::new(
                        AutomationTarget::PluginParam {
                            device_id: 0,
                            param_id: 2,
                            legacy_device_index: None,
                            legacy_slot: Some(PluginSlot::MidiFx(1)),
                        },
                        0.0,
                    )
                },
            ],
            next_lane_id: 4,
            ..Track::default()
        };
        // Pre-condition: nothing in `devices` yet.
        assert!(track.devices.is_empty());

        let mut song = Song {
            tracks: vec![std::mem::take(&mut track)],
            next_track_id: 2,
            ..Song::default()
        };
        song.ensure_ids();

        let t = &song.tracks[0];
        // Flattened order: midi_fx ++ instrument ++ fx.
        let ids: Vec<&str> = t.devices.iter().map(|p| p.plugin_id.as_str()).collect();
        assert_eq!(ids, vec!["arp", "quant", "synth", "comp", "reverb"]);
        // Legacy fields are drained.
        assert!(t.legacy_midi_fx_chain.is_empty());
        assert!(t.legacy_instrument.is_none());
        assert!(t.legacy_fx_chain.is_empty());

        // v29: lane slot → (index →) 安定 device_id まで ensure_ids が写像
        // する。 Instrument=index2, Fx(1)=index4, MidiFx(1)=index1 の device の
        // 採番済み id と一致すること。
        let device_id_of = |lane_id: u32| -> u64 {
            match t.lane_by_id(lane_id).unwrap().target {
                AutomationTarget::PluginParam {
                    device_id,
                    legacy_device_index,
                    legacy_slot,
                    ..
                } => {
                    assert!(legacy_slot.is_none(), "legacy_slot must be consumed");
                    assert!(
                        legacy_device_index.is_none(),
                        "legacy_device_index must be consumed"
                    );
                    device_id
                }
                _ => panic!("expected PluginParam"),
            }
        };
        assert_ne!(t.devices[2].id, 0, "devices must be assigned stable ids");
        assert_eq!(device_id_of(1), t.devices[2].id, "Instrument → n_midi (=2)");
        assert_eq!(device_id_of(2), t.devices[4].id, "Fx(1) → n_midi + has_inst + 1 (=4)");
        assert_eq!(device_id_of(3), t.devices[1].id, "MidiFx(1) → 1");
    }

    /// v23 migration is a no-op when `devices` is already populated (new
    /// format): legacy fields are empty and lane device_index is untouched.
    #[test]
    fn flatten_legacy_devices_is_noop_for_new_format() {
        use crate::plugin_format::PluginFormat;

        let mut track = Track {
            id: 1,
            devices: vec![PluginInstance::new("synth".into(), PluginFormat::Clap)],
            automation_lanes: vec![AutomationLane {
                id: 1,
                ..AutomationLane::new(
                    AutomationTarget::PluginParam {
                        device_id: 7,
                        param_id: 3,
                        legacy_device_index: None,
                        legacy_slot: None,
                    },
                    0.0,
                )
            }],
            next_lane_id: 2,
            ..Track::default()
        };
        track.flatten_legacy_devices();
        assert_eq!(track.devices.len(), 1);
        assert_eq!(track.devices[0].plugin_id, "synth");
        match track.automation_lanes[0].target {
            AutomationTarget::PluginParam { device_id, .. } => {
                assert_eq!(device_id, 7, "new-format device_id must be untouched");
            }
            _ => panic!("expected PluginParam"),
        }
    }

    #[test]
    fn project_file_roundtrip() {
        let pf = ProjectFile {
            version: CURRENT_VERSION,
            song: Song::default(),
            view: None,
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
            id: 0,
            start_beat: 0.5,
            duration_beats: 1.0,
            pitch: 60,
            velocity: 100,
            lyric: Some("こ".into()),
            muted: false,
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
                source: InstrumentSource::Vocal,
                clips: vec![Clip {
                    id: 1,
                    name: "こんにちは".into(),
                    start_beat: 0.0,
                    length_beats: 16.0,
                    content_id: 0,
                    notes: vec![
                        Note {
                            id: 0,
                            start_beat: 0.0,
                            duration_beats: 1.0,
                            pitch: 60,
                            velocity: 100,
                            lyric: Some("こ".into()),
                            muted: false,
                        },
                        Note {
                            id: 0,
                            start_beat: 1.5,
                            duration_beats: 0.5,
                            pitch: 62,
                            velocity: 100,
                            lyric: Some("ん".into()),
                            muted: false,
                        },
                    ],
                    color: None,
                    auto_lipsync: false,
                    muted: false,
                    speaker_id: 3061,
                    singer_name: "中国うさぎ".into(),
                    style_name: "ノーマル".into(),
                    talk: None,
                }],
                ..Track::default()
            }],
            ..Song::default()
        };
        assert_eq!(json_roundtrip(&song), song);
    }

    #[test]
    fn current_version_is_pinned() {
        // Bumped to 23 for the single linear device chain: `Track`'s three
        // role-keyed chains (`instrument` / `midi_fx_chain` / `fx_chain`)
        // collapse into one `devices: Vec<PluginInstance>`, each carrying a
        // `ports: PortConfig` (roles are derived from position, not stored),
        // and `AutomationTarget::PluginParam { slot } → { device_index }`.
        // v22 files forward-migrate: the old fields deserialize into
        // private legacy slots and `Track::flatten_legacy_devices` (run from
        // `ensure_ids`) flattens them into `devices` and remaps lane slots.
        // Pinning the constant catches accidental rollback. See
        // `docs/plan_linear_chain.md`. v24 adds `Song.project_id`
        // (`docs/plan_fixme_33_clipboard.md`, clipboard same-project detection).
        // v25: 旧 `group_transform` 持ちトラックに `builtin.video.transform`
        // 配置 device を `ensure_ids` で補う (additive、値 migration 無し)。
        // v26 (`docs/plan_voicevox_talk.md`): `Clip.talk` 追加 + テキストオーバーレイ表示が
        // `builtin.video.subtitle` device gate になり、v25 以前は load 時に Text 持ち
        // トラックへ字幕デバイスを auto-insert (`project::migrate_text_overlay_to_subtitle_device`)。
        // v27: `Clip.muted` / `Note.muted` 追加 (clip / note mute の SSoT)、v26 以前の
        // per-event mute は `project::migrate_per_event_mute_to_clip_mute` で `Clip.muted` へ畳み込む。
        // v28: `ProjectFile.view: Option<ViewState>` 追加 (= ズーム/スクロール等の
        // 表示状態を Song の兄弟として同梱)。Song / IPC は無改変、旧ファイルは `#[serde(default)]`
        // で `view == None` に forward-migrate (migration 関数不要)。
        // v29 (`docs/plan_arch_refactor.md` §1): 安定 id addressing —
        // `PluginInstance.id: u64` / `Send.id` / note・audio event・automation
        // point の要素 id を追加し、`PluginParam.device_index` → `device_id`、
        // `SendGain.send_idx` → `send_id` へ移行。旧 positional 値は
        // deserialize 専用 legacy field 経由で `ensure_ids` が id へ写像する。
        // v30 (§10): ClipContent を untagged → tagged (`type` field) 化。
        assert_eq!(CURRENT_VERSION, 30);
    }

    #[test]
    fn v25_ensure_ids_adds_transform_device_for_group_transform_tracks() {
        // 旧 group_transform 持ちトラックは ensure_ids で Transform
        // 配置 device を 1 つ得る (idempotent: 2 回呼んでも 1 つ)。group_transform 無しは付かない。
        let mut song = Song {
            tracks: vec![
                Track { id: 1, group_transform: Some(GroupTransform::default()), ..Track::default() },
                Track { id: 2, ..Track::default() },
            ],
            next_track_id: 3,
            ..Song::default()
        };
        song.ensure_ids();
        let t1 = song.track_by_id(1).unwrap();
        assert_eq!(
            t1.devices.iter().filter(|d| d.plugin_id == crate::video_fx::TRANSFORM_ID).count(),
            1,
            "group_transform 持ちトラックに Transform device が 1 つ付くべき"
        );
        let t2 = song.track_by_id(2).unwrap();
        assert!(
            !t2.devices.iter().any(|d| d.plugin_id == crate::video_fx::TRANSFORM_ID),
            "group_transform 無しトラックには Transform device を付けない"
        );
        // idempotent: 再実行で増えない。
        song.ensure_ids();
        assert_eq!(
            song.track_by_id(1)
                .unwrap()
                .devices
                .iter()
                .filter(|d| d.plugin_id == crate::video_fx::TRANSFORM_ID)
                .count(),
            1,
            "ensure_ids は idempotent (Transform device は重複しない)"
        );
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
                            id: 0,
                            dest_track_id: 2,
                            gain: 0.5,
                            mode: SendMode::PostFader,
                            enabled: true,
                        },
                        Send {
                            id: 0,
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
                        id: 0,
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
                next_point_id: 0,
                points: vec![
                    AutomationPoint {
                        id: 0,
                        time_beat: 0.0,
                        value: 0.5,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 0,
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
                ..Default::default()
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

    /// v29: `Song::remove_track_send` は安定 send id で削除し、 その send を
    /// 狙う SendGain lane だけを除去する。 残る send への参照は id なので
    /// **無変更のまま正しい** (positional reindex 儀式は廃止)。
    #[test]
    fn remove_track_send_drops_only_matching_send_gain_lanes() {
        let send_lane = |sid: u32| {
            AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                    send_id: sid,
                    legacy_send_idx: None,
                }),
                1.0,
            )
        };
        let mk_send = |sid: u32, dest: u32| crate::model::Send {
            id: sid,
            dest_track_id: dest,
            gain: 1.0,
            mode: crate::model::SendMode::PostFader,
            enabled: true,
        };
        let mut song = Song::default();
        song.tracks.push(Track {
            id: 42,
            sends: vec![mk_send(10, 1), mk_send(11, 2), mk_send(12, 3)],
            next_send_id: 13,
            automation_lanes: vec![send_lane(10), send_lane(11), send_lane(12)],
            ..Track::default()
        });
        // send id 11 (dest 2) を削除。
        assert!(song.remove_track_send(42, 11));
        let t = song.track_by_id(42).unwrap();
        assert_eq!(t.sends.len(), 2);
        assert_eq!(t.sends[0].dest_track_id, 1);
        assert_eq!(t.sends[1].dest_track_id, 3);
        let ids: Vec<u32> = t
            .automation_lanes
            .iter()
            .filter_map(|l| match l.target {
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain {
                    send_id, ..
                }) => Some(send_id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec![10, 12], "id 11 の lane だけ除去、残りは無変更");
        // 不在 id / 不在 track は false (no-op)。
        assert!(!song.remove_track_send(42, 99));
        assert!(!song.remove_track_send(999, 10));
    }

    /// r.md #8 M7 + v29: 旧 project の PluginParam MIDI binding は `slot`
    /// (v22) または `device_index` (v23-28) を持つ。 どちらも deserialize
    /// 失敗せず legacy field に載り、 `ensure_ids` が device_id へ写像する。
    #[test]
    fn binding_target_plugin_param_legacy_compat() {
        use crate::protocol::PluginSlot;
        // v22 JSON: device_index の代わりに slot。
        let json = r#"{"PluginParam":{"track":3,"slot":{"Fx":1},"param_id":7}}"#;
        let bt: BindingTarget = serde_json::from_str(json).expect("旧 slot 形式が load できる");
        assert_eq!(
            bt,
            BindingTarget::PluginParam {
                track: 3,
                device_id: 0, // 未 migration (ensure_ids が解決)
                param_id: 7,
                legacy_device_index: None,
                legacy_slot: Some(PluginSlot::Fx(1)),
            }
        );
        // v23-28 JSON (device_index) は legacy_device_index に載る。
        let json_v28 = r#"{"PluginParam":{"track":1,"device_index":2,"param_id":5}}"#;
        let bt2: BindingTarget = serde_json::from_str(json_v28).unwrap();
        assert!(matches!(
            bt2,
            BindingTarget::PluginParam {
                device_id: 0,
                legacy_device_index: Some(2),
                legacy_slot: None,
                ..
            }
        ));
        // v29 JSON (device_id) はそのまま。 bincode 往復 (IPC 経路) も一致。
        let bt3 = BindingTarget::PluginParam {
            track: 1,
            device_id: 42,
            param_id: 5,
            legacy_device_index: None,
            legacy_slot: None,
        };
        let via_json = json_roundtrip(&bt3);
        assert_eq!(bt3, via_json);
        let cfg = bincode::config::standard();
        let bytes = bincode::encode_to_vec(bt3, cfg).unwrap();
        let (back, _): (BindingTarget, usize) = bincode::decode_from_slice(&bytes, cfg).unwrap();
        assert_eq!(bt3, back);
    }

    #[test]
    fn gc_clip_contents_keeps_song_lane_references() {
        // Regression: a content_id only referenced by a song-level
        // automation clip (SongTempo master lane) must survive
        // `gc_clip_contents`. The earlier impl walked only `tracks[]` and
        // dropped it, silently deleting tempo-automation curves on save.
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Automation(AutomationContent::default()),
        );
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "tempo".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: cid,
            }],
            ..AutomationLane::new(AutomationTarget::SongTempo, 120.0)
        });
        song.gc_clip_contents();
        assert!(
            song.clip_contents.contains_key(&cid),
            "song-lane automation content must survive GC"
        );
        assert_eq!(
            song.clip_content_refcount(cid),
            1,
            "song-lane clip must count toward refcount"
        );
    }

    #[test]
    fn ensure_clip_contents_reassigns_song_lane_sentinel_ids() {
        // Regression: a song-lane clip carrying the sentinel content_id 0
        // must get a fresh id and a content entry, else automation eval /
        // GUI lookup always fall back to empty.
        let mut song = Song::default();
        song.song_lanes.push(AutomationLane {
            id: 1,
            clips: vec![AutomationClip {
                id: 1,
                name: "tempo".into(),
                start_beat: 0.0,
                length_beats: 4.0,
                content_id: 0,
            }],
            ..AutomationLane::new(AutomationTarget::SongTempo, 120.0)
        });
        song.ensure_clip_contents();
        let cid = song.song_lanes[0].clips[0].content_id;
        assert_ne!(cid, 0, "sentinel content_id must be reassigned");
        assert!(
            song.clip_contents.contains_key(&cid),
            "reassigned song-lane content must have an entry"
        );
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
            device_id: 0,
            param_id: 7,
            legacy_device_index: None,
            legacy_slot: None,
        });
        s.insert(AutomationTarget::PluginParam {
            device_id: 1,
            param_id: 7,
            legacy_device_index: None,
            legacy_slot: None,
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
                ..Default::default()
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
    fn tagged_clip_content_round_trips_audio_and_video() {
        // (v30 §10) tagged `ClipContent`: `"type"` タグで Audio / Video を明示判別する
        // (旧 `#[serde(untagged)]` + events の required-field 依存は撤去済)。
        let audio_json = r#"{
            "type": "Audio",
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
            "type": "Video",
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

    // ---- rescale_raw_clips_for_bpm (r.md #7) ----

    fn rescale_event(mode: StretchMode, start: f64, len: f64) -> AudioEvent {
        AudioEvent {
            source_id: 1,
            event_start_in_clip_beats: start,
            event_length_beats: len,
            source_start_frames: 0,
            source_end_frames: 48_000,
            stretch_mode: mode,
            fade_in_beats: 0.5,
            fade_out_beats: 0.25,
            ..Default::default()
        }
    }

    /// 1 track / 1 clip の Song を組み立て、(song, content_id) を返す。clip は
    /// `start_beat` / `length_beats` に置かれ、与えた `events` の content を参照する。
    fn rescale_song(events: Vec<AudioEvent>, start_beat: f64, length_beats: f64) -> (Song, ContentId) {
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents
            .insert(cid, ClipContent::Audio(AudioContent { events, next_event_id: 0 }));
        song.tracks = vec![Track {
            id: 1,
            clips: vec![Clip {
                id: 1,
                start_beat,
                length_beats,
                content_id: cid,
                ..Default::default()
            }],
            next_clip_id: 2,
            ..Track::default()
        }];
        (song, cid)
    }

    #[test]
    fn rescale_raw_doubles_length_on_bpm_double() {
        // Raw clip を BPM 120 → 240 (ratio 2.0)。event / clip の拍量は秒固定で 2 倍、
        // clip.start_beat は拍固定。
        let (mut song, cid) = rescale_song(vec![rescale_event(StretchMode::Raw, 1.0, 4.0)], 8.0, 4.0);
        song.rescale_raw_clips_for_bpm(120.0, 240.0);
        let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
            panic!("audio");
        };
        let ev = &a.events[0];
        assert_eq!(ev.event_start_in_clip_beats, 2.0);
        assert_eq!(ev.event_length_beats, 8.0);
        assert_eq!(ev.fade_in_beats, 1.0);
        assert_eq!(ev.fade_out_beats, 0.5);
        let clip = &song.tracks[0].clips[0];
        assert_eq!(clip.start_beat, 8.0, "start は拍固定");
        assert_eq!(clip.length_beats, 8.0);
    }

    #[test]
    fn rescale_raw_halves_length_on_bpm_half() {
        let (mut song, cid) = rescale_song(vec![rescale_event(StretchMode::Raw, 2.0, 4.0)], 0.0, 4.0);
        song.rescale_raw_clips_for_bpm(120.0, 60.0);
        let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
            panic!("audio");
        };
        assert_eq!(a.events[0].event_length_beats, 2.0);
        assert_eq!(a.events[0].event_start_in_clip_beats, 1.0);
        assert_eq!(song.tracks[0].clips[0].length_beats, 2.0);
    }

    #[test]
    fn rescale_leaves_non_raw_modes_untouched() {
        // Stretch / Repitch / Slice は拍固定。一切変えない。
        for mode in [StretchMode::Stretch, StretchMode::Repitch, StretchMode::Slice] {
            let (mut song, cid) = rescale_song(vec![rescale_event(mode, 1.0, 4.0)], 0.0, 4.0);
            song.rescale_raw_clips_for_bpm(120.0, 240.0);
            let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
                panic!("audio");
            };
            assert_eq!(a.events[0].event_length_beats, 4.0, "{mode:?} 不変");
            assert_eq!(a.events[0].event_start_in_clip_beats, 1.0, "{mode:?} 不変");
            assert_eq!(song.tracks[0].clips[0].length_beats, 4.0, "{mode:?} 不変");
        }
    }

    #[test]
    fn rescale_skips_mixed_mode_content() {
        // Raw と Stretch が混在する content は「全 event が Raw」でないので
        // content / clip ともに据え置き (event 単位でなく clip 単位の判定)。
        let (mut song, cid) = rescale_song(
            vec![
                rescale_event(StretchMode::Raw, 0.0, 2.0),
                rescale_event(StretchMode::Stretch, 2.0, 2.0),
            ],
            0.0,
            4.0,
        );
        song.rescale_raw_clips_for_bpm(120.0, 240.0);
        let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
            panic!("audio");
        };
        assert_eq!(a.events[0].event_length_beats, 2.0);
        assert_eq!(a.events[1].event_length_beats, 2.0);
        assert_eq!(song.tracks[0].clips[0].length_beats, 4.0);
    }

    #[test]
    fn rescale_shared_content_scales_once_all_clips() {
        // 同一 Raw content を 2 clip が共有。content events は一度だけスケールされ、
        // 参照する両 clip の length がスケールされる (start は各々固定)。
        let mut song = Song::default();
        let cid = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Audio(AudioContent {
                events: vec![rescale_event(StretchMode::Raw, 0.0, 4.0)],
                next_event_id: 0,
            }),
        );
        song.tracks = vec![Track {
            id: 1,
            clips: vec![
                Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: cid, ..Default::default() },
                Clip { id: 2, start_beat: 16.0, length_beats: 4.0, content_id: cid, ..Default::default() },
            ],
            next_clip_id: 3,
            ..Track::default()
        }];
        song.rescale_raw_clips_for_bpm(100.0, 150.0); // ratio 1.5
        let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
            panic!("audio");
        };
        assert_eq!(a.events[0].event_length_beats, 6.0);
        assert_eq!(song.tracks[0].clips[0].length_beats, 6.0);
        assert_eq!(song.tracks[0].clips[1].length_beats, 6.0);
        assert_eq!(song.tracks[0].clips[1].start_beat, 16.0, "start は拍固定");
    }

    #[test]
    fn rescale_degenerate_inputs_are_noop() {
        for (old, new) in [(0.0f32, 240.0f32), (120.0, 0.0), (f32::NAN, 240.0), (120.0, 120.0)] {
            let (mut song, cid) = rescale_song(vec![rescale_event(StretchMode::Raw, 1.0, 4.0)], 0.0, 4.0);
            song.rescale_raw_clips_for_bpm(old, new);
            let ClipContent::Audio(a) = &song.clip_contents[&cid] else {
                panic!("audio");
            };
            assert_eq!(a.events[0].event_length_beats, 4.0, "old={old} new={new}");
            assert_eq!(a.events[0].event_start_in_clip_beats, 1.0, "old={old} new={new}");
            assert_eq!(song.tracks[0].clips[0].length_beats, 4.0, "old={old} new={new}");
        }
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
    fn tagged_clip_content_round_trips_image() {
        // (v30 §10) tagged `ClipContent`: `"type": "Image"` で Image variant を明示判別する
        // (旧 untagged の opacity 依存 dispatch は撤去済)。
        let image_json = r#"{
            "type": "Image",
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
    fn image_source_without_name_field_deserializes_with_empty_name() {
        // v21 `.daw` files stored `ImageSource` without a `name` key. Loading
        // into v22 must succeed with `name` defaulting to "" (per
        // `#[serde(default)]`); the inspector then falls back to the on-disk
        // file name. A v22 source carries the original import name verbatim.
        let v21_json = r#"{
            "path": { "Absolute": "/img/_a1b2c3d4.png" },
            "width": 64,
            "height": 64,
            "format": "Png"
        }"#;
        let src: ImageSource = serde_json::from_str(v21_json).unwrap();
        assert_eq!(src.name, "");

        // Round-trip a v22 source with the original (Japanese) name.
        let named = ImageSource {
            path: ImageSourcePath::Absolute("/img/_a1b2c3d4.png".into()),
            name: "あ.png".into(),
            width: 64,
            height: 64,
            format: "Png".into(),
        };
        let json = serde_json::to_string(&named).unwrap();
        let back: ImageSource = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "あ.png");
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
                name: "logo.png".into(),
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
                name: "live.png".into(),
                width: 256,
                height: 256,
                format: "Png".into(),
            },
        );
        song.image_sources.insert(
            orphan_id,
            ImageSource {
                path: ImageSourcePath::Absolute("/tmp/orphan.png".into()),
                name: "orphan.png".into(),
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
