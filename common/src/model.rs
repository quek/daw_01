use std::collections::HashMap;
use std::path::PathBuf;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::plugin_format::PluginFormat;
use crate::scale::ScaleChange;

// arch-refactor #9 (god-file budget): model.rs を型群ごとにサブモジュールへ分割
// (pure code movement — 挙動・serialize 形式は不変)。各サブモジュールは `use super::*`
// で相互の型を参照し、ここで全て re-export するので外部の `common::model::Clip` 等の
// 絶対パスは不変。wire 型を含むため common/build.rs の WIRE_SOURCES にも 4 ファイルを
// 登録している (invariant #7: fingerprint handshake の検出網に穴を開けない)。
mod automation;
mod content;
mod modulation;
mod track;
pub use automation::*;
pub use content::*;
pub use modulation::*;
pub use track::*;

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

/// Song の imported media source プール (audio / video / image)。§10 bullet 4 で Song の
/// フラットな 3 マップをここへ集約した (god-struct 縮退)。nested `"media": {...}` として save / wire し、
/// 旧 .daw のフラット形式 (`audio_sources` 等を Song 直下) は load 時の JSON 前処理
/// `project::migrate_flat_media_to_pools` が `media` 下へ移す (save 互換)。nested を採用するのは
/// serde `flatten` が `HashMap<u32, _>` の整数キーを復元できないため。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MediaPools {
    /// Pool of imported audio file references (WAV / generated)。key = `AudioSourceId`。
    /// メタデータのみ (path / sample_rate / channels / frames)、decode 済みバッファは各
    /// プロセスが path から独立に復号。refcount 0 は `gc_audio_sources` が save 前に GC。
    #[serde(default)]
    pub audio_sources: HashMap<AudioSourceId, AudioSource>,
    /// Pool of imported video file references。key = `VideoSourceId`。メタデータのみ
    /// (path / width / height / framerate / duration / codec)。refcount 0 は `gc_video_sources`。
    #[serde(default)]
    pub video_sources: HashMap<VideoSourceId, VideoSource>,
    /// Pool of imported image file references (PNG / JPEG / WebP)。key = `ImageSourceId`。
    /// メタデータのみ (path / width / height / format)。refcount 0 は `gc_image_sources`。
    #[serde(default)]
    pub image_sources: HashMap<ImageSourceId, ImageSource>,
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
    /// §10 bullet 4: imported media source プール (audio / video / image)。旧 .daw は
    /// `audio_sources` / `video_sources` / `image_sources` を Song 直下にフラット保存していたが、
    /// serde `flatten` は `HashMap<u32, _>` の整数キーを content-buffer 経由で復元できない
    /// (`invalid type: string "1", expected u32`) ため nested `"media": {...}` として保存する。
    /// 旧フラット形式は load 時の JSON 前処理 `project::migrate_flat_media_to_pools` が `media`
    /// 下へ移す (= save 互換維持)。field access は `song.media.audio_sources` 等。
    #[serde(default)]
    pub media: MediaPools,
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
            media: MediaPools::default(),
            next_audio_source_id: 1,
            song_lanes: Vec::new(),
            next_song_lane_id: 1,
            midi_bindings: Vec::new(),
            scale_changes: Vec::new(),
            next_video_source_id: 1,
            video_resolution: default_video_resolution(),
            video_framerate: default_video_framerate(),
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
        /// v28 以前 migration 用 (deserialize 専用)。旧 save は chain 内 positional
        /// index、または旧 `slot: PluginSlot` (load 時 JSON 前処理
        /// `project::migrate_legacy_device_chains` が chain 長から index へ解決。
        /// r.md #8 M7: 旧 PluginParam binding が device_index 欠落で deserialize を
        /// 落としていたのを是正) を持つ。`Song::ensure_ids` の remap pass が安定 device_id へ写像。
        #[serde(default, rename = "device_index", skip_serializing)]
        legacy_device_index: Option<u32>,
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
        // 旧 3-split device chain (midi_fx_chain / instrument / fx_chain) の `devices` への
        // 平坦化、および automation lane / midi_binding の旧 `slot: PluginSlot` →
        // positional `device_index` 解決は、load 時の JSON 前処理
        // (`project::migrate_legacy_device_chains`、§10) が担う。ここでは前処理が残した
        // positional `legacy_device_index` / `legacy_send_idx` を安定 device_id / send_id へ
        // 写像する (下記 remap pass。新形式 = legacy = None は no-op)。

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
                if self.tracks[t_idx].clips[c_idx].content_id == 0 {
                    let new_id = self.alloc_content_id();
                    self.tracks[t_idx].clips[c_idx].content_id = new_id;
                }
                let cid = self.tracks[t_idx].clips[c_idx].content_id;
                // Ensure an entry exists for every referenced content_id so
                // lookups never miss. 旧 per-clip インライン content (v5 notes /
                // v19 name) は deserialize 前に `project::migrate_legacy_clip_content`
                // が content store へドレイン済み。
                self.clip_contents.entry(cid).or_default();
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
                    // Automation clips have no legacy in-place payload
                    // (v8-introduced) — just ensure the content store has an
                    // entry so audio thread / GUI lookups never miss. Default
                    // is `Midi(empty)`; writers promote to `Automation` on
                    // first edit. (Legacy name は前処理でドレイン済み。)
                    self.clip_contents.entry(cid).or_insert_with(|| {
                        ClipContent::Automation(AutomationContent::default())
                    });
                }
            }
        }
        // Song-level automation lanes share the same content store but are
        // not reached by the per-track walk above. Reassign sentinel ids and
        // ensure an entry exists — mirroring the `automation_lanes` handling so
        // SongTempo / TimeSig curves resolve instead of falling back to empty.
        // (Legacy name は前処理でドレイン済み。)
        for l_idx in 0..self.song_lanes.len() {
            let lane_clip_count = self.song_lanes[l_idx].clips.len();
            for c_idx in 0..lane_clip_count {
                if self.song_lanes[l_idx].clips[c_idx].content_id == 0 {
                    let new_id = self.alloc_content_id();
                    self.song_lanes[l_idx].clips[c_idx].content_id = new_id;
                }
                let cid = self.song_lanes[l_idx].clips[c_idx].content_id;
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
        self.media.audio_sources.retain(|id, _| live.contains(id));
    }

    /// Re-assign fresh `AudioSourceId` to any source whose id is the
    /// `0` sentinel (and bump `next_audio_source_id` above the highest
    /// seen). Idempotent — sources with non-zero ids are left untouched.
    /// Mirrors `ensure_clip_contents` semantics.
    pub fn ensure_audio_source_ids(&mut self) {
        let mut max_seen: AudioSourceId = 0;
        for id in self.media.audio_sources.keys() {
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
        if let Some(orphan) = self.media.audio_sources.remove(&0) {
            let new_id = self.alloc_audio_source_id();
            self.media.audio_sources.insert(new_id, orphan);
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
        self.media.video_sources.retain(|id, _| live.contains(id));
    }

    /// v12: re-assign fresh `VideoSourceId` to any source whose id is
    /// the `0` sentinel and bump `next_video_source_id` above the
    /// highest seen. Mirrors `ensure_audio_source_ids` semantics; v11
    /// files load with all-default fields so this only matters once
    /// v12 sources start being saved with sentinel ids (= shouldn't
    /// happen in practice, but the invariant is cheap to enforce).
    pub fn ensure_video_source_ids(&mut self) {
        let mut max_seen: VideoSourceId = 0;
        for id in self.media.video_sources.keys() {
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
        if let Some(orphan) = self.media.video_sources.remove(&0) {
            let new_id = self.alloc_video_source_id();
            self.media.video_sources.insert(new_id, orphan);
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
        self.media.image_sources.retain(|id, _| live.contains(id));
    }

    /// v13: re-assign fresh `ImageSourceId` to any source whose id is
    /// the `0` sentinel and bump `next_image_source_id` above the
    /// highest seen. Mirrors `ensure_video_source_ids` semantics.
    pub fn ensure_image_source_ids(&mut self) {
        let mut max_seen: ImageSourceId = 0;
        for id in self.media.image_sources.keys() {
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
        if let Some(orphan) = self.media.image_sources.remove(&0) {
            let new_id = self.alloc_image_source_id();
            self.media.image_sources.insert(new_id, orphan);
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
            aux_outputs: Vec::new(),
            aux_output_count: 0,
            ports,
            ara_archive: None,
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


#[cfg(test)]
mod tests;
