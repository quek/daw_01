use std::collections::HashMap;
use std::path::PathBuf;

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::scale::ScaleChange;

// arch-refactor #9 (god-file budget): model.rs を型群ごとにサブモジュールへ分割
// (pure code movement — 挙動・serialize 形式は不変)。各サブモジュールは `use super::*`
// で相互の型を参照し、ここで re-export するので外部の `common::model::Clip` 等の
// 絶対パスは不変。wire を渡る型を持つファイルは common/build.rs の WIRE_SOURCES に登録する
// (invariant #7: fingerprint handshake の検出網に穴を開けない)。wire に載らない `Song` の
// ロジックを切り出したファイル (section_ops / load_normalize / source_pools 等) は登録しない
// (ロジックの変更で fingerprint を動かさない、build.rs 冒頭)。
mod automation;
mod bounce_ops;
mod clip_window;
mod master_limiter;
mod media_manifest;
mod content;
mod content_split;
mod device;
mod ids;
mod lineage;
mod load_normalize;
mod midi_bind;
mod modulation;
mod native;
mod native_param;
mod param_address;
mod param_range;
mod plugin_instance;
mod section_ops;
mod sections;
mod session;
mod source_pools;
mod time_ops;
mod time_selection;
mod view_state;
mod track;
mod track_enable;
pub use automation::*;
pub use bounce_ops::*;
pub use clip_window::*;
pub use master_limiter::*;
pub use media_manifest::*;
pub use content::*;
pub use content_split::split_boundaries;
pub use device::*;
pub use ids::*;
pub use midi_bind::*;
pub use modulation::*;
pub use native::*;
pub use native_param::*;
pub use param_address::ParamStoreAt;
pub use param_range::*;
pub use plugin_instance::*;
pub use section_ops::*;
pub use sections::*;
pub use session::*;
pub use source_pools::*;
pub use time_ops::*;
pub use time_selection::*;
pub use track::*;
pub use view_state::{RackPanelKey, ViewState};

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
/// `docs/plan_track_clip_color.md`. 2026-09-03 (no version bump):
/// `AutomationLane.color` / `AutomationClip.color` follow the same two-level
/// scheme — lane `None` = per-target identity color (derived in the view
/// layer), clip `None` = inherit the lane's effective color.
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
/// Bumped to `31`: `Song.loop_start_beat` / `loop_end_beat` を撤去し、再生ループ
/// (ON/OFF + 範囲) を [`LoopRegion`] として session state + [`ViewState::loop_region`]
/// へ移した (「聴き方の都合」 は dirty を立てないが保存される)。v30 以前の `.daw` は
/// `project::legacy_song_loop_region` が Song 直下から読み出して移行する。
/// Bumped to `32` (r.md #44 / `docs/plan_clip_content_window.md`): `Clip` /
/// `AutomationClip` に `content_offset_beats` (= clip が共有 content のどこを
/// 見せているか) を追加。 端 trim が content を書き換えなくなり、`content_id` を
/// 共有する linked clip の開始・終了が完全に独立する。v31 以前は `serde(default)` の
/// `0.0` (= 従来どおり content 先頭から見せる) で読める。
/// Bumped to `33` (r.md #50 の follow-up): master の出力音量を [`Song::master_gain`]
/// として保存する。従来は GUI のセッション状態にしか無く、**保存しても開き直すと
/// 0dB に戻っていた**。v32 以前の `.daw` は `serde(default)` の `1.0` (unity) で
/// 読めるので、旧ファイルの聞こえ方は変わらない。
/// Bumped to `34` (r.md #71 プラグインのコピー / 移動): `BindingTarget::PluginParam`
/// の `track` を deserialize 専用 (`legacy_track`) に落とす。 device を別トラックへ
/// 移せるようになったので、所属 track を保存すると stale になる (実行時の解決は
/// `device_id` からの逆引き 1 本)。v33 以前の `.daw` は `track` を読んで
/// `legacy_device_index` の解決にだけ使う。
/// Bumped to `35` (r.md #87 クリップランチャー / セッションビュー、
/// `docs/plan_rmd_87_clip_launcher.md`): `Song.scenes` (ランチャーの列)、
/// `Track.session_clips` / `AutomationLane.session_clips` (セル)、および
/// 行ごとの主導権 `Track.launcher` / `AutomationLane.launcher` が追加される。
/// **主導権と鳴っているセルは「曲の一部」として保存する** — 停止 → 再生で同じセルが
/// 鳴り直し、書き出す音もこの状態で決まるため (Q9 / Q10)。v34 以前の `.daw` は全
/// field が `#[serde(default)]` で forward-migrate する (`scenes` は空 Vec、主導権は
/// `RowPlayback::Arranger` = 従来どおりアレンジだけが鳴る)。**load 時に列を補わない**
/// ので、開いただけでは `*` が立たない (r.md #9)。
///
/// `Song.last_launched_scene_id` (最後にユーザーが撃った列、`0` = 未発火) も同じ
/// 理由で保存する — 書き出しはこの列を範囲の先頭で撃った状態から走り出すので、
/// 落とすとシーン連鎖が再現できない。v34 以前は `0` で forward-migrate する。
///
/// あわせて `Song.global_launch_quantize` (グローバルローンチ量子化、既定 = 1 小節) と、
/// `MidiBinding` の入力が CC 固定 (`controller: u8`) から `MidiBindInput` (CC / ノート) へ
/// 変わり、`BindingTarget` にランチャー操作 6 種が加わる。パッドはノートで撃つので
/// CC だけでは足りない。旧 `controller` は deserialize 専用に降格し、
/// `Song::ensure_midi_binding_inputs` が load 時に `input` へ移す。
///
/// v36 (r.md #110 Parallel / `docs/plan_parallel.md`): `Track.devices` / `Song.master_fx_chain` の要素が
/// [`Device`] (plugin | Parallel) になり、[`AudioTap`] の source が track | chain の enum になった。
/// どちらも旧 JSON と byte 互換 (`untagged` / `flatten`) なので migration 関数は不要。
/// `PluginInstance.aux_input_count` (host 報告値) を追加。
///
/// v37: `AutomationLane.visible` を撤去し、レーンの非表示を [`ViewState::hidden_automation_lanes`]
/// (「見方の都合」 = 変えても `*` が立たない) へ移した。旧ファイルの `visible: false` は
/// `crate::project::load_project` が deserialize 前に拾って
/// [`crate::project::LoadedProject::hidden_automation_lanes`] へ移す。
///
/// v38 (r.md #114-#117): `Split::Selector` / `ModSourceKind::Adsr` / `RetriggerMode::Note` /
/// `TrackBuiltinParam::ParallelSelect` の variant と、 `LfoConfig` の Shape / Steps / Jitter /
/// Smooth / Delay / Fade In、 `ModSource.enabled` / `ModRouting.enabled` を追加。 旧ファイルは
/// `#[serde(default)]` で読める (migration 不要)。 新ファイルを旧ビルドで開くと unknown
/// variant で落ちるので version を上げて gate で弾く。
///
/// v39 (r.md #129 Rack 内蔵デバイス、`docs/plan_rack_native_devices.md`): `Track.strip` /
/// `Song.master_strip` を撤去し、組み込み Comp / EQ (master は Bus Comp / Tone EQ) をチェーン上の
/// [`Device::Native`] (`{"Native": {..}}`) にした。master の Limiter は [`Song::master_limiter`]。
/// オートメーション住所の `TrackBuiltin(Strip*)` / `MasterStrip(..)` は
/// [`AutomationTarget::NativeParam`] / [`AutomationTarget::MasterLimiter`] (実 device id) に変わる。
/// 旧ファイルは `project::migrate_legacy_song` の末尾 (`native_migration::migrate_strips_to_native`) が
/// 版に依存せず deserialize 前に移す (旧形と新形は重ならないので冪等)。
///
/// v40 (r.md #131 トラック無効化、`docs/plan_rmd_131_track_disable.md`): `Track.enabled` を追加。
/// 旧ファイルは `serde(default)` の `true` で読める (migration 不要)。新ファイルを旧ビルドで開くと
/// 無効トラックが黙って鳴り出す (未知フィールドを捨てる) ので、版を上げて gate で弾く。
///
/// v41 (r.md #130 グローバルトランスポーズ、`docs/plan_rmd_130_transpose.md`): [`Song::transpose`]・
/// [`Track::follow_transpose`]・[`AutomationTarget::SongTranspose`]・[`BindingTarget::SongTranspose`] を追加。
/// 旧ファイルは `serde(default)` (移調 0 / 全トラック追従) で読める。新ファイルを旧ビルドで開くと unknown variant で
/// 落ちるので版で弾く。
pub const CURRENT_VERSION: u32 = 41;

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

