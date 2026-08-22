//! Automation (target / builtin param / curve / point / lane / clip key)
//!
//! arch-refactor #9 (god-file budget) で model.rs から分割。pure code movement で
//! 挙動・serialize 形式は不変。sibling 型は `use super::*` 経由で参照する。

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use super::*;

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
        /// v28 以前 migration 用 (deserialize 専用)。旧 save は chain 内 positional
        /// index、または旧 `slot: PluginSlot` (load 時 JSON 前処理
        /// `project::migrate_legacy_device_chains` が index へ解決) を持つ。
        /// `Song::ensure_ids` の remap pass が安定 device_id へ写像。
        #[serde(default, rename = "device_index", skip_serializing)]
        legacy_device_index: Option<u32>,
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
    /// v32: clip の左端が curve のどの拍に当たるか。意味・不変条件は
    /// [`Clip::content_offset_beats`](crate::model::Clip::content_offset_beats)
    /// と完全に同一 (`AutomationClip` は `Clip` と同形なので窓も対称)。
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub content_offset_beats: f64,
}

impl AutomationClip {
    /// content-local 拍 0 が置かれる song-absolute 拍。
    /// 意味は [`Clip::content_origin_beat`](crate::model::Clip::content_origin_beat) と同じ。
    #[must_use]
    pub fn content_origin_beat(&self) -> f64 {
        self.start_beat - self.content_offset_beats
    }

    /// content-local 拍 → song-absolute 拍。
    #[must_use]
    pub fn content_to_song_beat(&self, content_beat: f64) -> f64 {
        self.content_origin_beat() + content_beat
    }

    /// song-absolute 拍 → content-local 拍。
    #[must_use]
    pub fn song_to_content_beat(&self, song_beat: f64) -> f64 {
        song_beat - self.content_origin_beat()
    }

    /// この clip が見せる content の窓 `[offset, offset + length)` (content-local 拍)。
    #[must_use]
    pub fn content_window(&self) -> (f64, f64) {
        (
            self.content_offset_beats,
            self.content_offset_beats + self.length_beats,
        )
    }
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
