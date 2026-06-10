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
    AudioContent, AudioEvent, Clip, ClipContent, InstrumentSource, MidiContent, Note, SendMode,
    Song, Track,
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
    /// Vec position in `song.tracks`。 widget layout (= sequential x 位置 +
    /// peak display lookup) 用。 IPC / AppEvent では track_id を使うこと
    /// (= Phase 6 review で SSOT 違反を解消、 reorder race 防止)。
    pub index: u32,
    /// stable な Track::id。 mixer strip → AppEvent → MainToChild まで
    /// 一貫してこれで track を識別する。
    pub track_id: u32,
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
    /// このトラックが「リターン」 (= 他トラックの send 宛先) かどうか。
    /// `is_group` と同じく派生値で、`Track::kind` のような field は無い。
    /// mixer がリターンストリップを通常 strip と分けて描画するために使う。
    pub is_return: bool,
    /// このトラックの depth (parent_group_id を辿った段数)。 0 = master 直下、
    /// 1 = 1 段ネスト、… mixer strip / arrangement view が階層インデント描画に使う。
    pub depth: u8,
    /// このトラックの effective 色 (明示上書き or id 由来の導出パレット色、
    /// `track_color::effective_track_color`)。 mixer strip が左端の縦カラー
    /// ストライプに使う (arrangement header の色ストライプと同じ idiom)。
    /// master strip は色を持たず neutral 背景なので使わない。
    pub color: [f32; 3],
}

impl Default for TrackMixEntry {
    fn default() -> Self {
        Self {
            index: u32::MAX,
            track_id: u32::MAX,
            name: String::new(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            peak_l_raw: 0.0,
            peak_r_raw: 0.0,
            is_group: false,
            is_return: false,
            depth: 0,
            color: [0.5, 0.5, 0.5],
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
    /// `features` から導出した行き先カテゴリ (種別タグ表示 + 自動振り分け用)。
    pub category: PluginCategory,
}

/// 統合プラグインピッカー (`docs/plan_unified_plugin_picker.md`): プラグインの
/// `features` から導出する「行き先カテゴリ」。優先順 **note-effect > instrument >
/// audio-effect**、 どの主カテゴリも名乗らなければ `Fx` に倒す (= 未分類は FX
/// チェーンへ)。 VST3 の note-effect は Phase B (scan 時 bus 探り) で `features` に
/// `"note-effect"` が乗るまで判定不可なので、 現状 VST3 は instrument / audio-effect
/// の二択。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginCategory {
    Instrument,
    Fx,
    MidiFx,
}

impl PluginCategory {
    pub fn from_features(features: &[String]) -> Self {
        let has = |k: &str| features.iter().any(|f| f == k);
        if has("note-effect") {
            Self::MidiFx
        } else if has("instrument") {
            Self::Instrument
        } else {
            // "audio-effect" も、 どの主カテゴリも持たない未分類も FX チェーンへ。
            Self::Fx
        }
    }

    /// ピッカー行に出す種別タグ。
    pub fn tag(self) -> &'static str {
        match self {
            Self::Instrument => "楽器",
            Self::Fx => "FX",
            Self::MidiFx => "MIDI",
        }
    }
}

/// FIXME #29: プラグインが「どの役割になれるか」。 note 出力の有無は
/// [`common::plugin_db::PluginEntry`] の port 3 bool (probe で確定する SSoT) から、
/// 音源↔音声エフェクトの区別は従来どおり `features` (= 信頼できる宣言) から導出する。
/// 役割は最終的に「能力 + チェーン内の位置」で決まる ([`PluginCapability::allows_slot_kind`])。
/// picker の挿入先決定と inspector の並べ替え drop 検証が共有する (SSoT / DRY)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginCapability {
    /// note 出力あり・audio 出力なし (アルペジエータ等)。 生成器列 (midi_fx) 専用。
    PureMidiEffect,
    /// note 出力あり・audio 出力あり (例: Scaler 2)。 生成器にも音源にもなれる。
    DualRole,
    /// note 出力なし・音源 (feature=instrument)。 音源スロット専用。
    Instrument,
    /// note 出力なし・音声エフェクト (または未分類)。 audio-fx 列専用。
    AudioFx,
}

impl PluginCapability {
    pub fn from_entry(e: &common::plugin_db::PluginEntry) -> Self {
        match (e.has_note_output, e.has_audio_output) {
            (true, true) => Self::DualRole,
            (true, false) => Self::PureMidiEffect,
            // note 出力なし: 音声面の役割は feature で決める (probe より宣言が信頼できる)。
            (false, _) => {
                if e.features.iter().any(|f| f == "instrument") {
                    Self::Instrument
                } else {
                    Self::AudioFx
                }
            }
        }
    }

    /// この能力が占められるチェーン slot 種別 (`0=MidiFx`, `1=Instrument`, `2=Fx`)。
    /// inspector の並べ替え drop 検証が、 ポート的に有効な落とし先だけ許すのに使う。
    pub fn allows_slot_kind(self, slot_kind: u8) -> bool {
        match self {
            Self::PureMidiEffect => slot_kind == 0,
            Self::DualRole => slot_kind == 0 || slot_kind == 1,
            Self::Instrument => slot_kind == 1,
            Self::AudioFx => slot_kind == 2,
        }
    }
}

#[cfg(test)]
mod plugin_capability_tests {
    use super::PluginCapability;
    use common::plugin_db::PluginEntry;
    use common::plugin_format::PluginFormat;

    fn entry(features: &[&str], note_out: bool, audio_out: bool) -> PluginEntry {
        PluginEntry {
            id: "x".into(),
            format: PluginFormat::Clap,
            name: "X".into(),
            vendor: String::new(),
            version: String::new(),
            features: features.iter().map(|s| (*s).to_string()).collect(),
            path: std::path::PathBuf::from("x"),
            descriptor_index: 0,
            has_note_input: false,
            has_note_output: note_out,
            has_audio_output: audio_out,
        }
    }

    #[test]
    fn from_entry_maps_ports_and_features() {
        use PluginCapability::*;
        // dual-role (例: Scaler 2): note 出力 + audio 出力。
        assert_eq!(PluginCapability::from_entry(&entry(&["instrument"], true, true)), DualRole);
        // 純 MIDI FX: note 出力のみ。
        assert_eq!(PluginCapability::from_entry(&entry(&["note-effect"], true, false)), PureMidiEffect);
        // 音源: note 出力なし + instrument feature。
        assert_eq!(PluginCapability::from_entry(&entry(&["instrument"], false, true)), Instrument);
        // 音声エフェクト: note 出力なし + instrument でない。
        assert_eq!(PluginCapability::from_entry(&entry(&["audio-effect"], false, true)), AudioFx);
        // 未分類 (port も feature も無し) は audio fx 扱い。
        assert_eq!(PluginCapability::from_entry(&entry(&[], false, false)), AudioFx);
    }

    #[test]
    fn allows_slot_kind_matches_role() {
        use PluginCapability::*;
        // slot_kind: 0=MidiFx, 1=Instrument, 2=Fx。
        assert!(PureMidiEffect.allows_slot_kind(0));
        assert!(!PureMidiEffect.allows_slot_kind(1));
        assert!(!PureMidiEffect.allows_slot_kind(2));
        // dual-role は生成器(0)・音源(1) 両方可、 audio fx(2) は不可。
        assert!(DualRole.allows_slot_kind(0));
        assert!(DualRole.allows_slot_kind(1));
        assert!(!DualRole.allows_slot_kind(2));
        assert!(Instrument.allows_slot_kind(1));
        assert!(!Instrument.allows_slot_kind(0));
        assert!(!Instrument.allows_slot_kind(2));
        assert!(AudioFx.allows_slot_kind(2));
        assert!(!AudioFx.allows_slot_kind(0));
        assert!(!AudioFx.allows_slot_kind(1));
    }
}

impl PluginPickEntry {
    fn from_db_entry(e: &common::plugin_db::PluginEntry) -> Self {
        Self {
            id: e.id.clone(),
            name: if e.name.is_empty() {
                e.id.clone()
            } else {
                e.name.clone()
            },
            vendor: e.vendor.clone(),
            features: e.features.clone(),
            format_label: e.format.as_str().to_string(),
            category: PluginCategory::from_features(&e.features),
        }
    }
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
    // FIXME #15: scrubable_number に表示する現値 (= first event 代表値)。
    pub gain_db: f32,
    pub pan: f32,
    pub pitch_semitones: f32,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    /// fade scrub の range 上限 (= clip 長 beats)。
    pub clip_length_beats: f64,
}

/// Image event 単位 field の inspector 表示用 read snapshot
/// (`docs/plan_image_overlay.md` §4 P4)。 `selected_clip` が
/// `ClipContent::Image` の clip を指していて、 中に少なくとも 1 event
/// があれば `inspector_image_event_summary()` が `Some` を返す。
/// view は "Image Event" section を出し、 数値入力 (x/y/w/h/opacity) と
/// fade / mute toggle を first event 代表値として表示する。
///
/// `{x,y,w,h,opacity}_automated` は対応する `ImageBuiltin` lane が
/// 存在する (`Track.automation_lanes` 上の visible 不問) か。 lane が
/// あれば automate toggle は ON 表示で「もう一度押すと削除」、 無ければ
/// OFF 表示で「押すと lane を作る」 動作になる
/// (`docs/plan_image_automation.md` §4.3 / §5)。
#[derive(Debug, Clone, Copy)]
pub struct InspectorImageEventSummary {
    pub target: ClipRef,
    pub muted: bool,
    pub fade_in_curve: common::model::FadeCurve,
    pub fade_out_curve: common::model::FadeCurve,
    pub x_automated: bool,
    pub y_automated: bool,
    pub w_automated: bool,
    pub h_automated: bool,
    pub opacity_automated: bool,
    pub rotation_automated: bool,
    // FIXME #15: scrubable_number に表示する現値 (= first event 代表値)。
    // rotation は radians 保持 (view が degree 表示に変換)。
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub opacity: f32,
    pub rotation_radians: f32,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    /// fade scrub の range 上限 (= clip 長 beats)。
    pub clip_length_beats: f64,
}

/// docs/plan_text_overlay.md §4 P5: text inspector の編集対象 numeric
/// field 列挙。 image inspector が field 毎に個別 `SetClipImage*` event を
/// 持つのに対し、 text は 23 field と多いため `SetClipTextNumField` 1 event と
/// `TextNumField` discriminator で集約する。 FIXME #15 で inspector の数値
/// 入力は scrubable_number 化され、 on_change が直接 `SetClipTextNumField` を
/// 発火する (= 旧 `ClipTextNumEditChanged` / `CommitClipTextNumEdit` の
/// buffer 経路は撤去)。
///
/// FadeInBeats / FadeOutBeats は automation lane 対象外 (= `_automated`
/// は常に false)、 残 21 field は対応する `TextBuiltinParam` 経由で
/// lane override 可能。 `text_num_to_builtin` で `TextBuiltinParam` への
/// 対応 mapping を提供。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TextNumField {
    X,
    Y,
    W,
    H,
    Rotation,
    FontSize,
    Opacity,
    FillR,
    FillG,
    FillB,
    FillA,
    OutlineR,
    OutlineG,
    OutlineB,
    OutlineA,
    OutlineWidth,
    ShadowR,
    ShadowG,
    ShadowB,
    ShadowA,
    ShadowOffsetX,
    ShadowOffsetY,
    ShadowBlur,
    FadeInBeats,
    FadeOutBeats,
}

/// `TextNumField` → `TextBuiltinParam` mapping (lane-eligible のみ)。
/// FadeInBeats / FadeOutBeats は lane 対象外なので `None` を返す。
pub fn text_num_to_builtin(field: TextNumField) -> Option<common::model::TextBuiltinParam> {
    use TextNumField as F;
    use common::model::TextBuiltinParam as B;
    Some(match field {
        F::X => B::X,
        F::Y => B::Y,
        F::W => B::W,
        F::H => B::H,
        F::Rotation => B::Rotation,
        F::FontSize => B::FontSize,
        F::Opacity => B::Opacity,
        F::FillR => B::FillR,
        F::FillG => B::FillG,
        F::FillB => B::FillB,
        F::FillA => B::FillA,
        F::OutlineR => B::OutlineR,
        F::OutlineG => B::OutlineG,
        F::OutlineB => B::OutlineB,
        F::OutlineA => B::OutlineA,
        F::OutlineWidth => B::OutlineWidth,
        F::ShadowR => B::ShadowR,
        F::ShadowG => B::ShadowG,
        F::ShadowB => B::ShadowB,
        F::ShadowA => B::ShadowA,
        F::ShadowOffsetX => B::ShadowOffsetX,
        F::ShadowOffsetY => B::ShadowOffsetY,
        F::ShadowBlur => B::ShadowBlur,
        F::FadeInBeats | F::FadeOutBeats => return None,
    })
}

/// FIXME #15 (`docs/plan_inspector_scrub.md`): inspector の scrubable_number
/// で drag / text 編集中の field を識別する key。 group transform の
/// `group_scrub_active: Option<GroupTransformParam>` と同 idiom で、 各
/// scrubable の active edge を検知して `BeginInspectorScrub` /
/// `EndInspectorScrub` を 1 undo step に bracket する。 audio / image は
/// fixed な variant、 text は 25 numeric field を `TextNumField` で内包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorScrubField {
    Gain,
    Pan,
    Pitch,
    FadeIn,
    FadeOut,
    ImageX,
    ImageY,
    ImageW,
    ImageH,
    ImageOpacity,
    ImageRotation,
    ImageFadeIn,
    ImageFadeOut,
    Text(TextNumField),
}

/// docs/plan_text_overlay.md §4 P5: text inspector の read snapshot
/// (= image idiom)。 `selected_clip` が `ClipContent::Text` を指していて
/// 中に 1 event 以上あれば `inspector_text_event_summary()` が `Some` を
/// 返す。 `automated` は lane を持つ全 `TextBuiltinParam` の集合 (=
/// inspector 各 「A」 toggle が ON / OFF 判定に使う)。 align / fade curve
/// は dropdown 直値、 `muted` は toggle 直値。
#[derive(Debug, Clone)]
pub struct InspectorTextEventSummary {
    pub target: ClipRef,
    pub muted: bool,
    pub align: common::model::TextAlign,
    pub fade_in_curve: common::model::FadeCurve,
    pub fade_out_curve: common::model::FadeCurve,
    pub automated: std::collections::HashSet<common::model::TextBuiltinParam>,
    /// FIXME #15: scrubable_number に表示する現値の供給源 (= first event
    /// snapshot)。 `text_num_field_value` で field 毎の f64 を取り出す
    /// (Rotation は degree に変換)。
    pub event: common::model::TextEvent,
    /// fade scrub の range 上限 (= clip 長 beats)。
    pub clip_length_beats: f64,
}

impl InspectorTextEventSummary {
    /// FIXME #15: scrubable_number の `value` 引数に渡す現値を field 毎に
    /// 取り出す。 Rotation は内部 radians を degree に変換して返す
    /// (= 旧 text_input が degree 表示だったのと整合、 on_change で
    /// radians に戻す)。
    pub fn text_num_field_value(&self, field: TextNumField) -> f64 {
        use TextNumField as F;
        let ev = &self.event;
        match field {
            F::X => ev.x.into(),
            F::Y => ev.y.into(),
            F::W => ev.w.into(),
            F::H => ev.h.into(),
            F::Rotation => ev.rotation_radians.to_degrees().into(),
            F::FontSize => ev.font_size_px.into(),
            F::Opacity => ev.opacity.into(),
            F::FillR => ev.fill_color[0].into(),
            F::FillG => ev.fill_color[1].into(),
            F::FillB => ev.fill_color[2].into(),
            F::FillA => ev.fill_color[3].into(),
            F::OutlineR => ev.outline_color[0].into(),
            F::OutlineG => ev.outline_color[1].into(),
            F::OutlineB => ev.outline_color[2].into(),
            F::OutlineA => ev.outline_color[3].into(),
            F::OutlineWidth => ev.outline_width_px.into(),
            F::ShadowR => ev.shadow_color[0].into(),
            F::ShadowG => ev.shadow_color[1].into(),
            F::ShadowB => ev.shadow_color[2].into(),
            F::ShadowA => ev.shadow_color[3].into(),
            F::ShadowOffsetX => ev.shadow_offset_px.0.into(),
            F::ShadowOffsetY => ev.shadow_offset_px.1.into(),
            F::ShadowBlur => ev.shadow_blur_px.into(),
            F::FadeInBeats => ev.fade_in_beats,
            F::FadeOutBeats => ev.fade_out_beats,
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

/// 「＋ Send」 ボタンで開く宛先トラックピッカーの状態。 plugin_picker の
/// `is_plugin_picker_open` と同 idiom で、 開いている間 `Some(..)` を保持し、
/// `src_track_id` (= send 元) を覚えておく。 track_picker.rs がこれを見て
/// modal を開閉する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendPickerState {
    /// この track に新しい send を追加する (= send 元)。
    pub src_track_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
pub struct ClipRef {
    pub track: u32,
    pub clip: u32,
}

/// v18 (`docs/plan_track_clip_color.md`): color_picker overlay (gui_01 #058)
/// の編集対象。`Some` の間 arrangement_view が 1 フレームごとに
/// `ui.color_picker` を呼んで overlay を描画する。`Track` は track id、
/// `Clip` は index ベース `ClipRef`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPickerTarget {
    Track(u32),
    Clip(ClipRef),
}

/// gui_01 #028: 1 point の addressing。daw_01 側は `(track_id, lane_id,
/// clip_id, point_idx)` 4-tuple で持つ (gui_01 の `AutomationPointKey`
/// と 1:1 対応)。`AppEvent::DeleteAutomationPoints` などの batch event
/// で複数受ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AutomationPointKeyRef {
    pub track_id: u32,
    pub lane_id: u32,
    pub clip_id: u32,
    pub point_idx: u32,
}

/// gui_01 #028: `MoveAutomationPoints` 用の 1 point delta。`value_norm`
/// は normalized 0..1 (widget が cursor 座標から計算した値)、handler が
/// `lane.target` を引いて plain 単位に逆変換する。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveAutomationPointEntry {
    pub key: AutomationPointKeyRef,
    pub prev_time_beat: f64,
    pub prev_value_norm: f32,
    pub next_time_beat: f64,
    pub next_value_norm: f32,
}

/// gui_01 #028 (Phase 63n-3): `MoveAutomationClips` /
/// `CloneAutomationClipsLinked` / `CloneAutomationClipsIndependent` 用
/// の 1 clip delta。`from` source clip → `to_lane` の `next_start_beat`
/// 位置へ移動 / 共有コピー / 独立コピー。lane 跨ぎは target 不一致でも
/// 全 accept (Bitwig 流、`docs/plan_automation.md` §5.4)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveAutomationClipEntry {
    pub from: common::model::AutomationClipKey,
    pub to_lane: common::model::AutomationLaneKey,
    pub prev_start_beat: f64,
    pub next_start_beat: f64,
}

/// gui_01 #028 (Phase 63n-3): `ResizeAutomationClips` 用の 1 clip delta。
/// 左 edge drag は `next_start` + `next_len` 両方変動、右 edge drag は
/// `next_len` のみ変動 (`prev_start == next_start`)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResizeAutomationClipEntry {
    pub key: common::model::AutomationClipKey,
    pub prev_start: f64,
    pub prev_len: f64,
    pub next_start: f64,
    pub next_len: f64,
}

/// 立ち絵 group transform inspector 用 snapshot（automate トグルの点灯状態）。
/// `docs/plan_tachie_group_transform.md` §5.5。
pub struct GroupTransformInspectorSummary {
    pub track_id: u32,
    /// `group_param_index` 順に、各 param の `GroupTransform` lane の有無。
    pub automated: [bool; 8],
    /// 編集対象の base transform（`group_transform` 無しなら default）。
    /// inspector の scrubable_number が毎フレーム現値として表示する。
    pub transform: common::model::GroupTransform,
}

/// inspector / resync が走査する `GroupTransformParam` の固定順
/// （`group_param_index` の index と一致）。
pub const GROUP_PARAMS: [common::model::GroupTransformParam; 8] = {
    use common::model::GroupTransformParam as G;
    [
        G::X,
        G::Y,
        G::Rotation,
        G::ScaleX,
        G::ScaleY,
        G::AnchorX,
        G::AnchorY,
        G::Opacity,
    ]
};

/// `GroupTransformParam` の edit buffer / `automated` 配列 index（固定順）。
pub fn group_param_index(p: common::model::GroupTransformParam) -> usize {
    use common::model::GroupTransformParam as G;
    match p {
        G::X => 0,
        G::Y => 1,
        G::Rotation => 2,
        G::ScaleX => 3,
        G::ScaleY => 4,
        G::AnchorX => 5,
        G::AnchorY => 6,
        G::Opacity => 7,
    }
}

/// inspector ラベル / status 用の短い param 名。
pub fn group_param_label(p: common::model::GroupTransformParam) -> &'static str {
    use common::model::GroupTransformParam as G;
    match p {
        G::X => "X",
        G::Y => "Y",
        G::Rotation => "Rotation",
        G::ScaleX => "ScaleX",
        G::ScaleY => "ScaleY",
        G::AnchorX => "AnchorX",
        G::AnchorY => "AnchorY",
        G::Opacity => "Opacity",
    }
}

/// `GroupTransform` の指定 param の plain 値を取り出す。
fn group_transform_field(
    gt: &common::model::GroupTransform,
    p: common::model::GroupTransformParam,
) -> f32 {
    use common::model::GroupTransformParam as G;
    match p {
        G::X => gt.x,
        G::Y => gt.y,
        G::Rotation => gt.rotation_radians,
        G::ScaleX => gt.scale_x,
        G::ScaleY => gt.scale_y,
        G::AnchorX => gt.anchor_x,
        G::AnchorY => gt.anchor_y,
        G::Opacity => gt.opacity,
    }
}

/// gui_01 #028 §7.3: `AutomationTarget` に対する人間可読 display name。
/// Inspector の knob hint や status_message で使う。`Plugin Param N` は
/// Phase 2 で IPC 経由で実 plugin の param name に置換する。
pub fn automation_target_display_name(
    target: &common::model::AutomationTarget,
) -> String {
    use common::model::{AutomationTarget, ImageBuiltinParam, TrackBuiltinParam};
    match target {
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume) => "Volume".into(),
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan) => "Pan".into(),
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::Mute) => "Mute".into(),
        AutomationTarget::TrackBuiltin(TrackBuiltinParam::SendGain { send_idx }) => {
            format!("Send {}", send_idx + 1)
        }
        AutomationTarget::PluginParam { param_id, .. } => format!("Param {param_id}"),
        AutomationTarget::SongTempo => "Tempo".into(),
        AutomationTarget::SongTimeSigNumerator => "Time Sig".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::X) => "Image X".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Y) => "Image Y".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::W) => "Image W".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::H) => "Image H".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Opacity) => "Image Opacity".into(),
        AutomationTarget::ImageBuiltin(ImageBuiltinParam::Rotation) => "Image Rotation".into(),
        AutomationTarget::TextBuiltin(p) => {
            use common::model::TextBuiltinParam as T;
            match p {
                T::X => "Text X".into(),
                T::Y => "Text Y".into(),
                T::W => "Text W".into(),
                T::H => "Text H".into(),
                T::Opacity => "Text Opacity".into(),
                T::Rotation => "Text Rotation".into(),
                T::FontSize => "Text FontSize".into(),
                T::FillR => "Text Fill R".into(),
                T::FillG => "Text Fill G".into(),
                T::FillB => "Text Fill B".into(),
                T::FillA => "Text Fill A".into(),
                T::OutlineR => "Text Outline R".into(),
                T::OutlineG => "Text Outline G".into(),
                T::OutlineB => "Text Outline B".into(),
                T::OutlineA => "Text Outline A".into(),
                T::OutlineWidth => "Text Outline Width".into(),
                T::ShadowR => "Text Shadow R".into(),
                T::ShadowG => "Text Shadow G".into(),
                T::ShadowB => "Text Shadow B".into(),
                T::ShadowA => "Text Shadow A".into(),
                T::ShadowOffsetX => "Text Shadow OffsetX".into(),
                T::ShadowOffsetY => "Text Shadow OffsetY".into(),
                T::ShadowBlur => "Text Shadow Blur".into(),
            }
        }
        AutomationTarget::GroupTransform(p) => {
            use common::model::GroupTransformParam as G;
            match p {
                G::X => "Group X".into(),
                G::Y => "Group Y".into(),
                G::Rotation => "Group Rotation".into(),
                G::ScaleX => "Group ScaleX".into(),
                G::ScaleY => "Group ScaleY".into(),
                G::AnchorX => "Group AnchorX".into(),
                G::AnchorY => "Group AnchorY".into(),
                G::Opacity => "Group Opacity".into(),
            }
        }
    }
}

/// gui_01 #028 §7.3: 最後にユーザーが触った parameter。`A` キー
/// shortcut で「現在 selected_track の lane に追加」 する際の source。
/// session-only (起動時 None、Undo / save 対象外)。
///
/// `track_id` は Bitwig 流に「touched parameter が **属する track**」
/// (selected_track ではなく)。これにより別 track のプラグインを
/// inspector で触った後 `A` を押すと、その plugin が乗る track 上に
/// lane ができる (= 期待動作)。
#[derive(Debug, Clone)]
pub struct TouchedParam {
    pub track_id: u32,
    pub target: common::model::AutomationTarget,
    /// inspector の hint 表示や status_message で使う名前
    /// ("Volume" / "Pan" / "Cutoff (Serum)" 等)。
    pub display_name: String,
    /// 設定時刻。stale 判定 (= track / plugin が削除されたあとの自動
    /// クリア) 用。
    pub touched_at: std::time::Instant,
}

pub const ARRANGE_PX_PER_BEAT: f32 = 24.0;
pub const ARRANGE_TRACK_HEIGHT: f32 = 88.0;
pub const DEFAULT_NOTE_DURATION: f64 = 0.25;
pub const DEFAULT_CLIP_LENGTH: f64 = 4.0;
/// 鍵盤レーン click のプレビュー発音 velocity (MIDI 0..=127、 固定値)。
/// gui_01 #055: widget は押下 pitch のみ返すので velocity は daw_01 側で固定。
const PREVIEW_VELOCITY: u8 = 100;

/// Audio Editor zoom の最小 view span (beats)。 1/64 拍 = 約 0.015 beats。
/// これ未満は描画上意味がなく `view_len` を 0 に近づけると `beats_per_px`
/// が発散するので clamp。
pub const MIN_AUDIO_EDITOR_VIEW_LEN_BEATS: f64 = 1.0 / 64.0;

/// Phase 2 PR-C: 進行中の plugin-FX bounce の追跡 entry。
/// `MainToChild::BounceClipFxOnline` 発火時に `AppData::pending_clip_fx_bounce
/// = Some(...)` でセット、 `ChildToMain::BounceClipFxComplete` 受信で
/// 完了処理 (= 新 audio source + 新 track + 新 Clip 配置) → `None` に戻す。
/// `path` / `source_track` / `source_clip` は IPC echo back と pending entry
/// の identifier 照合に使う。 `clip_name` / `clip_length_beats` /
/// `start_beat` は完了時の新 track / 新 Clip の名前 / 配置に使う。
/// `source_path` は完了時に AudioSource として登録するときの
/// `AudioSourcePath` (= ProjectRelative or Absolute、 outpath が
/// `<project_dir>/bounce/...` か `bounce_cache/...` かで決まる)。
#[derive(Debug, Clone)]
pub struct PendingClipFxBounce {
    pub source_track: u32,
    pub source_clip: u32,
    pub out_path: PathBuf,
    pub source_path: common::model::AudioSourcePath,
    pub clip_name: String,
    pub clip_length_beats: f64,
    pub start_beat: f64,
}

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
    /// Video thumbnail RGBA8 staging area, keyed by `VideoSourceId`.
    /// Populated by `action_import_video` (P3.4); drained by the
    /// runner (P3.5) which calls `Renderer::create_texture` +
    /// `upload_texture_rgba` and inserts the resulting `TextureHandle`
    /// into [`Self::video_texture_cache`]. After a successful upload
    /// the entry here is dropped (= the texture lives in GPU memory).
    /// `(width, height, rgba)`; rgba length is `width * height * 4`.
    pub video_thumbnail_rgba:
        std::collections::HashMap<common::model::VideoSourceId, (u32, u32, std::sync::Arc<Vec<u8>>)>,
    /// `VideoSourceId`s queued for GPU texture upload. The runner
    /// drains this each frame.
    pub pending_thumbnail_uploads: Vec<common::model::VideoSourceId>,
    /// GPU-side video thumbnail textures keyed by `VideoSourceId`.
    /// Written by the runner (P3.5) after a successful texture upload;
    /// read by `arrangement_view.rs` (P3.6) and passed to
    /// `ArrangementClip.thumbnail`.
    pub video_texture_cache:
        std::collections::HashMap<common::model::VideoSourceId, daw_ui_renderer::TextureHandle>,
    /// v13 (`docs/plan_image_overlay.md` §P2): Image BGRA8 staging
    /// area, keyed by `ImageSourceId`. Populated by
    /// `action_import_image`; drained by the runner (P3) which calls
    /// `Renderer::create_texture_bgra` + `upload_texture_bgra` and
    /// inserts the resulting `TextureHandle` into
    /// [`Self::image_texture_cache`]. After upload the entry here is
    /// dropped (= the texture lives in GPU memory). `(width, height,
    /// bgra)`; bgra length is `width * height * 4`.
    pub image_source_bgra: std::collections::HashMap<
        common::model::ImageSourceId,
        (u32, u32, std::sync::Arc<Vec<u8>>),
    >,
    /// v13: `ImageSourceId`s queued for GPU texture upload. Drained by
    /// the runner each frame.
    pub pending_image_uploads: Vec<common::model::ImageSourceId>,
    /// v13: GPU-side image textures keyed by `ImageSourceId`. Written
    /// by the runner after upload, read by `preview_window.rs`
    /// composite pass and `arrangement_view.rs` thumbnail rendering.
    pub image_texture_cache: std::collections::HashMap<
        common::model::ImageSourceId,
        daw_ui_renderer::TextureHandle,
    >,
    /// docs/plan_video.md P4: video preview window の表示フラグ。 menu
    /// "View → Video Preview" / shortcut で toggle、 runner が毎フレーム
    /// この値を見て第二 winit::Window を create / destroy する。 false で
    /// 起動するので video import 前は preview は出ない (= MV 開始時は
    /// preview 不要、 user が import 後に明示的に開く)。
    pub preview_window_visible: bool,
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
    /// gui_01 #028 (M14 Phase 63n-1): automation lane 群を **展開中** の
    /// track id 集合 (Bitwig 流: 既定は折り畳み)。 含まれない track の
    /// `automation_lanes_collapsed = true` を widget へ渡す。 ▶/▼ click
    /// で `ToggleTrackAutomationCollapsed` イベント経由に insert/remove。
    /// プロジェクト保存対象ではない (= session-only): UI 状態は再起動で
    /// 既定 (全 collapsed) に戻る。
    pub expanded_automation_tracks: std::collections::HashSet<u32>,
    /// gui_01 #034 (Phase 63n-10): master row の automation 展開状態。
    /// `expanded_automation_tracks` と直交した 1 bool で持つ (= track id
    /// 集合に MASTER_TRACK_ID を入れる方式は sentinel が混ざって SSoT が
    /// 曖昧、 master 専用 field の方が intent が明瞭)。 起動時 false、
    /// `ToggleTrackAutomationCollapsed { track_id: MASTER_TRACK_ID }` で flip。
    /// session-only / Undo / save 対象外。
    pub master_row_automation_expanded: bool,
    /// gui_01 #028 (M14 Phase 63n-3): 選択中の automation clip。 MIDI
    /// clip 用 `selected_clips` と直交 (= 同時に両方を持てる、 他 DAW
    /// 互換)。 widget の `SelectAutomationClips` で上書き、 widget へは
    /// 毎フレーム `&[AutomationClipKey]` で渡して selected highlight を
    /// 描画させる。 session-only。
    pub selected_automation_clips: Vec<common::model::AutomationClipKey>,
    /// Phase 3 (`docs/plan_automation.md` §10): 選択中の automation point。
    /// gui_01 #033 で widget 側の lasso 矩形選択が landing するまで空のまま
    /// だが、 copy / paste / quantize / delete のハンドラは selection を
    /// 入力として動くので先行実装する。 widget からは
    /// `SelectAutomationPoints` (#033) で上書き。 session-only。
    pub selected_automation_points: Vec<AutomationPointKeyRef>,
    /// gui_01 #028 §7.3: 最後にユーザーが触った parameter。`A` キー
    /// shortcut で「対応 lane を所有 track に追加」 する source。
    /// session-only (起動 None、Undo / save 対象外)。
    pub last_touched_param: Option<TouchedParam>,
    /// Phase 4 (`docs/plan_automation.md` §6): automation recording mode。
    /// transport bar の 4 way toggle (Read / Touch / Latch / Write) で切替。
    /// session-only / Undo 対象外 (= 起動時 `Read`、 project 保存対象外)。
    /// 起動時の値は Bitwig / Ableton Live / Reaper と同じく `Read`。
    /// Phase 4 Step C+ で audio thread もこの値を読んで recording lane の
    /// curve eval をバイパスし、 GUI からの knob 値を `playhead_beat`
    /// 起点に point として書き込む。
    pub recording_mode: common::model::RecordingMode,
    /// Phase 7 B3 (2026-05-13): メトロノーム on/off。 transport bar の
    /// toggle button で切り替え、 `AppEvent::SetMetronomeEnabled(bool)` で
    /// 更新 → `MainToChild::SetMetronomeEnabled(bool)` を audio に送信。
    /// audio thread は内蔵 click 音 (sine + linear envelope decay、 accent:
    /// downbeat 880Hz / 他 440Hz) を master mix に重ねる。 起動時 default
    /// false。 session-only (project save には含めない)。
    pub metronome_enabled: bool,
    /// Phase 7 B4 Step D (2026-05-13): MIDI 録音中 flag。 Record toggle ON
    /// で true、 Stop / OFF で false。 true のとき handle_midi_note_on/off
    /// は armed track の MIDI clip に書き込む (= 既存 step-input mode は
    /// midi_recording == false のときのみ動作)。 session-only / Undo 対象外。
    pub midi_recording: bool,
    /// Phase 7 B4 Step C (2026-05-13): count-in 中で「0 拍到達まで preroll
    /// 待ち」 状態。 `start_recording` で `count_in_bars > 0` なら true、
    /// audio engine の `preroll_remaining_samples` mirror が 0 に達したら
    /// `on_tick` が `midi_recording_pending → midi_recording` 遷移させる。
    /// metronome は pending 中だけ強制 ON で click guide を流す。
    pub midi_recording_pending: bool,
    /// Phase 7 B4 Step C (2026-05-13): count-in bars (0 / 1 / 2)。 transport
    /// bar の dropdown で設定、 default 0。 session-only state。
    pub count_in_bars: u8,
    /// Phase 7 B4 Step D (2026-05-13): 直近 note_on の `(track_id, key) →
    /// start_beat`。 note_off 受信時に start_beat 取り出して `length_beats =
    /// playhead - start` を確定する。 stop / midi_recording 解除で clear。
    pub midi_recording_active_notes:
        std::collections::HashMap<(u32, u8), f64>,
    /// Phase 7 B4 Step C (2026-05-13): count-in 開始前の metronome_enabled
    /// 状態 snapshot。 count-in 中だけ強制 ON にし、 `stop_recording` 時に
    /// 元の値へ戻す (= user の「click off」 設定を尊重しつつ count-in 中は
    /// guide が聞こえる)。 None なら recording 開始前 = 復元不要。
    pub metronome_enabled_pre_recording: Option<bool>,
    /// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn の bind 待ち target。
    /// `Some` なら次に来る MIDI CC をこの target に bind (= `Song.midi_bindings`
    /// に追加 + None に戻す)。 `None` (default) なら CC は既存 binding lookup
    /// で target に値を流す (= 通常モード)。 transport bar の「MIDI Learn」
    /// button で `StartMidiLearn(target)` 経由で Some 化、 `CancelMidiLearn`
    /// or 1 度の CC 受信 (= bind 確定) で None に戻る。
    pub midi_learn_target: Option<common::model::BindingTarget>,
    /// Phase 4 Step B (`docs/plan_automation.md` §6): 現在 user が触っている
    /// (= dragging) parameter の集合。 mixer / inspector / lane default knob
    /// の press で insert、 release で remove。 plugin GUI 経由の gesture も
    /// CLAP `CLAP_EVENT_PARAM_GESTURE_BEGIN/END` IPC からここに反映する
    /// (Phase 2c の `PluginParamTouchedFromChild` は begin のみ送るので
    /// end の IPC 追加は Step B follow-up)。 session-only / Undo 対象外。
    /// Step C で audio thread はこの set を読んで該当 lane の curve eval
    /// を bypass する。 `latched_param_gestures` (= Latch mode 用に保持する
    /// "1 度触れた parameter") と組み合わせて、 Read/Touch/Latch/Write の
    /// 4 mode の挙動差を audio thread 側で実現する。
    pub active_param_gestures:
        std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    /// Phase 4 Step C (`docs/plan_automation.md` §6): `Latch` / `Write` mode
    /// で「再生中に 1 度でも触れた parameter」 を transport stop まで保持する
    /// set。 `ParamGestureBegin` が `is_playing == true` 中に発火すると
    /// 即時 insert され、 `stop()` で clear される。 `Touch` mode では使われ
    /// ない (= active_param_gestures だけが「現在 recording 中」 を意味する)。
    /// audio thread への通知は active ∪ latched の和集合を毎 tick 送る (Step
    /// C-2 で IPC `SetRecordingLanes` が landing したら lock-free 化、 当面
    /// は per-tick LoadSong で済ます)。 session-only / Undo 対象外。
    pub latched_param_gestures:
        std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    /// Phase 4 Step C: parameter ごとの「直近 record した beat」 を保持する
    /// throttle 用 map。 audio bridge tick は ~60Hz、 BPM=120 で 1/64 beat
    /// は ~31ms。 同 tick 内で同じ playhead に何度も point insert しない
    /// よう、 `playhead - last_beat >= 1/64` のときだけ insert する。
    /// `stop()` で clear。 session-only / Undo 対象外。
    pub recording_last_beat:
        std::collections::HashMap<(u32, common::model::AutomationTarget), f64>,
    /// Phase 4 Step C-2: 直近 `MainToChild::SetRecordingLanes` で audio thread
    /// に送った recording lane set のスナップショット。 GUI の currently
    /// recording set (= active ∪ latched, mode 依存) と diff を取って、 変化
    /// したときだけ IPC を送信する。 LoadSong は set が「縮んだ」 (= 1 度
    /// recording 終了した lane が出た) ときに送る (= audio thread が curve
    /// eval に戻るときに最新 points を読ませる)。 session-only / Undo 対象外。
    pub last_sent_recording_lanes:
        std::collections::HashSet<(u32, common::model::AutomationTarget)>,
    /// Phase 4 Step C-3 (`docs/plan_automation.md` §6): plugin GUI で knob 値が
    /// 変更されるたびに `PluginParamValueChangedFromChild` で受け取る最新値の
    /// cache。 `(track_id, slot, param_id) -> plain value`。 audio bridge tick
    /// で `current_plain_value(PluginParam)` がここから plain 値を引いて
    /// `AutomationPoint` を生成する。 session-only / Undo 対象外。 plugin
    /// reload で古い entry が残るが、 lane.target も同 plugin_id を持つので
    /// stale 値が誤って record されるリスクは低い (= 念のため Step C-3
    /// follow-up で plugin unload 時に該当 entry を消す)。
    pub plugin_param_values: std::collections::HashMap<
        (u32, common::protocol::PluginSlot, u32),
        f64,
    >,
    /// Phase 2 (`docs/plan_automation.md` §7.5): plugin parameter
    /// 一覧キャッシュ。 plugin host が `PluginParamList` IPC で送って
    /// くるたびに上書き。 `(track_id, slot)` で identify、 Parameter
    /// Picker (Phase 3+) / lane の label 解決 / norm↔plain 変換に
    /// 使う。 session-only (save 対象外、 plugin reload で再取得)。
    pub plugin_params: std::collections::HashMap<
        (u32, common::protocol::PluginSlot),
        Vec<common::protocol::PluginParamInfo>,
    >,
    /// gui_01 #031 (M14 Phase 63n-6): track ごとの row 高さ override。
    /// `Some(px)` で個別 track 高さ、`None` (= map に entry なし) で
    /// global default `arrange_track_row_h` を使う。 widget の Alt+drag
    /// or 下端 splitter drag で `SetSingleTrackRowH` 発火 → ここに反映。
    /// Alt+wheel は引き続き global を変える (`SetTrackRowH`)。
    /// session-only (= save / Undo 対象外、 必要になったら `Track.row_h`
    /// として model 化する)。
    pub track_row_overrides: std::collections::HashMap<u32, u16>,
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
    /// 選択 anchor (= 末尾)。 stable `ClipKey` (track_id + clip_id) 保持で
    /// 並べ替え / undo を跨いでも壊れない。 index 解決は `selected_clip_ref()`。
    pub selected_clip: Option<common::model::ClipKey>,
    /// 選択集合。 stable `ClipKey` 保持。 index 解決は `selected_clip_refs()`。
    pub selected_clips: Vec<common::model::ClipKey>,
    pub selected_notes: Vec<u32>,
    /// gui_01 #068: 前フレームに arrangement でホバーされた clip の
    /// `content_id` (= 連動ハイライトの active group 計算に使う held-value)。
    /// widget の `ArrangementResponse.hovered_clip` を毎フレーム解決して
    /// 保持する。 session-only。
    pub arrange_hover_content: Option<common::model::ContentId>,
    /// gui_01 #090: ポインタが今乗っている automation lane の key
    /// (`ArrangementResponse.hovered_automation_lane` を毎フレーム mirror)。
    /// Ctrl+A の「lane の全ポイント選択 → 全クリップへ段階拡大」振り分けに
    /// 使う。 `None` = clip 領域 / lane 外。 1 フレーム遅延だが pointer は
    /// 瞬間移動しないので実用上問題なし (= `arrange_hover_content` と同 idiom)。
    pub arrange_hovered_automation_lane: Option<common::model::AutomationLaneKey>,
    /// 鍵盤レーン click のプレビュー発音中の `(track_id, pitch)` (gui_01 #055,
    /// `docs/plan_pianoroll_keyboard_preview.md`)。 widget の
    /// `PianoRollResponse::keyboard_active_pitch` を前フレーム値と差分して
    /// note-on/off を導出するための held-value。 押下開始した track id を pitch
    /// と一緒に持つことで、 note-off を必ず note-on と同じ track へ送る
    /// (glissando / release で stuck note を防ぐ)。 `None` で発音なし。
    /// session-only (project save には含めない)。
    pub preview_note: Option<(u32, u8)>,

    // -------- View state --------
    pub bottom_panel: u8,
    /// `Some(target)` で Audio Editor (= clip ダブルクリックで開く波形
    /// 編集 view) が開いている。 bottom_panel の Piano Roll タブが
    /// audio_editor view に切り替わる (`docs/plan_audio_clip.md` §3.10
    /// 「piano_roll の領域を流用」)。 `None` なら通常の Piano Roll が
    /// 表示される。 audio clip ダブルクリックで `Some` 化、 Esc / Audio
    /// Editor close で `None` に戻る。
    pub audio_editor_clip: Option<ClipRef>,
    /// Audio Editor で選択中の event index 群 (`audio_editor_clip` の clip
    /// 内 events Vec への index)。 複数選択対応: click = 単一、 Shift+click =
    /// トグル、 空き領域 drag = 矩形選択、 Ctrl+A = 全選択。 空 Vec で
    /// 「未選択」。 anchor (= Inspector / footer / nav の代表) は last()
    /// (= `audio_editor_anchor_event`)。 編集 (gain/pan/fade 等) は選択集合
    /// 全体に broadcast (`audio_event_target_indices`)。 close で clear、
    /// undo でも clear (index は容易にずれるため、 ノート選択と同方針)。
    pub audio_editor_selected_events: Vec<usize>,
    /// Audio Editor 内のマウス hover 位置を clip 内 beat (clip 始端 = 0)
    /// に変換した値。 audio_editor.rs が毎フレーム push、 マウスが
    /// waveform 領域外なら `None`。 E キー (split) と将来の波形クリック
    /// 系操作で「マウス位置を cursor として使う」 ために保持する。
    pub audio_editor_hover_beat_in_clip: Option<f64>,
    /// Audio Editor の表示開始位置 (clip 始端からの offset、 beats 単位)。
    /// `OpenAudioEditor` で 0 にリセット、 wheel scroll / Ctrl+wheel zoom で
    /// 更新。 view 範囲は `[view_start_beat, view_start_beat + view_len_beats]`
    /// で、 0 ≤ view_start ≤ clip.length - view_len をホスト側で clamp。
    pub audio_editor_view_start_beat: f64,
    /// Audio Editor の表示 span (beats 単位)。 `OpenAudioEditor` で
    /// `clip.length_beats` にリセット (= 全体表示)。 Ctrl+wheel で zoom
    /// 倍率変更、 最小 `MIN_AUDIO_EDITOR_VIEW_LEN_BEATS` で clamp。
    pub audio_editor_view_len_beats: f64,
    pub arrange_zoom_x: f32,
    pub arrange_scroll_beat: f32,
    /// arrangement の縦 scroll offset (px、 smooth)。 `0.0` で first track
    /// が lanes 上端、 wheel scroll で増減。 widget 側で `SetTrackTop` を
    /// 発火するので handler がここに書き込む。 overscroll (lanes 領域
    /// 外への描画) の scissor は widget 側 (gui_01 #048) の責務。
    pub arrange_track_top: f32,
    /// arrangement の 1 track row 高さ (px)。Alt+wheel で 16..96 に縦ズーム。
    /// default は `ARRANGE_TRACK_HEIGHT`。
    pub arrange_track_row_h: f32,
    /// FIXME #16: arrangement の track header 幅 (px、 default 160.0)。 header と
    /// lanes の境界 (右端 splitter) drag で gui_01 arrangement widget が
    /// `SetHeaderW` を発火 → `SetArrangeHeaderW` 経由でここを更新する。 widget は
    /// 毎フレーム `view.header_w` としてこの値を読む。 session-only (= save /
    /// Undo 対象外、 `arrange_track_row_h` と同じ扱い)。
    pub arrange_header_w: f32,
    /// FIXME #23: inspector の param セクション (title 下〜chain 上) の実描画高さ
    /// (px)。 immediate-mode なので「前フレームに測った高さ」を `scroll_area` の
    /// content_size として使う (= lag-by-one)。 描画末尾で実測値に更新。
    /// session-only (save / Undo 対象外)。
    pub inspector_body_h: f32,
    pub pianoroll_zoom_x: f32,
    pub pianoroll_zoom_y: f32,
    pub pianoroll_top_pitch: u8,
    pub pianoroll_scroll_beat: f32,    /// FL Studio の smart length 互換: 直近に作成 / リサイズ / クリック選択した
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
    /// piano_roll がまだ一度も描画されていない (= `last_pianoroll_grid_size` 未測定)
    /// 状態で auto-fit が要求された場合に立つフラグ。 初回描画で grid_size が
    /// 確定したフレームの Edit 内で消費 → `fit_piano_roll_to_clip` を再実行する。
    /// これが無いと「Piano Roll タブ未表示で clip を選択 → タブを開いても fit
    /// されない、 2 回目以降のみ fit」 という初回 fit 喪失バグになる。
    pub pending_pianoroll_fit: bool,
    /// 同様に arrangement の lanes 領域サイズ (px)。
    pub last_arrange_canvas_size: (f32, f32),

    // -------- Playback / metering --------
    pub is_playing: bool,
    pub is_looping: bool,
    pub playhead_beat: Option<f32>,
    /// Pro Tools 流の「Stop で再生開始位置に戻す」 用、 直前の play()
    /// 開始時点の playhead を保持。 stop() で playhead_beat に書き戻し
    /// + SeekTo IPC で audio engine も同位置にリセットする。 None の
    ///   間 (= まだ一度も play していない or stop 済みで restore 完了) は
    ///   stop() は何もしない。
    pub playback_origin_beat: Option<f32>,
    pub master_gain: f32,
    pub peak_l_display: f32,
    pub peak_r_display: f32,
    pub peak_l_norm: f32,
    pub peak_r_norm: f32,

    // -------- Plugin database / picker --------
    pub plugin_db: Option<Arc<PluginDatabase>>,
    pub plugin_picker_entries: Vec<PluginPickEntry>,
    pub plugin_picker_visible: Vec<PluginPickEntry>,
    /// プラグインピッカーの検索ボックスに入力中の絞り込みクエリ。
    /// 1 文字毎に [`AppEvent::SetPluginPickerQuery`] で更新し、
    /// [`AppData::refresh_picker_visible`] で subsequence マッチに使う。
    pub plugin_picker_query: String,
    pub is_plugin_picker_open: bool,
    /// 検索結果リスト ([`plugin_picker_visible`]) 内のカーソル位置 (0-based)。
    /// `text_input` focus 中の ↑↓ (gui_01 #057 / Phase 86 `TextInputResponse::nav_up/nav_down`)
    /// で [`AppEvent::MovePluginPickerCursor`] を発火して移動し、 Enter で
    /// `plugin_picker_visible.get(cursor)` を確定する。 `refresh_picker_visible` が
    /// 呼ばれる度 (絞り込み再計算 / モーダル open / rescan 完了) に 0 にリセット。
    pub plugin_picker_cursor: usize,

    // -------- Font picker (Text クリップのフォント選択, FIXME #25) ----------
    /// `available_font_families()` で列挙したシステムフォント名 (キャッシュ)。
    /// 初回 open 時に background thread で 1 度だけ読む (~20-860ms)。
    pub font_picker_families: Vec<String>,
    /// 検索 + デフォルト行で絞り込んだ表示用リスト。先頭 `""` = renderer
    /// default (=「デフォルト」行)。
    pub font_picker_visible: Vec<String>,
    pub font_picker_query: String,
    pub font_picker_cursor: usize,
    pub is_font_picker_open: bool,
    /// background のフォント列挙が走行中。
    pub font_picker_loading: bool,
    /// 編集対象の text クリップ (open 時に anchor から確定)。
    pub font_picker_target: Option<ClipRef>,
    /// open 時の元フォント。cancel / commit の undo 復元元。
    pub font_picker_restore: String,

    /// 「＋ Send」 ボタンで開く宛先トラックピッカーの状態。 `Some` の間
    /// modal が開いており、 宛先選択 or 閉じる操作で `None` に戻る。
    /// plugin picker の `is_plugin_picker_open` と同 idiom。
    pub send_picker: Option<SendPickerState>,

    // -------- Save flow / IPC --------
    /// `RequestAllStates` を発行した順に保持するキュー。 front が現在 in-flight
    /// の request、 後続は先行 request の応答後に順次 dispatch される。 空の
    /// 間は新規 request を発行するときに即時 `RequestAllStates` を送る。
    /// 詳細は [`PendingStateRequest`] / [`DeferredEdit`]。
    pub pending_state_queue: VecDeque<PendingStateRequest>,
    pub audio_tx: Option<UnboundedSender<MainToChild>>,
    pub plugin_tx: Option<UnboundedSender<MainToChild>>,
    /// 子プロセス自動再起動 supervisor (`bootstrap::ChildSupervisor`)。
    /// production (GUI mode) では `Some`、 script / test 経路では `None`。
    /// `ChildDisconnected` event 受信時に `respawn(kind)` で新 child を
    /// spawn + handshake + Session/OpenWorkerPool 再送し、 新 tx で
    /// `audio_tx` / `plugin_tx` を差し替える。
    pub supervisor: Option<Arc<crate::bootstrap::ChildSupervisor>>,

    /// Phase 2 PR-C: plugin-FX bounce が進行中なら `Some`。 `None` で
    /// 新規 bounce を受け付ける。 同時 1 件のみ。 `MainToChild::
    /// BounceClipFxOnline` 発火時に `Some` 化、 `ChildToMain::
    /// BounceClipFxComplete` 受信で `None` に戻す + 新 track / 新 clip
    /// 配置。 path / source_track / source_clip は IPC echo back と
    /// pending entry を identifier 照合するために保持。
    pub pending_clip_fx_bounce: Option<PendingClipFxBounce>,
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
    /// ユーザーが plugin picker で手動追加した plugin の集合。load 完了
    /// (`on_plugin_loaded_from_child`) で consume し、(1) daw_audio へ `LoadSong` を
    /// 再送して新 plugin を signal path に入れ、(2) GUI 自動 open を `gui_open_requests`
    /// に queue する。`select_plugin_from_db` で (track_id, slot) を積む。
    /// プロジェクト読込時の一斉復元では積まれない (= project-open 時の初回 LoadSong が
    /// 全 chain を渡すので per-plugin の再 sync は不要、 GUI も自動 open しない)。
    pending_added_plugin_finalize: std::collections::HashSet<(u32, PluginSlot)>,
    /// load 完了して「いま開く」段になった GUI auto-open 要求の queue。runner の
    /// frame loop が `drain_pending_gui_opens` で消費し `open_slot_gui` を呼ぶ。
    /// handle_event (IPC 受信) から直接 window を作らず frame loop へ 1 フレーム
    /// 遅延させる seam (headless test は frame loop を回さない → window を作らない)。
    gui_open_requests: Vec<(u32, PluginSlot)>,
    /// `play()` was called while `pending_plugin_loads` was non-empty;
    /// re-fire it once the last `SlotPluginLoaded` arrives.
    pub pending_play: bool,

    // -------- Background workers --------
    pub synth_result: Arc<Mutex<Vec<common::voicevox::SynthResult>>>,
    pub rescan_result: Arc<Mutex<Option<PluginDatabase>>>,
    /// プロジェクトロード時の audio / image background decode の staging。
    /// `Some` の間は streaming load 進行中 (= 再生 gate + 進捗 overlay 表示、
    /// FIXME #24)。 `begin_asset_decode` で `Some`、 全件取り込みで `None`。
    pub asset_decode: Option<Arc<Mutex<AssetDecodeStaging>>>,
    /// 進捗 overlay 用 (done, total)。 draw で Mutex を取らずに済むよう
    /// `on_asset_decode_tick` がプレーン値で更新する。 `None` = 非表示。
    pub load_progress: Option<(usize, usize)>,
    /// 進捗 overlay のラベル (ロード / 走査で文言が違う)。`load_progress` が
    /// `Some` のときだけ使われる。
    pub load_progress_label: &'static str,
    /// VOICEVOX engine `/singers` の結果。 起動時に background thread が
    /// `AppEvent::SingersLoaded` で投入する。 engine 未起動 / fetch 失敗時は
    /// 空のまま (Track Inspector の dropdown は default singer のみ表示)。
    pub singers: Vec<common::voicevox::VoiceVoxSinger>,
    /// `singers` から派生した Vocal speaker dropdown のキャッシュ。
    /// `vocal_speaker_entries` は `(speaker_id, style_name)` の逆引き、
    /// `vocal_speaker_labels` は「<キャラ> - <スタイル>」表示ラベル。
    /// `singers` 投入時 (`SingersLoaded`) にのみ再構築し、 Track Inspector
    /// 描画の毎フレーム `format!` / `clone` を消す (singers は engine 起動時に
    /// 一度設定されて以降ほぼ不変なので、 派生キャッシュが SSoT として安定)。
    pub vocal_speaker_entries: Vec<(u32, String)>,
    pub vocal_speaker_labels: Vec<String>,
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
    /// 口パク (lip-sync) 自動再生成の debounce 用世代カウンタ。song 変更で
    /// bump し、`mark_lipsync_dirty` が timer thread に値を渡す。timer 発火時に
    /// 値が一致していれば (= それ以降変更なし) 再生成する (rapid 編集を coalesce)。
    pub lipsync_gen: u64,
    pub is_rescanning: bool,
    pub status_message: String,

    /// rename 中の track の **安定 ID** (positional index ではない)。 index で持つと
    /// track の reorder / delete で別 track に rename がすり替わる SSoT 違反になる
    /// (2026-06-09 の「最上段だけ rename できない / フリーズ」バグの原因)。 None で非 rename。
    pub track_rename_id: Option<u32>,
    pub track_rename_text: String,

    /// 編集中の clip rename。 `Some` のとき該当 clip rect に inline
    /// text_input を重ね描きする (track rename の clip 版)。 `ClipRef` は
    /// index ベースなので rename mode 中の track/clip reorder は track
    /// rename と同様に想定しない。
    pub clip_rename: Option<ClipRef>,
    pub clip_rename_text: String,

    /// v18 (`docs/plan_track_clip_color.md`): color_picker (gui_01 #058) の
    /// 開いている編集対象。`None` で非表示。`open_color_picker` で `Some` に、
    /// picker の `dismissed` で `None` に戻す。
    pub color_picker_target: Option<ColorPickerTarget>,
    /// color_picker overlay の anchor 矩形 (popup を出す基準位置)。開いた場所
    /// (右クリックした header / clip rect、inspector のスウォッチ rect) を保持し、
    /// どの view から開いても同じ位置に popup が出るようにする。
    pub color_picker_anchor: Option<daw_ui_renderer::Rect>,
    /// color_picker session 中に既に undo snapshot を取ったか。`open_color_picker`
    /// で `false` にリセットし、最初の色変更 (`SetTrackColor`/`SetClipColor`) で
    /// 1 度だけ snapshot を取って `true` にする。これで「drag 開始〜終了」 が
    /// 1 undo step にまとまり、 変更しないまま閉じても dead step が増えない。
    pub color_picker_session_dirty: bool,

    /// gui_01 #071: 空きレーン右クリック (`ArrangementEditRequest::SecondaryClickEmpty`)
    /// で開く clip 生成コンテキストメニューの stash。`Some((track_id, snap 済み beat,
    /// 右クリック viewport pos))` の間、毎フレーム `ui.context_menu_at` で `pos` に
    /// メニューを描画する (color_picker overlay と同 idiom)。on_select (= Text クリップ
    /// 生成) で `None` に戻す。
    pub clip_create_menu: Option<(u32, f64, (f32, f32))>,
    /// 上記メニューの 1-shot open trigger。`SecondaryClickEmpty` 受信 Edit で `true` に
    /// し、overlay が `open_at = Some(pos)` を 1 フレームだけ渡したら `false` に戻す
    /// (毎フレーム `Some` を渡すと outside-click で閉じても翌フレーム再 open するため)。
    pub clip_create_menu_open: bool,

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
    /// FIXME #15: audio / image inspector の数値 field は scrubable_number
    /// (drag + type) 化されたため、 個別の名前付き edit-buffer
    /// (`clip_gain_db_edit_text` 等 / `clip_image_*_edit_text`) は撤去
    /// (scrubable が編集状態を自前で内包)。 `clip_edit_buffer_target` は
    /// content / font_family 文字列 buffer の resync 判定に引き続き使う。
    pub clip_edit_buffer_target: Option<ClipRef>,

    /// v19 (`docs/plan_tachie_group_transform.md` §5.5): inspector の
    /// scrubable_number で transform を drag / text 編集中の param。drag・編集の
    /// 開始/終了 edge を検知して `BeginGroupTransformDrag` / `End` を発火し、
    /// 一連の操作を undo 1 step に bracket するための tracker（`None` = idle）。
    pub group_scrub_active: Option<common::model::GroupTransformParam>,

    /// FIXME #15 (`docs/plan_inspector_scrub.md`): audio / image / text
    /// inspector の scrubable_number で drag / text 編集中の field。 drag・
    /// 編集の開始/終了 edge を検知して `BeginInspectorScrub` /
    /// `EndInspectorScrub` を発火し、 一連の操作を undo 1 step に bracket
    /// する tracker（`None` = idle）。 group_scrub_active と同 idiom。
    pub inspector_scrub_active: Option<InspectorScrubField>,

    /// Video export の進捗 `(done_frames, total_frames)`。background thread が
    /// `ExportProgress` で更新。`None` = export 非実行。UI が進捗オーバーレイの
    /// 表示判定に使う。
    pub export_progress: Option<(u64, u64)>,
    /// 実行中 export のキャンセルフラグ。UI の Cancel ボタンで `true` にすると
    /// render loop が次フレームで中断し出力を破棄する。`None` = export 非実行。
    pub export_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 1 ステップ video export 中、 音声レンダリング完了待ちの mp4 出力先。
    /// export ダイアログで mp4 を選ぶ → 音声を temp WAV へ自動レンダリング →
    /// `ExportWavComplete` でこの mp4 へ video export（+ WAV mux）を開始する。
    /// `None` = 音声レンダリング待ちでない。
    pub pending_video_export: Option<std::path::PathBuf>,
    /// 自動レンダリングした音声 temp WAV。video export 完了後に削除する。
    pub export_temp_wav: Option<std::path::PathBuf>,

    /// docs/plan_text_overlay.md §4 P5: text inspector の文字列 edit buffer。
    /// `text` / `font_family` は文字列 field なので text_input のまま
    /// standalone (= scrubable 化されない)。 Enter / focus 喪失で
    /// `CommitClipText{Content,FontFamily}Edit` を発火。 FIXME #15 で
    /// 25 numeric field は scrubable_number 化され、 `clip_text_num_edits`
    /// HashMap は撤去 (scrubable が編集状態を自前で内包)。
    pub clip_text_content_edit_text: String,
    pub clip_text_font_family_edit_text: String,

    pub undo_stack: VecDeque<Song>,
    pub redo_stack: VecDeque<Song>,
    /// 最後に save / load / recovery / new した時点の Song 内容 (= dirty 判定の
    /// SSoT)。 `is_dirty` は本質的に `self.song != self.saved_song` の派生で、
    /// Undo/Redo で内容がこのベースラインに戻れば自動で clean になり、 タイトルの
    /// `*` が消える (`recompute_dirty` / `reset_saved_baseline` 参照)。
    saved_song: Song,

    pub is_help_open: bool,

    /// per-user データディレクトリ (recent / recent_saved / recovery /
    /// window_state の永続化先) の **Single Source of Truth**。 production は
    /// `AppDirs::production()` (= `%LOCALAPPDATA%/daw_01/`)、 test は
    /// `AppDirs::under(tempdir)` か `None`。 `None` は「永続化しない」 を
    /// 意味し、 実ユーザー状態を汚染しない (= dispatcher と同じ DI パターン)。
    pub app_dirs: Option<common::app_dirs::AppDirs>,
    /// 「最近開いたファイル」 (= Open ダイアログ / OpenRecent 経由で読み込んだ
    /// .daw)。 File メニュー「Open Recent ►」 に表示。 永続化先は
    /// `app_dirs.recent()` (= `%LOCALAPPDATA%/daw_01/recent.json`)。
    pub recent_files: common::recent::RecentFiles,
    /// 「最近保存したファイル」 (= Save / Save As で書き込んだ先)。 File
    /// メニュー「Recently Saved ►」 に表示。 永続化先は
    /// `app_dirs.recent_saved()` (= `%LOCALAPPDATA%/daw_01/recent_saved.json`)。
    /// 開いた履歴と分離して「保存先だけ覚えておく」 UX を提供する。
    pub recent_saved: common::recent::RecentFiles,
    /// `recent_files` の filename だけ抽出したキャッシュ。 gui_01 `menu_bar`
    /// API が label に `&'a str` を要求し、 'a が `Ui` の borrow 寿命
    /// (= `&AppData` の寿命) と一致するため、 label 文字列も AppData 内に
    /// 持っておく必要がある。 frame 内で `&app.recent_files_labels[i]` を
    /// 渡せば lifetime が解決する。 `push_recent` / load 時に更新。
    pub recent_files_labels: Vec<String>,
    /// `recent_saved` の filename キャッシュ。 同じ理由。
    pub recent_saved_labels: Vec<String>,

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
    /// 未保存変更がある状態でウィンドウを閉じようとしたとき表示する
    /// 「保存して終了 / 保存せず終了 / キャンセル」 確認モーダル
    /// (`close_confirm_modal`)。 `request_close` で is_dirty なら立てる。
    pub show_close_confirm: bool,
    /// Runner が毎フレーム監視し、 `true` になったら cleanup して
    /// event loop を抜ける終了フラグ。 not-dirty close / 「保存せず終了」 /
    /// 保存完了 (sync or async) のいずれかで立つ。
    pub should_quit: bool,
    /// 「保存して終了」 を選んだが plugin state 取得待ちで save が非同期
    /// (`PendingStateRequest::Save`) になっている間 `true`。
    /// `on_all_states_from_child` で save が完了 (is_dirty=false) したら
    /// `should_quit` に変える。 save 試行が終われば (= pending Save が
    /// 消えれば) クリアする (後続の手動 save が誤って quit しないように)。
    pub quit_after_save: bool,
    pub is_dragging: bool,
    pub midi_input_label: String,

    pub step_cursor_beat: f64,
    pub step_size_beats: f64,

    /// Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw toggle。 ON のとき
    /// piano_roll で note 追加時の pitch を `Song.scale_at(beat).snap(pitch)` で
    /// in-scale に寄せる。 piano_roll header の toggle で切替、 session-only
    /// state (project save しない)。 Highlight mode が前提 (Fold mode は
    /// widget 側で既に in-scale pitch を push する)。
    pub snap_on_draw: bool,
    /// Phase 7 B5 (`docs/plan_scale.html` §4.4): piano_roll が Fold mode か。
    /// `true` で out-of-scale 行を非表示 (Ableton K キー Fold to Scale 相当)、
    /// `false` で Highlight mode (root 行強調 + in-scale 通常 + out 行 dim)。
    /// piano_roll snap toolbar の「Fold」 toggle で切替、 session-only state。
    /// `Song.scale_changes` が空のときは `view.scale = None` で機能 OFF。
    pub piano_roll_fold: bool,
    /// Phase 7 B5 (`docs/plan_scale.html` §5.2): Snap Live Input toggle。 ON
    /// のとき MIDI 録音中の note_on pitch を `Song.scale_at(playhead).snap(pitch)`
    /// で in-scale に寄せる。 transport bar の toggle で切替、 session-only
    /// state。 step input (recording 停止中の MIDI input) には適用しない
    /// (= pitch を「聞いて」 決める用途、 Cubase / Bitwig も同方針)。
    pub snap_live_input: bool,

    /// 背景スレッド (autosave / playhead poll / MIDI / IPC bridge / VOICEVOX
    /// 合成 / plugin DB rescan) からメインスレッドへ `AppEvent` を送るための
    /// dispatcher。 production は `WinitDispatcher` (winit `EventLoopProxy`
    /// ラップ)、 test は `RecordingDispatcher` (Mutex<Vec> に蓄積)。
    pub event_proxy: Arc<dyn BackgroundDispatcher>,
}

impl AppData {
    // DI composition root: 全ての外部依存 (IPC sender / dispatcher / job /
    // plugin DB / supervisor / app_dirs) を注入する。 依存数が clippy の
    // 7-arg 閾値を超えるが、 composition root の性質上自然なので allow。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        audio_tx: UnboundedSender<MainToChild>,
        plugin_tx: UnboundedSender<MainToChild>,
        // 将来的な auto-select 用に予約。現在は song に反映していない。
        _clap_plugin_path: Option<PathBuf>,
        plugin_db: Option<Arc<PluginDatabase>>,
        event_proxy: Arc<dyn BackgroundDispatcher>,
        voicevox_job: Arc<dyn JobDispatcher>,
        supervisor: Option<Arc<crate::bootstrap::ChildSupervisor>>,
        app_dirs: Option<common::app_dirs::AppDirs>,
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
        let recovery_candidates = app_dirs
            .as_ref()
            .map(|d| common::recovery::scan_recovery_files(&d.recovery_dir()))
            .unwrap_or_default();
        let recent_files =
            load_recent_list(app_dirs.as_ref().map(|d| d.recent()));
        let recent_saved =
            load_recent_list(app_dirs.as_ref().map(|d| d.recent_saved()));
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
                let mut v: Vec<PluginPickEntry> =
                    db.entries.iter().map(PluginPickEntry::from_db_entry).collect();
                v.sort_by_key(|e| e.name.to_lowercase());
                v
            })
            .unwrap_or_default();

        let app = Self {
            song,
            file_path: None,
            audio_source_cache: AudioSourceCache::new(),
            video_thumbnail_rgba: std::collections::HashMap::new(),
            pending_thumbnail_uploads: Vec::new(),
            video_texture_cache: std::collections::HashMap::new(),
            image_source_bgra: std::collections::HashMap::new(),
            pending_image_uploads: Vec::new(),
            image_texture_cache: std::collections::HashMap::new(),
            preview_window_visible: false,
            arrangement_hover_beat: None,
            arrangement_hover_beat_raw: None,
            arrangement_hover_clip: None,
            selected_track_ids: Vec::new(),
            collapsed_groups: std::collections::HashSet::new(),
            expanded_automation_tracks: std::collections::HashSet::new(),
            master_row_automation_expanded: false,
            selected_automation_clips: Vec::new(),
            selected_automation_points: Vec::new(),
            last_touched_param: None,
            recording_mode: common::model::RecordingMode::default(),
            metronome_enabled: false,
            midi_recording: false,
            midi_recording_pending: false,
            count_in_bars: 0,
            midi_recording_active_notes: std::collections::HashMap::new(),
            metronome_enabled_pre_recording: None,
            midi_learn_target: None,
            active_param_gestures: std::collections::HashSet::new(),
            latched_param_gestures: std::collections::HashSet::new(),
            recording_last_beat: std::collections::HashMap::new(),
            last_sent_recording_lanes: std::collections::HashSet::new(),
            plugin_param_values: std::collections::HashMap::new(),
            plugin_params: std::collections::HashMap::new(),
            track_row_overrides: std::collections::HashMap::new(),
            track_plugin_ids: std::collections::HashMap::new(),
            loaded_slots: std::collections::HashMap::new(),
            plugin_latencies: std::collections::HashMap::new(),
            selected_clip: None,
            selected_clips: Vec::new(),
            selected_notes: Vec::new(),
            arrange_hover_content: None,
            arrange_hovered_automation_lane: None,
            bottom_panel: 0,
            audio_editor_clip: None,
            audio_editor_selected_events: Vec::new(),
            audio_editor_hover_beat_in_clip: None,
            audio_editor_view_start_beat: 0.0,
            audio_editor_view_len_beats: 0.0,
            arrange_zoom_x: ARRANGE_PX_PER_BEAT,
            arrange_scroll_beat: 0.0,
            arrange_track_top: 0.0,
            arrange_track_row_h: ARRANGE_TRACK_HEIGHT,
            arrange_header_w: 160.0,
            inspector_body_h: 800.0,
            pianoroll_zoom_x: 64.0,
            pianoroll_zoom_y: 14.0,
            pianoroll_top_pitch: 84, // C6
            pianoroll_scroll_beat: 0.0,            last_note_duration_beats: DEFAULT_NOTE_DURATION,
            preview_note: None,
            pianoroll_snap_enabled: true,
            pianoroll_snap_choice: crate::view::snap::CHOICE_PIANOROLL_DEFAULT,
            arrange_snap_enabled: true,
            arrange_snap_choice: crate::view::snap::CHOICE_ARRANGE_DEFAULT,
            last_pianoroll_grid_size: (0.0, 0.0),
            pending_pianoroll_fit: false,
            last_arrange_canvas_size: (0.0, 0.0),
            is_playing: false,
            is_looping: false,
            playhead_beat: None,
            playback_origin_beat: None,
            master_gain: 1.0,
            peak_l_display: 0.0,
            peak_r_display: 0.0,
            peak_l_norm: 0.0,
            peak_r_norm: 0.0,
            plugin_db,
            plugin_picker_entries,
            plugin_picker_visible: Vec::new(),
            plugin_picker_query: String::new(),
            is_plugin_picker_open: false,
            plugin_picker_cursor: 0,
            font_picker_families: Vec::new(),
            font_picker_visible: Vec::new(),
            font_picker_query: String::new(),
            font_picker_cursor: 0,
            is_font_picker_open: false,
            font_picker_loading: false,
            font_picker_target: None,
            font_picker_restore: String::new(),
            send_picker: None,
            pending_state_queue: VecDeque::new(),
            audio_tx: Some(audio_tx),
            plugin_tx: Some(plugin_tx),
            pending_clip_fx_bounce: None,
            #[cfg(windows)]
            plugin_host_windows: HashMap::new(),
            track_peak_display: initial_peak_display,
            pending_plugin_loads: std::collections::HashSet::new(),
            pending_added_plugin_finalize: std::collections::HashSet::new(),
            gui_open_requests: Vec::new(),
            pending_play: false,
            synth_result: Arc::new(Mutex::new(Vec::new())),
            rescan_result: Arc::new(Mutex::new(None)),
            asset_decode: None,
            load_progress: None,
            load_progress_label: "",
            singers: Vec::new(),
            vocal_speaker_entries: Vec::new(),
            vocal_speaker_labels: Vec::new(),
            voicevox_cache: Arc::new(Mutex::new(common::voicevox_cache::VoiceVoxCache::new())),
            voicevox_job,
            supervisor,
            voicevox_launch_attempted: false,
            lipsync_gen: 0,
            is_rescanning: false,
            status_message: String::new(),
            track_rename_id: None,
            color_picker_target: None,
            color_picker_anchor: None,
            color_picker_session_dirty: false,
            clip_create_menu: None,
            clip_create_menu_open: false,
            track_rename_text: String::new(),
            clip_rename: None,
            clip_rename_text: String::new(),
            bpm_edit_text: format!("{initial_bpm:.1}"),
            time_sig_num_edit_text: initial_time_sig_num.to_string(),
            clip_edit_buffer_target: None,
            clip_text_content_edit_text: String::new(),
            clip_text_font_family_edit_text: String::new(),
            group_scrub_active: None,
            inspector_scrub_active: None,
            export_progress: None,
            export_cancel: None,
            pending_video_export: None,
            export_temp_wav: None,
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            // 実値は new 末尾で `app.saved_song = app.song.clone()` に上書き
            // する (初期 song は 1 track 持ちで Song::default と異なるため、
            // ここで default を入れたままだと起動直後に spurious dirty になる)。
            saved_song: Song::default(),
            is_help_open: false,
            app_dirs,
            recent_files,
            recent_saved,
            // 初期 label cache は new() の末尾でまとめて初期化する。 Self
            // literal の途中で他 field を参照できないので、 一旦 empty で
            // 構築し、 caller 側で `init_recent_labels` を呼ばずに済むよう
            // make_app! / fn new の末尾で埋める (= line ~810 付近の
            // 「app.init_recent_labels()」 を見よ)。
            recent_files_labels: Vec::new(),
            recent_saved_labels: Vec::new(),
            is_dirty: false,
            last_autosave: std::time::Instant::now(),
            recovery_session_id: common::recovery::new_session_id(),
            recovery_candidates,
            show_recovery_modal,
            show_close_confirm: false,
            should_quit: false,
            quit_after_save: false,
            is_dragging: false,
            midi_input_label: String::new(),
            step_cursor_beat: 0.0,
            step_size_beats: DEFAULT_NOTE_DURATION,
            snap_on_draw: false,
            snap_live_input: false,
            piano_roll_fold: false,
            event_proxy,
        };
        // recent_files / recent_saved の path 列から filename label cache を
        // 1 回構築。 push_recent / push_recent_saved 経由の更新でも自動的に
        // 同期されるので、 初回のみここで初期化する。
        let mut app = app;
        app.init_recent_labels();
        // FIXME #29: cache が旧 port-probe 版 (PluginEntry の 3 bool 未取得) なら、
        // 起動時に 1 回だけ自動で再 probe (rescan) して port 構成を埋める。 production
        // (app_dirs=Some) のみ — test は app_dirs=None なので実システム scan を避ける。
        if app.app_dirs.is_some()
            && app
                .plugin_db
                .as_ref()
                .is_some_and(|db| db.needs_port_probe())
        {
            tracing::info!("plugin cache predates port-probe; auto-rescanning to fill port info");
            app.begin_rescan();
        }
        // 起動直後の song (1 track 持ち) を保存ベースラインに確定する。
        // is_dirty は literal で false 初期化済 (song == saved_song)。
        app.saved_song = app.song.clone();
        app
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
        crate::group_compose::is_group_track(&self.song, track_id)
    }

    /// A track acts as a "return" iff at least one other track has a
    /// `Send` whose `dest_track_id` points at it. Purely derived (no
    /// `Track::kind`), mirroring `is_group_track`. SSOT (CLAUDE.md).
    pub fn is_return_track(&self, track_id: u32) -> bool {
        self.song
            .tracks
            .iter()
            .flat_map(|t| t.sends.iter())
            .any(|s| s.dest_track_id == track_id)
    }

    /// 「＋ Send」 ピッカーに出す宛先候補 `(track_id, display_name)`。
    /// `src_track_id` 自身は除外し、 加えて「その宛先が send 辺で
    /// (直接 / 間接に) `src` に戻ってくる」 = ルーティング閉路を作る track
    /// も除外する。 閉路判定は send グラフ上で `dest` から `src` への
    /// 到達可能性を BFS で見る (= `dest` を起点に send を辿って `src` に
    /// 着けば、 `src -> dest` を足すと閉路になる)。 schedule compiler 側も
    /// 閉路を弾くが、 GUI で予め隠すことで誤操作を防ぐ。
    pub fn send_destination_candidates(&self, src_track_id: u32) -> Vec<(u32, String)> {
        // dest を起点に send 辺を辿って src に到達するか。 到達するなら
        // src -> dest は閉路を成すので候補から除く。
        let creates_cycle = |dest: u32| -> bool {
            if dest == src_track_id {
                return true;
            }
            let mut stack = vec![dest];
            let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
            while let Some(cur) = stack.pop() {
                if cur == src_track_id {
                    return true;
                }
                if !seen.insert(cur) {
                    continue;
                }
                if let Some(t) = self.song.track_by_id(cur) {
                    for s in &t.sends {
                        stack.push(s.dest_track_id);
                    }
                }
            }
            false
        };
        self.song
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.id != src_track_id && !creates_cycle(t.id))
            .map(|(i, t)| {
                let name = if t.name.is_empty() {
                    format!("Track {}", i + 1)
                } else {
                    t.name.clone()
                };
                (t.id, name)
            })
            .collect()
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
        // Phase 6 review perf (E10): 旧コードは各 track ごとに
        // `is_group_track(t.id)` (= O(N) all-tracks scan) +
        // `compute_track_depth(t)` (= O(depth) parent chain walk) を呼び、
        // 合計 O(N²) per frame だった。 大型 song で 60fps drop。
        // 単一 pass で is_group_set / depths を batch 計算して O(N) に。
        let n_tracks = self.song.tracks.len();
        let mut is_group_set: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(n_tracks);
        // リターン判定も同 pass で batch 集計 (= is_group と同 idiom)。
        // ある track に向けて 1 本でも send があれば、 その宛先はリターン。
        let mut is_return_set: std::collections::HashSet<u32> =
            std::collections::HashSet::with_capacity(n_tracks);
        let mut id_to_parent: std::collections::HashMap<u32, Option<u32>> =
            std::collections::HashMap::with_capacity(n_tracks);
        for t in &self.song.tracks {
            id_to_parent.insert(t.id, t.parent_group_id);
            if let Some(pid) = t.parent_group_id {
                is_group_set.insert(pid);
            }
            for s in &t.sends {
                is_return_set.insert(s.dest_track_id);
            }
        }
        // depth は parent chain を walk するが、 lookup を `id_to_parent`
        // HashMap で O(1) 化 (= 旧 `track_by_id` の line search O(N) を削減)。
        // 32 hops で saturate (= cycle 防御は schedule compiler 側にもある)。
        let compute_depth = |track_id: u32| -> u8 {
            let mut cursor = id_to_parent.get(&track_id).copied().flatten();
            let mut depth: u8 = 0;
            let mut hops = 0u8;
            while let Some(pid) = cursor {
                depth = depth.saturating_add(1);
                hops = hops.saturating_add(1);
                if hops > 32 {
                    break;
                }
                cursor = id_to_parent.get(&pid).copied().flatten();
            }
            depth
        };
        self.song
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (l, r) = self.track_peak_display.get(i).copied().unwrap_or((0.0, 0.0));
                TrackMixEntry {
                    index: i as u32,
                    track_id: t.id,
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
                    is_group: is_group_set.contains(&t.id),
                    is_return: is_return_set.contains(&t.id),
                    depth: compute_depth(t.id),
                    color: crate::view::track_color::effective_track_color(t),
                }
            })
            .collect()
    }

    pub fn selected_track_label(&self) -> String {
        let n_selected = self.selected_track_ids.len();
        if n_selected > 1 {
            return format!("{n_selected} tracks selected");
        }
        if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
            return "Master".into();
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
        // master bus は audio fx のみ。 各 master fx を sidechain entry として
        // 返す (slot_kind=2 / track_id=MASTER_TRACK_ID)。 source 選択肢は通常
        // track 同様 sidechain_source_choices が全 track を列挙する。
        if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
            return self
                .song
                .master_fx_chain
                .iter()
                .enumerate()
                .map(|(i, p)| SidechainEntry {
                    track_id: common::model::MASTER_TRACK_ID,
                    slot_kind: 2,
                    slot_index: i as u32,
                    plugin_name: resolve_plugin_name(&self.plugin_db, &p.plugin_id),
                    current_source: p.sidechain_sources.first().copied().flatten(),
                })
                .collect();
        }
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
        let cref = self.selected_clip_ref()?;
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
            self.audio_editor_anchor_event().unwrap_or(0)
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
            gain_db: event.gain_db,
            pan: event.pan,
            pitch_semitones: event.pitch_semitones,
            fade_in_beats: event.fade_in_beats,
            fade_out_beats: event.fade_out_beats,
            clip_length_beats: clip.length_beats,
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
        let cur = self.audio_editor_anchor_event().unwrap_or(0).min(n - 1);
        let n_i = n as i32;
        let next = (cur as i32).wrapping_add(delta).rem_euclid(n_i);
        Some(next as usize)
    }

    /// Audio Editor の選択 anchor (= Inspector / footer / nav の代表 event
    /// index)。 選択集合の last (= 最後に選択した event)。 空なら None。
    pub fn audio_editor_anchor_event(&self) -> Option<usize> {
        self.audio_editor_selected_events.last().copied()
    }

    /// `selected_clip` が `ClipContent::Image` の clip を指していて、
    /// 中に少なくとも 1 event があれば first event を代表値として
    /// `InspectorImageEventSummary` を返す。
    /// 編集 AppEvent (`SetClipImageX` 等) は全 event に同じ値を broadcast
    /// するので、 multi-event clip でも view は first event を「代表値」
    /// として見せれば編集後に整合が取れる。 数値値 (x/y/w/h/opacity/
    /// fade_in_beats/fade_out_beats) は inspector の edit buffer (text
    /// 文字列) 側に持つので summary には含めない (= dropdown / toggle
    /// のみ snapshot に乗せる)。
    pub fn inspector_image_event_summary(&self) -> Option<InspectorImageEventSummary> {
        let cref = self.selected_clip_ref()?;
        let track = self.song.tracks.get(cref.track as usize)?;
        let clip = track.clips.get(cref.clip as usize)?;
        let common::model::ClipContent::Image(image) =
            self.song.clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        let event = image.events.first()?;
        let has_lane = |field: common::model::ImageBuiltinParam| {
            track.automation_lanes.iter().any(|l| {
                matches!(l.target, common::model::AutomationTarget::ImageBuiltin(p) if p == field)
            })
        };
        Some(InspectorImageEventSummary {
            target: cref,
            muted: event.muted,
            fade_in_curve: event.fade_in_curve,
            fade_out_curve: event.fade_out_curve,
            x_automated: has_lane(common::model::ImageBuiltinParam::X),
            y_automated: has_lane(common::model::ImageBuiltinParam::Y),
            w_automated: has_lane(common::model::ImageBuiltinParam::W),
            h_automated: has_lane(common::model::ImageBuiltinParam::H),
            opacity_automated: has_lane(common::model::ImageBuiltinParam::Opacity),
            rotation_automated: has_lane(common::model::ImageBuiltinParam::Rotation),
            x: event.x,
            y: event.y,
            w: event.w,
            h: event.h,
            opacity: event.opacity,
            rotation_radians: event.rotation_radians,
            fade_in_beats: event.fade_in_beats,
            fade_out_beats: event.fade_out_beats,
            clip_length_beats: clip.length_beats,
        })
    }

    /// PR-D 段階 2: set_clip_audio_event_* 系 helper の broadcast 範囲を
    /// 決める。 audio_editor が `target` clip を開いていて event を
    /// 選択中なら、 当該 event 1 つだけ更新 (= multi-event clip の個別
    /// 編集)。 そうでなければ全 event に broadcast (= Phase 2 PR1-3 の
    /// 既存挙動、 1 clip 1 event 前提なので broadcast = first event 編集)。
    /// 引数 `n_events` は当該 ClipContent::Audio の events 長 (= 呼び出し
    /// 前に immutable get で取得)。
    fn audio_event_target_indices(&self, target: ClipRef, n_events: usize) -> Vec<usize> {
        if self.audio_editor_clip == Some(target)
            && !self.audio_editor_selected_events.is_empty()
        {
            let mut v: Vec<usize> = self
                .audio_editor_selected_events
                .iter()
                .copied()
                .filter(|&i| i < n_events)
                .collect();
            v.sort_unstable();
            v.dedup();
            // 選択はあるが全て範囲外 (stale) なら全 event に broadcast
            // (= 旧 `idx < n_events` else 全件 の挙動を踏襲)。
            if v.is_empty() { (0..n_events).collect() } else { v }
        } else {
            (0..n_events).collect()
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
        let indices = self.audio_event_target_indices(target, n_events);
        if indices.is_empty() {
            return false;
        }
        if let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        {
            for &i in &indices {
                if let Some(event) = audio.events.get_mut(i) {
                    f(event);
                }
            }
            self.sync_song_to_plugin_host();
            true
        } else {
            false
        }
    }

    /// `target` clip が `ClipContent::Image` の場合、 全 ImageEvent に
    /// `f` を適用する (= image clip は audio_editor のような per-event
    /// 選択 UI を持たないので broadcast 固定)。 戻り値は「実際に何らか
    /// の event を更新したか」 (= caller が edit buffer resync を呼ぶか
    /// の判断に使う)。
    fn mutate_image_events_in_clip<F>(&mut self, target: ClipRef, mut f: F) -> bool
    where
        F: FnMut(&mut common::model::ImageEvent),
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
        if let Some(common::model::ClipContent::Image(image)) =
            self.song.clip_contents.get_mut(&content_id)
        {
            if image.events.is_empty() {
                return false;
            }
            for event in &mut image.events {
                f(event);
            }
            true
        } else {
            false
        }
    }

    pub fn inspector_chain(&self) -> Vec<ChainEntry> {
        // master bus は audio fx のみ。 master_fx_chain を FX section として返す。
        if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
            return self
                .song
                .master_fx_chain
                .iter()
                .enumerate()
                .map(|(i, p)| ChainEntry {
                    slot_kind: 2,
                    slot_index: i as u32,
                    section_label: "FX".into(),
                    plugin_name: resolve_plugin_name(&self.plugin_db, &p.plugin_id),
                })
                .collect();
        }
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

    /// v18 (`docs/plan_track_clip_color.md`): color_picker を開く。target と
    /// anchor (popup 基準位置 = 開いた場所の rect) をセットし、session_dirty を
    /// false に戻す (= 次の色変更が session 先頭の 1 snapshot を取る)。
    /// 右クリック「色...」/ inspector スウォッチから呼ぶ。
    pub fn open_color_picker(
        &mut self,
        target: ColorPickerTarget,
        anchor: daw_ui_renderer::Rect,
    ) {
        self.color_picker_target = Some(target);
        self.color_picker_anchor = Some(anchor);
        self.color_picker_session_dirty = false;
    }

    /// color_picker の色変更 (`SetTrackColor`/`SetClipColor`) 用の undo snapshot。
    /// picker session 中は session 先頭 (まだ dirty でない) で 1 回だけ、
    /// picker が開いていない discrete edit (= 「トラック色に戻す」 reset 等) は
    /// 毎回 snapshot する。
    fn snapshot_for_color_edit(&mut self) {
        if self.color_picker_target.is_some() {
            if !self.color_picker_session_dirty {
                self.push_undo_snapshot();
                self.color_picker_session_dirty = true;
            }
        } else {
            self.push_undo_snapshot();
        }
    }

    // -------- Undo/Redo ----------------------------------------------------

    fn push_undo_snapshot(&mut self) {
        if self.undo_stack.len() >= UNDO_LIMIT {
            self.undo_stack.pop_front();
        }
        self.undo_stack.push_back(self.song.clone());
        self.redo_stack.clear();
    }

    /// `is_dirty` を「保存時点の内容との差分」から再計算する (SSoT)。
    /// 編集 / Undo / Redo は sticky に `is_dirty = true` を立てるだけで、
    /// 実際に保存状態へ戻ったか否かはここで内容比較して確定する。 タイトル
    /// 描画 (runner) が毎フレーム is_dirty が立っているときだけ本関数を呼ぶ
    /// ので、 clean な間は比較ゼロ・dirty な間も 1 フレーム 1 回に収まる。
    /// snapshot 方式の Undo なので index 比較より内容比較の方が頑健
    /// (相殺編集 / redo で保存点を通り越す / stack トリミング を正しく判定)。
    pub(crate) fn recompute_dirty(&mut self) {
        self.is_dirty = self.song != self.saved_song;
    }

    /// New / Open / Restore 時に呼ぶ。 現在の song を保存ベースラインに確定し、
    /// 別プロジェクトの Undo/Redo 履歴を破棄して clean 状態にする。
    /// (save は履歴を残したいので別扱い: `saved_song` 更新のみ。)
    fn reset_saved_baseline(&mut self) {
        self.saved_song = self.song.clone();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.is_dirty = false;
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
        // stable ClipKey 保持なので並べ替え / undo を跨いでも追従する。 clip が
        // 削除されて解決できない key のみ落とす。
        if let Some(k) = self.selected_clip
            && self.clip_at(k).is_none()
        {
            self.selected_clip = None;
        }
        let mut keys = std::mem::take(&mut self.selected_clips);
        keys.retain(|k| self.clip_at(*k).is_some());
        self.selected_clips = keys;
        // note の index は undo で容易にずれるため、安全側で clear する。
        self.selected_notes.clear();
        // audio event の選択 index も同様に undo でずれるため clear。
        self.audio_editor_selected_events.clear();
        self.track_rename_id = None;
        self.track_rename_text.clear();
        self.clip_rename = None;
        self.clip_rename_text.clear();
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
        //
        // Risk E (plan_undo_reconcile_polish.md): 多段 Undo で reconcile
        // が毎 step 走る cost を測定するための timing log。 plan B 完了で
        // diff は slot 単位の最小 set に絞られているので、 plugin chain
        // が変わらない Undo は HashMap iter のみで終わる。 変わる場合の
        // RemoveSlot/SetSlot IPC コストを観測したい場合は
        // `daw_gui::app::undo_perf=trace` で見る。
        let reconcile_started = std::time::Instant::now();
        self.reconcile_plugins_with_song();
        let reconcile_elapsed = reconcile_started.elapsed();
        tracing::trace!(
            target: "daw_gui::app::undo_perf",
            elapsed_us = reconcile_elapsed.as_micros() as u64,
            "reconcile_plugins_with_song after Undo/Redo"
        );
        self.resync_song_edit_texts();    }

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
                | AppEvent::AddInstrumentTrack
                | AppEvent::GroupSelectedTracks { .. }
                | AppEvent::SetTrackParent { .. }
                | AppEvent::SetLipsyncTarget { .. }
                | AppEvent::SetMouthMapSlot { .. }
                | AppEvent::RemoveLastTrack
                | AppEvent::CommitRenameTrack
                | AppEvent::CommitRenameClip
                | AppEvent::CreateClip { .. }
                | AppEvent::ResizeClip { .. }
                | AppEvent::DeleteSelectedClip
                | AppEvent::DuplicateClipsShared(_)
                | AppEvent::DuplicateClipsUnique(_)
                | AppEvent::CloneClipsLinked(_)
                | AppEvent::CloneClipsIndependent(_)
                | AppEvent::MakeClipUnique(_)
                | AppEvent::SplitClipAtPlayhead { .. }
                | AppEvent::GlueSelectedClips
                | AppEvent::SetClipReversed { .. }
                | AppEvent::SetClipMuted { .. }
                // ResetTrackClipColors は discrete な一括編集なので undoable。
                // (SetTrackColor / SetClipColor は picker live drag のため非 undoable)
                | AppEvent::ResetTrackClipColors { .. }
                // 注: SetTrackColor / SetClipColor は is_undoable に **入れない**。
                // color_picker の live drag で毎フレーム発火するため、 ここで
                // 毎回 snapshot すると undo 履歴が溢れる。 代わりに各 arm が
                // 「picker session 先頭で 1 回 / discrete edit は毎回」 だけ
                // snapshot する (image PiP drag と同思想)。
                | AppEvent::SetClipStretchMode { .. }
                // FIXME #15: inspector の数値 field は scrubable_number 化され、
                // SetClipGainDb / Pan / Pitch / FadeIn/OutBeats は drag 中 per-
                // frame 発火するため **非 undoable**。 drag / text 編集 stroke は
                // `BeginInspectorScrub` (1 snapshot) で 1 undo step に bracket する。
                | AppEvent::SetClipFadeInCurve { .. }
                | AppEvent::SetClipFadeOutCurve { .. }
                | AppEvent::BeginImagePiPDrag
                | AppEvent::BeginGroupTransformDrag
                | AppEvent::BeginInspectorScrub
                | AppEvent::BeginTextPiPDrag
                | AppEvent::CommitClipTextContentEdit
                | AppEvent::CommitClipTextFontFamilyEdit
                | AppEvent::SetClipTextMuted { .. }
                | AppEvent::SetClipTextAlign { .. }
                | AppEvent::SetClipTextFadeInCurve { .. }
                | AppEvent::SetClipTextFadeOutCurve { .. }
                // EndImagePiPDrag は非 undoable (begin 側に snapshot あり)
                // 注: SetClipImage{X,Y,W,H,Opacity} は preview drag で
                // 毎フレーム発火するので非 undoable。 drag begin の
                // BeginImagePiPDrag 1 個で 1 snapshot を取り、 drag 中
                // の連続更新は同 step 内に集約する。 inspector の数値
                // 入力 commit は CommitClipImage*Edit が undoable なので
                // 1 commit = 1 step を維持。
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
                | AppEvent::DeleteAudioEditorSelection
                | AppEvent::ImportAudio { .. }
                | AppEvent::ImportVideo { .. }
                | AppEvent::ImportImage { .. }
                | AppEvent::AddTextClipAt { .. }
                | AppEvent::AddNote { .. }
                | AppEvent::ResizeNote { .. }
                | AppEvent::ResizeNotes(_)
                | AppEvent::SetNotePositions(_)
                | AppEvent::DeleteSelectedNotes
                | AppEvent::DuplicateSelectedNotes
                | AppEvent::CopyNotes(_)
                | AppEvent::SetNoteLyrics { .. }
                | AppEvent::SetNoteVelocities(_)
                | AppEvent::SetTrackSpeaker { .. }
                | AppEvent::QuantizeSelectedNotes(_)
                | AppEvent::SelectPluginFromDb { .. }
                | AppEvent::CommitBpmEdit
                | AppEvent::CommitTimeSigNumEdit
                | AppEvent::SetSongTimeSigDenominator(_)
                // gui_01 #028 (Phase 63n-1/-2/-3): automation lane / point / clip 編集。
                // SetLaneDefault / SetLaneEnabled / SetLaneVisible 等の knob / toggle 系
                // は drag 中の連続発火 (live preview) を考慮すると個別に Undo step 化
                // するのは UX 過多なので、 構造変化系 (lane add / delete / clip 追加削除
                // / point 追加削除 / curve type 変更) のみ undoable に登録する。
                // SetLaneDefault と SetAutomationCurveType は { prev, next } を持つので
                // 後で snapshotless Undo に置換できるが、 当面は Song snapshot 経由。
                | AppEvent::AddAutomationFromLastTouched
                | AppEvent::AddImageAutomationLane { .. }
                | AppEvent::RemoveImageAutomationLane { .. }
                | AppEvent::AddTextAutomationLane { .. }
                | AppEvent::RemoveTextAutomationLane { .. }
                | AppEvent::AddGroupAutomationLane { .. }
                | AppEvent::RemoveGroupAutomationLane { .. }
                | AppEvent::CreateAutomationClip { .. }
                | AppEvent::DeleteLane { .. }
                | AppEvent::AddAutomationPoint { .. }
                | AppEvent::MoveAutomationPoints { .. }
                | AppEvent::DeleteAutomationPoints { .. }
                | AppEvent::SetAutomationCurveType { .. }
                | AppEvent::MoveAutomationClips { .. }
                | AppEvent::CloneAutomationClipsLinked { .. }
                | AppEvent::CloneAutomationClipsIndependent { .. }
                | AppEvent::ResizeAutomationClips { .. }
                | AppEvent::DeleteAutomationClips { .. }
                | AppEvent::MakeAutomationClipUnique(_)
                | AppEvent::DuplicateAutomationClipsShared(_)
                | AppEvent::DuplicateAutomationClipsUnique(_)
                // Phase 3: point quantize は構造変化系として Undo step
                // 化。 SelectAutomationPoints は session-only なので除外。
                | AppEvent::QuantizeSelectedAutomationPoints(_)
                // gui_01 #033 Phase 63n-9: tension/bend handle drag は
                // release frame の 1 件のみ発火 (widget 内仕様、 連続発火
                // による Undo 履歴爆発はない)。 値 1 件で point の curve
                // を上書きする structural change なので Undo step 化。
                | AppEvent::SetAutomationCurveBezierTension { .. }
                | AppEvent::SetAutomationCurveExponentialBend { .. }
                // Phase 7 B5 (`docs/plan_scale.html`): scale event 編集と
                // pitch quantize は構造変化系として Undo step 化。 Snap on
                // Draw / Snap Live Input toggle は session-only で除外。
                | AppEvent::SetScaleAtPlayhead { .. }
                | AppEvent::ClearScaleChanges
                | AppEvent::QuantizePitchesToScale(_)
        )
    }

    /// 選択中ノートを JSON シリアライズ。OS clipboard 経由で root.rs から
    /// `Ui::set_clipboard_text` に渡される。何も copy できない (選択無し /
    /// クリップ未選択 / シリアライズ失敗) 場合は `None`。
    /// 戻り値は `(json, note_count)` 。`note_count` は呼び出し側で status_message
    /// 表示等に使う (ここで status_message を書かないのは `&self` で済ませるため)。
    pub fn copy_selected_notes_as_json(&self) -> Option<(String, usize)> {
        let r = self.selected_clip_ref()?;
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
        // 外部 (= 他アプリ / 改竄) clipboard 由来の値域を信用しない。
        // pitch を 0..=127、 velocity を 1..=127 に clamp し、
        // start_beat / duration_beats が非有限 or 負 (duration は 0 含む)
        // の note は破棄する (= NaN/Inf を model に入れない)。
        let mut clipboard: Vec<Note> = clipboard
            .into_iter()
            .filter_map(|mut n| {
                if !n.start_beat.is_finite()
                    || n.start_beat < 0.0
                    || !n.duration_beats.is_finite()
                    || n.duration_beats <= 0.0
                {
                    return None;
                }
                n.pitch = n.pitch.min(127);
                n.velocity = n.velocity.clamp(1, 127);
                Some(n)
            })
            .collect();
        if clipboard.is_empty() {
            return;
        }
        let Some(r) = self.selected_clip_ref() else {
            self.status_message = "貼り付け先のクリップが選択されていません".to_string();
            return;
        };
        // 貼り付け先 clip が実在しなければ何も挿入しない (= notes_in_clip_mut
        // が None を返す経路)。 ここで弾いておくことで spurious な undo
        // snapshot を積まない。
        if self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
            .is_none()
        {
            return;
        }
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
        // 実挿入の直前に undo snapshot を積む (= Ctrl+Z で paste を取り消せる)。
        self.push_undo_snapshot();
        let count = clipboard.len();
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        let mut new_indices = Vec::with_capacity(clipboard.len());
        for src in &mut clipboard {
            src.start_beat += anchor;
            new_indices.push(notes.len() as u32);
            notes.push(src.clone());
        }
        self.selected_notes = new_indices;
        self.sync_song_to_plugin_host();
        self.status_message = format!("貼り付け: {count} ノート");
    }

    fn set_note_velocity(&mut self, note_idx: u32, velocity: u8) {
        let Some(r) = self.selected_clip_ref() else {
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
        let Some(r) = self.selected_clip_ref() else {
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
        let Some(r) = self.selected_clip_ref() else {
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
        self.sync_song_to_plugin_host();    }

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

/// `reconcile_plugins_with_song` の Phase B が計算する action。
/// IPC dispatch から独立した純粋データ型にすることで unit test しやすく
/// する (4dc982c で導入した slot-level diff の regression 防止)。
#[derive(Debug, Clone, PartialEq)]
pub enum SlotReconcileAction {
    /// host にあるが Song に無い slot を host から消す
    /// (= `MainToChild::RemoveSlotPlugin` 相当)。
    RemoveSlot { track_id: u32, slot: PluginSlot },
    /// Song にあるが host に無い、 もしくは plugin_id_str が違う slot を
    /// (再) load する (= `MainToChild::SetSlotPlugin` 相当)。 caller が
    /// `plugin_db` から format / path を解決して IPC を組み立てる。
    LoadSlot {
        track_id: u32,
        slot: PluginSlot,
        plugin_id_str: String,
        initial_state: Option<Vec<u8>>,
    },
}

/// Phase B 純粋関数化。 song と現在の loaded_slots cache を見て、 host
/// と Song を揃えるための action 列を返す。 副作用なし (IPC は呼ばない、
/// AppData にも触らない)。
pub fn compute_slot_reconcile_actions(
    song: &common::model::Song,
    loaded_slots: &HashMap<(u32, PluginSlot), LoadedSlotInfo>,
) -> Vec<SlotReconcileAction> {
    let mut actions = Vec::new();
    for track in &song.tracks {
        let track_id = track.id;

        // Song 側 (slot, &PluginInstance) 列を midi_fx → instrument →
        // fx の順で組み立て。 `reconcile_plugins_with_song` の従来動作と
        // 同じ順序を維持する (= load 順が音にも影響するため変えない)。
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

        // (1) host にあるが Song に無い slot → RemoveSlot
        // 順序を安定にするため slot を sort してから push する (= test の
        // assertion が決定的になる)。 production の挙動は HashMap iter
        // 順依存だが、 RemoveSlot 同士は独立操作なので順序は無関係。
        let mut host_extra_slots: Vec<PluginSlot> = loaded_slots
            .iter()
            .filter(|((tid, _), _)| *tid == track_id)
            .map(|((_, s), _)| *s)
            .filter(|s| !song_slot_set.contains(s))
            .collect();
        host_extra_slots.sort_by_key(|s| slot_sort_key(*s));
        for slot in host_extra_slots {
            actions.push(SlotReconcileAction::RemoveSlot { track_id, slot });
        }

        // (2) Song にあるが host に無い、 もしくは plugin_id_str が違う slot → LoadSlot
        for (slot, inst) in song_slots {
            let need_load = match loaded_slots.get(&(track_id, slot)) {
                None => true,
                Some(info) => info.plugin_id_str != inst.plugin_id,
            };
            if !need_load {
                continue;
            }
            actions.push(SlotReconcileAction::LoadSlot {
                track_id,
                slot,
                plugin_id_str: inst.plugin_id.clone(),
                initial_state: inst.state.clone(),
            });
        }
    }

    // master bus fx chain。 通常 track と同じ reconcile を `track_id ==
    // MASTER_TRACK_ID` / `PluginSlot::Fx(i)` keying で行う。 master は audio fx
    // のみ持つので midi_fx / instrument 分岐は無い。
    {
        let track_id = common::model::MASTER_TRACK_ID;
        let mut song_slots: Vec<(PluginSlot, &common::model::PluginInstance)> = Vec::new();
        for (i, p) in song.master_fx_chain.iter().enumerate() {
            song_slots.push((PluginSlot::Fx(i as u32), p));
        }
        let song_slot_set: std::collections::HashSet<PluginSlot> =
            song_slots.iter().map(|(s, _)| *s).collect();

        let mut host_extra_slots: Vec<PluginSlot> = loaded_slots
            .iter()
            .filter(|((tid, _), _)| *tid == track_id)
            .map(|((_, s), _)| *s)
            .filter(|s| !song_slot_set.contains(s))
            .collect();
        host_extra_slots.sort_by_key(|s| slot_sort_key(*s));
        for slot in host_extra_slots {
            actions.push(SlotReconcileAction::RemoveSlot { track_id, slot });
        }

        for (slot, inst) in song_slots {
            let need_load = match loaded_slots.get(&(track_id, slot)) {
                None => true,
                Some(info) => info.plugin_id_str != inst.plugin_id,
            };
            if !need_load {
                continue;
            }
            actions.push(SlotReconcileAction::LoadSlot {
                track_id,
                slot,
                plugin_id_str: inst.plugin_id.clone(),
                initial_state: inst.state.clone(),
            });
        }
    }
    actions
}

/// `PluginSlot` を安定に sort するためのキー。 RemoveSlot 列の順序を
/// 決定的にする (test 用)。 production 動作には影響しない (= RemoveSlot
/// 同士は独立)。
fn slot_sort_key(slot: PluginSlot) -> (u8, u32) {
    match slot {
        PluginSlot::MidiFx(i) => (0, i),
        PluginSlot::Instrument => (1, 0),
        PluginSlot::Fx(i) => (2, i),
    }
}

/// `RequestAllStates` の発行理由。 plugin_host から `AllPluginStates`
/// が返ってくるまで [`AppData::pending_state_queue`] に保持し、 応答時に
/// 対応する完了処理 (save または deferred edit) を実行する。
///
/// 連続した編集 (例: 2 連続 delete_track) は queue に積まれ、 各々が
/// 自前の `RequestAllStates` 応答を待ってから順次実行される。 これで
/// 「先行 edit の応答待ち中に来た 2 番目の edit が state 同期なしで
/// 走り、 Undo で knob 値が復元されない」 race を回避する。
#[derive(Debug, Clone)]
pub enum PendingStateRequest {
    /// project save。 ファイル書き出し完了で消費される。 `snapshot` は **この Save の
    /// `RequestAllStates` を発行する瞬間** の song を凍結したもの。 enqueue 時点では
    /// `None` で積み、 `dispatch_front_state_request` が state 収集を始めるその瞬間に
    /// 充填する。 こうすると snapshot の plugin layout と、 host が返す plugin state の
    /// layout が同時刻サンプリングになり (FIFO IPC)、 待機中の slot 削除 / 並べ替えが
    /// 保存ファイルへ誤適用される窓が出ない。 充填後に走った編集はこの snapshot に
    /// 入らず live song に留まり、 次の save に回る
    /// (grill-me 2026-06-10, `docs/plan_progress_streaming.md`)。
    Save {
        path: PathBuf,
        snapshot: Option<Box<Song>>,
    },
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

/// 口パク (lip-sync) 背景ジョブの 1 vocal clip 分の結果。`query_phonemes` の
/// 出力と、生成先 clip の配置情報 (start / length / earliest note) をまとめて
/// main thread へ渡す (`AppEvent::LipsyncGenerated`)。docs/plan_pakupaku.md §7。
#[derive(Debug, Clone, PartialEq)]
pub struct LipsyncClipResult {
    /// 生成先 clip の start_beat (= 元 vocal clip と揃える)。
    pub clip_start_beat: f64,
    /// 生成先 clip の length_beats (= 元 vocal clip と揃える)。
    pub clip_len_beats: f64,
    /// clip 内 earliest note の clip-local start_beat (REST offset 配置の基準)。
    pub first_note_local_beat: f64,
    /// VOICEVOX phoneme 列 (先頭/末尾 pau 込み、frame 0 起点)。
    pub phonemes: Vec<common::voicevox::Phoneme>,
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
    /// 未保存変更ありの close 確認モーダルで「保存して終了」。 save を発行し、
    /// 完了後に終了する (plugin 有り project は非同期保存を待つ)。
    CloseConfirmSave,
    /// close 確認モーダルで「保存せず終了」。 保存せず即終了。
    CloseConfirmDiscard,
    /// close 確認モーダルで「キャンセル」 (Esc / 外クリック / ✕ 含む)。
    /// 終了を取りやめてアプリに戻る。
    CloseConfirmCancel,
    /// FIXME #27: 別の daw_gui を起動しようとした (single-instance)。 2 つ目の
    /// プロセスが既存インスタンスにこれを送って前面化を要求する。 window 操作
    /// なので runner の `user_event` が直接処理し、 `handle_event` には届かない。
    RaiseMainWindow,
    Play,
    Stop,
    PlayToggle,
    ToggleLoop,
    /// `R` キー: 選択中 clip(s) の bounding range を loop 範囲に設定して
    /// loop ON + 再生開始。 既に loop ON かつ範囲が選択 clip と一致するなら
    /// loop を OFF にする (再生は維持)。 選択 clip が無ければ no-op。
    LoopSelectedClipToggle,
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
    /// Phase 5 Step 5.1 follow-up (gui_01 #035): transport の BPM
    /// scrubable_number drag 中に流れる連続 BPM 変化。 widget 内で 1.0..=400.0
    /// に clamp 済前提だが defensive で再 clamp。 `bpm_edit_text` も同期して
    /// text input mode の表示を追随させる。 Undo 対象外 (= 連続発火、
    /// release edge の ParamGestureEnd で 1 step Undo 化を別途検討)。
    /// 軽量 IPC `MainToChild::SetSongBpm` で audio engine 即時反映。
    SetSongBpmFromScrub(f32),
    /// Phase 5 Step 5.1 follow-up: TimeSig numerator scrub。 1..=32 clamp、
    /// `time_sig_num_edit_text` 同期、 軽量 IPC `MainToChild::SetSongTimeSigNumerator`。
    SetSongTimeSigNumFromScrub(u8),
    Undo,
    Redo,
    PushUndoSnapshot,
    QuantizeSelectedNotes(u8),
    /// 鍵盤レーン click のピッチプレビュー (gui_01 #055,
    /// `docs/plan_pianoroll_keyboard_preview.md`)。 piano_roll_view が毎フレーム
    /// `resp.keyboard_active_pitch` を `preview_note` の pitch と比較し、 変化した
    /// ときだけ発火する。 `track_idx` は描画中 clip の track (Vec index)、 `pitch`
    /// は今フレームの押下 pitch (`None` = release / 鍵盤外)。 handler が前回
    /// `preview_note` と差分して note-on/off IPC を送る。 Undo 対象外。
    PreviewPitchChanged { track_idx: u32, pitch: Option<u8> },
    SetNoteVelocity { note: u32, velocity: u8 },
    /// gui_01 #018 (M14 Phase 64): velocity lane drag で 1 batch 更新。
    /// `selected_clip` の note を `(id, velocity)` で一括書き換え。 1 drag =
    /// 1 Undo step。
    SetNoteVelocities(Vec<(u32, u8)>),
    AddInstrumentTrack,
    /// Group the selected tracks under a fresh group track. Mirrors
    /// Ableton Live's Cmd/Ctrl+G: the *selection-root* tracks become
    /// children of the new group (their `parent_group_id` is set), and
    /// the new group is inserted just *before* the highest-positioned
    /// selected track (= 一番上の選択 track の直前 / 子の上にヘッダー)。
    /// `track_ids` must be non-empty — Live forbids empty groups and so
    /// do we. FIXME #13 (plan_group_nesting): only the *selection roots*
    /// (tracks whose `parent_group_id` is not itself in the selection)
    /// are re-parented, so a selected group keeps its own children and
    /// nesting is preserved (depth unbounded) instead of being flattened.
    GroupSelectedTracks {
        track_ids: Vec<u32>,
    },
    /// gui_01 #028 (M14 Phase 63n-1): track 行の disclosure ▶/▼ click。
    /// `expanded_automation_tracks` の `track_id` を反転し、 widget が
    /// 次フレームで lane 群を展開 / 折り畳む。 session-only な UI 状態
    /// なので Undo / save 対象外。
    ToggleTrackAutomationCollapsed {
        track_id: u32,
    },
    // ----------------------------------------------------------------
    // gui_01 #028 (M14 Phase 63n-2) — automation lane / point 編集
    // ----------------------------------------------------------------
    /// Lane 全体の bypass。`★`/`☆` icon click。
    SetLaneEnabled {
        track_id: u32,
        lane_id: u32,
        enabled: bool,
    },
    /// Lane の表示 / 非表示。`👁` icon click。
    SetLaneVisible {
        track_id: u32,
        lane_id: u32,
        visible: bool,
    },
    /// Lane header の default value slider drag。`prev` / `next` は
    /// 共に **normalized 0..1** (widget の slider 帯と同単位)。handler
    /// 側で `lane.target` を引いて plain 単位に逆変換してから格納する。
    /// drag 中は per-frame で発火 (live preview)、release で 1 度確定。
    SetLaneDefault {
        track_id: u32,
        lane_id: u32,
        prev_norm: f32,
        next_norm: f32,
    },
    /// Lane の `✕` icon click → `Track.automation_lanes` から該当 lane
    /// を除去。lane 内 clip の `content_id` が他 clip と共有されてい
    /// なければ `clip_contents` の該当 entry も `gc_clip_contents`
    /// 次サイクルで GC される (このイベント自体は触らない)。
    DeleteLane {
        track_id: u32,
        lane_id: u32,
    },
    /// gui_01 #030 (M14 Phase 63n-5): lane 高さ drag (Alt+drag or
    /// 下端 splitter)。`prev` / `next` は px、widget 側で
    /// `[automation_lane_min_height_px, automation_lane_max_height_px]`
    /// に clamp 済。drag 中は per-frame 発火 (live preview)、release で
    /// 1 件確定。`SetLaneDefault` と同パターン。
    SetLaneHeight {
        track_id: u32,
        lane_id: u32,
        prev_px: u16,
        next_px: u16,
    },
    /// gui_01 #031 (M14 Phase 63n-6): MIDI track row 高さの個別 override。
    /// Alt+drag or 下端 splitter drag で発火。 既存 `Alt+wheel`
    /// (`SetTrackRowH(f32)` = global default) と独立、 個別 track は
    /// override map に保存。 drag 中は per-frame 発火、 release で確定。
    SetSingleTrackRowH {
        track_id: u32,
        prev_px: u16,
        next_px: u16,
    },
    /// Lane body 内 dblclick で 1 point 追加。`time_beat` は clip-local、
    /// `value_norm` は normalized 0..1 (widget が clip rect 内 cursor
    /// 座標から計算済)。handler は norm → plain 変換 + `time_beat` 昇順
    /// 維持を担当。
    AddAutomationPoint {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        time_beat: f64,
        value_norm: f32,
    },
    /// 1 つ以上の point の position 更新 (release 時に 1 度発火)。
    /// `MoveAutomationPointEntry` の `value_norm` は normalized、handler
    /// 側で plain 化。`point_idx` は **同 frame 内のみ valid** なので、
    /// drag session 内では gui_01 widget が prev_index を保持する前提
    /// (本 event 受信時はそのフレームの index で OK)。
    MoveAutomationPoints {
        deltas: Vec<MoveAutomationPointEntry>,
    },
    /// Alt+click on point → 即時削除 (1 件)、もしくは将来の rect select
    /// → 一括削除を batch で受ける。`Vec<AutomationPointKey>` を
    /// daw_01 内部型 (`(track_id, lane_id, clip_id, point_idx)` 4-tuple
    /// 相当) で運ぶ。
    DeleteAutomationPoints {
        points: Vec<AutomationPointKeyRef>,
    },
    /// 右クリック popup → curve type 選択 → 1 point の `curve` 更新。
    /// `prev` / `next` は Undo 構築用に両方持たせる (gui_01 §11.4 と
    /// 同 idiom、`SetTrackVolume` 等と同じ pattern)。
    SetAutomationCurveType {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        prev: common::model::AutomationCurve,
        next: common::model::AutomationCurve,
    },
    /// gui_01 #033 Phase 63n-9: Bezier curve 中央 handle drag (lane 高さ
    /// 連動 sensitivity、 Alt × 0.2 微調整) の release で 1 件発火。
    /// 当該 point の `curve` を `AutomationCurve::Bezier { tension: next }`
    /// で上書きする。 widget 側で `-1.0..=1.0` clamp 済。 type が Bezier
    /// 以外だった場合 (= race) は no-op (handler 内で current curve を
    /// 確認、 異なれば skip)。
    SetAutomationCurveBezierTension {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        prev: f32,
        next: f32,
    },
    /// gui_01 #033 Phase 63n-9: Exponential curve 中央 handle drag の
    /// release で 1 件発火。 当該 point の `curve` を `Exponential { bend:
    /// next }` で上書き。 値域 / race 扱いは `SetAutomationCurveBezierTension`
    /// と同。
    SetAutomationCurveExponentialBend {
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        prev: f32,
        next: f32,
    },
    // ----------------------------------------------------------------
    // gui_01 #028 (M14 Phase 63n-3) — automation clip drag / select
    // ----------------------------------------------------------------
    /// 修飾なし drag release → source lane から clip を remove + `to_lane`
    /// に start_beat 昇順 insert。lane 跨ぎ accept (target 不一致も OK)。
    MoveAutomationClips {
        deltas: Vec<MoveAutomationClipEntry>,
    },
    /// Ctrl+drag release → source 残置 + 同一 `ContentId` を持つ新 clip
    /// を `to_lane` に追加 (linked、curve を共有)。
    CloneAutomationClipsLinked {
        deltas: Vec<MoveAutomationClipEntry>,
    },
    /// Ctrl+Shift+drag release → source 残置 + content を deep clone (新
    /// `ContentId` 採番) した独立 clip を `to_lane` に追加。
    CloneAutomationClipsIndependent {
        deltas: Vec<MoveAutomationClipEntry>,
    },
    /// 左右 edge drag release → 各 clip の start / len 上書き。
    ResizeAutomationClips {
        deltas: Vec<ResizeAutomationClipEntry>,
    },
    /// caller-driven (右クリック menu / shortcut から発火、 widget は
    /// 提供せず) → 該当 lane から `clip_id` で除去。content の GC は次の
    /// save / `gc_clip_contents` で行う。
    DeleteAutomationClips {
        keys: Vec<common::model::AutomationClipKey>,
    },
    /// 短 click on automation clip → `selected_automation_clips` を
    /// `next` で上書き。MIDI 用 `selected_clips` は触らない (= 共存)。
    SelectAutomationClips {
        prev: Vec<common::model::AutomationClipKey>,
        next: Vec<common::model::AutomationClipKey>,
    },
    /// Phase 3 (gui_01 #033 widget 側 lasso 完了後に発火される想定):
    /// `selected_automation_points` を `next` で上書き。 `prev` は Undo 用
    /// (selection 自体は session state なので Undo 非対象だが、 `SelectClips`
    /// と同じ idiom で signature を揃える)。
    SelectAutomationPoints {
        prev: Vec<AutomationPointKeyRef>,
        next: Vec<AutomationPointKeyRef>,
    },
    /// Phase 3: `selected_automation_points` を grid (`1/div` beat) に snap。
    /// piano roll の `QuantizeSelectedNotes` と同 idiom。 同 clip 内の
    /// point は sort 維持のためまとめて sort し直し、 selection も新 idx に
    /// 再採番する。 `div = 1` で 1 beat 単位、 `4` で 1/4 beat 単位。
    QuantizeSelectedAutomationPoints(u8),
    /// 右クリック menu「Make Unique」 → 共有中 (`refcount >= 2`) の
    /// automation clip の content を deep clone (新 `ContentId`)、独立化。
    /// 既に独立 clip の場合は status_message で通知。MIDI clip 用
    /// `MakeClipUnique(ClipRef)` と同 idiom の lane 版。
    MakeAutomationClipUnique(common::model::AutomationClipKey),
    /// FIXME #21: D shortcut: 選択中の automation clip 群をまとめて共有コピー。
    /// MIDI 用 `DuplicateClipsShared` の automation lane 版。 選択ブロック span
    /// だけ後ろにずらして複製し、 新 key 群を `selected_automation_clips` に
    /// 上書きする (D 連打で後方連鎖)。
    DuplicateAutomationClipsShared(Vec<common::model::AutomationClipKey>),
    /// FIXME #21: Alt+D shortcut: 選択中の automation clip 群をまとめて独立コピー
    /// (content を deep clone + 新 ContentId)。 配置・選択は shared 版と同じ。
    DuplicateAutomationClipsUnique(Vec<common::model::AutomationClipKey>),
    /// gui_01 #028 §7.3: parameter touch 通知。inspector の knob drag /
    /// plugin GUI の knob 操作 (Phase 2+ で IPC 経由) で発火し、
    /// `last_touched_param` を更新。`A` キー shortcut の source になる。
    /// session-only / Undo 不要。
    TouchParam {
        track_id: u32,
        target: common::model::AutomationTarget,
        display_name: String,
    },
    /// `A` キー shortcut。`last_touched_param` の lane を該当 track に
    /// 追加。既に同 target の lane があれば visible = true で復活、なけ
    /// れば新規作成 (default = 現在の plain 値)。`expanded_automation_tracks`
    /// にも所有 track を insert して即時展開。
    AddAutomationFromLastTouched,

    /// Inspector の image event section「📈」 ボタンから発火。 選択中
    /// image clip の track に `ImageBuiltin(field)` lane を追加 (既存
    /// あれば visible / enabled 復活)。 default_value は first ImageEvent
    /// の現値 (`docs/plan_image_automation.md` §4.1)。 undoable。
    AddImageAutomationLane { field: common::model::ImageBuiltinParam },

    /// Inspector の automate toggle が ON 状態のとき、 もう一度押すと
    /// 該当 `ImageBuiltin(field)` lane を track から削除する。 削除後は
    /// ImageEvent.field がふたたび effective (= override 解除)。 lane
    /// が無ければ no-op。 undoable。
    RemoveImageAutomationLane { field: common::model::ImageBuiltinParam },

    /// docs/plan_text_overlay.md §4 P8: Inspector の text section「A」
    /// ボタンから発火。 選択中 text clip の track に `TextBuiltin(field)`
    /// lane を追加 (既存あれば visible / enabled 復活)。 default_value は
    /// `lane_default_for_target` 経由で first TextEvent の現値 (event が
    /// 無ければ TextEvent::default 相当の常識値)。 undoable。
    AddTextAutomationLane { field: common::model::TextBuiltinParam },

    /// Inspector の automate toggle が ON 状態で再押し → 該当
    /// `TextBuiltin(field)` lane を track から削除 (= override 解除、
    /// TextEvent.field が effective に戻る)。 lane が無ければ no-op。
    /// undoable。
    RemoveTextAutomationLane { field: common::model::TextBuiltinParam },

    /// v19 (`docs/plan_tachie_group_transform.md` §5.5): 選択中 visual group
    /// track に `GroupTransform(param)` lane を追加（既存あれば visible /
    /// enabled 復活）。default_value は現 group_transform の field 値。undoable。
    AddGroupAutomationLane { param: common::model::GroupTransformParam },
    /// automate toggle 再押しで `GroupTransform(param)` lane を削除。undoable。
    RemoveGroupAutomationLane { param: common::model::GroupTransformParam },
    /// preview 上の group box drag 開始（undo snapshot を 1 個取る。group lane
    /// recording は未対応）。undoable。
    BeginGroupTransformDrag,
    /// preview drag 中の live 設定（非 undoable）。`set_group_transform_field`
    /// + edit buffer resync で inspector も同期。
    SetGroupTransformField {
        track_id: u32,
        param: common::model::GroupTransformParam,
        value: f32,
    },
    /// preview drag 終了（非 undoable、begin に snapshot あり）。
    EndGroupTransformDrag,

    /// FIXME #15 (`docs/plan_inspector_scrub.md`): audio / image / text
    /// inspector の scrubable_number で drag / text 編集を開始した瞬間に
    /// 発火する marker。 handler 本体は no-op、 `is_undoable` に含まれる
    /// ので handle_event 冒頭の auto push_undo_snapshot だけが効き、 drag
    /// 中の `SetClip*` 連発 (= 非 undoable) を 1 undo step に集約する
    /// (= group transform の `BeginGroupTransformDrag` と同 idiom)。
    BeginInspectorScrub,
    /// scrubable_number の drag / text 編集を終了した瞬間に発火 (非
    /// undoable、 begin 側に snapshot あり)。
    EndInspectorScrub,

    /// docs/plan_text_overlay.md §4 P6: text PiP rect drag で発火する
    /// `SetClipText{X,Y,W,H,Rotation}` 群 (= image と同 idiom)。 lane が
    /// effective なら handler 側で「TextEvent.field を直接書く」 動作で、
    /// lane override が drag を隠す挙動も同様。
    SetClipTextX { target: ClipRef, value: f32 },
    SetClipTextY { target: ClipRef, value: f32 },
    SetClipTextW { target: ClipRef, value: f32 },
    SetClipTextH { target: ClipRef, value: f32 },
    SetClipTextRotation { target: ClipRef, value: f32 },

    /// preview window で PiP rect の drag 操作を始めた瞬間に発火する
    /// marker event (`docs/plan_image_overlay.md` §4 P5)。 handler 本体
    /// は no-op、 is_undoable に含まれるので handle_event 冒頭の
    /// auto push_undo_snapshot だけが効く。 drag 中の SetClipImage*
    /// 連発を 1 個の Undo step に集約する用途 (= AE / Premiere 流の
    /// 「drag 1 stroke = 1 undo」 UX)。 同時に「drag 中の lane recording」
    /// の `ParamGestureBegin` 相当として、 lane を持つ image field を
    /// active_param_gestures に登録する。
    BeginImagePiPDrag,

    /// preview window で PiP rect の drag を終了した瞬間に発火する
    /// (= MouseInput Released)。 `BeginImagePiPDrag` で active_param
    /// _gestures に登録した image field を全て remove する。 non-
    /// undoable (= drag begin 側に snapshot がある)。
    EndImagePiPDrag,

    /// docs/plan_text_overlay.md §4 P6: text PiP rect の drag を開始 /
    /// 終了する marker (`BeginImagePiPDrag` と同 idiom)。 Begin で
    /// `TextBuiltin(_)` lane を `active_param_gestures` に seed、 End で
    /// 全 remove。 Begin は undoable (= drag 1 stroke = 1 undo)、 End は
    /// non-undoable。
    BeginTextPiPDrag,
    EndTextPiPDrag,

    /// docs/plan_text_overlay.md §4 P5: text inspector の編集パス。
    /// Mute / Text content / Font family は string-shaped で個別 event、
    /// 23 numeric field + 2 fade beats は `SetClipTextNumField` 1 event で
    /// `TextNumField` discriminator dispatch する。 FIXME #15 で数値入力は
    /// scrubable_number 化され、 on_change が直接 `SetClipTextNumField` を
    /// 発火 (= 旧 buffer 経路 `ClipTextNumEditChanged` / `CommitClipTextNumEdit`
    /// は撤去)。 lane override 経由でも同様、 lane が effective なら
    /// TextEvent.field の直接書き込みは preview に反映されない。
    SetClipTextMuted { target: ClipRef, muted: bool },
    SetClipTextContent { target: ClipRef, value: String },
    SetClipTextFontFamily { target: ClipRef, value: String },
    SetClipTextAlign { target: ClipRef, value: common::model::TextAlign },
    SetClipTextFadeInCurve { target: ClipRef, curve: common::model::FadeCurve },
    SetClipTextFadeOutCurve { target: ClipRef, curve: common::model::FadeCurve },

    /// FIXME #15: text inspector の scrubable_number on_change から発火
    /// (drag 中 per-frame / text commit)。 `set_clip_text_num_field` で
    /// `value` (= Rotation は radians) を clamp + 全 TextEvent に書く。
    /// 非 undoable (= drag stroke を `Begin/EndInspectorScrub` で bracket)。
    SetClipTextNumField { target: ClipRef, field: TextNumField, value: f32 },

    ClipTextContentEditChanged(String),
    ClipTextFontFamilyEditChanged(String),
    CommitClipTextContentEdit,
    CommitClipTextFontFamilyEdit,

    /// 選択中 text clip の `TextEvent` 現値から文字列 edit buffer
    /// (content / font_family) を再生成。 inspector が clip 切替 / Undo /
    /// Redo の効果を反映するときに呼ぶ。 FIXME #15 で 25 numeric field は
    /// scrubable_number 化され現値を summary から直接読むため、 数値 buffer
    /// の再生成は不要になった。
    ResyncClipTextEditBuffers(ClipRef),
    /// Phase 4 (`docs/plan_automation.md` §6): automation recording mode の
    /// transport 4 way toggle。 session-only / Undo 対象外。
    SetRecordingMode(common::model::RecordingMode),
    /// Phase 7 B3 (2026-05-13): メトロノーム on/off。 transport bar の toggle
    /// で発火、 `AppData.metronome_enabled` を更新 + `MainToChild::Set
    /// MetronomeEnabled(bool)` を audio に送信。 session-only / Undo 対象外。
    SetMetronomeEnabled(bool),
    /// Phase 7 B4 Step C/D (2026-05-13): MIDI 録音 toggle。 Record button
    /// click で発火。 `count_in_bars > 0` なら preroll 開始、 0 なら即時
    /// recording 開始。 既に走行中なら stop。 session-only / Undo 対象外。
    ToggleMidiRecording,
    /// Phase 7 B4 Step C (2026-05-13): count-in bars (0 / 1 / 2) 設定。
    /// transport bar dropdown で発火。 session-only / Undo 対象外。
    SetCountInBars(u8),
    /// Phase 4 Step B (`docs/plan_automation.md` §6): mixer / inspector /
    /// plugin GUI で parameter knob の drag が **開始** した瞬間に発火。
    /// `active_param_gestures` に insert + `last_touched_param` を更新
    /// (= 既存 `TouchParam` の subsume)。 audio thread は Step C で
    /// `recording_mode != Read` 時に該当 lane の curve eval を bypass する。
    /// session-only / Undo 対象外 (= mutation は全て session field)。
    ParamGestureBegin {
        track_id: u32,
        target: common::model::AutomationTarget,
        display_name: String,
    },
    /// Phase 4 Step B: parameter knob の drag が **終了** した瞬間に発火。
    /// `active_param_gestures` から remove。 Touch mode では これで該当
    /// lane の recording が止まる (Latch / Write mode は別の latched set
    /// が transport stop まで持続するので、 本イベントだけでは止まらない)。
    /// session-only / Undo 対象外。
    ParamGestureEnd {
        track_id: u32,
        target: common::model::AutomationTarget,
    },
    /// Phase 2 (`docs/plan_automation.md` §7.5): plugin から param 一覧を
    /// 受信。 plugin_params にキャッシュ。 plugin reload / `params.changed`
    /// 経由で送られるたびに上書き。
    PluginParamListFromChild {
        track: u32,
        slot: common::protocol::PluginSlot,
        plugin_id: u32,
        params: Vec<common::protocol::PluginParamInfo>,
    },
    /// Phase 2: plugin GUI で knob touch (CLAP gesture begin / VST3
    /// beginEdit)。 last_touched_param を plugin param で更新する。
    PluginParamTouchedFromChild {
        track: u32,
        slot: common::protocol::PluginSlot,
        param_id: u32,
        display_name: String,
    },
    /// Phase 2: plugin GUI 内で value 変更 (CLAP out_event PARAM_VALUE
    /// / VST3 performEdit)。 Phase 2 では cache 用、 Phase 4 で recording
    /// mode の point 生成 source。
    PluginParamValueChangedFromChild {
        track: u32,
        slot: common::protocol::PluginSlot,
        param_id: u32,
        value: f64,
    },
    /// Phase 4 Step C-3: plugin GUI で knob を release した通知 (CLAP
    /// PARAM_GESTURE_END 経由)。 daw_gui の `active_param_gestures` から
    /// 対応 PluginParam target を remove する (= Touch mode の recording
    /// 停止 + audio bypass 解除)。
    PluginParamGestureEndFromChild {
        track: u32,
        slot: common::protocol::PluginSlot,
        param_id: u32,
    },
    /// 子プロセス (daw_audio / daw_plugin_host) が exit したことを
    /// `audio_pipe_loop` / `plugin_pipe_loop` が検知して synthetic に
    /// 流す。 handler は re-spawn + Session / OpenWorkerPool 再送 +
    /// state restore (`SetProjectDir` / `LoadSong` / plugin slots) を
    /// 走らせ、 `is_playing = false` でユーザーに「再起動しました」 を
    /// status_message で通知する。 non-undoable。
    ChildDisconnected {
        kind: common::protocol::ChildKind,
    },
    /// gui_01 #029 (M14 Phase 63n-4): lane body 内 clip ギャップ
    /// dblclick で発行される clip 作成イベント。MIDI clip の
    /// `DoubleClickEmpty → CreateClip` と同 idiom の lane 版。
    /// `start_beat` は widget が snap 適用済、`len_beats` は widget
    /// style の `automation_clip_default_len_beats` (default 4.0)。
    CreateAutomationClip {
        lane: common::model::AutomationLaneKey,
        start_beat: f64,
        len_beats: f64,
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
    /// 引数は rename 対象 track の **安定 ID** (positional index ではない)。
    BeginRenameTrack(u32),
    RenameTrackChanged(String),
    CommitRenameTrack,
    CancelRenameTrack,
    /// clip rename (track rename の clip 版)。 右クリックメニュー "Rename"
    /// または F2 で開始、 該当 clip rect に inline text_input を重ねる。
    BeginRenameClip(ClipRef),
    RenameClipChanged(String),
    CommitRenameClip,
    CancelRenameClip,
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
    /// Phase 7 B1-M Step 1 (2026-05-13): MIDI Control Change (CC)。 MIDI Learn
    /// 経路の入力。 GUI handler で midi_learn_target Some なら新規 binding
    /// 追加、 None なら既存 binding lookup → target に値送信。
    MidiControlChange { channel: u8, controller: u8, value: u8 },
    /// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn 開始 (= 「次の CC を
    /// この target に bind」 の意思表示)。 transport bar の Learn button で
    /// 発火、 midi_learn_target = Some(target)。
    StartMidiLearn(common::model::BindingTarget),
    /// Phase 7 B1-M Step 2 (2026-05-13): Learn cancel (= midi_learn_target を
    /// None に戻す)。 user が誤って Learn を始めた場合の取り消し用。
    CancelMidiLearn,
    /// Phase 7 B1-M Step 2 (2026-05-13): 既存 MIDI binding の削除。 inspector
    /// の binding list 等から発火 (= 段階 4 で UI 拡張、 段階 2 では未使用)。
    RemoveMidiBinding(usize),
    MidiInputOpened(Option<String>),

    // -------- Bottom panel -------------------------------------------------
    SelectBottomPanel(u8),

    // -------- Arrangement / clip operations -------------------------------
    SelectClip { target: ClipRef, additive: bool },
    SetClipSelection(Vec<ClipRef>),
    /// Ctrl+A (クリップ領域): 曲全体・全トラックの全クリップを選択。
    /// 一括選択なので view ジャンプ (fit_piano_roll / select_track) は
    /// 起こさない。 既に全選択なら冪等。 selection のみ更新で非 undoable。
    SelectAllClips,
    ClearSelection,
    /// 右クリック「共有を一括選択」 — target と同じ `content_id` を持つ
    /// 全 clip (linked clip group) を選択する。 refcount==1 なら自身 1 個。
    /// 共有グループの可視化 / まとめ移動・削除に使う。
    SelectLinkedClips(ClipRef),
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
    /// FIXME #21: 選択中の clip 群をまとめて共有コピー (linked clip) する
    /// (D shortcut / `docs/plan_clip_share_clone.md` §3.2)。 選択ブロック全体の
    /// span だけ後ろにずらして相対位置を保ったまま複製し (Ctrl+drag と同じ
    /// セマンティクス)、 複製を選択集合にする。 単一 clip では span = clip 長で
    /// 旧 `DuplicateClipShared` と完全一致。 source の `content_id` を流用。
    DuplicateClipsShared(Vec<ClipRef>),
    /// FIXME #21: 選択中の clip 群をまとめて独立コピー (deep clone + 新 ContentId)
    /// する (Alt+D shortcut / §3.3)。 配置・選択は `DuplicateClipsShared` と同じ。
    DuplicateClipsUnique(Vec<ClipRef>),
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
    /// ピアノロールで選択中ノートを複製 (D キー)。選択範囲ぶん後ろにずらして
    /// 複製し、元ノートは据え置き、複製を新しい選択にする (連打で後方へ連鎖)。
    /// selected_clip 無し / 選択空なら no-op。Undoable。
    DuplicateSelectedNotes,
    /// gui_01 #054: piano_roll widget が Ctrl+drag コピー release で発行する
    /// `PianoRollEditRequest::Copy` を変換したもの。各 `(source note id,
    /// new_start_beat, new_pitch)` で source を deep clone し新 note として追加
    /// (元は据え置き)、複製を新選択にする。Undoable。
    CopyNotes(Vec<(u32, f64, u8)>),
    /// gui_01 #017 (M14 Phase 59) で piano_roll widget が L キー → Enter
    /// commit 時に発行する歌詞分配バッチ。 各 `(note_id, lyric)` を指定
    /// `clip_ref` 内で更新。 widget が空文字列を `None` に正規化済みなので
    /// daw_01 側で `is_empty` 判定不要 (None = 歌詞削除)。 1 batch = 1 undo。
    SetNoteLyrics {
        clip_ref: ClipRef,
        lyrics: Vec<(u32, Option<String>)>,
    },

    // -------- Plugin picker / chain ---------------------------------------
    OpenPluginPicker,
    ClosePluginPicker,
    SelectPluginFromDb {
        id: String,
        keep_open: bool,
        open_gui: bool,
    },
    /// プラグインピッカーの検索ボックスが 1 文字毎に発行する。 query を更新し
    /// `refresh_picker_visible` で subsequence 絞り込みを再計算する。
    SetPluginPickerQuery(String),
    /// 検索結果リスト ([`AppData::plugin_picker_visible`]) のカーソルを `delta` だけ
    /// 移動し `[0, visible.len()-1]` で clamp。 visible が空なら no-op。
    /// text_input が focus 中の ↑↓ (gui_01 #057 / Phase 86 `TextInputResponse::nav_up`
    /// / `nav_down`) で発火し、 Enter で `plugin_picker_visible.get(cursor)` を確定する。
    MovePluginPickerCursor(i32),

    // -------- Font picker (Text クリップのフォント選択, FIXME #25) ----------
    /// inspector の Font ボタンで発火。anchor の text クリップを対象に取り、
    /// 元フォントを退避してフォントピッカー modal を開く。初回は background で
    /// システムフォントを列挙する。
    OpenFontPicker,
    /// フォントピッカーを閉じる (= cancel)。preview で変えた font を元に戻す。
    /// modal の on_close (Esc / 外クリック / ✕) から発火。
    CloseFontPicker,
    SetFontPickerQuery(String),
    /// 検索リストのカーソルを移動し、移動先フォントをキャンバスにライブ
    /// プレビュー (非 undo)。
    MoveFontPickerCursor(i32),
    /// マウスが乗った行のフォントをライブプレビュー (cursor を合わせる)。
    HoverFontInPicker(usize),
    /// 行を確定 (= click / Enter)。元→選択を 1 undo step にして font を適用し閉じる。
    CommitFontFromPicker(String),
    /// background のフォント列挙完了。
    FontFamiliesLoaded(Vec<String>),

    /// FIXME #24: プロジェクトロードの background asset decode が 1 件完了する
    /// たびに発火。 staging を caches へ流し込み、 全件完了で gate を外す。
    AssetDecodeTick,

    /// FIXME #26 Phase B: 再スキャンの VST3 note-effect probe 進捗 (done, total)。
    /// load_overlay に「プラグイン走査中 done/total」を出す。
    RescanProgress { done: usize, total: usize },

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
    Tick { samples: u64, peak_l: f32, peak_r: f32, preroll: u64 },
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
        /// `ProcessData` shared memory の名前。 audio engine に
        /// `OpenPluginShmem` を送るのに使う。 SSoT: incoming bridge が
        /// stale な audio_tx clone を握る代わりに、 ここまで運んで AppData
        /// が live な `self.audio_tx` (respawn で差し替わる側) から送る。
        shmem_id: String,
        /// saved state を `state_load(&bytes)` で復元しようとして失敗した
        /// 場合の理由文字列。 `None` = state_load が呼ばれなかった
        /// (= 新規 add)、 or 復元成功。 plugin 自体は default 状態で chain
        /// に挿さっているので「load 失敗」 ではなく「設定が復元されなかった」
        /// 旨を status_message でユーザーに伝える (silent corruption 防止)。
        state_load_error: Option<String>,
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
    /// FIXME #16: arrangement の track header 幅を更新 (gui_01 widget の右端
    /// splitter drag が発火)。 handler 側で 80..480 px に clamp。 session-only。
    SetArrangeHeaderW(f32),
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
    /// v18 (`docs/plan_track_clip_color.md`): track の表示色を設定。
    /// `color == None` で id 由来の導出パレット色 (auto) に戻す。音響的な
    /// 意味はなく model field のみ更新 (= audio engine への送信不要)。Undo 対象。
    SetTrackColor { track: u32, color: Option<[f32; 3]> },
    /// v18 (`docs/plan_track_clip_color.md`): Ableton 流に、track の全 clip の
    /// 色上書き (`Clip.color`) を外して track 色継承に戻す (= 一括 reset)。
    /// track 自身の color は変えない。track header context menu から発火。Undo 対象。
    ResetTrackClipColors { track: u32 },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    /// Phase 7 B4 (2026-05-13): track Record-arm を toggle。 業界標準どおり
    /// caller 側で前状態を反転、 audio engine には `MainToChild::SetTrackArmed`
    /// で確定値を送る。 session-only / Undo 対象外 (= 業界標準は arm を Undo
    /// 履歴に積まない、 mute / solo と同 idiom)。
    ToggleTrackArmed(u32),
    TrackPeaksTick(Vec<(f32, f32)>),

    // -------- Aux send / return ------------------------------------------
    /// master 直下 (`parent_group_id = None`) の通常 track を 1 本作り
    /// `"Return N"` と命名する (N = 既存リターン数 + 1)。 track が選択中なら
    /// その track に `Send { dest = 新リターン, gain 1.0, PostFader, enabled }`
    /// を 1 本足して即座に効果が聞こえるようにする (Ableton "Add Return")。
    /// 構造変化なので full-song resend を trigger する。
    AddReturnTrack,
    /// `src_track_id` の `sends` に `dest_track_id` 宛ての send を 1 本追加。
    /// gain 1.0 / PostFader / enabled。 構造変化 → full-song resend。
    AddSend { src_track_id: u32, dest_track_id: u32 },
    /// `track_id` の `sends[send_idx]` を削除。 構造変化 → full-song resend。
    /// (後続 send の index がずれるが、 resend で schedule が再 compile
    /// されるため問題ない。 automation lane の reindex は本タスク対象外。)
    RemoveSend { track_id: u32, send_idx: usize },
    /// `track_id` の `sends[send_idx].mode` を設定。 tap 位置 (pre/post)
    /// は routing graph に影響するので 構造変化 → full-song resend。
    SetSendMode { track_id: u32, send_idx: usize, mode: SendMode },
    /// `track_id` の `sends[send_idx].gain` を設定 (clamp 0..2) + realtime
    /// `MainToChild::SetSendGain` を送る。 SetTrackVolume と同 idiom、
    /// full-song resend しない (= drag 中の高頻度更新)。
    SetSendGain { track_id: u32, send_idx: usize, gain: f32 },
    /// `track_id` の `sends[send_idx].enabled` を設定 + realtime
    /// `MainToChild::SetSendEnabled` を送る。 full-song resend しない。
    SetSendEnabled { track_id: u32, send_idx: usize, enabled: bool },
    /// 宛先トラックピッカーを開く (= send 元 = `src_track_id`)。
    OpenSendPicker { src_track_id: u32 },
    /// 宛先トラックピッカーを閉じる。
    CloseSendPicker,

    // -------- VOICEVOX ----------------------------------------------------
    // PR-V4: SynthesizeVocal / VocalSynthCompleted は削除済 (builtin
    // VOICEVOX plugin 経由で自動 synth)。
    /// VOICEVOX engine `/singers` の取得結果。 起動時 background thread が
    /// 1 度発行する。 失敗時は空 Vec で送る。
    SingersLoaded(Vec<common::voicevox::VoiceVoxSinger>),
    /// 口パク (lip-sync) 背景ジョブ完了。`regenerate_lipsync_for_track` が
    /// spawn したスレッドが `query_phonemes` の結果を vocal clip 単位で詰めて
    /// 発行し、handler (`apply_lipsync_generated`) が口 track へ反映する。
    /// 派生データなので Undo 対象外 (= `is_undoable` に入れない)。
    LipsyncGenerated {
        vocal_track_id: u32,
        bpm: f32,
        clips: Vec<LipsyncClipResult>,
    },
    /// Track Inspector: vocal track の口パク出力先 (口 track id) を設定。
    /// `None` で解除。設定後に口パクを再生成する。
    SetLipsyncTarget { track: u32, target: Option<u32> },
    /// Track Inspector: 口 track の `mouth_map` の 1 slot (口形状 →
    /// ImageSourceId) を設定。`0` で解除。設定後、この口 track を出力先に
    /// している vocal track の口パクを再生成する。
    SetMouthMapSlot {
        track: u32,
        shape: common::model::MouthShape,
        source_id: common::model::ImageSourceId,
    },
    /// 口パク自動再生成 debounce timer の発火。`mark_lipsync_dirty` が
    /// 立てた timer thread が送る。`lipsync_gen` と一致するときだけ
    /// (= それ以降変更なし) 全 bound vocal track を再生成する。Undo 対象外。
    LipsyncDebounceFired(u64),
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
    /// Phase 7 B4 Step E (2026-05-13): MIDI export menu trigger。 rfd で
    /// path 取得 → `midi_export::export_midi(&song, &path)` で SMF1 書き出し。
    /// 失敗時は status_message に error を出すのみ (= モーダル無し)。
    ExportMidi,

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
    ImportAudio {
        paths: Vec<PathBuf>,
        /// drag&drop で drop position から計算された target track index
        /// (= arrangement view の y 座標から). `None` なら handler 側で
        /// `cursor_track_index().unwrap_or(0)` にフォールバック (= File
        /// menu / 起動 dialog 経由の場合は位置情報がないため)。
        target_track_idx: Option<u32>,
        /// drag&drop の drop X 位置から計算した beat (snap 済み)。 生成する
        /// clip をこの beat に置く (= ドロップしたカーソル位置に貼る)。 `None`
        /// なら handler 側で playhead にフォールバック (= dialog / File menu 経由)。
        target_beat: Option<f64>,
    },

    /// File menu → "Import Audio..." entry. Opens an `rfd` file picker
    /// (multi-select, WAV filter), then forwards the chosen paths to
    /// `AppEvent::ImportAudio`. The dialog itself is `rfd`'s native
    /// modal so we don't need our own ui state. `docs/plan_audio_clip.md`
    /// §3.1 — File menu からの import 経路。
    OpenImportAudioDialog,

    /// Video file import (`docs/plan_video.md` P2). For each path:
    /// copies the video into `<project_dir>/samples/<hash>.<ext>`,
    /// extracts the audio stream to a paired `.wav` via WMF,
    /// registers a `VideoSource` and (when present) the paired
    /// `AudioSource`, and appends a new video track + paired audio
    /// track to `Song.tracks` with one clip each starting at the
    /// playhead. Runs synchronously on the GUI thread — typical MV
    /// clips finish in 1-3s, slow imports leave the user with a
    /// momentary stall instead of a complex completion dispatch.
    ImportVideo {
        paths: Vec<PathBuf>,
        /// drag&drop の drop X 位置から計算した beat (snap 済み)。 生成する
        /// video / 対 audio clip をこの beat に置く。 `None` なら playhead に
        /// フォールバック (= dialog / File menu / smoke test 経由)。
        target_beat: Option<f64>,
    },
    /// v13 (`docs/plan_image_overlay.md` §P2): import one or more
    /// image files (PNG / JPEG / WebP / static), allocating an
    /// `ImageSource` per file and a Clip holding the image as a PiP
    /// overlay. `target_track_idx`: drag&drop の drop 位置から計算した
    /// track index。 既存 track を指していればその track に貼り付け、
    /// track が無い領域 (= 範囲外 index) / dialog 経由 (`None`) なら
    /// arrangement 先頭に新規 track を作って貼る。
    /// Image clips default to aspect-fit PiP; the user shrinks /
    /// positions them in P5 drag handle UI or P4 inspector.
    ImportImage {
        paths: Vec<PathBuf>,
        target_track_idx: Option<u32>,
        /// drag&drop の drop X 位置から計算した beat (snap 済み)。 生成する
        /// image clip をこの beat に置く。 `None` なら playhead / beat 0 に
        /// フォールバック (= dialog / File menu 経由)。
        target_beat: Option<f64>,
    },
    /// v13: open the platform file dialog filtered to supported image
    /// extensions and dispatch the selection as `AppEvent::ImportImage`.
    OpenImportImageDialog,

    /// docs/plan_text_clip_creation.md: 空きレーン右クリック → "Text クリップ" で
    /// `track` (track id) の `start_beat` 位置に `ClipContent::Text` clip を 1 個追加
    /// する。clip は単一 `TextEvent` を default 体裁 (= center band, 64 px white font)
    /// で持つ。text 内容 / styles は inspector、PiP rect は preview drag で編集。
    /// (旧 `AddTextClip` = File menu で新規 track を先頭に作る版は廃止。text トラックは
    /// v16 で他トラックと統一済みのため、他 clip と同じくタイムライン上で生成する。)
    AddTextClipAt { track: u32, start_beat: f64 },

    /// File menu → "Import Video..." entry. Opens an `rfd` file
    /// picker (mp4 / mov / mkv / webm filter) and forwards to
    /// `AppEvent::ImportVideo`.
    OpenImportVideoDialog,

    /// Toggle the video preview window's visibility (`docs/plan_video.md`
    /// P4). When `AppData.preview_window_visible` flips to `true` the
    /// runner creates a second `winit::Window` + `Renderer` pair; flipping
    /// back to `false` (= user closed the window or re-toggled the menu)
    /// destroys it. P4 only opens an empty placeholder window — actual
    /// video frame composite arrives in P5/P7.
    TogglePreviewWindow,

    /// File menu → "Export Video..." (`docs/plan_video.md` P8). Opens
    /// a save dialog for the output mp4 path + a second open dialog
    /// (cancelable) for an optional audio WAV to mux in. Forwards to
    /// `AppEvent::ExportMp4` once the user picks paths.
    OpenExportMp4Dialog,

    /// Synchronous mp4 render at `output_path`, optionally muxing the
    /// PCM Float32 WAV at `audio_wav` as an AAC stream. Blocks the
    /// GUI thread for the duration (= MVP simplicity). v12
    /// (`docs/plan_video.md` P8).
    ExportMp4 {
        output_path: PathBuf,
        audio_wav: Option<PathBuf>,
    },
    /// background export thread が毎フレーム発火（`done` / `total` フレーム）。
    /// `export_progress` を更新して進捗オーバーレイに反映。非 undoable。
    ExportProgress { done: u64, total: u64 },
    /// background export thread の完了通知（成功時は出力 path、失敗 /
    /// キャンセル時は理由）。`export_progress` / `export_cancel` をクリアして
    /// status_message に結果を出す。非 undoable。
    ExportFinished {
        result: Result<PathBuf, String>,
    },
    /// 進捗オーバーレイの Cancel ボタン → 実行中 export の `export_cancel`
    /// フラグを立てる（render loop が次フレームで中断）。非 undoable。
    CancelExport,

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

    /// v18 (`docs/plan_track_clip_color.md`): clip の表示色を設定。
    /// `color == None` でトラック色継承に戻す (Ableton "match track color")。
    /// model field のみ更新。Undo 対象。
    SetClipColor { target: ClipRef, color: Option<[f32; 3]> },

    /// Set `AudioEvent.stretch_mode` for every event in the selected
    /// audio clip. Phase 1 で再生に効くのは `Raw` / `Repitch` のみ;
    /// `Stretch` / `Slice` は §3.7 に従って Raw 同等で再生される
    /// (Phase 3+ で本実装)。
    SetClipStretchMode { target: ClipRef, mode: common::model::StretchMode },

    // ---- Audio event 数値 field 編集 (Phase 2 PR2) ----------------------
    /// FIXME #15: audio / image inspector が `clip_edit_buffer_target` を
    /// `target` に同期するために発火する純 sync marker。 数値 field は
    /// scrubable_number 化され現値を summary から直接読むため buffer 再生成
    /// は不要だが、 text section と共有する `clip_edit_buffer_target` を
    /// 正しい clip に向けておくために残す。 `is_undoable` ではない。
    ResyncClipEditBuffers(ClipRef),

    /// FIXME #15: scrubable_number の on_change が発火する programmatic な
    /// field 設定 (drag 中 per-frame / text commit)。 全 event に broadcast
    /// (`SetClipReversed` 等と同じ semantics)。 非 undoable (= drag stroke
    /// を `Begin/EndInspectorScrub` で 1 undo step に bracket)。
    SetClipGainDb { target: ClipRef, gain_db: f32 },
    SetClipPan { target: ClipRef, pan: f32 },
    SetClipPitchSemitones { target: ClipRef, semitones: f32 },

    // ---- Audio event fade 編集 (Phase 2 PR3) ----------------------------
    /// Fade length / curve の programmatic 設定。 `SetClipGainDb` 等と
    /// 同じ semantics で全 event に broadcast、 値は clip.length_beats
    /// で clamp (= fade が clip より長くならない)。 curve は spec §3.5
    /// の Linear / Exponential / SCurve から選択 (Inspector dropdown 経由)。
    /// `target` の `ClipContent` が `Audio` / `Image` のいずれであっても
    /// fade フィールドが存在するので kind-aware に書き分ける (handler
    /// 側で resolve)。
    SetClipFadeInBeats { target: ClipRef, beats: f64 },
    SetClipFadeOutBeats { target: ClipRef, beats: f64 },
    SetClipFadeInCurve { target: ClipRef, curve: common::model::FadeCurve },
    SetClipFadeOutCurve { target: ClipRef, curve: common::model::FadeCurve },

    // ---- Image event 編集 (`docs/plan_image_overlay.md` §4 P4) -----------
    /// PiP rect / opacity / rotation の programmatic 設定 (Inspector の
    /// scrubable_number on_change から / preview drag handle / JS test API
    /// 経由)。 全 ImageEvent に broadcast。 各値は仕様に従って clamp:
    /// x/y/w/h は [0.0, 1.0]、 opacity も [0.0, 1.0]、 rotation は
    /// `-π..=π` で wrap (= 360° 連続入力可)。 FIXME #15: inspector の
    /// scrubable 化で `ClipImage*EditChanged` / `CommitClipImage*Edit` は
    /// 撤去 (drag stroke を `Begin/EndInspectorScrub` で bracket)。
    SetClipImageX { target: ClipRef, value: f32 },
    SetClipImageY { target: ClipRef, value: f32 },
    SetClipImageW { target: ClipRef, value: f32 },
    SetClipImageH { target: ClipRef, value: f32 },
    SetClipImageOpacity { target: ClipRef, value: f32 },
    /// `value` は radians 単位 (= 内部単位)。 inspector は degree で
    /// 入力するが commit で radians に変換してから発火する。
    SetClipImageRotation { target: ClipRef, value: f32 },

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

    // ---- Bounce (with FX) — Phase 2 PR-C --------------------------------
    /// audio clip を **plugin chain 込み** で render し、 結果を **新 track**
    /// に新 audio clip として配置 (`docs/plan_audio_followup.md` PR-C)。
    /// async (= IPC freewheel render → ChildToMain::BounceClipFxComplete)。
    /// `is_undoable` には入れず、 完了通知 handler 内で
    /// `push_undo_snapshot` を明示呼び出し (= 1 完了 = 1 Undo step)。
    BounceClipWithFx(ClipRef),
    /// Plugin-FX bounce 完了通知 (audio engine 側 thread → main thread)。
    /// `error == None` で `path` の WAV が完全書き出し成功。 `frames`
    /// は実際に書き出された frame 数 (tail 込み)。 `source_track` /
    /// `source_clip` は元 clip 識別子 (= pending entry と照合に使う)。
    BounceClipFxComplete {
        path: PathBuf,
        source_track: u32,
        source_clip: u32,
        error: Option<String>,
        frames: u64,
    },

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

    // ---- Audio Editor scroll / zoom -----------------------------------
    /// Audio Editor の `view_start_beat` を変更 (= 水平 scroll)。
    /// 0 ≤ start ≤ clip.length_beats - view_len_beats で clamp、
    /// `audio_editor_clip` が None なら no-op。 view state なので非 undoable。
    SetAudioEditorScroll(f64),
    /// Audio Editor の `view_start_beat` / `view_len_beats` を一括変更
    /// (= zoom anchor 保持のため start/len 同時更新)。 view_len は
    /// `MIN_AUDIO_EDITOR_VIEW_LEN_BEATS` 以上 + clip.length_beats 以下、
    /// view_start も clamp。 `audio_editor_clip` が None なら no-op。
    SetAudioEditorZoom { view_start_beat: f64, view_len_beats: f64 },

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
    /// Audio Editor の event 選択集合を `indices` で置き換える (= 矩形
    /// 選択 / Shift+click トグル / Ctrl+A 全選択)。 index は clip 内
    /// events Vec への index。 重複は handler 側で除外。 view state なので
    /// 非 undoable。
    SetAudioEditorEventSelection(Vec<usize>),
    /// Audio Editor で選択中の全 event を削除 (= Delete key、 複数選択
    /// 対応)。 `audio_editor_clip` が開いていて選択が空でないときのみ。
    /// 削除後 selection は clear。
    DeleteAudioEditorSelection,

    // -------- Phase 7 B5 (`docs/plan_scale.html`): Scale & Root ------------
    /// 現在 playhead 位置で active な scale event を `(root, scale)` で更新。
    /// `scale_changes` が空なら beat=0 の event を新規追加 (`plan §4.1`)。
    /// 空でなければ `Song::scale_at(playhead)` で見つかる event を update。
    /// undoable (= 1 dropdown commit = 1 Undo step)。
    SetScaleAtPlayhead {
        root: u8,
        scale: common::scale::Scale,
    },
    /// 全 scale event を削除 (= Scale 機能 OFF / chromatic に戻す)。
    /// Transport bar の root dropdown で「— (No Key)」 を選んだとき発火。
    /// undoable。
    ClearScaleChanges,
    /// 既存ノートの pitch を最寄りの in-scale pitch に一括補正。
    /// 対象は `QuantizePitchTarget`。 各 note の `pitch = scale_at(note の
    /// song-global beat).snap(pitch)` で書き換え (note の start_beat 時点の
    /// scale を尊重 = 転調をまたぐ note も自然に補正される)。 1 操作 1 Undo
    /// step。 piano_roll の右クリック menu / inspector ボタン経由で発火。
    QuantizePitchesToScale(QuantizePitchTarget),
    /// Snap on Draw toggle (session-only)。 piano_roll header の toggle で
    /// 切替。 Undo 非対象 (= session 設定)。
    ToggleSnapOnDraw,
    /// Snap Live Input toggle (session-only)。 transport bar の toggle で
    /// 切替。 Undo 非対象。
    ToggleSnapLiveInput,
    /// piano_roll の Fold to Scale toggle (session-only)。 piano_roll snap
    /// toolbar の「Fold」 button で切替。 Undo 非対象。
    ToggleFoldToScale,
}

/// Phase 7 B5: `QuantizePitchesToScale` の対象スコープ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantizePitchTarget {
    /// `selected_clip` + `selected_notes` の note を quantize。 piano_roll で
    /// 範囲選択した note の一括補正。
    SelectedNotes,
    /// `selected_clip` の全 note を quantize (note 選択不要)。 piano_roll header
    /// or arrangement clip 右クリック「Quantize all to Scale」 等から発火。
    SelectedClipAllNotes,
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
        // Video export 中（音声 freewheel → 映像 render）は、export フロー自身の
        // inbound event 以外をすべて drop し、song / transport / plugin を mutate
        // させない。危険な window は音声 freewheel（offline mode）: ここで song を
        // 変えると render 中の snapshot と乖離し、plugin/audio IPC が offline render
        // と競合する。映像 phase でも song mutation は audio と乖離するため一律遮断。
        //
        // この gate の恒久的な存在意義は **gui_01 の入力系を通らない非 UI イベント源**:
        //   - MIDI ハードウェア入力スレッド（`midi.rs` dispatch → proxy.send_event で
        //     MidiNoteOn / MidiControlChange を直送）。export 中のライブ MIDI / CC
        //     (MIDI Learn 経由で TrackVolume 等) が offline render を乱すのを防ぐ。
        //   - IPC bridge（ChildToMain → AppEvent）。
        // これらは背景スレッド発の AppEvent で、main window の widget 入力ではないため
        // gui_01 #065（true modal の pointer/keyboard masking）では遮断できない。
        // main window 上の UI（fader / menu / arrangement 等）の視覚的遮断は #065 の
        // 責務。この gate はその UI event も結果的に落とすが、主目的は上記の非 UI 源。
        //
        // whitelist は export 自身の制御 event のみ:
        //   ExportWavComplete … 音声 render 完了 → action_export_mp4 へ chain
        //   ExportProgress / ExportFinished … background render thread からの進捗/完了
        //   CancelExport … modal の Cancel ボタン（plugin GUI 等の別 OS window 経由の
        //   close は handle_event を通らないので on_tick 側で別 gate）
        if self.pending_video_export.is_some() || self.export_progress.is_some() {
            let allow = matches!(
                event,
                AppEvent::ExportWavComplete { .. }
                    | AppEvent::ExportProgress { .. }
                    | AppEvent::ExportFinished { .. }
                    | AppEvent::CancelExport
            );
            if !allow {
                return;
            }
        }

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
            AppEvent::CloseConfirmSave => self.close_confirm_save(),
            AppEvent::CloseConfirmDiscard => {
                self.show_close_confirm = false;
                self.should_quit = true;
            }
            AppEvent::CloseConfirmCancel => {
                self.show_close_confirm = false;
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
            AppEvent::PreviewPitchChanged { track_idx, pitch } => {
                // gui_01 #055: 押下 pitch を track id 付き held-value に解決し、
                // 前回値と差分して note-on/off を音源トラックへ送る。 track id は
                // reorder race-free な addressing (audio 側で index に再解決)。
                // 対象 track が存在しない / pitch=None なら next=None (= 発音停止)。
                let next = pitch
                    .and_then(|p| self.song.tracks.get(track_idx as usize).map(|t| (t.id, p)));
                for action in diff_preview(self.preview_note, next) {
                    match action {
                        PreviewAction::NoteOff { track_id, pitch } => {
                            self.send_audio(MainToChild::PreviewNoteOff { track_id, pitch });
                        }
                        PreviewAction::NoteOn { track_id, pitch } => {
                            self.send_audio(MainToChild::PreviewNoteOn {
                                track_id,
                                pitch,
                                velocity: PREVIEW_VELOCITY,
                            });
                        }
                    }
                }
                self.preview_note = next;
            }
            AppEvent::LoopSelectedClipToggle => {
                self.loop_selected_clip_toggle();
            }
            AppEvent::BpmEditChanged(s) => {
                self.bpm_edit_text = s;
            }
            AppEvent::CommitBpmEdit => {
                self.commit_bpm_edit();
            }
            AppEvent::SetSongBpmFromScrub(next) => {
                let clamped = next.clamp(1.0, 400.0);
                if (self.song.bpm - clamped).abs() > f32::EPSILON {
                    self.song.bpm = clamped;
                    self.bpm_edit_text = format!("{:.1}", clamped);
                    self.send_audio(MainToChild::SetSongBpm { bpm: clamped });
                    // Phase 6 review (dirty flag fix): scrub 中の連続 commit
                    // は Undo step を増やさない方針なので `push_undo_snapshot`
                    // は呼ばないが、 `is_dirty` は立てる。 立てないと autosave
                    // が走らず、 BPM scrub だけで crash した場合に変更が消える
                    // (= silent data loss)。
                    self.is_dirty = true;
                }
            }
            AppEvent::SetSongTimeSigNumFromScrub(next) => {
                let clamped = next.clamp(1, 32);
                if self.song.time_sig.0 != clamped {
                    self.song.time_sig.0 = clamped;
                    self.time_sig_num_edit_text = clamped.to_string();
                    self.send_audio(MainToChild::SetSongTimeSigNumerator { num: clamped });
                    // 上と同じ理由で autosave 用に dirty flag を立てる。
                    self.is_dirty = true;
                }
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
            AppEvent::AddInstrumentTrack => self.action_add_instrument_track(),
            // FIXME #27: 前面化は runner の user_event が window へ直接行うため、
            // ここには届かない。 match 網羅のための no-op。
            AppEvent::RaiseMainWindow => {}
            AppEvent::GroupSelectedTracks { track_ids } => {
                self.action_group_selected_tracks(&track_ids);
            }
            AppEvent::ToggleTrackAutomationCollapsed { track_id } => {
                // gui_01 #034 (Phase 63n-10): master row の expansion は
                // 通常 track の set とは別 SSoT。
                if track_id == common::model::MASTER_TRACK_ID {
                    self.master_row_automation_expanded =
                        !self.master_row_automation_expanded;
                } else if !self.expanded_automation_tracks.insert(track_id) {
                    self.expanded_automation_tracks.remove(&track_id);
                }
            }
            AppEvent::SetLaneEnabled {
                track_id,
                lane_id,
                enabled,
            } => self.set_lane_enabled(track_id, lane_id, enabled),
            AppEvent::SetLaneVisible {
                track_id,
                lane_id,
                visible,
            } => self.set_lane_visible(track_id, lane_id, visible),
            AppEvent::SetLaneDefault {
                track_id,
                lane_id,
                prev_norm: _,
                next_norm,
            } => self.set_lane_default(track_id, lane_id, next_norm),
            AppEvent::DeleteLane { track_id, lane_id } => {
                self.delete_lane(track_id, lane_id)
            }
            AppEvent::SetLaneHeight {
                track_id,
                lane_id,
                prev_px: _,
                next_px,
            } => self.set_lane_height(track_id, lane_id, next_px),
            AppEvent::SetSingleTrackRowH {
                track_id,
                prev_px: _,
                next_px,
            } => {
                self.track_row_overrides.insert(track_id, next_px);
            }
            AppEvent::AddAutomationPoint {
                track_id,
                lane_id,
                clip_id,
                time_beat,
                value_norm,
            } => self.add_automation_point(track_id, lane_id, clip_id, time_beat, value_norm),
            AppEvent::MoveAutomationPoints { deltas } => {
                self.move_automation_points(&deltas)
            }
            AppEvent::DeleteAutomationPoints { points } => {
                self.delete_automation_points(&points)
            }
            AppEvent::SetAutomationCurveType {
                track_id,
                lane_id,
                clip_id,
                point_idx,
                prev: _,
                next,
            } => self.set_automation_curve_type(track_id, lane_id, clip_id, point_idx, next),
            AppEvent::SetAutomationCurveBezierTension {
                track_id,
                lane_id,
                clip_id,
                point_idx,
                prev: _,
                next,
            } => self.set_automation_curve_bezier_tension(
                track_id, lane_id, clip_id, point_idx, next,
            ),
            AppEvent::SetAutomationCurveExponentialBend {
                track_id,
                lane_id,
                clip_id,
                point_idx,
                prev: _,
                next,
            } => self.set_automation_curve_exponential_bend(
                track_id, lane_id, clip_id, point_idx, next,
            ),
            AppEvent::MoveAutomationClips { deltas } => {
                self.move_automation_clips(&deltas)
            }
            AppEvent::CloneAutomationClipsLinked { deltas } => {
                self.clone_automation_clips_linked(&deltas)
            }
            AppEvent::CloneAutomationClipsIndependent { deltas } => {
                self.clone_automation_clips_independent(&deltas)
            }
            AppEvent::DuplicateAutomationClipsShared(keys) => {
                self.duplicate_automation_clips_shared(&keys);
            }
            AppEvent::DuplicateAutomationClipsUnique(keys) => {
                self.duplicate_automation_clips_unique(&keys);
            }
            AppEvent::ResizeAutomationClips { deltas } => {
                self.resize_automation_clips(&deltas)
            }
            AppEvent::DeleteAutomationClips { keys } => {
                self.delete_automation_clips(&keys)
            }
            AppEvent::SelectAutomationClips { prev: _, next } => {
                self.selected_automation_clips = next;
            }
            AppEvent::SelectAutomationPoints { prev: _, next } => {
                self.selected_automation_points = next;
            }
            AppEvent::QuantizeSelectedAutomationPoints(div) => {
                self.quantize_selected_automation_points(div);
            }
            AppEvent::MakeAutomationClipUnique(key) => {
                self.make_automation_clip_unique(key);
            }
            AppEvent::TouchParam {
                track_id,
                target,
                display_name,
            } => {
                self.last_touched_param = Some(TouchedParam {
                    track_id,
                    target,
                    display_name,
                    touched_at: std::time::Instant::now(),
                });
            }
            AppEvent::AddAutomationFromLastTouched => {
                self.add_automation_from_last_touched();
            }
            AppEvent::AddImageAutomationLane { field } => {
                self.add_image_automation_lane(field);
            }
            AppEvent::RemoveImageAutomationLane { field } => {
                self.remove_image_automation_lane(field);
            }
            AppEvent::AddTextAutomationLane { field } => {
                self.add_text_automation_lane(field);
            }
            AppEvent::RemoveTextAutomationLane { field } => {
                self.remove_text_automation_lane(field);
            }
            AppEvent::AddGroupAutomationLane { param } => {
                self.add_group_automation_lane(param);
            }
            AppEvent::RemoveGroupAutomationLane { param } => {
                self.remove_group_automation_lane(param);
            }
            AppEvent::BeginGroupTransformDrag => {
                // snapshot は is_undoable 経由。group lane recording は未対応。
            }
            AppEvent::SetGroupTransformField { track_id, param, value } => {
                // scrubable_number / preview drag からの live 設定。inspector は
                // track.group_transform を毎フレーム直接読むので resync 不要。
                self.set_group_transform_field(track_id, param, value);
            }
            AppEvent::EndGroupTransformDrag => {}
            // FIXME #15: inspector scrubable_number の drag / text 編集
            // stroke を 1 undo step に bracket。 Begin は is_undoable 経由で
            // snapshot を 1 個取る (本体 no-op)、 End は snapshotless。
            AppEvent::BeginInspectorScrub => {}
            AppEvent::EndInspectorScrub => {}
            AppEvent::BeginImagePiPDrag => {
                // snapshot は is_undoable 経由で既に取られている (=
                // handle_event 冒頭の push_undo_snapshot)。 ここでは
                // lane recording seed のみ:  selected_clip が指す image
                // track に対し、 lane を持つ field を `active_param
                // _gestures` に登録する。 record_automation_points_for
                // _tick が再生中に 1/64 beat 刻みで point を打ち続ける。
                // drag end (= MouseInput Released) で
                // `image_drag_release` 経路から ParamGestureEnd 相当を
                // クリアする。
                self.begin_image_pip_drag_recording();
            }
            AppEvent::EndImagePiPDrag => {
                self.end_image_pip_drag_recording();
            }
            AppEvent::SetRecordingMode(mode) => {
                self.recording_mode = mode;
                self.sync_recording_lanes_with_audio();
            }
            AppEvent::SetMetronomeEnabled(enabled) => {
                // Phase 7 B3 (2026-05-13): metronome on/off。 audio thread は
                // 次 buffer から `render_metronome` の有無を切り替える (= 無効
                // 時は mix step 自体 skip = CPU 0)。 GUI 側は transport bar の
                // toggle UI 更新のみ。
                self.metronome_enabled = enabled;
                self.send_audio(MainToChild::SetMetronomeEnabled(enabled));
            }
            AppEvent::ToggleMidiRecording => {
                self.toggle_midi_recording();
            }
            AppEvent::SetCountInBars(bars) => {
                self.count_in_bars = bars.min(2);
            }
            AppEvent::ParamGestureBegin {
                track_id,
                target,
                display_name,
            } => {
                self.active_param_gestures.insert((track_id, target.clone()));
                // Phase 4 Step C: Latch / Write mode で 再生中の gesture begin は
                // latched_param_gestures にも入れる。 stop まで「触れた事実」 を
                // 保持し、 release 後も curve 上書きを継続する。 Touch mode では
                // latched は使わない (= release で recording 完全停止)。
                if matches!(
                    self.recording_mode,
                    common::model::RecordingMode::Latch | common::model::RecordingMode::Write
                ) && self.is_playing
                {
                    self.latched_param_gestures.insert((track_id, target.clone()));
                }
                // `TouchParam` を発火し続けるより、 gesture begin で `last_touched_param`
                // を更新する idiom に統一する。 (= drag 開始の瞬間が touch、 drag 中
                // の値変化は touch を再発火しない)
                self.last_touched_param = Some(TouchedParam {
                    track_id,
                    target,
                    display_name,
                    touched_at: std::time::Instant::now(),
                });
                self.sync_recording_lanes_with_audio();
            }
            AppEvent::ParamGestureEnd { track_id, target } => {
                self.active_param_gestures.remove(&(track_id, target.clone()));
                // Phase 4 Step C: Touch mode の場合、 release で recording 完全停止 →
                // recording_last_beat からも 該当 entry を消す (= 次の gesture begin
                // で改めて throttle 開始)。 Latch / Write は stop まで latched 継続
                // なので last_beat も保持する (= 連続 record)。
                if self.recording_mode == common::model::RecordingMode::Touch {
                    self.recording_last_beat.remove(&(track_id, target));
                }
                self.sync_recording_lanes_with_audio();
            }
            AppEvent::PluginParamListFromChild {
                track,
                slot,
                plugin_id: _,
                params,
            } => {
                self.plugin_params.insert((track, slot), params);
            }
            AppEvent::PluginParamTouchedFromChild {
                track,
                slot,
                param_id,
                display_name,
            } => {
                // Phase 2c: host から来る `display_name` は placeholder
                // (= "Param N")。 `plugin_params` cache から param の
                // 本来の name を引いて上書きする。 cache に該当 entry
                // がなければ host から送られてきた placeholder をそのまま。
                let resolved_name = self
                    .plugin_params
                    .get(&(track, slot))
                    .and_then(|params| params.iter().find(|p| p.id == param_id))
                    .map(|info| info.name.clone())
                    .unwrap_or(display_name);
                let target = common::model::AutomationTarget::PluginParam { slot, param_id };
                // Phase 4 Step C-3: ParamGestureBegin として同経路で active /
                // latched に反映する (= mixer knob と同 idiom、 audio thread
                // 側 bypass も統一)。 last_touched_param は handler 内で更新。
                self.handle_event(AppEvent::ParamGestureBegin {
                    track_id: track,
                    target,
                    display_name: resolved_name,
                });
            }
            AppEvent::PluginParamValueChangedFromChild {
                track,
                slot,
                param_id,
                value,
            } => {
                // Phase 4 Step C-3: plugin GUI knob の最新値を per-(track, slot,
                // param_id) cache に保存。 `current_plain_value(PluginParam)`
                // が record tick でこの値を read して point を生成する。
                self.plugin_param_values
                    .insert((track, slot, param_id), value);
            }
            AppEvent::ChildDisconnected { kind } => {
                self.handle_child_disconnected(kind);
            }
            AppEvent::PluginParamGestureEndFromChild {
                track,
                slot,
                param_id,
            } => {
                // Phase 4 Step C-3: plugin GUI knob release。 mixer の
                // ParamGestureEnd と同経路に流す (= active_param_gestures
                // から remove + sync_recording_lanes_with_audio で bypass
                // 解除)。
                let target = common::model::AutomationTarget::PluginParam { slot, param_id };
                self.handle_event(AppEvent::ParamGestureEnd {
                    track_id: track,
                    target,
                });
            }
            AppEvent::CreateAutomationClip {
                lane,
                start_beat,
                len_beats,
            } => self.create_automation_clip(lane, start_beat, len_beats),
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
            AppEvent::BeginRenameTrack(track_id) => {
                self.begin_rename_track(track_id);
            }
            AppEvent::RenameTrackChanged(text) => {
                self.track_rename_text = text;
            }
            AppEvent::CommitRenameTrack => self.commit_rename_track(),
            AppEvent::CancelRenameTrack => {
                self.track_rename_id = None;
                self.track_rename_text.clear();
            }
            AppEvent::BeginRenameClip(target) => self.begin_rename_clip(target),
            AppEvent::RenameClipChanged(text) => {
                self.clip_rename_text = text;
            }
            AppEvent::CommitRenameClip => self.commit_rename_clip(),
            AppEvent::CancelRenameClip => {
                self.clip_rename = None;
                self.clip_rename_text.clear();
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
            AppEvent::MidiNoteOff { pitch } => {
                // Phase 7 B4 Step D (2026-05-13): 録音中は note_off で
                // length_beats を確定。 step-input mode は note end を追跡
                // しないので無視。
                self.handle_midi_note_off(pitch);
            }
            AppEvent::MidiControlChange { channel, controller, value } => {
                // Phase 7 B1-M Step 2 (2026-05-13): Learn mode なら binding 追加、
                // 通常モードなら既存 binding lookup で target に値送信。
                self.handle_midi_control_change(channel, controller, value);
            }
            AppEvent::StartMidiLearn(target) => {
                self.midi_learn_target = Some(target);
                self.status_message =
                    "MIDI Learn: 次の CC を bind します...".to_string();
            }
            AppEvent::CancelMidiLearn => {
                self.midi_learn_target = None;
                self.status_message = "MIDI Learn cancel".to_string();
            }
            AppEvent::RemoveMidiBinding(idx) => {
                if idx < self.song.midi_bindings.len() {
                    self.song.midi_bindings.remove(idx);
                }
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
            AppEvent::SelectAllClips => {
                self.select_all_clips();
            }
            AppEvent::ClearSelection => {
                self.selected_clip = None;
                self.selected_clips.clear();
                self.selected_notes.clear();
            }
            AppEvent::SelectLinkedClips(target) => {
                self.select_linked_clips(target);
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
                    && let Some(r) = self.selected_clip_ref()
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
            AppEvent::DuplicateSelectedNotes => self.duplicate_selected_notes(),
            AppEvent::CopyNotes(entries) => self.copy_notes(&entries),
            AppEvent::SetNoteLyrics { clip_ref, lyrics } => {
                self.set_note_lyrics(clip_ref, &lyrics);
            }
            AppEvent::OpenPluginPicker => {
                self.plugin_picker_query.clear();
                self.refresh_picker_visible();
                self.is_plugin_picker_open = true;
            }
            AppEvent::ClosePluginPicker => {
                self.is_plugin_picker_open = false;
                self.plugin_picker_query.clear();
            }
            AppEvent::SetPluginPickerQuery(query) => {
                self.plugin_picker_query = query;
                self.refresh_picker_visible();
            }
            AppEvent::MovePluginPickerCursor(delta) => {
                let len = self.plugin_picker_visible.len();
                if len > 0 {
                    let new = (self.plugin_picker_cursor as i32 + delta)
                        .clamp(0, len as i32 - 1) as usize;
                    self.plugin_picker_cursor = new;
                }
            }
            AppEvent::OpenFontPicker => self.open_font_picker(),
            AppEvent::CloseFontPicker => self.close_font_picker(),
            AppEvent::SetFontPickerQuery(query) => {
                self.font_picker_query = query;
                self.refresh_font_picker_visible();
            }
            AppEvent::MoveFontPickerCursor(delta) => self.move_font_picker_cursor(delta),
            AppEvent::HoverFontInPicker(idx) => self.hover_font_in_picker(idx),
            AppEvent::CommitFontFromPicker(family) => self.commit_font_from_picker(family),
            AppEvent::FontFamiliesLoaded(families) => self.on_font_families_loaded(families),
            AppEvent::AssetDecodeTick => self.on_asset_decode_tick(),
            AppEvent::RescanProgress { done, total } => {
                self.load_progress = Some((done, total));
                self.load_progress_label = "プラグインを走査中";
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
                // 上限は viewport 高に近いところまで広げる (1 トラックを画面いっぱいに
                // 表示できるようにする)。 viewport 高はここでは未知なので大きめに取り、
                // 実描画時は area.h と min を取って絶対に visible 数 0 にならない構造で
                // 描画側 (`tracks_visible = ((area.h - RULER_H) / row_h).max(1.0)`) が
                // 吸収する。
                self.arrange_track_row_h = h.clamp(16.0, 2000.0);
            }
            AppEvent::SetArrangeHeaderW(w) => {
                // track 名が読める下限と lanes を潰さない上限で clamp。 widget は
                // 毎フレーム `view.header_w` としてこの値を読むので即反映される。
                self.arrange_header_w = w.clamp(80.0, 480.0);
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
            AppEvent::SelectPluginFromDb { id, keep_open, open_gui } => {
                self.select_plugin_from_db(id, keep_open, open_gui);
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
            AppEvent::Tick { samples, peak_l, peak_r, preroll } => {
                // Phase 7 B4 Step C: preroll mirror で count-in 完了を検知。
                // midi_recording_pending == true かつ preroll == 0 なら、
                // midi_recording に昇格して以後の MIDI input を armed track に
                // 書き込む。
                if self.midi_recording_pending && preroll == 0 {
                    self.midi_recording_pending = false;
                    self.midi_recording = true;
                }
                let _ = preroll;  // 上で消費
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
            AppEvent::SlotPluginLoadedFromChild { track, slot, id, name, plugin_id, shmem_id, state_load_error } => {
                self.on_plugin_loaded_from_child(track, slot, id, name, plugin_id, shmem_id, state_load_error);
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
            AppEvent::SetTrackColor { track, color } => {
                self.snapshot_for_color_edit();
                if let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track) {
                    t.color = color;
                }
            }
            AppEvent::ResetTrackClipColors { track } => {
                // 全 clip の上書きを外す (= track 色継承)。undo は is_undoable で取得済。
                if let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track) {
                    for clip in &mut t.clips {
                        clip.color = None;
                    }
                }
            }
            AppEvent::ToggleTrackMute(track) => {
                self.toggle_track_mute(track);
            }
            AppEvent::ToggleTrackSolo(track) => {
                self.toggle_track_solo(track);
            }
            AppEvent::ToggleTrackArmed(track) => {
                self.toggle_track_armed(track);
            }
            AppEvent::TrackPeaksTick(peaks) => {
                self.on_track_peaks_tick(&peaks);
            }
            AppEvent::AddReturnTrack => {
                self.action_add_return_track();
            }
            AppEvent::AddSend { src_track_id, dest_track_id } => {
                self.add_send(src_track_id, dest_track_id);
            }
            AppEvent::RemoveSend { track_id, send_idx } => {
                self.remove_send(track_id, send_idx);
            }
            AppEvent::SetSendMode { track_id, send_idx, mode } => {
                self.set_send_mode(track_id, send_idx, mode);
            }
            AppEvent::SetSendGain { track_id, send_idx, gain } => {
                self.set_send_gain(track_id, send_idx, gain);
            }
            AppEvent::SetSendEnabled { track_id, send_idx, enabled } => {
                self.set_send_enabled(track_id, send_idx, enabled);
            }
            AppEvent::OpenSendPicker { src_track_id } => {
                self.send_picker = Some(SendPickerState { src_track_id });
            }
            AppEvent::CloseSendPicker => {
                self.send_picker = None;
            }
            AppEvent::ExportWav => {
                self.action_export_wav();
            }
            AppEvent::ExportMidi => {
                self.action_export_midi();
            }
            AppEvent::ImportAudio { paths, target_track_idx, target_beat } => {
                self.action_import_audio(paths, target_track_idx, target_beat);
            }
            AppEvent::OpenImportAudioDialog => {
                self.action_open_import_audio_dialog();
            }
            AppEvent::ImportVideo { paths, target_beat } => {
                #[cfg(windows)]
                self.action_import_video(paths, target_beat);
                #[cfg(not(windows))]
                {
                    let _ = (paths, target_beat);
                    self.status_message =
                        "Video import は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::OpenImportVideoDialog => {
                #[cfg(windows)]
                self.action_open_import_video_dialog();
                #[cfg(not(windows))]
                {
                    self.status_message =
                        "Video import は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::ImportImage { paths, target_track_idx, target_beat } => {
                self.action_import_image(paths, target_track_idx, target_beat);
            }
            AppEvent::OpenImportImageDialog => {
                self.action_open_import_image_dialog();
            }
            AppEvent::AddTextClipAt { track, start_beat } => {
                self.add_text_clip_to_track(track, start_beat);
            }
            AppEvent::TogglePreviewWindow => {
                self.preview_window_visible = !self.preview_window_visible;
                self.status_message = if self.preview_window_visible {
                    "Video preview: 表示".into()
                } else {
                    "Video preview: 非表示".into()
                };
            }
            AppEvent::OpenExportMp4Dialog => {
                #[cfg(windows)]
                self.action_open_export_mp4_dialog();
                #[cfg(not(windows))]
                {
                    self.status_message =
                        "Video export は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::ExportMp4 { output_path, audio_wav } => {
                #[cfg(windows)]
                self.action_export_mp4(output_path, audio_wav);
                #[cfg(not(windows))]
                {
                    let _ = (output_path, audio_wav);
                    self.status_message =
                        "Video export は Windows 専用 (WMF 経由) です".into();
                }
            }
            AppEvent::ExportProgress { done, total } => {
                self.export_progress = Some((done, total));
            }
            AppEvent::ExportFinished { result } => {
                self.export_progress = None;
                self.export_cancel = None;
                // 自動レンダリングした音声 temp WAV を削除。
                if let Some(wav) = self.export_temp_wav.take() {
                    let _ = std::fs::remove_file(&wav);
                }
                match result {
                    Ok(path) => {
                        self.status_message =
                            format!("Video export 完了: {}", path.display());
                    }
                    Err(e) if e == "export cancelled" => {
                        self.status_message =
                            "Video export をキャンセルしました".into();
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "export failed");
                        self.status_message = format!("Video export 失敗: {e}");
                    }
                }
            }
            AppEvent::CancelExport => {
                if let Some(flag) = &self.export_cancel {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.status_message = "Video export をキャンセル中...".into();
                }
            }
            AppEvent::SetClipReversed { target, reversed } => {
                self.set_clip_audio_event_reversed(target, reversed);
            }
            AppEvent::SetClipColor { target, color } => {
                self.snapshot_for_color_edit();
                propagate_clip_color(&mut self.song.tracks, target, color);
            }
            AppEvent::SetClipMuted { target, muted } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_muted(target, muted);
                } else {
                    self.set_clip_audio_event_muted(target, muted);
                }
            }
            AppEvent::SetClipStretchMode { target, mode } => {
                self.set_clip_audio_event_stretch_mode(target, mode);
            }
            AppEvent::ResyncClipEditBuffers(target) => {
                // FIXME #15: 数値 buffer は撤去済み。 text section と共有する
                // `clip_edit_buffer_target` を target に向ける純 sync。
                self.clip_edit_buffer_target = Some(target);
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
            AppEvent::SetClipFadeInBeats { target, beats } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_in_beats(target, beats);
                } else {
                    self.set_clip_audio_event_fade_in_beats(target, beats);
                }
            }
            AppEvent::SetClipFadeOutBeats { target, beats } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_out_beats(target, beats);
                } else {
                    self.set_clip_audio_event_fade_out_beats(target, beats);
                }
            }
            AppEvent::SetClipFadeInCurve { target, curve } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_in_curve(target, curve);
                } else {
                    self.set_clip_audio_event_fade_in_curve(target, curve);
                }
            }
            AppEvent::SetClipFadeOutCurve { target, curve } => {
                if self.is_image_clip(target) {
                    self.set_clip_image_event_fade_out_curve(target, curve);
                } else {
                    self.set_clip_audio_event_fade_out_curve(target, curve);
                }
            }
            AppEvent::SetClipImageX { target, value } => {
                self.set_clip_image_event_x(target, value);
            }
            AppEvent::SetClipImageY { target, value } => {
                self.set_clip_image_event_y(target, value);
            }
            AppEvent::SetClipImageW { target, value } => {
                self.set_clip_image_event_w(target, value);
            }
            AppEvent::SetClipImageH { target, value } => {
                self.set_clip_image_event_h(target, value);
            }
            AppEvent::SetClipImageOpacity { target, value } => {
                self.set_clip_image_event_opacity(target, value);
            }
            AppEvent::SetClipImageRotation { target, value } => {
                self.set_clip_image_event_rotation_radians(target, value);
            }
            AppEvent::SetClipTextX { target, value } => {
                self.set_clip_text_event_x(target, value);
            }
            AppEvent::SetClipTextY { target, value } => {
                self.set_clip_text_event_y(target, value);
            }
            AppEvent::SetClipTextW { target, value } => {
                self.set_clip_text_event_w(target, value);
            }
            AppEvent::SetClipTextH { target, value } => {
                self.set_clip_text_event_h(target, value);
            }
            AppEvent::SetClipTextRotation { target, value } => {
                self.set_clip_text_event_rotation_radians(target, value);
            }
            AppEvent::BeginTextPiPDrag => {
                self.begin_text_pip_drag_recording();
            }
            AppEvent::EndTextPiPDrag => {
                self.end_text_pip_drag_recording();
            }
            AppEvent::SetClipTextMuted { target, muted } => {
                self.set_clip_text_event_muted(target, muted);
            }
            AppEvent::SetClipTextContent { target, value } => {
                self.set_clip_text_event_content(target, value);
            }
            AppEvent::SetClipTextFontFamily { target, value } => {
                self.set_clip_text_event_font_family(target, value);
            }
            AppEvent::SetClipTextAlign { target, value } => {
                self.set_clip_text_event_align(target, value);
            }
            AppEvent::SetClipTextFadeInCurve { target, curve } => {
                self.set_clip_text_event_fade_in_curve(target, curve);
            }
            AppEvent::SetClipTextFadeOutCurve { target, curve } => {
                self.set_clip_text_event_fade_out_curve(target, curve);
            }
            AppEvent::SetClipTextNumField { target, field, value } => {
                self.set_clip_text_num_field(target, field, value);
            }
            AppEvent::ClipTextContentEditChanged(s) => {
                self.clip_text_content_edit_text = s;
            }
            AppEvent::ClipTextFontFamilyEditChanged(s) => {
                self.clip_text_font_family_edit_text = s;
            }
            AppEvent::CommitClipTextContentEdit => {
                self.commit_clip_text_content_edit();
            }
            AppEvent::CommitClipTextFontFamilyEdit => {
                self.commit_clip_text_font_family_edit();
            }
            AppEvent::ResyncClipTextEditBuffers(target) => {
                self.resync_clip_text_event_edit_buffers(target);
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
            AppEvent::SetAudioEditorScroll(start) => {
                self.set_audio_editor_scroll(start);
            }
            AppEvent::SetAudioEditorZoom { view_start_beat, view_len_beats } => {
                self.set_audio_editor_zoom(view_start_beat, view_len_beats);
            }
            AppEvent::SelectAudioEditorEvent(idx) => {
                self.audio_editor_selected_events = idx.into_iter().collect();
            }
            AppEvent::SetAudioEditorEventSelection(indices) => {
                self.set_audio_editor_event_selection(indices);
            }
            AppEvent::DeleteAudioEditorSelection => {
                self.delete_audio_editor_selection();
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
            AppEvent::ToggleClipReversed(target) => {
                let cur = self.is_clip_audio_event_reversed(target);
                self.set_clip_audio_event_reversed(target, !cur);
            }
            AppEvent::BounceClipInPlace(target) => {
                self.bounce_clip_in_place(target);
            }
            AppEvent::BounceClipWithFx(target) => {
                self.bounce_clip_with_fx(target);
            }
            AppEvent::BounceClipFxComplete {
                path,
                source_track,
                source_clip,
                error,
                frames,
            } => {
                self.handle_bounce_clip_fx_complete(
                    path,
                    source_track,
                    source_clip,
                    error,
                    frames,
                );
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
                if let Some(mp4_path) = self.pending_video_export.take() {
                    // 1 ステップ video export の音声レンダリング完了 → video
                    // export を開始（音声失敗時は映像のみで続行）。
                    let wav = match &error {
                        Some(err) => {
                            tracing::warn!(
                                error = %err,
                                "audio render for video export failed; video-only"
                            );
                            self.status_message = format!(
                                "音声レンダリング失敗 ({err}); 映像のみで書き出します"
                            );
                            if let Some(t) = self.export_temp_wav.take() {
                                let _ = std::fs::remove_file(&t);
                            }
                            None
                        }
                        None => self.export_temp_wav.clone(),
                    };
                    #[cfg(windows)]
                    self.action_export_mp4(mp4_path, wav);
                    #[cfg(not(windows))]
                    let _ = (mp4_path, wav);
                } else if let Some(err) = error {
                    self.status_message = format!("WAV 書き出し失敗: {err}");
                } else {
                    self.status_message = "WAV 書き出し完了".to_string();
                }
            }
            // PR-V4: SynthesizeVocal / VocalSynthCompleted は削除済。
            // vocal track は builtin VOICEVOX plugin が自動 synth する
            // (= sync_vocal_metadata 経由で歌詞 / note を flush →
            // background thread で HTTP synth)。 user の explicit
            // synth トリガは不要。
            AppEvent::SingersLoaded(singers) => {
                tracing::info!(
                    count = singers.len(),
                    "VOICEVOX singers loaded"
                );
                self.singers = singers;
                // Vocal speaker dropdown の派生キャッシュを再構築 (= Track
                // Inspector の毎フレーム format!/clone を消す)。 singers が
                // 変わるのはここだけなので、 ここで 1 度作れば常に整合する。
                self.vocal_speaker_entries.clear();
                self.vocal_speaker_labels.clear();
                for s in &self.singers {
                    for st in &s.styles {
                        self.vocal_speaker_labels
                            .push(format!("{} - {}", s.name, st.name));
                        self.vocal_speaker_entries.push((st.id, st.name.clone()));
                    }
                }
            }
            AppEvent::LipsyncGenerated { vocal_track_id, bpm, clips } => {
                self.apply_lipsync_generated(vocal_track_id, bpm, clips);
            }
            AppEvent::SetLipsyncTarget { track, target } => {
                self.set_lipsync_target(track, target);
            }
            AppEvent::SetMouthMapSlot { track, shape, source_id } => {
                self.set_mouth_map_slot(track, shape, source_id);
            }
            AppEvent::LipsyncDebounceFired(generation) => {
                if generation == self.lipsync_gen {
                    let ids: Vec<u32> = self
                        .song
                        .tracks
                        .iter()
                        .filter(|t| t.lipsync_target_track.is_some())
                        .map(|t| t.id)
                        .collect();
                    for id in ids {
                        self.regenerate_lipsync_for_track(id);
                    }
                }
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
            AppEvent::DuplicateClipsShared(sources) => {
                self.duplicate_clips_shared(&sources);
            }
            AppEvent::DuplicateClipsUnique(sources) => {
                self.duplicate_clips_unique(&sources);
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
            AppEvent::SetScaleAtPlayhead { root, scale } => {
                self.set_scale_at_playhead(root, scale);
            }
            AppEvent::ClearScaleChanges => {
                self.song.scale_changes.clear();
            }
            AppEvent::QuantizePitchesToScale(target) => {
                self.quantize_pitches_to_scale(target);
            }
            AppEvent::ToggleSnapOnDraw => {
                self.snap_on_draw = !self.snap_on_draw;
            }
            AppEvent::ToggleSnapLiveInput => {
                self.snap_live_input = !self.snap_live_input;
            }
            AppEvent::ToggleFoldToScale => {
                self.piano_roll_fold = !self.piano_roll_fold;
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

    pub(crate) fn send_audio(&self, msg: MainToChild) {
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
        // PR-V3: vocal track が builtin VOICEVOX を instrument に持つ場合、
        // notes / bpm 変更を plugin に flush して背景 synth を trigger。
        // 既存 vocal block (= track.instrument is None の旧 project) には
        // 影響しない (= sync_vocal_metadata 内で format check で skip)。
        self.sync_vocal_metadata();
        // 口パク自動再生成 (binding 済み vocal track のみ、debounce 付き)。
        self.mark_lipsync_dirty();
    }

    // -------- File ----------------------------------------------------------

    fn action_new(&mut self) {
        let mut song = Song::default();
        Self::migrate_legacy_vocal_tracks(&mut song);
        self.song = song;
        self.file_path = None;
        self.selected_track_ids.clear();
        self.collapsed_groups.clear();
        self.selected_clip = None;
        self.selected_notes.clear();
        self.resize_track_peak_display();
        // sync 前に migrated vocal track の builtin VOICEVOX を SetSlotPlugin
        // で plugin host に load 要求する (= restore_plugin_from_song と同
        // 経路、 起動直後の Song::default のみ self を持つので clone 経由)。
        let song_snapshot = self.song.clone();
        self.restore_plugin_from_song(&song_snapshot);
        self.sync_song_to_plugin_host();
        self.resync_song_edit_texts();
        // 新規プロジェクトを clean (= '*' 無し) で開始し、 旧プロジェクトの
        // Undo/Redo 履歴を破棄する (sync_song_to_plugin_host が is_dirty を
        // 立てるので、 ここで baseline 確定して打ち消す)。
        self.reset_saved_baseline();
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

    /// Phase 6 review fix: project load 直後に `self.song.audio_sources` 全件
    /// を WAV decode して `self.audio_source_cache` に詰める。 旧コードでは
    /// この path が欠落していて、 saved project を開いた audio clip の波形が
    /// 表示されなかった (= `arrangement_view::draw_audio_clip_waveform` で
    /// `audio_source_cache.get(event.source_id) → None`)。 import 経由 (=
    /// drag&drop / Open Import Audio) で session 中に追加した source は import_one
    /// が即 decode + cache 投入していたので、 そちらだけ波形が出るという
    /// intermittent な見え方になっていた。
    ///
    /// caller は `self.file_path` と `self.song` をセット済の前提。
    /// ProjectRelative は file_path.parent() で resolve、 Generated は廃止
    /// 仕様で skip。 decode 失敗は warn ログのみ (= waveform が出ないだけで
    /// 他機能は動く defensive)。
    /// プロジェクトの audio / image source を **background スレッドで** decode
    /// し、 caches へ逐次取り込む (FIXME #24 / `docs/plan_progress_streaming.md`)。
    /// 旧 `decode_*_sources_into_cache` は GUI スレッドで同期 decode し UI を
    /// 固めていた。 本関数は構造の swap 後に呼ばれ、 work-list を作って 1 本の
    /// thread で順次 decode、 1 件ごとに `AssetDecodeTick` を発火して `on_asset_
    /// decode_tick` が cache へ流し込む (= 波形 / 画像が順次出る streaming load)。
    /// 完了まで `asset_decode` は `Some` で、 再生はこの間 gate される。
    fn begin_asset_decode(&mut self) {
        use common::model::{AudioSourcePath, ImageSourcePath};
        // file_path = None (= 未保存 project の sidecar 復元) の場合、
        // ProjectRelative は resolve できないので skip。
        let project_dir: Option<PathBuf> = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // 未 cache の audio source だけ work-list に (idempotent)。
        let mut audio_jobs: Vec<(common::model::AudioSourceId, PathBuf)> = Vec::new();
        for (&source_id, source) in &self.song.audio_sources {
            if self.audio_source_cache.contains(source_id) {
                continue;
            }
            let abs = match &source.path {
                AudioSourcePath::Absolute(abs) => abs.clone(),
                AudioSourcePath::ProjectRelative(rel) => match project_dir.as_ref() {
                    Some(dir) => dir.join(rel),
                    None => continue,
                },
                // PR-V4 で廃止 (builtin VOICEVOX plugin 経由)。
                AudioSourcePath::Generated { .. } => continue,
            };
            audio_jobs.push((source_id, abs));
        }
        // 未 staging の image source。
        let mut image_jobs: Vec<(common::model::ImageSourceId, PathBuf)> = Vec::new();
        for (&source_id, source) in &self.song.image_sources {
            if self.image_source_bgra.contains_key(&source_id) {
                continue;
            }
            let abs = match &source.path {
                ImageSourcePath::Absolute(abs) => abs.clone(),
                ImageSourcePath::ProjectRelative(rel) => match project_dir.as_ref() {
                    Some(dir) => dir.join(rel),
                    None => continue,
                },
            };
            image_jobs.push((source_id, abs));
        }

        let total = audio_jobs.len() + image_jobs.len();
        if total == 0 {
            self.asset_decode = None;
            return;
        }
        let staging = Arc::new(Mutex::new(AssetDecodeStaging { total, ..Default::default() }));
        self.asset_decode = Some(Arc::clone(&staging));
        self.load_progress = Some((0, total));
        self.load_progress_label = "プロジェクトを読込中";
        let proxy = self.event_proxy.clone();
        std::thread::spawn(move || {
            for (id, abs) in audio_jobs {
                let decoded = crate::import_audio::decode_wav(&abs)
                    .map_err(|e| {
                        tracing::warn!(path = %abs.display(), error = %e, "asset decode (audio) failed");
                    })
                    .ok()
                    .map(std::sync::Arc::new);
                if let Ok(mut g) = staging.lock() {
                    if let Some(buf) = decoded {
                        g.audio.push((id, buf));
                    }
                    g.done += 1;
                }
                proxy.send(AppEvent::AssetDecodeTick);
            }
            for (id, abs) in image_jobs {
                let decoded = decode_image_to_bgra(&abs);
                if let Ok(mut g) = staging.lock() {
                    if let Some(img) = decoded {
                        g.image.push((id, img));
                    }
                    g.done += 1;
                }
                proxy.send(AppEvent::AssetDecodeTick);
            }
        });
    }

    /// background decode から 1 件 decode 完了するたびに発火。 staging に
    /// 溜まった結果を self caches へ流し込み (= 該当 clip の波形 / 画像が描画
    /// 開始)、 全件完了で gate を外して queue 中の Play を流す。
    fn on_asset_decode_tick(&mut self) {
        let Some(staging) = self.asset_decode.clone() else {
            return;
        };
        let (audio, image, done, total) = {
            let Ok(mut g) = staging.lock() else {
                return;
            };
            (
                std::mem::take(&mut g.audio),
                std::mem::take(&mut g.image),
                g.done,
                g.total,
            )
        };
        for (id, buf) in audio {
            self.audio_source_cache.insert(id, buf);
        }
        for (id, (w, h, bytes)) in image {
            self.image_source_bgra.insert(id, (w, h, bytes));
            self.pending_image_uploads.push(id);
        }
        if done >= total {
            tracing::info!(total, "asset decode complete");
            self.asset_decode = None;
            self.load_progress = None;
            // 読込完了 → gate していた Play を流す (plugin gate が残っていれば
            // play() が再 queue する)。
            if self.pending_play {
                self.pending_play = false;
                self.play();
            }
        } else {
            self.load_progress = Some((done, total));
        }
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
                Self::migrate_legacy_vocal_tracks(&mut song);
                self.restore_plugin_from_song(&song);
                self.song = song;
                self.file_path = Some(path.clone());
                // FIXME #24: audio / image source の decode は重いので background
                // スレッドへ。 構造は既に swap 済みなので即操作可、 波形 / 画像は
                // streaming で順次出る (begin_asset_decode → AssetDecodeTick)。
                self.begin_asset_decode();
                self.selected_track_ids.clear();
                self.collapsed_groups.clear();
                self.selected_clip = None;
                self.selected_notes.clear();
                self.resize_track_peak_display();
                self.sync_song_to_plugin_host();
                self.resync_song_edit_texts();
                // load した内容を新しい保存ベースラインに確定し、 前プロジェクトの
                // Undo/Redo 履歴を破棄する (reset_saved_baseline 内で is_dirty=false)。
                self.reset_saved_baseline();
                // sidecar 検出: 前回のセッションが正常終了せず、 同 file の
                // autosave が残っているなら recovery modal に追加。 ユーザーが
                // 「復元」 で sidecar に切り替えられる。
                let sidecar = common::recovery::sidecar_for(&path);
                if sidecar.exists() && !self.recovery_candidates.contains(&sidecar) {
                    // sidecar が .daw より新しいときだけ復元候補に出す。 古い
                    // (= 保存後の消し損ね / unclean exit 残骸) は stale なので
                    // 提示せず掃除する (delete-on-save の取りこぼし救済)。
                    if Self::recovery_sidecar_is_newer(&sidecar, &path) {
                        tracing::info!(
                            sidecar = %sidecar.display(),
                            "sidecar autosave detected on open (newer than saved file)"
                        );
                        self.recovery_candidates.push(sidecar);
                        self.show_recovery_modal = true;
                    } else {
                        tracing::info!(
                            sidecar = %sidecar.display(),
                            "stale sidecar autosave (not newer than saved file); removing"
                        );
                        let _ = std::fs::remove_file(&sidecar);
                    }
                }
                self.push_recent(path);
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to load project");
                self.status_message = format!("Open 失敗: {e:#}");
            }
        }
    }

    /// File メニュー「Open Recent」 / 「Recently Saved」 用の display label
    /// を再計算。 `RecentFiles.paths` (= PathBuf 列) から basename を抽出
    /// した String 列を返す。 lifetime 都合上、 menu widget の `&'a str`
    /// label として渡すために AppData field に持つ必要があるので、 push 時に
    /// 呼ぶ。 起動時 (= `new`) では `init_recent_labels` の方で 1 回呼ぶ。
    fn rebuild_recent_labels(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| p.display().to_string())
            })
            .collect()
    }

    /// 起動直後 / load 直後に label cache を 1 度更新する helper。
    /// `AppData::new` が `recent_files: load_recent_files()` で paths を
    /// 復元するため、 同時に label cache も復元したい。 caller (= bootstrap)
    /// が `app.init_recent_labels()` を 1 回呼ぶ。
    pub fn init_recent_labels(&mut self) {
        self.recent_files_labels =
            Self::rebuild_recent_labels(&self.recent_files.paths);
        self.recent_saved_labels =
            Self::rebuild_recent_labels(&self.recent_saved.paths);
    }

    /// 「最近開いたファイル」 履歴に追加。 `recent.json` に永続化。
    fn push_recent(&mut self, path: PathBuf) {
        self.recent_files.push(path);
        self.recent_files_labels =
            Self::rebuild_recent_labels(&self.recent_files.paths);
        if let Some(disk) = self.app_dirs.as_ref().map(|d| d.recent())
            && let Err(e) = common::recent::save(&disk, &self.recent_files)
        {
            tracing::warn!(
                error = ?e,
                path = %disk.display(),
                "failed to persist recent files"
            );
        }
    }

    /// 「最近保存したファイル」 履歴に追加。 `recent_saved.json` に永続化。
    /// 開いた履歴 (`recent_files`) と完全に独立。 Save / Save As の両 path
    /// で実 file 書き込み成功後に呼ぶ。
    fn push_recent_saved(&mut self, path: PathBuf) {
        self.recent_saved.push(path);
        self.recent_saved_labels =
            Self::rebuild_recent_labels(&self.recent_saved.paths);
        if let Some(disk) = self.app_dirs.as_ref().map(|d| d.recent_saved())
            && let Err(e) = common::recent::save(&disk, &self.recent_saved)
        {
            tracing::warn!(
                error = ?e,
                path = %disk.display(),
                "failed to persist recent saved files"
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
                let Some(dir) =
                    self.app_dirs.as_ref().map(|d| d.recovery_dir())
                else {
                    // 永続化先未設定 (= test 等)。 未保存 project の autosave は skip。
                    return;
                };
                if let Err(e) = common::recovery::ensure_recovery_dir(&dir) {
                    tracing::warn!(error = ?e, "failed to create recovery dir");
                    return;
                }
                common::recovery::recovery_path_for_session(
                    &dir,
                    &self.recovery_session_id,
                )
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

    /// 手動保存成功後に、 この project に紐づく autosave を削除する。
    /// `maybe_autosave` が書く 2 箇所 (sidecar / session recovery file) を両方
    /// 消し、 `recovery_candidates` からも除く。 これで save 直後に unclean
    /// exit (クラッシュ / 強制終了) しても、 次回起動の recovery modal に
    /// 「save より古い」 候補が出ず、 保存内容を巻き戻すリスクを断つ。
    fn clear_stale_autosave_after_save(&mut self, saved_path: &Path) {
        let mut stale: Vec<PathBuf> = vec![common::recovery::sidecar_for(saved_path)];
        if let Some(dir) = self.app_dirs.as_ref().map(|d| d.recovery_dir()) {
            stale.push(common::recovery::recovery_path_for_session(
                &dir,
                &self.recovery_session_id,
            ));
        }
        for p in stale {
            if p.exists() {
                match std::fs::remove_file(&p) {
                    Ok(()) => {
                        tracing::info!(path = %p.display(), "removed stale autosave after save")
                    }
                    Err(e) => tracing::warn!(
                        error = ?e,
                        path = %p.display(),
                        "failed to remove stale autosave after save"
                    ),
                }
            }
            self.recovery_candidates.retain(|c| c != &p);
        }
        // 次の autosave までの 60s タイマーを reset (= save 直後に即書き戻さない)。
        self.last_autosave = std::time::Instant::now();
    }

    /// sidecar autosave が元 `.daw` より新しい (= 前回 unclean exit 時の未保存
    /// 変更を表す) かを mtime で判定する。 どちらかの mtime が取れない場合は
    /// 安全側に倒して `true` (= 候補に出して user 判断に委ねる) を返す。
    fn recovery_sidecar_is_newer(sidecar: &Path, daw: &Path) -> bool {
        let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        match (mtime(sidecar), mtime(daw)) {
            (Some(s), Some(d)) => s > d,
            _ => true,
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
        // FIXME #24: recovery 復元も load path と同じく background streaming
        // decode へ。 file_path を先にセット済みなので ProjectRelative も解決可。
        self.begin_asset_decode();
        self.selected_track_ids.clear();
        self.collapsed_groups.clear();
        self.selected_clip = None;
        self.selected_notes.clear();
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        self.resync_song_edit_texts();
        // 復元した内容を新しい保存ベースラインに確定し、 履歴を破棄する。
        self.reset_saved_baseline();
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
        if let Some(dir) = self.app_dirs.as_ref().map(|d| d.recovery_dir()) {
            let p = common::recovery::recovery_path_for_session(
                &dir,
                &self.recovery_session_id,
            );
            if p.exists()
                && let Err(e) = std::fs::remove_file(&p)
            {
                tracing::warn!(
                    error = ?e,
                    path = %p.display(),
                    "failed to remove recovery file on shutdown"
                );
            }
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

        // Phase B: 各 track の slot 列を diff。 純粋関数で action 列を
        // 計算し、 順に IPC を dispatch する (test しやすさのため切り出し)。
        let Some(db) = self.plugin_db.clone() else {
            // plugin DB が未ロードなら SetSlotPlugin の組み立て不可。
            // RemoveSlotPlugin 単体は db 不要だが、 Phase B はまとめて
            // skip する (= db ロード待ち)。
            if !self.song.tracks.is_empty() {
                tracing::warn!("reconcile: plugin database not loaded; phase B skipped");
            }
            return;
        };
        let actions = compute_slot_reconcile_actions(&self.song, &self.loaded_slots);
        for action in actions {
            match action {
                SlotReconcileAction::RemoveSlot { track_id, slot } => {
                    tracing::info!(track_id, ?slot, "reconcile: removing extra host slot");
                    self.send_plugin(MainToChild::RemoveSlotPlugin {
                        track: track_id,
                        slot,
                    });
                    self.cleanup_slot_gui(track_id, slot);
                    self.loaded_slots.remove(&(track_id, slot));
                    self.pending_plugin_loads.remove(&(track_id, slot));
                }
                SlotReconcileAction::LoadSlot {
                    track_id,
                    slot,
                    plugin_id_str,
                    initial_state,
                } => {
                    let Some(entry) = db.find_by_id(&plugin_id_str) else {
                        tracing::error!(
                            id = %plugin_id_str,
                            track = track_id,
                            ?slot,
                            "reconcile: plugin id not in database"
                        );
                        continue;
                    };
                    tracing::info!(
                        track_id,
                        ?slot,
                        plugin_id = %plugin_id_str,
                        "reconcile: loading slot from song"
                    );
                    self.track_pending_load(track_id, slot);
                    self.send_plugin(MainToChild::SetSlotPlugin {
                        track: track_id,
                        slot,
                        format: entry.format,
                        path: entry.path.clone(),
                        plugin_id: entry.id.clone(),
                        initial_state,
                    });
                }
            }
        }
    }

    /// PR-V3 後段: 旧 project file を読み込んだとき、 `track.source =
    /// Vocal` で `track.instrument` が空の track を「builtin VOICEVOX が
    /// instrument に load された状態」 に書き換える。 caller (= action_
    /// open_path / action_new) は本関数で `&mut song` を migrate してから
    /// `restore_plugin_from_song` に渡す → 通常の plugin restore と同じ
    /// 経路で daw_plugin_host 側に SetSlotPlugin が飛ぶ。
    ///
    /// 既に instrument が居る vocal track (= 既に PR-V3 前段で auto-load
    /// 済 or 手動で plugin を入れた) は touch しない。 idempotent。
    fn migrate_legacy_vocal_tracks(song: &mut Song) {
        for track in &mut song.tracks {
            let is_legacy_vocal = matches!(
                track.source,
                common::model::InstrumentSource::Vocal { .. }
            ) && track.instrument.is_none();
            if !is_legacy_vocal {
                continue;
            }
            track.instrument = Some(common::model::PluginInstance {
                plugin_id: common::plugin_db::BUILTIN_ID_VOICEVOX.to_string(),
                format: PluginFormat::Builtin,
                state: None,
                sidechain_sources: Vec::new(),
            });
            tracing::info!(
                track_id = track.id,
                track_name = %track.name,
                "PR-V3: legacy vocal track migrated to builtin VOICEVOX"
            );
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
        // master bus fx chain も `(MASTER_TRACK_ID, Fx(i))` で送る。
        for (i, p) in song.master_fx_chain.iter().enumerate() {
            to_send.push((common::model::MASTER_TRACK_ID, PluginSlot::Fx(i as u32), p.clone()));
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

    /// ウィンドウを閉じる要求 (`WindowEvent::CloseRequested`) のエントリ。
    /// 未保存変更があれば確認モーダルを開き、 無ければ即 `should_quit` を
    /// 立てる (= Runner が cleanup + exit する)。 ふつうの DAW と同じく
    /// 「閉じる前に保存するか確認」 する。
    pub fn request_close(&mut self) {
        // 既に終了確定 / 非同期保存待ち中なら、 ✕ 連打でモーダルを
        // 再表示しない (= 二重操作の無視)。
        if self.should_quit || self.quit_after_save {
            return;
        }
        if self.is_dirty {
            self.show_close_confirm = true;
        } else {
            self.should_quit = true;
        }
    }

    /// close 確認モーダルで「保存して終了」 を選んだ処理。 save を発行し:
    /// - 同期保存が済んだ (plugin 無し / 既存 path) → 即 `should_quit`
    /// - plugin state 取得待ちで非同期保存が enqueue された → `quit_after_save`
    ///   を立て、 `on_all_states_from_child` の完了で終了する
    /// - Save As ダイアログをキャンセルした (保存されず pending も無い) →
    ///   何もしない (モーダルは閉じてアプリに留まる)
    fn close_confirm_save(&mut self) {
        self.show_close_confirm = false;
        self.action_save();
        if !self.is_dirty {
            self.should_quit = true;
        } else if self.has_pending_save() {
            self.quit_after_save = true;
        }
    }

    /// `pending_state_queue` に未処理の `Save` request が残っているか。
    /// 非同期保存 (plugin state 取得待ち) の in-flight 判定に使う。
    fn has_pending_save(&self) -> bool {
        self.pending_state_queue
            .iter()
            .any(|r| matches!(r, PendingStateRequest::Save { .. }))
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
        !self.song.master_fx_chain.is_empty()
            || self.song.tracks.iter().any(|t| {
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
    fn apply_plugin_states_to(song: &mut Song, states: &[SlotState]) {
        for s in states {
            // Phase 6 review (silent corruption fix): plugin_host が
            // `state_save()` で `Err` を返したエントリは `error` 付きで
            // 来る。 そのとき `s.data` は None なので、 既存 state を
            // 上書きすると **過去 save 時に保存された state が消える**
            // (= 旧バグ: save 失敗 → 次 save で空 state 確定)。 error あり
            // のエントリは skip して既存 state を保つ。
            if s.error.is_some() {
                tracing::warn!(
                    track = s.track,
                    ?s.slot,
                    error = s.error.as_deref(),
                    "apply_plugin_states: state save errored, preserving previous state",
                );
                continue;
            }
            // master bus は track ではなく Song.master_fx_chain に書き戻す
            // (audio fx のみなので Fx slot だけ扱う)。
            if s.track == common::model::MASTER_TRACK_ID {
                if let PluginSlot::Fx(i) = s.slot
                    && let Some(p) = song.master_fx_chain.get_mut(i as usize)
                {
                    p.state = s.data.clone();
                }
                continue;
            }
            let Some(track) = song.tracks.iter_mut().find(|t| t.id == s.track) else {
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

    /// `RequestAllStates` 待ちの request を [`AppData::pending_state_queue`]
    /// に積む。 queue が空 (= 現在 in-flight なし) なら同時に
    /// `RequestAllStates` を 1 発送る。 既に in-flight なら積むだけで
    /// IPC は発行しない (= 先行 request の応答処理時に次の `RequestAllStates`
    /// が改めて送られる、 [`AppData::on_all_states_from_child`] 参照)。
    fn enqueue_state_request(&mut self, req: PendingStateRequest) {
        let was_idle = self.pending_state_queue.is_empty();
        self.pending_state_queue.push_back(req);
        if was_idle {
            self.dispatch_front_state_request();
        }
    }

    /// queue 先頭 request の state 収集を開始する (= `RequestAllStates` 送信)。
    /// 先頭が **まだ snapshot を持たない `Save`** なら、 送信する **この瞬間** の
    /// live song を凍結して snapshot に充填する。 これで snapshot の plugin slot
    /// 配置と、 host が `RequestAllStates` を処理して返す state の配置が同時刻
    /// サンプリングになる: FIFO IPC により、 この送信より前に出された layout 変更
    /// (先行 Deferred の `RemoveSlotPlugin` 等) は既に host で処理済み・live にも
    /// 反映済みであり、 この送信より後の変更は host では `RequestAllStates` の後に
    /// 処理されるため、 返る state は必ず「今 live にある配置」 と一致する。
    fn dispatch_front_state_request(&mut self) {
        let needs_snapshot = matches!(
            self.pending_state_queue.front(),
            Some(PendingStateRequest::Save { snapshot: None, .. })
        );
        if needs_snapshot {
            let snap = Box::new(self.song.clone());
            if let Some(PendingStateRequest::Save { snapshot, .. }) =
                self.pending_state_queue.front_mut()
            {
                *snapshot = Some(snap);
            }
        }
        self.send_plugin(MainToChild::RequestAllStates);
    }

    /// project save の trigger。 plugin がある場合は plugin_host から
    /// 最新 state を取って Song に書き戻してから save する。 plugin が
    /// 1 つもなければ即 save。 既に `RequestAllStates` 在線中なら queue
    /// に積んで先行 request の応答後に処理させる (= 順序保持)。
    fn begin_save(&mut self, path: PathBuf) {
        if !self.song_has_plugin() {
            // plugin が無ければ state 収集 (RequestAllStates) は不要。 今の live を
            // そのまま凍結して即 serialize する。 cache migration は finish_save 内で
            // 行う (= live と snapshot の両方に適用、 file_path は成功時のみ確定)。
            let snapshot = Box::new(self.song.clone());
            self.finish_save(snapshot, path);
            return;
        }
        // plugin 有り: snapshot は **state 収集を始める瞬間** に取る (co-temporal)。
        // ここでは None で積み、 dispatch_front_state_request が RequestAllStates を
        // 送るその瞬間に live を凍結する。 こうすると snapshot の plugin slot 配置と、
        // 返ってくる state の配置が一致し、 待機中の slot 削除等による誤適用が消える。
        self.enqueue_state_request(PendingStateRequest::Save { path, snapshot: None });
    }

    /// plugin state 取得待ちで save が非同期進行中か (= queue に Save あり)。
    /// FIXME #24: この間 load_overlay が「保存中…」インジケータを出す
    /// (= 非ブロック、 編集は続行可)。
    pub(crate) fn is_async_save_pending(&self) -> bool {
        self.pending_state_queue
            .iter()
            .any(|r| matches!(r, PendingStateRequest::Save { .. }))
    }

    /// `song` 内の未保存 import/bounce cache source を `<project_dir>/samples,bounce/`
    /// へ移して path を `ProjectRelative` に書き換える。 save flow で **直列化する
    /// snapshot と working state の live の両方** に適用する: ファイルは move なので、
    /// 片方だけ移すと他方が移動後ファイルを見失う (= 初回呼び出しが move、 2 回目以降は
    /// dst.exists で path 書換のみ)。 失敗しても save は続行し missing source として
    /// 扱う。 status へ最後の失敗メッセージを残す (`&mut status` で借用衝突を避ける)。
    fn migrate_unsaved_sources(song: &mut Song, project_dir: &Path, status: &mut String) {
        // Phase 1 PR3: 未保存 project 中に import した audio source (`docs/plan_audio_clip.md`
        // §13 Q2)。 Phase 2 PR-C: 未保存 project の Bounce 出力 (`docs/plan_audio_followup.md`)。
        if let Err(e) = import_audio::migrate_unsaved_audio_sources_into(song, project_dir) {
            tracing::warn!(error = ?e, "import_cache → samples/ への移行で一部失敗");
            *status = format!("Audio sources の samples/ 移行で一部失敗: {e}");
        }
        if let Err(e) = import_audio::migrate_unsaved_bounce_sources_into(song, project_dir) {
            tracing::warn!(error = ?e, "bounce_cache → bounce/ への移行で一部失敗");
            *status = format!("Audio sources の bounce/ 移行で一部失敗: {e}");
        }
    }

    /// 凍結済み `snapshot` をファイルへ書き出して保存を完了する。
    ///
    /// cache migration は **2 段階**で行い、 破壊的なファイル移動を serialize 成功後に
    /// のみ確定する: (1) serialize 前に snapshot の audio path だけを `ProjectRelative`
    /// へ書き換えて move plan を取る (I/O なし)、 (2) serialize 成功後に plan を commit
    /// (実ファイル move) し、 live も migrate する。 こうすると書き出し失敗時に
    /// import_cache のファイルが無傷で残り、 live は `Absolute(cache)` のまま
    /// autosave/recovery が健全に働く。 **serialize が成功して初めて** file_path を確定し
    /// (旧契約)、 audio engine へ新 project_dir + song を流す。 saved baseline = snapshot、
    /// `is_dirty` は live と snapshot の差で再計算する (state 待ちの間の編集が live に
    /// あれば dirty)。
    fn finish_save(&mut self, mut snapshot: Box<Song>, path: PathBuf) {
        // serialize する snapshot の path を ProjectRelative に書き換え、 実ファイル
        // 移動の plan を取る (= ここでは I/O しない、 破棄しても無害)。
        let (audio_moves, bounce_moves) = match path.parent() {
            Some(dir) => (
                import_audio::plan_unsaved_audio_migration(&mut snapshot, dir),
                import_audio::plan_unsaved_bounce_migration(&mut snapshot, dir),
            ),
            None => (Vec::new(), Vec::new()),
        };
        match common::project::save(&path, &snapshot) {
            Ok(()) => {
                tracing::info!(path = %path.display(), "saved project");
                // serialize 成功 → 破壊的 migration を確定する。 まず snapshot 由来の
                // ファイルを move (plan を commit)、 次に live を migrate して live も
                // ProjectRelative + 自己完結にする (plan 済みファイルは dst.exists で
                // dedup、 live 固有 source があれば move)。
                if let Err(e) = import_audio::commit_migration(&audio_moves) {
                    tracing::warn!(error = ?e, "samples/ への移行確定で一部失敗");
                    self.status_message =
                        format!("Audio sources の samples/ 移行で一部失敗: {e}");
                }
                if let Err(e) = import_audio::commit_migration(&bounce_moves) {
                    tracing::warn!(error = ?e, "bounce/ への移行確定で一部失敗");
                    self.status_message =
                        format!("Audio sources の bounce/ 移行で一部失敗: {e}");
                }
                if let Some(dir) = path.parent() {
                    Self::migrate_unsaved_sources(&mut self.song, dir, &mut self.status_message);
                }
                // serialize 成功時のみ file_path を確定する (旧契約)。
                self.file_path = Some(path.clone());
                // saved baseline = 直列化した snapshot そのもの。 save 後も Undo
                // できるよう履歴は残す (= reset_saved_baseline は使わない)。
                self.saved_song = *snapshot;
                // live が snapshot から乖離している (state 待ち中の編集) なら dirty。
                self.recompute_dirty();
                // 保存成功後、 この project の autosave (sidecar + 未保存→Save As
                // 用の session recovery file) を削除する。 save 後の .daw が
                // authoritative なので、 古い autosave が残ると unclean exit 後の
                // 次回 Open / 起動で recovery modal が「save より古い」 状態を提示し、
                // 復元すると保存内容を巻き戻してしまう。
                self.clear_stale_autosave_after_save(&path);
                // 保存内容が source of truth になったので、 同 file の sidecar
                // autosave (前回までの未保存 snapshot) を削除する。 残すと
                // クラッシュ / 強制終了でクリーン終了処理が走らなかったとき、
                // 次回 Open 時に recovery modal が「save より古い状態」 を復元
                // 候補として提示してしまう (= 保存した作業の巻き戻し事故)。
                let sidecar = common::recovery::sidecar_for(&path);
                match std::fs::remove_file(&sidecar) {
                    Ok(()) => tracing::info!(
                        sidecar = %sidecar.display(),
                        "removed stale sidecar autosave after save"
                    ),
                    // NotFound は正常 (autosave 未作成 / Save As の新規 path)。
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => tracing::warn!(
                        error = ?e,
                        sidecar = %sidecar.display(),
                        "failed to remove sidecar autosave after save"
                    ),
                }
                // この session で既に modal 候補に入っていた場合も除く。
                self.recovery_candidates.retain(|p| p != &sidecar);
                if self.recovery_candidates.is_empty() {
                    self.show_recovery_modal = false;
                }
                // 「最近開いたファイル」 にも入れる (= save した file は次回
                // 開きたい候補なので、 user 期待としては自然)。 さらに
                // 「最近保存したファイル」 別 list にも記録する。
                self.push_recent(path.to_path_buf());
                self.push_recent_saved(path.to_path_buf());
                // PR6: migration で audio_sources の path が
                // `Absolute(import_cache)` → `ProjectRelative(samples/)` に書き換わり、
                // project_dir も新たに確定した。 live song と project_dir を audio engine
                // へ再送して `AudioClipRenderer` を rebuild させる (順序保証 IPC なので
                // SetProjectDir → LoadSong)。 live (snapshot ではない) を送るのは、 audio
                // が反映すべきは再生対象の working state だから。
                let project_dir: Option<PathBuf> = path.parent().map(Path::to_path_buf);
                self.send_audio(MainToChild::SetProjectDir(project_dir));
                let song = self.song.clone();
                self.send_audio(MainToChild::LoadSong(song));
                // 「保存して終了」: この保存は成功した。 plugin state 待ちの間に live へ
                // 編集が入って dirty なら (co-temporal snapshot は編集前で凍結されている
                // ため、 その編集はこの保存に含まれない)、 残りを確定するため同じ path へ
                // 再保存して終了意図を維持する。 clean なら終了する。 save 成功が分かる
                // この場所で判定するので、 失敗時の無限再保存ループに陥らない。
                if self.quit_after_save {
                    if self.is_dirty {
                        self.begin_save(path);
                    } else {
                        self.should_quit = true;
                        self.quit_after_save = false;
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = ?e, path = %path.display(), "failed to save project");
                self.status_message = format!("保存に失敗しました: {e}");
                // 保存失敗 → 終了しない (データ損失回避)。 終了意図はクリアして、
                // state 待ちのたびに再保存が走り続ける無限ループを防ぐ。
                self.quit_after_save = false;
            }
        }
    }

    // -------- Playback -----------------------------------------------------

    fn play(&mut self) {
        // export 中は再生を禁止する。音声 freewheel フェーズの realtime play は
        // offline render と競合し、書き出される音声を壊しうる（映像フェーズは
        // 独立だが、 混乱を避けて export 全体で一律に止める）。
        if self.pending_video_export.is_some() || self.export_progress.is_some() {
            self.status_message = "Video export 中は再生できません".into();
            return;
        }
        // FIXME #24: プロジェクトロードの asset decode 中は音声がまだ揃って
        // いないので再生を gate して queue する (load 完了で on_asset_decode_tick
        // が flush)。
        if self.asset_decode.is_some() {
            self.pending_play = true;
            self.status_message = "プロジェクト読込中...".into();
            return;
        }
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
        // play() で LoadSong を再送しない (= 旧バグ: 大量 WAV のとき
        // audio engine の compile_audio_schedule = decode + schedule
        // build が同期で 2 秒以上かかり再生開始が遅延)。 song の変更は
        // 既に sync_song_to_plugin_host 経由で audio engine に届いている
        // 前提 (= IPC 順序保証)。
        // Pro Tools 流の「Stop で開始位置に戻る」 用に、 実際の再生
        // 開始時の playhead を保存。 ruler クリック等で playhead を
        // 移動してから play した場合は、 その位置が origin になる。
        self.playback_origin_beat = Some(self.playhead_beat.unwrap_or(0.0));
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
        // Pro Tools 流: 停止時に playhead を「再生開始位置」 (= 直前の
        // play() 呼び出し時点の playhead) に戻す。 GUI 側 playhead_beat
        // の即時上書きと、 audio engine への SeekTo IPC を 1 セットで
        // 実行する。 後者を送らないと on_tick が直近サンプル位置を返し
        // て GUI 側の戻し操作を打ち消す。 origin が None (= まだ一度も
        // play していない) なら playhead は触らない。
        if let Some(origin) = self.playback_origin_beat.take() {
            self.playhead_beat = Some(origin);
            let sr = common::audio_bridge::SAMPLE_RATE as f64;
            let bpm = self.song.bpm.max(1.0) as f64;
            let samples = (origin as f64 * 60.0 / bpm * sr).max(0.0) as u64;
            self.send_audio(MainToChild::SeekTo { samples });
        }
        // Phase 4 Step C: recording session を transport stop でクローズ。
        // Latch / Write の latched set + per-param 直近 record 位置を全て
        // clear。 これで次の Play 時には latched / last_beat が空からスタート、
        // touching しない limit 何も record されない (Touch / Latch / Write 共通)。
        self.latched_param_gestures.clear();
        self.recording_last_beat.clear();
        // Phase 4 Step C-2: audio thread の recording bypass を解除 +
        // 最新 song を送る (= curve eval に戻る瞬間に正しい point sequence
        // が反映される)。 currently_recording_lanes は !is_playing なので
        // 必ず empty に解決する。
        self.sync_recording_lanes_with_audio();
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

    /// `R` キー: 選択中 clip(s) の bounding range (= 最小 `start_beat` 〜
    /// 最大 `start_beat + length_beats`) を loop 範囲に設定し、 loop ON +
    /// 再生開始。 既に loop ON かつ現在の loop 範囲が同じ bounding range
    /// と一致するなら loop を OFF にする (再生は維持)。
    ///
    /// 「選択 clip」 は `selected_clips` を優先し、空なら `selected_clip`
    /// (単数 fallback) を使う。 両方とも空 / 全 ref が無効なら no-op。
    fn loop_selected_clip_toggle(&mut self) {
        let Some((start, end)) = self.selected_clips_range() else {
            return;
        };

        const EPS: f64 = 1e-9;
        let same_range = (self.song.loop_start_beat - start).abs() < EPS
            && (self.song.loop_end_beat - end).abs() < EPS;

        if self.is_looping && same_range {
            self.is_looping = false;
            self.send_audio(MainToChild::SetLoop(false));
            return;
        }

        self.set_loop_range(start, end);
        if !self.is_looping {
            self.is_looping = true;
            self.send_audio(MainToChild::SetLoop(true));
        }
        if !self.is_playing {
            self.play();
        }
    }

    /// 選択中 clip 群の bounding beat range を返す。 `selected_clips` を
    /// 優先し、空なら `selected_clip` を 1 件として扱う。 全 ref が
    /// 無効 (track / clip が見つからない) or 長さ 0 の場合は `None`。
    fn selected_clips_range(&self) -> Option<(f64, f64)> {
        let refs: Vec<ClipRef> = if !self.selected_clips.is_empty() {
            self.selected_clip_refs()
        } else if let Some(r) = self.selected_clip_ref() {
            vec![r]
        } else {
            return None;
        };

        let mut min_start = f64::INFINITY;
        let mut max_end = f64::NEG_INFINITY;
        for r in &refs {
            let Some(track) = self.song.tracks.get(r.track as usize) else {
                continue;
            };
            let Some(clip) = track.clips.get(r.clip as usize) else {
                continue;
            };
            let s = clip.start_beat;
            let e = clip.start_beat + clip.length_beats;
            if s < min_start {
                min_start = s;
            }
            if e > max_end {
                max_end = e;
            }
        }

        (min_start.is_finite() && max_end > min_start).then_some((min_start, max_end))
    }

    // -------- Track operations ---------------------------------------------

    /// `AppEvent::DeleteTrack` の dispatcher。 plugin が song に居る
    /// 場合は `RequestAllStates` を投げて、 受信時に最新 plugin state
    /// を Song に書き込んでから [`Self::push_undo_snapshot`] + 削除を
    /// 実行する。 これで「knob を回した状態で track 削除 → Undo」 で
    /// knob 値が復元される。 plugin 無しの song は即時実行 (= state を
    /// 取りに行く相手が居ない)。
    fn delete_track(&mut self, idx: u32) {
        let Some(track_id) = self.song.tracks.get(idx as usize).map(|t| t.id) else {
            return;
        };
        if !self.song_has_plugin() {
            self.push_undo_snapshot();
            self.delete_track_inner(track_id);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(
            DeferredEdit::DeleteTrack { track_id },
        ));
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

        // selected_clip / selected_clips は stable ClipKey 保持なので、 残った
        // track の index shift には自動追従する (再マッピング不要)。 ただし
        // 削除された track を指す key は解決不能になるので、 set / anchor 双方
        // から落とす (after_undo_redo / action_remove_last_track と同方針)。
        let mut keys = std::mem::take(&mut self.selected_clips);
        keys.retain(|k| self.clip_at(*k).is_some());
        self.selected_clips = keys;
        if let Some(k) = self.selected_clip
            && self.clip_at(k).is_none()
        {
            self.selected_clip = None;
            self.selected_notes.clear();
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
        // selected_clip / selected_clips は stable ClipKey 保持なので、 track の
        // index swap には自動追従する (id 不変、 再マッピング不要)。 旧 index
        // ベース実装は selected_clips を取りこぼすバグがあったが、 これで解消。
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
        // selected_clips / selected_clip は stable ClipKey 保持なので reorder
        // (track の index 変化) に自動追従する。 旧実装の id ラウンドトリップ
        // (抽出 → 並べ替え → index 逆引き) は不要になった。

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
        // selected_clips / selected_clip は stable ClipKey 保持のため再構築不要。

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

    fn begin_rename_track(&mut self, track_id: u32) {
        let Some(name) = self
            .song
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .map(|t| t.name.clone())
        else {
            return;
        };
        self.track_rename_text = name;
        self.track_rename_id = Some(track_id);
    }

    fn commit_rename_track(&mut self) {
        let Some(track_id) = self.track_rename_id else {
            return;
        };
        self.track_rename_id = None;
        let new_name = self.track_rename_text.trim().to_string();
        self.track_rename_text.clear();
        if new_name.is_empty() {
            return;
        }
        if let Some(track) = self.song.tracks.iter_mut().find(|t| t.id == track_id) {
            track.name = new_name;
        }
        self.sync_song_to_plugin_host();
    }

    fn begin_rename_clip(&mut self, target: ClipRef) {
        let Some(content_id) = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        // 表示されている名前 (= clip_display_label と同じ) を編集開始値にする。
        // Text clip は本文 (= first TextEvent.text) を、 それ以外は content_name を pre-fill。
        self.clip_rename_text = self
            .song
            .clip_contents
            .get(&content_id)
            .and_then(|c| c.text_events())
            .and_then(|events| events.first())
            .map(|ev| ev.text.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.song.content_name(content_id).to_string());
        self.clip_rename = Some(target);
    }

    /// clip rename を確定。 trim 後空文字なら無変更 (track rename と同じ)。
    /// clip 名は表示専用 (audio / plugin processing に無関係) なので
    /// `sync_song_to_plugin_host` は呼ばない。 song の変更は autosave /
    /// undo snapshot (`is_undoable`) に乗る。 名前は `content_id` 単位の
    /// SSoT (`Song.clip_content_names`) に書くので、 同 content を共有する
    /// linked clip 全部が同時に rename される。
    fn commit_rename_clip(&mut self) {
        let Some(target) = self.clip_rename else {
            return;
        };
        self.clip_rename = None;
        let new_name = self.clip_rename_text.trim().to_string();
        self.clip_rename_text.clear();
        if new_name.is_empty() {
            return;
        }
        let Some(content_id) = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        // Text clip は本文 (= 全 TextEvent.text) に書く。 表示名 (clip_display_label)
        // は content-first で本文を優先するので、 content_name を書いても見えない。
        // set_clip_text_event_content が全 event 書換え + edit buffer resync + is_dirty を
        // 行う (inspector の content 編集と同経路)。 非 Text clip は従来どおり content_name。
        if matches!(
            self.song.clip_contents.get(&content_id),
            Some(common::model::ClipContent::Text(_))
        ) {
            self.set_clip_text_event_content(target, new_name);
        } else {
            self.song.set_content_name(content_id, new_name);
            // content_name 経路は単独で is_dirty を立てないため明示的に arm する
            // (push_undo_snapshot は is_undoable 経由で済むが dirty は立てない)。
            self.is_dirty = true;
        }
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

    /// PR-V3: track.source = Vocal で instrument に builtin VOICEVOX が
    /// load されている全 track の clip notes を `NoteMetadata` 配列に
    /// 変換し、 plugin host に `SetBuiltinPluginNoteMetadata` で送る。
    /// plugin_id 未確定 (= load 完了通知前) の track はスキップ、
    /// `SlotPluginLoadedFromChild` 受信時に再呼び出しされる。
    ///
    /// PR-V4 follow-up: vocal track が 1 つでも存在するなら VOICEVOX
    /// engine を lazy spawn する。 旧 `begin_vocal_synth` 内にあった
    /// 起動 logic を移植 (= localhost:50021 が起動済でなければ自動で
    /// spawn、 builtin plugin の HTTP synth を成功させる前提)。
    pub fn sync_vocal_metadata(&mut self) {
        let bpm = self.song.bpm;
        let has_vocal_track = self.song.tracks.iter().any(|t| {
            matches!(t.source, common::model::InstrumentSource::Vocal { .. })
        });
        if has_vocal_track {
            self.ensure_voicevox_engine();
        }
        for track in &self.song.tracks {
            if !matches!(track.source, common::model::InstrumentSource::Vocal { .. }) {
                continue;
            }
            // builtin VOICEVOX を instrument に持っているか?
            let Some(inst) = track.instrument.as_ref() else {
                continue;
            };
            if inst.format != PluginFormat::Builtin
                || inst.plugin_id != common::plugin_db::BUILTIN_ID_VOICEVOX
            {
                continue;
            }
            // plugin_id (= u32 host-side id) を loaded_slots から引く。
            let Some(slot_info) = self
                .loaded_slots
                .get(&(track.id, PluginSlot::Instrument))
            else {
                continue;
            };
            let host_plugin_id = slot_info.plugin_id;

            // 全 clip の notes を NoteMetadata 配列に flatten。 note_id は
            // (clip-internal index) を「track 内通し番号」 にしないと衝突
            // する可能性があるので、 ここでは「全 clip 連結 index」 を使う
            // (= clip 1 の note 数 + clip 2 の note index)。 PR-V2.4 で
            // 改めて clip 単位にする予定。
            let mut entries: Vec<common::plugin_metadata::NoteMetadata> = Vec::new();
            for clip in &track.clips {
                let notes: &[common::model::Note] = self
                    .song
                    .clip_contents
                    .get(&clip.content_id)
                    .and_then(|c| c.notes())
                    .unwrap_or(&[]);
                for n in notes {
                    let note_id = entries.len() as u32;
                    entries.push(common::plugin_metadata::NoteMetadata {
                        note_id,
                        // clip-relative beats を song-absolute に変換 (=
                        // VOICEVOX synth wrapper が earliest を引いて
                        // 0 起点にする、 clip 境界跨ぎでも一貫)。
                        start_beat: clip.start_beat + n.start_beat,
                        duration_beats: n.duration_beats,
                        pitch: n.pitch,
                        velocity: n.velocity,
                        lyric: n.lyric.clone().unwrap_or_default(),
                    });
                }
            }
            self.send_plugin(MainToChild::SetBuiltinPluginNoteMetadata {
                plugin_id: host_plugin_id,
                bpm,
                entries,
            });
        }
    }

    /// 口パク (lip-sync) を再生成する (docs/plan_pakupaku.md §7)。`vocal_track_id`
    /// の各 clip の notes を snapshot し、背景スレッドで `query_phonemes`
    /// (`sing_frame_audio_query` のみ) を叩いて結果を `AppEvent::LipsyncGenerated`
    /// で main thread へ返す。binding (`lipsync_target_track`) 未設定 / 口 track の
    /// `mouth_map` 未設定 / notes を持つ clip 無し のときは no-op。歌唱のみ (Q6)。
    pub fn regenerate_lipsync_for_track(&mut self, vocal_track_id: u32) {
        let Some(vocal) = self.song.tracks.iter().find(|t| t.id == vocal_track_id) else {
            return;
        };
        let Some(target_id) = vocal.lipsync_target_track else {
            return;
        };
        // 口 track が存在し mouth_map が設定済みか (= 生成する意味があるか)。
        let configured = self.song.tracks.iter().any(|t| {
            t.id == target_id && t.mouth_map.as_ref().is_some_and(|m| m.is_configured())
        });
        if !configured {
            return;
        }
        let bpm = self.song.bpm;
        // notes を持つ各 vocal clip を snapshot (配置情報 + notes の owned copy)。
        let mut snaps: Vec<(f64, f64, f64, Vec<common::model::Note>)> = Vec::new();
        for clip in &vocal.clips {
            let Some(notes) = self
                .song
                .clip_contents
                .get(&clip.content_id)
                .and_then(|c| c.notes())
            else {
                continue;
            };
            if notes.is_empty() {
                continue;
            }
            let first_note_local_beat = notes
                .iter()
                .map(|n| n.start_beat)
                .fold(f64::INFINITY, f64::min);
            snaps.push((
                clip.start_beat,
                clip.length_beats,
                first_note_local_beat,
                notes.to_vec(),
            ));
        }
        if snaps.is_empty() {
            return;
        }
        self.ensure_voicevox_engine();
        let proxy = self.event_proxy.clone();
        std::thread::spawn(move || {
            let mut clips = Vec::with_capacity(snaps.len());
            for (clip_start_beat, clip_len_beats, first_note_local_beat, notes) in snaps {
                match common::voicevox::query_phonemes(&notes, bpm) {
                    Ok(phonemes) => clips.push(LipsyncClipResult {
                        clip_start_beat,
                        clip_len_beats,
                        first_note_local_beat,
                        phonemes,
                    }),
                    Err(e) => {
                        tracing::warn!(error = ?e, vocal_track_id, "lip-sync phoneme query failed");
                    }
                }
            }
            // 全 clip が失敗したら既存 clip を温存 (transient な engine 落ち対策)。
            if !clips.is_empty() {
                proxy.send(AppEvent::LipsyncGenerated {
                    vocal_track_id,
                    bpm,
                    clips,
                });
            }
        });
    }

    /// `AppEvent::LipsyncGenerated` handler。口 track の自動生成 clip
    /// (`auto_lipsync == true`) を全て差し替える。派生データなので Undo
    /// snapshot は積まない (user 編集側が Undo 対象、手編集は保持しない = Q8)。
    fn apply_lipsync_generated(
        &mut self,
        vocal_track_id: u32,
        bpm: f32,
        results: Vec<LipsyncClipResult>,
    ) {
        // HTTP 中に song が変わっている可能性があるため id ベースで再解決。
        let Some(target_id) = self
            .song
            .tracks
            .iter()
            .find(|t| t.id == vocal_track_id)
            .and_then(|t| t.lipsync_target_track)
        else {
            return;
        };
        let Some(m_idx) = self.song.tracks.iter().position(|t| t.id == target_id) else {
            return;
        };
        let Some(mouth_map) = self.song.tracks[m_idx].mouth_map.clone() else {
            return;
        };
        // 既存の自動生成 clip を全削除 (手編集保持しない)。
        self.song.tracks[m_idx].clips.retain(|c| !c.auto_lipsync);
        // 各 vocal clip 分の口画像 clip を生成して口 track へ追加。
        let res = self.song.video_resolution;
        for r in &results {
            let mut events = common::lipsync::build_mouth_events(
                &r.phonemes,
                &mouth_map,
                bpm,
                r.first_note_local_beat,
                r.clip_len_beats,
            );
            if events.is_empty() {
                continue;
            }
            // build_mouth_events は rect を全画面 default で返すので、 そのままだと
            // 口画像が preview 全面に伸びて「大きすぎる / 他レイヤーとサイズが合わ
            // ない」。 通常の画像取り込み (action_import_image) と同じく、 各口画像の
            // 素材寸法から aspect-fit rect を計算して上書きし、 立ち絵の他の子レイヤー
            // (眉 / 目 等を同寸 PNG で取り込んだもの) と同じ収まりに揃える。
            for ev in &mut events {
                if let Some(src) = self.song.image_sources.get(&ev.source_id) {
                    let (x, y, w, h) = aspect_fit_pip_rect(res, (src.width, src.height));
                    ev.x = x;
                    ev.y = y;
                    ev.w = w;
                    ev.h = h;
                }
            }
            let content_id = self.song.alloc_content(
                common::model::ClipContent::Image(common::model::ImageContent { events }),
                "口パク".to_string(),
            );
            let m = &mut self.song.tracks[m_idx];
            let clip_id = m.alloc_clip_id();
            m.clips.push(Clip {
                id: clip_id,
                name: String::new(),
                start_beat: r.clip_start_beat,
                length_beats: r.clip_len_beats,
                content_id,
                notes: Vec::new(),
                color: None,
                auto_lipsync: true,
            });
        }
        // 削除した古い clip の content を回収。
        self.song.gc_clip_contents();
        self.is_dirty = true;
    }

    /// `SetLipsyncTarget` handler。vocal track の出力先 binding を更新し、
    /// 設定時は口パクを再生成する (snapshot は `is_undoable` 経由で handler
    /// 前に取得済み = binding 変更を undo 可能)。
    fn set_lipsync_target(&mut self, track_id: u32, target: Option<u32>) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        t.lipsync_target_track = target;
        self.is_dirty = true;
        if target.is_some() {
            self.regenerate_lipsync_for_track(track_id);
        }
    }

    /// `SetMouthMapSlot` handler。口 track の `mouth_map` の 1 slot を更新し、
    /// この口 track を出力先にしている vocal track の口パクを再生成する。
    fn set_mouth_map_slot(
        &mut self,
        track_id: u32,
        shape: common::model::MouthShape,
        source_id: common::model::ImageSourceId,
    ) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        let map = t
            .mouth_map
            .get_or_insert_with(common::model::MouthMap::default);
        match shape {
            common::model::MouthShape::A => map.a = source_id,
            common::model::MouthShape::I => map.i = source_id,
            common::model::MouthShape::U => map.u = source_id,
            common::model::MouthShape::E => map.e = source_id,
            common::model::MouthShape::O => map.o = source_id,
            common::model::MouthShape::N => map.n = source_id,
            common::model::MouthShape::Closed => map.closed = source_id,
        }
        self.is_dirty = true;
        // この口 track を出力先にしている vocal track を再生成する。
        let vocal_ids: Vec<u32> = self
            .song
            .tracks
            .iter()
            .filter(|v| v.lipsync_target_track == Some(track_id))
            .map(|v| v.id)
            .collect();
        for vid in vocal_ids {
            self.regenerate_lipsync_for_track(vid);
        }
    }

    /// song 変更時に呼ぶ (= `sync_song_to_plugin_host` から)。binding を持つ
    /// vocal track があれば debounce timer を立て、quiet period (400ms) 後に
    /// `LipsyncDebounceFired` を送る。rapid 編集 (歌詞タイプ等) は世代カウンタで
    /// coalesce され、最後の 1 回だけ再生成される。
    fn mark_lipsync_dirty(&mut self) {
        if !self
            .song
            .tracks
            .iter()
            .any(|t| t.lipsync_target_track.is_some())
        {
            return;
        }
        self.lipsync_gen = self.lipsync_gen.wrapping_add(1);
        let generation = self.lipsync_gen;
        let proxy = self.event_proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            proxy.send(AppEvent::LipsyncDebounceFired(generation));
        });
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
        // FIXME #30: 挿入位置は「選択中で最下段の track の直後」 (純ロジックは
        // add_track_insert_index)。 選択が無いときだけ従来どおり末尾。
        let insert_at = add_track_insert_index(&self.song.tracks, &self.selected_track_ids);
        self.song.tracks.insert(insert_at, track);
        // 追加直後はこの新 track を唯一の選択 + カーソルにする (次の操作の対象)。
        self.selected_track_ids = vec![id];
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(insert_at, "added instrument track");
    }

    // ----------------------------------------------------------------
    // gui_01 #028 (M14 Phase 63n-2) — automation lane / point handlers
    // ----------------------------------------------------------------

    fn set_lane_enabled(&mut self, track_id: u32, lane_id: u32, enabled: bool) {
        if let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) {
            lane.enabled = enabled;
            self.sync_song_to_plugin_host();
        }
    }

    fn set_lane_visible(&mut self, track_id: u32, lane_id: u32, visible: bool) {
        if let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) {
            lane.visible = visible;
            // visible は再生に影響しないが、Song 構造の変化なので同期。
            self.sync_song_to_plugin_host();
        }
    }

    /// Lane header default slider drag (release / live preview)。
    /// `next_norm` は normalized 0..=1、target に応じて plain 単位に
    /// 逆変換してから格納する。同時に last-touched param も更新する
    /// (lane default knob を回した後 `A` を押すと同 lane が visible
    /// 復活する閉ループ)。
    fn set_lane_default(&mut self, track_id: u32, lane_id: u32, next_norm: f32) {
        let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) else {
            return;
        };
        let target = lane.target.clone();
        lane.default_value = common::automation::norm_to_plain(&target, next_norm);
        let display_name = automation_target_display_name(&target);
        self.last_touched_param = Some(TouchedParam {
            track_id,
            target,
            display_name,
            touched_at: std::time::Instant::now(),
        });
        self.sync_song_to_plugin_host();
    }

    /// gui_01 #030 (M14 Phase 63n-5): lane 高さ drag。`next_px` は
    /// widget 側で min/max に clamp 済なのでそのまま反映。
    fn set_lane_height(&mut self, track_id: u32, lane_id: u32, next_px: u16) {
        if let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) {
            lane.height_px = next_px;
            // 高さは描画状態のみで再生に影響しないが、 Song 構造の
            // 変化なので同期 (= 他 process が song を読むときに矛盾
            // しないよう)。
            self.sync_song_to_plugin_host();
        }
    }

    fn delete_lane(&mut self, track_id: u32, lane_id: u32) {
        // gui_01 #034 (Phase 63n-10): master row sentinel 対応。 song_lanes
        // の方にあれば該当 idx を探して remove、 通常 track なら従来通り。
        if track_id == common::model::MASTER_TRACK_ID {
            if let Some(idx) = self.song.song_lanes.iter().position(|l| l.id == lane_id) {
                self.song.song_lanes.remove(idx);
                self.sync_song_to_plugin_host();
            }
        } else if let Some(track) = self.song.track_by_id_mut(track_id)
            && let Some(idx) = track.lane_index_by_id(lane_id)
        {
            track.automation_lanes.remove(idx);
            // 共有先のなくなった clip_contents は次の save / GC で
            // 自動回収。
            self.sync_song_to_plugin_host();
        }
    }

    /// dblclick on lane body → 1 point 追加。clip-local `time_beat`
    /// 昇順を保つよう挿入位置を二分探索で決める。
    fn add_automation_point(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        time_beat: f64,
        value_norm: f32,
    ) {
        let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) else {
            return;
        };
        let target = lane.target.clone();
        let Some(clip) = lane.clip_by_id(clip_id) else {
            return;
        };
        let content_id = clip.content_id;
        let plain = common::automation::norm_to_plain(&target, value_norm);
        let entry = self
            .song
            .clip_contents
            .entry(content_id)
            .or_insert_with(|| {
                common::model::ClipContent::Automation(
                    common::model::AutomationContent::default(),
                )
            });
        let points = match entry {
            common::model::ClipContent::Automation(a) => &mut a.points,
            _ => {
                tracing::warn!(
                    content_id,
                    "AddAutomationPoint: content variant is not Automation, skipping"
                );
                return;
            }
        };
        let new_point = common::model::AutomationPoint {
            time_beat,
            value: plain,
            curve: common::model::AutomationCurve::Linear,
        };
        let insert_at = points.partition_point(|p| p.time_beat <= time_beat);
        points.insert(insert_at, new_point);
        self.sync_song_to_plugin_host();
    }

    fn move_automation_points(&mut self, deltas: &[MoveAutomationPointEntry]) {
        if deltas.is_empty() {
            return;
        }
        // 各 delta の lane.target を引いて plain 化、同 clip 内の point
        // を一括更新後に sort で昇順を保つ。同一 clip 複数 point は
        // group して 1 度の sort で済ませる。
        let mut touched: std::collections::HashSet<common::model::ContentId> =
            std::collections::HashSet::new();
        for delta in deltas {
            let Some(lane) = self
                .song
                .automation_lane_by_key_mut(delta.key.track_id, delta.key.lane_id)
            else {
                continue;
            };
            let target = lane.target.clone();
            let Some(clip) = lane.clip_by_id(delta.key.clip_id) else {
                continue;
            };
            let content_id = clip.content_id;
            let plain = common::automation::norm_to_plain(&target, delta.next_value_norm);
            let Some(entry) = self.song.clip_contents.get_mut(&content_id) else {
                continue;
            };
            let common::model::ClipContent::Automation(a) = entry else {
                continue;
            };
            if let Some(p) = a.points.get_mut(delta.key.point_idx as usize) {
                p.time_beat = delta.next_time_beat;
                p.value = plain;
                touched.insert(content_id);
            }
        }
        for cid in touched {
            if let Some(common::model::ClipContent::Automation(a)) =
                self.song.clip_contents.get_mut(&cid)
            {
                a.points.sort_by(|p1, p2| {
                    p1.time_beat
                        .partial_cmp(&p2.time_beat)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
        }
        self.sync_song_to_plugin_host();
    }

    fn delete_automation_points(&mut self, points: &[AutomationPointKeyRef]) {
        if points.is_empty() {
            return;
        }
        // 同じ content_id でまとめて、index 降順で削除 (前から消すと
        // 後の index がずれるため)。
        let mut by_content: std::collections::HashMap<
            common::model::ContentId,
            Vec<u32>,
        > = std::collections::HashMap::new();
        for k in points {
            let Some(lane) = self.song.automation_lane_by_key(k.track_id, k.lane_id) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip_id) else {
                continue;
            };
            by_content.entry(clip.content_id).or_default().push(k.point_idx);
        }
        for (cid, mut indices) in by_content {
            indices.sort_unstable_by(|a, b| b.cmp(a));
            indices.dedup();
            if let Some(common::model::ClipContent::Automation(a)) =
                self.song.clip_contents.get_mut(&cid)
            {
                for idx in indices {
                    if (idx as usize) < a.points.len() {
                        a.points.remove(idx as usize);
                    }
                }
            }
        }
        self.sync_song_to_plugin_host();
    }

    fn set_automation_curve_type(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        next: common::model::AutomationCurve,
    ) {
        let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) else {
            return;
        };
        let Some(clip) = lane.clip_by_id(clip_id) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Automation(a)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        if let Some(p) = a.points.get_mut(point_idx as usize) {
            p.curve = next;
            self.sync_song_to_plugin_host();
        }
    }

    /// gui_01 #033 Phase 63n-9: Bezier curve handle drag release で 1 件
    /// 発火される `SetAutomationCurveBezierTension` の handler。 既存
    /// curve type が `Bezier` でない場合は no-op (= race / 仕様外発火)。
    /// `next` は widget で `-1.0..=1.0` clamp 済だが、 defensive で再 clamp。
    fn set_automation_curve_bezier_tension(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        next: f32,
    ) {
        let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) else {
            return;
        };
        let Some(clip) = lane.clip_by_id(clip_id) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Automation(a)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        if let Some(p) = a.points.get_mut(point_idx as usize)
            && matches!(p.curve, common::model::AutomationCurve::Bezier { .. })
        {
            p.curve = common::model::AutomationCurve::Bezier {
                tension: next.clamp(-1.0, 1.0),
            };
            self.sync_song_to_plugin_host();
        }
    }

    /// gui_01 #033 Phase 63n-9: Exponential curve handle drag release で
    /// 発火される `SetAutomationCurveExponentialBend` の handler。 既存
    /// curve type が `Exponential` でない場合は no-op。
    fn set_automation_curve_exponential_bend(
        &mut self,
        track_id: u32,
        lane_id: u32,
        clip_id: u32,
        point_idx: u32,
        next: f32,
    ) {
        let Some(lane) = self.song.automation_lane_by_key_mut(track_id, lane_id) else {
            return;
        };
        let Some(clip) = lane.clip_by_id(clip_id) else {
            return;
        };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Automation(a)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return;
        };
        if let Some(p) = a.points.get_mut(point_idx as usize)
            && matches!(p.curve, common::model::AutomationCurve::Exponential { .. })
        {
            p.curve = common::model::AutomationCurve::Exponential {
                bend: next.clamp(-1.0, 1.0),
            };
            self.sync_song_to_plugin_host();
        }
    }

    /// Phase 3: `selected_automation_points` を grid (`1/div` beat) に snap。
    /// piano roll の [`Self::quantize_selected_notes`] と同 idiom。 sort
    /// invariant を維持するため snap 後に各 clip 内 point 列を sort し直し、
    /// `selected_automation_points` も新 idx で再構築する。 selection 再
    /// 構築は `(snapped_time, value)` で lookup する (point に stable id
    /// が無いので、 同 frame 内の値ペアで identify)。 同 clip 内に snap
    /// 結果が同位置になる point が複数いれば最初の一致を採用。
    fn quantize_selected_automation_points(&mut self, div: u8) {
        if self.selected_automation_points.is_empty() {
            return;
        }
        let div = div.max(1) as f64;
        let snap = |b: f64| ((b * div).round() / div).max(0.0);
        let selected = self.selected_automation_points.clone();

        // `content_id` ごとに、 quantize 対象 idx 群と、 selection lookup 用の
        // `(snapped_time, value)` ペア群を集める。 ペアは selection の現順序
        // を維持するため Vec で持つ。
        #[derive(Clone, Copy)]
        struct Owner {
            track_id: u32,
            lane_id: u32,
            clip_id: u32,
        }
        struct ContentBuckets {
            owner: Owner,
            idxs: Vec<u32>,
            lookups: Vec<(f64, f64)>,
        }
        let mut by_content: std::collections::HashMap<
            common::model::ContentId,
            ContentBuckets,
        > = std::collections::HashMap::new();
        for k in &selected {
            let Some(lane) = self.song.automation_lane_by_key(k.track_id, k.lane_id) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip_id) else {
                continue;
            };
            let content_id = clip.content_id;
            let Some(common::model::ClipContent::Automation(a)) =
                self.song.clip_contents.get(&content_id)
            else {
                continue;
            };
            let Some(p) = a.points.get(k.point_idx as usize) else {
                continue;
            };
            let entry = by_content.entry(content_id).or_insert_with(|| ContentBuckets {
                owner: Owner {
                    track_id: k.track_id,
                    lane_id: k.lane_id,
                    clip_id: k.clip_id,
                },
                idxs: Vec::new(),
                lookups: Vec::new(),
            });
            entry.idxs.push(k.point_idx);
            entry.lookups.push((snap(p.time_beat), p.value));
        }

        let mut new_selection: Vec<AutomationPointKeyRef> = Vec::with_capacity(selected.len());
        for (content_id, bucket) in by_content {
            let ContentBuckets {
                owner,
                idxs,
                lookups,
            } = bucket;
            let Some(common::model::ClipContent::Automation(a)) =
                self.song.clip_contents.get_mut(&content_id)
            else {
                continue;
            };
            // snap 対象 point の time_beat を書き換え。 重複 idx は HashSet
            // で除去せず、 set_mut が冪等なのでそのまま再代入。
            for idx in &idxs {
                if let Some(p) = a.points.get_mut(*idx as usize) {
                    p.time_beat = snap(p.time_beat);
                }
            }
            a.points.sort_by(|p1, p2| {
                p1.time_beat
                    .partial_cmp(&p2.time_beat)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            // 新 idx を `(snapped_time, value)` で lookup。
            for (st, sv) in &lookups {
                if let Some(new_idx) = a.points.iter().position(|p| {
                    (p.time_beat - st).abs() < 1e-9 && (p.value - sv).abs() < 1e-9
                }) {
                    new_selection.push(AutomationPointKeyRef {
                        track_id: owner.track_id,
                        lane_id: owner.lane_id,
                        clip_id: owner.clip_id,
                        point_idx: new_idx as u32,
                    });
                }
            }
        }

        self.selected_automation_points = new_selection;
        self.sync_song_to_plugin_host();
    }

    /// Phase 3: 選択中 automation point を JSON 化して OS clipboard に
    /// 出せるよう text を返す。 [`Self::copy_selected_notes_as_json`] と同
    /// idiom。 point の `value` は target ごとに値域が違う (Volume:
    /// 0..=2.0、 Pan: -1..=1 等) ので、 lane の `target` を引いて
    /// **normalized 0..=1** で serialize する。 paste 側でも target を
    /// 引いて plain に戻す (= target が違う lane に貼っても curve の
    /// shape を保てる、 Bitwig 流)。
    ///
    /// 戻り値は `(json, count)`。 何も copy できない (選択無し / lookup
    /// 失敗) 場合は `None`。
    pub fn copy_selected_automation_points_as_json(&self) -> Option<(String, usize)> {
        if self.selected_automation_points.is_empty() {
            return None;
        }
        #[derive(serde::Serialize, serde::Deserialize)]
        struct CopiedPoint {
            time_beat: f64,
            value_norm: f32,
            curve: common::model::AutomationCurve,
        }
        let mut copied: Vec<CopiedPoint> = Vec::with_capacity(self.selected_automation_points.len());
        for k in &self.selected_automation_points {
            let Some(lane) = self.song.automation_lane_by_key(k.track_id, k.lane_id) else {
                continue;
            };
            let Some(clip) = lane.clip_by_id(k.clip_id) else {
                continue;
            };
            let Some(common::model::ClipContent::Automation(a)) =
                self.song.clip_contents.get(&clip.content_id)
            else {
                continue;
            };
            let Some(p) = a.points.get(k.point_idx as usize) else {
                continue;
            };
            let value_norm = common::automation::plain_to_norm(&lane.target, p.value);
            copied.push(CopiedPoint {
                time_beat: p.time_beat,
                value_norm,
                curve: p.curve,
            });
        }
        if copied.is_empty() {
            return None;
        }
        // earliest time_beat を anchor として 0.0 にシフト (Note と同じ)。
        let earliest = copied
            .iter()
            .map(|p| p.time_beat)
            .fold(f64::INFINITY, f64::min);
        if earliest.is_finite() {
            for p in &mut copied {
                p.time_beat -= earliest;
            }
        }
        let count = copied.len();
        let json = serde_json::to_string(&copied).ok()?;
        Some((json, count))
    }

    /// Phase 3: clipboard 文字列を `Vec<CopiedPoint>` として deserialize
    /// し、 現在「paste 先 clip」 に挿入する。 paste 先の決定順:
    ///
    /// 1. `selected_automation_clips` が単一なら、 その clip
    /// 2. それ以外で `selected_automation_points` が非空なら、 最初の point
    ///    が指す clip
    /// 3. それ以外なら status_message で通知して no-op
    ///
    /// anchor: playhead が paste 先 clip の範囲内なら playhead 相対位置、
    /// それ以外は clip 先頭 (= 0.0)。 各 point は target に応じて norm →
    /// plain 変換して `insert_at = partition_point` で sort 維持 insert。
    /// 完了後、 新規挿入 point の idx を `selected_automation_points` に
    /// 上書き (= 直後に Delete / Move 等が selection 経由で効くように)。
    pub fn paste_automation_points_from_json(&mut self, json: &str) {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct CopiedPoint {
            time_beat: f64,
            value_norm: f32,
            curve: common::model::AutomationCurve,
        }
        let Ok(clipboard) = serde_json::from_str::<Vec<CopiedPoint>>(json) else {
            return;
        };
        if clipboard.is_empty() {
            return;
        }

        // paste 先 clip を決定。
        let dest_key: common::model::AutomationClipKey =
            if let [only] = self.selected_automation_clips.as_slice() {
                *only
            } else if let Some(first) = self.selected_automation_points.first() {
                common::model::AutomationClipKey {
                    track: first.track_id,
                    lane: first.lane_id,
                    clip: first.clip_id,
                }
            } else {
                self.status_message =
                    "貼り付け先の automation clip が選択されていません".to_string();
                return;
            };

        let Some(lane) = self.song.automation_lane_by_key(dest_key.track, dest_key.lane) else {
            return;
        };
        let target = lane.target.clone();
        let Some(clip) = lane.clip_by_id(dest_key.clip) else {
            return;
        };
        let content_id = clip.content_id;
        let clip_start = clip.start_beat;
        let clip_len = clip.length_beats;

        // anchor: playhead が clip 範囲内なら clip-local playhead、 それ以外
        // は clip 先頭。 playhead 未設定 (= 停止中で初期位置) なら 0.0。
        let anchor = self
            .playhead_beat
            .map(|ph| {
                let local = ph as f64 - clip_start;
                if local >= 0.0 && local < clip_len {
                    local
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);

        // dest content が既存で automation でない壊れたモデルなら、 undo を
        // 触る前に bail する (= no-op snapshot を積んで redo_stack を消さない)。
        if let Some(c) = self.song.clip_contents.get(&content_id)
            && !matches!(c, common::model::ClipContent::Automation(_))
        {
            self.status_message =
                "貼り付け先 clip が automation でない (型不整合)".to_string();
            return;
        }
        // dest clip / lane が解決でき automation と確認できたので、 実際に挿入
        // する直前で undo snapshot を積む (= Ctrl+Z で paste を取り消せる)。
        // これより上の早期 return では song を変更していないので snapshot 不要。
        self.push_undo_snapshot();

        let entry = self
            .song
            .clip_contents
            .entry(content_id)
            .or_insert_with(|| {
                common::model::ClipContent::Automation(
                    common::model::AutomationContent::default(),
                )
            });
        let points = match entry {
            common::model::ClipContent::Automation(a) => &mut a.points,
            _ => {
                self.status_message =
                    "貼り付け先 clip が automation でない (型不整合)".to_string();
                return;
            }
        };

        // 挿入後の新 idx は sort のたび変動するので、 全 point を挿入し
        // 終えてから「挿入した値ペア」 で再 lookup する。
        let mut inserted_pairs: Vec<(f64, f64)> = Vec::with_capacity(clipboard.len());
        let count = clipboard.len();
        for src in &clipboard {
            let plain = common::automation::norm_to_plain(&target, src.value_norm);
            let t = (src.time_beat + anchor).max(0.0);
            let new_point = common::model::AutomationPoint {
                time_beat: t,
                value: plain,
                curve: src.curve,
            };
            let insert_at = points.partition_point(|p| p.time_beat <= t);
            points.insert(insert_at, new_point);
            inserted_pairs.push((t, plain));
        }

        // 挿入後の idx を retrieve。 idiom は quantize 側と同じ。
        let new_indices: Vec<u32> = inserted_pairs
            .iter()
            .filter_map(|(t, v)| {
                points
                    .iter()
                    .position(|p| (p.time_beat - t).abs() < 1e-9 && (p.value - v).abs() < 1e-9)
                    .map(|i| i as u32)
            })
            .collect();

        self.selected_automation_points = new_indices
            .into_iter()
            .map(|i| AutomationPointKeyRef {
                track_id: dest_key.track,
                lane_id: dest_key.lane,
                clip_id: dest_key.clip,
                point_idx: i,
            })
            .collect();
        self.sync_song_to_plugin_host();
        self.status_message = format!("貼り付け: {count} オートメーションポイント");
    }

    /// 修飾なし drag release。source lane から取り出して target lane へ
    /// `start_beat` 昇順 insert。lane 跨ぎ可、target 不一致でも accept
    /// (curve は normalized なので意味温存、`docs/plan_automation.md`
    /// §5.4)。
    fn move_automation_clips(&mut self, deltas: &[MoveAutomationClipEntry]) {
        if deltas.is_empty() {
            return;
        }
        for d in deltas {
            let mut taken: Option<common::model::AutomationClip> = None;
            if let Some(source_lane) =
                self.song.automation_lane_by_key_mut(d.from.track, d.from.lane)
                && let Some(idx) = source_lane.clip_index_by_id(d.from.clip)
            {
                taken = Some(source_lane.clips.remove(idx));
            }
            let Some(mut clip) = taken else { continue };
            clip.start_beat = d.next_start_beat;
            if let Some(target_lane) = self
                .song
                .automation_lane_by_key_mut(d.to_lane.track, d.to_lane.lane)
            {
                let start = clip.start_beat;
                let pos = target_lane
                    .clips
                    .partition_point(|c| c.start_beat < start);
                target_lane.clips.insert(pos, clip);
            }
        }
        self.sync_song_to_plugin_host();
    }

    /// Ctrl+drag release。source は残置、同じ `ContentId` を持つ新 clip
    /// を `to_lane` に追加 (linked: curve を共有)。target が source と
    /// 同じ lane でも問題なく動く。
    fn clone_automation_clips_linked(&mut self, deltas: &[MoveAutomationClipEntry]) {
        if deltas.is_empty() {
            return;
        }
        for d in deltas {
            let template = {
                let Some(source_lane) =
                    self.song.automation_lane_by_key(d.from.track, d.from.lane)
                else {
                    continue;
                };
                let Some(source_clip) = source_lane.clip_by_id(d.from.clip) else {
                    continue;
                };
                (
                    source_clip.content_id,
                    source_clip.name.clone(),
                    source_clip.length_beats,
                )
            };
            let Some(target_lane) = self
                .song
                .automation_lane_by_key_mut(d.to_lane.track, d.to_lane.lane)
            else {
                continue;
            };
            let new_id = target_lane.alloc_clip_id();
            let new_clip = common::model::AutomationClip {
                id: new_id,
                name: template.1,
                start_beat: d.next_start_beat,
                length_beats: template.2,
                content_id: template.0,
            };
            let start = new_clip.start_beat;
            let pos = target_lane
                .clips
                .partition_point(|c| c.start_beat < start);
            target_lane.clips.insert(pos, new_clip);
        }
        self.sync_song_to_plugin_host();
    }

    /// Ctrl+Shift+drag release。source は残置、content を deep clone (新
    /// `ContentId` 採番) して独立 clip を追加。共有グループには入らない。
    fn clone_automation_clips_independent(
        &mut self,
        deltas: &[MoveAutomationClipEntry],
    ) {
        if deltas.is_empty() {
            return;
        }
        for d in deltas {
            let template = {
                let Some(source_lane) =
                    self.song.automation_lane_by_key(d.from.track, d.from.lane)
                else {
                    continue;
                };
                let Some(source_clip) = source_lane.clip_by_id(d.from.clip) else {
                    continue;
                };
                (
                    source_clip.content_id,
                    source_clip.name.clone(),
                    source_clip.length_beats,
                )
            };
            // Content を deep clone (`ClipContent` enum 全体の clone なので
            // Midi/Audio/Automation いずれも対応)。content が無い場合は空
            // Automation で作成。
            let cloned_content = self
                .song
                .clip_contents
                .get(&template.0)
                .cloned()
                .unwrap_or_else(|| {
                    common::model::ClipContent::Automation(
                        common::model::AutomationContent::default(),
                    )
                });
            let new_content_id = self.song.alloc_content_id();
            self.song.clip_contents.insert(new_content_id, cloned_content);
            let Some(target_lane) = self
                .song
                .automation_lane_by_key_mut(d.to_lane.track, d.to_lane.lane)
            else {
                continue;
            };
            let new_id = target_lane.alloc_clip_id();
            let new_clip = common::model::AutomationClip {
                id: new_id,
                name: template.1,
                start_beat: d.next_start_beat,
                length_beats: template.2,
                content_id: new_content_id,
            };
            let start = new_clip.start_beat;
            let pos = target_lane
                .clips
                .partition_point(|c| c.start_beat < start);
            target_lane.clips.insert(pos, new_clip);
        }
        self.sync_song_to_plugin_host();
    }

    /// 選択 automation clip 群の bounding span (= MIDI `clip_block_span` の lane
    /// 版)。 解決できない stale key は無視、 有効 clip が無ければ `None`。
    fn automation_block_span(&self, sources: &[common::model::AutomationClipKey]) -> Option<f64> {
        let mut min_start = f64::MAX;
        let mut max_end = f64::MIN;
        for &src in sources {
            let Some(clip) = self
                .song
                .automation_lane_by_key(src.track, src.lane)
                .and_then(|lane| lane.clip_by_id(src.clip))
            else {
                continue;
            };
            min_start = min_start.min(clip.start_beat);
            max_end = max_end.max(clip.start_beat + clip.length_beats);
        }
        (max_end >= min_start).then_some(max_end - min_start)
    }

    /// `source` の共有コピーを `new_start_beat` に 1 つ生成し新 key を返す
    /// (選択・sync は呼び出し側)。 `content_id` を流用し linked group に追加。
    fn duplicate_one_automation_clip_shared_at(
        &mut self,
        source: common::model::AutomationClipKey,
        new_start_beat: f64,
    ) -> Option<common::model::AutomationClipKey> {
        let (content_id, name, length) = {
            let lane = self.song.automation_lane_by_key(source.track, source.lane)?;
            let src_clip = lane.clip_by_id(source.clip)?;
            (src_clip.content_id, src_clip.name.clone(), src_clip.length_beats)
        };
        let lane = self
            .song
            .automation_lane_by_key_mut(source.track, source.lane)?;
        let new_id = lane.alloc_clip_id();
        let new_clip = common::model::AutomationClip {
            id: new_id,
            name,
            start_beat: new_start_beat,
            length_beats: length,
            content_id,
        };
        let pos = lane.clips.partition_point(|c| c.start_beat < new_start_beat);
        lane.clips.insert(pos, new_clip);
        Some(common::model::AutomationClipKey {
            track: source.track,
            lane: source.lane,
            clip: new_id,
        })
    }

    /// `source` の独立コピー (content deep clone + 新 ContentId) を
    /// `new_start_beat` に生成し新 key を返す。
    fn duplicate_one_automation_clip_unique_at(
        &mut self,
        source: common::model::AutomationClipKey,
        new_start_beat: f64,
    ) -> Option<common::model::AutomationClipKey> {
        let (src_content_id, name, length) = {
            let lane = self.song.automation_lane_by_key(source.track, source.lane)?;
            let src_clip = lane.clip_by_id(source.clip)?;
            (src_clip.content_id, src_clip.name.clone(), src_clip.length_beats)
        };
        let cloned_content = self
            .song
            .clip_contents
            .get(&src_content_id)
            .cloned()
            .unwrap_or_else(|| {
                common::model::ClipContent::Automation(
                    common::model::AutomationContent::default(),
                )
            });
        let new_content_id = self.song.alloc_content_id();
        self.song
            .clip_contents
            .insert(new_content_id, cloned_content);
        let lane = self
            .song
            .automation_lane_by_key_mut(source.track, source.lane)?;
        let new_id = lane.alloc_clip_id();
        let new_clip = common::model::AutomationClip {
            id: new_id,
            name,
            start_beat: new_start_beat,
            length_beats: length,
            content_id: new_content_id,
        };
        let pos = lane.clips.partition_point(|c| c.start_beat < new_start_beat);
        lane.clips.insert(pos, new_clip);
        Some(common::model::AutomationClipKey {
            track: source.track,
            lane: source.lane,
            clip: new_id,
        })
    }

    /// FIXME #21: 選択 automation clip 群をまとめて共有複製 (D shortcut)。 選択
    /// ブロック span だけ後ろにずらして複製し、 複製群を選択にする (連打で後方連鎖)。
    fn duplicate_automation_clips_shared(&mut self, sources: &[common::model::AutomationClipKey]) {
        let Some(offset) = self.automation_block_span(sources) else {
            return;
        };
        let mut new_keys = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song
                .automation_lane_by_key(src.track, src.lane)
                .and_then(|lane| lane.clip_by_id(src.clip))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(k) = self.duplicate_one_automation_clip_shared_at(src, new_start) {
                new_keys.push(k);
            }
        }
        if !new_keys.is_empty() {
            self.selected_automation_clips = new_keys;
            self.sync_song_to_plugin_host();
        }
    }

    /// FIXME #21: 選択 automation clip 群をまとめて独立複製 (Alt+D shortcut)。
    fn duplicate_automation_clips_unique(&mut self, sources: &[common::model::AutomationClipKey]) {
        let Some(offset) = self.automation_block_span(sources) else {
            return;
        };
        let mut new_keys = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song
                .automation_lane_by_key(src.track, src.lane)
                .and_then(|lane| lane.clip_by_id(src.clip))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(k) = self.duplicate_one_automation_clip_unique_at(src, new_start) {
                new_keys.push(k);
            }
        }
        if !new_keys.is_empty() {
            self.selected_automation_clips = new_keys;
            self.sync_song_to_plugin_host();
        }
    }

    fn resize_automation_clips(&mut self, deltas: &[ResizeAutomationClipEntry]) {
        if deltas.is_empty() {
            return;
        }
        for d in deltas {
            let Some(lane) = self
                .song
                .automation_lane_by_key_mut(d.key.track, d.key.lane)
            else {
                continue;
            };
            if let Some(clip) = lane.clip_by_id_mut(d.key.clip) {
                clip.start_beat = d.next_start;
                clip.length_beats = d.next_len;
            }
        }
        self.sync_song_to_plugin_host();
    }

    /// `refcount >= 2` の共有 automation clip を独立化。content を deep
    /// clone + 新 `ContentId` 採番、当該 clip だけ新 id を指す。`refcount
    /// == 1` のときは no-op + status_message で通知 (= MIDI 用
    /// `MakeClipUnique` と同 UX)。
    fn make_automation_clip_unique(&mut self, key: common::model::AutomationClipKey) {
        let content_id = {
            let Some(lane) = self.song.automation_lane_by_key(key.track, key.lane) else {
                return;
            };
            let Some(clip) = lane.clip_by_id(key.clip) else {
                return;
            };
            clip.content_id
        };
        if self.song.clip_content_refcount(content_id) <= 1 {
            self.status_message = "すでに独立 clip です".into();
            return;
        }
        let Some(cloned_content) = self.song.clip_contents.get(&content_id).cloned()
        else {
            return;
        };
        let new_content_id = self.song.alloc_content_id();
        self.song.clip_contents.insert(new_content_id, cloned_content);
        if let Some(lane) = self.song.automation_lane_by_key_mut(key.track, key.lane)
            && let Some(clip) = lane.clip_by_id_mut(key.clip)
        {
            clip.content_id = new_content_id;
        }
        self.sync_song_to_plugin_host();
    }

    /// gui_01 #029 (M14 Phase 63n-4): lane body 空き領域 dblclick で
    /// automation clip を新規作成。`docs/plan_automation.md` §5.5。
    /// 初期 `points` は **空** (= `lane.default_value` 引きずり)、
    /// user が dblclick で point を追加していく Bitwig 流。
    fn create_automation_clip(
        &mut self,
        lane_key: common::model::AutomationLaneKey,
        start_beat: f64,
        len_beats: f64,
    ) {
        // 新 ContentId を先に採番 + 空 Automation content を登録。
        let new_content_id = self.song.alloc_content_id();
        self.song.clip_contents.insert(
            new_content_id,
            common::model::ClipContent::Automation(
                common::model::AutomationContent::default(),
            ),
        );
        let Some(lane) = self
            .song
            .automation_lane_by_key_mut(lane_key.track, lane_key.lane)
        else {
            return;
        };
        let display = automation_target_display_name(&lane.target);
        let clip_id = lane.alloc_clip_id();
        let new_clip = common::model::AutomationClip {
            id: clip_id,
            name: format!("{display} curve"),
            start_beat,
            length_beats: len_beats,
            content_id: new_content_id,
        };
        let pos = lane.clips.partition_point(|c| c.start_beat < start_beat);
        lane.clips.insert(pos, new_clip);
        self.sync_song_to_plugin_host();
    }

    /// `A` キー shortcut の handler。`last_touched_param` の lane を
    /// 該当 track に追加 (or 既存があれば visible = true で復活)。
    /// 仕様: `docs/plan_automation.md` §7.3。
    /// Inspector の image event「📈」 ボタンから呼ばれる。 選択中 image
    /// clip の track に `AutomationTarget::ImageBuiltin(field)` lane を
    /// 追加する (= `docs/plan_image_automation.md` §4.1)。 既存 lane が
    /// 同 target で見つかれば visible / enabled を `true` に戻して
    /// 終わり (= 削除復活 UX)、 無ければ新規作成。 default_value は
    /// 同 track 上の first image event の field 値、 image event が
    /// 解決できなければ image field 共通 default (0 for x/y、 1 for w/h
    /// /opacity)。
    fn add_image_automation_lane(
        &mut self,
        field: common::model::ImageBuiltinParam,
    ) {
        use common::model::{AutomationLane, AutomationTarget, ClipContent, ImageBuiltinParam};
        let Some(target_clip) = self.selected_clip_ref() else {
            self.status_message =
                "Image Automation: 画像 clip を選択してください".into();
            return;
        };
        let track_id_opt = self
            .song
            .tracks
            .get(target_clip.track as usize)
            .map(|t| t.id);
        let Some(track_id) = track_id_opt else {
            return;
        };
        let target = AutomationTarget::ImageBuiltin(field);

        // 既存 lane を find。 あれば visible / enabled を true に。
        if let Some(track) = self.song.track_by_id_mut(track_id)
            && let Some(lane) = track
                .automation_lanes
                .iter_mut()
                .find(|l| l.target == target)
        {
            lane.visible = true;
            lane.enabled = true;
            self.expanded_automation_tracks.insert(track_id);
            self.status_message = format!(
                "Image Automation lane '{}' は既に存在します",
                automation_target_display_name(&target)
            );
            self.sync_song_to_plugin_host();
            return;
        }

        // default_value: 同 track 上の first image event の field 値。
        // image event が無ければ field ごとの常識値。 clamp 範囲は field
        // 種別で異なる (x/y/w/h/opacity = [0,1]、 rotation = [-π, π])。
        let default_value: f64 = {
            let Some(track) = self.song.track_by_id(track_id) else {
                return;
            };
            let event = track.clips.iter().find_map(|c| {
                self.song.clip_contents.get(&c.content_id).and_then(|content| {
                    match content {
                        ClipContent::Image(img) => img.events.first(),
                        _ => None,
                    }
                })
            });
            let v = match event {
                Some(ev) => match field {
                    ImageBuiltinParam::X => ev.x,
                    ImageBuiltinParam::Y => ev.y,
                    ImageBuiltinParam::W => ev.w,
                    ImageBuiltinParam::H => ev.h,
                    ImageBuiltinParam::Opacity => ev.opacity,
                    ImageBuiltinParam::Rotation => ev.rotation_radians,
                },
                None => match field {
                    ImageBuiltinParam::X | ImageBuiltinParam::Y => 0.0,
                    ImageBuiltinParam::W
                    | ImageBuiltinParam::H
                    | ImageBuiltinParam::Opacity => 1.0,
                    ImageBuiltinParam::Rotation => 0.0,
                },
            };
            let v = f64::from(v);
            match field {
                ImageBuiltinParam::Rotation => {
                    v.clamp(-std::f64::consts::PI, std::f64::consts::PI)
                }
                _ => v.clamp(0.0, 1.0),
            }
        };

        let Some(track) = self.song.track_by_id_mut(track_id) else {
            return;
        };
        let lane_id = track.alloc_lane_id();
        let new_lane = AutomationLane {
            id: lane_id,
            target: target.clone(),
            default_value,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: Vec::new(),
            next_clip_id: 1,
        };
        track.automation_lanes.push(new_lane);
        self.expanded_automation_tracks.insert(track_id);
        self.status_message = format!(
            "Added image automation lane: {}",
            automation_target_display_name(&target)
        );
        self.sync_song_to_plugin_host();
    }

    /// PiP drag 中に image lane gesture が `active_param_gestures` に
    /// 残っているか。 record_automation_points_for_tick の起動条件で
    /// 「停止中でも image drag 中なら record を回す」 ために使う。
    fn image_pip_drag_active(&self) -> bool {
        self.active_param_gestures
            .iter()
            .any(|(_, t)| matches!(t, common::model::AutomationTarget::ImageBuiltin(_)))
    }

    /// preview drag begin で呼ばれる。 選択中 image clip の track 上で
    /// `AutomationTarget::ImageBuiltin(_)` lane を持つ全 field を
    /// `active_param_gestures` に登録。 record_automation_points_for_tick
    /// が再生中に 1/64 beat throttle で point を打つ pipeline に乗る
    /// (`docs/plan_image_automation.md` §5)。
    ///
    /// 停止中の drag は ImageEvent.field を直接編集する経路で UI を
    /// 動かすが、 lane が override しているので preview は変化しない (=
    /// default 値だけが変わる)。 「停止中の drag で keyframe を打つ」
    /// UX は別途 follow-up (`docs/plan_image_automation.md` §8 未確定
    /// 事項)。
    fn begin_image_pip_drag_recording(&mut self) {
        use common::model::{AutomationTarget, ImageBuiltinParam};
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let Some(track) = self.song.tracks.get(target_clip.track as usize) else {
            return;
        };
        let track_id = track.id;
        // lane が存在する field を全て active_param_gestures に。 record
        // path が curve insert を行う。
        let fields = [
            ImageBuiltinParam::X,
            ImageBuiltinParam::Y,
            ImageBuiltinParam::W,
            ImageBuiltinParam::H,
            ImageBuiltinParam::Opacity,
        ];
        let mut seeded: Vec<AutomationTarget> = Vec::new();
        for field in fields {
            let target = AutomationTarget::ImageBuiltin(field);
            let has_lane = track
                .automation_lanes
                .iter()
                .any(|l| l.enabled && l.target == target);
            if has_lane {
                self.active_param_gestures.insert((track_id, target.clone()));
                if matches!(
                    self.recording_mode,
                    common::model::RecordingMode::Latch
                        | common::model::RecordingMode::Write
                ) && self.is_playing
                {
                    self.latched_param_gestures.insert((track_id, target.clone()));
                }
                seeded.push(target);
            }
        }
        if !seeded.is_empty() {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// preview drag end で呼ばれる。 begin で seed した全 ImageBuiltin
    /// gesture を `active_param_gestures` から remove。 Touch mode では
    /// `recording_last_beat` からも消す (= 連続録音停止)。 Latch / Write
    /// では latched は stop まで残す (= 既存 ParamGestureEnd と同 idiom)。
    fn end_image_pip_drag_recording(&mut self) {
        use common::model::AutomationTarget;
        // image lane gesture だけを掃除 (audio / plugin gesture は残す)。
        let to_remove: Vec<(u32, AutomationTarget)> = self
            .active_param_gestures
            .iter()
            .filter(|(_, t)| matches!(t, AutomationTarget::ImageBuiltin(_)))
            .cloned()
            .collect();
        let any = !to_remove.is_empty();
        for key in to_remove {
            self.active_param_gestures.remove(&key);
            if self.recording_mode == common::model::RecordingMode::Touch {
                self.recording_last_beat.remove(&key);
            }
        }
        if any {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// docs/plan_text_overlay.md §4 P6: 選択中 text clip の track 上で
    /// `TextBuiltin(_)` lane を持つ全 field を `active_param_gestures` に
    /// 登録 (= image PiP drag と同 idiom)。 lane が無い field は drag が
    /// TextEvent.field を直接書くだけ (= lane override 無し時の単純経路)。
    fn begin_text_pip_drag_recording(&mut self) {
        use common::model::{AutomationTarget, TextBuiltinParam};
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let Some(track) = self.song.tracks.get(target_clip.track as usize) else {
            return;
        };
        let track_id = track.id;
        let fields = [
            TextBuiltinParam::X,
            TextBuiltinParam::Y,
            TextBuiltinParam::W,
            TextBuiltinParam::H,
            TextBuiltinParam::Rotation,
        ];
        let mut seeded = false;
        for field in fields {
            let target = AutomationTarget::TextBuiltin(field);
            let has_lane = track
                .automation_lanes
                .iter()
                .any(|l| l.enabled && l.target == target);
            if has_lane {
                self.active_param_gestures.insert((track_id, target.clone()));
                if matches!(
                    self.recording_mode,
                    common::model::RecordingMode::Latch
                        | common::model::RecordingMode::Write
                ) && self.is_playing
                {
                    self.latched_param_gestures.insert((track_id, target));
                }
                seeded = true;
            }
        }
        if seeded {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// docs/plan_text_overlay.md §4 P6: text PiP drag end で seed した
    /// `TextBuiltin(_)` gesture を `active_param_gestures` から remove
    /// (= image PiP drag end と同 idiom)。
    fn end_text_pip_drag_recording(&mut self) {
        use common::model::AutomationTarget;
        let to_remove: Vec<(u32, AutomationTarget)> = self
            .active_param_gestures
            .iter()
            .filter(|(_, t)| matches!(t, AutomationTarget::TextBuiltin(_)))
            .cloned()
            .collect();
        let any = !to_remove.is_empty();
        for key in to_remove {
            self.active_param_gestures.remove(&key);
            if self.recording_mode == common::model::RecordingMode::Touch {
                self.recording_last_beat.remove(&key);
            }
        }
        if any {
            self.sync_recording_lanes_with_audio();
        }
    }

    /// text PiP drag が active (= `TextBuiltin(_)` lane gesture を保持)
    /// なら true。 停止中の drag-while-stopped auto-keyframe を image と
    /// 同様に許可するため、 `record_automation_points_for_tick` の gate
    /// で `image_pip_drag_active() || text_pip_drag_active()` の OR で
    /// 使う。
    fn text_pip_drag_active(&self) -> bool {
        self.active_param_gestures
            .iter()
            .any(|(_, t)| matches!(t, common::model::AutomationTarget::TextBuiltin(_)))
    }

    /// 選択中 image clip の track から `ImageBuiltin(field)` lane を
    /// 削除 (= override 解除)。 lane が見つからない場合は no-op + status
    /// 表示。 削除後は ImageEvent.field がふたたび effective。
    fn remove_image_automation_lane(
        &mut self,
        field: common::model::ImageBuiltinParam,
    ) {
        use common::model::AutomationTarget;
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let target = AutomationTarget::ImageBuiltin(field);
        let Some(track) = self.song.tracks.get_mut(target_clip.track as usize) else {
            return;
        };
        let before = track.automation_lanes.len();
        track.automation_lanes.retain(|l| l.target != target);
        let removed = before - track.automation_lanes.len();
        if removed == 0 {
            self.status_message = format!(
                "Image Automation: {} lane が見つかりません",
                automation_target_display_name(&target)
            );
            return;
        }
        self.status_message = format!(
            "Image Automation lane '{}' を削除しました",
            automation_target_display_name(&target)
        );
        self.sync_song_to_plugin_host();
    }

    // ---- 立ち絵 group transform (`docs/plan_tachie_group_transform.md` §5.5) --

    /// 選択中（cursor）group track に `GroupTransform(param)` lane を追加。
    /// 既存があれば visible / enabled 復活のみ。default_value は現
    /// `group_transform` の field 値（plain）。`add_image_automation_lane` と同型。
    fn add_group_automation_lane(&mut self, param: common::model::GroupTransformParam) {
        use common::model::{AutomationLane, AutomationTarget};
        let Some(track_id) = self.cursor_track_id() else {
            self.status_message =
                "Group Transform: group track を選択してください".into();
            return;
        };
        let target = AutomationTarget::GroupTransform(param);
        if let Some(track) = self.song.track_by_id_mut(track_id)
            && let Some(lane) =
                track.automation_lanes.iter_mut().find(|l| l.target == target)
        {
            lane.visible = true;
            lane.enabled = true;
            self.expanded_automation_tracks.insert(track_id);
            self.status_message = format!(
                "Group Automation lane '{}' は既に存在します",
                automation_target_display_name(&target)
            );
            self.sync_song_to_plugin_host();
            return;
        }
        let gt = self
            .song
            .track_by_id(track_id)
            .and_then(|t| t.group_transform)
            .unwrap_or_default();
        let default_value = f64::from(group_transform_field(&gt, param));
        let Some(track) = self.song.track_by_id_mut(track_id) else {
            return;
        };
        let lane_id = track.alloc_lane_id();
        track.automation_lanes.push(AutomationLane {
            id: lane_id,
            target: target.clone(),
            default_value,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: Vec::new(),
            next_clip_id: 1,
        });
        self.expanded_automation_tracks.insert(track_id);
        self.status_message = format!(
            "Added group automation lane: {}",
            automation_target_display_name(&target)
        );
        self.sync_song_to_plugin_host();
    }

    /// 選択中 group track から `GroupTransform(param)` lane を削除。
    fn remove_group_automation_lane(
        &mut self,
        param: common::model::GroupTransformParam,
    ) {
        use common::model::AutomationTarget;
        let Some(track_id) = self.cursor_track_id() else {
            return;
        };
        let target = AutomationTarget::GroupTransform(param);
        let Some(track) = self.song.track_by_id_mut(track_id) else {
            return;
        };
        let before = track.automation_lanes.len();
        track.automation_lanes.retain(|l| l.target != target);
        if before == track.automation_lanes.len() {
            self.status_message = format!(
                "Group Automation: {} lane が見つかりません",
                automation_target_display_name(&target)
            );
            return;
        }
        self.status_message = format!(
            "Group Automation lane '{}' を削除しました",
            automation_target_display_name(&target)
        );
        self.sync_song_to_plugin_host();
    }

    /// `Track.group_transform`（無ければ default を Some 化）の該当 field を
    /// 設定。`last_touched_param` も更新（touch+A 用）。純 visual なので audio
    /// へは送らない。
    fn set_group_transform_field(
        &mut self,
        track_id: u32,
        param: common::model::GroupTransformParam,
        value: f32,
    ) {
        use common::model::GroupTransformParam as G;
        let Some(track) = self.song.track_by_id_mut(track_id) else {
            return;
        };
        let gt = track.group_transform.get_or_insert_with(Default::default);
        match param {
            G::X => gt.x = value,
            G::Y => gt.y = value,
            G::Rotation => gt.rotation_radians = value,
            G::ScaleX => gt.scale_x = value,
            G::ScaleY => gt.scale_y = value,
            G::AnchorX => gt.anchor_x = value,
            G::AnchorY => gt.anchor_y = value,
            G::Opacity => gt.opacity = value,
        }
        self.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::GroupTransform(param),
            display_name: format!("Group {}", group_param_label(param)),
            touched_at: std::time::Instant::now(),
        });
    }

    /// group track が「visual group」か（§5.6 派生判定）。subtree に image /
    /// video / text 表示 clip を持つ track が 1 つでもある、または既に
    /// `group_transform` データを持つなら true。inspector / 合成の gate。
    pub fn group_has_visual_content(&self, group_track_id: u32) -> bool {
        crate::group_compose::group_has_visual_content(&self.song, group_track_id)
    }

    /// group inspector 用 summary。cursor track が visual group なら、各 param に
    /// `GroupTransform(param)` lane があるか（=「A」 トグル点灯）を返す。
    pub fn inspector_group_transform_summary(
        &self,
    ) -> Option<GroupTransformInspectorSummary> {
        let idx = self.cursor_track_index()?;
        let track = self.song.tracks.get(idx)?;
        if !self.is_group_track(track.id) || !self.group_has_visual_content(track.id) {
            return None;
        }
        let mut automated = [false; 8];
        for param in GROUP_PARAMS {
            automated[group_param_index(param)] = track.automation_lanes.iter().any(
                |l| matches!(l.target, common::model::AutomationTarget::GroupTransform(p) if p == param),
            );
        }
        Some(GroupTransformInspectorSummary {
            track_id: track.id,
            automated,
            transform: track.group_transform.unwrap_or_default(),
        })
    }

    /// docs/plan_text_overlay.md §4 P8: 選択中 text clip の track に
    /// `TextBuiltin(field)` lane を追加。 既存 lane があれば visible /
    /// enabled を再有効化のみ。 default_value は `lane_default_for_target`
    /// 経由で TextEvent の現値 (= image lane と同 idiom、 23 field 分の
    /// match は `lane_default_for_target` 内に集約済)。
    fn add_text_automation_lane(
        &mut self,
        field: common::model::TextBuiltinParam,
    ) {
        use common::model::{AutomationLane, AutomationTarget};
        let Some(target_clip) = self.selected_clip_ref() else {
            self.status_message =
                "Text Automation: text clip を選択してください".into();
            return;
        };
        let track_id_opt = self
            .song
            .tracks
            .get(target_clip.track as usize)
            .map(|t| t.id);
        let Some(track_id) = track_id_opt else {
            return;
        };
        let target = AutomationTarget::TextBuiltin(field);

        // 既存 lane があれば visible / enabled だけを true に。
        if let Some(track) = self.song.track_by_id_mut(track_id)
            && let Some(lane) = track
                .automation_lanes
                .iter_mut()
                .find(|l| l.target == target)
        {
            lane.visible = true;
            lane.enabled = true;
            self.expanded_automation_tracks.insert(track_id);
            self.status_message = format!(
                "Text Automation lane '{}' は既に存在します",
                automation_target_display_name(&target)
            );
            self.sync_song_to_plugin_host();
            return;
        }

        // 23 field 分の現値解決は `lane_default_for_target` が TextBuiltin
        // を扱う (TextEvent 無し時の常識値も同関数内)。 caller は track_id
        // + target を流すだけ。
        let touched = TouchedParam {
            track_id,
            target: target.clone(),
            display_name: automation_target_display_name(&target).to_string(),
            touched_at: std::time::Instant::now(),
        };
        let default_value = self.lane_default_for_target(&touched);

        let Some(track) = self.song.track_by_id_mut(track_id) else {
            return;
        };
        let lane_id = track.alloc_lane_id();
        track.automation_lanes.push(AutomationLane {
            id: lane_id,
            target: target.clone(),
            default_value,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: Vec::new(),
            next_clip_id: 1,
        });
        self.expanded_automation_tracks.insert(track_id);
        self.status_message = format!(
            "Added text automation lane: {}",
            automation_target_display_name(&target)
        );
        self.sync_song_to_plugin_host();
    }

    /// 選択中 text clip の track から `TextBuiltin(field)` lane を削除
    /// (= override 解除、 TextEvent.field が ふたたび effective)。 lane
    /// が無ければ no-op + status 表示。
    fn remove_text_automation_lane(
        &mut self,
        field: common::model::TextBuiltinParam,
    ) {
        use common::model::AutomationTarget;
        let Some(target_clip) = self.selected_clip_ref() else {
            return;
        };
        let target = AutomationTarget::TextBuiltin(field);
        let Some(track) = self.song.tracks.get_mut(target_clip.track as usize)
        else {
            return;
        };
        let before = track.automation_lanes.len();
        track.automation_lanes.retain(|l| l.target != target);
        let removed = before - track.automation_lanes.len();
        if removed == 0 {
            self.status_message = format!(
                "Text Automation: {} lane が見つかりません",
                automation_target_display_name(&target)
            );
            return;
        }
        self.status_message = format!(
            "Text Automation lane '{}' を削除しました",
            automation_target_display_name(&target)
        );
        self.sync_song_to_plugin_host();
    }

    /// 子プロセス (daw_audio / daw_plugin_host) の pipe loop が break
    /// したときに呼ばれる。 `ChildSupervisor.respawn(kind)` で新 child を
    /// spawn + handshake + Session/OpenWorkerPool 再送し、 成功なら
    /// `audio_tx` / `plugin_tx` を新 sender に差し替え、 SetProjectDir +
    /// LoadSong + restore_plugin_from_song で state restore。 失敗時は
    /// tx を None のまま status_message でユーザーに通知 (= sync_song
    /// _to_plugin_host を呼ぶと send が捨てられるが panic しない)。
    ///
    /// `is_playing` は false に戻す (= ユーザーが Play を押し直す前提)。
    /// audio engine が再起動した直後の playhead は 0 で、 旧 playhead に
    /// 自動 seek すると意図しない位置から再生になるので、 user に明示
    /// 操作してもらう方が安全。
    fn handle_child_disconnected(&mut self, kind: common::protocol::ChildKind) {
        use common::protocol::ChildKind;
        let was_playing = self.is_playing;
        self.is_playing = false;
        self.pending_play = false;
        self.active_param_gestures.clear();
        self.latched_param_gestures.clear();
        match kind {
            ChildKind::Audio => {
                self.audio_tx = None;
                tracing::warn!("daw_audio child disconnected");
            }
            ChildKind::PluginHost => {
                self.plugin_tx = None;
                self.pending_plugin_loads.clear();
                self.loaded_slots.clear();
                tracing::warn!("daw_plugin_host child disconnected");
            }
        }

        // supervisor 経由で respawn を試みる。 supervisor が None
        // (= script / test 経路) なら通知だけで終わる。
        let Some(supervisor) = self.supervisor.clone() else {
            self.status_message = format!(
                "{}が切断されました{} — supervisor 無効",
                kind.as_str(),
                if was_playing { " (再生停止)" } else { "" }
            );
            return;
        };
        match supervisor.respawn(kind) {
            Ok(new_tx) => {
                match kind {
                    ChildKind::Audio => self.audio_tx = Some(new_tx),
                    ChildKind::PluginHost => self.plugin_tx = Some(new_tx),
                }
                // state restore: project_dir + LoadSong (= sync_song_to
                // _plugin_host 経路)、 plugin slots は restore_plugin_from
                // _song で SetSlotPlugin 再送。
                let song_snapshot = self.song.clone();
                self.restore_plugin_from_song(&song_snapshot);
                self.sync_song_to_plugin_host();
                self.status_message = format!(
                    "{}を再起動しました{}",
                    kind.as_str(),
                    if was_playing { " (再生は手動で再開してください)" } else { "" }
                );
                tracing::info!(?kind, "child respawn + state restore completed");
            }
            Err(e) => {
                self.status_message = format!(
                    "{}の再起動に失敗しました: {} — アプリ再起動が必要です",
                    kind.as_str(),
                    e
                );
                tracing::error!(error = %e, ?kind, "child respawn failed");
            }
        }
    }

    fn add_automation_from_last_touched(&mut self) {
        let Some(touched) = self.last_touched_param.clone() else {
            self.status_message =
                "No parameter touched yet — drag any knob first".into();
            return;
        };
        // Phase 5 Step 5.1 (gui_01 #034): song-level target は master row の
        // `song_lanes` に追加 (= track 紐付け無し)。 TrackBuiltin / PluginParam
        // は従来通り該当 track の automation_lanes に追加。
        let is_song_level = matches!(
            touched.target,
            common::model::AutomationTarget::SongTempo
                | common::model::AutomationTarget::SongTimeSigNumerator
        );
        // song-level でない場合のみ touched track が削除済か検査。
        if !is_song_level && self.song.track_by_id(touched.track_id).is_none() {
            self.last_touched_param = None;
            self.status_message =
                "Last-touched parameter's track was removed".into();
            return;
        }
        // 既存 lane を find (target 一致)。 master か track かで lookup 経路が分岐。
        let existing_lane_id: Option<u32> = if is_song_level {
            self.song
                .song_lanes
                .iter()
                .find(|l| l.target == touched.target)
                .map(|l| l.id)
        } else {
            self.song
                .track_by_id(touched.track_id)
                .and_then(|t| {
                    t.automation_lanes
                        .iter()
                        .find(|l| l.target == touched.target)
                        .map(|l| l.id)
                })
        };
        if let Some(lane_id) = existing_lane_id {
            // 既存 lane を visible / enabled = true に戻して expand。
            let lookup_track_id = if is_song_level {
                common::model::MASTER_TRACK_ID
            } else {
                touched.track_id
            };
            if let Some(lane) =
                self.song.automation_lane_by_key_mut(lookup_track_id, lane_id)
            {
                lane.visible = true;
                lane.enabled = true;
            }
            if is_song_level {
                self.master_row_automation_expanded = true;
            } else {
                self.expanded_automation_tracks.insert(touched.track_id);
            }
            self.status_message = format!(
                "Automation lane '{}' は既に存在します",
                touched.display_name
            );
            self.sync_song_to_plugin_host();
            return;
        }
        // 新規 lane を作成。default_value は target に応じて現在値を引く。
        let default_value = self.lane_default_for_target(&touched);
        if is_song_level {
            let lane_id = self.song.alloc_song_lane_id();
            let new_lane = common::model::AutomationLane {
                id: lane_id,
                target: touched.target.clone(),
                default_value,
                enabled: true,
                visible: true,
                height_px: 60,
                clips: Vec::new(),
                next_clip_id: 1,
            };
            self.song.song_lanes.push(new_lane);
            self.master_row_automation_expanded = true;
        } else {
            let Some(track) = self.song.track_by_id_mut(touched.track_id) else {
                return;
            };
            let lane_id = track.alloc_lane_id();
            let new_lane = common::model::AutomationLane {
                id: lane_id,
                target: touched.target.clone(),
                default_value,
                enabled: true,
                visible: true,
                height_px: 60,
                clips: Vec::new(),
                next_clip_id: 1,
            };
            track.automation_lanes.push(new_lane);
            self.expanded_automation_tracks.insert(touched.track_id);
        }
        self.status_message = format!(
            "Added automation lane: {}",
            touched.display_name
        );
        self.sync_song_to_plugin_host();
    }

    /// `AddAutomationFromLastTouched` の補助。target の現在値を plain
    /// 単位で取得 (lane.default_value 初期化用)。 track-builtin は
    /// track の strip 値、 plugin param は 0.0 (Phase 2 で IPC lookup)、
    /// song-level は `song.bpm` / `song.time_sig.0`。
    fn lane_default_for_target(&self, touched: &TouchedParam) -> f64 {
        use common::model::{AutomationTarget, TrackBuiltinParam};
        match &touched.target {
            AutomationTarget::TrackBuiltin(param) => {
                let Some(track) = self.song.track_by_id(touched.track_id) else {
                    return 0.0;
                };
                match param {
                    TrackBuiltinParam::Volume => f64::from(track.volume),
                    TrackBuiltinParam::Pan => f64::from(track.pan),
                    TrackBuiltinParam::Mute => {
                        if track.muted {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    TrackBuiltinParam::SendGain { .. } => 0.0,
                }
            }
            AutomationTarget::PluginParam { .. } => 0.0,
            AutomationTarget::SongTempo => f64::from(self.song.bpm),
            AutomationTarget::SongTimeSigNumerator => f64::from(self.song.time_sig.0),
            // Image PiP default: 同 track の最初の image clip の first
            // event 値を初期値に使う。 1 つも image clip が無い (= lane
            // を空 image track で先行追加するケース) は 0.0 fallback。
            AutomationTarget::ImageBuiltin(field) => {
                use common::model::{ClipContent, ImageBuiltinParam};
                let Some(track) = self.song.track_by_id(touched.track_id) else {
                    return 0.0;
                };
                let event = track.clips.iter().find_map(|c| {
                    self.song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|content| match content {
                            ClipContent::Image(img) => img.events.first(),
                            _ => None,
                        })
                });
                let Some(ev) = event else { return 0.0 };
                f64::from(match field {
                    ImageBuiltinParam::X => ev.x,
                    ImageBuiltinParam::Y => ev.y,
                    ImageBuiltinParam::W => ev.w,
                    ImageBuiltinParam::H => ev.h,
                    ImageBuiltinParam::Opacity => ev.opacity,
                    ImageBuiltinParam::Rotation => ev.rotation_radians,
                })
            }
            // Text default: 同 track の first text event の field 値。
            // text clip が無い (= lane を空 track で先行追加) は field
            // ごとの常識値 (色 RGBA は (1,1,1,1) や (0,0,0,1) 等)。
            AutomationTarget::TextBuiltin(field) => {
                use common::model::{ClipContent, TextBuiltinParam as T};
                let Some(track) = self.song.track_by_id(touched.track_id) else {
                    return 0.0;
                };
                let event = track.clips.iter().find_map(|c| {
                    self.song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|content| match content {
                            ClipContent::Text(t) => t.events.first(),
                            _ => None,
                        })
                });
                let Some(ev) = event else {
                    // text clip 無し → default 値 (= TextEvent::default
                    // の常識値と整合させる)。
                    return match field {
                        T::X => 0.0,
                        T::Y => 0.4,
                        T::W => 1.0,
                        T::H => 0.2,
                        T::Opacity => 1.0,
                        T::Rotation => 0.0,
                        T::FontSize => 64.0,
                        T::FillR | T::FillG | T::FillB | T::FillA => 1.0,
                        T::OutlineR | T::OutlineG | T::OutlineB => 0.0,
                        T::OutlineA => 1.0,
                        T::OutlineWidth => 0.0,
                        T::ShadowR | T::ShadowG | T::ShadowB => 0.0,
                        T::ShadowA => 0.5,
                        T::ShadowOffsetX | T::ShadowOffsetY => 0.0,
                        T::ShadowBlur => 0.0,
                    };
                };
                f64::from(match field {
                    T::X => ev.x,
                    T::Y => ev.y,
                    T::W => ev.w,
                    T::H => ev.h,
                    T::Opacity => ev.opacity,
                    T::Rotation => ev.rotation_radians,
                    T::FontSize => ev.font_size_px,
                    T::FillR => ev.fill_color[0],
                    T::FillG => ev.fill_color[1],
                    T::FillB => ev.fill_color[2],
                    T::FillA => ev.fill_color[3],
                    T::OutlineR => ev.outline_color[0],
                    T::OutlineG => ev.outline_color[1],
                    T::OutlineB => ev.outline_color[2],
                    T::OutlineA => ev.outline_color[3],
                    T::OutlineWidth => ev.outline_width_px,
                    T::ShadowR => ev.shadow_color[0],
                    T::ShadowG => ev.shadow_color[1],
                    T::ShadowB => ev.shadow_color[2],
                    T::ShadowA => ev.shadow_color[3],
                    T::ShadowOffsetX => ev.shadow_offset_px.0,
                    T::ShadowOffsetY => ev.shadow_offset_px.1,
                    T::ShadowBlur => ev.shadow_blur_px,
                })
            }
            // Group transform default: 同 track の group_transform (無ければ
            // GroupTransform::default) の該当 field。 group は表示 clip を持たない
            // ので image/text のような clip 探索は不要。
            AutomationTarget::GroupTransform(param) => {
                use common::model::GroupTransformParam as G;
                let gt = self
                    .song
                    .track_by_id(touched.track_id)
                    .and_then(|t| t.group_transform)
                    .unwrap_or_default();
                f64::from(match param {
                    G::X => gt.x,
                    G::Y => gt.y,
                    G::Rotation => gt.rotation_radians,
                    G::ScaleX => gt.scale_x,
                    G::ScaleY => gt.scale_y,
                    G::AnchorX => gt.anchor_x,
                    G::AnchorY => gt.anchor_y,
                    G::Opacity => gt.opacity,
                })
            }
        }
    }

    fn delete_automation_clips(&mut self, keys: &[common::model::AutomationClipKey]) {
        if keys.is_empty() {
            return;
        }
        for k in keys {
            let Some(lane) = self.song.automation_lane_by_key_mut(k.track, k.lane) else {
                continue;
            };
            if let Some(idx) = lane.clip_index_by_id(k.clip) {
                lane.clips.remove(idx);
            }
        }
        // 選択中だった clip があれば selection からも除く。
        self.selected_automation_clips
            .retain(|sel| !keys.iter().any(|k| k == sel));
        self.sync_song_to_plugin_host();
    }

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
        // FIXME #13 (plan_group_nesting): selection-root rule。 選択集合のうち、
        // 親 (`parent_group_id`) が同じ選択集合に **含まれていない** トラック
        // (= 最上位) だけを新グループへ付け替える。 グループとその子を一緒に
        // 選んだ場合、 子は元のグループに残り、 内側グループの階層が平坦化しない
        // (= 元グループが解除されない)。
        let selected: std::collections::HashSet<u32> = child_ids.iter().copied().collect();
        let roots: Vec<u32> = child_ids
            .iter()
            .copied()
            .filter(|id| {
                self.song
                    .track_by_id(*id)
                    .and_then(|t| t.parent_group_id)
                    .is_none_or(|pid| !selected.contains(&pid))
            })
            .collect();
        if roots.is_empty() {
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
                .track_by_id(roots[0])
                .and_then(|t| t.parent_group_id);
            if roots.iter().all(|id| {
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
        // Repoint every selection-root track's parent to the new group.
        // 子孫 (= 親が選択集合内のトラック) は元の親に残すことで、 内側
        // グループの入れ子が保たれる (FIXME #13)。
        for &cid in &roots {
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
        if !self.song_has_plugin() {
            self.push_undo_snapshot();
            self.action_ungroup_tracks_inner(track_ids);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(
            DeferredEdit::UngroupTracks {
                track_ids: track_ids.to_vec(),
            },
        ));
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
        // selected_clips / selected_clip は stable ClipKey 保持。 削除された
        // track の clip を指す選択だけ落とす (track は上で pop 済なので clip_at が
        // 解決できない)。 残りは index 変化に自動追従。
        let mut keys = std::mem::take(&mut self.selected_clips);
        keys.retain(|k| self.clip_at(*k).is_some());
        self.selected_clips = keys;
        if let Some(k) = self.selected_clip
            && self.clip_at(k).is_none()
        {
            self.selected_clip = self.selected_clips.last().copied();
            self.selected_notes.clear();
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
    }

    // -------- Clip / note / midi -------------------------------------------

    /// Phase 7 B4 Step D (2026-05-13): MIDI input note_on dispatcher。
    /// `midi_recording` で arm 録音 path、 そうでなければ既存 step-input mode。
    fn handle_midi_note_on(&mut self, pitch: u8, velocity: u8) {
        if self.midi_recording {
            self.record_midi_note_on(pitch, velocity);
        } else {
            self.step_input_note_on(pitch, velocity);
        }
    }

    /// Phase 7 B4 Step D: MIDI input note_off dispatcher。 録音中は length 確定、
    /// step-input mode は no-op (既存挙動)。
    fn handle_midi_note_off(&mut self, pitch: u8) {
        if self.midi_recording {
            self.record_midi_note_off(pitch);
        }
    }

    /// Phase 7 B1-M Step 2 (2026-05-13): MIDI Learn 経路 + 通常 lookup 経路。
    /// `midi_learn_target` Some なら新規 binding 追加 (= 同じ
    /// `(channel, controller)` 既存 entry は replace、 1 度の CC 受信で None
    /// に戻る)、 None なら `Song.midi_bindings` を lookup して match した
    /// 全 target に値を送る。 channel = 16 は any-channel match。
    fn handle_midi_control_change(
        &mut self,
        channel: u8,
        controller: u8,
        value: u8,
    ) {
        if let Some(target) = self.midi_learn_target.take() {
            // Learn mode: 既存 同 (channel, controller) を retain で除外 +
            // 新 binding push。 status_message は次 frame の通常 status に上書き
            // されるが「bind 完了」 を一瞬表示。
            self.song.midi_bindings.retain(|b| {
                !(b.controller == controller && b.channel == channel)
            });
            self.song.midi_bindings.push(common::model::MidiBinding {
                channel,
                controller,
                target,
            });
            self.status_message =
                format!("MIDI bind: CC {controller} (ch {channel}) → {target:?}");
            return;
        }
        // 通常 lookup: 該当 binding 全てに値送信 (= 同 CC を複数 target に
        // bind する usage を許容)。 channel = 16 は any-channel match。
        let targets: Vec<common::model::BindingTarget> = self
            .song
            .midi_bindings
            .iter()
            .filter(|b| {
                b.controller == controller
                    && (b.channel == channel || b.channel == 16)
            })
            .map(|b| b.target)
            .collect();
        for target in targets {
            self.apply_midi_value_to_target(target, value);
        }
    }

    /// Phase 7 B1-M Step 2: CC 値 (0..127) を target に適用。 normalization は
    /// target ごとに違う (= TrackVolume は 0..1、 TrackPan は -1..1、
    /// SongTempo は 60..180 BPM linear)。 既存 setter (set_track_volume /
    /// set_track_pan / song.bpm + IPC) を経由するので audio engine 反映も
    /// automatic。
    fn apply_midi_value_to_target(
        &mut self,
        target: common::model::BindingTarget,
        value: u8,
    ) {
        let v_norm = f32::from(value.min(127)) / 127.0;
        match target {
            common::model::BindingTarget::TrackVolume(track_id) => {
                self.set_track_volume(track_id, v_norm.clamp(0.0, 1.0));
            }
            common::model::BindingTarget::TrackPan(track_id) => {
                let pan = (v_norm * 2.0 - 1.0).clamp(-1.0, 1.0);
                self.set_track_pan(track_id, pan);
            }
            common::model::BindingTarget::SongTempo => {
                // CC 0..127 → 60..180 BPM linear。 SetSongBpm 軽量 IPC で
                // audio engine の song.bpm を即時更新 (= LoadSong 不要)。
                let bpm = (60.0 + v_norm * 120.0).clamp(1.0, 400.0);
                self.song.bpm = bpm;
                self.send_audio(MainToChild::SetSongBpm { bpm });
            }
            common::model::BindingTarget::PluginParam {
                track,
                slot,
                param_id,
            } => {
                // Phase 7 B1-M Step 4 (2026-05-13): bind データは永続化されて
                // いるが、 actual な injection (= GUI → audio thread → plugin
                // host で IParameterChanges / CLAP_EVENT_PARAM_VALUE 送信) は
                // extended scope (別フェーズ = plan_b1_vst3_completion.md
                // 参照)。 RT-safe IPC + audio thread への queue + plugin
                // host での event injection が必要なため。 現状は bind 完了
                // は status_message に表示されるが、 CC 受信時は warning log
                // のみで音には反映されない。 user に「bind 自体は保存される
                // が CC 受信は次フェーズで動く」 を可視化。
                tracing::warn!(
                    track,
                    ?slot,
                    param_id,
                    cc_norm = v_norm,
                    "MIDI binding to PluginParam is stored but injection \
                     is not yet implemented (extended scope)"
                );
            }
        }
    }

    /// 既存 step-input mode (= selected_clip + step_cursor_beat に固定 length
    /// で 1 note ずつ手動入力)。 midi_recording == false のときだけ走る。
    fn step_input_note_on(&mut self, pitch: u8, velocity: u8) {
        let Some(target) = self.selected_clip_ref() else {
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

    /// Phase 7 B4 Step D: 録音中の note_on 処理。 armed track 全てに対して
    /// playhead 位置の MIDI clip (無ければ新規作成 / 末尾近ければ延長) に
    /// 仮 length 0.05 beat の note を push + active_notes に start_beat
    /// 記録 (= note_off で length 上書き)。 既存 clip の content_id 共有
    /// (linked clip) の場合、 sibling にも自動反映 (= ContentId 共有の意図、
    /// 録音書き込みでも同じ動作、 不都合なら別 phase で「録音前に
    /// make_unique」 を検討)。
    fn record_midi_note_on(&mut self, pitch: u8, velocity: u8) {
        let playhead =
            self.playhead_beat.map(f64::from).unwrap_or(0.0);
        if playhead < 0.0 {
            return;
        }
        // Phase 7 B5 (`docs/plan_scale.html` §5.2): Snap Live Input。
        // note_off も同じ snap を適用するので、 deterministic snap で
        // (track_id, pitch) lookup が整合する。 step input は別 path
        // (`step_input_note_on`) を経由するのでここの snap は録音時のみ。
        let pitch = if self.snap_live_input {
            self.song
                .scale_at(playhead)
                .map(|sc| sc.snap(pitch))
                .unwrap_or(pitch)
        } else {
            pitch
        };
        let armed_ids: Vec<u32> = self
            .song
            .tracks
            .iter()
            .filter(|t| t.armed)
            .map(|t| t.id)
            .collect();
        for track_id in armed_ids {
            self.midi_recording_active_notes
                .insert((track_id, pitch), playhead);
            self.ensure_midi_clip_at_playhead(track_id, playhead);
            let Some((track_idx, clip_idx)) =
                self.find_midi_clip_at_playhead(track_id, playhead)
            else {
                continue;
            };
            let clip_start =
                self.song.tracks[track_idx].clips[clip_idx].start_beat;
            let local_start = playhead - clip_start;
            if let Some(notes) =
                self.song.notes_in_clip_mut(track_idx, clip_idx)
            {
                notes.push(common::model::Note {
                    start_beat: local_start,
                    duration_beats: 0.05,
                    pitch,
                    velocity,
                    lyric: None,
                });
            }
        }
        self.sync_song_to_plugin_host();
    }

    /// Phase 7 B4 Step D: 録音中の note_off 処理。 active_notes から start
    /// を取り出し、 length = playhead - start で確定。 該当 note を
    /// `start_beat` (clip-local) + `pitch` で再 search (= note_on 時に
    /// active_notes に track-domain start を保存しているので、 clip-local
    /// に戻して check)。
    fn record_midi_note_off(&mut self, pitch: u8) {
        let playhead =
            self.playhead_beat.map(f64::from).unwrap_or(0.0);
        // Phase 7 B5: note_on 側で snap した pitch で active_notes に登録して
        // いるので、 note_off の lookup key も同じ snap を適用。 snap は
        // deterministic なので転調を跨がない note なら lookup は必ず hit する。
        let pitch = if self.snap_live_input {
            self.song
                .scale_at(playhead)
                .map(|sc| sc.snap(pitch))
                .unwrap_or(pitch)
        } else {
            pitch
        };
        let armed_ids: Vec<u32> = self
            .song
            .tracks
            .iter()
            .filter(|t| t.armed)
            .map(|t| t.id)
            .collect();
        for track_id in armed_ids {
            let Some(start) = self
                .midi_recording_active_notes
                .remove(&(track_id, pitch))
            else {
                continue;
            };
            let Some((track_idx, clip_idx)) =
                self.find_midi_clip_containing_beat(track_id, start)
            else {
                continue;
            };
            let clip_start =
                self.song.tracks[track_idx].clips[clip_idx].start_beat;
            let local_start = start - clip_start;
            let length = (playhead - start).max(0.05);
            if let Some(notes) =
                self.song.notes_in_clip_mut(track_idx, clip_idx)
                && let Some(n) = notes.iter_mut().find(|n| {
                    (n.start_beat - local_start).abs() < 1e-6
                        && n.pitch == pitch
                })
            {
                n.duration_beats = length;
            }
        }
        self.sync_song_to_plugin_host();
    }

    /// playhead 位置に armed track 用 MIDI clip があれば何もしない、 末尾
    /// 直近 (= 1 beat 以内) なら延長、 それ以外なら新規 clip を playhead
    /// 位置に作成 (length 4 beat、 ContentId 新規採番 + clip_contents 登録)。
    fn ensure_midi_clip_at_playhead(
        &mut self,
        track_id: u32,
        playhead: f64,
    ) {
        let Some(track_idx) =
            self.song.tracks.iter().position(|t| t.id == track_id)
        else {
            return;
        };
        // 既存 clip 内ならそのまま。
        if self.song.tracks[track_idx].clips.iter().any(|c| {
            playhead >= c.start_beat
                && playhead < c.start_beat + c.length_beats
        }) {
            return;
        }
        // 末尾の直近 1 beat 以内なら延長。
        if let Some(clip) = self.song.tracks[track_idx]
            .clips
            .iter_mut()
            .find(|c| {
                let end = c.start_beat + c.length_beats;
                playhead >= end && playhead - end <= 1.0
            })
        {
            clip.length_beats = playhead - clip.start_beat + 4.0;
            return;
        }
        // 新規 clip 作成。
        let cid = self.song.alloc_content_id();
        self.song.clip_contents.insert(
            cid,
            common::model::ClipContent::Midi(common::model::MidiContent {
                notes: vec![],
            }),
        );
        let track = &mut self.song.tracks[track_idx];
        let new_clip_id = track.next_clip_id;
        track.next_clip_id += 1;
        let new_clip = common::model::Clip {
            id: new_clip_id,
            start_beat: playhead,
            length_beats: 4.0,
            name: String::new(),
            content_id: cid,
            ..Default::default()
        };
        track.clips.push(new_clip);
        self.song
            .set_content_name(cid, format!("Recorded {new_clip_id}"));
    }

    /// playhead が clip 範囲 `[start_beat, start_beat + length_beats)` に
    /// 含まれる MIDI clip の (track_idx, clip_idx) を返す。 無ければ None。
    fn find_midi_clip_at_playhead(
        &self,
        track_id: u32,
        playhead: f64,
    ) -> Option<(usize, usize)> {
        let track_idx =
            self.song.tracks.iter().position(|t| t.id == track_id)?;
        let track = &self.song.tracks[track_idx];
        let clip_idx = track.clips.iter().position(|c| {
            playhead >= c.start_beat
                && playhead < c.start_beat + c.length_beats
        })?;
        Some((track_idx, clip_idx))
    }

    /// 指定 beat 位置を含む MIDI clip の (track_idx, clip_idx)。
    /// `find_midi_clip_at_playhead` と同等だが、 引数を意味的に区別する
    /// (= note_off 時は note_on 時刻、 note_on 時は playhead を渡す)。
    fn find_midi_clip_containing_beat(
        &self,
        track_id: u32,
        beat: f64,
    ) -> Option<(usize, usize)> {
        self.find_midi_clip_at_playhead(track_id, beat)
    }

    /// Phase 7 B4 Step C/D: Record toggle button click。 既に recording
    /// (含 pending) なら stop、 idle なら start。
    fn toggle_midi_recording(&mut self) {
        if self.midi_recording || self.midi_recording_pending {
            self.stop_recording();
        } else {
            self.start_recording();
        }
    }

    /// Phase 7 B4 Step C: 録音開始。 `count_in_bars > 0` なら preroll +
    /// metronome 強制 ON、 0 なら即時 recording。 いずれも `Play` を audio に
    /// 送る。 metronome の元状態は `metronome_enabled_pre_recording` に snap。
    fn start_recording(&mut self) {
        if self.midi_recording || self.midi_recording_pending {
            return;
        }
        self.midi_recording_active_notes.clear();
        let bars = self.count_in_bars;
        self.metronome_enabled_pre_recording = Some(self.metronome_enabled);
        if bars > 0 {
            if !self.metronome_enabled {
                // count-in 強制 ON。 既存 SetMetronomeEnabled handler を
                // 呼び出して audio engine への IPC 送信もまとめて。
                self.handle_event(AppEvent::SetMetronomeEnabled(true));
            }
            let beats_per_bar = f64::from(self.song.time_sig.0.max(1));
            let preroll_beats = f64::from(bars) * beats_per_bar;
            let sr = f64::from(common::audio_bridge::SAMPLE_RATE);
            let bpm = f64::from(self.song.bpm.max(1.0));
            let preroll_samples =
                (preroll_beats * 60.0 / bpm * sr).round().max(0.0) as u64;
            self.send_audio(MainToChild::StartCountIn {
                samples: preroll_samples,
            });
            self.midi_recording_pending = true;
        } else {
            self.midi_recording = true;
        }
        self.send_audio(MainToChild::Play);
    }

    /// Phase 7 B4 Step C/D: 録音停止 / cancel。 active_notes 全 clear、
    /// metronome を recording 開始前の状態に戻す、 audio engine の preroll を
    /// `StartCountIn { samples: 0 }` で cancel (= count-in 中の stop に対応)。
    fn stop_recording(&mut self) {
        let was_active =
            self.midi_recording || self.midi_recording_pending;
        self.midi_recording = false;
        self.midi_recording_pending = false;
        self.midi_recording_active_notes.clear();
        if let Some(prev) =
            self.metronome_enabled_pre_recording.take()
            && prev != self.metronome_enabled
        {
            self.handle_event(AppEvent::SetMetronomeEnabled(prev));
        }
        if was_active {
            // count-in 中の cancel もしくは録音終了。 audio engine 側 preroll
            // を 0 で上書きして count-in 即時終了。 normal 録音終了の場合は
            // preroll は既に 0 なので no-op。
            self.send_audio(MainToChild::StartCountIn { samples: 0 });
        }
    }

    /// stable `ClipKey` (track_id + clip_id) → 現在の index ベース `ClipRef`。
    /// track / clip が見つからなければ `None` (= 削除済 / undo で消えた)。
    pub fn clip_ref_of(&self, key: common::model::ClipKey) -> Option<ClipRef> {
        let t_idx = self.song.tracks.iter().position(|t| t.id == key.track_id)?;
        let c_idx = self.song.tracks[t_idx]
            .clips
            .iter()
            .position(|c| c.id == key.clip_id)?;
        Some(ClipRef {
            track: t_idx as u32,
            clip: c_idx as u32,
        })
    }

    /// index ベース `ClipRef` → stable `ClipKey`。 範囲外なら `None`。
    pub fn clip_key_of(&self, r: ClipRef) -> Option<common::model::ClipKey> {
        let t = self.song.tracks.get(r.track as usize)?;
        let c = t.clips.get(r.clip as usize)?;
        Some(common::model::ClipKey {
            track_id: t.id,
            clip_id: c.id,
        })
    }

    /// 選択 anchor (`selected_clip` = 末尾) を現在の `ClipRef` へ解決。
    pub fn selected_clip_ref(&self) -> Option<ClipRef> {
        self.selected_clip.and_then(|k| self.clip_ref_of(k))
    }

    /// 選択集合 (`selected_clips`) を現在の `ClipRef` 群へ解決 (解決でき
    /// ない stale key は除外)。 owned `Vec` を返す。
    pub fn selected_clip_refs(&self) -> Vec<ClipRef> {
        self.selected_clips
            .iter()
            .filter_map(|k| self.clip_ref_of(*k))
            .collect()
    }

    /// stable `ClipKey` → `&Clip` (track_by_id + clip_by_id)。
    pub fn clip_at(&self, key: common::model::ClipKey) -> Option<&common::model::Clip> {
        self.song
            .track_by_id(key.track_id)
            .and_then(|t| t.clip_by_id(key.clip_id))
    }

    fn select_clip(&mut self, target: ClipRef, additive: bool) {
        let Some(key) = self.clip_key_of(target) else {
            return;
        };
        let mut keys = self.selected_clips.clone();
        if additive {
            if let Some(pos) = keys.iter().position(|k| *k == key) {
                keys.remove(pos);
            } else {
                keys.push(key);
            }
        } else {
            keys = vec![key];
        }
        let primary = keys.last().copied();
        self.selected_clips = keys;
        self.selected_clip = primary;
        self.selected_notes.clear();
        self.step_cursor_beat = 0.0;
        if let Some(r) = self.selected_clip_ref() {
            self.select_track(r.track);
        }
        // クリップが新しく primary になったらピアノロールを auto-fit。
        // 同 clip 再選択でも fit し直す (ノート編集で範囲が変わることがある)。
        if primary.is_some() {
            self.fit_piano_roll_to_clip();
        }
    }

    fn set_clip_selection(&mut self, targets: Vec<ClipRef>) {
        let keys: Vec<common::model::ClipKey> =
            targets.iter().filter_map(|r| self.clip_key_of(*r)).collect();
        let primary = keys.last().copied();
        self.selected_clips = keys;
        self.selected_clip = primary;
        self.selected_notes.clear();
        self.step_cursor_beat = 0.0;
        if let Some(r) = self.selected_clip_ref() {
            self.select_track(r.track);
        }
        if primary.is_some() {
            self.fit_piano_roll_to_clip();
        }
    }

    /// Ctrl+A (クリップ領域): 曲全体・全トラックの全クリップを選択。
    /// 全選択は一括操作なので `set_clip_selection` と違い view ジャンプ
    /// (fit_piano_roll_to_clip / select_track) を起こさない (= 表示を
    /// 飛ばさない、 grill-me 2026-06-09 決定)。 既に全選択なら冪等。
    /// anchor (末尾) は inspector 表示用に維持。 selection のみで非 undoable。
    fn select_all_clips(&mut self) {
        let all: Vec<common::model::ClipKey> = self
            .song
            .tracks
            .iter()
            .flat_map(|t| {
                t.clips
                    .iter()
                    .map(|c| common::model::ClipKey {
                        track_id: t.id,
                        clip_id: c.id,
                    })
            })
            .collect();
        if all.is_empty() {
            return;
        }
        // 既に全選択なら冪等 (集合一致を順序非依存で判定)。
        if self.selected_clips.len() == all.len() {
            let cur: std::collections::HashSet<common::model::ClipKey> =
                self.selected_clips.iter().copied().collect();
            if all.iter().all(|k| cur.contains(k)) {
                return;
            }
        }
        self.selected_clip = all.last().copied();
        self.selected_clips = all;
        self.selected_notes.clear();
    }

    /// 単一 clip (新規作成直後の `ClipRef`) を選択集合にする。 ClipRef→ClipKey
    /// 変換して anchor + set を更新 (view ジャンプ無し)。 create / duplicate の
    /// 結果選択用。
    fn set_single_clip_selection(&mut self, r: ClipRef) {
        let key = self.clip_key_of(r);
        self.selected_clip = key;
        self.selected_clips = key.into_iter().collect();
    }

    /// 新規 clip 群 (`ClipRef`) を選択集合にする (anchor = 末尾、 view ジャンプ
    /// 無し)。 ClipRef→ClipKey 変換。 clone / split / glue の結果選択用。
    fn select_new_clips(&mut self, refs: &[ClipRef]) {
        let keys: Vec<common::model::ClipKey> =
            refs.iter().filter_map(|r| self.clip_key_of(*r)).collect();
        self.selected_clip = keys.last().copied();
        self.selected_clips = keys;
    }

    /// Ctrl+A (ピアノロール): `selected_clip` の MIDI 全ノート id を返す
    /// (id = clip 内 notes Vec の index)。 非 MIDI / 未選択なら空。
    pub fn all_note_ids_in_selected_clip(&self) -> Vec<u32> {
        let Some(target) = self.selected_clip_ref() else {
            return Vec::new();
        };
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return Vec::new();
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return Vec::new();
        };
        match self.song.clip_contents.get(&clip.content_id) {
            Some(common::model::ClipContent::Midi(midi)) => {
                (0..midi.notes.len() as u32).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Ctrl+A (オーディオエディタ): 開いている clip の全 audio event index
    /// を返す。 audio_editor_clip が無い / 非 audio なら空。
    pub fn all_audio_event_indices(&self) -> Vec<usize> {
        let Some(target) = self.audio_editor_clip else {
            return Vec::new();
        };
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return Vec::new();
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return Vec::new();
        };
        match self.song.clip_contents.get(&clip.content_id) {
            Some(common::model::ClipContent::Audio(audio)) => (0..audio.events.len()).collect(),
            _ => Vec::new(),
        }
    }

    /// Ctrl+A (automation lane): 指定 lane 内の全ポイントを
    /// `AutomationPointKeyRef` で列挙する。 lane.clips の各 clip の content
    /// (`ClipContent::Automation`) points を走査。 master row
    /// (`MASTER_TRACK_ID`) も `automation_lane_by_key` 経由で対応。
    /// lane が無い / ポイントが無いなら空。
    pub fn all_automation_points_in_lane(
        &self,
        lane: common::model::AutomationLaneKey,
    ) -> Vec<AutomationPointKeyRef> {
        let Some(lane_ref) = self.song.automation_lane_by_key(lane.track, lane.lane) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for clip in &lane_ref.clips {
            let n = match self.song.clip_contents.get(&clip.content_id) {
                Some(common::model::ClipContent::Automation(a)) => a.points.len(),
                _ => 0,
            };
            for point_idx in 0..n as u32 {
                out.push(AutomationPointKeyRef {
                    track_id: lane.track,
                    lane_id: lane.lane,
                    clip_id: clip.id,
                    point_idx,
                });
            }
        }
        out
    }

    /// 右クリック「共有を一括選択」: `target` と同じ `content_id` を持つ
    /// main clip を全 track から集めて選択する (linked clip group)。
    /// `content_id` は payload 種別ごとに別空間なので automation clip 等と
    /// 混ざらない。 refcount==1 のときは自身 1 個の選択 (= 無害)。 clicked
    /// `target` を末尾 (= primary) に置いて piano_roll fit 対象を維持する。
    fn select_linked_clips(&mut self, target: ClipRef) {
        let Some(content_id) = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        let mut linked = Vec::new();
        for (t_idx, track) in self.song.tracks.iter().enumerate() {
            for (c_idx, clip) in track.clips.iter().enumerate() {
                if clip.content_id == content_id {
                    linked.push(ClipRef {
                        track: t_idx as u32,
                        clip: c_idx as u32,
                    });
                }
            }
        }
        if linked.is_empty() {
            return;
        }
        if let Some(pos) = linked.iter().position(|r| *r == target) {
            let last = linked.len() - 1;
            linked.swap(pos, last);
        }
        let count = linked.len();
        self.set_clip_selection(linked);
        self.status_message = if count <= 1 {
            "共有クリップはありません (この clip は単独)".to_string()
        } else {
            format!("共有クリップ {count} 個を選択しました")
        };
    }

    /// 現 selected_clip のノート bounding box が piano_roll grid 領域に
    /// 収まるよう zoom_x / zoom_y / scroll_beat / top_pitch を自動調整する。
    /// ノート無しの clip は clip 全長が見える初期 zoom にフォールバック。
    /// `last_pianoroll_grid_size` が未測定 (= 0) の場合は `pending_pianoroll_fit`
    /// を立てて return → piano_roll が初めて描画され grid_size が確定したフレームの
    /// Edit 内で再実行される (初回 fit 喪失バグの修正、 [piano_roll_view::draw] 参照)。
    fn fit_piano_roll_to_clip(&mut self) {
        let Some(target) = self.selected_clip_ref() else { return };
        let Some(track) = self.song.tracks.get(target.track as usize) else { return };
        let Some(clip) = track.clips.get(target.clip as usize) else { return };
        let (grid_w, grid_h) = self.last_pianoroll_grid_size;
        if grid_w < 16.0 || grid_h < 16.0 {
            self.pending_pianoroll_fit = true;
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
        }    }

    /// 親 group chain のいずれかが `collapsed_groups` に含まれる (= 折り畳まれた
    /// group の配下で hide される) か。 arrangement widget の `is_visible_track`
    /// と同じ判定を daw_01 側で行い、 mixer の strip 折り畳み (FIXME #7) が
    /// arrangement と同じ可視集合を共有する (`collapsed_groups` が SSoT)。
    /// 32 hop で cycle 安全。
    pub fn is_hidden_under_collapsed_group(&self, track_id: u32) -> bool {
        let mut cursor = self
            .song
            .track_by_id(track_id)
            .and_then(|t| t.parent_group_id);
        let mut hops = 0u8;
        while let Some(pid) = cursor {
            if self.collapsed_groups.contains(&pid) {
                return true;
            }
            hops += 1;
            if hops > 32 {
                break;
            }
            cursor = self.song.track_by_id(pid).and_then(|t| t.parent_group_id);
        }
        false
    }

    /// 全 track の全 clip が arrangement canvas に収まるよう zoom_x / scroll_beat /
    /// track_row_h を自動調整する。clip 0 個なら song.length_beats でフォールバック。
    fn fit_arrange_to_content(&mut self) {
        let (canvas_w, canvas_h) = self.last_arrange_canvas_size;
        if canvas_w < 16.0 || canvas_h < 16.0 {
            return;
        }
        // 行高は widget が canvas_h に描く行数で割る: 常に先頭へ prepend される
        // master 行 (arrangement_view.rs が常時 `Some(&master_row)`) + 可視 track 数。
        // collapsed group 配下の子は widget 側 `is_visible_track` で描画されないので、
        // ここでも親 chain に collapsed があれば除外して同じ可視集合を数える
        // (`compute_track_depth` と同じ parent_group_id walk、 32 hop で cycle 安全)。
        let visible_track_count = self
            .song
            .tracks
            .iter()
            .filter(|t| {
                let mut cursor = t.parent_group_id;
                let mut hops = 0u8;
                while let Some(pid) = cursor {
                    if self.collapsed_groups.contains(&pid) {
                        return false;
                    }
                    hops += 1;
                    if hops > 32 {
                        break;
                    }
                    cursor = self.song.track_by_id(pid).and_then(|t| t.parent_group_id);
                }
                true
            })
            .count();
        // +1 は master 行 (widget が visible_tracks[0] に常時 prepend する)。
        let row_count = (visible_track_count + 1).max(1);

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
        let row_h = (canvas_h / row_count as f32).clamp(16.0, 96.0);
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
        // 新 clip 群を stable ClipKey (track.id + clip.id) で選択。
        self.selected_clips = new_refs
            .iter()
            .filter_map(|(t_idx, c_id)| {
                let track = self.song.tracks.get(*t_idx as usize)?;
                track
                    .clips
                    .iter()
                    .any(|c| c.id == *c_id)
                    .then_some(common::model::ClipKey {
                        track_id: track.id,
                        clip_id: *c_id,
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
    /// 未保存 project は `%LOCALAPPDATA%/daw_01/bounce_cache/<filename>.wav`
    /// (= `import_cache` と同じ fallback、 save 時に
    /// `migrate_unsaved_bounce_sources_into` が `<project_dir>/bounce/` へ
    /// 移動 + path を ProjectRelative 化する)。
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
        // 名前は content_id 単位 SSoT から (legacy clip.name は v20 で空)。
        let clip_name = self.song.content_name(clip.content_id).to_string();
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
        let safe_name: String = clip_name
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
                // 未保存 project: user bounce_cache に一時書き出し。
                // save 時に `migrate_unsaved_bounce_sources_into` で
                // `<project_dir>/bounce/` へ移動 + ProjectRelative 化される。
                let cache = import_audio::unsaved_bounce_cache_dir();
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
            clip_name,
            out_path.display()
        );
    }

    /// PR-C: plugin chain 込みで render し、 結果を **新 track + 新 Clip**
    /// に配置 (`docs/plan_audio_followup.md` PR-C / `docs/plan_audio_clip
    /// .md` §3.8 "Bounce")。 Bounce In Place (Pre-FX) と異なり async (=
    /// IPC 経由で freewheel render 完了通知待ち)。 完了通知の handler
    /// (`handle_bounce_clip_fx_complete`) 内で Undo snapshot を 1 回だけ
    /// 取る。 既に bounce 進行中なら重複 request を拒否。
    fn bounce_clip_with_fx(&mut self, target: ClipRef) {
        if self.pending_clip_fx_bounce.is_some() {
            self.status_message =
                "Bounce (with FX): 既に bounce 中です。 完了をお待ちください".into();
            return;
        }
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get(target.clip as usize).cloned() else {
            return;
        };
        // 名前は content_id 単位 SSoT から (legacy clip.name は v20 で空)。
        let clip_name = self.song.content_name(clip.content_id).to_string();
        let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get(&clip.content_id).cloned()
        else {
            self.status_message = "Bounce (with FX): audio clip ではありません".into();
            return;
        };
        if audio.events.is_empty() {
            self.status_message = "Bounce (with FX): events が空です".into();
            return;
        }

        let engine_sr = common::audio_bridge::SAMPLE_RATE;
        let bpm = self.song.bpm.max(1.0) as f64;
        let samples_per_beat = engine_sr as f64 * 60.0 / bpm;
        let start_frame = (clip.start_beat * samples_per_beat).max(0.0) as u64;
        let end_frame = ((clip.start_beat + clip.length_beats) * samples_per_beat)
            .max(0.0) as u64;
        if end_frame <= start_frame {
            self.status_message = "Bounce (with FX): clip 長が 0 です".into();
            return;
        }

        // 出力 path を決定。 logic は bounce_clip_in_place と同じだが
        // suffix を `_fx_` にして区別 (= 同 clip を Pre-FX / FX 両方
        // bounce しても上書きにならない)。
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64 % 100_000_000)
            .unwrap_or(0);
        let safe_name: String = clip_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe_name = if safe_name.is_empty() {
            "bounce".into()
        } else {
            safe_name
        };
        let filename = format!("{safe_name}_fx_{ts:08}.wav");

        let project_dir = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
        let (out_path, source_path) = match project_dir.as_deref() {
            Some(dir) => {
                let bounce_dir = dir.join("bounce");
                if let Err(e) = std::fs::create_dir_all(&bounce_dir) {
                    self.status_message =
                        format!("Bounce (with FX): bounce/ 作成失敗: {e}");
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
                // 未保存 project: bounce_cache に一時書き出し。 save 時に
                // `migrate_unsaved_bounce_sources_into` で `<project_dir>
                // /bounce/` へ移動 + ProjectRelative 化される。
                let cache = import_audio::unsaved_bounce_cache_dir();
                if let Err(e) = std::fs::create_dir_all(&cache) {
                    self.status_message =
                        format!("Bounce (with FX): bounce_cache/ 作成失敗: {e}");
                    return;
                }
                let dst = cache.join(&filename);
                (
                    dst.clone(),
                    common::model::AudioSourcePath::Absolute(dst.clone()),
                )
            }
        };

        // pending entry をセット。 完了通知 handler はこの entry を見て
        // 新 track / 新 clip を組み立てる。
        self.pending_clip_fx_bounce = Some(PendingClipFxBounce {
            source_track: target.track,
            source_clip: target.clip,
            out_path: out_path.clone(),
            source_path,
            clip_name: clip_name.clone(),
            clip_length_beats: clip.length_beats,
            start_beat: clip.start_beat,
        });

        // bookend: SetRenderMode(Offline) → LoadSong (= 最新 song
        // snapshot を audio engine に渡す) → BounceClipFxOnline。
        // ExportWav と同じ pattern。 完了通知で Realtime に戻す。
        let song = self.song.clone();
        self.send_audio(MainToChild::LoadSong(song));
        self.send_plugin(MainToChild::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        self.send_audio(MainToChild::BounceClipFxOnline {
            path: out_path,
            source_track: target.track,
            source_clip: target.clip,
            start_frame,
            end_frame,
        });
        self.status_message =
            format!("Bounce (with FX): '{}' を render 中...", clip_name);
    }

    /// PR-C: BounceClipFxOnline 完了通知の処理。 SetRenderMode(Realtime)
    /// で bookend 解除、 success なら新 audio source + 新 track + 新
    /// audio clip を配置 + Undo snapshot。 失敗時は status_message のみ
    /// (= pending クリア + 残骸ファイル削除)。
    fn handle_bounce_clip_fx_complete(
        &mut self,
        path: PathBuf,
        source_track: u32,
        source_clip: u32,
        error: Option<String>,
        frames: u64,
    ) {
        // bookend を Realtime に戻す (= 失敗時も忘れず)。
        self.send_plugin(MainToChild::SetRenderMode(
            common::protocol::RenderMode::Realtime,
        ));

        let Some(pending) = self.pending_clip_fx_bounce.take() else {
            tracing::warn!("BounceClipFxComplete with no pending bounce; ignoring");
            return;
        };
        if pending.source_track != source_track
            || pending.source_clip != source_clip
            || pending.out_path != path
        {
            tracing::warn!(
                ?path,
                source_track,
                source_clip,
                "BounceClipFxComplete identifier mismatch with pending; ignoring"
            );
            return;
        }
        if let Some(err) = error {
            self.status_message = format!("Bounce (with FX) 失敗: {err}");
            let _ = std::fs::remove_file(&path);
            return;
        }
        if frames == 0 {
            self.status_message =
                "Bounce (with FX): render 結果が空です (= silence のみ?)".into();
            let _ = std::fs::remove_file(&path);
            return;
        }

        // 1 完了 = 1 Undo step として snapshot を取る。
        self.push_undo_snapshot();

        let engine_sr = common::audio_bridge::SAMPLE_RATE;
        // 採番した new_source_id を `audio_sources` に登録。 path は
        // `pending.source_path` (= ProjectRelative or Absolute、 確定済)。
        let new_source = common::model::AudioSource {
            path: pending.source_path,
            sample_rate: engine_sr,
            channels: 2,
            frames,
            original_bpm: Some(self.song.bpm),
            root_key: None,
        };
        let new_source_id = self.song.alloc_audio_source_id();
        self.song.audio_sources.insert(new_source_id, new_source);

        // decode して audio_source_cache に登録 (= 即時再生で playback
        // できるよう)。 失敗しても tracker 表示等は問題ないので warn だけ。
        match crate::import_audio::decode_wav(&path) {
            Ok(buffer) => {
                self.audio_source_cache.insert(new_source_id, Arc::new(buffer));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "Bounce (with FX): WAV decode for cache failed (track is created; will reload on next save/load)"
                );
            }
        }

        // 新 track 作成 (空 plugin chain)。 名前は元 clip 名 + " (FX)"。
        let new_track_id = self.song.alloc_track_id();
        let new_track_name = format!("{} (FX)", pending.clip_name);
        let new_track = Track {
            id: new_track_id,
            name: new_track_name.clone(),
            clips: Vec::new(),
            ..Track::default()
        };
        self.song.tracks.push(new_track);
        let new_track_idx = self.song.tracks.len() - 1;

        // 新 Clip = single-event content (= bounce 結果は flat な audio)。
        let new_event = AudioEvent {
            source_id: new_source_id,
            event_start_in_clip_beats: 0.0,
            event_length_beats: pending.clip_length_beats,
            source_start_frames: 0,
            source_end_frames: frames,
            ..AudioEvent::default()
        };
        let new_content_id = self.song.alloc_content(
            common::model::ClipContent::Audio(common::model::AudioContent {
                events: vec![new_event],
            }),
            format!("{} (bounced FX)", pending.clip_name),
        );

        let new_track_mut = &mut self.song.tracks[new_track_idx];
        let new_clip_id = new_track_mut.alloc_clip_id();
        new_track_mut.clips.push(common::model::Clip {
            id: new_clip_id,
            name: String::new(),
            start_beat: pending.start_beat,
            length_beats: pending.clip_length_beats,
            content_id: new_content_id,
            notes: Vec::new(),
            color: None,
            auto_lipsync: false,
        });

        self.resize_track_peak_display();
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        self.status_message = format!(
            "Bounce (with FX) 完了: 新トラック '{}' を追加",
            new_track_name
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

    /// FIXME #15: audio inspector の数値 field は scrubable_number 化され
    /// 現値を summary から直接読むため、 専用 edit buffer は撤去。 この関数
    /// は text section と共有する `clip_edit_buffer_target` を current audio
    /// clip に同期する純 marker (= 多数の audio 編集パス / song 差し替えから
    /// 呼ばれる)。 target が audio clip を解決できなければ `None` 化する。
    fn resync_clip_audio_event_edit_buffers(&mut self, target: ClipRef) {
        let resolved = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song.clip_contents.get(&c.content_id))
            .is_some_and(|content| matches!(content, common::model::ClipContent::Audio(_)));
        self.clip_edit_buffer_target = if resolved { Some(target) } else { None };
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

    // -------- Image event editors (`docs/plan_image_overlay.md` §4 P4) ----

    fn set_clip_image_event_x(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.x = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_y(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.y = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_w(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.w = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_h(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.h = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_opacity(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_image_events_in_clip(target, |e| e.opacity = value);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_rotation_radians(&mut self, target: ClipRef, value: f32) {
        // -π..=π で wrap して保存。 lane override 経由でも同じ wrap が
        // composite で適用される (= preview 表示は modulo 2π)。
        let two_pi = std::f32::consts::TAU;
        let wrapped =
            ((value + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI;
        self.mutate_image_events_in_clip(target, |e| e.rotation_radians = wrapped);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_muted(&mut self, target: ClipRef, muted: bool) {
        self.mutate_image_events_in_clip(target, |e| e.muted = muted);
    }

    /// docs/plan_text_overlay.md §4 P6: image と同 idiom の text event
    /// setter 群。 drag / inspector commit / lane override 経由のいずれも
    /// このパスで TextEvent.field を直接書く。
    fn mutate_text_events_in_clip<F>(&mut self, target: ClipRef, mut f: F) -> bool
    where
        F: FnMut(&mut common::model::TextEvent),
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
        if let Some(common::model::ClipContent::Text(t)) =
            self.song.clip_contents.get_mut(&content_id)
        {
            if t.events.is_empty() {
                return false;
            }
            for event in &mut t.events {
                f(event);
            }
            true
        } else {
            false
        }
    }

    fn set_clip_text_event_x(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.x = value);
    }

    fn set_clip_text_event_y(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.y = value);
    }

    fn set_clip_text_event_w(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.w = value);
    }

    fn set_clip_text_event_h(&mut self, target: ClipRef, value: f32) {
        let value = value.clamp(0.0, 1.0);
        self.mutate_text_events_in_clip(target, |e| e.h = value);
    }

    fn set_clip_text_event_rotation_radians(&mut self, target: ClipRef, value: f32) {
        let two_pi = std::f32::consts::TAU;
        let wrapped =
            ((value + std::f32::consts::PI).rem_euclid(two_pi)) - std::f32::consts::PI;
        self.mutate_text_events_in_clip(target, |e| e.rotation_radians = wrapped);
    }

    fn set_clip_text_event_muted(&mut self, target: ClipRef, muted: bool) {
        self.mutate_text_events_in_clip(target, |e| e.muted = muted);
    }

    fn set_clip_text_event_content(&mut self, target: ClipRef, value: String) {
        // 単一行 text のみ (`plan_text_overlay.md` §1.1)、 '\n' は除外。
        let value = value.replace(['\n', '\r'], " ");
        if self.mutate_text_events_in_clip(target, |e| e.text = value.clone()) {
            // Text 編集は plugin host へ sync しない経路なので、 ここで明示的に
            // dirty を立てる (= 未保存変更ありとしてタイトルに '*' / autosave 対象)。
            // 表示名は content から導出するので content_name は触らない
            // (デフォルト無名のまま、 clip_display_label が本文を表示)。
            self.is_dirty = true;
        }
        self.resync_clip_text_event_edit_buffers(target);
    }

    fn set_clip_text_event_font_family(&mut self, target: ClipRef, value: String) {
        self.mutate_text_events_in_clip(target, |e| e.font_family = value.clone());
        self.resync_clip_text_event_edit_buffers(target);
    }

    fn set_clip_text_event_align(&mut self, target: ClipRef, value: common::model::TextAlign) {
        self.mutate_text_events_in_clip(target, |e| e.align = value);
    }

    fn set_clip_text_event_fade_in_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_text_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    fn set_clip_text_event_fade_out_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_text_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    /// docs/plan_text_overlay.md §4 P5: 23 numeric field + 2 fade beats
    /// を 1 関数で dispatch。 各 field の clamp / wrap rule を inline 適用。
    /// X/Y/W/H/Rotation は P6 drag 経路の setter を流用して double-define
    /// を回避。
    fn set_clip_text_num_field(
        &mut self,
        target: ClipRef,
        field: TextNumField,
        value: f32,
    ) {
        use TextNumField as F;
        match field {
            F::X => self.set_clip_text_event_x(target, value),
            F::Y => self.set_clip_text_event_y(target, value),
            F::W => self.set_clip_text_event_w(target, value),
            F::H => self.set_clip_text_event_h(target, value),
            F::Rotation => self.set_clip_text_event_rotation_radians(target, value),
            F::FontSize => {
                let v = value.max(1.0);
                self.mutate_text_events_in_clip(target, |e| e.font_size_px = v);
            }
            F::Opacity => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.opacity = v);
            }
            F::FillR => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[0] = v);
            }
            F::FillG => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[1] = v);
            }
            F::FillB => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[2] = v);
            }
            F::FillA => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.fill_color[3] = v);
            }
            F::OutlineR => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[0] = v);
            }
            F::OutlineG => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[1] = v);
            }
            F::OutlineB => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[2] = v);
            }
            F::OutlineA => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_color[3] = v);
            }
            F::OutlineWidth => {
                let v = value.max(0.0);
                self.mutate_text_events_in_clip(target, |e| e.outline_width_px = v);
            }
            F::ShadowR => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[0] = v);
            }
            F::ShadowG => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[1] = v);
            }
            F::ShadowB => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[2] = v);
            }
            F::ShadowA => {
                let v = value.clamp(0.0, 1.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_color[3] = v);
            }
            F::ShadowOffsetX => {
                self.mutate_text_events_in_clip(target, |e| e.shadow_offset_px.0 = value);
            }
            F::ShadowOffsetY => {
                self.mutate_text_events_in_clip(target, |e| e.shadow_offset_px.1 = value);
            }
            F::ShadowBlur => {
                let v = value.max(0.0);
                self.mutate_text_events_in_clip(target, |e| e.shadow_blur_px = v);
            }
            F::FadeInBeats => {
                let max_beats = self.clip_length_beats(target).unwrap_or(0.0);
                let v = (f64::from(value)).clamp(0.0, max_beats);
                self.mutate_text_events_in_clip(target, |e| e.fade_in_beats = v);
            }
            F::FadeOutBeats => {
                let max_beats = self.clip_length_beats(target).unwrap_or(0.0);
                let v = (f64::from(value)).clamp(0.0, max_beats);
                self.mutate_text_events_in_clip(target, |e| e.fade_out_beats = v);
            }
        }
        self.resync_clip_text_event_edit_buffers(target);
    }

    fn commit_clip_text_content_edit(&mut self) {
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let value = self.clip_text_content_edit_text.clone();
        self.set_clip_text_event_content(target, value);
    }

    fn commit_clip_text_font_family_edit(&mut self) {
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let value = self.clip_text_font_family_edit_text.clone();
        self.set_clip_text_event_font_family(target, value);
    }

    // -------- Font picker (FIXME #25) -------------------------------------

    /// 編集対象 text クリップの現在のフォント名 (先頭 event)。text クリップで
    /// なければ `None`。
    fn clip_text_font_family(&self, target: ClipRef) -> Option<String> {
        self.song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song.clip_contents.get(&c.content_id))
            .and_then(|content| content.text_events())
            .and_then(|events| events.first())
            .map(|e| e.font_family.clone())
    }

    fn open_font_picker(&mut self) {
        // anchor が text クリップのときだけ開く (Font ボタンは text inspector に
        // しか出ないが防衛的に確認)。
        let Some(target) = self.selected_clip_ref() else {
            return;
        };
        let Some(original) = self.clip_text_font_family(target) else {
            return;
        };
        self.font_picker_target = Some(target);
        self.font_picker_restore = original;
        self.font_picker_query.clear();
        self.font_picker_cursor = 0;
        self.is_font_picker_open = true;
        self.refresh_font_picker_visible();
        // システムフォント列挙は重い (~20-860ms) ので background で 1 度だけ。
        if self.font_picker_families.is_empty() && !self.font_picker_loading {
            self.begin_font_load();
        }
    }

    fn begin_font_load(&mut self) {
        self.font_picker_loading = true;
        let proxy = self.event_proxy.clone();
        std::thread::spawn(move || {
            let families = daw_ui_core::available_font_families();
            proxy.send(AppEvent::FontFamiliesLoaded(families));
        });
    }

    fn on_font_families_loaded(&mut self, families: Vec<String>) {
        self.font_picker_families = families;
        self.font_picker_loading = false;
        self.refresh_font_picker_visible();
    }

    fn refresh_font_picker_visible(&mut self) {
        let query = self.font_picker_query.trim();
        let mut visible: Vec<String> = Vec::new();
        // query が空のときだけ先頭に「デフォルト」行 (`""`) を出す。
        if query.is_empty() {
            visible.push(String::new());
            visible.extend(self.font_picker_families.iter().cloned());
        } else {
            visible.extend(
                self.font_picker_families
                    .iter()
                    .filter(|f| crate::fuzzy::subsequence_match(f, query))
                    .cloned(),
            );
        }
        self.font_picker_visible = visible;
        self.font_picker_cursor = 0;
    }

    fn move_font_picker_cursor(&mut self, delta: i32) {
        let len = self.font_picker_visible.len();
        if len == 0 {
            return;
        }
        self.font_picker_cursor =
            (self.font_picker_cursor as i32 + delta).clamp(0, len as i32 - 1) as usize;
        self.preview_font_at_cursor();
    }

    fn hover_font_in_picker(&mut self, idx: usize) {
        // 既に cursor がそこなら no-op (= hover 中の毎フレーム連発を抑止)。
        if idx >= self.font_picker_visible.len() || self.font_picker_cursor == idx {
            return;
        }
        self.font_picker_cursor = idx;
        self.preview_font_at_cursor();
    }

    /// cursor 位置のフォントを編集対象クリップへライブ適用 (非 undo / 非 dirty)。
    /// `""` = renderer default。
    fn preview_font_at_cursor(&mut self) {
        let Some(target) = self.font_picker_target else {
            return;
        };
        let Some(family) = self.font_picker_visible.get(self.font_picker_cursor).cloned() else {
            return;
        };
        self.set_clip_text_event_font_family(target, family);
    }

    fn commit_font_from_picker(&mut self, family: String) {
        let Some(target) = self.font_picker_target else {
            return;
        };
        // 元 → 選択 を 1 undo step にするため、 一旦元へ戻してから snapshot し
        // (= undo 先 = 元フォント)、 選択フォントを適用する (preview で既に選択
        // 値になっていても結果は同じ)。
        self.set_clip_text_event_font_family(target, self.font_picker_restore.clone());
        self.push_undo_snapshot();
        self.set_clip_text_event_font_family(target, family);
        self.is_dirty = true;
        // commit 経路では close_font_picker (on_close) の restore を no-op 化する
        // ため target を先に落とす。
        self.font_picker_target = None;
        self.is_font_picker_open = false;
    }

    fn close_font_picker(&mut self) {
        // cancel: preview で変えた font を元へ戻す。commit 済みなら target は
        // None なので no-op。
        if let Some(target) = self.font_picker_target {
            self.set_clip_text_event_font_family(target, self.font_picker_restore.clone());
        }
        self.is_font_picker_open = false;
        self.font_picker_target = None;
    }

    /// docs/plan_text_overlay.md §4 P5: clip 切替 / Undo / Redo / lane
    /// override 変化等で文字列 edit buffer (content / font_family) を current
    /// TextEvent の値で再構築。 FIXME #15: 25 numeric field は scrubable_number
    /// 化され現値を summary から直接読むため、 数値 buffer の再生成は不要に
    /// なった。 target が Text variant でないなら文字列 buffer を空にして
    /// `clip_edit_buffer_target` を `None`。
    fn resync_clip_text_event_edit_buffers(&mut self, target: ClipRef) {
        let event_snapshot = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song.clip_contents.get(&c.content_id))
            .and_then(|content| content.text_events())
            .and_then(|events| events.first())
            .cloned();
        let Some(ev) = event_snapshot else {
            self.clip_text_content_edit_text.clear();
            self.clip_text_font_family_edit_text.clear();
            self.clip_edit_buffer_target = None;
            return;
        };
        self.clip_text_content_edit_text = ev.text.clone();
        self.clip_text_font_family_edit_text = ev.font_family.clone();
        self.clip_edit_buffer_target = Some(target);
    }

    /// docs/plan_text_overlay.md §4 P5: text inspector が表示する
    /// snapshot (= image idiom)。 selected_clip が Text variant の clip
    /// を指していて、 first event があれば `Some` を返す。 各 numeric
    /// field の `*_automated` は対応する TextBuiltin lane が track に
    /// 存在するか。
    pub fn inspector_text_event_summary(&self) -> Option<InspectorTextEventSummary> {
        let cref = self.selected_clip_ref()?;
        let track = self.song.tracks.get(cref.track as usize)?;
        let clip = track.clips.get(cref.clip as usize)?;
        let common::model::ClipContent::Text(t) =
            self.song.clip_contents.get(&clip.content_id)?
        else {
            return None;
        };
        let event = t.events.first()?;
        let mut automated = std::collections::HashSet::new();
        for lane in &track.automation_lanes {
            if let common::model::AutomationTarget::TextBuiltin(p) = lane.target {
                automated.insert(p);
            }
        }
        Some(InspectorTextEventSummary {
            target: cref,
            muted: event.muted,
            align: event.align,
            fade_in_curve: event.fade_in_curve,
            fade_out_curve: event.fade_out_curve,
            automated,
            event: event.clone(),
            clip_length_beats: clip.length_beats,
        })
    }

    fn set_clip_image_event_fade_in_beats(&mut self, target: ClipRef, beats: f64) {
        let max_beats = self.clip_length_beats(target).unwrap_or(0.0);
        let beats = beats.clamp(0.0, max_beats);
        self.mutate_image_events_in_clip(target, |e| e.fade_in_beats = beats);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_fade_out_beats(&mut self, target: ClipRef, beats: f64) {
        let max_beats = self.clip_length_beats(target).unwrap_or(0.0);
        let beats = beats.clamp(0.0, max_beats);
        self.mutate_image_events_in_clip(target, |e| e.fade_out_beats = beats);
        self.resync_clip_image_event_edit_buffers(target);
    }

    fn set_clip_image_event_fade_in_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_image_events_in_clip(target, |e| e.fade_in_curve = curve);
    }

    fn set_clip_image_event_fade_out_curve(
        &mut self,
        target: ClipRef,
        curve: common::model::FadeCurve,
    ) {
        self.mutate_image_events_in_clip(target, |e| e.fade_out_curve = curve);
    }

    /// FIXME #15: image inspector の数値 field は scrubable_number 化され
    /// 現値を summary から直接読むため、 専用 edit buffer は撤去。 この関数
    /// は text section と共有する `clip_edit_buffer_target` を current image
    /// clip に同期する純 marker (= image 編集パス各所から呼ばれる)。 target
    /// が image clip を解決できなければ `None` 化する。
    fn resync_clip_image_event_edit_buffers(&mut self, target: ClipRef) {
        let resolved = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .and_then(|c| self.song.clip_contents.get(&c.content_id))
            .is_some_and(|content| matches!(content, common::model::ClipContent::Image(_)));
        self.clip_edit_buffer_target = if resolved { Some(target) } else { None };
    }

    /// `target` が指す clip が `ClipContent::Image` か。 commit / fade /
    /// mute handler の kind dispatch で使う。 範囲外 / 別 variant は false。
    pub fn is_image_clip(&self, target: ClipRef) -> bool {
        let Some(track) = self.song.tracks.get(target.track as usize) else {
            return false;
        };
        let Some(clip) = track.clips.get(target.clip as usize) else {
            return false;
        };
        matches!(
            self.song.clip_contents.get(&clip.content_id),
            Some(common::model::ClipContent::Image(_))
        )
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
        // 別 clip を開くときは前 clip の選択 index は stale なので clear
        // (同 clip の再 open は選択を保持)。 index ベース選択は context が
        // 変わると意味を失う (= close / undo と同方針)。
        if self.audio_editor_clip != Some(target) {
            self.audio_editor_selected_events.clear();
        }
        self.audio_editor_clip = Some(target);
        self.bottom_panel = 1;
        // 開いた clip 全体を見せる初期 view (= 既存挙動と等価)。 wheel
        // scroll / Ctrl+wheel zoom で以降は view_start / view_len を変更。
        let len_beats = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map_or(0.0, |c| c.length_beats);
        self.audio_editor_view_start_beat = 0.0;
        self.audio_editor_view_len_beats = len_beats.max(0.0);
    }

    fn close_audio_editor(&mut self) {
        self.audio_editor_clip = None;
        self.audio_editor_selected_events.clear();
        self.audio_editor_hover_beat_in_clip = None;
        self.audio_editor_view_start_beat = 0.0;
        self.audio_editor_view_len_beats = 0.0;
    }

    /// Audio Editor 水平 scroll: `view_start_beat` を `[0, total - view_len]`
    /// で clamp。 `audio_editor_clip` が None / clip が解決できない場合は no-op。
    fn set_audio_editor_scroll(&mut self, new_start: f64) {
        let Some(target) = self.audio_editor_clip else { return };
        let Some(clip) = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
        else {
            return;
        };
        let total = clip.length_beats.max(0.0);
        let view_len = self.audio_editor_view_len_beats.max(0.0).min(total);
        let max_start = (total - view_len).max(0.0);
        self.audio_editor_view_start_beat = new_start.clamp(0.0, max_start);
    }

    /// Audio Editor zoom: `view_start_beat` + `view_len_beats` を一括設定。
    /// `view_len` は `[MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, clip.length]`、
    /// `view_start` は `[0, clip.length - view_len]` で clamp。
    fn set_audio_editor_zoom(&mut self, new_start: f64, new_len: f64) {
        let Some(target) = self.audio_editor_clip else { return };
        let Some(clip) = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
        else {
            return;
        };
        let total = clip.length_beats.max(0.0);
        let len = new_len.clamp(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS, total.max(MIN_AUDIO_EDITOR_VIEW_LEN_BEATS));
        let max_start = (total - len).max(0.0);
        self.audio_editor_view_start_beat = new_start.clamp(0.0, max_start);
        self.audio_editor_view_len_beats = len;
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
        let Some(idx) = self.audio_editor_anchor_event() else {
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
        self.audio_editor_selected_events = vec![insert_at];
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
        self.audio_editor_selected_events = vec![new_idx];
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        self.status_message = format!("Audio event 追加: {display_name}");
    }

    /// Audio Editor の event 選択集合を `indices` で置き換える。 重複を
    /// 除いて格納 (anchor = last なので最後に追加された index が代表)。
    /// 範囲外 index は use 時に `.get` で無視されるのでここでは除外しない
    /// (= n_events を知るための再 resolve を避ける)。 view state、 非 undoable。
    fn set_audio_editor_event_selection(&mut self, indices: Vec<usize>) {
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<usize> = indices.into_iter().filter(|i| seen.insert(*i)).collect();
        self.audio_editor_selected_events = deduped;
    }

    /// Audio Editor で選択中の全 event を削除 (= Delete key、 複数選択
    /// 対応)。 高い index から `remove` して shift を回避。 削除後は
    /// selection を clear。 events が空になっても content は保持。
    fn delete_audio_editor_selection(&mut self) {
        let Some(target) = self.audio_editor_clip else {
            return;
        };
        let Some(content_id) = self
            .song
            .tracks
            .get(target.track as usize)
            .and_then(|t| t.clips.get(target.clip as usize))
            .map(|c| c.content_id)
        else {
            return;
        };
        let mut indices: Vec<usize> = self.audio_editor_selected_events.clone();
        indices.sort_unstable();
        indices.dedup();
        if indices.is_empty() {
            return;
        }
        if let Some(common::model::ClipContent::Audio(audio)) =
            self.song.clip_contents.get_mut(&content_id)
        {
            for &i in indices.iter().rev() {
                if i < audio.events.len() {
                    audio.events.remove(i);
                }
            }
        } else {
            return;
        }
        self.audio_editor_selected_events.clear();
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
            self.selected_clip_ref().into_iter().collect()
        } else {
            self.selected_clip_refs()
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
            self.selected_clip_ref().into_iter().collect()
        } else {
            self.selected_clip_refs()
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

        // FIXME #6: overlay clip (image / video / text) は「clip 長 = 表示長」が
        // 不変条件。 上の Audio 分岐は overlay event を触らないので、 clip を
        // 伸ばしただけだと event 長が据え置かれ、 clip 範囲内でも event 範囲を
        // 抜けて途中で消える。 当該 content の単一/末尾 event を新 clip 長まで
        // extend する (extend-only / idempotent / linked clip 安全、 load 修復の
        // `ensure_overlay_event_coverage` と同一ロジック)。
        if let Some(content) = self.song.clip_contents.get_mut(&content_id) {
            content.ensure_event_covers_clip(new_length_beats);
        }

        self.sync_song_to_plugin_host();
    }

    /// 共有コピー (D shortcut): 末尾直後 (start+length) に同サイズの clip を
    /// 1 つ生成、 `content_id` を流用。 `docs/plan_clip_share_clone.md` §3.2。
    /// 選択 clip 群の bounding span (`max_end - min_start`)。 複製を選択ブロック
    /// 直後に並べるためのオフセット (相対位置を保ったままブロック複製)。 単一
    /// clip では clip 長と一致する (= 旧 single duplicate と同挙動)。 解決でき
    /// ない stale ref は無視、 有効 clip が 1 つも無ければ `None`。
    fn clip_block_span(&self, sources: &[ClipRef]) -> Option<f64> {
        let mut min_start = f64::MAX;
        let mut max_end = f64::MIN;
        for &src in sources {
            let Some(clip) = self
                .song
                .tracks
                .get(src.track as usize)
                .and_then(|t| t.clips.get(src.clip as usize))
            else {
                continue;
            };
            min_start = min_start.min(clip.start_beat);
            max_end = max_end.max(clip.start_beat + clip.length_beats);
        }
        (max_end >= min_start).then_some(max_end - min_start)
    }

    /// `source` の共有コピーを `new_start_beat` に 1 つ生成し、 新 `ClipRef` を
    /// 返す (選択・sync は呼び出し側)。 同 `content_id` を流用 → 名前 (content_id
    /// 単位 SSoT) も共有、 色 (per-clip) は source 引き継ぎ。
    fn duplicate_one_clip_shared_at(
        &mut self,
        source: ClipRef,
        new_start_beat: f64,
    ) -> Option<ClipRef> {
        let src_clip = self
            .song
            .tracks
            .get(source.track as usize)?
            .clips
            .get(source.clip as usize)?;
        let new_length = src_clip.length_beats;
        let content_id = src_clip.content_id;
        let src_color = src_clip.color;
        let track = self.song.tracks.get_mut(source.track as usize)?;
        let new_clip_id = track.alloc_clip_id();
        let new_idx = track.clips.len() as u32;
        track.clips.push(Clip {
            id: new_clip_id,
            name: String::new(),
            start_beat: new_start_beat,
            length_beats: new_length,
            content_id,
            notes: Vec::new(),
            color: src_color,
            auto_lipsync: false,
        });
        Some(ClipRef { track: source.track, clip: new_idx })
    }

    /// `source` の独立コピー (content を deep clone + 新 ContentId 採番) を
    /// `new_start_beat` に 1 つ生成し、 新 `ClipRef` を返す。 §3.3。
    fn duplicate_one_clip_unique_at(
        &mut self,
        source: ClipRef,
        new_start_beat: f64,
    ) -> Option<ClipRef> {
        let src_clip = self
            .song
            .tracks
            .get(source.track as usize)?
            .clips
            .get(source.clip as usize)?;
        let new_length = src_clip.length_beats;
        let src_content_id = src_clip.content_id;
        let src_color = src_clip.color;
        let new_content_id = self.song.fork_content(src_content_id);
        let track = self.song.tracks.get_mut(source.track as usize)?;
        let new_clip_id = track.alloc_clip_id();
        let new_idx = track.clips.len() as u32;
        track.clips.push(Clip {
            id: new_clip_id,
            name: String::new(),
            start_beat: new_start_beat,
            length_beats: new_length,
            content_id: new_content_id,
            notes: Vec::new(),
            color: src_color,
            auto_lipsync: false,
        });
        Some(ClipRef { track: source.track, clip: new_idx })
    }

    /// FIXME #21: 選択 clip 群をまとめて共有複製 (D shortcut)。 選択ブロック span
    /// だけ後ろにずらして相対位置を保ったまま複製し (Ctrl+drag と同じセマンティ
    /// クス)、 複製群を選択にする。 D 連打で後方連鎖する。
    fn duplicate_clips_shared(&mut self, sources: &[ClipRef]) {
        let Some(offset) = self.clip_block_span(sources) else {
            return;
        };
        let mut new_refs = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song
                .tracks
                .get(src.track as usize)
                .and_then(|t| t.clips.get(src.clip as usize))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(r) = self.duplicate_one_clip_shared_at(src, new_start) {
                new_refs.push(r);
            }
        }
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
            self.selected_notes.clear();
            self.sync_song_to_plugin_host();
        }
    }

    /// FIXME #21: 選択 clip 群をまとめて独立複製 (Alt+D shortcut)。 配置・選択は
    /// `duplicate_clips_shared` と同じ、 各 clip の content を独立化する点が違う。
    fn duplicate_clips_unique(&mut self, sources: &[ClipRef]) {
        let Some(offset) = self.clip_block_span(sources) else {
            return;
        };
        let mut new_refs = Vec::with_capacity(sources.len());
        for &src in sources {
            let Some(new_start) = self
                .song
                .tracks
                .get(src.track as usize)
                .and_then(|t| t.clips.get(src.clip as usize))
                .map(|c| c.start_beat + offset)
            else {
                continue;
            };
            if let Some(r) = self.duplicate_one_clip_unique_at(src, new_start) {
                new_refs.push(r);
            }
        }
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
            self.selected_notes.clear();
            self.sync_song_to_plugin_host();
        }
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
            // 共有コピー: content_id 流用 → 名前も自動共有。色 (per-clip) は
            // source の色を引き継ぐ。
            let content_id = src_clip.content_id;
            let src_color = src_clip.color;
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
                name: String::new(),
                start_beat: drop_start.max(0.0),
                length_beats: new_length,
                content_id,
                notes: Vec::new(),
                color: src_color,
                auto_lipsync: false,
            });
            new_refs.push(ClipRef {
                track: to_track_idx as u32,
                clip: new_idx,
            });
        }
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
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
            // 独立コピー: content + 名前を fork。色 (per-clip) は source の色を引き継ぐ。
            let src_content_id = src_clip.content_id;
            let src_color = src_clip.color;
            let new_content_id = self.song.fork_content(src_content_id);
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
                name: String::new(),
                start_beat: drop_start.max(0.0),
                length_beats: new_length,
                content_id: new_content_id,
                notes: Vec::new(),
                color: src_color,
                auto_lipsync: false,
            });
            new_refs.push(ClipRef {
                track: to_track_idx as u32,
                clip: new_idx,
            });
        }
        if !new_refs.is_empty() {
            self.select_new_clips(&new_refs);
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
        // content + 名前を fork して独立化 (fork 時点の名前を引き継ぐ)。
        let new_content_id = self.song.fork_content(content_id);
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
        track.clips.push(Clip {
            id: new_clip_id,
            name: String::new(),
            start_beat,
            length_beats: DEFAULT_CLIP_LENGTH,
            content_id,
            notes: Vec::new(),
            color: None,
            auto_lipsync: false,
        });
        // デフォルトでクリップ名は無し (= content_name 未設定)。 表示名は
        // arrangement_view::clip_display_label が内容 (Text 本文 / ノート歌詞)
        // から導出する。 ユーザーが Rename したときだけ明示名が入る。
        let r = ClipRef {
            track: track_idx,
            clip: new_idx,
        };
        self.set_single_clip_selection(r);
        self.selected_notes.clear();
        self.select_track(track_idx);
        self.sync_song_to_plugin_host();
    }

    fn delete_selected_clip(&mut self) {
        if self.selected_clips.is_empty() {
            return;
        }
        // ClipKey → 現在の index ClipRef に解決し、 同 track 内は高 clip index
        // から remove して shift を回避する。
        let mut targets: Vec<ClipRef> = self.selected_clip_refs();
        self.selected_clips.clear();
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

    // -------- Phase 7 B5 (`docs/plan_scale.html`): Scale operations -------

    /// Transport bar の root / scale dropdown commit handler。
    /// `scale_changes` が空なら beat=0 で新規追加、 そうでなければ
    /// `scale_at(playhead)` で見つかる event を update。 plan §4.1 と一致。
    fn set_scale_at_playhead(&mut self, root: u8, scale: common::scale::Scale) {
        let playhead = self
            .playhead_beat
            .map(f64::from)
            .unwrap_or(0.0)
            .max(0.0);
        let root = root.min(11);
        if self.song.scale_changes.is_empty() {
            self.song.scale_changes.push(common::scale::ScaleChange {
                beat: 0.0,
                root,
                scale,
            });
            return;
        }
        // `scale_at` の semantics に合わせて「playhead 以下の最新 event」
        // を update。 playhead 未満の event が無ければ最初の event を update
        // (Cubase Transport の Chord Track edit と同じ idiom)。
        let target_idx = self
            .song
            .scale_changes
            .iter()
            .rposition(|c| c.beat <= playhead)
            .unwrap_or(0);
        if let Some(ev) = self.song.scale_changes.get_mut(target_idx) {
            ev.root = root;
            ev.scale = scale;
        }
    }

    /// `selected_clip` の note の pitch を最寄り in-scale に一括補正。
    /// 各 note の start_beat 時点の scale を尊重 (転調をまたぐ note は
    /// それぞれの local scale で snap される)。
    fn quantize_pitches_to_scale(&mut self, target: QuantizePitchTarget) {
        let Some(r) = self.selected_clip_ref() else {
            return;
        };
        if self.song.scale_changes.is_empty() {
            self.status_message =
                "Scale が設定されていません (Transport bar の Key dropdown で設定)".to_string();
            return;
        }
        let Some(track) = self.song.tracks.get(r.track as usize) else {
            return;
        };
        let Some(clip) = track.clips.get(r.clip as usize) else {
            return;
        };
        let clip_start_beat = clip.start_beat;
        // immutable borrow で snap 計算を済ませてから可変借用に切り替える
        // (= borrow checker 衝突回避)。 `Song::clip_notes` は `Clip` を経由する
        // shared note 取得 helper、 mutable 版は `notes_in_clip_mut`。
        let snaps: Vec<(u32, u8)> = {
            let notes = self.song.clip_notes(clip);
            let target_indices: Vec<u32> = match target {
                QuantizePitchTarget::SelectedNotes => self.selected_notes.clone(),
                QuantizePitchTarget::SelectedClipAllNotes => {
                    (0..notes.len() as u32).collect()
                }
            };
            target_indices
                .iter()
                .filter_map(|&i| {
                    let n = notes.get(i as usize)?;
                    let global_beat = clip_start_beat + n.start_beat;
                    let new_pitch = self
                        .song
                        .scale_at(global_beat)
                        .map(|sc| sc.snap(n.pitch))
                        .unwrap_or(n.pitch);
                    if new_pitch != n.pitch {
                        Some((i, new_pitch))
                    } else {
                        None
                    }
                })
                .collect()
        };
        let count = snaps.len();
        if count == 0 {
            self.status_message =
                "対象 note は既に in-scale です".to_string();
            return;
        }
        if let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        {
            for (i, new_pitch) in snaps {
                if let Some(n) = notes.get_mut(i as usize) {
                    n.pitch = new_pitch;
                }
            }
        }
        self.status_message =
            format!("{count} 件の note を scale に補正しました");
        self.sync_song_to_plugin_host();    }

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
        // Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw。
        // scale_changes が空なら scale_at が None → unwrap_or で raw pitch
        // 維持 = 機能 OFF と同じ挙動。
        let pitch = if self.snap_on_draw {
            let clip_start_beat = self
                .song
                .tracks
                .get(track_idx as usize)
                .and_then(|t| t.clips.get(clip_idx as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0);
            let global_beat = clip_start_beat + start_beat;
            self.song
                .scale_at(global_beat)
                .map(|sc| sc.snap(pitch))
                .unwrap_or(pitch)
        } else {
            pitch
        };
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
        if let Some(key) = self.clip_key_of(r) {
            self.selected_clip = Some(key);
            if !self.selected_clips.contains(&key) {
                self.selected_clips = vec![key];
            }
        }
        self.selected_notes = vec![new_idx];
        self.last_note_duration_beats = duration;
        self.sync_song_to_plugin_host();    }

    fn set_note_positions(&mut self, entries: &[(u32, f64, u8)]) {
        let Some(r) = self.selected_clip_ref() else {
            return;
        };
        // Phase 7 B5 (`docs/plan_scale.html` §5.1): Snap on Draw を note 移動
        // (y-drag で pitch 変更) にも適用。 borrow checker のため snap 計算は
        // immutable phase で済ませる。 Fold mode のときは widget が既に
        // in-scale pitch を push しているので idempotent (snap が no-op)。
        let snapped: Vec<(u32, f64, u8)> = if self.snap_on_draw {
            let clip_start_beat = self
                .song
                .tracks
                .get(r.track as usize)
                .and_then(|t| t.clips.get(r.clip as usize))
                .map(|c| c.start_beat)
                .unwrap_or(0.0);
            entries
                .iter()
                .map(|&(idx, beat, pitch)| {
                    let global_beat = clip_start_beat + beat.max(0.0);
                    let new_pitch = self
                        .song
                        .scale_at(global_beat)
                        .map(|sc| sc.snap(pitch))
                        .unwrap_or(pitch);
                    (idx, beat, new_pitch)
                })
                .collect()
        } else {
            entries.to_vec()
        };
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        for &(idx, beat, pitch) in &snapped {
            let Some(note) = notes.get_mut(idx as usize) else {
                continue;
            };
            note.start_beat = beat.max(0.0);
            note.pitch = pitch;
        }
        self.sync_song_to_plugin_host();    }

    fn resize_notes(&mut self, entries: &[(u32, f64, f64)]) {
        let Some(r) = self.selected_clip_ref() else {
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
        self.sync_song_to_plugin_host();    }

    /// ピアノロールで選択中ノート (`selected_notes`) を複製する (D キー)。
    /// 複製は選択範囲の beat span ぶん後ろにずらし、元ノートは据え置き、
    /// 複製を新しい選択にする (連打で後方へ連鎖)。selected_clip 無し /
    /// 選択空 / clip 解決失敗なら no-op。
    fn duplicate_selected_notes(&mut self) {
        let Some(r) = self.selected_clip_ref() else {
            return;
        };
        let selected: Vec<u32> = self.selected_notes.clone();
        if selected.is_empty() {
            return;
        }
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        let new_ids = duplicate_notes_into(notes, &selected);
        if new_ids.is_empty() {
            return;
        }
        self.selected_notes = new_ids;
        self.sync_song_to_plugin_host();    }

    /// gui_01 #054 (Ctrl+drag コピー): `entries` = [(source note index,
    /// new_start_beat, new_pitch)]。各 source を deep clone して指定位置へ配置し
    /// (元は据え置き)、複製を新選択にする。selected_clip 無し / 該当 index 無しなら no-op。
    fn copy_notes(&mut self, entries: &[(u32, f64, u8)]) {
        let Some(r) = self.selected_clip_ref() else {
            return;
        };
        let Some(notes) = self
            .song
            .notes_in_clip_mut(r.track as usize, r.clip as usize)
        else {
            return;
        };
        let new_ids = copy_notes_into(notes, entries);
        if new_ids.is_empty() {
            return;
        }
        self.selected_notes = new_ids;
        self.sync_song_to_plugin_host();    }

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
        self.sync_song_to_plugin_host();    }

    fn delete_selected_notes(&mut self) {
        let Some(r) = self.selected_clip_ref() else {
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
        self.sync_song_to_plugin_host();    }

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
            self.sync_song_to_plugin_host();        }
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

    // Args mirror the `SlotPluginLoadedFromChild` AppEvent (= the IPC
    // message's fields); bundling them into a struct would just shuffle the
    // same data, so allow the wide signature.
    #[allow(clippy::too_many_arguments)]
    fn on_plugin_loaded_from_child(
        &mut self,
        track_id: u32,
        slot: PluginSlot,
        id: String,
        _name: String,
        plugin_id: u32,
        shmem_id: String,
        // Phase 6 review (silent corruption fix): plugin_host が saved state
        // を `state_load(&bytes)` で適用しようとして失敗したときの理由。
        // `Some(reason)` のとき plugin は default 状態で chain に居る ⇒
        // ユーザーには「設定が復元されなかった」 ことを status_message で
        // 知らせて、 必要なら再 load / preset 適用してもらう。
        state_load_error: Option<String>,
    ) {
        // SSoT (code review 2026-06-06): audio engine に `ProcessData` shmem を
        // 開かせる。 incoming bridge の stale clone ではなく、 respawn で
        // 差し替わる live な `self.audio_tx` から送ることで、 audio respawn
        // 後にロードした plugin の音が出なくなる bug を防ぐ。
        self.send_audio(MainToChild::OpenPluginShmem {
            plugin_id,
            shmem_id,
            track: track_id,
            slot,
        });
        if let Some(reason) = state_load_error {
            let msg = format!(
                "Plugin state 復元失敗 (track {track_id} {slot:?}, id={id}): {reason}"
            );
            tracing::error!(track = track_id, ?slot, %id, %reason, "state_load failed (notified by plugin host)");
            self.status_message = msg;
        }
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
        // master bus は track Vec に居ないので master_fx_chain に reconcile する。
        // master は audio fx のみ (Fx slot) を持つ。 track 経路の sidechain 保存
        // idiom をそのまま適用 (= 既存 instance の state/format/sidechain を引き継ぐ)。
        // 以降の共通 finalize (sync / gui_open / pending_play flush) は master /
        // track どちらも通る (early return しない)。
        if track_id == common::model::MASTER_TRACK_ID {
            if let PluginSlot::Fx(i) = slot {
                let i = i as usize;
                let (existing_state, format, existing_sc) = self
                    .song
                    .master_fx_chain
                    .get(i)
                    .map(|p| (p.state.clone(), p.format, p.sidechain_sources.clone()))
                    .unwrap_or((None, PluginFormat::Clap, Vec::new()));
                let inst = common::model::PluginInstance {
                    plugin_id: id,
                    format,
                    state: existing_state,
                    sidechain_sources: existing_sc,
                };
                if i < self.song.master_fx_chain.len() {
                    self.song.master_fx_chain[i] = inst;
                } else {
                    self.song.master_fx_chain.push(inst);
                }
            }
        } else if let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) {
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
        } else {
            // track id が Vec に無い (load 中に track 削除された等)。 master でも
            // なく該当 track も居ないので、 従来どおり finalize せず early return。
            return;
        }

        // ユーザーが手動追加した plugin の load 完了 finalize:
        // (1) daw_audio へ LoadSong を再送して新 plugin を signal path に入れる。
        //     従来この add path だけ audio 再 sync が欠落しており、 save 等 次の
        //     sync_song_to_plugin_host まで signal に反映されなかった (= bug)。
        // (2) GUI 自動 open を frame loop に queue する。
        // pending_play flush より前に sync し、 Play 待ち再生も最新 schedule で開始させる。
        if self.pending_added_plugin_finalize.remove(&(track_id, slot)) {
            self.sync_song_to_plugin_host();
            self.gui_open_requests.push((track_id, slot));
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

        // PR-V3: builtin VOICEVOX が instrument に load されたら、 直後に
        // 歌詞 metadata を flush して背景 synth を trigger する。 plugin_id
        // が `loaded_slots` に登録された後でないと sync_vocal_metadata が
        // skip するため、 ここで明示呼び出し。 vocal でない track の load
        // も同 path を通るが、 sync_vocal_metadata 内で format check して
        // skip するので overhead は最小。
        if slot == PluginSlot::Instrument {
            self.sync_vocal_metadata();
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
        // load 失敗時は finalize 予約も取り消す (stale entry が後の project-load で
        // 誤 sync / 誤 open しないように)。
        self.pending_added_plugin_finalize.remove(&(track, slot));
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

    /// plugin_host が plugin destroy を完了した通知を受けて、 audio engine
    /// に `ClosePluginShmem` を送り (SSoT: live な `self.audio_tx` 経由)、
    /// `track_plugin_ids` 等の daw_gui ローカル状態をクリーンアップする。
    fn on_plugin_unloaded_from_child(&mut self, plugin_id: u32) {
        // SSoT (code review 2026-06-06): stale な incoming-bridge clone では
        // なく live audio_tx から送る。 respawn 後に dangling shmem 参照が
        // 残るのを防ぐ。
        self.send_audio(MainToChild::ClosePluginShmem { plugin_id });
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
        // PR2.1: plugin_host_windows / IPC は track_id ベース。 master 選択時は
        // cursor_track_id が MASTER_TRACK_ID を返す (Vec に居ないので index 経由
        // 不可)。
        let Some(track_id) = self.cursor_track_id() else {
            return;
        };
        // 既に開いていれば閉じる (toggle)。開いていなければ open_slot_gui で開く。
        #[cfg(windows)]
        {
            if self.plugin_host_windows.contains_key(&(track_id, slot)) {
                self.send_plugin(MainToChild::CloseSlotGui { track: track_id, slot });
                return;
            }
        }
        self.open_slot_gui(track_id, slot);
    }

    /// 指定 (track_id, slot) のプラグイン GUI を embedded container window で開く。
    /// 既に開いていれば何もしない (重複 open 防止)。Windows 専用 (他 OS では no-op)。
    /// `toggle_slot_gui` (手動トグル) と plugin 追加時の自動 open の両方から使う。
    fn open_slot_gui(&mut self, track_id: u32, slot: PluginSlot) {
        #[cfg(windows)]
        {
            if self.plugin_host_windows.contains_key(&(track_id, slot)) {
                return;
            }
            let label = if track_id == common::model::MASTER_TRACK_ID {
                match slot {
                    PluginSlot::Fx(i) => self
                        .song
                        .master_fx_chain
                        .get(i as usize)
                        .map(|p| format!("Master / {}", self.resolve_name(&p.plugin_id))),
                    _ => None,
                }
                .unwrap_or_else(|| "Master".into())
            } else {
                self.song
                    .tracks
                    .iter()
                    .find(|t| t.id == track_id)
                    .and_then(|t| self.slot_ref_name(t, slot))
                    .unwrap_or_else(|| "(unknown)".into())
            };
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
            let _ = (track_id, slot);
        }
    }

    /// runner の frame loop から毎フレーム呼ぶ。plugin 追加 → load 完了で queue された
    /// GUI auto-open 要求を処理する (実 window 生成 + `OpenSlotGuiEmbedded` 送出)。
    /// window 生成を handle_event ではなく frame loop に置くことで、frame loop を
    /// 回さない headless test では window を作らない。
    pub(crate) fn drain_pending_gui_opens(&mut self) {
        if self.gui_open_requests.is_empty() {
            return;
        }
        for (track_id, slot) in std::mem::take(&mut self.gui_open_requests) {
            self.open_slot_gui(track_id, slot);
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
    /// `order[i]` は新位置 i に来る旧 chain index。FIXME #29: 並べ替えは各プラグインの
    /// capability (ポート構成) で「ポート的に有効な配置」だけ許す。 dual-role は生成器位置
    /// ⇔ 音源位置を跨いで動かせ、 純粋な音源/MIDI FX/audio FX は自セクション内のみ。
    /// 無効な drop は early-return で no-op (UI は元の並びに戻る)。
    fn reorder_inspector_chain(&mut self, order: &[usize]) {
        let chain = self.inspector_chain();
        if chain.len() != order.len() {
            return;
        }
        // master bus は chain が master_fx_chain (FX only) なので順序だけ適用。
        // cross-section は起きない (全 slot_kind == 2)。
        if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
            let fx_count = self.song.master_fx_chain.len();
            if fx_count == order.len()
                && fx_count > 0
                && order.iter().all(|&o| o < fx_count)
            {
                let new_fx: Vec<_> = (0..fx_count)
                    .map(|new_i| self.song.master_fx_chain[order[new_i]].clone())
                    .collect();
                self.song.master_fx_chain = new_fx;
                self.sync_song_to_plugin_host();
            }
            return;
        }
        let Some(track_idx) = self.cursor_track_index() else {
            return;
        };
        // chain (= inspector_chain 順 [midi_fx..., inst?, fx...]) を flat な
        // PluginInstance 列にする。 これで old chain index ↔ PluginInstance が対応。
        let flat: Vec<common::model::PluginInstance> = {
            let Some(track) = self.song.tracks.get(track_idx) else {
                return;
            };
            let mut v = Vec::with_capacity(chain.len());
            v.extend(track.midi_fx_chain.iter().cloned());
            if let Some(inst) = track.instrument.as_ref() {
                v.push(inst.clone());
            }
            v.extend(track.fx_chain.iter().cloned());
            v
        };
        if flat.len() != chain.len() || order.iter().any(|&o| o >= flat.len()) {
            return;
        }
        // 各 flat index の capability を解決。 db 不在 / 未登録は prior slot_kind に
        // 縛り (従来の section 内 reorder と等価な安全側)、 既知プラグインは port 由来。
        let db = self.plugin_db.clone();
        let caps: Vec<PluginCapability> = (0..flat.len())
            .map(|i| {
                db.as_ref()
                    .and_then(|d| d.find_by_id(&flat[i].plugin_id))
                    .map(PluginCapability::from_entry)
                    .unwrap_or(match chain[i].slot_kind {
                        0 => PluginCapability::PureMidiEffect,
                        1 => PluginCapability::Instrument,
                        _ => PluginCapability::AudioFx,
                    })
            })
            .collect();
        // 音源 (= prior slot_kind 1) の新位置。 これを境に前=生成器(0) / 後=audio fx(2)、
        // その位置自身が音源(1)。 音源が無ければ生成器→audio fx の単調列。
        let inst_new_pos = (0..order.len()).find(|&i| chain[order[i]].slot_kind == 1);
        let mut new_kinds = vec![0u8; order.len()];
        let mut saw_fx = false;
        for i in 0..order.len() {
            let cap = caps[order[i]];
            let kind = match inst_new_pos {
                Some(p) if i < p => 0,
                Some(p) if i == p => 1,
                Some(_) => 2,
                None if cap == PluginCapability::AudioFx => 2,
                None => 0,
            };
            // ポート的に置けない位置への drop は棄却 (= UI は元の並びへ)。
            if !cap.allows_slot_kind(kind) {
                return;
            }
            // 音源の無い列では「生成器の後に audio fx」 の単調性だけ要求する。
            if inst_new_pos.is_none() {
                if kind == 2 {
                    saw_fx = true;
                } else if saw_fx {
                    return;
                }
            }
            new_kinds[i] = kind;
        }
        // 新順序 + new_kinds で 3 つの Vec へ再分割 (section 跨ぎ書き戻しを含む)。
        let mut new_midi = Vec::new();
        let mut new_inst = None;
        let mut new_fx = Vec::new();
        for i in 0..order.len() {
            let inst = flat[order[i]].clone();
            match new_kinds[i] {
                0 => new_midi.push(inst),
                1 => new_inst = Some(inst),
                _ => new_fx.push(inst),
            }
        }
        let Some(track) = self.song.tracks.get_mut(track_idx) else {
            return;
        };
        track.midi_fx_chain = new_midi;
        track.instrument = new_inst;
        track.fx_chain = new_fx;
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
        // master bus は track Vec に居ないので master_fx_chain (Fx slot のみ)
        // を対象にする。
        let inst = if track_id == common::model::MASTER_TRACK_ID {
            self.song.master_fx_chain.get_mut(slot_index as usize)
        } else {
            let Some(track) = self.song.track_by_id_mut(track_id) else {
                return;
            };
            match slot_kind {
                0 => track.midi_fx_chain.get_mut(slot_index as usize),
                1 => track.instrument.as_mut(),
                _ => track.fx_chain.get_mut(slot_index as usize),
            }
        };
        let Some(inst) = inst else { return };
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
        // master 選択時は cursor_track_id == MASTER_TRACK_ID (Vec に居ない)。
        let Some(track_id) = self.cursor_track_id() else {
            return;
        };

        if !self.song_has_plugin() {
            self.push_undo_snapshot();
            self.remove_slot_inner(track_id, slot);
            return;
        }
        self.enqueue_state_request(PendingStateRequest::Deferred(
            DeferredEdit::RemoveSlot { track_id, slot },
        ));
    }

    fn remove_slot_inner(&mut self, track_id: u32, slot: PluginSlot) {
        // master bus は track Vec に居ないので master_fx_chain から削除する。
        // host への RemoveSlotPlugin + GUI cleanup + slot cache 削除は track と
        // 共通 idiom。 master は Fx slot のみ。
        if track_id == common::model::MASTER_TRACK_ID {
            self.send_plugin(MainToChild::RemoveSlotPlugin {
                track: track_id,
                slot,
            });
            self.cleanup_slot_gui(track_id, slot);
            self.loaded_slots.remove(&(track_id, slot));
            if let PluginSlot::Fx(i) = slot {
                let i = i as usize;
                if i < self.song.master_fx_chain.len() {
                    self.song.master_fx_chain.remove(i);
                }
            }
            return;
        }
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
                PluginSlot::Instrument => {
                    track.instrument = None;
                    // instrument を外したら vocal 状態も解除 (vocal性は instrument 追従)。
                    track.source = InstrumentSource::None;
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
    /// state を Song に書き戻したあと、 [`AppData::pending_state_queue`]
    /// の front を取り出して完了処理 (save または deferred edit) を実行する。
    /// queue に後続がある場合は次の `RequestAllStates` を改めて発行し、
    /// 連続 deferred edit が個別に最新 state を捕まえられるようにする。
    fn on_all_states_from_child(&mut self, states: Vec<SlotState>) {
        // live song の plugin state を最新化する (= dirty 判定の整合と、
        // Deferred の Undo snapshot が最新 knob を捕まえるため)。 queue が空
        // だった場合 (= 想定外タイミングの応答) でも害はない。 Save の serialize
        // 対象は live ではなく凍結 snapshot なので、 下の match 内で snapshot 側に
        // も別途適用する。
        Self::apply_plugin_states_to(&mut self.song, &states);
        // Phase 6 review (silent corruption fix): plugin_host 側で
        // `state_save()` が `Err` を返したエントリは `SlotState.error`
        // 経由で報告される。 旧コードはこれを `.ok().flatten()` で握り
        // つぶしていて、 保存 file に空 state を書き → 次回開いたとき
        // plugin が default 状態に戻る silent corruption になっていた。
        // 集計して status_message に表示し、 ユーザーが「N 個の plugin
        // state が保存されなかった」 と認識できるようにする。 件数が多い
        // と message が長くなりすぎるので、 先頭 3 件のみ詳細を出し
        // 残りは件数集約。
        let failed: Vec<&SlotState> = states
            .iter()
            .filter(|s| s.error.is_some())
            .collect();
        if !failed.is_empty() {
            let mut msg = format!("Plugin state 保存失敗 ({} 件): ", failed.len());
            for (i, s) in failed.iter().take(3).enumerate() {
                if i > 0 {
                    msg.push_str(", ");
                }
                msg.push_str(&format!(
                    "track {} {:?}",
                    s.track, s.slot
                ));
            }
            if failed.len() > 3 {
                msg.push_str(&format!(" ... +{}", failed.len() - 3));
            }
            tracing::error!(failed_count = failed.len(), %msg, "plugin state save failures");
            self.status_message = msg;
        }
        let Some(req) = self.pending_state_queue.pop_front() else {
            return;
        };
        match req {
            PendingStateRequest::Save { path, snapshot } => {
                // snapshot は dispatch_front_state_request が **この save の
                // RequestAllStates を送る瞬間** に充填しているはず。 受け取った
                // states (= その RequestAllStates の応答) はその瞬間の host layout を
                // 反映するので、 snapshot のスロット配置と一致し、 位置 index 適用でも
                // 誤適用が起きない。 万一 None (想定外) なら防御的に live を凍結する。
                let mut snapshot = snapshot.unwrap_or_else(|| Box::new(self.song.clone()));
                Self::apply_plugin_states_to(&mut snapshot, &states);
                self.finish_save(snapshot, path);
            }
            PendingStateRequest::Deferred(edit) => {
                // ここで初めて Undo snapshot を push する。 Song に
                // 最新 state が入った状態を捕まえるため (plugin が
                // 削除される編集を Undo すると knob 値が復元される)。
                self.push_undo_snapshot();
                self.execute_deferred_edit(edit);
            }
        }
        // 後続の request が積まれていれば、 改めて `RequestAllStates` を発行して
        // 次の応答待ちに入る。 ここで「直前の edit が走ったあとの最新 state」 を
        // 再取得することで、 各 deferred edit が自前の knob snapshot を持つ。 さらに
        // 新たな front が Save なら、 dispatch_front_state_request が **この瞬間**
        // (= 先行 Deferred が live layout を確定させた直後) に live を凍結するので、
        // その Save の snapshot は返ってくる state と同じ layout になる。
        if !self.pending_state_queue.is_empty() {
            self.dispatch_front_state_request();
        }
        // 「保存して終了」 の完了判定は `finish_save` (save 成否が分かる場所) が行う。
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
        // 再生中のみ Tick の playhead を反映する。 停止中は GUI 側 playhead が
        // 権威 (stop() の「開始位置へ戻す」 / ruler seek / engine respawn 後の
        // 据え置き)。 これを入れないと、 stop() が playhead を origin に戻した
        // 後に IPC キューへ残った in-flight Tick (engine が Stop/SeekTo を反映
        // する前に読まれた直近サンプル位置) が後着で playhead を打ち消し、
        // 「Space で停止してもプレイヘッドが元位置に戻らないことがある」 race を
        // 生む。 stop() の SeekTo は engine 側カーソルを次の Play 用に揃えるため
        // 引き続き送る (= GUI 表示の権威と engine state を分離)。
        if self.is_playing && next_beat != self.playhead_beat {
            self.playhead_beat = next_beat;
        }

        // Phase 4 Step C: recording tick。 is_playing 中で recording_mode が
        // Read 以外、 かつ active ∪ latched gesture が non-empty なら、 各
        // gesture の現在 plain 値を AutomationPoint として playhead 位置に
        // 書き込む (1/64 beat throttle)。 Step C-2: audio thread は
        // `SetRecordingLanes` で受け取った set の lane の curve eval を bypass
        // しているので、 per-tick LoadSong は不要 (= recording 中は audio が
        // track.volume / track.pan の live value をそのまま鳴らす、 recording
        // 終了の瞬間に sync_recording_lanes_with_audio が LoadSong を送る)。
        //
        // `docs/plan_image_automation.md` §5: image PiP drag 中は再生
        // していなくても record path を動かす (= 停止中の drag で現
        // playhead に keyframe を打つ AE / Premiere 流 UX)。 image-only
        // 例外なので audio 経路は従来通り is_playing 必須。
        let image_dragging = self.image_pip_drag_active();
        // image drag 中は recording_mode = Read でも record path を回す
        // (= 「停止中の drag で現 playhead に keyframe」 を許可。 audio
        // 経路は recording_mode を尊重)。
        let audio_recording =
            self.is_playing && self.recording_mode != common::model::RecordingMode::Read;
        if (audio_recording || image_dragging)
            && let Some(ph) = self.playhead_beat
        {
            let _inserted = self.record_automation_points_for_tick(f64::from(ph));
        }

        // Video export 中は plugin GUI window の閉じる要求も無視する
        // （`handle_event` を通らない per-frame 経路なので個別に gate）。
        // export の音声 freewheel 中に CloseSlotGui を送ると plugin host の
        // offline render と競合しうる。
        #[cfg(windows)]
        if self.pending_video_export.is_none() && self.export_progress.is_none() {
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

    /// Phase 4 Step C: tick ごとの automation recording。 `is_playing` と
    /// `recording_mode != Read` が caller で確認済の前提。 各 active ∪ latched
    /// gesture について、 該当 track に同 target を持つ lane を探し、 lane 内
    /// で playhead を含む clip を探し、 1/64 beat throttle で AutomationPoint
    /// を insert する。
    ///
    /// Touch mode は active のみ、 Latch / Write は active ∪ latched (latched は
    /// `ParamGestureBegin` 時に再生中なら自動で insert 済)。
    ///
    /// 戻り値は今 tick で insert した点の総数 (= 0 なら sync skip)。 lane / clip
    /// が見つからない gesture は silently skip (= MVP: lane / clip は事前に user
    /// が作成する。 Bitwig 流 auto-create は Step C follow-up)。
    fn record_automation_points_for_tick(&mut self, playhead_beat: f64) -> usize {
        // recording_mode = Read でも image / text PiP drag 中だけは continue
        // (= 「停止中の drag が AE/Premiere 流の auto-keyframe を打つ」
        // 仕様。 audio gesture には影響しない、 drag が active な image /
        // text lane だけが record される)。
        let visual_dragging = self.image_pip_drag_active() || self.text_pip_drag_active();
        if self.recording_mode == common::model::RecordingMode::Read && !visual_dragging {
            return 0;
        }
        // active ∪ latched (Touch mode は latched が常に空なので active のみ)。
        let mut recording: Vec<(u32, common::model::AutomationTarget)> = Vec::new();
        for key in self.active_param_gestures.iter() {
            recording.push(key.clone());
        }
        if matches!(
            self.recording_mode,
            common::model::RecordingMode::Latch | common::model::RecordingMode::Write
        ) {
            for key in self.latched_param_gestures.iter() {
                if !self.active_param_gestures.contains(key) {
                    recording.push(key.clone());
                }
            }
        }
        if recording.is_empty() {
            return 0;
        }

        const THIN_INTERVAL_BEATS: f64 = 1.0 / 64.0;
        let mut inserted = 0usize;
        for (track_id, target) in recording {
            let last = self
                .recording_last_beat
                .get(&(track_id, target.clone()))
                .copied();
            if let Some(prev) = last
                && playhead_beat - prev < THIN_INTERVAL_BEATS
            {
                continue;
            }
            // 現在 plain 値 (= live knob 位置) を取得。 TrackBuiltin は song の
            // 現在値、 PluginParam は `plugin_param_values` cache
            // (`PluginParamValueChanged` で更新) から引く (`current_plain_value`
            // 参照)。 値が無ければ skip。
            let plain_value = match self.current_plain_value(track_id, &target) {
                Some(v) => v,
                None => continue,
            };
            // lane + clip を探す。
            let (clip_start, content_id) =
                match self.find_recording_lane(track_id, &target, playhead_beat) {
                    Some(ids) => ids,
                    None => continue,
                };
            // AutomationPoint は clip-local 時間で保存するので、 playhead から
            // clip.start_beat を引いて local 化する。
            let clip_local_beat = playhead_beat - clip_start;
            if self.insert_recording_point(content_id, clip_local_beat, plain_value) {
                self.recording_last_beat
                    .insert((track_id, target.clone()), playhead_beat);
                inserted += 1;
            }
        }
        inserted
    }

    /// Phase 4 Step C-2: GUI の currently recording set (= active ∪ latched
    /// based on mode) を計算する。 audio thread に送る IPC の payload と、
    /// `record_automation_points_for_tick` の iter source の両方で使う。
    pub(crate) fn currently_recording_lanes(
        &self,
    ) -> std::collections::HashSet<(u32, common::model::AutomationTarget)> {
        let mut set: std::collections::HashSet<(u32, common::model::AutomationTarget)> =
            std::collections::HashSet::new();
        if !self.is_playing || self.recording_mode == common::model::RecordingMode::Read {
            return set;
        }
        for k in &self.active_param_gestures {
            set.insert(k.clone());
        }
        if matches!(
            self.recording_mode,
            common::model::RecordingMode::Latch | common::model::RecordingMode::Write
        ) {
            for k in &self.latched_param_gestures {
                set.insert(k.clone());
            }
        }
        set
    }

    /// Phase 4 Step C-2: GUI の currently recording set が前回 audio thread
    /// に送った snapshot と異なる場合、 `SetRecordingLanes` IPC を送る。 set が
    /// 縮んだ (= recording 終了した lane が出た) 場合は、 audio thread が
    /// curve eval に戻るタイミングで最新 points を反映させるため、 LoadSong
    /// も送る (= `sync_song_to_plugin_host`)。
    ///
    /// 呼び出し場所:
    /// - `ParamGestureBegin` handler (set が拡大する可能性)
    /// - `ParamGestureEnd` handler (Touch mode で set が縮む)
    /// - `stop()` (Latch / Write で latched 全 clear、 set が縮む)
    /// - `SetRecordingMode(_)` handler (mode 変化で latched 寄与が変わる)
    fn sync_recording_lanes_with_audio(&mut self) {
        let next = self.currently_recording_lanes();
        if next == self.last_sent_recording_lanes {
            return;
        }
        let shrunk = self
            .last_sent_recording_lanes
            .iter()
            .any(|k| !next.contains(k));
        let lanes_vec: Vec<(u32, common::model::AutomationTarget)> =
            next.iter().cloned().collect();
        self.send_audio(MainToChild::SetRecordingLanes { lanes: lanes_vec });
        if shrunk {
            // recording 終了した lane の最新 points を audio thread に流す
            // (= bypass が解除されて curve eval に戻る瞬間に、 record session
            // 中に insert した点列で正しい curve が引かれるよう保証する)。
            self.sync_song_to_plugin_host();
        }
        self.last_sent_recording_lanes = next;
    }

    /// Phase 4 Step C: target に対応する現在 plain 値を返す。
    /// - `TrackBuiltin(Volume / Pan)`: Song の track field から直接
    /// - `PluginParam { slot, param_id }`: `plugin_param_values` cache (= plugin
    ///   GUI からの `PluginParamValueChangedFromChild` で更新される最新値) を
    ///   引く。 cache に entry が無い場合は `None` (= 一度も plugin GUI から
    ///   value 通知が来ていない、 record skip)
    /// - Mute / Send は M5 scope 外で `None`
    fn current_plain_value(
        &self,
        track_id: u32,
        target: &common::model::AutomationTarget,
    ) -> Option<f64> {
        use common::model::{ClipContent, ImageBuiltinParam};
        match target {
            // Phase 5: song-level target は track_id 無関係、 Song の現在値を返す
            common::model::AutomationTarget::SongTempo => Some(f64::from(self.song.bpm)),
            common::model::AutomationTarget::SongTimeSigNumerator => {
                Some(f64::from(self.song.time_sig.0))
            }
            common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Volume,
            ) => self
                .song
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .map(|t| f64::from(t.volume)),
            common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Pan,
            ) => self
                .song
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .map(|t| f64::from(t.pan)),
            common::model::AutomationTarget::PluginParam { slot, param_id } => self
                .plugin_param_values
                .get(&(track_id, *slot, *param_id))
                .copied(),
            // Image PiP: 同 track の first image event の field 値を現在値とする
            // (`docs/plan_image_automation.md` §4)。 drag が ImageEvent.field を
            // 更新 → ここで再読み込み → record_automation_points_for_tick が
            // point を打つ、 という pipeline。
            common::model::AutomationTarget::ImageBuiltin(field) => {
                let track = self.song.tracks.iter().find(|t| t.id == track_id)?;
                let event = track.clips.iter().find_map(|c| {
                    self.song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|content| match content {
                            ClipContent::Image(img) => img.events.first(),
                            _ => None,
                        })
                })?;
                Some(f64::from(match field {
                    ImageBuiltinParam::X => event.x,
                    ImageBuiltinParam::Y => event.y,
                    ImageBuiltinParam::W => event.w,
                    ImageBuiltinParam::H => event.h,
                    ImageBuiltinParam::Opacity => event.opacity,
                    ImageBuiltinParam::Rotation => event.rotation_radians,
                }))
            }
            // Text PiP: 同 track の first text event の field 値 (image と同
            // idiom)。 23 field 全部を返す (= color / shadow も lane に流す
            // ため)。
            common::model::AutomationTarget::TextBuiltin(field) => {
                use common::model::TextBuiltinParam as T;
                let track = self.song.tracks.iter().find(|t| t.id == track_id)?;
                let event = track.clips.iter().find_map(|c| {
                    self.song
                        .clip_contents
                        .get(&c.content_id)
                        .and_then(|content| match content {
                            ClipContent::Text(t) => t.events.first(),
                            _ => None,
                        })
                })?;
                Some(f64::from(match field {
                    T::X => event.x,
                    T::Y => event.y,
                    T::W => event.w,
                    T::H => event.h,
                    T::Opacity => event.opacity,
                    T::Rotation => event.rotation_radians,
                    T::FontSize => event.font_size_px,
                    T::FillR => event.fill_color[0],
                    T::FillG => event.fill_color[1],
                    T::FillB => event.fill_color[2],
                    T::FillA => event.fill_color[3],
                    T::OutlineR => event.outline_color[0],
                    T::OutlineG => event.outline_color[1],
                    T::OutlineB => event.outline_color[2],
                    T::OutlineA => event.outline_color[3],
                    T::OutlineWidth => event.outline_width_px,
                    T::ShadowR => event.shadow_color[0],
                    T::ShadowG => event.shadow_color[1],
                    T::ShadowB => event.shadow_color[2],
                    T::ShadowA => event.shadow_color[3],
                    T::ShadowOffsetX => event.shadow_offset_px.0,
                    T::ShadowOffsetY => event.shadow_offset_px.1,
                    T::ShadowBlur => event.shadow_blur_px,
                }))
            }
            _ => None,
        }
    }

    /// Phase 4 Step C: track の lane の中から、 同 target を持ち、 かつ playhead
    /// を含む clip を持つ lane を返す。 戻り値は `(clip.start_beat, content_id)`
    /// (clip-local 時間化に必要)。 lane が無い / clip が無い場合 `None`
    /// (= record skip)。
    fn find_recording_lane(
        &self,
        track_id: u32,
        target: &common::model::AutomationTarget,
        playhead_beat: f64,
    ) -> Option<(f64, common::model::ContentId)> {
        // Phase 5: SongTempo / SongTimeSigNumerator は song_lanes を参照、
        // track_id は ignore (= song-level lane は track に紐付かない)。
        let lane = match target {
            common::model::AutomationTarget::SongTempo
            | common::model::AutomationTarget::SongTimeSigNumerator => self
                .song
                .song_lanes
                .iter()
                .find(|l| l.enabled && l.target == *target)?,
            _ => {
                let track = self.song.tracks.iter().find(|t| t.id == track_id)?;
                track
                    .automation_lanes
                    .iter()
                    .find(|l| l.enabled && l.target == *target)?
            }
        };
        let clip = lane.clips.iter().find(|c| {
            playhead_beat >= c.start_beat && playhead_beat < c.start_beat + c.length_beats
        })?;
        Some((clip.start_beat, clip.content_id))
    }

    /// Phase 4 Step C: 指定 content (= shared automation curve) に
    /// `(time_beat, value, Linear)` point を sort 順を保って insert する。
    /// `time_beat` は **clip-local** (caller が `playhead_beat - clip.start_beat`
    /// に変換済を渡す)。 content_id の entry が `Automation` variant でない場合は
    /// false を返す。
    ///
    /// Step D thinning は `common::automation::thin_collinear_and_insert` に
    /// 抽出 (pure fn、 unit test 付き)。 ε は plain 単位で固定 0.005
    /// (Volume 範囲 0..=2 / Pan 範囲 -1..=1 のいずれでも 0.25% 程度)。
    fn insert_recording_point(
        &mut self,
        content_id: common::model::ContentId,
        time_beat: f64,
        plain_value: f64,
    ) -> bool {
        const THIN_EPSILON_PLAIN: f64 = 0.005;
        let entry = self
            .song
            .clip_contents
            .entry(content_id)
            .or_insert_with(|| {
                common::model::ClipContent::Automation(common::model::AutomationContent::default())
            });
        let points = match entry {
            common::model::ClipContent::Automation(a) => &mut a.points,
            _ => return false,
        };
        common::automation::thin_collinear_and_insert(
            points,
            time_beat,
            plain_value,
            THIN_EPSILON_PLAIN,
        );
        // 録音 gesture 途中で crash しても、 挿入済の点が autosave に
        // 乗るよう dirty を立てる。 GUI tick 経路 (= audio callback で
        // ない) なので bool 書き込みは RT 制約に抵触しない。
        self.is_dirty = true;
        true
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
        // FIXME #15: clip 数値 field は scrubable_number 化され専用 buffer は
        // 撤去。 共有 `clip_edit_buffer_target` だけ song 差し替え (open / new
        // / undo / redo) に追従させる。 selected_clip が image / text の場合は
        // 次フレームの view 側 target 不一致検知で正しい resync が走るため、
        // ここでは audio marker (= 非 audio なら None 化) で十分。
        match self.selected_clip_ref() {
            Some(target) => self.resync_clip_audio_event_edit_buffers(target),
            None => {
                self.clip_edit_buffer_target = None;
            }
        }
    }

    fn set_master_gain(&mut self, gain: f32) {
        let clamped = gain.clamp(0.0, 1.0);
        self.master_gain = clamped;
        self.send_audio(MainToChild::SetMasterGain(clamped));
    }

    // -------- Plugin picker -----------------------------------------------

    fn select_plugin_from_db(&mut self, id: String, keep_open: bool, open_gui: bool) {
        // 無修飾 / Shift は選択で閉じる。 Ctrl (keep_open) は開いたまま連続追加
        // できる (各プラグインは自分の種別で振り分く)。
        if !keep_open {
            self.is_plugin_picker_open = false;
        }
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
        // FIXME #29: 行き先は features ではなく **port 構成由来の capability** で決める
        // (note 出力の有無は probe 済みの SSoT)。 dual-role は音源の空き状況で
        // 生成器 / 音源に振り分ける。
        let capability = PluginCapability::from_entry(entry);
        self.ensure_first_track();

        // master bus 選択時は track Vec ではなく Song.master_fx_chain を対象に
        // する。 master は audio fx のみ持つので、 note 出力や音源能力を持つ
        // プラグイン (生成器 / 音源 / dual-role) は挿せない。
        if self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID) {
            if capability != PluginCapability::AudioFx {
                tracing::warn!(
                    ?capability,
                    "master bus に audio fx 以外は挿せない"
                );
                return;
            }
            let track_id = common::model::MASTER_TRACK_ID;
            let dest_slot = PluginSlot::Fx(self.song.master_fx_chain.len() as u32);
            self.track_pending_load(track_id, dest_slot);
            // Shift (open_gui=false) のときは GUI 自動 open しない (ロードはする)。
            if open_gui {
                self.pending_added_plugin_finalize.insert((track_id, dest_slot));
            }
            self.send_plugin(MainToChild::SetSlotPlugin {
                track: track_id,
                slot: dest_slot,
                format: entry_format,
                path,
                plugin_id: entry_id.clone(),
                initial_state: None,
            });
            self.song.master_fx_chain.push(common::model::PluginInstance::new(
                entry_id,
                entry_format,
            ));
            return;
        }

        let Some(track_idx) = self.cursor_track_index() else {
            return;
        };
        // PR2.1: send `Track::id` to the plugin host (track_idx は
        // ローカルの song.tracks 操作のみで使う)。
        let track_id = self.song.tracks[track_idx].id;
        // FIXME #29: 行き先スロットは capability + 音源の空き状況で決まる。
        // PureMidiEffect は生成器列、 AudioFx は audio-fx 列、 Instrument は音源スロット
        // (単一なので 2 つ目は差し替え)。 DualRole (例: Scaler 2) は音源が空なら音源、
        // 埋まっていれば「後段を鳴らす生成器」として生成器列末尾へ (grilled 既定挿入)。
        let dest_slot = match capability {
            PluginCapability::Instrument => PluginSlot::Instrument,
            PluginCapability::AudioFx => {
                let next = self.song.tracks[track_idx].fx_chain.len() as u32;
                PluginSlot::Fx(next)
            }
            PluginCapability::PureMidiEffect => {
                let next = self.song.tracks[track_idx].midi_fx_chain.len() as u32;
                PluginSlot::MidiFx(next)
            }
            PluginCapability::DualRole => {
                if self.song.tracks[track_idx].instrument.is_none() {
                    PluginSlot::Instrument
                } else {
                    let next = self.song.tracks[track_idx].midi_fx_chain.len() as u32;
                    PluginSlot::MidiFx(next)
                }
            }
        };
        self.track_pending_load(track_id, dest_slot);
        // ユーザーが手動追加した plugin は load 完了時に daw_audio 再 sync + GUI 自動
        // open する (project-load の一斉復元はこの集合に積まれない)。 Shift
        // (open_gui=false) のときは GUI 自動 open を抑止 (ロードはする)。
        if open_gui {
            self.pending_added_plugin_finalize.insert((track_id, dest_slot));
        }
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
                    // 統合モデル: instrument に builtin VOICEVOX を挿したら vocal track
                    // 化 (source=Vocal)、 それ以外の音源なら通常 (None) に戻す。 旧
                    // "+Vocal Track" ボタンの役割をここに集約 (sync_vocal_metadata が
                    // source==Vocal を見て歌詞 synth を走らせる)。
                    track.source = if entry_id.as_str() == common::plugin_db::BUILTIN_ID_VOICEVOX
                    {
                        InstrumentSource::Vocal {
                            speaker_id: common::voicevox::DEFAULT_SINGER_ID,
                            style_name: "ノーマル".into(),
                        }
                    } else {
                        InstrumentSource::None
                    };
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

    // PR-V4: 旧 VOICEVOX synth path (begin_vocal_synth /
    // finish_vocal_synth) は削除。 vocal track は builtin VOICEVOX
    // instrument plugin で再生され、 歌詞 flush は sync_vocal_metadata で
    // 自動行われる (= explicit Synth ボタンは不要)。

    /// VOICEVOX engine の lazy spawn (旧 `begin_vocal_synth` から
    /// 移植)。 sync_vocal_metadata で「vocal track が 1 つでもある」
    /// 状態が初めて発生した時に呼ばれ、 background thread で
    /// `voicevox_engine::is_running()` を確認、 未起動なら
    /// `spawn_engine` で localhost:50021 を立ち上げる。 try は 1 度
    /// だけ (`voicevox_launch_attempted` flag で抑止)、 user が手動で
    /// engine を落とした場合は手動再起動。 spawn 後の child は
    /// `JobObject` に attach するので daw_gui 終了で auto-kill される。
    fn ensure_voicevox_engine(&mut self) {
        if self.voicevox_launch_attempted {
            return;
        }
        self.voicevox_launch_attempted = true;
        let job = Arc::clone(&self.voicevox_job);
        std::thread::spawn(move || {
            if common::voicevox_engine::is_running() {
                return;
            }
            let Some(engine) = common::voicevox_engine::resolve_engine_path() else {
                let cfg_hint = common::voicevox_engine::engine_path_config_file()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "<no localappdata>".into());
                tracing::warn!(
                    hint = %cfg_hint,
                    "VOICEVOX engine path not configured (set DAW_VOICEVOX_PATH or write the exe path to the config file)"
                );
                return;
            };
            tracing::info!(?engine, "lazy spawn VOICEVOX engine for builtin plugin");
            match common::voicevox_engine::spawn_engine(&engine) {
                Ok(child) => {
                    if let Err(e) = job.assign_std(&child) {
                        tracing::warn!(error = ?e, "failed to attach VOICEVOX to job");
                    }
                    // child を drop しても std::process::Child は wait
                    // しない (Windows)。 JobObject 経由で auto-kill される。
                    std::mem::forget(child);
                }
                Err(e) => {
                    tracing::error!(error = ?e, ?engine, "failed to spawn VOICEVOX engine");
                }
            }
        });
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
            Ok(mut db) => {
                // FIXME #29: VST3 / CLAP とも descriptor からは port 構成が分からない
                // (VST3 は category tag 無し、 CLAP は feature に note 出力の有無が無い)。
                // 各プラグインを使い捨て probe プロセスで起動して note in/out・audio out
                // を読み、 PluginEntry の 3 bool (capability の SSoT) を更新する。 probe
                // 失敗 / timeout は scan-time 暫定値を保持 (退行しない)。 builtin は code が
                // SSoT なので probe しない。
                let probe_idx: Vec<usize> = db
                    .entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| {
                        matches!(
                            e.format,
                            common::plugin_format::PluginFormat::Vst3
                                | common::plugin_format::PluginFormat::Clap
                        )
                    })
                    .map(|(i, _)| i)
                    .collect();
                let total = probe_idx.len();
                for (n, &i) in probe_idx.iter().enumerate() {
                    proxy.send(AppEvent::RescanProgress { done: n, total });
                    let (format, path, id) = {
                        let e = &db.entries[i];
                        (e.format, e.path.clone(), e.id.clone())
                    };
                    if let Some(cfg) = crate::subprocess::probe_plugin_ports(format, &path, &id) {
                        let e = &mut db.entries[i];
                        e.has_note_input = cfg.has_note_input;
                        e.has_note_output = cfg.has_note_output;
                        e.has_audio_output = cfg.has_audio_output;
                    }
                }
                if total > 0 {
                    proxy.send(AppEvent::RescanProgress { done: total, total });
                }
                // FIXME #29 Step 7: probe 済みを示す版を立てる (起動時の自動再 probe 判定用)。
                db.port_probe_version = common::plugin_db::PORT_PROBE_VERSION;
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
        // 走査進捗 overlay を消す (FIXME #26 Phase B)。
        self.load_progress = None;
        let Some(new_db) = self.rescan_result.lock().ok().and_then(|mut g| g.take()) else {
            return;
        };
        let new_db = Arc::new(new_db);
        self.plugin_db = Some(new_db);
        self.rebuild_picker_entries();
        self.refresh_picker_visible();
    }

    // -------- Mixer --------------------------------------------------------

    // Phase 6 review (SSOT fix): `track_id` は stable な Track::id。 旧 GUI
    // 側は Vec index を受け取って `self.song.tracks.get_mut(idx)` していたが、
    // IPC を通すと audio engine 側の Vec 順序とずれて race を起こすため、
    // ここから IPC まで一貫して id で識別する。
    fn set_track_volume(&mut self, track_id: u32, volume: f32) {
        let v = volume.clamp(0.0, 1.0);
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        // SetSongBpmFromScrub と同 idiom: 値が実際に変わったときだけ
        // dirty を立てて autosave に乗せる (= drag 途中 crash でも保存)。
        let changed = (t.volume - v).abs() > f32::EPSILON;
        t.volume = v;
        if changed {
            self.is_dirty = true;
        }
        let msg = MainToChild::SetTrackVolume { track: track_id, volume: v };
        self.send_audio(msg);
        // gui_01 #028 §7.3: knob 操作で last-touched param を更新。
        // `A` キー shortcut の source になる。
        self.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Volume,
            ),
            display_name: "Volume".to_string(),
            touched_at: std::time::Instant::now(),
        });
    }

    fn set_track_pan(&mut self, track_id: u32, pan: f32) {
        let p = pan.clamp(-1.0, 1.0);
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        let changed = (t.pan - p).abs() > f32::EPSILON;
        t.pan = p;
        if changed {
            self.is_dirty = true;
        }
        let msg = MainToChild::SetTrackPan { track: track_id, pan: p };
        self.send_audio(msg);
        self.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::Pan,
            ),
            display_name: "Pan".to_string(),
            touched_at: std::time::Instant::now(),
        });
    }

    // -------- Aux send / return -------------------------------------------

    /// Ableton "Add Return" 相当。 master 直下の通常 track を 1 本作って
    /// `"Return N"` と命名し、 track が選択中ならその track に新リターン宛て
    /// の send を 1 本足して即座に効果が聞こえるようにする。 構造変化なので
    /// `sync_song_to_plugin_host` で full-song resend (= schedule 再 compile)。
    /// `action_add_instrument_track` を mirror した構成。
    fn action_add_return_track(&mut self) {
        // 既存リターン数 + 1 で命名 (= 派生集合の cardinality)。
        let existing_returns = self
            .song
            .tracks
            .iter()
            .filter(|t| self.is_return_track(t.id))
            .count();
        let id = self.song.alloc_track_id();
        let track = Track {
            id,
            name: format!("Return {}", existing_returns + 1),
            // リターンは master 直下に流す。
            parent_group_id: None,
            ..Track::default()
        };
        self.song.tracks.push(track);
        // 選択中 track があれば、 そこから新リターンへ即座に send を 1 本張る
        // (Ableton "Add Return" の即時性)。 選択が無ければ wiring だけ作って
        // ユーザーが後で「＋ Send」 で繋ぐ。 自分自身宛て (= 新リターンが
        // 選択されていた可能性) は意味が無いので除外。
        if let Some(sel_id) = self.cursor_track_id()
            && sel_id != id
            && let Some(src) = self.song.tracks.iter_mut().find(|t| t.id == sel_id)
        {
            src.sends.push(common::model::Send {
                dest_track_id: id,
                gain: 1.0,
                mode: SendMode::PostFader,
                enabled: true,
            });
        }
        self.resize_track_peak_display();
        self.sync_song_to_plugin_host();
        tracing::info!(return_id = id, "added return track");
    }

    /// `src_track_id` に `dest_track_id` 宛ての send を 1 本追加。 構造変化
    /// なので full-song resend。 同宛先の重複 send は許す (= Ableton も複数
    /// 同一 return への send を別途持てる訳ではないが、 本 MVP では単純に
    /// append、 picker 側で self-cycle のみ除外)。
    fn add_send(&mut self, src_track_id: u32, dest_track_id: u32) {
        if src_track_id == dest_track_id {
            return;
        }
        let Some(src) = self.song.tracks.iter_mut().find(|t| t.id == src_track_id) else {
            return;
        };
        src.sends.push(common::model::Send {
            dest_track_id,
            gain: 1.0,
            mode: SendMode::PostFader,
            enabled: true,
        });
        self.sync_song_to_plugin_host();
        tracing::info!(src_track_id, dest_track_id, "added send");
    }

    /// `track_id` の `sends[send_idx]` を削除。 構造変化 → full-song resend。
    /// 後続 send の index がずれるが、 resend で schedule が新 index で
    /// 再 compile されるため問題ない。 なお in-flight な automation lane が
    /// `SendGain { send_idx }` を target にしている場合、 その lane の参照は
    /// 旧 index のまま残る — 本タスクでは lane の reindex は行わない (= 別
    /// タスク。 当面は stale lane を許容、 schedule 側は範囲外 send_idx を
    /// 無視する)。
    fn remove_send(&mut self, track_id: u32, send_idx: usize) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        if send_idx >= t.sends.len() {
            return;
        }
        t.sends.remove(send_idx);
        self.sync_song_to_plugin_host();
        tracing::info!(track_id, send_idx, "removed send");
    }

    /// `track_id` の `sends[send_idx].mode` を設定。 tap 位置 (pre/post) は
    /// routing graph に影響するので 構造変化 → full-song resend。
    fn set_send_mode(&mut self, track_id: u32, send_idx: usize, mode: SendMode) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        let Some(send) = t.sends.get_mut(send_idx) else {
            return;
        };
        if send.mode == mode {
            return;
        }
        send.mode = mode;
        self.sync_song_to_plugin_host();
    }

    /// `sends[send_idx].gain` を 0..2 に clamp して設定 + realtime IPC。
    /// `set_track_volume` を mirror — full-song resend しない (= drag 中の
    /// 高頻度更新を audio engine が live re-read する)。 last-touched param も
    /// 更新して `A` キーで send-gain automation lane を生やせるようにする。
    fn set_send_gain(&mut self, track_id: u32, send_idx: usize, gain: f32) {
        let g = gain.clamp(0.0, 2.0);
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        let Some(send) = t.sends.get_mut(send_idx) else {
            return;
        };
        let changed = (send.gain - g).abs() > f32::EPSILON;
        send.gain = g;
        if changed {
            self.is_dirty = true;
        }
        // send_idx は u8 (= AutomationTarget / protocol の表現) に収まる範囲
        // のみ realtime 送信。 256 本以上の send は非現実的だが防衛的に gate。
        let Ok(send_idx_u8) = u8::try_from(send_idx) else {
            return;
        };
        self.send_audio(MainToChild::SetSendGain {
            track: track_id,
            send_idx: send_idx_u8,
            gain: g,
        });
        self.last_touched_param = Some(TouchedParam {
            track_id,
            target: common::model::AutomationTarget::TrackBuiltin(
                common::model::TrackBuiltinParam::SendGain { send_idx: send_idx_u8 },
            ),
            display_name: format!("Send {}", send_idx + 1),
            touched_at: std::time::Instant::now(),
        });
    }

    /// `sends[send_idx].enabled` を設定 + realtime IPC。 `set_send_gain` と
    /// 同 idiom、 full-song resend しない (= 配線は維持したまま mute)。
    fn set_send_enabled(&mut self, track_id: u32, send_idx: usize, enabled: bool) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        let Some(send) = t.sends.get_mut(send_idx) else {
            return;
        };
        let changed = send.enabled != enabled;
        send.enabled = enabled;
        if changed {
            self.is_dirty = true;
        }
        let Ok(send_idx_u8) = u8::try_from(send_idx) else {
            return;
        };
        self.send_audio(MainToChild::SetSendEnabled {
            track: track_id,
            send_idx: send_idx_u8,
            enabled,
        });
    }

    fn toggle_track_mute(&mut self, track_id: u32) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        t.muted = !t.muted;
        let muted = t.muted;
        // toggle なので値は必ず変化する → autosave 用に dirty を立てる。
        self.is_dirty = true;
        let msg = MainToChild::SetTrackMuted { track: track_id, muted };
        self.send_audio(msg);
    }

    fn toggle_track_solo(&mut self, track_id: u32) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        t.solo = !t.solo;
        let solo = t.solo;
        // toggle なので値は必ず変化する → autosave 用に dirty を立てる。
        self.is_dirty = true;
        let msg = MainToChild::SetTrackSolo { track: track_id, solo };
        self.send_audio(msg);
    }

    fn toggle_track_armed(&mut self, track_id: u32) {
        let Some(t) = self.song.tracks.iter_mut().find(|t| t.id == track_id) else {
            return;
        };
        t.armed = !t.armed;
        let armed = t.armed;
        let msg = MainToChild::SetTrackArmed { track: track_id, armed };
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
        let mut v: Vec<PluginPickEntry> =
            db.entries.iter().map(PluginPickEntry::from_db_entry).collect();
        v.sort_by_key(|e| e.name.to_lowercase());
        self.plugin_picker_entries = v;
    }

    fn refresh_picker_visible(&mut self) {
        // master bus は audio FX しか持てないので、 master 選択中は FX カテゴリの
        // プラグインだけをリストに出す (それ以外は挿せないので隠す)。 通常トラックは
        // 全カテゴリ混合で見せ、 種別は選択時に features から自動振り分けする
        // (plan_unified_plugin_picker.md)。
        let master = self.cursor_track_id() == Some(common::model::MASTER_TRACK_ID);
        // 検索クエリ (前後空白を除去)。 空なら (master フィルタを除き) 全件、 非空なら
        // name / vendor のいずれかへの subsequence マッチで AND 絞り込みする。
        let query = self.plugin_picker_query.trim();
        let visible: Vec<PluginPickEntry> = self
            .plugin_picker_entries
            .iter()
            .filter(|e| !master || e.category == PluginCategory::Fx)
            .filter(|e| {
                query.is_empty()
                    || crate::fuzzy::subsequence_match(&e.name, query)
                    || crate::fuzzy::subsequence_match(&e.vendor, query)
            })
            .cloned()
            .collect();
        self.plugin_picker_visible = visible;
        // 絞り込み再計算後はカーソルを先頭に戻す (要件 7)。 query 変更 / target 切替 /
        // rescan 完了で呼ばれるため、 「絞り込みが変わったら先頭にリセット」 が自然。
        self.plugin_picker_cursor = 0;
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

    /// Phase 7 B4 Step E (2026-05-13): File → Export MIDI...
    /// `rfd` で .mid ファイル保存先を選択 → `midi_export::export_midi`
    /// で SMF1 書き出し。 audio engine への IPC 不要 (= GUI process 単独で
    /// `Song` snapshot を SMF に変換)。 失敗時は status_message に error。
    fn action_export_midi(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("MIDI", &["mid", "midi"])
            .save_file()
        else {
            return;
        };
        match crate::midi_export::export_midi(&self.song, &path) {
            Ok(()) => {
                self.status_message =
                    format!("MIDI 書き出し完了: {}", path.display());
            }
            Err(e) => {
                self.status_message = format!("MIDI 書き出し失敗: {e}");
                tracing::error!(error = %e, path = %path.display(), "MIDI export failed");
            }
        }
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
        // dialog 経由は位置情報がないので target_track_idx / target_beat = None
        // (= cursor_track / playhead にフォールバック)。
        self.action_import_audio(paths, None, None);
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

    fn action_import_audio(
        &mut self,
        paths: Vec<PathBuf>,
        target_track_idx: Option<u32>,
        target_beat: Option<f64>,
    ) {
        if paths.is_empty() {
            return;
        }
        let project_dir: Option<PathBuf> = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // 引数 `target_track_idx` (= drag&drop の drop 位置から arrangement
        // view が計算) を最優先、 None なら cursor_track_index にフォール
        // バック (= File menu / dialog 経由)、 さらに無いときは 0。 範囲外
        // (= track 数を超える) 値は最後の track に clamp。
        let n_tracks = self.song.tracks.len();
        let target_track_idx: usize = target_track_idx
            .map(|i| (i as usize).min(n_tracks.saturating_sub(1)))
            .or_else(|| self.cursor_track_index())
            .unwrap_or(0);
        // drag&drop の drop 位置 (`target_beat`) を最優先、 無ければ playhead。
        let start_beat_seed: f64 =
            target_beat.unwrap_or(self.playhead_beat.unwrap_or(0.0) as f64);
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
            let content_id = self.song.alloc_content(
                ClipContent::Audio(AudioContent {
                    events: vec![event],
                }),
                imported.display_name.clone(),
            );

            let track = &mut self.song.tracks[target_track_idx];
            let new_clip_id = track.alloc_clip_id();
            track.clips.push(Clip {
                id: new_clip_id,
                name: String::new(),
                start_beat: next_start_beat,
                length_beats,
                content_id,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
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

    /// Video import (docs/plan_video.md P2). For each path:
    ///   1. `import_one_video` does the WMF metadata read + audio
    ///      extract + decode (on the GUI thread; typical phone-video
    ///      imports finish in 1-3s).
    ///   2. Allocate `AudioSourceId` for the extracted audio (if any),
    ///      register it on `Song.audio_sources`, cache the decoded
    ///      buffer.
    ///   3. Allocate `VideoSourceId`, link it to the audio id, register
    ///      on `Song.video_sources`.
    ///   4. Build one `VideoEvent` covering the whole source and wrap
    ///      it in a fresh `ClipContent::Video`.
    ///   5. Append a new `TrackKind::Video` track and (when audio is
    ///      present) a paired `TrackKind::Audio` track. Each carries a
    ///      single clip starting at the playhead.
    ///
    /// Subsequent imports stack at the end of the timeline by bumping
    /// `next_start_beat`. Failures are collected per path and surfaced
    /// in `status_message` along with the success count.
    #[cfg(windows)]
    fn action_import_video(&mut self, paths: Vec<PathBuf>, target_beat: Option<f64>) {
        use common::model::{
            AudioContent, AudioEvent, ClipContent, Track, VideoContent, VideoEvent,
        };

        if paths.is_empty() {
            return;
        }
        let project_dir: Option<PathBuf> = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));

        // drag&drop の drop 位置 (`target_beat`) を最優先、 無ければ playhead。
        let start_beat_seed: f64 =
            target_beat.unwrap_or(self.playhead_beat.unwrap_or(0.0) as f64);
        let bpm = self.song.bpm;
        let mut imported_ok = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut next_start_beat = start_beat_seed.max(0.0);

        for path in paths {
            let imported = match crate::import_video::import_one_video(
                &path,
                project_dir.as_deref(),
            ) {
                Ok(i) => i,
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let display_name = imported.display_name.clone();
            let duration_micros = imported.video_source.duration_micros;
            let video_length_beats = micros_to_beats(duration_micros, bpm);

            // 1) Register paired audio (if present) before video so we
            //    have the AudioSourceId for the video back-link.
            let audio_source_id = imported.audio.as_ref().map(|audio| {
                let id = self.song.alloc_audio_source_id();
                self.song.audio_sources.insert(id, audio.source.clone());
                self.audio_source_cache.insert(id, audio.buffer.clone());
                id
            });

            // 2) Register video source with the audio back-link.
            let video_source_id = self.song.alloc_video_source_id();
            let mut vs = imported.video_source;
            vs.audio_source_id = audio_source_id;
            self.song.video_sources.insert(video_source_id, vs);

            // 2b) Stash the thumbnail RGBA (if extracted) and queue
            //     a GPU upload. The runner picks this up next frame
            //     (P3.5) and writes the resulting TextureHandle into
            //     `video_texture_cache` for the arrangement view to
            //     read.
            if let Some(thumb) = imported.thumbnail {
                self.video_thumbnail_rgba.insert(
                    video_source_id,
                    (thumb.width, thumb.height, std::sync::Arc::new(thumb.rgba)),
                );
                self.pending_thumbnail_uploads.push(video_source_id);
            }

            // 3) Video clip content + auto track.
            let v_content_id = self.song.alloc_content(
                ClipContent::Video(VideoContent {
                    events: vec![VideoEvent {
                        source_id: video_source_id,
                        event_start_in_clip_beats: 0.0,
                        event_length_beats: video_length_beats,
                        source_start_micros: 0,
                        source_end_micros: duration_micros,
                        ..VideoEvent::default()
                    }],
                }),
                display_name.clone(),
            );
            let video_track_id = self.song.alloc_track_id();
            let mut video_track = Track {
                id: video_track_id,
                name: format!("{display_name} (Video)"),
                ..Track::default()
            };
            let v_clip_id = video_track.alloc_clip_id();
            video_track.clips.push(Clip {
                id: v_clip_id,
                name: String::new(),
                start_beat: next_start_beat,
                length_beats: video_length_beats,
                content_id: v_content_id,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
            });
            self.song.tracks.push(video_track);

            // 4) Paired audio clip + audio track (only when audio is
            //    present in the source).
            if let (Some(audio), Some(audio_src_id)) =
                (imported.audio, audio_source_id)
            {
                let audio_length_beats = frames_to_beats(
                    audio.buffer.frames,
                    audio.buffer.sample_rate,
                    bpm,
                );
                let a_content_id = self.song.alloc_content(
                    ClipContent::Audio(AudioContent {
                        events: vec![AudioEvent {
                            source_id: audio_src_id,
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: audio_length_beats,
                            source_start_frames: 0,
                            source_end_frames: audio.buffer.frames,
                            ..AudioEvent::default()
                        }],
                    }),
                    format!("{display_name} (audio)"),
                );
                let audio_track_id = self.song.alloc_track_id();
                let mut audio_track = Track {
                    id: audio_track_id,
                    name: format!("{display_name} (Audio)"),
                    ..Track::default()
                };
                let a_clip_id = audio_track.alloc_clip_id();
                audio_track.clips.push(Clip {
                    id: a_clip_id,
                    name: String::new(),
                    start_beat: next_start_beat,
                    length_beats: audio_length_beats,
                    content_id: a_content_id,
                    notes: Vec::new(),
                    color: None,
                    auto_lipsync: false,
                });
                self.song.tracks.push(audio_track);
            }

            next_start_beat += video_length_beats;
            imported_ok += 1;
        }

        if imported_ok > 0 {
            self.is_dirty = true;
            self.sync_song_to_plugin_host();
        }

        self.status_message = match (imported_ok, errors.is_empty()) {
            (0, false) => format!("Video import 失敗: {}", errors.join(" / ")),
            (n, true) => format!("Video import 完了: {n} ファイル (V + A track 追加)"),
            (n, false) => format!(
                "Video import: {n} ファイル成功、 {} 件エラー: {}",
                errors.len(),
                errors.join(" / ")
            ),
        };
    }

    /// File menu → "Import Video..." 経路。 `rfd` の native file picker
    /// (multi-select、 mp4/mov/mkv/webm filter) を開いて、 選択された
    /// path を `action_import_video` に転送する。
    #[cfg(windows)]
    fn action_open_import_video_dialog(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "mov", "mkv", "webm", "m4v", "avi"])
            .set_title("Import Video...")
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        // dialog 経由は位置情報がないので target_beat = None (= playhead)。
        self.action_import_video(paths, None);
    }

    /// v13 (`docs/plan_image_overlay.md` §P2): import one or more
    /// image files as PiP overlay clips. Each successful import:
    ///
    /// 1. Allocates an `ImageSourceId` and registers an `ImageSource`
    ///    in `Song.image_sources` (path / dimensions / format).
    /// 2. Stages the BGRA8 bytes in `image_source_bgra` and queues a
    ///    GPU texture upload via `pending_image_uploads` — the runner
    ///    picks this up next frame (P3) and writes the resulting
    ///    `TextureHandle` into `image_texture_cache`.
    /// 3. Creates a Video-kind Track + an Image clip occupying the
    ///    project length (= so the user immediately sees the image
    ///    on top of any active video). PiP rect defaults to full-
    ///    screen; the user shrinks/positions it via the P5 drag
    ///    handle UI or the P4 inspector.
    ///
    /// Errors are accumulated; partial-success is permitted (= the
    /// status bar summarizes how many files succeeded / failed).
    fn action_import_image(
        &mut self,
        paths: Vec<PathBuf>,
        target_track_idx: Option<u32>,
        target_beat: Option<f64>,
    ) {
        if paths.is_empty() {
            return;
        }
        let project_dir = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));

        // drop 位置から計算した track index が既存 track を指していれば、その
        // track に画像 clip を貼り付ける (= ドロップしたトラックに追加)。 track の
        // 無い下の領域 (= 範囲外 index) や dialog 経由 (None) は従来どおり
        // arrangement 先頭 (index 0) に新規 track を作って貼る。
        let dest_track_idx: Option<usize> =
            resolve_image_drop_target(target_track_idx, self.song.tracks.len());

        // drag&drop の drop 位置 (`target_beat`) を最優先。 無いとき (dialog 経由)
        // は従来挙動: 既存 track に貼るときは playhead を seed に順送り配置
        // (複数枚を重ねない)、 新規 track 経路は各画像が自分の track を持つので
        // beat 0 始まり。
        let mut next_start_beat = match target_beat {
            Some(b) => b.max(0.0),
            None if dest_track_idx.is_some() => {
                (self.playhead_beat.unwrap_or(0.0) as f64).max(0.0)
            }
            None => 0.0_f64,
        };
        let image_clip_length_beats = (self.song.length_beats * 0.5).max(8.0);

        let mut imported_ok = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for path in &paths {
            let imported = match crate::import_image::import_one_image(
                path,
                project_dir.as_deref(),
            ) {
                Ok(i) => i,
                Err(e) => {
                    errors.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };

            // 1) Register ImageSource + stage BGRA for GPU upload.
            let image_source_id = self.song.alloc_image_source_id();
            self.song.image_sources.insert(image_source_id, imported.source);
            let w = imported.bgra.len() as u32;
            let _ = w;
            // bgra length matches width * height * 4 by construction of
            // `import_one_image`; re-fetch dims from the source we just
            // inserted so the staging matches.
            let src = &self.song.image_sources[&image_source_id];
            self.image_source_bgra.insert(
                image_source_id,
                (src.width, src.height, std::sync::Arc::new(imported.bgra)),
            );
            self.pending_image_uploads.push(image_source_id);

            // 2) Build the Image clip content. Single ImageEvent
            // covering the whole clip。 デフォルト PiP rect は
            // `Song.video_resolution` と画像 aspect で「アスペクト比
            // 維持の中央配置」 を計算する (= 縦長画像を 16:9 preview に
            // 入れると上下に余白、 横長画像なら左右に余白)。 ユーザーが
            // 後から inspector / preview drag で自由に拡縮 / 配置できる。
            let display_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("image")
                .to_string();
            let (def_x, def_y, def_w, def_h) = aspect_fit_pip_rect(
                self.song.video_resolution,
                (src.width, src.height),
            );
            let i_content_id = self.song.alloc_content(
                common::model::ClipContent::Image(common::model::ImageContent {
                    events: vec![common::model::ImageEvent {
                        source_id: image_source_id,
                        event_start_in_clip_beats: 0.0,
                        event_length_beats: image_clip_length_beats,
                        x: def_x,
                        y: def_y,
                        w: def_w,
                        h: def_h,
                        ..common::model::ImageEvent::default()
                    }],
                }),
                display_name.clone(),
            );

            // 3) 配置先 track を決める。
            //    - 既存 track (drop 先): その index にそのまま貼る。
            //    - 新規 track: arrangement 先頭 (index 0) に Video 用 track を
            //      作って挿入 → 既存 video layer の上に合成される
            //      (multi-track composite top-wins, plan_video §4 P7)。
            let place_idx = match dest_track_idx {
                Some(idx) => idx,
                None => {
                    let image_track_id = self.song.alloc_track_id();
                    self.song.tracks.insert(
                        0,
                        Track {
                            id: image_track_id,
                            name: format!("{display_name} (Image)"),
                            ..Track::default()
                        },
                    );
                    0
                }
            };
            let track = &mut self.song.tracks[place_idx];
            let i_clip_id = track.alloc_clip_id();
            track.clips.push(Clip {
                id: i_clip_id,
                name: String::new(),
                start_beat: next_start_beat,
                length_beats: image_clip_length_beats,
                content_id: i_content_id,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
            });
            // 既存 track に複数枚貼るときだけ順送り。 新規 track 経路は各画像が
            // 自分の track を持つので beat 0 固定 (従来挙動)。
            if dest_track_idx.is_some() {
                next_start_beat += image_clip_length_beats;
            }
            imported_ok += 1;
        }

        if imported_ok > 0 {
            self.is_dirty = true;
            // No `sync_song_to_plugin_host` — image clips have no
            // audio engine implications, the daw_audio process never
            // sees them.
        }

        // `paths.is_empty()` early-returns above, so we know
        // imported_ok + errors.len() >= 1 — the (0, true) "nothing
        // happened" case is unreachable here.
        self.status_message = match (imported_ok, errors.is_empty()) {
            (0, false) => format!("Image import 失敗: {}", errors.join(" / ")),
            (n, true) => format!("Image import 完了: {n} ファイル"),
            (n, false) => format!(
                "Image import: {n} ファイル成功、 {} 件エラー: {}",
                errors.len(),
                errors.join(" / ")
            ),
        };
    }

    /// docs/plan_text_clip_creation.md: 空きレーン右クリック → "Text クリップ" 経路。
    /// `track_id` の track の `start_beat` 位置に `ClipContent::Text` clip を 1 個追加する。
    /// clip は default 体裁 ("Title" / 64px / 中央横帯) の単一 `TextEvent` を持つ。
    /// 「text トラック」 は存在せず (v16 で全 track 統一済み)、 text は他 clip と同じく
    /// 任意の track 上にタイムラインで生成する。 content / styles は inspector、 PiP rect は
    /// preview drag で編集。 clip 長は他 clip 生成 (`create_clip`) と同じ `DEFAULT_CLIP_LENGTH`。
    fn add_text_clip_to_track(&mut self, track_id: u32, start_beat: f64) {
        let Some(track_idx) = self.song.tracks.iter().position(|t| t.id == track_id) else {
            return;
        };
        let start_beat = start_beat.max(0.0);
        let length_beats = DEFAULT_CLIP_LENGTH;

        let content_id = self.song.alloc_content(
            common::model::ClipContent::Text(common::model::TextContent {
                events: vec![common::model::TextEvent {
                    text: "Title".into(),
                    event_length_beats: length_beats,
                    ..common::model::TextEvent::default()
                }],
            }),
            // デフォルトでクリップ名は無し。 表示名は clip_display_label が
            // TextEvent.text ("Title") から導出する (= 名前 == 本文)。
            String::new(),
        );

        let track = &mut self.song.tracks[track_idx];
        let clip_id = track.alloc_clip_id();
        let new_clip_idx = track.clips.len() as u32;
        track.clips.push(common::model::Clip {
            id: clip_id,
            name: String::new(),
            start_beat,
            length_beats,
            content_id,
            notes: Vec::new(),
            color: None,
            auto_lipsync: false,
        });

        // create_clip と同様、 生成直後の clip を選択して inspector に出す。
        let r = ClipRef {
            track: track_idx as u32,
            clip: new_clip_idx,
        };
        self.set_single_clip_selection(r);
        self.selected_notes.clear();
        self.select_track(track_idx as u32);

        self.is_dirty = true;
        self.status_message = "Text clip 追加".into();
        self.sync_song_to_plugin_host();
    }

    /// File menu → "Import Image..." 経路。 `rfd` の native file picker
    /// (multi-select、 png/jpg/jpeg/webp/bmp/gif filter) を開いて、 選択
    /// された path を `action_import_image` に転送する。 OS-neutral
    /// (= image crate のみ、 cfg(windows) 不要)。
    fn action_open_import_image_dialog(&mut self) {
        let Some(paths) = rfd::FileDialog::new()
            .add_filter(
                "Image",
                &["png", "jpg", "jpeg", "webp", "bmp", "tif", "tiff", "tga", "gif"],
            )
            .set_title("Import Image...")
            .pick_files()
        else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        // dialog 経由は drop 位置情報が無いので target_track_idx / target_beat = None
        // (= 新規 track を先頭に作って貼る、従来挙動)。
        self.action_import_image(paths, None, None);
    }

    /// File menu → "Export Video..." (`docs/plan_video.md` P8)。
    /// **mp4 出力先を選ぶダイアログ 1 つだけ**。プロジェクト音声は temp WAV へ
    /// 自動レンダリング（daw_audio の freewheel）し、完了（`ExportWavComplete`）
    /// 後に video export して mux する（`action_export_mp4`）。旧仕様の「音声
    /// WAV を別途選ばせる 2 つ目のダイアログ」 は廃止。
    #[cfg(windows)]
    fn action_open_export_mp4_dialog(&mut self) {
        if self.export_progress.is_some() || self.pending_video_export.is_some() {
            self.status_message = "Video export を実行中です".into();
            return;
        }
        let default_name = self
            .file_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(|s| format!("{s}.mp4"))
            .unwrap_or_else(|| "untitled.mp4".into());
        let Some(output_path) = rfd::FileDialog::new()
            .add_filter("MP4 Video", &["mp4"])
            .set_file_name(&default_name)
            .set_title("Export Video to MP4...")
            .save_file()
        else {
            return;
        };
        // 音声を temp WAV へ自動レンダリング → 完了後に video export + mux。
        let temp_wav = std::env::temp_dir()
            .join(format!("daw01_export_audio_{}.wav", std::process::id()));
        self.pending_video_export = Some(output_path);
        self.export_temp_wav = Some(temp_wav.clone());
        self.status_message = "音声をレンダリング中...".into();
        // 再生中なら停止（freewheel と realtime play の競合を回避）。
        if self.is_playing {
            self.stop();
        }
        let song = self.song.clone();
        self.send_audio(MainToChild::LoadSong(song));
        self.send_plugin(MainToChild::SetRenderMode(
            common::protocol::RenderMode::Offline,
        ));
        self.send_audio(MainToChild::ExportWav { path: temp_wav });
    }

    /// Synchronous mp4 render (`docs/plan_video.md` P8). Blocks the
    /// GUI thread for the duration — typical 1-minute MV at 1080p30
    /// finishes in ~10s on a recent laptop (CPU NV12 conversion is
    /// the bottleneck). Surface progress / completion via
    /// `status_message`; failure surfaces the error there too.
    #[cfg(windows)]
    /// mp4 export を **background thread** で実行する。長尺 / 多レイヤーの
    /// project は 1 フレーム ~100ms（GPU readback + 動画デコード）で数十秒〜
    /// 数分かかるため、 GUI スレッド同期だと UI とファイルダイアログが固まる
    /// （= 旧挙動でハングと誤認されていた）。進捗は `ExportProgress`、完了は
    /// `ExportFinished` を `event_proxy` 経由で送り、 UI が進捗オーバーレイ +
    /// Cancel を出す。
    fn action_export_mp4(
        &mut self,
        output_path: PathBuf,
        audio_wav: Option<PathBuf>,
    ) {
        if self.export_progress.is_some() {
            self.status_message = "Video export を実行中です".into();
            return;
        }
        let project_dir = self
            .file_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        let song = self.song.clone();
        let proxy = self.event_proxy.clone();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.export_cancel = Some(cancel.clone());
        self.export_progress = Some((0, 0));
        self.status_message = format!("Video export 開始: {}", output_path.display());
        std::thread::spawn(move || {
            let cfg = crate::render_video::RenderConfig::new(&song, &output_path)
                .with_project_dir(project_dir.as_deref())
                .with_audio_wav(audio_wav.as_deref());
            // 進捗は 5 フレームごと（+ 開始 / 完了）に間引いて送る（毎フレーム
            // 送ると event queue を圧迫する）。
            let mut last_sent = 0u64;
            let mut on_progress = |done: u64, total: u64| {
                if done == 0 || done >= total || done.saturating_sub(last_sent) >= 5 {
                    last_sent = done;
                    proxy.send(AppEvent::ExportProgress { done, total });
                }
            };
            let result = crate::render_video::render_mp4_cancellable(
                &cfg,
                &cancel,
                &mut on_progress,
            )
            .map(|stats| stats.output_path);
            proxy.send(AppEvent::ExportFinished { result });
        });
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
        // Audio Editor が開いていて、 マウスが waveform 領域内にある
        // ときは「audio_editor_clip を audio editor のマウス hover 位置
        // で split」 として優先処理する。 audio editor は bottom_panel
        // 内なので arrangement_hover_beat は更新されず、 既存 path だと
        // 「マウスを arrangement に置いて...」 status で no-op になる。
        // Audio Editor 上の波形領域に **マウスが乗っているとき** だけ
        // event 分割に振り分ける。 Audio Editor が開いていてもマウスが
        // arrangement 上にある場合は通常の clip 分割パスを使う (= ユーザー
        // は arrangement の clip を分割したいのでそのまま流す)。
        if self.audio_editor_clip.is_some()
            && self.audio_editor_hover_beat_in_clip.is_some()
        {
            self.action_split_audio_editor_event_at_cursor();
            return;
        }

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
            self.selected_clip_refs()
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
            self.select_new_clips(&new_selection);
            self.selected_notes.clear();
        }
        self.status_message = format!("Split: {split_count} clip を分割しました");
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
    }

    /// Audio Editor が開いているとき、 cursor 位置 (= マウス hover、
    /// fallback で playhead) が乗っている event を 2 つに分割する。
    /// `audio_editor_clip` は変更せず、 audio content の events Vec に
    /// 後半 event を `event_idx + 1` の位置に挿入。 fade_out (前半側) と
    /// fade_in (後半側) は 0 にリセット (Bitwig / Reaper の split 慣行)。
    /// 選択は後半 event に移動。
    ///
    /// 戻り値は分割成功時 `true`。 cursor が解決できない / event 上に
    /// 乗っていない場合は status_message を出して `false` を返す。
    fn action_split_audio_editor_event_at_cursor(&mut self) -> bool {
        let Some(target) = self.audio_editor_clip else {
            return false;
        };

        // cursor 位置 (clip 内 beat)。 hover (= マウスが waveform 上)
        // を最優先、 無ければ playhead が clip 内なら playhead を使う。
        let in_clip_beat: Option<f64> = self
            .audio_editor_hover_beat_in_clip
            .or_else(|| {
                let ph = self.playhead_beat? as f64;
                let clip = self
                    .song
                    .tracks
                    .get(target.track as usize)?
                    .clips
                    .get(target.clip as usize)?;
                let in_clip = ph - clip.start_beat;
                (in_clip >= 0.0 && in_clip < clip.length_beats).then_some(in_clip)
            });
        let Some(in_clip_beat) = in_clip_beat else {
            self.status_message =
                "Split: マウスを Audio Editor の波形上に置くか playhead を clip 内に置いてください"
                    .into();
            return false;
        };

        // event_idx を解決 (= cursor が strict interior に乗っている event)。
        let track = self
            .song
            .tracks
            .get(target.track as usize);
        let clip = track.and_then(|t| t.clips.get(target.clip as usize));
        let Some(clip) = clip else { return false };
        let content_id = clip.content_id;
        let Some(common::model::ClipContent::Audio(audio_ro)) =
            self.song.clip_contents.get(&content_id)
        else {
            return false;
        };
        let event_idx_opt = audio_ro.events.iter().position(|e| {
            let s = e.event_start_in_clip_beats;
            let l = e.event_length_beats;
            in_clip_beat > s + 1e-9 && in_clip_beat < s + l - 1e-9
        });
        let Some(event_idx) = event_idx_opt else {
            self.status_message =
                "Split: カーソル位置に分割可能な event がありません".into();
            return false;
        };
        // 元 event を clone して詳細パラメータを後半 event にコピー。
        let event = audio_ro.events[event_idx].clone();

        // mut 取り直し → 分割実行。
        let Some(common::model::ClipContent::Audio(audio_mut)) =
            self.song.clip_contents.get_mut(&content_id)
        else {
            return false;
        };

        let offset_in_event = in_clip_beat - event.event_start_in_clip_beats;
        let len_beats = event.event_length_beats.max(1e-9);
        let event_len_frames = event
            .source_end_frames
            .saturating_sub(event.source_start_frames);
        let frame_offset = ((offset_in_event / len_beats) * event_len_frames as f64)
            .round()
            .clamp(0.0, event_len_frames as f64) as u64;

        // reversed のときは clip 時間 → source frame の対応が逆向き
        // (event_start に source_end が、 event_end に source_start が
        // 対応)。 split frame も反転して計算する。
        let (front_ss, front_se, back_ss, back_se) = if event.reversed {
            let mid = event.source_end_frames.saturating_sub(frame_offset);
            (mid, event.source_end_frames, event.source_start_frames, mid)
        } else {
            let mid = event.source_start_frames + frame_offset;
            (event.source_start_frames, mid, mid, event.source_end_frames)
        };

        // 前半 event を in-place で更新 (= event_start は変えず、 length と
        // source 範囲を縮める)。 fade_out は split で消す (右端が新しく
        // なったので元 fade_out 値は意味を失う)。
        {
            let front = &mut audio_mut.events[event_idx];
            front.source_start_frames = front_ss;
            front.source_end_frames = front_se;
            front.event_length_beats = offset_in_event;
            front.fade_out_beats = 0.0;
        }

        // 後半 event は元 event のパラメータ (gain / pan / pitch / fade /
        // stretch / reversed / muted / onsets / beat_markers) を引き継ぐ。
        // event_start は cursor 位置、 length は残り、 source は分割後の
        // 後半側、 fade_in は 0 にリセット (左端が新しいため)。
        let mut back = event.clone();
        back.source_start_frames = back_ss;
        back.source_end_frames = back_se;
        back.event_start_in_clip_beats = in_clip_beat;
        back.event_length_beats = (len_beats - offset_in_event).max(0.0);
        back.fade_in_beats = 0.0;
        audio_mut.events.insert(event_idx + 1, back);

        // 選択は後半 event (= ユーザーは「分割直後に新規 event を編集
        // したい」 ことが多い、 Reaper / Bitwig 流)。
        self.audio_editor_selected_events = vec![event_idx + 1];
        self.status_message = "Split: event を分割しました".into();
        self.is_dirty = true;
        self.sync_song_to_plugin_host();
        if self.clip_edit_buffer_target == Some(target) {
            self.resync_clip_audio_event_edit_buffers(target);
        }
        true
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
        // 色 (per-clip) は両半が引き継ぐ (= 色付き clip を split したら両方同色)。
        // front は clip_mut をそのまま使うので色は不変、 back の新 clip にこれを写す。
        let src_color = clip.color;
        if !(playhead > clip_start && playhead < clip_end) {
            return false; // playhead 範囲外 / 端ぴったりは split 不要
        }
        let split_offset = playhead - clip_start;
        let front_len = split_offset;
        let back_len = clip_len - split_offset;
        let src_content_id = clip.content_id;
        // 名前は content_id 単位 SSoT から取得 (legacy clip.name は v20 で空)。
        let src_name = self.song.content_name(src_content_id).to_string();
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
            // Automation clips live on `Track.automation_lanes`, not in
            // `Track.clips`. Reaching here means the content store has
            // a stale Automation entry referenced from a MIDI/Audio
            // clip — refuse to split rather than guess.
            ClipContent::Automation(_) => return false,
            // Video clip split (docs/plan_video.md §4 P6). Mirrors the
            // Audio path: partition events front/back by split_offset,
            // straddling events get source_micros range proportionally
            // bisected (= linear partition; CFR assumption holds since
            // MVP doesn't expose time-stretch). Both halves allocate
            // fresh content_ids so the linked-clip semantics of the
            // source clip don't follow the split.
            ClipContent::Video(mut video) => {
                let mut back_events: Vec<common::model::VideoEvent> = Vec::new();
                let mut keep_front: Vec<common::model::VideoEvent> = Vec::new();
                for ev in video.events.drain(..) {
                    let e_start = ev.event_start_in_clip_beats;
                    let e_end = e_start + ev.event_length_beats;
                    if e_end <= split_offset {
                        keep_front.push(ev);
                    } else if e_start >= split_offset {
                        back_events.push(common::model::VideoEvent {
                            event_start_in_clip_beats: e_start - split_offset,
                            ..ev
                        });
                    } else {
                        let frac_front = (split_offset - e_start) / ev.event_length_beats;
                        let total_src =
                            ev.source_end_micros.saturating_sub(ev.source_start_micros);
                        let split_src_offset =
                            (total_src as f64 * frac_front).round() as u64;
                        let mid_src_micros = ev.source_start_micros + split_src_offset;
                        let mut front_ev = ev.clone();
                        front_ev.event_length_beats = split_offset - e_start;
                        front_ev.source_end_micros = mid_src_micros;
                        keep_front.push(front_ev);
                        back_events.push(common::model::VideoEvent {
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: e_end - split_offset,
                            source_start_micros: mid_src_micros,
                            ..ev
                        });
                    }
                }
                let front = common::model::VideoContent {
                    events: keep_front,
                };
                let back = common::model::VideoContent {
                    events: back_events,
                };
                let front_id = self.song.alloc_content_id();
                self.song
                    .clip_contents
                    .insert(front_id, ClipContent::Video(front));
                ClipContent::Video(back)
            }
            // Image clip Split (`docs/plan_image_overlay.md` §4 P4)。
            // Audio / Video と同じ「event を split_offset で前後に振り分け」
            // pattern。 ImageEvent は単一画像 source への参照 + PiP rect /
            // opacity のみ持つので、 source 切り出し位置 (source_*_frames /
            // source_*_micros) のような時間軸 attribute は無い。 そのため
            // straddle event は時間長 (event_length_beats) だけを 2 つに
            // 分割し、 PiP rect / opacity / source_id は両 event が共有
            // (= 同じ画像を 2 つの時間 region で表示し続ける)。 fade_out
            // (前半側) / fade_in (後半側) は 0 にリセット (Audio / Video
            // と同じ split 慣行)。
            ClipContent::Image(mut image) => {
                let mut back_events: Vec<common::model::ImageEvent> = Vec::new();
                let mut keep_front: Vec<common::model::ImageEvent> = Vec::new();
                for ev in image.events.drain(..) {
                    let e_start = ev.event_start_in_clip_beats;
                    let e_end = e_start + ev.event_length_beats;
                    if e_end <= split_offset {
                        keep_front.push(ev);
                    } else if e_start >= split_offset {
                        back_events.push(common::model::ImageEvent {
                            event_start_in_clip_beats: e_start - split_offset,
                            ..ev
                        });
                    } else {
                        let mut front_ev = ev.clone();
                        front_ev.event_length_beats = split_offset - e_start;
                        front_ev.fade_out_beats = 0.0;
                        keep_front.push(front_ev);
                        back_events.push(common::model::ImageEvent {
                            event_start_in_clip_beats: 0.0,
                            event_length_beats: e_end - split_offset,
                            fade_in_beats: 0.0,
                            ..ev
                        });
                    }
                }
                let front = common::model::ImageContent { events: keep_front };
                let back = common::model::ImageContent { events: back_events };
                let front_id = self.song.alloc_content_id();
                self.song
                    .clip_contents
                    .insert(front_id, ClipContent::Image(front));
                ClipContent::Image(back)
            }
            // Text clip Split (`docs/plan_text_overlay.md` §2.2)。 image
            // split と同 idiom: event を split_offset で前後に振り分け。
            // text 内容 / font / color 等は両半が共有 (= 同 text の 2 つ
            // の時間 region で表示)、 fade_out (前半) / fade_in (後半) は
            // 0 リセット。 後 commit で実装、 まずは split skip で
            // build を通す。
            ClipContent::Text(_) => return false,
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
        // 両半は元 clip の共有名を引き継ぐ (split は両側を fresh content_id に
        // fork するので、 名前も両方へ複製する)。
        if !src_name.is_empty() {
            self.song
                .set_content_name(front_content_id, src_name.clone());
            self.song.set_content_name(back_content_id, src_name.clone());
        }

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
            name: String::new(),
            start_beat: clip_start + front_len,
            length_beats: back_len,
            content_id: back_content_id,
            notes: Vec::new(),
            color: src_color,
            auto_lipsync: false,
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
        for r in self.selected_clip_refs() {
            by_track.entry(r.track).or_default().push(r);
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

            // Detect mixed kinds. Glue is only valid within a single
            // ClipContent variant (= can't merge audio + video). The 3-way
            // enum extends the old `Option<bool>` (= Audio vs MIDI) so
            // Video clips are also eligible for Glue (docs/plan_video.md
            // §4 P6).
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum GlueKind {
                Midi,
                Audio,
                Video,
                Image,
            }
            let mut glue_kind: Option<GlueKind> = None;
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
                let this_kind = match content {
                    ClipContent::Midi(_) => GlueKind::Midi,
                    ClipContent::Audio(_) => GlueKind::Audio,
                    ClipContent::Video(_) => GlueKind::Video,
                    ClipContent::Image(_) => GlueKind::Image,
                    // Automation clips don't live in `Track.clips` so a
                    // stale link here is unreachable, but be defensive
                    // and treat as a kind change to abort.
                    ClipContent::Automation(_) => {
                        had_mixed_kind = true;
                        break;
                    }
                    // Text clip Glue は後 commit で実装。 まずは「混在」
                    // 扱いで abort、 Image / Video / Audio / MIDI 同士は
                    // 動作維持。
                    ClipContent::Text(_) => {
                        had_mixed_kind = true;
                        break;
                    }
                };
                match glue_kind {
                    None => glue_kind = Some(this_kind),
                    Some(prev) if prev != this_kind => {
                        had_mixed_kind = true;
                        break;
                    }
                    _ => {}
                }
            }
            if had_mixed_kind {
                continue;
            }
            let glue_kind = match glue_kind {
                Some(k) => k,
                None => continue,
            };

            // Compute combined range + collect content fragments.
            let mut combined_start = f64::INFINITY;
            let mut combined_end = f64::NEG_INFINITY;
            let mut combined_name = String::new();
            #[derive(Default)]
            struct Fragments {
                midi_notes: Vec<Note>,
                audio_events: Vec<AudioEvent>,
                video_events: Vec<common::model::VideoEvent>,
                image_events: Vec<common::model::ImageEvent>,
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
                    combined_name =
                        self.song.content_name(clip.content_id).to_string();
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
                    // Same as the split path above: an Automation
                    // variant referenced from `Track.clips` is a
                    // stale link, skip silently.
                    ClipContent::Automation(_) => {}
                    // Image Glue (`docs/plan_image_overlay.md` §4 P4):
                    // Audio と同じ shift logic。 PiP rect / opacity /
                    // fade / source_id は per-event なので clone してから
                    // event_start を offset するだけ。
                    ClipContent::Image(image) => {
                        for ev in &image.events {
                            frags.image_events.push(common::model::ImageEvent {
                                event_start_in_clip_beats: ev
                                    .event_start_in_clip_beats
                                    + offset_into_combined,
                                ..ev.clone()
                            });
                        }
                    }
                    // Video Glue (docs/plan_video.md §4 P6): same shift
                    // logic as Audio. source_micros range stays as-is
                    // per event since Glue doesn't change content
                    // mapping, only repositions the events on the
                    // combined timeline.
                    ClipContent::Video(video) => {
                        for ev in &video.events {
                            frags.video_events.push(common::model::VideoEvent {
                                event_start_in_clip_beats: ev
                                    .event_start_in_clip_beats
                                    + offset_into_combined,
                                ..ev.clone()
                            });
                        }
                    }
                    // Text clip Glue は後 commit で実装。 abort 済み
                    // (had_mixed_kind = true) なので reach 不能、 防衛的
                    // に no-op。
                    ClipContent::Text(_) => {}
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
            let new_content = match glue_kind {
                GlueKind::Audio => ClipContent::Audio(AudioContent {
                    events: frags.audio_events,
                }),
                GlueKind::Video => ClipContent::Video(common::model::VideoContent {
                    events: frags.video_events,
                }),
                GlueKind::Image => ClipContent::Image(common::model::ImageContent {
                    events: frags.image_events,
                }),
                GlueKind::Midi => {
                    let mut notes = frags.midi_notes;
                    notes.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
                    ClipContent::Midi(MidiContent { notes })
                }
            };
            self.song.clip_contents.insert(new_content_id, new_content);
            // merged clip の名前は content_id 単位 SSoT へ。
            if !combined_name.is_empty() {
                self.song
                    .set_content_name(new_content_id, combined_name.clone());
            }

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
                name: String::new(),
                start_beat: combined_start,
                length_beats: combined_len,
                content_id: new_content_id,
                notes: Vec::new(),
                color: None,
                auto_lipsync: false,
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
                "Glue: MIDI / Audio / Video / Image / Vocal clip が混在しているため Glue できません"
                    .into();
            return;
        }
        if glued_count == 0 {
            tracing::warn!("Glue: glued_count==0 (no track had 2+ clips)");
            self.status_message =
                "Glue: 同じ track 上で 2 つ以上の clip を選択してください".into();
            return;
        }

        tracing::info!(glued_count, ?new_refs, "Glue completed");
        self.select_new_clips(&new_refs);
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
/// 画像を `preview_resolution` 内に **アスペクト比維持で中央 fit** する
/// PiP rect を計算する。 `(x, y, w, h)` は normalized 0..=1。 画像が
/// preview より横長なら横一杯 + 上下に余白、 縦長なら縦一杯 + 左右に
/// 余白。 image / preview 寸法が 0 のときは安全側で全画面 `(0,0,1,1)`。
fn aspect_fit_pip_rect(
    preview_resolution: (u32, u32),
    image_size: (u32, u32),
) -> (f32, f32, f32, f32) {
    let (pw, ph) = preview_resolution;
    let (iw, ih) = image_size;
    if pw == 0 || ph == 0 || iw == 0 || ih == 0 {
        return (0.0, 0.0, 1.0, 1.0);
    }
    let preview_aspect = pw as f32 / ph as f32;
    let image_aspect = iw as f32 / ih as f32;
    if image_aspect >= preview_aspect {
        // 画像が横長 → 横幅一杯、 縦に余白。
        let w = 1.0_f32;
        let h = preview_aspect / image_aspect;
        let x = 0.0_f32;
        let y = (1.0 - h) * 0.5;
        (x, y, w, h)
    } else {
        // 画像が縦長 → 縦一杯、 横に余白。
        let h = 1.0_f32;
        let w = image_aspect / preview_aspect;
        let y = 0.0_f32;
        let x = (1.0 - w) * 0.5;
        (x, y, w, h)
    }
}

/// 画像ドロップの配置先を解決する (`action_import_image` の core 判定)。
/// `target_track_idx` は arrangement の drop 位置 (`position.y / row_h`) から
/// 算出した track index。
/// - `Some(idx)` (= 既存 track を指す): その track に画像 clip を貼る。
/// - `None`: 新規 track を作って貼る。範囲外 index (= track の無い下の領域への
///   ドロップ) と入力 `None` (= dialog 経由で位置情報なし) はどちらもこちら。
fn resolve_image_drop_target(target_track_idx: Option<u32>, n_tracks: usize) -> Option<usize> {
    target_track_idx.and_then(|i| {
        let i = i as usize;
        (i < n_tracks).then_some(i)
    })
}

#[cfg(test)]
mod image_drop_target_tests {
    use super::resolve_image_drop_target;

    #[test]
    fn resolves_existing_track_or_falls_back_to_new() {
        // 既存 track を指す → その index に貼り付け。
        assert_eq!(resolve_image_drop_target(Some(0), 3), Some(0));
        assert_eq!(resolve_image_drop_target(Some(2), 3), Some(2));
        // 範囲外 index (= track の無い下の領域へのドロップ) → 新規 track (None)。
        assert_eq!(resolve_image_drop_target(Some(3), 3), None);
        assert_eq!(resolve_image_drop_target(Some(99), 3), None);
        // dialog 経由 (= 入力 None、位置情報なし) → 新規 track (None)。
        assert_eq!(resolve_image_drop_target(None, 3), None);
        // track が 0 本 → 何を指しても新規 track。
        assert_eq!(resolve_image_drop_target(Some(0), 0), None);
        assert_eq!(resolve_image_drop_target(None, 0), None);
    }
}

/// FIXME #30: 新規 track の挿入 index を決めるピュアロジック。 選択中の
/// track が 1 つ以上あれば「最下段 (= `tracks` 内 index 最大) の選択の直後」、
/// 無ければ末尾。 複数選択でも一番下を基準にすることで選択のかたまりを割らない。
/// `selected` 内の stale id (= `tracks` に存在しない) は `position()` が `None` を
/// 返すので自然に無視され、 全部 stale なら末尾に fallback する。
fn add_track_insert_index(tracks: &[Track], selected: &[u32]) -> usize {
    selected
        .iter()
        .filter_map(|sid| tracks.iter().position(|t| t.id == *sid))
        .max()
        .map_or(tracks.len(), |bottom| bottom + 1)
}

#[cfg(test)]
mod add_track_insert_index_tests {
    use super::add_track_insert_index;
    use common::model::Track;

    fn tracks(ids: &[u32]) -> Vec<Track> {
        ids.iter()
            .map(|&id| Track { id, ..Track::default() })
            .collect()
    }

    #[test]
    fn appends_at_end_when_no_selection() {
        let t = tracks(&[10, 11, 12]);
        assert_eq!(add_track_insert_index(&t, &[]), 3);
    }

    #[test]
    fn inserts_after_single_selected() {
        let t = tracks(&[10, 11, 12]);
        // 先頭 (id 10, index 0) を選択 → 直後 = index 1。
        assert_eq!(add_track_insert_index(&t, &[10]), 1);
        // 中央 (id 11, index 1) → index 2。
        assert_eq!(add_track_insert_index(&t, &[11]), 2);
        // 末尾 (id 12, index 2) → index 3 (= 末尾と一致)。
        assert_eq!(add_track_insert_index(&t, &[12]), 3);
    }

    #[test]
    fn inserts_after_bottom_most_of_multi_selection() {
        let t = tracks(&[10, 11, 12, 13]);
        // 選択 {10, 12} の最下段は 12 (index 2) → index 3。 vec の順序に依らない。
        assert_eq!(add_track_insert_index(&t, &[10, 12]), 3);
        assert_eq!(add_track_insert_index(&t, &[12, 10]), 3);
        // {11, 12} の最下段は 12 (index 2) → index 3。
        assert_eq!(add_track_insert_index(&t, &[11, 12]), 3);
    }

    #[test]
    fn stale_ids_fall_back_to_end() {
        let t = tracks(&[10, 11]);
        // 全部 stale → 末尾。
        assert_eq!(add_track_insert_index(&t, &[999, 1000]), 2);
        // 一部 stale → 生きている最下段 (id 10, index 0) の直後。
        assert_eq!(add_track_insert_index(&t, &[10, 999]), 1);
    }

    #[test]
    fn empty_track_list() {
        assert_eq!(add_track_insert_index(&[], &[]), 0);
        assert_eq!(add_track_insert_index(&[], &[5]), 0);
    }
}

fn frames_to_beats(frames: u64, sample_rate: u32, bpm: f32) -> f64 {
    if sample_rate == 0 || bpm <= 0.0 {
        return 0.0;
    }
    let secs = frames as f64 / sample_rate as f64;
    secs * (bpm as f64) / 60.0
}

/// `docs/plan_video.md`: μs duration → project beats. Used by
/// `action_import_video` to set the visual length of the auto-created
/// video clip to its native duration at the project's current tempo.
fn micros_to_beats(micros: u64, bpm: f32) -> f64 {
    if bpm <= 0.0 {
        return 0.0;
    }
    let secs = micros as f64 / 1_000_000.0;
    secs * (bpm as f64) / 60.0
}


/// `app_dirs` から解決した path で recent list を復元。 path が `None`
/// (= 永続化先なし) や読み込み失敗時は空 list を返す (起動を妨げない)。
fn load_recent_list(path: Option<PathBuf>) -> common::recent::RecentFiles {
    let Some(path) = path else {
        return Default::default();
    };
    match common::recent::load(&path) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = ?e, path = %path.display(), "recent list load failed");
            Default::default()
        }
    }
}

/// 選択ノートを複製して `notes` 末尾に追加するピュアロジック (D キー複製の core)。
/// `selected` の各 index のノートを clone し、選択範囲の beat span
/// (`max(start+dur) - min(start)`、最低 1/16 拍) ぶん後ろへずらして append する。
/// 元ノートは不変。戻り値は複製ノートの新 index (= 複製後の選択)。
/// 選択が空 / 該当 index 無しなら `notes` 不変で空 Vec を返す。
fn duplicate_notes_into(notes: &mut Vec<Note>, selected: &[u32]) -> Vec<u32> {
    let mut clones: Vec<Note> = selected
        .iter()
        .filter_map(|&idx| notes.get(idx as usize).cloned())
        .collect();
    if clones.is_empty() {
        return Vec::new();
    }
    let min_start = clones
        .iter()
        .map(|n| n.start_beat)
        .fold(f64::INFINITY, f64::min);
    let max_end = clones
        .iter()
        .map(|n| n.start_beat + n.duration_beats)
        .fold(f64::NEG_INFINITY, f64::max);
    let offset = (max_end - min_start).max(0.0625);
    for n in &mut clones {
        n.start_beat += offset;
    }
    let base = notes.len() as u32;
    let count = clones.len() as u32;
    notes.append(&mut clones);
    (base..base + count).collect()
}

/// gui_01 #054 (Ctrl+drag コピー) の core。`entries` = [(source note index,
/// new_start_beat, new_pitch)]。各 source を clone し start_beat/pitch を指定値にして
/// `notes` 末尾へ追加。元は不変。戻り値は複製の新 index。該当 index 無しなら不変で空 Vec。
fn copy_notes_into(notes: &mut Vec<Note>, entries: &[(u32, f64, u8)]) -> Vec<u32> {
    let mut clones: Vec<Note> = Vec::new();
    for &(idx, new_beat, new_pitch) in entries {
        if let Some(src) = notes.get(idx as usize) {
            let mut c = src.clone();
            c.start_beat = new_beat.max(0.0);
            c.pitch = new_pitch;
            clones.push(c);
        }
    }
    if clones.is_empty() {
        return Vec::new();
    }
    let base = notes.len() as u32;
    let count = clones.len() as u32;
    notes.append(&mut clones);
    (base..base + count).collect()
}

/// `SetClipColor` の core: target clip の `content_id` を共有する全 track の全 clip へ
/// `color` を伝播する (= 共有クリップの色を変えれば共有先全部が同色、 cross-track 含む)。
/// `content_id == 0` (未採番 sentinel) のときは伝播せず target clip のみ塗る (defensive、
/// 別々の未採番 clip を巻き込まない)。target が範囲外なら何もしない。
///
/// 「クリップ色をトラックに揃える」(`ResetTrackClipColors`) は逆に **track-scoped** で
/// 他 track の共有 clip を変えない — 色は per-clip 所有 (`Clip.color`) なので、 SET 伝播 /
/// RESET track-local の両立ができる (`docs/plan_track_clip_color.md` 追加要件)。
fn propagate_clip_color(tracks: &mut [Track], target: ClipRef, color: Option<[f32; 3]>) {
    let content_id = tracks
        .get(target.track as usize)
        .and_then(|t| t.clips.get(target.clip as usize))
        .map(|c| c.content_id);
    match content_id {
        Some(cid) if cid != 0 => {
            for t in tracks.iter_mut() {
                for clip in t.clips.iter_mut().filter(|c| c.content_id == cid) {
                    clip.color = color;
                }
            }
        }
        _ => {
            if let Some(clip) = tracks
                .get_mut(target.track as usize)
                .and_then(|t| t.clips.get_mut(target.clip as usize))
            {
                clip.color = color;
            }
        }
    }
}

#[cfg(test)]
mod clip_color_tests {
    use super::{ClipRef, propagate_clip_color};
    use common::model::{Clip, Track};

    fn clip(id: u32, content_id: u32) -> Clip {
        Clip { id, content_id, length_beats: 4.0, ..Clip::default() }
    }

    #[test]
    fn set_color_propagates_to_all_clips_sharing_content_cross_track() {
        // track0: cid=7 と cid=9、 track1: cid=7 (linked, cross-track)。
        let mut tracks = vec![
            Track { id: 1, clips: vec![clip(1, 7), clip(2, 9)], ..Track::default() },
            Track { id: 2, clips: vec![clip(3, 7)], ..Track::default() },
        ];
        propagate_clip_color(&mut tracks, ClipRef { track: 0, clip: 0 }, Some([0.9, 0.3, 0.3]));
        // cid==7 は cross-track 含め全部同色、 cid==9 は不変 (= 確定動作 1)。
        assert_eq!(tracks[0].clips[0].color, Some([0.9, 0.3, 0.3]));
        assert_eq!(tracks[1].clips[0].color, Some([0.9, 0.3, 0.3]));
        assert_eq!(tracks[0].clips[1].color, None);
    }

    #[test]
    fn set_color_content_id_zero_colors_only_target() {
        // content_id == 0 (未採番 sentinel) は伝播せず target のみ (別の cid==0 を巻き込まない)。
        let mut tracks =
            vec![Track { id: 1, clips: vec![clip(1, 0), clip(2, 0)], ..Track::default() }];
        propagate_clip_color(&mut tracks, ClipRef { track: 0, clip: 0 }, Some([0.1, 0.2, 0.3]));
        assert_eq!(tracks[0].clips[0].color, Some([0.1, 0.2, 0.3]));
        assert_eq!(tracks[0].clips[1].color, None);
    }

    #[test]
    fn set_color_out_of_range_target_is_noop() {
        let mut tracks = vec![Track { id: 1, clips: vec![clip(1, 7)], ..Track::default() }];
        propagate_clip_color(&mut tracks, ClipRef { track: 5, clip: 0 }, Some([0.5, 0.5, 0.5]));
        assert_eq!(tracks[0].clips[0].color, None);
    }
}

/// [`diff_preview`] が返す 1 アクション (鍵盤プレビューの note-on/off)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewAction {
    NoteOff { track_id: u32, pitch: u8 },
    NoteOn { track_id: u32, pitch: u8 },
}

/// 鍵盤プレビューの状態遷移 (gui_01 #055)。 現在発音中の note `prev` と今フレーム
/// の目標 `next` (どちらも `(track_id, pitch)`) を比較し、 発行すべき note-off /
/// note-on を順に返す (held-value + caller diff)。
/// - `None → Some`: `[NoteOn]`
/// - `Some(a) → Some(b)` (a≠b): `[NoteOff(a), NoteOn(b)]`
/// - `Some → None`: `[NoteOff]`
/// - 変化なし: 空 (= 同 pitch 押下継続中は再送しない)
fn diff_preview(prev: Option<(u32, u8)>, next: Option<(u32, u8)>) -> Vec<PreviewAction> {
    if prev == next {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if let Some((track_id, pitch)) = prev {
        actions.push(PreviewAction::NoteOff { track_id, pitch });
    }
    if let Some((track_id, pitch)) = next {
        actions.push(PreviewAction::NoteOn { track_id, pitch });
    }
    actions
}

#[cfg(test)]
mod preview_tests {
    use super::{PreviewAction, diff_preview};

    #[test]
    fn none_to_some_emits_note_on() {
        assert_eq!(
            diff_preview(None, Some((3, 60))),
            vec![PreviewAction::NoteOn { track_id: 3, pitch: 60 }],
        );
    }

    #[test]
    fn same_pitch_held_emits_nothing() {
        assert_eq!(diff_preview(Some((3, 60)), Some((3, 60))), vec![]);
    }

    #[test]
    fn glissando_emits_off_then_on() {
        // Some(a) → Some(b): 旧 pitch off → 新 pitch on の順 (CLAP の同 time
        // Off→On 要件と整合)。
        assert_eq!(
            diff_preview(Some((3, 60)), Some((3, 62))),
            vec![
                PreviewAction::NoteOff { track_id: 3, pitch: 60 },
                PreviewAction::NoteOn { track_id: 3, pitch: 62 },
            ],
        );
    }

    #[test]
    fn release_emits_note_off() {
        assert_eq!(
            diff_preview(Some((3, 60)), None),
            vec![PreviewAction::NoteOff { track_id: 3, pitch: 60 }],
        );
    }

    #[test]
    fn track_change_retriggers_on_new_track() {
        // 同 pitch でも track が変われば旧 track off + 新 track on。
        assert_eq!(
            diff_preview(Some((3, 60)), Some((5, 60))),
            vec![
                PreviewAction::NoteOff { track_id: 3, pitch: 60 },
                PreviewAction::NoteOn { track_id: 5, pitch: 60 },
            ],
        );
    }
}

#[cfg(test)]
mod note_duplicate_tests {
    use super::{copy_notes_into, duplicate_notes_into};
    use common::model::Note;

    fn note(start: f64, dur: f64, pitch: u8) -> Note {
        Note {
            start_beat: start,
            duration_beats: dur,
            pitch,
            velocity: 100,
            lyric: None,
        }
    }

    #[test]
    fn single_note_duplicated_after_itself() {
        let mut notes = vec![note(0.0, 1.0, 60)];
        let new_ids = duplicate_notes_into(&mut notes, &[0]);
        assert_eq!(new_ids, vec![1]);
        assert_eq!(notes.len(), 2);
        // offset = (0+1) - 0 = 1 → 複製は start 1.0、長さ/pitch は維持
        assert_eq!(notes[1].start_beat, 1.0);
        assert_eq!(notes[1].duration_beats, 1.0);
        assert_eq!(notes[1].pitch, 60);
        // 元ノートは不変
        assert_eq!(notes[0].start_beat, 0.0);
    }

    #[test]
    fn multi_note_keeps_relative_positions_and_shifts_by_span() {
        // [0,1) と [2,3)。選択範囲 span = 3 - 0 = 3。
        let mut notes = vec![note(0.0, 1.0, 60), note(2.0, 1.0, 64)];
        let new_ids = duplicate_notes_into(&mut notes, &[0, 1]);
        assert_eq!(new_ids, vec![2, 3]);
        assert_eq!(notes.len(), 4);
        assert_eq!(notes[2].start_beat, 3.0); // 0 + 3
        assert_eq!(notes[2].pitch, 60);
        assert_eq!(notes[3].start_beat, 5.0); // 2 + 3
        assert_eq!(notes[3].pitch, 64);
    }

    #[test]
    fn subset_selection_duplicates_only_selected() {
        // 3 ノート、index 0 と 2 だけ選択。選択範囲 span = (2+1) - 0 = 3。
        let mut notes = vec![note(0.0, 1.0, 60), note(1.0, 1.0, 62), note(2.0, 1.0, 64)];
        let new_ids = duplicate_notes_into(&mut notes, &[0, 2]);
        assert_eq!(new_ids, vec![3, 4]);
        assert_eq!(notes.len(), 5);
        assert_eq!(notes[3].start_beat, 3.0); // 0 + 3
        assert_eq!(notes[3].pitch, 60);
        assert_eq!(notes[4].start_beat, 5.0); // 2 + 3
        assert_eq!(notes[4].pitch, 64);
        // 選択外の index 1 は複製されず元のまま
        assert_eq!(notes[1].start_beat, 1.0);
    }

    #[test]
    fn empty_selection_is_noop() {
        let mut notes = vec![note(0.0, 1.0, 60)];
        let new_ids = duplicate_notes_into(&mut notes, &[]);
        assert!(new_ids.is_empty());
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn out_of_range_index_ignored() {
        let mut notes = vec![note(0.0, 1.0, 60)];
        let new_ids = duplicate_notes_into(&mut notes, &[5]);
        assert!(new_ids.is_empty());
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn copy_places_clone_at_target_beat_and_pitch() {
        // Ctrl+drag: note0 を beat 4.0 / pitch 67 へコピー。元は据え置き。
        let mut notes = vec![note(0.0, 1.0, 60)];
        let new_ids = copy_notes_into(&mut notes, &[(0, 4.0, 67)]);
        assert_eq!(new_ids, vec![1]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1].start_beat, 4.0);
        assert_eq!(notes[1].pitch, 67);
        assert_eq!(notes[1].duration_beats, 1.0); // 長さは維持
        assert_eq!(notes[0].start_beat, 0.0); // 元は不変
        assert_eq!(notes[0].pitch, 60);
    }

    #[test]
    fn copy_multi_preserves_each_target() {
        let mut notes = vec![note(0.0, 1.0, 60), note(1.0, 0.5, 62)];
        let new_ids = copy_notes_into(&mut notes, &[(0, 2.0, 60), (1, 3.0, 64)]);
        assert_eq!(new_ids, vec![2, 3]);
        assert_eq!(notes[2].start_beat, 2.0);
        assert_eq!(notes[2].pitch, 60);
        assert_eq!(notes[3].start_beat, 3.0);
        assert_eq!(notes[3].pitch, 64);
        assert_eq!(notes[3].duration_beats, 0.5);
    }

    #[test]
    fn copy_empty_entries_is_noop() {
        let mut notes = vec![note(0.0, 1.0, 60)];
        let new_ids = copy_notes_into(&mut notes, &[]);
        assert!(new_ids.is_empty());
        assert_eq!(notes.len(), 1);
    }
}

#[cfg(test)]
mod aspect_fit_tests {
    use super::aspect_fit_pip_rect;

    #[test]
    fn square_image_in_16_9_preview_pillarbox() {
        // 正方形 (1:1) を 16:9 preview → 縦一杯、 左右余白。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (500, 500));
        assert!((h - 1.0).abs() < 1e-5);
        assert!((w - (9.0 / 16.0)).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
        assert!((x - (1.0 - 9.0 / 16.0) * 0.5).abs() < 1e-5);
    }

    #[test]
    fn portrait_image_in_16_9_preview_pillarbox() {
        // 縦長 PNG (例 2894x4613) を 16:9 preview → 縦一杯、 左右に大きな余白。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (2894, 4613));
        assert!((h - 1.0).abs() < 1e-5);
        let expected_w = (2894.0 / 4613.0) / (1920.0 / 1080.0);
        assert!((w - expected_w).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
        assert!((x - (1.0 - expected_w) * 0.5).abs() < 1e-5);
    }

    #[test]
    fn landscape_image_in_16_9_preview_letterbox() {
        // 21:9 (超横長) を 16:9 preview → 横一杯、 上下に余白。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (2100, 900));
        assert!((w - 1.0).abs() < 1e-5);
        let expected_h = (1920.0 / 1080.0) / (2100.0 / 900.0);
        assert!((h - expected_h).abs() < 1e-5);
        assert!((x - 0.0).abs() < 1e-5);
        assert!((y - (1.0 - expected_h) * 0.5).abs() < 1e-5);
    }

    #[test]
    fn same_aspect_fills_preview() {
        // 16:9 を 16:9 preview → 全画面 (= 余白なし)。
        let (x, y, w, h) = aspect_fit_pip_rect((1920, 1080), (1280, 720));
        assert!((w - 1.0).abs() < 1e-5);
        assert!((h - 1.0).abs() < 1e-5);
        assert!((x - 0.0).abs() < 1e-5);
        assert!((y - 0.0).abs() < 1e-5);
    }

    #[test]
    fn zero_dimension_falls_back_to_full_screen() {
        assert_eq!(aspect_fit_pip_rect((0, 1080), (500, 500)), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(aspect_fit_pip_rect((1920, 1080), (0, 500)), (0.0, 0.0, 1.0, 1.0));
    }
}

#[cfg(test)]
mod master_fx_tests {
    use super::{compute_slot_reconcile_actions, SlotReconcileAction};
    use common::model::{MASTER_TRACK_ID, PluginInstance, Song};
    use common::plugin_format::PluginFormat;
    use common::protocol::PluginSlot;
    use std::collections::HashMap;

    #[test]
    fn reconcile_emits_loadslot_for_master_fx() {
        // master_fx_chain に 1 plugin、 host (loaded_slots) は空 → master の
        // Fx(0) に対する LoadSlot が 1 件出る。
        let mut song = Song::default();
        song.master_fx_chain.push(PluginInstance::new(
            "vendor.reverb".to_string(),
            PluginFormat::Clap,
        ));
        let loaded = HashMap::new();
        let actions = compute_slot_reconcile_actions(&song, &loaded);
        assert!(
            actions.iter().any(|a| matches!(
                a,
                SlotReconcileAction::LoadSlot { track_id, slot, plugin_id_str, .. }
                    if *track_id == MASTER_TRACK_ID
                        && *slot == PluginSlot::Fx(0)
                        && plugin_id_str == "vendor.reverb"
            )),
            "master fx に対する LoadSlot が emit されること: {actions:?}"
        );
    }

    #[test]
    fn master_fx_chain_survives_serde_roundtrip_and_forward_migrates() {
        // master_fx_chain 付き Song を JSON 経由で往復しても保持される。
        let mut song = Song::default();
        song.master_fx_chain.push(PluginInstance::new(
            "vendor.eq".to_string(),
            PluginFormat::Clap,
        ));
        let json = serde_json::to_string(&song).unwrap();
        let back: Song = serde_json::from_str(&json).unwrap();
        assert_eq!(back.master_fx_chain.len(), 1);
        assert_eq!(back.master_fx_chain[0].plugin_id, "vendor.eq");

        // master_fx_chain field を持たない旧 file は空 Vec に forward-migrate。
        let legacy = r#"{"bpm":120.0,"time_sig":[4,4],"length_beats":16.0}"#;
        let migrated: Song = serde_json::from_str(legacy).unwrap();
        assert!(migrated.master_fx_chain.is_empty());
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

/// decode 済み audio staging entry (source_id, buffer)。
type DecodedAudio = (
    common::model::AudioSourceId,
    std::sync::Arc<crate::audio_source_cache::AudioSourceBuffer>,
);
/// decode 済み image staging entry (source_id, (w, h, bgra))。
type DecodedImage = (
    common::model::ImageSourceId,
    (u32, u32, std::sync::Arc<Vec<u8>>),
);

/// FIXME #24: background asset decode の中間バッファ。 decode スレッドが結果を
/// push + `done` を進め、 GUI スレッドの `on_asset_decode_tick` が caches へ排出
/// する。
#[derive(Default)]
pub struct AssetDecodeStaging {
    /// decode 済みで未取り込みの audio。
    pub audio: Vec<DecodedAudio>,
    /// 同 image。
    pub image: Vec<DecodedImage>,
    /// 処理済み件数 (成功 + 失敗、 進捗表示用)。
    pub done: usize,
    /// 総件数。
    pub total: usize,
}

/// PNG/JPEG/… を decode して BGRA8 + 寸法を返す (background thread から呼ぶ自由
/// 関数)。 失敗 / 0 サイズは `None`。 旧 `decode_image_sources_into_cache` の
/// image::open → RGBA → BGRA part を抽出したもの。
fn decode_image_to_bgra(abs: &Path) -> Option<(u32, u32, std::sync::Arc<Vec<u8>>)> {
    match image::open(abs) {
        Ok(dynamic) => {
            let rgba = dynamic.into_rgba8();
            let (w, h) = rgba.dimensions();
            if w == 0 || h == 0 {
                tracing::warn!(path = %abs.display(), "asset decode (image): zero-sized");
                return None;
            }
            let mut bytes = rgba.into_raw();
            for px in bytes.chunks_exact_mut(4) {
                px.swap(0, 2); // RGBA → BGRA
            }
            Some((w, h, std::sync::Arc::new(bytes)))
        }
        Err(e) => {
            tracing::warn!(path = %abs.display(), error = %e, "asset decode (image) failed");
            None
        }
    }
}

#[cfg(test)]
mod plugin_category_tests {
    use super::PluginCategory;

    fn feats(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn routing_priority_and_fallback() {
        // 統合ピッカーの自動振り分け規則: 優先順 note-effect > instrument >
        // audio-effect、 どの主カテゴリも無ければ FX チェーンへ (plan_unified_plugin_picker.md)。
        let cases: &[(&[&str], PluginCategory)] = &[
            (&["instrument", "synthesizer"], PluginCategory::Instrument),
            (&["audio-effect"], PluginCategory::Fx),
            (&["audio-effect", "reverb"], PluginCategory::Fx),
            (&["note-effect"], PluginCategory::MidiFx),
            // 音を出す方が勝つ: instrument は audio-effect に優先 (features 順非依存)。
            (&["instrument", "audio-effect"], PluginCategory::Instrument),
            (&["audio-effect", "instrument"], PluginCategory::Instrument),
            // note-effect は最優先 (features 順非依存)。
            (&["note-effect", "instrument"], PluginCategory::MidiFx),
            (&["instrument", "note-effect"], PluginCategory::MidiFx),
            // 未分類 (主カテゴリ無し / 空) は FX チェーンへ倒す。
            (&[], PluginCategory::Fx),
            (&["reverb"], PluginCategory::Fx),
        ];
        for (features, expected) in cases {
            assert_eq!(
                PluginCategory::from_features(&feats(features)),
                *expected,
                "features = {features:?}",
            );
        }
    }
}