/// プラグインエディタ窓 1 枚分の位置とサイズ (r.md #65)。
///
/// `x` / `y` は **screen 座標の窓左上**、`width` / `height` は **client 領域**の
/// サイズ。どちらも physical pixels — VST3 `ViewRect` も CLAP `gui` も Windows では
/// physical px を使う契約なので (iplugview.h "Coordinates" 節 /
/// `CLAP_WINDOW_API_WIN32` の "uses physical size")、DPI スケールを掛け直さない。
///
/// **`Song` ではなく [`ViewState`] 側に置く**: 窓をどこに出すかは「作った中身」では
/// なく「見方の都合」なので、動かしても `*` (dirty) は付かない
/// (memory `project_dirty_flag_rule`、ズーム / スクロール / ループ範囲と同じ扱い)。
/// device 単位に持つので key は安定 id (`PluginInstance.id`) — 並べ替えで貼り替えが
/// 要る positional index は使わない (アーキテクチャ不変条件 #1)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct EditorWindowGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
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

/// 再生ループの状態 — ON/OFF と範囲を 1 つに束ねた SSoT。
///
/// **`Song` には置かない**。 ループは「作った中身」 ではなく「聴き方の都合」 なので、
/// ズーム / スクロールと同じく session state (daw_gui の `TransportState`) が所有し、
/// [`ViewState`] でプロジェクトに永続化する = **変更しても dirty (`*`) にならないが
/// 保存される**。 audio engine へは `AudioCommand::SetLoop(LoopRegion)` で 3 値まとめて
/// 届き、 engine 側も 1 つの値として保持する (ON/OFF と範囲を別経路にしない)。
///
/// `end_beat <= start_beat` (既定の 0/0 を含む) は「範囲未定義」 で、 engine は
/// 曲全体の content envelope をループ区間として使う ([`crate::timing::effective_loop_bounds`])。
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize, Encode, Decode)]
pub struct LoopRegion {
    /// ループ ON/OFF (transport bar の ⟳ トグル)。
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub start_beat: f64,
    #[serde(default)]
    pub end_beat: f64,
}

impl LoopRegion {
    /// 範囲が定義済 (= 正の長さを持つ) か。
    pub fn has_range(&self) -> bool {
        self.end_beat > self.start_beat
    }

    /// 定義済なら `(start, end)`。 未定義なら `None` (= 描画しない / 全曲を既定にする)。
    pub fn range(&self) -> Option<(f64, f64)> {
        self.has_range().then_some((self.start_beat, self.end_beat))
    }

    /// 信頼境界 (disk からの load / IPC 受信) 用の値域正規化。 `NaN` / 負値は
    /// 下流の `samples_per_beat` 換算や描画を壊すので 0 に落とす (`NaN <= 0.0` は
    /// `false` なので `!is_finite()` 側で弾く)。 冪等。
    pub fn sanitize(&mut self) {
        for v in [&mut self.start_beat, &mut self.end_beat] {
            if !(v.is_finite() && *v >= 0.0) {
                *v = 0.0;
            }
        }
    }

    /// タイムライン ripple (時間の挿入 / 削除) に追従させる。 `Song` 内の時間位置に
    /// [`Song::ripple_timeline`] が適用するのと同じ規則を、 `Song` の外に住むこの
    /// 範囲へ適用する。
    pub fn apply_ripple(&mut self, r: Ripple) {
        r.shift(&mut self.start_beat);
        r.shift(&mut self.end_beat);
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
    /// §10 bullet 4: 安定 id アロケータ群 (track / device / content / audio-video-image source /
    /// song-lane / section / mod-source の `next_*_id`)。旧 .daw のフラット形式は load 時
    /// `project::migrate_flat_ids_to_allocators` が `ids` 下へ移す (save 互換)。device 採番は
    /// track devices と `master_fx_chain` が共有する Song-global (invariant #1)。access は
    /// `song.ids.next_track_id` 等。
    #[serde(default)]
    pub ids: IdAllocators,
    /// Shared clip content store. Each `Clip.content_id` references one
    /// entry here; multiple clips with the same `content_id` share the
    /// same `notes` (linked / pooled clips, REAPER pooled MIDI model).
    /// Entries with refcount == 0 are GC'd by `Song::gc_clip_contents`
    /// before save.
    #[serde(default)]
    pub clip_contents: HashMap<ContentId, ClipContent>,
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
    /// master bus の audio fx chain。 通常 track の `Track.devices` と同 schema
    /// (= 同 `PluginInstance` を再利用)。 master は audio fx のみ持つ (= 音源境界
    /// なしの単一 Vec、 master bus に instrument / arpeggiator は無意味)。 automation の
    /// `song_lanes` と同じく「master 固有データは Track ではなく Song 直下に置く」
    /// 既存パターン (`automation_lane_by_key_mut` 参照) の踏襲。 audio engine は全
    /// track mix 後・metronome 前に `(MASTER_TRACK_ID, PluginSlot::Fx(i))` keying で
    /// 直列 process する。 旧 file は `#[serde(default)]` で空 Vec に forward-migrate。
    /// r.md #110: 要素は [`Device`] (plugin か Parallel)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub master_fx_chain: Vec<Device>,
    /// v33: master bus の出力音量 (linear amp、`1.0` = 0dB unity、上限
    /// [`MAX_TRACK_GAIN`] = +6dB)。全 track を mix し `master_fx_chain` を通した
    /// **最後**に掛かる。
    ///
    /// `master_fx_chain` / `song_lanes` と同じ「master 固有データは Track ではなく
    /// Song 直下」流儀。ここに置くのは **曲の一部** だから — マスター音量を変えると
    /// 書き出す音が変わるので、「作った中身が変わる = dirty を立てる」 側
    /// (`docs/plan_arch_refactor.md` の dirty 規約) に入る。
    ///
    /// v32 以前の `.daw` はフィールドを持たないので `1.0` (unity) に
    /// forward-migrate する (= 旧ファイルの聞こえ方は変わらない)。
    #[serde(default = "default_master_gain")]
    pub master_gain: f32,
    /// v39 (r.md #129): master のフェーダー後に固定で掛かる Limiter。信号順は
    /// `合算 → master_fx_chain (組み込み Bus Comp / Tone EQ を含む) → master_gain → Limiter`。
    /// チェーン上の device ではない (動かせず消せない)。旧 file は `master_strip.limiter` から移す。
    #[serde(default)]
    pub master_limiter: MasterLimiterSettings,
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
    /// docs/plan_modulation.md §1: 共有モジュレーション源 (sidechain +
    /// エンベロープフォロワー) の唯一の store。 `AuxInputRoute` / `ModRouting`
    /// から `ModSource.id` で参照される。 旧 file は空 Vec で forward-migrate。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mod_sources: Vec<ModSource>,
    /// **song-level lane 非依存モジュレーション** (`docs/plan_modulation_routing_redesign.md`
    /// §2): `SongTempo` / `SongTimeSigNumerator` 等の song-wide param を変調する
    /// `ModRouting`。track 内 param は `Track.mod_routings`、song-wide はこちら
    /// (`song_lanes` と同じ「master 固有データは Song 直下」流儀)。空 Vec で変調なし。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub song_mod_routings: Vec<ModRouting>,
    /// v35 (r.md #87 クリップランチャー): ランチャーの**列**。`Vec` の順序が
    /// そのまま表示順で、並べ替えは `Vec` 内の move、参照は常に [`Scene::id`]
    /// (positional index を持たない = アーキ不変条件 1)。
    ///
    /// Arranger セクション (`Song.sections`) とは**完全に無関係** — 列を足しても
    /// 曲の長さもクリップ位置も動かない (`docs/plan_rmd_87_clip_launcher.md` Q3)。
    /// 旧 file は空 Vec で forward-migrate し、**load 時に列を補わない**
    /// (補うと「開いただけで `*`」になる、r.md #9)。グリッドが描く空きの列は
    /// 表示上のプレースホルダで、そこにセルを置いた瞬間に実体化する。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenes: Vec<Scene>,
    /// v35 (r.md #87): ランチャーのグローバルローンチ量子化 (既定 = 1 小節)。
    ///
    /// **曲の一部** — セルの [`LaunchSettings::quantize`] が `Global` のとき
    /// 「いつ鳴り始めるか」がこれで決まり、書き出す音が変わる。保存しないと
    /// 「開き直したら書き出し結果が違う」になる (計画書 Q9 / Q10)。
    /// [`LaunchQuantize::Global`] 自身は入らない (自己参照になる) が、壊れた値が
    /// 来ても [`LaunchQuantize::beats`] が `None` を返して量子化なしに倒れる。
    #[serde(default = "default_global_launch_quantize")]
    pub global_launch_quantize: LaunchQuantize,
    /// v35 (r.md #87): **ユーザーが最後に撃った [`Scene`] の id**。`0` = 未発火。
    ///
    /// `Track.launcher` / `AutomationLane.launcher` が「行ごとの起点」を持つのと
    /// 対になる **曲全体の起点** で、シーン連鎖 (シーンのフォローアクション) が
    /// どこから始まるかを決める。書き出しは範囲の先頭でこの列を撃った状態から
    /// 走り出すので、これを保存しないと「聴こえている連鎖」を再現できない
    /// (`docs/plan_rmd_87_clip_launcher.md` Q9)。
    ///
    /// **フォローアクションによる遷移先を書いてはいけない** (同 §1.4)。書くと
    /// 「何秒鳴らしてから書き出したか」で出力が変わり、同じプロジェクト →
    /// 同じファイルという再現性が壊れる。走行中の現在位置は `audio_bridge` の
    /// atomic で GUI へ publish するだけで `Song` には入れない。
    ///
    /// 曲の一部なので変更で `*` が立つ (`Track.launcher` と同じ扱い、Q10)。
    /// 消えた列を指していたら [`Song::normalize_session`] が `0` へ落とす。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub last_launched_scene_id: u32,
    /// v41 (r.md #130): **グローバルトランスポーズの基準値** (半音、`-24..=24`)。ノートのデータは書き換えない
    /// 非破壊の曲パラメーターで、テンポと同じくレーン (`AutomationTarget::SongTranspose`) と変調を受ける。
    /// 実効値の評価は [`crate::transpose::transpose_in_clip`] 1 本、追従するトラックは
    /// [`Song::track_follows_transpose`]。書き出す音が変わるので曲の一部 (undo / `*`)。旧 file は 0。
    #[serde(default, skip_serializing_if = "is_zero_i8")]
    pub transpose: i8,
}

/// v34 以前の `.daw` に無いので 1 小節へ forward-migrate (Live / Bitwig / Studio One と同じ既定)。
fn default_global_launch_quantize() -> LaunchQuantize {
    DEFAULT_GLOBAL_LAUNCH_QUANTIZE
}

fn default_video_resolution() -> (u32, u32) {
    (1920, 1080)
}

fn default_video_framerate() -> f32 {
    30.0
}

/// v33 以前の `.daw` に `master_gain` は無いので unity (0dB) へ forward-migrate。
fn default_master_gain() -> f32 {
    1.0
}

impl Default for Song {
    fn default() -> Self {
        Self {
            bpm: 120.0,
            time_sig: (4, 4),
            length_beats: 64.0,
            tracks: Vec::new(),
            ids: IdAllocators {
                next_track_id: 1,
                next_device_id: 1,
                next_content_id: 1,
                next_audio_source_id: 1,
                next_video_source_id: 1,
                next_image_source_id: 1,
                next_song_lane_id: 1,
                next_section_id: 1,
                next_mod_source_id: 1,
                next_mod_routing_id: 1,
                next_scene_id: 1,
            },
            clip_contents: HashMap::new(),
            clip_content_names: HashMap::new(),
            media: MediaPools::default(),
            song_lanes: Vec::new(),
            midi_bindings: Vec::new(),
            scale_changes: Vec::new(),
            video_resolution: default_video_resolution(),
            video_framerate: default_video_framerate(),
            master_fx_chain: Vec::new(),
            master_gain: default_master_gain(),
            master_limiter: MasterLimiterSettings::default(),
            project_id: 0,
            sections: Vec::new(),
            mod_sources: Vec::new(),
            song_mod_routings: Vec::new(),
            scenes: Vec::new(),
            global_launch_quantize: DEFAULT_GLOBAL_LAUNCH_QUANTIZE,
            last_launched_scene_id: 0,
            transpose: 0,
        }
    }
}

impl Song {
    /// Allocate a new stable track id, bumping the song-level counter.
    pub fn alloc_track_id(&mut self) -> u32 {
        // `u32::MAX` is reserved as `MASTER_TRACK_ID`; clamp the usable
        // range to `[1, MASTER_TRACK_ID - 1]` so we never hand out the
        // sentinel, and `saturating_add` keeps the counter from wrapping
        // back to the `0` sentinel on exhaustion.
        let id = self.ids.next_track_id.clamp(1, MASTER_TRACK_ID - 1);
        self.ids.next_track_id = id.saturating_add(1);
        id
    }

    /// v29: 新規 device (plugin / native / Parallel / chain) 用の Song-global 安定 id を採番
    /// する。 track devices / master_fx_chain 共用。実体は [`IdAllocators::alloc_device_id`]。
    pub fn alloc_device_id(&mut self) -> u64 {
        self.ids.alloc_device_id()
    }

    /// master Limiter の先読み遅延を compile 時に焼くか: 静的 ON、または song 側に enabled な
    /// `MasterLimiter(On)` レーン / 変調がある。PDC と DSP 遅延の SSoT。
    #[must_use]
    pub fn master_limiter_latency_active(&self) -> bool {
        let on = AutomationTarget::MasterLimiter(MasterLimiterParam::On);
        self.master_limiter.on
            || self.song_lanes.iter().any(|l| l.enabled && l.target == on)
            || self.song_mod_routings.iter().any(|r| r.enabled && r.target == on)
    }

    /// Phase 5: allocate a new song-level automation lane id (`song_lanes`)。
    /// `next_song_lane_id` を bump して返す。
    pub fn alloc_song_lane_id(&mut self) -> u32 {
        let id = self.ids.next_song_lane_id.max(1);
        self.ids.next_song_lane_id = id.saturating_add(1);
        id
    }

    /// allocate a new stable `Section` id, bumping `next_section_id`。
    /// `0` は "未採番" sentinel なので最低 `1` から返す。
    pub fn alloc_section_id(&mut self) -> u32 {
        let id = self.ids.next_section_id.max(1);
        self.ids.next_section_id = id.saturating_add(1);
        id
    }

    /// docs/plan_modulation.md §1: allocate a new stable `ModSource` id,
    /// bumping `next_mod_source_id`。 `0` は "未採番" sentinel なので最低 `1` から返す。
    pub fn alloc_mod_source_id(&mut self) -> u32 {
        let id = self.ids.next_mod_source_id.max(1);
        self.ids.next_mod_source_id = id.saturating_add(1);
        id
    }

    /// r.md #89: allocate a new stable `ModRouting` id。`0` は "未採番" sentinel なので
    /// 最低 `1` から返す。[`AutomationTarget::ModRoutingDepth`] の参照先。
    pub fn alloc_mod_routing_id(&mut self) -> u32 {
        let id = self.ids.next_mod_routing_id.max(1);
        self.ids.next_mod_routing_id = id.saturating_add(1);
        id
    }

    /// 全 `ModRouting` を (置き場に関係なく) 走査する。
    pub fn all_mod_routings(&self) -> impl Iterator<Item = &ModRouting> {
        self.tracks
            .iter()
            .flat_map(|t| t.mod_routings.iter())
            .chain(self.song_mod_routings.iter())
    }

    /// 安定 `ModRouting::id` で 1 本の変調を引く (track / song のどの store に居ても)。
    pub fn mod_routing_by_id(&self, routing_id: u32) -> Option<&ModRouting> {
        self.all_mod_routings().find(|r| r.id == routing_id)
    }

    /// [`Self::mod_routing_by_id`] の可変版。
    pub fn mod_routing_by_id_mut(&mut self, routing_id: u32) -> Option<&mut ModRouting> {
        self.tracks
            .iter_mut()
            .flat_map(|t| t.mod_routings.iter_mut())
            .chain(self.song_mod_routings.iter_mut())
            .find(|r| r.id == routing_id)
    }

    /// `routing_id` の変調が置かれている track id (`MASTER_TRACK_ID` = song 側)。
    ///
    /// **置き場を `target` だけから決める全域関数は作らない** (master fx chain の
    /// `PluginParam` が乗らない)。実際に置かれている場所を引くこの述語が SSoT。
    #[must_use]
    pub fn mod_routing_owner(&self, routing_id: u32) -> Option<u32> {
        for t in &self.tracks {
            if t.mod_routings.iter().any(|r| r.id == routing_id) {
                return Some(t.id);
            }
        }
        self.song_mod_routings
            .iter()
            .any(|r| r.id == routing_id)
            .then_some(MASTER_TRACK_ID)
    }

    /// `source_id` のモジュレーターのツマミ (lane / routing) が置かれる track id。
    /// ソースの `owner_track_id` そのもの (`0` = legacy は `MASTER_TRACK_ID` に倒す)。
    #[must_use]
    pub fn mod_source_owner(&self, source_id: u32) -> Option<u32> {
        self.mod_sources.iter().find(|m| m.id == source_id).map(|m| {
            if m.owner_track_id == 0 {
                MASTER_TRACK_ID
            } else {
                m.owner_track_id
            }
        })
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

    /// ランチャーが主導権を握っている行 (トラック / オートメーションレーン / song lane) が
    /// 1 つでもあるか。 GUI が「アレンジのみ」でも『アレンジへ返す』列を残すかの判定
    /// (帯を隠したままアレンジが鳴らない理由を画面に残す)。
    #[must_use]
    pub fn any_launcher_owned_row(&self) -> bool {
        self.tracks.iter().any(|t| {
            t.launcher.is_launcher() || t.automation_lanes.iter().any(|l| l.launcher.is_launcher())
        }) || self.song_lanes.iter().any(|l| l.launcher.is_launcher())
    }

    /// 全オートメーションレーン (トラック + song lane) を走査する。
    pub fn all_automation_lanes(&self) -> impl Iterator<Item = &AutomationLane> {
        self.tracks.iter().flat_map(|t| t.automation_lanes.iter()).chain(self.song_lanes.iter())
    }

    /// 全レーンの [`AutomationLaneKey`] (track lane は track id、song lane は
    /// [`MASTER_TRACK_ID`])。表示 / 非表示の集合 (view 側) を全件で操作するときに使う。
    pub fn all_automation_lane_keys(&self) -> impl Iterator<Item = AutomationLaneKey> + '_ {
        self.tracks
            .iter()
            .flat_map(|t| t.automation_lanes.iter().map(move |l| AutomationLaneKey { track: t.id, lane: l.id }))
            .chain(self.song_lanes.iter().map(|l| AutomationLaneKey { track: MASTER_TRACK_ID, lane: l.id }))
    }

    /// [`Song::all_automation_lanes`] の mut 版。
    pub fn all_automation_lanes_mut(&mut self) -> impl Iterator<Item = &mut AutomationLane> {
        self.tracks
            .iter_mut()
            .flat_map(|t| t.automation_lanes.iter_mut())
            .chain(self.song_lanes.iter_mut())
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
    pub fn fx_chain_by_track_id(&self, track_id: u32) -> Option<&[Device]> {
        if track_id == MASTER_TRACK_ID {
            Some(&self.master_fx_chain)
        } else {
            self.track_by_id(track_id).map(|t| t.devices.as_slice())
        }
    }

    /// read-write counterpart of `fx_chain_by_track_id`。
    pub fn fx_chain_by_track_id_mut(&mut self, track_id: u32) -> Option<&mut Vec<Device>> {
        if track_id == MASTER_TRACK_ID {
            Some(&mut self.master_fx_chain)
        } else {
            self.track_by_id_mut(track_id).map(|t| &mut t.devices)
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

    /// `track_id` の表示名 — **トラック名を画面に出す口はこれと [`Track::display_name`] の 2 本だけ** (r.md #133)。
    /// 未命名は並び順の番号、`MASTER_TRACK_ID` は `Master`。無いトラック (削除済みを指したまま) は
    /// `(削除済み)` — 空を返すと「名前の無い行」になって、何を指していたのかが追えない。
    #[must_use]
    pub fn track_display_name(&self, track_id: u32) -> std::borrow::Cow<'_, str> {
        if track_id == MASTER_TRACK_ID {
            return std::borrow::Cow::Borrowed("Master");
        }
        self.tracks
            .iter()
            .enumerate()
            .find(|(_, t)| t.id == track_id)
            .map_or(std::borrow::Cow::Borrowed("(削除済み)"), |(i, t)| t.display_name(i))
    }

    /// [`ClipKey`] が指すクリップ。 アレンジのクリップとランチャーのセルの
    /// **どちらも**引ける ([`Track::clip_by_id`])。
    #[must_use]
    pub fn clip_by_key(&self, key: ClipKey) -> Option<&Clip> {
        self.track_by_id(key.track_id)?.clip_by_id(key.clip_id)
    }

    /// [`Self::clip_by_key`] の可変版。
    pub fn clip_by_key_mut(&mut self, key: ClipKey) -> Option<&mut Clip> {
        self.track_by_id_mut(key.track_id)?.clip_by_id_mut(key.clip_id)
    }

    /// 行レイアウト用の track index (表示順)。 **住所には使わない** —
    /// index は削除 / 並べ替えで意味が変わる (アーキ不変条件 1)。
    #[must_use]
    pub fn track_index_of(&self, track_id: u32) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == track_id)
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
    /// - **Disabled** (r.md #131): 実効的に無効なトラック ([`Song::track_effectively_enabled`])
    ///   は常に隠す。solo の判定にも数えない (無効トラックの solo は他を隠さない)。
    ///
    /// Cycle-safe: `parent_group_id` walks are hop-capped at `tracks.len()`.
    pub fn track_visually_silenced(&self, track_id: u32) -> bool {
        if !self.track_effectively_enabled(track_id) {
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
        // (2) solo rule (mirrors audio exactly)。solo は実効的に有効なトラックのものだけ数える。
        if !self.tracks.iter().any(|t| self.solo_counts(t)) {
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
            self.solo_counts(c) && {
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

    /// Allocate a fresh `ContentId`, bumping the song-level counter.
    /// 実体は [`IdAllocators::alloc_content_id`] (= 採番規則の SSoT)。
    pub fn alloc_content_id(&mut self) -> ContentId {
        self.ids.alloc_content_id()
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

    /// Clear the shared name for a `ContentId` (clip rename → empty string,
    /// r.md #15). After this `content_name` returns `""` and
    /// `clip_display_label` falls back to the derived source label (Text
    /// body / note lyrics) or blank. Removing the key (rather than storing
    /// `""`) keeps the map free of empty sentinels — `content_name` already
    /// treats a missing key as empty.
    pub fn clear_content_name(&mut self, content_id: ContentId) {
        self.clip_content_names.remove(&content_id);
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
        //    r.md #44: 内容窓の起点も content 拍量なので同じ比でスケールする
        //    (= trim 済み clip でも「窓が content の同じ場所を見せ続ける」)。
        //    v35 (r.md #87): launcher のセルも同じ content を指すので同じ比でスケールする
        //    (漏らすと bpm 変更後にセルだけ窓が content とズレる)。
        for track in &mut self.tracks {
            for clip in track.all_clips_mut() {
                if raw_content_ids.contains(&clip.content_id) {
                    clip.length_beats *= ratio;
                    clip.content_offset_beats *= ratio;
                }
            }
        }
        true
    }

    pub fn ensure_clip_contents(&mut self) {
        // Collect all live content_ids first so we can bump the counter
        // above the highest one before allocating new ids for sentinels.
        // Walks both main `clips` and every `automation_lanes[].clips`.
        // v35 (r.md #87): `all_clips` は arrangement + launcher のセルを両方返す。
        let mut max_seen: ContentId = 0;
        for track in &self.tracks {
            for clip in track.all_clips() {
                if clip.content_id != 0 {
                    max_seen = max_seen.max(clip.content_id);
                }
            }
            for lane in &track.automation_lanes {
                for clip in lane.all_clips() {
                    if clip.content_id != 0 {
                        max_seen = max_seen.max(clip.content_id);
                    }
                }
            }
        }
        for lane in &self.song_lanes {
            for clip in lane.all_clips() {
                if clip.content_id != 0 {
                    max_seen = max_seen.max(clip.content_id);
                }
            }
        }
        if self.ids.next_content_id <= max_seen {
            self.ids.next_content_id = max_seen + 1;
        }
        if self.ids.next_content_id == 0 {
            self.ids.next_content_id = 1;
        }

        // `ids` / `clip_contents` / `tracks` をフィールド分割で同時に可変借用する
        // (旧実装の index ループは `self.alloc_content_id()` を呼ぶための回避策だった。
        //  採番規則を `IdAllocators` へ下ろしたので走査そのものを素直に書ける)。
        let Song { tracks, song_lanes, ids, clip_contents, .. } = self;
        for track in tracks.iter_mut() {
            for clip in track.all_clips_mut() {
                if clip.content_id == 0 {
                    clip.content_id = ids.alloc_content_id();
                }
                // Ensure an entry exists for every referenced content_id so
                // lookups never miss. 旧 per-clip インライン content (v5 notes /
                // v19 name) は deserialize 前に `project::migrate_legacy_clip_content`
                // が content store へドレイン済み。
                clip_contents.entry(clip.content_id).or_default();
            }
            for lane in track.automation_lanes.iter_mut() {
                for clip in lane.all_clips_mut() {
                    if clip.content_id == 0 {
                        clip.content_id = ids.alloc_content_id();
                    }
                    // Automation clips have no legacy in-place payload
                    // (v8-introduced) — just ensure the content store has an
                    // entry so audio thread / GUI lookups never miss. Default
                    // is `Midi(empty)`; writers promote to `Automation` on
                    // first edit. (Legacy name は前処理でドレイン済み。)
                    clip_contents.entry(clip.content_id).or_insert_with(|| {
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
        for lane in song_lanes.iter_mut() {
            for clip in lane.all_clips_mut() {
                if clip.content_id == 0 {
                    clip.content_id = ids.alloc_content_id();
                }
                clip_contents.entry(clip.content_id).or_insert_with(|| {
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
        // v35 (r.md #87): `all_clips` は arrangement + launcher のセルを両方数える。
        let main_clips = self
            .tracks
            .iter()
            .flat_map(Track::all_clips)
            .filter(|c| c.content_id == content_id)
            .count();
        let auto_clips = self
            .tracks
            .iter()
            .flat_map(|t| t.automation_lanes.iter())
            .flat_map(AutomationLane::all_clips)
            .filter(|c| c.content_id == content_id)
            .count();
        // Song-level lanes share the same content store, so they must be
        // counted too — otherwise a shared tempo curve reads refcount 0
        // and the GUI / GC treat it as unreferenced.
        let song_lane_clips = self
            .song_lanes
            .iter()
            .flat_map(AutomationLane::all_clips)
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
    /// [`ClipKey`] が指すクリップの note 列 (共有 content の実体)。
    /// アレンジのクリップでもランチャーのセルでも同じように引ける。
    pub fn notes_in_clip_mut(&mut self, key: ClipKey) -> Option<&mut Vec<Note>> {
        let content_id = self.clip_by_key(key)?.content_id;
        self.clip_contents
            .get_mut(&content_id)
            .and_then(|c| c.notes_mut())
    }

    /// `track_id` の send (安定 id = `send_id`) を削除する。 v29 で id
    /// addressing になったため、 残る send への参照は**無変更のまま正しい**
    /// (positional 時代の「後続 index を詰める」 reindex 儀式は不要になった —
    /// r.md #8 A5 で実際に壊れた class の構造的解消)。
    /// 削除成功で `true`、 track 不在 / id 不在なら `false`。
    ///
    /// **その send を狙う SendGain のレーン / 変調はここでは落とさない。** 「send の無い SendGain は
    /// dangling」は [`Self::prune_dangling_param_targets`] の表が SSoT で、その深さを指す変調までの連鎖と
    /// 一緒に、daw_gui の SongDoc の口 (`enforce_edit_invariants`) が同じ undo step で掃除する。
    pub fn remove_track_send(&mut self, track_id: u32, send_id: u32) -> bool {
        let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return false;
        };
        let Some(pos) = t.sends.iter().position(|s| s.id == send_id) else {
            return false;
        };
        t.sends.remove(pos);
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
        let live = self.live_content_ids();
        self.clip_contents.retain(|id, _| live.contains(id));
        // Shared names follow content lifecycle: drop names whose
        // content_id no longer has any referencing clip.
        self.clip_content_names.retain(|id, _| live.contains(id));
    }

    /// クリップ (arrangement / launcher セル / track・song automation lane) から
    /// 到達できる `ContentId` の集合。 `gc_clip_contents` と [`Song::live_source_ids`] の
    /// 唯一の「生きている content」 判定。
    ///
    /// v35 (r.md #87): launcher のセルも `all_clips` 経由で「生きている」に数える。
    /// 数え落とすと保存のたびにセルの中身が GC で消える。 Song-level automation lanes
    /// (SongTempo / TimeSig master lanes) も同じ `clip_contents` store を使うので、
    /// 歩かないと tempo automation の curve が save 前に dead 判定されて次回 load で消える。
    pub fn live_content_ids(&self) -> std::collections::HashSet<ContentId> {
        let mut live: std::collections::HashSet<ContentId> = self
            .tracks
            .iter()
            .flat_map(Track::all_clips)
            .map(|c| c.content_id)
            .collect();
        for track in &self.tracks {
            for lane in &track.automation_lanes {
                for clip in lane.all_clips() {
                    live.insert(clip.content_id);
                }
            }
        }
        for lane in &self.song_lanes {
            for clip in lane.all_clips() {
                live.insert(clip.content_id);
            }
        }
        live
    }
}

/// serde `skip_serializing_if` 用: `u32` が 0 か。`Clip::speaker_id`
/// の「未採番は serialize しない」に使う。
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// serde `skip_serializing_if` 用: `i8` が 0 か (`Song::transpose` の「移調なしは書かない」)。
fn is_zero_i8(v: &i8) -> bool {
    *v == 0
}

/// serde `skip_serializing_if` 用: `f64` がちょうど 0 か。
/// `Clip::content_offset_beats` / `AutomationClip::content_offset_beats` の
/// 「trim していない clip は serialize しない」に使う (既定値と完全一致のときだけ省く
/// ので、丸め誤差で 0 近傍になった値は素直に書き出す)。
fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
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
#[cfg(test)]
mod native_tests;
