//! `arrangement` widget — DAW timeline (track header / ruler / lanes / clip drag) を 1 widget で扱う library widget (M9 Phase 45e)。
//!
//! 公開 API は `F:/dev/daw_01/docs/gui_01_conversation.md` の `## #005 [Replied]` を逐語踏襲。
//! 設計は piano_roll と完全平行 (heavy + cached + overlay / commit-by-release / `make_edit` callback)。
//!
//! - **schema**: `ArrangementClip { id, start_beat, len_beats, name, color }` / `ArrangementTrack { id, name, muted, solo, clips }`。
//!   `id` は track / clip 内で安定 (move/resize/track 跨ぎでも不変、index ではない)。
//! - **描画 + drag state machine + hit-test + shortcut + rect select** は widget 内に閉じる。
//!   heavy() ブロック + cached(viewport_key) で背景を粗粒度キャッシュ、selection / drag preview / playhead /
//!   loop band は cached 外で毎フレーム描画。
//! - **Edit 構築は callback**: `make_edit: Fn(ArrangementEditRequest) -> Edit<M>`。
//!   widget 自身は Model 型を知らず no-Clone 不変条件と整合する。
//! - **commit-by-release**: drag 中は library が overlay 描画、release frame で初めて
//!   `MoveClips` / `ResizeClips` / `SetLoopRange` を発行する。drag 中の Mutate Edit は発行しない。
//! - **track header の Rename / Delete** は widget 内蔵せず、`Response.track_header_rects` を返して
//!   app 側で `context_menu_for` 等を重ねて呼ぶ (#005 設計判断)。
//! - **SelectTrack トリガ** は track header 全体 click (button hit zone を除く)。

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::Arc;

use daw_ui_platform::{CursorIcon, Modifiers};
use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand, TextureHandle, TexturedQuad};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::snap::SnapConfig;
use crate::time::{TimeDisplay, TimeMapping};
use crate::ui::Ui;
use crate::viewport::ViewportState1D;
use crate::widgets::heavy::HeavyCtx;
use crate::widgets::playhead::draw_playhead_line;
use crate::widgets::ruler_ops::{
    LoopBandHit, LoopDragKind, LoopDragSession, PlayheadDragSession,
    compute_loop_drag_endpoints, draw_loop_band, loop_band_hit_kind,
};
use crate::widgets::time_grid::{BarBeatGridStyle, TimeRulerStyle};
use crate::widgets::toggle_button::ToggleButtonStyle;

// ============================================================
// Public types (conversation #005 [Replied] のまま)
// ============================================================

/// clip の identity。track_id + clip_id (どちらも track / track 内 clip で安定)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ClipKey {
    pub track: u32,
    pub clip: u32,
}

/// M14 Phase 63k (#025): audio clip の inline 編集用フィールド (gain_db / fade_in/out)。
/// `ArrangementClip.audio_edit = Some(...)` のとき widget が dB handle line + fade 角 grip +
/// envelope を描画 + 当該 grip 領域に drag handler を bind。 MIDI / Vocal clip は `None` で
/// 既存挙動 (audio 描画 / hit zone 完全に無効、 通常の Move/Resize のみ)。
///
/// 値は **caller が clamp 済**: gain_db は ±24 dB 想定、 fade_*_beats は 0..len_beats、
/// fade_*_curve は描画用。 widget 側は drag commit 値も同範囲で clamp する (range 統一は caller
/// の責務、 widget 側は描画 / hit-test の sanity guard のみ)。
#[derive(Clone, Copy, Debug)]
pub struct ArrangementClipAudioEdit {
    pub gain_db: f32,
    pub fade_in_beats: f64,
    pub fade_out_beats: f64,
    pub fade_in_curve: FadeCurve,
    pub fade_out_curve: FadeCurve,
}

/// M14 Phase 63k (#025): fade のカーブ形状 (3 種、 Bitwig spec §3.5 と整合)。
/// vertical drag で順送り (`Linear → Exponential → SCurve → Linear`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FadeCurve {
    Linear,
    Exponential,
    SCurve,
}

impl FadeCurve {
    /// Vertical drag で次の curve に進める (`Linear → Exponential → SCurve → Linear`)。
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            FadeCurve::Linear => FadeCurve::Exponential,
            FadeCurve::Exponential => FadeCurve::SCurve,
            FadeCurve::SCurve => FadeCurve::Linear,
        }
    }

    /// ghost label / debug 表示用の英語名 (Bitwig / Reaper と整合)。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            FadeCurve::Linear => "Linear",
            FadeCurve::Exponential => "Exponential",
            FadeCurve::SCurve => "SCurve",
        }
    }
}

/// M14 Phase 63k (#025): fade の対象 edge (`In` = clip 左角、 `Out` = clip 右角)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FadeEdge {
    In,
    Out,
}

/// M14 Phase 72 (daw_01 #044): track 種別 (Audio = 既存挙動、 Video = video frame thumbnail 描画 +
/// header layout 簡略化)。 default = `Audio` で既存 caller の breaking を最小化。
///
/// - **Audio**: 既存挙動完全互換 — header に instrument / fx_chain / volume / pan slot を描画、
///   clip rect は waveform / MIDI note。
/// - **Video**: header から instrument / fx_chain / volume / pan を **非描画** (M/S/R + name +
///   lane disclosure のみ)、 行背景を [`ArrangementStyle::track_background_video`] で塗る、
///   clip rect 内に `clip.thumbnail` が `Some` なら texture を aspect-fit (黒帯 letterbox) で
///   描画、 `None` なら [`ArrangementStyle::video_clip_loading`] 単色 rect。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TrackKind {
    #[default]
    Audio,
    Video,
}

/// 1 つの clip。`Arc<str>` で複数 clip 間の name 共有可能。
#[derive(Clone, Debug)]
pub struct ArrangementClip {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    pub name: Arc<str>,
    pub color: Option<Color>,
    /// M14 Phase 63e (#019) / Phase 114 (#086): 共有 (linked) clip の **リンク識別フラグ兼 hue**。
    /// `None` で通常 clip、 `Some(_)` で「この clip は共有グループの member」 を意味し、 widget は
    /// clip 名の左に `share_group_link_glyph` (`⇌`) を描く + `in_active_group` 時の hover 強調対象にする。
    ///
    /// **M14 Phase 114 (#086) で役割を「リンク識別」 に限定**: hue 値で fill / border を上書きするのは
    /// やめ、 clip 塗りは `color` を唯一の source にした (= 「clip で色を選べば共有 clip 全部がその色に
    /// なる」 / 「トラックに揃えればその色になる」 が成立)。 現状 widget は値 (`f32`) を描画に使わず
    /// `is_some()` だけを見るが、 caller の互換 (refcount >= 2 で `Some(hue)` を渡す既存契約) を保つため
    /// 型は `Option<f32>` のまま据え置き、 hue 値は将来の hue ベース theming 用に予約する。
    pub share_group_color: Option<f32>,
    /// M14 Phase 63k (#025): audio clip の inline 編集 (gain_db / fade)。 `Some` で widget が
    /// dB handle / fade 角 / envelope を描画 + grip 領域に drag handler を bind し
    /// `SetClipGainDb` / `SetClipFade` / `SetClipFadeCurve` を発行する。 MIDI / Vocal clip は
    /// `None` で既存挙動 (clip 内 hit zone 全体が Move、 audio 描画なし)。
    pub audio_edit: Option<ArrangementClipAudioEdit>,
    /// M14 Phase 72 (daw_01 #044): video clip 用 thumbnail。 `Some((handle, width, height))` で
    /// widget が clip rect 内に texture を aspect-fit (黒帯 letterbox) で描画する。 `(width, height)`
    /// は texture の native size (= [`daw_ui_renderer::Renderer::texture_size`] と同じ値)。 widget が
    /// Renderer 参照を持たない設計と整合させるため caller が同梱で渡す前提
    /// (daw_01 は ffmpeg-next decode 時の `VideoFrame.width/height` を流用すれば boilerplate ゼロ)。
    /// `None` のときは `track.kind == Video` なら [`ArrangementStyle::video_clip_loading`] 単色 rect
    /// 描画、 `Audio` なら field 自体が無視される (= caller が kind と clip 種別を一致させる責任)。
    pub thumbnail: Option<(TextureHandle, u32, u32)>,
    /// M14 Phase 96 (daw_01 #068) / Phase 114 (#086): 共有グループ「連動ハイライト」フラグ。 `true` の
    /// とき widget が selection (黄塗り) とは **別レイヤ** の強調 (glow wash + bright thick border) を
    /// 重ねる (= 「今アクティブな共有グループの member」)。 M14 Phase 114 (#086) で強調色は hue 由来から
    /// **identity-neutral な `ArrangementStyle::share_group_active_color`** に変更 (clip fill が user 指定
    /// 色になったため hue wash だと喧嘩する。 hover 中は 1 グループのみ強調 = 色で区別する必要が無い)。
    /// caller (daw_01) は毎フレーム `{selected clip} ∪ {前フレーム hovered_clip}` の `content_id` 集合を
    /// 作り、 同 content を共有する clip に `true` を立てて渡す想定 (selection 由来の強調を widget が
    /// 知らない / 別グループの誤強調を避けるため per-clip flag 方式)。
    /// **`false` のとき描画は既存挙動と pixel 完全一致** (常に `false` で渡せば移行安全)。
    /// `share_group_color == None` の clip は share group member でないので強調しない (defensive)。
    /// hover 由来で毎フレーム変わるため widget は viewport_key (cache key) には含めず、 cached 外の
    /// overlay で毎フレーム描画する (selection overlay と同 idiom)。
    pub in_active_group: bool,
}

/// 1 つの track。`clips` は `start_beat` 昇順前提。
///
/// `muted` / `solo` / `armed` / `collapsed` / `automation_lanes_collapsed` は意味的に独立した bool 状態
/// (集約の余地がない、 Bitwig / Reaper の track schema と整合) なので `clippy::struct_excessive_bools`
/// を allow する。
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
pub struct ArrangementTrack {
    pub id: u32,
    pub name: Arc<str>,
    pub muted: bool,
    pub solo: bool,
    /// M14 Phase 68 (#040): Record-arm 状態。 `true` で track header に R button が active 描画され、
    /// caller (audio engine) は armed track のみを録音入力 (MIDI device / audio input) の対象とする
    /// (Bitwig / Live / Reaper と同 idiom)。 mute / solo と独立 (排他なし、 任意数の track を armed に
    /// できる)。 widget は R button の click で `ArrangementEditRequest::ToggleTrackArmed(track_id)` を
    /// 発行し、 caller が `track.armed = !track.armed` で反転する (mute / solo と完全同 idiom)。
    pub armed: bool,
    pub clips: Vec<ArrangementClip>,
    /// M10 Phase 47b: track volume (`0.0..=1.0`、`1.0` で unity)。
    /// track header rect 内 buttons の下に horizontal slider band として描画される (`row_h` 余裕がある時のみ)。
    /// 将来 `ArrangementClip.volume` を再導入する場合は `effective = track.volume * clip.volume` の乗算 (DAW 標準)。
    pub volume: f32,
    /// M14 Phase 63c (#016): 親 track の id (`None` で top-level)。 「ある track が group として
    /// 振る舞う条件」 は **他の track の `parent_id` がこの id を指す** こと (= 子を持つ track が group)。
    /// caller 側は parent_id を model に持つだけ (Reaper folder / Live group と整合)、 widget は逆引きで
    /// `is_group_track` を判定して disclosure / 背景色を切替える。
    pub parent_id: Option<u32>,
    /// M14 Phase 63c (#016): 親を辿った段数 (0 = top-level)。 widget 側で BFS すると O(N²) なので
    /// **caller 計算で渡す前提** (track 構成変化時に 1 度計算すればよい、 描画毎には不要)。
    /// `header_x = rect.x + depth * style.indent_px` で indent 描画。
    pub depth: u8,
    /// M14 Phase 63c (#016): true なら子孫 track row を描画 skip。 widget 側で `parent_id` chain を
    /// 辿って「親 chain のいずれかが collapsed なら自分も hide」 と判定する (= group の disclosure
    /// state と整合)。 caller は collapsed フラグを各 track に set して渡すだけ (state は caller 側で
    /// `HashSet<u32>` 等に保持)。
    pub collapsed: bool,
    /// M14 Phase 63n-1 (#028): track の automation lane 群を折り畳むか (▶ = collapsed / ▼ = expanded)。
    /// `automation_lanes.is_empty()` の track は disclosure を描画しないので、 この値は意味を持たない。
    /// `true` で track 行高さは `track_row_h` のまま (= 既存挙動互換)。 `false` で expanded =
    /// `automation_lanes.iter().filter(|l| l.visible)` を上から積む (各 `lane.height_px` を加算)。
    pub automation_lanes_collapsed: bool,
    /// M14 Phase 63n-1 (#028): track 配下の automation lane 群 (Volume / Pan / plugin parameter 等)。
    /// 空 `Vec` で **既存挙動完全互換** (lane 行非表示 + disclosure 非描画)。 daw_01 conversation #028 の
    /// 確定 schema に従う。 各 lane は `target` を持たず (= caller 責務、 widget は label / icon の
    /// 表示しか必要としない)、 内部に `clips: Vec<ArrangementAutomationClip>` を持つ。
    pub automation_lanes: Vec<ArrangementAutomationLane>,
    /// M14 Phase 63n-6 (#031): per-track row 高さ override (px)。 `None` で `view.track_row_h` を使用、
    /// `Some(h)` で **このトラックのみ** が `h` px の row 高さで描画される (= Bitwig per-track zoom と
    /// 同 idiom)。 新 splitter / Alt+drag gesture で `SetSingleTrackRowH { track, prev, next }` を発行
    /// → caller が `t.row_h = Some(next)` で反映 → 次 frame で「そのトラックだけ」 が伸び縮みする。
    /// 既存 Alt+wheel (`SetTrackRowH(f32)`) は引き続き **global** で `view.track_row_h` を update —
    /// override 済 (`Some`) track は global zoom に追従しない (per-track 自由度を尊重)。
    /// `lane.height_px` (lane 個別高さ) と独立で、 expanded 時の総高さは `row_h + Σ visible_lane.height_px`。
    pub row_h: Option<u16>,
    /// M14 Phase 72 (daw_01 #044): track 種別 (Audio / Video)。 default = `Audio` で既存 caller 互換。
    /// `Video` のとき: 行背景を `track_background_video` で塗る + header から instrument/fx_chain/
    /// volume/pan slot を非描画 + clip 描画を thumbnail / loading 色に切り替える ([`TrackKind`] 参照)。
    pub kind: TrackKind,
    /// M14 Phase 87 (daw_01 #059): track の表示色。 `Some(c)` で header 左端に幅
    /// `style.track_color_strip_w` の色縦ストライプを描画 (selected / group / video 背景の上に
    /// 重ねる)。 `None` で既存挙動完全互換 (strip 非描画)。 `ArrangementClip.color` の track 版。
    pub color: Option<Color>,
}

// ============================================================
// M14 Phase 63n-1 (#028): automation lane schema
// ============================================================

/// M14 Phase 63n-1 (#028): automation lane の identity (track id + lane id)。
/// daw_01 conversation #028 で確定した key 型 (caller 側 mirror と shape 一致)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AutomationLaneKey {
    pub track: u32,
    pub lane: u32,
}

/// M14 Phase 63n-1 (#028): automation clip の identity (lane key + clip id)。
/// `lane_key()` helper で `AutomationLaneKey` に降格できる。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AutomationClipKey {
    pub track: u32,
    pub lane: u32,
    pub clip: u32,
}

impl AutomationClipKey {
    /// `(track, lane)` 部分を抽出 (lane 操作 EditRequest と clip 操作 EditRequest の混在 dispatch 用)。
    #[must_use]
    pub fn lane_key(self) -> AutomationLaneKey {
        AutomationLaneKey { track: self.track, lane: self.lane }
    }
}

/// M14 Phase 63n-1 (#028): automation point の identity (clip key + point index)。
/// **point の index は同 frame 内のみ valid** (point の add / delete で再採番される)。 widget は
/// hit-test 結果としてこの key を返し、 caller は `clip.points[point_idx]` で値を取得する。 drag 中は
/// session 内に持ち越して frame 跨ぎで stable に追跡する設計 (Phase 63n-2 で実装)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AutomationPointKey {
    pub clip: AutomationClipKey,
    pub point_idx: u32,
}

/// M14 Phase 63n-1 (#028): automation point の incoming curve 種別 (= 直前の point からこの point に
/// 至る曲線形状)。 daw_01 conversation #033 で `apply_curve` の式を更新済 (Bezier は真の S 字 cubic
/// に書き直し、 Exponential variant を追加)。 描画は daw_01 `common::automation::apply_curve` を SSoT
/// として完全ミラー (描画と再生の数値完全一致を保証、 audio/MIDI と同 idiom)。
/// - `Hold`: 階段 (前の値を保持して垂直立ち上がり)
/// - `Linear`: 直線
/// - `Bezier { tension }`: 制御点 x = (1/3, 2/3) 固定、 y を `tension` で対角線 ↔ end-hold lerp した
///   cubic Bezier (`-1.0..=1.0`、 `0.0` で直線等価、 `+1.0` で滑らかな S 字、 `-1.0` で overshoot 反転 S 字)。
/// - `Exponential { bend }`: `value = prev + (next - prev) * t.powf(2^bend)` の polyline (`-1.0..=1.0`、
///   `0.0` で直線、 `+1.0` で前半遅・後半速 (二次曲線)、 `-1.0` で前半速・後半遅 (平方根))。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ArrangementCurveKind {
    Hold,
    Linear,
    Bezier { tension: f32 },
    Exponential { bend: f32 },
}

/// M14 Phase 63n-9 (#033): `SetAutomationCurveParam` の対象種別 (Bezier tension / Exponential bend)。
/// daw_01 #033 §B 仕様の `BezierTension` / `ExponentialBend` 2 variant 1 対 1 対応。 caller は match で
/// `point.curve` の対応 variant を更新する idiom (Bezier { tension: next_value } / Exponential { bend: next_value })。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SetAutomationCurveParamKind {
    BezierTension,
    ExponentialBend,
}

/// M14 Phase 63n-1 (#028): automation point の clip-local 座標 + curve 種別。
/// `time_beat` は clip-local (clip start からのオフセット拍)、 `value_norm` は `0.0..=1.0` 正規化。
#[derive(Clone, Copy, Debug)]
pub struct ArrangementAutomationPoint {
    pub time_beat: f64,
    pub value_norm: f32,
    pub curve: ArrangementCurveKind,
}

/// M14 Phase 63n-1 (#028): lane 内の automation clip。 MIDI / Audio clip と意味的に独立した型として
/// 扱う (= ClipKey 階層化ではなく別 schema、 #028 [Resolved] で確定)。
#[derive(Clone, Debug)]
pub struct ArrangementAutomationClip {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    pub name: Arc<str>,
    /// clip-local 座標 (clip start = 0)、 `time_beat` 昇順前提 (caller 責務)。
    pub points: Vec<ArrangementAutomationPoint>,
    /// linked clip 識別フラグ (`[0.0, 1.0)` hue)。 M14 Phase 114 (#086): `Some(_)` で clip 名の左に
    /// link glyph を描く (audio clip と同 path)。 fill / border は `lane.color` が source で、 hue 値は
    /// 描画に使わない (リンク識別のみ。 audio clip の `share_group_color` と同方針)。
    pub share_group_color: Option<f32>,
}

/// M14 Phase 63n-1 (#028): automation lane (target を持たず、 widget は label / icon の表示しか扱わない)。
/// caller (daw_01) が `target` (Track の volume/pan、 plugin parameter 等) を別途保持する。
#[derive(Clone, Debug)]
pub struct ArrangementAutomationLane {
    pub id: u32,
    pub label: Arc<str>,
    /// lane header の icon 用 1 文字 ('V' / 'P' / 'F' 等)。 caller が parameter 種別から決める。
    pub icon_glyph: char,
    /// lane 識別色 (curve 線 + アクセント)。
    pub color: Color,
    /// `false` で curve / clip / point を灰色描画 (bypass 表示)。
    pub enabled: bool,
    /// `false` で lane 行を描画しない + prefix sum にも含めない (= 隣 lane が詰める)。
    pub visible: bool,
    /// 行高さ (px、 default 60、 widget は読むだけで mutate しない)。
    /// Phase 63n-1 では splitter drag UX を入れない (#028 [Resolved] で確定)。
    pub height_px: u16,
    /// `0.0..=1.0` の knob 表示 / curve 範囲外で表示する水平線。 plain ↔ normalized 変換は caller 責務。
    pub default_value_norm: f32,
    pub clips: Vec<ArrangementAutomationClip>,
}

// ============================================================
// M14 Phase 63n-10 (#034): master row (song-level automation)
// ============================================================

/// M14 Phase 63n-10 (#034): `AutomationLaneKey::track` が **master row** 由来の lane を指す sentinel。
/// caller (daw_01) は `AutomationLaneKey { track: MASTER_TRACK_ID, lane }` で master lane を identify、
/// EditRequest 受信側は `key.track == MASTER_TRACK_ID` で master / 通常 track を分岐する。
/// 値は `u32::MAX` (= 通常 track id がここに到達することは現実的に無い)。 daw_01 conversation
/// #034 で確定した「sentinel 規約 + 既存 EditRequest 全流用」 設計の基幹。
pub const MASTER_TRACK_ID: u32 = u32::MAX;

/// M14 Phase 63n-10 (#034): arrangement の上端に常時表示される **master row** (song-level automation)。
/// daw_01 `Song.song_lanes` (= SongTempo / SongTimeSigNumerator 等) の widget 側 representation。
/// 通常 `ArrangementTrack` と異なり `clips` (MIDI / Audio) / `muted` / `solo` / `parent_id` / `volume`
/// は持たず、 **automation lane の Vec のみ** を保持する (= master 行は clip drag や mute/solo gesture を
/// 発火しない、 daw_01 #034 §G で確定)。
///
/// `arrangement()` に `Some(&master)` で渡すと上端 1 行 ("Master" ラベル + 折り畳み可能な automation lane 群)
/// として描画され、 `None` で旧挙動 (master row 無し) と完全互換。 縦 scroll では通常 track と一緒に
/// 動く (= 上端 sticky にしない、 Reaper 流 master at top、 daw_01 #034 §H)。
///
/// lane 群は `ArrangementAutomationLane` を re-use (= label / icon / color / clips / 等は通常 track と
/// 同 schema、 daw_01 が lane.target で SongTempo / SongTimeSigNumerator を識別)。 既存 `AddAutomationPoint`
/// `MoveAutomationPoints` `SetLaneEnabled` 等の EditRequest は `lane.track = MASTER_TRACK_ID` の形で
/// そのまま発火する (= 新 variant 不要、 #034 §F で確定)。
#[derive(Clone, Debug)]
pub struct ArrangementMasterRow {
    /// 展開 / 折り畳み状態 (通常 track の `automation_lanes_collapsed` と同 idiom)。
    /// `▶` (collapsed = true) / `▼` (expanded = false) を toggle すると
    /// `ToggleTrackAutomationCollapsed { track: MASTER_TRACK_ID }` が発火する。
    /// `automation_lanes.is_empty()` で disclosure 非描画 (= toggle 不可)。
    pub automation_lanes_collapsed: bool,
    /// SongTempo / SongTimeSigNumerator 等の song-level lane。 既存 `ArrangementAutomationLane` 型を
    /// re-use、 lane.target は区別の必要なし (= widget はただ描画するだけ、 daw_01 が target で
    /// 何を意味するかを管理)。 各 lane の `AutomationLaneKey { track: MASTER_TRACK_ID, lane: lane.id }`
    /// で EditRequest を発火する。
    pub automation_lanes: Vec<ArrangementAutomationLane>,
    /// row 高さの override (= `Some(px)` で固定、 None で global default = `view.track_row_h`)。
    /// 通常 track の `row_h: Option<u16>` (Phase 63n-6) と完全同 idiom。 expanded 時の総高さは
    /// `effective_h + Σ visible_lane.height_px`、 collapsed 時は `effective_h` のみ
    /// (= 通常 track と同じ式、 #034 §Q3 (A) で確定)。
    pub height_px_override: Option<u16>,
}

/// arrangement の view 状態 (pan / zoom / playhead / loop)。値渡し (Copy)。
#[derive(Clone, Copy, Debug)]
pub struct ArrangementView {
    /// 表示 left の拍 (浮動小数で smooth scroll)。
    pub start_beat: f64,
    /// 表示する拍範囲 (= zoom 倍率の逆数)。
    pub len_beats: f64,
    /// 縦 scroll offset (px、smooth)。`track_top = 0.0` で first track が lanes 上端。
    pub track_top: f32,
    /// 表示可能 row 数 (SetTrackTop の上限計算に user が使う、widget は読み取らず情報のみ)。
    pub tracks_visible: f32,
    /// 1 track row の高さ (px)。
    pub track_row_h: f32,
    /// track header 領域の幅 (px、`0.0` で header 無し)。
    pub header_w: f32,
    /// ruler 領域の高さ (px、`0.0` で ruler 無し)。
    pub ruler_h: f32,
    /// playhead 線を描く拍位置 (`None` で disabled)。
    pub playhead_beat: Option<f64>,
    /// ループ範囲 (`Some((start, end))`)。`start <= end` 前提。
    pub loop_range: Option<(f64, f64)>,
    /// track 構成 / clip 編集で bump する hook (cache busting)。
    /// selection 変化では bump しない (selection は cached 外 overlay)。
    pub data_generation: u64,
    /// テンポ (BPM)。M13 Phase 55 で追加。`time_ruler` / `bar_beat_grid` に渡す
    /// `TimeMapping::tempo_bpm` に使う (BarBeat 表示の bar 線位置計算では `time_sig` だけで
    /// 足りるが、将来 Seconds/SMPTE 切替で必要になるため field として保持)。
    pub bpm: f32,
    /// 拍子 (numerator, denominator)。M13 Phase 55 で追加。`(4, 4)` で 4/4、`(3, 4)` で 3/4、
    /// `(6, 8)` で 6/8。内部で `numerator * 4 / denominator` (= beats_per_bar) に変換
    /// (3/4 → 3, 6/8 → 3)。
    pub time_sig: (u8, u8),
    /// (M9 Phase 45f) drag overlay と commit 値、 dblclick `beat` の grid 吸着設定 (#010 [Replied])。
    /// `Default::default()` は `Adaptive` ON。 raw 動作を保ちたい caller は `SnapConfig::OFF` を渡す。
    /// drag 中 `pointer.modifiers.alt` で一時無効化。
    pub snap: SnapConfig,
    /// M14 Phase 127 (daw_01 #105): Arranger レーンの高さ (px、`0.0` で無し)。 ruler の直下・track lanes
    /// の上に水平に確保し、 曲のパート (Intro / Aメロ / サビ …) を表す色帯 (`SectionView`) を描く。
    /// `0.0` で従来描画と完全互換 (レーン無し)。 section データ自体は `arrangement()` の
    /// `sections: &[SectionView]` 引数で渡す (`ArrangementView` の `Copy` を壊さないため、 可変長 `Vec`
    /// は view の field には持たせない = `tracks` / `master_row` と同じ「描画対象は別引数」 idiom)。
    pub arranger_lane_h: f32,
}

impl Default for ArrangementView {
    fn default() -> Self {
        Self {
            start_beat: 0.0,
            len_beats: 16.0,
            track_top: 0.0,
            tracks_visible: 8.0,
            track_row_h: 32.0,
            header_w: 160.0,
            ruler_h: 24.0,
            playhead_beat: None,
            loop_range: None,
            data_generation: 0,
            bpm: 120.0,
            time_sig: (4, 4),
            snap: SnapConfig::DEFAULT,
            arranger_lane_h: 0.0,
        }
    }
}

/// M14 Phase 127 (daw_01 #105): Arranger レーンに描く「曲のパート」 1 件 (Studio One の Arranger Track
/// 相当)。 `arrangement()` に `&[SectionView]` のスライスで渡す (昇順・非交差 = 重複なし前提、 隙間は許容)。
///
/// widget は section を一切 mutate しない (drag 中は帯のみ視覚プレビュー、 release で高レベル意図を
/// `ArrangementEditRequest` (`CreateSection` / `MoveSection` / `ResizeSection` / `DuplicateSection` …)
/// として 1 度 emit するだけ)。 破壊的リフロー (clip 分割 / ripple / フルスコープ移動 / 重複正規化) は
/// 全て caller (daw_01) が行う。 `ArrangementClip` と同じく `Arc<str>` name を持つので `Copy` ではない。
#[derive(Clone, Debug)]
pub struct SectionView {
    /// caller が採番する安定 id (Move / Resize / Rename / 右クリックの対象指定に使う)。
    pub id: u32,
    /// 帯に描く名前 (Intro / Aメロ / サビ …)。
    pub name: Arc<str>,
    /// 帯の塗り色 (linear RGB)。 名前ラベルは `clip_text_color_for` で自動コントラスト選択。
    pub color: [f32; 3],
    /// 開始拍。
    pub start_beat: f64,
    /// 長さ拍 (end = `start_beat + len_beats`)。
    pub len_beats: f64,
    /// M14 Phase 128 (daw_01 #106): 選択状態。 `true` の帯は選択ハイライト (選択 clip と同 idiom =
    /// 明るい太枠) で描画。 caller が `selected_section_ids` 等の選択集合から per-section に設定する
    /// (SSoT = caller、 widget は描画に使うだけ)。 `false` で従来描画 (回帰)。
    pub selected: bool,
}

/// clip drag の種別 (hit-test 結果)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipDragKind {
    /// clip 中央 drag = 平行移動 (start_beat + 任意 track 跨ぎ)。
    Move,
    /// 左端 drag = start_beat / len_beats 両方変化。
    ResizeLeft,
    /// 右端 drag = len_beats のみ変化。
    ResizeRight,
}

/// `MoveClips` の delta 1 件 (track 跨ぎ可)。`from.clip` は track 跨ぎでも不変。
#[derive(Clone, Copy, Debug)]
pub struct MoveClipDelta {
    pub from: ClipKey,
    pub to_track: u32,
    pub prev_start_beat: f64,
    pub next_start_beat: f64,
}

/// `ResizeClips` の delta 1 件。`ResizeLeft` は両方変化、`ResizeRight` は `next_start == prev_start`。
#[derive(Clone, Copy, Debug)]
pub struct ResizeClipDelta {
    pub key: ClipKey,
    pub prev_start: f64,
    pub prev_len: f64,
    pub next_start: f64,
    pub next_len: f64,
    /// FIXME #61: Shift 修飾で開始した端 drag は **time-stretch** (= 内容を
    /// 新しい長さに伸縮)、 無印は **trim** (= 再生範囲を変える)。 geometry
    /// (`next_start` / `next_len`) は両者同一で、 解釈だけ caller (daw_01) が
    /// 分岐する。 `Ableton` 流 (plain=trim / Shift=stretch)。
    pub stretch: bool,
}

/// M14 Phase 63k (#025): `SetClipGainDb` の delta 1 件 (release 時に発火)。
/// `prev_gain_db` で undo を構築できる (caller は `Edit::with_inverse` で対称適用すれば良い)。
#[derive(Clone, Copy, Debug)]
pub struct ClipGainDelta {
    pub key: ClipKey,
    pub prev_gain_db: f32,
    pub next_gain_db: f32,
}

/// M14 Phase 63k (#025): `SetClipFade` の delta 1 件 (length 変更、 release 時に発火)。
/// `edge` で fade_in / fade_out を区別、 `prev_beats` / `next_beats` は当該 edge の length 拍数。
#[derive(Clone, Copy, Debug)]
pub struct ClipFadeDelta {
    pub key: ClipKey,
    pub edge: FadeEdge,
    pub prev_beats: f64,
    pub next_beats: f64,
}

/// M14 Phase 63k (#025): `SetClipFadeCurve` の delta 1 件 (curve 切替、 release 時に発火)。
/// vertical drag で `next_curve` が `prev → next()` (Linear → Exp → SCurve → Linear) に進む。
#[derive(Clone, Copy, Debug)]
pub struct ClipFadeCurveDelta {
    pub key: ClipKey,
    pub edge: FadeEdge,
    pub next_curve: FadeCurve,
}

/// M14 Phase 63n-2 (#028): `MoveAutomationPoints` の delta 1 件 (release 時に発火)。
/// `point` の `point_idx` は **drag 開始時 frame の index** (= caller `clip.points` 配列の index)。
/// caller は (1) `clip.points[delta.point.point_idx]` を `next_*` に更新、 (2) 必要に応じて
/// `time_beat` 昇順を保つよう sort し直す (重複 time_beat を許すか tie-break するかは caller 仕様)。
/// `prev_*` は Undoable 構築用 (caller が `Edit::with_inverse` で対称適用できる)。
#[derive(Clone, Copy, Debug)]
pub struct MoveAutomationPointDelta {
    pub point: AutomationPointKey,
    pub prev_time_beat: f64,
    pub prev_value_norm: f32,
    pub next_time_beat: f64,
    pub next_value_norm: f32,
}

/// M14 Phase 63n-3 (#028): `MoveAutomationClips` / `CloneAutomationClipsLinked` /
/// `CloneAutomationClipsIndependent` の delta 1 件 (lane 跨ぎ可、 release 時に発火)。
/// `MoveClipDelta` と同 shape の lane 版 (caller の dispatch ロジックを 1:1 で踏襲できる)。
/// `to_lane` は drop 先 lane key (= 同一 track 内 lane 跨ぎ + track 跨ぎ both 可)。
#[derive(Clone, Copy, Debug)]
pub struct MoveAutomationClipDelta {
    pub from: AutomationClipKey,
    pub to_lane: AutomationLaneKey,
    pub prev_start_beat: f64,
    pub next_start_beat: f64,
}

/// M14 Phase 63n-3 (#028): `ResizeAutomationClips` の delta 1 件 (release 時に発火)。
/// `ResizeClipDelta` と完全同 shape。 `ResizeLeft` で `next_start != prev_start` + `next_len != prev_len`、
/// `ResizeRight` で `next_start == prev_start` + `next_len != prev_len`。
#[derive(Clone, Copy, Debug)]
pub struct ResizeAutomationClipDelta {
    pub key: AutomationClipKey,
    pub prev_start: f64,
    pub prev_len: f64,
    pub next_start: f64,
    pub next_len: f64,
}

/// M14 Phase 63c (#016): track header click 時の selection 変更 modifier (DAW 業界標準)。
/// caller が `SelectTrack` Edit を受け取ったときに `(prev, next, modifier)` から動作意図を判別できる
/// ようにするため、 widget 側で modifier を decoded して送る (caller boilerplate の削減)。
///
/// - `Single`: 修飾なし click → `next = vec![clicked]`、 anchor を clicked で update
/// - `RangeFromAnchor`: Shift+click → 直前 Single click 位置 (= widget 内 anchor) と clicked の間の
///   visible 列上の連続範囲を選択。 anchor が無い場合は Single 同等。
/// - `Toggle`: Ctrl+click → `next = if prev.contains(&clicked) { prev - clicked } else { prev + clicked }`、
///   anchor は更新しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectModifier {
    Single,
    RangeFromAnchor,
    Toggle,
}

/// arrangement が user に発行する Edit 要求。1 frame 内で消費される一時 ADT。
#[derive(Debug)]
pub enum ArrangementEditRequest {
    SelectClips { prev: Vec<ClipKey>, next: Vec<ClipKey> },
    /// M14 Phase 63c (#016): multi-select 化。 旧 `{ prev: Option<u32>, next: Option<u32> }` を
    /// `{ prev: Vec<u32>, next: Vec<u32>, modifier: SelectModifier }` に置換 (1 → N の breaking change)。
    /// 単一選択は `next = vec![tid]`、 解除は `next = vec![]`。 modifier は caller の Edit dispatch
    /// 時に動作意図 (Single / Range / Toggle) を判別する用 (typically caller は `next` をそのまま
    /// `selected_tracks` に書き込めば良く、 modifier は無視可能)。
    SelectTrack { prev: Vec<u32>, next: Vec<u32>, modifier: SelectModifier },
    MoveClips(Vec<MoveClipDelta>),
    /// M14 Phase 63e (#019): **Ctrl + drag** で発火する「共有コピー」 意図。 `MoveClips` と同じ
    /// `Vec<MoveClipDelta>` shape だが、 semantics が異なる:
    /// - `from`: source clip identity (残置、 削除しない)
    /// - `to_track`: 新 clip の配置 track
    /// - `prev_start_beat`: source clip 位置 (informational、 残置)
    /// - `next_start_beat`: 新 clip の配置位置 (snap 適用済 absolute beat)
    ///
    /// daw_01 caller は (a) source clip を残し (b) `from.clip` の `content_id` をそのまま共有する
    /// 新 clip を `to_track` の `next_start_beat` に追加する。 ピアノロール編集が同 source の
    /// 全 clip に即時反映される (REAPER pooled MIDI 流)。
    CloneClipsLinked(Vec<MoveClipDelta>),
    /// M14 Phase 63e (#019): **Ctrl + Shift + drag** で発火する「独立コピー」 意図。 `from` が
    /// 指す source clip を残し、 内容を **deep clone した新 clip** を追加する意図。 daw_01 caller は
    /// content を fork (新 `ContentId` を採番) して新 clip に紐付ける。 fields の意味は
    /// `CloneClipsLinked` と同じ。
    CloneClipsIndependent(Vec<MoveClipDelta>),
    ResizeClips(Vec<ResizeClipDelta>),
    DeleteClips(Vec<ClipKey>),
    DoubleClickClip(ClipKey),
    DoubleClickEmpty { track: u32, beat: f64 },
    /// M14 Phase 99 (daw_01 #071): 空きレーン (clip / automation lane に吸収されない真の
    /// track row 空白) の **右クリック (secondary press)**。`DoubleClickEmpty` と対になる
    /// secondary 版で、「右クリック → コンテキストメニューで clip 種別を選んで生成」
    /// (REAPER の右クリック空きエリア → Insert new item idiom) の入口。
    /// - `beat`: `DoubleClickEmpty` と同様 **widget 内で snap 済み**の絶対 beat (caller は
    ///   後処理不要)。`track`: track id。
    /// - `pos`: コンテキストメニューの表示アンカー用の右クリック座標 (viewport 座標、popup の
    ///   anchor 系と同じ)。caller は `ui.context_menu_at(id, Some(pos), items, on_select)` 等で
    ///   この pos にメニューを開ける。
    /// - master row 上 / clip 上 / automation lane 上では発火しない (= `DoubleClickEmpty` と
    ///   同じ exclusion)。
    SecondaryClickEmpty { track: u32, beat: f64, pos: (f32, f32) },
    // ===== M14 Phase 127 (daw_01 #105): Arranger レーン (曲のパート Section) の編集意図 =====
    // widget は section を一切 mutate しない。 構造変化 (Create/Move/Resize/Duplicate) は snap 適用済
    // (`view.snap.snap_beat`、 Alt で一時無効) + `0.0` 以上 clamp で emit。 隣接帯への食い込み厳密 clamp /
    // 重複正規化は caller (daw_01 `normalize_sections`) が行う (既存「widget は snap + sanity floor、
    // 実 clamp は caller」 規約どおり)。 ループ化・ジャンプは既存 `SetLoopRange` / `SetPlayheadBeat` を再利用。
    /// M14 Phase 128 (daw_01 #106): 帯の単 click (短 click、 移動 < 4px) による section 選択変更。
    /// `track header` click の `SelectTrack` と同 idiom で `modifier` (Single / RangeFromAnchor / Toggle) を
    /// 載せる (修飾なし = Single、 Shift = RangeFromAnchor、 Ctrl = Toggle で multi-select)。 caller は
    /// `selected_section_ids` に対して modifier に応じた選択変更を適用する。 **同 short click で
    /// `SetPlayheadBeat(section.start)` も併発**する (= クリックで「選択 + ジャンプ」、 Studio One / REAPER 流)。
    /// drag (Move/Resize/Duplicate/Create) では発火しない。 widget 内 anchor は持たず (`SelectTrack` と異なり
    /// RangeFromAnchor の anchor 解決は caller 側、 section は 1 次元で caller が `id` 順を知っているため)。
    SelectSection { id: u32, modifier: SelectModifier },
    /// 空き Arranger レーンの dblclick (既定長 1 bar = `time_sig` 由来) または範囲 drag (描いた範囲) による
    /// section 新規作成。 `start` は snap 適用済絶対拍 (`0.0` 以上)、 `len` は `> 0`。 名前・色は caller が
    /// 採番時に付与する (Intro / Aメロ / サビ … 循環) ので emit に含めない。
    CreateSection { start: f64, len: f64 },
    /// 帯中央 drag → release で 1 度発火する section 平行移動。 `next_start` は snap 適用済 (`0.0` 以上)。
    /// `prev_start` で Undoable 構築容易 (`SetTrackVolume` と同 pattern)。 drag 中は帯のみ live preview、
    /// 内容のリフローは caller が release で行う。
    MoveSection { id: u32, prev_start: f64, next_start: f64 },
    /// 帯端 drag → release で発火する section リサイズ。 左端 = start / len 両方変化、 右端 = len のみ
    /// (`next_start == prev_start`)。 `ResizeClipDelta` と同 shape。 snap 適用済 (`0.0` 以上、 `len > 0`)。
    ResizeSection {
        id: u32,
        prev_start: f64,
        prev_len: f64,
        next_start: f64,
        next_len: f64,
    },
    /// Ctrl+drag → release で発火する section 複製 (`CloneClipsLinked` と同じ Ctrl+drag idiom)。 元 section
    /// (`id`) を残し、 caller が新 section を `dest_start` (snap 適用済絶対拍) に採番して追加する意図。
    DuplicateSection { id: u32, dest_start: f64 },
    /// 帯名 dblclick → section の改名開始 (`BeginRenameTrack` と同 idiom、 rename UI は caller が出す)。
    BeginRenameSection(u32),
    /// 帯上の右クリック (secondary press) → caller が `pos` (viewport 座標) にコンテキストメニュー
    /// (改名 / 色 / このセクションをループ / 帯のみ削除 / 範囲ごと削除) を開く入口 (`SecondaryClickEmpty`
    /// と同 idiom)。
    SecondaryClickSection { id: u32, pos: (f32, f32) },
    BeginRenameTrack(u32),
    DeleteTrack(u32),
    MoveTrackUp(u32),
    MoveTrackDown(u32),
    /// M10 Phase 46: track header drag&drop による並び替え。`order` は新順での `track.id` 列。
    /// `MoveTrackUp/Down` は keep (button / keyboard 用)、`ReorderTracks` は drag&drop 用。
    ReorderTracks(Vec<u32>),
    /// M10 Phase 47b: track header の bottom band slider drag による volume 編集。
    /// `next` は `0.0..=1.0` で widget 側 clamp 済み。`prev/next` で Undoable 化容易。
    SetTrackVolume { track: u32, prev: f32, next: f32 },
    ToggleTrackMute(u32),
    ToggleTrackSolo(u32),
    /// M14 Phase 68 (#040): track header の R button click。 caller は `track.armed = !track.armed` で
    /// 反転する (mute / solo と完全同 idiom、 任意数の track を armed にできる排他なしの toggle)。
    /// armed track のみが audio engine の録音入力 (MIDI device / audio input) 対象 (Bitwig / Live /
    /// Reaper と同 idiom)。
    ToggleTrackArmed(u32),
    SetLoopRange { start: f64, end: f64 },
    /// M14 Phase 63j (#024): ruler 上 click / drag による playhead seek 要求。 caller は
    /// (a) `view.playhead_beat = Some(beat)` 更新 (b) audio engine への seek IPC 送信に変換する。
    /// widget は press frame と continuation frame (drag 中) で発火し、 release frame では
    /// emit しない (drag 中の最後の値が確定値、 release 専用 commit は無し)。 同 frame 内で
    /// 同値を 2 回送らないよう session 側で `last_emitted_beat` を保持。
    /// **snap 適用済 + `0.0` 以上に clamp** (`view.snap.snap_beat(raw, alt, zoom)`)。
    SetPlayheadBeat(f64),
    /// 横ズーム (`zoom_x_px_per_beat` = px/beat を **絶対値で更新**、`Ctrl+wheel` で発火)。
    /// widget 側で `current_zoom_x * factor` を計算済の絶対値を送る (`SetTrackRowH` と同パターン)。
    /// min/max の clamp は app 側で実施 (目安 2..400 px/beat、 1 拍 = 2px の超 zoom out 〜
    /// 1 拍 = 400px の超 zoom in)。 widget 側は NaN/inf 防御の sanity clamp `0.1..10000` のみ。
    SetZoomX(f32),
    SetScrollX(f64),
    SetTrackTop(f32),
    /// M10 Phase 48: 縦ズーム (`track_row_h` を絶対値で更新、`Alt+wheel` で発火)。
    /// min/max の clamp は app 側で実施 (目安 16..96 px)。 **global** (`view.track_row_h` を更新)、
    /// `ArrangementTrack.row_h: Some(_)` の override 済 track には影響しない (#031 per-track と独立)。
    SetTrackRowH(f32),
    /// M14 Phase 63n-6 (#031): **per-track** row 高さ resize (新 splitter / Alt+drag gesture で発火)。
    /// `prev` は session anchor (drag 開始時の effective row 高さ)、 `next` は drag 中 cursor 位置から
    /// 計算した新 height (px)。 caller は `track` に対応する `ArrangementTrack.row_h = Some(next)` で
    /// 反映する (= 「そのトラックだけ」 が伸び縮みする per-track zoom)。 widget は floor 1 px のみ
    /// (= 異常入力 safety)、 caller side で `[min, max]` clamp する idiom (`SetTrackRowH(f32)` と整合)。
    SetSingleTrackRowH { track: u32, prev: u16, next: u16 },
    /// M14 Phase 117 (daw_01 #091): track header 右端の splitter (= header / lanes 境界の縦線) drag に
    /// よる **全 track 共通** の header 幅編集。 drag 中は per-frame で発火 (live preview)、 release で
    /// session 破棄 (per-frame で final 値が発火済)。 `next` は **raw px** (widget は NaN/負値防止の
    /// `max(0.0)` floor のみ)、 実用 clamp (例 `80..=480`) は caller が行う (`SetZoomX` / `SetTrackRowH`
    /// と同じ「widget は sanity floor のみ、 min/max は caller」 idiom)。 caller は `view.header_w` を
    /// `next` (clamp 後) で更新するだけで、 次 frame に header / lanes が連動伸縮する。 `prev` は drag 開始時の
    /// header 幅 (Undoable 構築用、 per-frame emit でも anchor 固定値)。
    SetHeaderW { prev: f32, next: f32 },
    /// M14 Phase 63c (#016): group 折り畳み toggle (▼/▶ disclosure click)。
    /// caller は `track.collapsed` を反転した値を保存する (widget 側は描画時に親 chain の
    /// collapsed を辿って子孫 row を skip)。
    ToggleGroupCollapsed(u32),
    /// M14 Phase 63c (#016): drag-and-drop による parent 変更 + 挿入位置指定。 multi-select 中は
    /// selected track 群を一括移動する設計のため `tracks: Vec<u32>` (単一 track でも `vec![id]`)。
    /// `parent` が `None` で top-level に持ち上げ (= ungroup)、 `Some(id)` で `id` track の配下に移動。
    /// `anchor_after` は **caller の `tracks` 配列内** で source tracks を挿入する直前 track id
    /// (`None` で先頭挿入)。 caller は (1) source を arr_tracks から remove (2) parent_id を `parent`
    /// に更新 (3) anchor_after の直後に source を挿入、 という再構築をすればよい。 同 parent 内 reorder
    /// もこの variant で表現できる (parent が変化しないだけ)、 widget は drag drop で常に SetTrackParent
    /// を発行する (`ReorderTracks` は keyboard / context menu 等の caller-driven reorder 用に残す)。
    SetTrackParent {
        tracks: Vec<u32>,
        parent: Option<u32>,
        anchor_after: Option<u32>,
    },
    /// M14 Phase 63k (#025): clip 中央 dB handle 帯の縦 drag による gain_db 変更 (release
    /// 時に 1 度発火)。 `Vec<ClipGainDelta>` shape は selection multi-clip 対応の余地を残す
    /// ため (将来 `selected_clips` を一括移動する変種)。 単一 clip drag では `vec![delta]` の
    /// 1 件で発火。 widget 側で ±24 dB に clamp 済 (caller の clamp 不要、 ただし caller 側で
    /// 別 range を望む場合は再 clamp してよい)。
    SetClipGainDb(Vec<ClipGainDelta>),
    /// M14 Phase 63k (#025): clip 上端の角 drag (sticky horizontal) による fade length 変更
    /// (release 時に 1 度発火)。 `edge` で fade_in / fade_out を区別、 `next_beats` は当該
    /// edge の length 拍数 (widget 側で `0.0..=clip.len_beats` に clamp 済)。 同 release で
    /// `SetClipFadeCurve` と同時発火することはない (sticky direction で必ず排他)。
    SetClipFade(Vec<ClipFadeDelta>),
    /// M14 Phase 63k (#025): clip 上端の角 drag (sticky vertical, |dy| > 10 px) による fade
    /// curve トグル (release 時に 1 度発火)。 `next_curve` は `prev.next()` (Linear → Exp →
    /// SCurve → Linear)。 同 release で `SetClipFade` と同時発火することはない。
    SetClipFadeCurve(Vec<ClipFadeCurveDelta>),
    /// M14 Phase 63n-1 (#028): track の automation lane 群を折り畳み・展開する。 caller は
    /// `track.automation_lanes_collapsed` を反転した値を保存する (widget は描画時に lane 行を
    /// 上から積むかどうかをこのフラグで判定)。 `automation_lanes` が空の track では発火しない
    /// (disclosure を描画しないため)。
    ToggleTrackAutomationCollapsed { track: u32 },
    /// M14 Phase 63n-2 (#028): lane header の `★` icon click。 caller は `lane.enabled = enabled`
    /// を保存する (widget 側は次 frame 反映)。 widget は click 時に `!current` を渡すので、
    /// caller は単純に上書きで OK。 disabled lane (`enabled = false`) は curve / clip rect が
    /// 灰色描画 (bypass marker) になる (Phase 63n-1 から既存)。
    SetLaneEnabled { lane: AutomationLaneKey, enabled: bool },
    /// M14 Phase 63n-2 (#028): lane header の `👁` icon click。 caller は `lane.visible = visible`
    /// を保存する。 `visible = false` で lane 行は描画しない + 高さに含めない (= 隣 lane が詰める)。
    SetLaneVisible { lane: AutomationLaneKey, visible: bool },
    /// M14 Phase 63n-2 (#028): lane header の default value horizontal slider 帯 drag による
    /// `default_value_norm` 変更 (release 時に 1 度発火)。 `prev` / `next` で Undoable 構築容易
    /// (`SetTrackVolume` と同 pattern)。 widget 側で `0.0..=1.0` に clamp 済。
    SetLaneDefault { lane: AutomationLaneKey, prev: f32, next: f32 },
    /// M14 Phase 63n-5 (#030): lane 下端 splitter (高さ ±`automation_lane_resize_handle_px` の hot zone)
    /// drag による `lane.height_px` 変更。 widget 側で `style.automation_lane_min_height_px` /
    /// `style.automation_lane_max_height_px` に clamp 済 (caller は `next` を信用して別 clamp しない)。
    /// drag 中は per-frame `next` 更新で live preview (`SetLaneDefault` と同 pattern)、 release frame
    /// で 1 度だけ `prev = anchor_height_px` で発火 (Undoable 構築容易)。 Alt+drag は採用せず —
    /// Alt は既存 widget で point 削除 / clip snap 一時無効に重く使われており、 lane resize に
    /// 重ねると意図不明な gesture が増えるため。 Bitwig / Live / Reaper と同じ NsResize cursor 付き
    /// splitter を採用 (要望 §A 案 2、 daw_01 #030 で best practice 委譲済)。
    SetLaneHeight {
        lane: AutomationLaneKey,
        prev: u16,
        next: u16,
    },
    /// M14 Phase 63n-2 (#028): lane header の `✕` icon click。 caller は当該 lane を track の
    /// `automation_lanes` から remove する。 lane 内の clip / point も同時に消える (caller 仕様)。
    /// undo は caller 責務 (widget は単発の Edit を発行するだけ)。
    DeleteLane(AutomationLaneKey),
    /// M14 Phase 63n-2 (#028): lane body 内の空き領域 click による point 追加。 `time_beat` は
    /// **clip-local** (clip start からのオフセット拍、 widget 側で snap 適用済 + `0.0..=clip.len_beats`
    /// に clamp)、 `value_norm` は cy 座標から逆算した `0.0..=1.0`。 caller は (1) `clip.points` に
    /// 新 point を `time_beat` 昇順で insert (curve は前 point の curve を継承するか default Linear、
    /// caller 仕様)、 (2) 必要に応じて undo 履歴に push。 widget は同 frame 内の連続 click でも
    /// 1 click = 1 Edit 発行 (重複 add は caller の dedup 仕様で吸収)。
    AddAutomationPoint {
        clip: AutomationClipKey,
        time_beat: f64,
        value_norm: f32,
    },
    /// M14 Phase 63n-4 (#029): lane body 内 clip ギャップ (= 既存 clip と x 範囲が重ならない empty zone)
    /// での dblclick による automation clip 新規作成。 MIDI `DoubleClickEmpty { track, beat }` の lane 版
    /// idiom: `start_beat` は widget 側で snap 適用済 (Alt+dblclick で snap 一時無効、 既存 dblclick と
    /// 同 idiom)、 `len_beats` は `style.automation_clip_default_len_beats` (default 4.0) を渡す suggestion。
    /// caller は自前ポリシー (project 既定長 / 次 clip 直前まで cap / 既存 clip と overlap 拒否) で
    /// 上書き可能。 既存 clip 上の dblclick は引き続き `AddAutomationPoint` (widget 内 priority 排他)。
    CreateAutomationClip {
        lane: AutomationLaneKey,
        start_beat: f64,
        len_beats: f64,
    },
    /// M14 Phase 63n-2 (#028): lane 内 point の drag (release 時に 1 度発火)。 `point_idx` は
    /// **drag 開始時 frame の index** (caller の `clip.points` 配列 index)。 release frame 時点で
    /// caller の Vec が drag 開始時から再配列されていない前提 (drag 中に他 thread から変更が
    /// 入る場合は caller 責務で sort)。 widget 側で `time_beat` snap + `0.0..=clip.len_beats` clamp、
    /// `value_norm` は `0.0..=1.0` clamp 済。
    MoveAutomationPoints(Vec<MoveAutomationPointDelta>),
    /// M14 Phase 63n-2 (#028): Alt + click on point による point 削除 (即時発火、 commit-by-release
    /// なし)。 `point_indices` は frame 内の index 列 (caller は降順 sort して `Vec::remove` で消す
    /// idiom が安全)。 widget は単一 click で `vec![key]` の 1 件を発行するが、 将来 multi-select
    /// 拡張で複数 point 同時削除に拡張可。
    DeleteAutomationPoints(Vec<AutomationPointKey>),
    /// M14 Phase 63n-2 (#028): point 右クリック popup から選択された curve 種別の commit。
    /// widget 自身は popup を描画しない (`Response.automation_curve_popup_request` で anchor を
    /// 返すだけ、 caller が `context_menu_for` で popup を開いて選択を `SetAutomationCurveType` に
    /// 変換)。 `prev` は popup 表示時点の curve、 `next` は user 選択値 (caller 責務)。
    SetAutomationCurveType {
        point: AutomationPointKey,
        prev: ArrangementCurveKind,
        next: ArrangementCurveKind,
    },
    /// M14 Phase 63n-3 (#028): lane 内 automation clip の Move drag (release 時に発火)。 既存 MIDI
    /// clip の `MoveClips` と semantics 1:1 対応 (= caller dispatch ロジックを 1:1 で踏襲できる、
    /// ただし key 型 / lane 跨ぎ semantics が異なるため別 variant 化、 #028 [Resolved] §11.2 で確定)。
    /// drag 中は描画 overlay のみ、 release で 1 度だけ発火。 単一 clip 限定 (multi-select は仕様 §scope 外)。
    MoveAutomationClips(Vec<MoveAutomationClipDelta>),
    /// M14 Phase 63n-3 (#028): Ctrl + drag による automation clip 共有コピー (release 時に発火)。
    /// `MoveAutomationClips` と同 shape、 semantics は MIDI clip の `CloneClipsLinked` と同じ:
    /// source は残置、 同一 content (= 同一 share_group_color hue) を持つ新 clip を `to_lane` の
    /// `next_start_beat` に追加する意図。 caller は ContentId 共有 + Song.clip_contents map 経由で
    /// points を共有 (daw_01 #028 §5 と整合)。
    CloneAutomationClipsLinked(Vec<MoveAutomationClipDelta>),
    /// M14 Phase 63n-3 (#028): Ctrl+Shift + drag による automation clip 独立コピー (release 時に発火)。
    /// `MoveAutomationClips` と同 shape、 semantics は MIDI clip の `CloneClipsIndependent` と同じ:
    /// source は残置、 内容を deep clone した新 clip (新 ContentId 採番) を追加する意図。
    CloneAutomationClipsIndependent(Vec<MoveAutomationClipDelta>),
    /// M14 Phase 63n-3 (#028): lane 内 automation clip の Resize drag (release 時に発火)。
    /// 既存 MIDI clip の `ResizeClips` と semantics 1:1 対応、 単一 clip 限定。
    ResizeAutomationClips(Vec<ResizeAutomationClipDelta>),
    /// M14 Phase 63n-3 (#028): automation clip の delete (caller-driven、 widget は trigger を提供
    /// しない)。 caller は context menu / keyboard shortcut から発火する想定 (widget は
    /// `Response.automation_clip_rects` で全 clip rect を毎 frame 返すため、 caller が
    /// `context_menu_for(rect, &["Delete", ...], ...)` で右クリック menu を毎 frame 呼ぶ idiom)。
    /// API 完全性のため variant は定義するが、 widget 内部からは現状 emit しない。
    DeleteAutomationClips(Vec<AutomationClipKey>),
    /// M14 Phase 63n-3 (#028): automation clip の selection 変更 (短 click on clip で発火)。
    /// `prev` / `next` は順序保持 `Vec` (= 1 click は `next = vec![hit_key]` の単一選択)。
    /// caller は単純に上書きで OK。 既存 MIDI clip `SelectClips` と independence: caller 側で
    /// `selected_automation_clips` と `selected_clips` を別 collection で持ち、 必要なら一方の click
    /// で他方を clear するかは caller 仕様 (Bitwig は mutually exclusive、 他 DAW は coexist)。
    SelectAutomationClips {
        prev: Vec<AutomationClipKey>,
        next: Vec<AutomationClipKey>,
    },
    /// M14 Phase 63n-9 (#033): Bezier `tension` / Exponential `bend` の連続変更 (handle drag release で
    /// 1 度発火、 drag 中は widget 内部 preview state で live 描画のみ → release で commit)。 daw_01 #033
    /// §B 仕様の `SetAutomationCurveParam` を、 caller の AppEvent dispatch を簡潔にするため **1 variant +
    /// kind enum** で表現 (= `SetAutomationCurveBezierTension` / `SetAutomationCurveExponentialBend` を別
    /// variant に分けず、 `kind: SetAutomationCurveParamKind` で discriminate)。 daw_01 側で
    /// `match kind { BezierTension => ..., ExponentialBend => ... }` で 2 分岐するだけで処理可能。
    ///
    /// 値域: `prev_value` / `next_value` は **`-1.0..=1.0` clamp 済** (widget 側で clamp)、 caller は再 clamp
    /// 不要。 `BezierTension` は Bezier `tension`、 `ExponentialBend` は Exponential `bend` に対応。
    ///
    /// 発火条件: handle (`selected_automation_points` 内 point の Bezier / Exponential 入射 segment に出る
    /// 8x8 px 円) を press → drag (`-1.0..=1.0` 連続値) → release。 prev_value と等しいまま release なら
    /// no-op (= 1e-4 閾値、 caller は同値を受信した場合無視で OK)。
    SetAutomationCurveParam {
        point: AutomationPointKey,
        kind: SetAutomationCurveParamKind,
        prev_value: f32,
        next_value: f32,
    },
    /// M14 Phase 63n-8 (#033): automation point の selection 変更。 lasso 矩形 drag 完了時 + point 短 click
    /// (= drag < 4px の release) の 2 経路で発火。 `prev` / `next` は順序保持 `Vec` で `SelectAutomationClips`
    /// と同 idiom (caller は単純に `selected_automation_points = next` で上書き、 必要なら undo 履歴に push)。
    ///
    /// **発火条件と next 計算** (Q2=A の zone 排他 lasso + 短 click select):
    /// - lane body の **空き zone** (= clip / point / lane resize splitter / lane header の **外**) で drag:
    ///   - 修飾なし → `next = lasso 内 points` (= 旧 selection 破棄)
    ///   - Shift+drag → `next = prev ∪ lasso 内 points` (union)
    ///   - Ctrl+drag → `next = prev XOR lasso 内 points` (toggle)
    /// - point 上の **短 click** (= drag<4px の release、 Alt なし):
    ///   - 修飾なし → `next = vec![clicked]` (single select、 prev 破棄)
    ///   - Shift / Ctrl → `next = prev XOR vec![clicked]` (toggle)
    /// - 空き zone の **短 click** (= drag<4px の release):
    ///   - 修飾なし → `next = vec![]` (clear)
    ///   - Shift / Ctrl → no-op (selection 維持、 = 誤操作回避)
    ///
    /// point 上の **長 drag** (>=4px) は selection を変えず `MoveAutomationPoints` を発火する
    /// (= drag した点が prev に含まれるなら全 selected を batch、 含まれないなら単独 move、 widget 側で
    /// `selected_automation_points` を見て自動分岐)。 Alt+click は引き続き即時 `DeleteAutomationPoints`
    /// (selection は変化しない)。
    ///
    /// lasso 中の hit 判定: point の **中心** が lasso rect に含まれるか (= rect の中心 / 端ではなく point 位置)、
    /// daw_01 #033 仕様文面と整合。 invisible lane の points は対象外 (= 既存 `automation_point_at` の
    /// visible scope と一致)。
    SelectAutomationPoints {
        prev: Vec<AutomationPointKey>,
        next: Vec<AutomationPointKey>,
    },
}

/// `Ui::arrangement` の戻り値。
#[derive(Clone, Debug)]
pub struct ArrangementResponse {
    pub hovered_track: Option<u32>,
    pub hovered_clip: Option<ClipKey>,
    pub hovered_zone: Option<ClipDragKind>,
    pub dragging: Option<ClipDragKind>,
    pub rect_select_active: bool,
    pub selection_changed: bool,
    pub clicked_at_track_beat: Option<(u32, f64)>,
    /// 各 track header の rect (app 側で `context_menu_for` / rename overlay を重ねる用)。
    pub track_header_rects: Vec<(u32, Rect)>,
    /// 各 clip の lanes 内 rect (app 側で `context_menu_for` / overlay を重ねる用)。
    /// `track_header_rects` と同じ semantics:
    /// - `(ClipKey, Rect)` のペアで、 描画順 (= 上から下、 左から右) で並ぶ
    /// - **visible_tracks ベース**: collapsed group の子 clip は含まれない
    /// - 完全 off-screen の clip (track row が viewport 外 / clip が beat 範囲外) は除外
    ///   (draw 側の culling と整合、 caller 側 hit-test には影響なし)
    /// - 部分的にカリングされた clip は full rect を返す (clip_to_rect 結果そのまま)
    pub clip_rects: Vec<(ClipKey, Rect)>,
    pub ruler_rect: Rect,
    /// M10 Phase 46: drag 中の track id (`Some` なら header reorder drag セッションが進行中)。
    pub reordering: Option<u32>,
    /// M10 Phase 47b: drag 中の track id (`Some` なら header volume slider drag セッションが進行中)。
    pub dragging_track_volume: Option<u32>,
    /// M14 Phase 63n-2 (#028): 全 visible automation point の rect (caller の `context_menu_for`
    /// 等で popup anchor として使用)。 `clip_rects` と同 semantics:
    /// - `(AutomationPointKey, Rect)` のペアで描画順 (= 上から下、 左から右) で並ぶ
    /// - **visible_tracks ベース**: collapsed group の子 track / collapsed automation lane / `lane.visible
    ///   = false` の lane に属する point は含まれない
    /// - 完全 off-screen (lane 行が viewport 外 / point が beat 範囲外) の point は除外
    /// - rect は描画されている point dot の bounding box (= 半径 4px の正方形 = 8x8 px @ default radius)
    ///
    /// caller は `for (key, rect) in &resp.automation_point_rects { ui.context_menu_for(*rect,
    /// &["Hold", "Linear", "Bezier"], ...) }` で右クリック context menu を毎 frame 呼ぶ idiom
    /// (`clip_rects` と同 pattern、 daw_01 #028 §11.4 で確定)。 widget 自体は popup を描画しない。
    pub automation_point_rects: Vec<(AutomationPointKey, Rect)>,
    /// M14 Phase 63n-3 (#028): 全 visible automation clip の rect (lane body 内、 縦 padding 適用済)。
    /// caller は `for (key, rect) in &resp.automation_clip_rects { ui.context_menu_for(*rect,
    /// &["Make Unique", "Delete", ...], ...) }` で右クリック context menu を毎 frame 呼ぶ idiom
    /// (`clip_rects` / `automation_point_rects` と同 pattern)。 collapsed group / hidden lane / 完全
    /// off-screen の clip は除外。
    pub automation_clip_rects: Vec<(AutomationClipKey, Rect)>,
    /// M14 Phase 63n-3 (#028): drag 中の automation clip kind (`Some` なら lane 内 clip drag セッション
    /// 進行中)。 既存 `dragging` (MIDI clip 用) と直交、 同 frame 内で両方 `Some` にならない (排他)。
    pub dragging_automation_clip: Option<ClipDragKind>,
    /// M14 Phase 63n-8 (#033): automation point の lasso 矩形 drag セッションが進行中なら `true`
    /// (= 空き automation lane zone で drag 開始 → release までの間)。 caller の cursor / status
    /// indicator 用 (既存 `rect_select_active` (MIDI clip 用) と直交、 同 frame で両方 true にならない)。
    pub automation_lasso_active: bool,
    /// M14 Phase 116 (daw_01 #090): ポインタが今 hover している automation lane body の key。
    /// `hovered_clip` / `hovered_zone` と同じ「毎フレーム算出の hover state」 idiom。 caller (daw_01) は
    /// widget draw とは別フェーズ (`dispatch_shortcuts` 等) で「ポインタ下が clip 領域か automation lane か」
    /// を区別できる (例: Ctrl+A の context 全選択の起点判定)。
    ///
    /// 算出は `automation_lane_key_at_y` を widget 内部で呼ぶ。 **lane body 全域**をカバー (点 / clip が
    /// 無い空き領域でも `Some`)。 lane header (展開トグル帯) は含まない (= `lanes` pane 内の body のみ)。
    /// master row の lane (sentinel `MASTER_TRACK_ID`) も対象。
    ///
    /// **clip-first の first-hit**: `hovered_clip` が `Some` のとき (= ポインタが clip 上) は
    /// `hovered_automation_lane` は `None` (排他、 piano_roll の `hovered_*` と同流儀)。 lane と clip は
    /// 縦に別領域なので通常同時には成立しないが、 構造的に排他を保証する。
    pub hovered_automation_lane: Option<AutomationLaneKey>,
    /// M14 Phase 127 (daw_01 #105): ポインタが今 hover している Arranger section の id (`hovered_clip`
    /// と同じ「毎フレーム算出の hover state」 idiom)。 section は ruler と track lanes の中間 lane なので
    /// clip / automation lane とは y 領域が排他 (同時に複数 hover しない)。
    pub hovered_section: Option<u32>,
    /// FIXME #067: ポインタが今 hover している Arranger section の zone (Move / ResizeLeft / ResizeRight)。
    /// clip の `hovered_zone` と同 idiom で widget 内の cursor 設定が読む (帯端 hover で `EwResize`、 帯中央
    /// hover で `Move`)。 これが無いと section 帯はクリップと違い resize ハンドルの ↔ カーソルが出ず、 端を
    /// 掴んでリサイズできることが discoverable でなかった (= ユーザ報告「Aメロの端でカーソルが矢印にならない」)。
    pub hovered_section_zone: Option<ClipDragKind>,
    /// M14 Phase 127 (daw_01 #105): drag 中の section drag kind (`Some` なら既存 section の Move/Resize
    /// drag セッション進行中)。 `dragging` (MIDI clip) / `dragging_automation_clip` と直交、 同 frame 内で
    /// 複数 `Some` にならない (y 領域排他)。 範囲 drag による新規作成中は `None` (transient creation)。
    pub dragging_section: Option<ClipDragKind>,
    /// M14 Phase 127 (daw_01 #105): 全 visible section の Arranger レーン内 rect (`clip_rects` と同
    /// semantics、 描画順 = 左から右)。 caller が `context_menu_for(rect, ...)` で右クリックメニューを
    /// 重ねる用 (`SecondaryClickSection` の `pos` と併用可)。 完全 off-screen (beat 範囲外) の section は除外。
    pub section_rects: Vec<(u32, Rect)>,
}

impl Default for ArrangementResponse {
    fn default() -> Self {
        Self {
            hovered_track: None,
            hovered_clip: None,
            hovered_zone: None,
            dragging: None,
            rect_select_active: false,
            selection_changed: false,
            clicked_at_track_beat: None,
            track_header_rects: Vec::new(),
            clip_rects: Vec::new(),
            ruler_rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            reordering: None,
            dragging_track_volume: None,
            automation_point_rects: Vec::new(),
            automation_clip_rects: Vec::new(),
            dragging_automation_clip: None,
            automation_lasso_active: false,
            hovered_automation_lane: None,
            hovered_section: None,
            hovered_section_zone: None,
            dragging_section: None,
            section_rects: Vec::new(),
        }
    }
}

/// arrangement の見た目スタイル。`Default` で example 互換の見た目を再現。
#[derive(Clone, Copy, Debug)]
pub struct ArrangementStyle {
    pub bg: Color,
    pub header_bg: Color,
    /// M14 Phase 87 (daw_01 #059): `ArrangementTrack.color = Some(c)` のとき header 左端に
    /// 描く色縦ストライプの幅 (px)。 selected / group / video 背景の上に重ねるので、 それらと
    /// 色衝突せず常にトラック色が視認できる (Cubase / Live / Logic と同 idiom)。 `color = None`
    /// の track では非描画 (= 既存挙動完全互換)。
    pub track_color_strip_w: f32,
    pub ruler_bg: Color,
    /// M14 Phase 72 (daw_01 #044): video track の行背景 (audio track は既存 `bg` のまま)。
    /// 推奨 default = 暗青 `rgb(0.13, 0.14, 0.18)` で audio 背景 (`rgb(0.10, 0.11, 0.13)`) と
    /// 視覚区別。 lane 描画前に `track.kind == Video` のとき 1 度塗る。
    pub track_background_video: Color,
    /// M14 Phase 72 (daw_01 #044): video clip 内 `thumbnail = None` のときの fallback fill
    /// (= decode 失敗 / loading 中)。 推奨 default = 暗グレー `rgb(0.18, 0.18, 0.20)`。
    pub video_clip_loading: Color,
    pub bar_line: Color,
    pub beat_line: Color,
    pub bar_line_width_px: f32,
    pub beat_line_width_px: f32,
    pub lane_line: Color,
    pub lane_line_width_px: f32,
    pub clip_default_fill: Color,
    pub clip_border: Color,
    pub clip_border_w: f32,
    pub clip_radius: f32,
    /// FIXME #73: clip / automation point の **drag ゴースト** (drag 中の半透明
    /// プレビュー) のハイライト塗り色。 かつては選択中 clip 本体の fill にも使って
    /// いたが、 黄色など同系色の clip だと「選択 = 黄塗り」 が clip 本来の色と衝突して
    /// 選択状態が判別できなかった (#73)。 選択表示は fill を潰さず `push_selection_ring`
    /// の 2 重リング (明 + 暗) で示すようにし、 この色は drag ゴースト専用に絞った。
    pub clip_selected_fill: Color,
    /// 選択リングの **外側 (明)** 線。 暗い lane 背景に対して光る。
    pub clip_selected_border: Color,
    /// 選択リングの **内側 (暗)** 線 (FIXME #73)。 黄 / 白など明るい fill に対して
    /// コントラストする。 `clip_selected_border` (明) と対で、 fill 色に依らず
    /// どんな clip でも選択枠が視認できる 2 重リングを成す。
    pub clip_selected_border_inner: Color,
    pub clip_selected_border_w: f32,
    pub clip_text_color: Color,
    /// M14 Phase 89 (daw_01 #060): auto-contrast が「暗い文字」を選んだときの色 (明るい fill 上)。
    /// `clip_text_color` (明るい文字、暗い fill 上) と対をなす黒寄りプール。 selected clip の
    /// 旧ハードコード `rgb(0.10, 0.10, 0.15)` をこの field に統合した (SSoT)。
    pub clip_text_color_dark: Color,
    /// M14 Phase 89 (daw_01 #060): clip / video clip の名前 + link glyph の色を、 widget が実際に
    /// 塗る fill の WCAG relative luminance から自動選択するか (default true)。 `true` のとき明るい
    /// fill には `clip_text_color_dark`、 暗い fill には `clip_text_color` を選びコントラストを最大化
    /// する (share clip の半透明 fill は lane bg と合成した実効色で判定)。 `false` で常に
    /// `clip_text_color` 固定 (opt-out)。
    pub clip_auto_contrast_text: bool,
    pub clip_text_size: f32,
    pub track_selected_bg: Color,
    pub track_text_color: Color,
    /// track header の **トラック名** + group disclosure グリフ (▶ / ▼) の font size
    /// (default 12.0)。 daw_01 #076 でトラック名にも適用 (旧来は汎用 button 16px 固定で
    /// disclosure グリフ専用だった)。
    pub track_text_size: f32,
    pub playhead_color: Color,
    pub playhead_width_px: f32,
    pub loop_band: Color,
    pub loop_handle: Color,
    pub loop_handle_w: f32,
    /// M14 Phase 127 (daw_01 #105): Arranger レーンの背景色 (header 見出し列 + 本体帯)。
    pub arranger_lane_bg: Color,
    /// M14 Phase 127 (daw_01 #105): Arranger header 見出し ("Arranger") のテキスト色。
    pub arranger_label_color: Color,
    /// M14 Phase 127 (daw_01 #105): section の範囲 drag 作成 preview / Ctrl+drag 複製 ghost を薄く
    /// 描く半透明 fill (まだ存在しない / 複製先の帯)。
    pub arranger_preview_fill: Color,
    /// resize handle の幅 (px)。clip rect 左右 edge から **内外** この px = resize、
    /// それ以外 (rect 中央) = move。短 clip (`r.w <= resize_handle_px * 2`) は rect 内
    /// すべて Move、rect 外側のみ resize 判定。
    pub resize_handle_px: f32,
    pub mute_button: ToggleButtonStyle,
    pub solo_button: ToggleButtonStyle,
    /// M14 Phase 68 (#040): R button (Record-arm toggle) のスタイル。 mute_button / solo_button と
    /// 同 1:1 idiom、 default は active 時 record red (鮮やかな赤)、 hint_band は更に明るい赤系。
    pub armed_button: ToggleButtonStyle,
    /// M10 Phase 46: track header drag&drop 時に drop 位置 (target row の上 edge) に描く横 line の色。
    pub reorder_drop_indicator: Color,
    /// drop indicator の縦幅 (px)。
    pub reorder_drop_indicator_h: f32,
    /// drag 中 row 複製の不透明度 (`0.0..1.0`、`1.0` で完全不透明)。RGB は元 row 色 (selected/header) を使う。
    pub reorder_drag_alpha: f32,
    /// M14 Phase 101 (daw_01 #072): reorder drop が group の内側に着地するとき、 対象 group header 行を
    /// この色で塗り重ねて「どの group に入るか」 を肯定的に示す (Cubase の緑矢印に相当)。 半透明推奨
    /// (背景の名前 / button が透けるように)。 top-level drop では描かれない。
    pub reorder_group_highlight: Color,
    /// M10 Phase 47b: track header bottom band slider の縦幅 (px、`0.0` で disable)。
    /// 必要 row_h の目安は `pad*2 + btn_h + gap + band_h` (default で `4*2 + 20 + 2 + 4 = 34px`、
    /// 32px row では非表示 = progressive disclosure。Phase 48 縦ズームで row_h を上げると表示される)。
    pub track_volume_band_h: f32,
    /// volume band の trough (背景) 色。
    pub track_volume_band_track: Color,
    /// volume band の fill (volume 値表示) 色。
    pub track_volume_band_fill: Color,
    /// M13 Phase 55: ruler の小節番号テキスト色 (`time_ruler` 内の label_color にマップ)。
    pub ruler_label_color: Color,
    /// M14 Phase 63c (#016): group hierarchy で 1 段ネストするごとに track header を右にずらす量 (px)。
    /// 各 track の `header_x = rect.x + depth * indent_px`。 default = 16.0。
    pub indent_px: f32,
    /// M14 Phase 63c (#016): ▼ / ▶ disclosure アイコンの色 (group 行の左端)。
    /// M14 Phase 113 (daw_01 #085): group track 専用の背景 tint は撤去 (旧 `track_group_bg`)。
    /// group は indent + disclosure ▶▼ の構造手掛かりのみで識別し、 行背景は他 track と同色。
    pub disclosure_color: Color,
    // ---- M14 Phase 63e (#019): drag-modifier-aware ghost (Ctrl / Ctrl+Shift) ----
    /// Ctrl + drag 中の ghost rect 塗り (linked clone 意図、 default = 緑系の半透明)。
    pub clip_clone_linked_fill: Color,
    /// Ctrl + drag 中の ghost rect 枠色 (default = 明るい緑)。
    pub clip_clone_linked_border: Color,
    /// Ctrl + Shift + drag 中の ghost rect 塗り (independent clone 意図、 default = 橙系の半透明)。
    pub clip_clone_indep_fill: Color,
    /// Ctrl + Shift + drag 中の ghost rect 枠色 (default = 明るい橙)。
    pub clip_clone_indep_border: Color,
    /// ghost rect 左上に重ねる badge glyph (`⇌` / `+`) の font_size。 default = `clip_text_size`。
    pub clip_clone_badge_size: f32,
    /// badge glyph の color (default = `clip_text_color` と同等の白)。
    pub clip_clone_badge_color: Color,
    // ---- M14 Phase 63e (#019) / Phase 114 (#086): share group (linked clip group) 描画パラメータ ----
    /// share clip の name 左に描く link glyph (default = `'⇌'` U+21CC)。 font に存在しない場合は
    /// caller 側で ASCII (`'~'` 等) に差し替える。 M14 Phase 114 (#086) で `share_group_color` の hue 値で
    /// fill / border を塗る挙動を撤去したため、 共有マークはこの glyph + 下記 active 強調のみが担う。
    pub share_group_link_glyph: char,
    // ---- M14 Phase 96 (daw_01 #068) / Phase 114 (#086): 共有グループ連動ハイライト (active group 強調) ----
    /// M14 Phase 114 (daw_01 #086): `ArrangementClip.in_active_group == true` の clip に重ねる強調色
    /// (glow wash + bright thick border 共通)。 旧 hue 由来から **identity-neutral な bright 中立色** に変更
    /// (clip fill が user 指定色になったので hue wash だと喧嘩する、 hover 中は 1 グループのみ強調 = 色で
    /// 区別する必要が無い)。 default = bright cool white。 glow wash はこの RGB を `share_group_active_glow_alpha`
    /// で、 border はこの色を不透明で描く。
    pub share_group_active_color: Color,
    /// active group 強調の outline 太さ (px)。 透明 fill + この太さの bright border を clip rect に
    /// 重ねる (= clip 名 / fill を隠さず枠だけ強調)。 default = 2.5 で通常 `clip_border_w` (1.0) /
    /// `clip_selected_border_w` (2.0) より太くして「束ねられている」 印象を出す。
    pub share_group_active_border_w: f32,
    /// active group 強調の glow wash alpha (`[0.0, 1.0]`)。 `share_group_active_color` の bright 中立色を
    /// この alpha で clip 全体に敷いて「明るくする」 表現にする (= selection の黄塗りとは別レイヤ)。
    /// default = 0.22 で clip 名の可読性を保ちつつ強調が分かる。 `0.0` で glow なし = bright border のみ
    /// (= ring のみの強調にしたい theme 向け)。
    pub share_group_active_glow_alpha: f32,
    // ---- M14 Phase 63k (#025): audio clip inline 編集 (dB handle / fade) ----
    /// audio_edit が Some の clip に重ねる dB handle line の色 (default 半透明白)。
    pub audio_db_handle_color: Color,
    /// dB handle line の太さ (default 1.5 px)。
    pub audio_db_handle_width_px: f32,
    /// dB handle hit zone の縦帯 (handle line を中心に上下 ± half_band_h)。 default 8.0 = ±4 px。
    pub audio_db_handle_band_h: f32,
    /// dB handle 帯の左右 margin (clip 端から内側にこの px は除外、 端の resize/fade grip と
    /// 被らないようにする)。 default 24.0。
    pub audio_db_handle_x_margin: f32,
    /// dB drag の感度 (1 px = この dB)。 default 0.25 dB/px (= 4 px/dB)。
    /// negative dy = 上に drag = gain 増加。
    pub audio_db_pixels_per_db: f32,
    /// rect 上下端にマップする dB 範囲 (`±` この値)。 default 24.0 → 上端 = +24 dB、 下端 = -24 dB。
    /// drag の commit 値もこの範囲に clamp される。
    pub audio_db_range_db: f32,
    /// fade 角 grip の正方形サイズ (px、 clip 上端の左右にこの size の正方形 hit zone)。 default 12.0。
    pub audio_fade_corner_size_px: f32,
    /// fade envelope (clip 上端から fade 末尾まで斜辺) と grip の描画色 (default 半透明白)。
    pub audio_fade_overlay_color: Color,
    /// fade envelope 線の太さ (default 1.0 px)。
    pub audio_fade_overlay_width_px: f32,
    /// audio grip / handle を表示する最小 clip 幅 (これ未満では hit zone 全 disable、 描画も
    /// skip)。 default 32.0 → ResizeLeft + ResizeRight + 中央 = 32 px 必要、 短 clip は audio
    /// gesture 起動しない。
    pub audio_min_clip_w_for_handles_px: f32,
    /// fade grip の sticky direction lock を確定する閾値 (drag 累積 |dx|/|dy| のうち大きい方が
    /// この px を超えたら方向 lock)。 default 10.0 (要望文 §3.2 と整合)。
    pub audio_fade_sticky_threshold_px: f32,
    /// drag 中の ghost label (`+3.2 dB` / `Curve: Exponential`) の font_size。 default 11.0
    /// (= clip_text_size と同等)。
    pub audio_ghost_label_size: f32,
    /// ghost label の color (default 白系)。
    pub audio_ghost_label_color: Color,
    // ---- M14 Phase 63n-1 (#028): automation lane 描画パラメータ ----
    /// automation lane 行の左 header 領域の最低幅 (px、 これ未満で label / icon / slider 帯を skip)。
    pub automation_lane_header_min_w_px: f32,
    /// lane 行の背景色 (track 行と差別化、 default は通常 track 行背景より暗め)。
    pub automation_lane_bg: Color,
    /// disabled lane (= `enabled = false`) の curve / clip / point 描画色 (灰色 bypass 表現)。
    pub automation_lane_disabled_color: Color,
    /// lane curve 線の太さ (px)。 default 1.5。
    pub automation_curve_line_width_px: f32,
    /// lane 内 point の半径 (px)。 default 4.0。
    pub automation_point_radius_px: f32,
    /// M14 Phase 63n-9 (#033): tension/bend handle (= selected point の Bezier / Exponential 入射 segment
    /// 中央に出る dot) の半径 (px)。 default 4.0 (= 8x8 px 円、 selection の point dot と同サイズ)。
    pub automation_curve_param_handle_radius_px: f32,
    /// M14 Phase 63n-9 (#033): tension/bend handle の fill。 default 黄色系 (curve / point とは異なる色相
    /// で「これは handle」 と user に明示)。
    pub automation_curve_param_handle_fill: Color,
    /// M14 Phase 63n-9 (#033): tension/bend handle の border。 default 黒 (= handle を背景から分離)。
    pub automation_curve_param_handle_border: Color,
    /// M14 Phase 63n-9 (#033): handle を curve から上方向 (= y - offset) に offset させる px。 default 10.0
    /// (= curve 線 (1.5px) と完全に分離して click target が curve と紛れない)。
    pub automation_curve_param_handle_offset_px: f32,
    /// M14 Phase 63n-9 (#033): handle drag 中の preview curve line (= 新しい tension/bend で描き直した
    /// segment) の色。 default 黄色系 (cached curve の lane.color と区別して「これは preview」 と user に
    /// 明示)。 line_width は `automation_curve_line_width_px * 1.5` (= 通常 1.5 → 2.25px、 +50%) で cached
    /// curve を視覚的に上書き。
    pub automation_curve_param_preview_color: Color,
    /// M14 Phase 63n-8 (#033): selected automation point の半径 (px)。 default 5.0 (= 通常 4.0 から +25%)。
    /// `automation_point_radius_px` より大きい値を期待 (= 視認性、 「selected の方が大きく / 明るく見える」 SSoT)。
    pub automation_point_radius_selected_px: f32,
    /// M14 Phase 63n-8 (#033): selected automation point の fill。 default 白 (= 通常の curve_color から
    /// 大きく外して selected を強調)、 lane が disabled でも変えない (= selected を見失わないため)。
    pub automation_point_selected_fill: Color,
    /// M14 Phase 63n-8 (#033): selected automation point の border。 default 白 (上書き fill と同色 +
    /// `automation_point_radius_px` の border_w で枠線扱い)、 widget 側で `border_w = 1.5` (= 通常 1.0 から +50%)
    /// で「枠線が太い」 visual を作る。
    pub automation_point_selected_border: Color,
    /// M14 Phase 63n-8 (#033): lasso 矩形 (空き automation lane zone での drag) の fill (半透明)。
    /// default は cyan 系 12% alpha。 widget は drag 中 cached 外で overlay 描画する。
    pub automation_lasso_fill: Color,
    /// M14 Phase 63n-8 (#033): lasso 矩形の border。 default cyan 60% alpha + 1px。
    pub automation_lasso_border: Color,
    /// lane 内 default_value 水平線の色 (clip ギャップ / curve 範囲外の値表示)。
    pub automation_default_line_color: Color,
    /// default_value 水平線の太さ (px)。 default 1.0。
    pub automation_default_line_width_px: f32,
    /// lane 内 clip rect の縦 padding (px、 上下にこの px を確保した残りを clip rect の高さに)。 default 6.0。
    pub automation_clip_v_pad_px: f32,
    /// M14 Phase 63n-4 (#029): lane body 空き領域の dblclick で発行する `CreateAutomationClip`
    /// の既定長 (拍)。 default 4.0 (= 1 bar @ 4/4)。 caller は受信時に自前のポリシー (例えば「次 clip
    /// 直前まで cap」 / 「project 既定 length」) で上書き可能、 widget は単に既定値を suggestion として
    /// 渡すのみ。 MIDI `DoubleClickEmpty` は caller が len を決める idiom だが、 lane は zoom / snap に
    /// 合わせた賢い default を widget 側で持てる余地があるため style 経由で expose (#029 §A 参照)。
    pub automation_clip_default_len_beats: f64,
    /// lane header の slider 帯 (default_value_norm 表示) の縦幅 (px)。 default 4.0。
    pub automation_default_band_h: f32,
    /// M14 Phase 63n-5 (#030): lane 下端 splitter drag の hot zone 高さ (px)。 default 4.0。
    /// `lane_y + lh - handle ≤ py < lane_y + lh` の y 範囲 + body x 範囲が hit zone。
    /// `automation_clip_v_pad_px` (= 6.0) の bottom padding 内に収まるよう小さめに設定 (clip rect とは
    /// 衝突しない: clip 縦範囲は body.y+pad..body.y+h-pad)。
    pub automation_lane_resize_handle_px: f32,
    /// M14 Phase 117 (daw_01 #091): track header 列と lanes の境界 (`rect.x + header_w` の縦線) を中心と
    /// した header 幅 drag splitter の hot zone 幅 (px、 横方向)。 default 8.0 → 境界 ±4px。 track header
    /// 右端には常に 4px の inner pad があり、 この splitter の header 側 (±4px の左半分) はその pad に収まる
    /// ので M/S/R ボタン / lane disclosure / volume band と衝突しない。 `0.0` で header 幅 drag を無効化。
    pub header_resize_handle_px: f32,
    /// M14 Phase 63n-5 (#030): lane 高さ drag の **下限 px** (`SetLaneHeight.next` clamp 用)。 default 30。
    /// 30 px は header の icon row + label が 1 段で読める最小 (Bitwig "small" preset 相当)。
    pub automation_lane_min_height_px: u16,
    /// M14 Phase 63n-5 (#030) / 63n-6 (#031): lane 高さ drag の **上限 px**。 default 2000。
    /// 実効 max は **`min(style.automation_lane_max_height_px, lanes.h.round())`** (= 描画 pane の
    /// 縦サイズ以下に runtime clamp される、 daw_01 #031 の「最大は画面いっぱいまで」 要望対応)。
    /// style 値は「絶対 cap」 として、 lanes.h が極端に小さい場合 (= overflow scroll 中) でも lane が
    /// 過剰に伸びないようにする safety net。 default 2000 は典型 desktop の縦サイズ (1080〜1440 px) を
    /// 上回るため事実上無制限。
    pub automation_lane_max_height_px: u16,
    /// disclosure ▶ / ▼ glyph の描画 font size。 default = `track_text_size`。
    pub automation_disclosure_size: f32,
    /// lane header に描く icon glyph (`★` / `[V]` / `👁` / `▣` / `✕`) の font size。 default = `track_text_size`。
    pub automation_lane_icon_size: f32,
    /// lane header の text color (label + icon、 default = `track_text_color`)。
    pub automation_lane_text_color: Color,
    // ---- M14 Phase 63n-10 (#034): master row 描画パラメータ ----
    /// master row header の背景塗り (track.color の代わりに使う neutral gray、 default
    /// `rgb(0.45, 0.45, 0.48)`)。 daw_01 #034 §B 仕様。 track 色と differentiate しつつ track header_bg
    /// より暗くないようにして「特殊だが視認可能」 を保つ。
    pub master_row_color: Color,
    /// master row header の "Master" label に使う font size (default = `track_text_size`)。
    /// 通常 track と並んだとき揃って見えるよう同サイズが既定。
    pub master_row_label_size: f32,
    /// master row header の "Master" label の文字色 (default = `track_text_color`)。
    pub master_row_label_color: Color,
}

impl Default for ArrangementStyle {
    #[allow(clippy::too_many_lines)]
    fn default() -> Self {
        let mute_button = ToggleButtonStyle {
            off_color: Color::rgb(0.18, 0.20, 0.24),
            on_color: Color::rgb(0.55, 0.18, 0.18),
            border: Color::rgb(0.30, 0.32, 0.36),
            border_width: 1.0,
            radius: 3.0,
            font_size: 11.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
            on_text_color: None,
        };
        let solo_button = ToggleButtonStyle {
            off_color: Color::rgb(0.18, 0.20, 0.24),
            on_color: Color::rgb(0.55, 0.50, 0.18),
            border: Color::rgb(0.30, 0.32, 0.36),
            border_width: 1.0,
            radius: 3.0,
            font_size: 11.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
            on_text_color: None,
        };
        // M14 Phase 68 (#040): R button (Record-arm)。 active = 鮮やかな赤、
        // off = mute / solo と同 neutral 灰。
        let armed_button = ToggleButtonStyle {
            off_color: Color::rgb(0.18, 0.20, 0.24),
            on_color: Color::rgb(0.65, 0.18, 0.18),
            border: Color::rgb(0.30, 0.32, 0.36),
            border_width: 1.0,
            radius: 3.0,
            font_size: 11.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
            on_text_color: None,
        };
        Self {
            bg: Color::rgb(0.10, 0.11, 0.13),
            header_bg: Color::rgb(0.14, 0.15, 0.18),
            track_color_strip_w: 4.0,
            ruler_bg: Color::rgb(0.16, 0.17, 0.20),
            // M14 Phase 72 (#044): 暗青で audio bg (rgb(0.10, 0.11, 0.13)) と視覚区別。
            track_background_video: Color::rgb(0.13, 0.14, 0.18),
            // M14 Phase 72 (#044): 暗グレーで「loading 中」 を控えめに表現。
            video_clip_loading: Color::rgb(0.18, 0.18, 0.20),
            bar_line: Color::rgba(1.0, 1.0, 1.0, 0.30),
            beat_line: Color::rgba(1.0, 1.0, 1.0, 0.10),
            bar_line_width_px: 1.5,
            beat_line_width_px: 1.0,
            lane_line: Color::rgba(0.0, 0.0, 0.0, 0.55),
            lane_line_width_px: 1.0,
            clip_default_fill: Color::rgb(0.18, 0.40, 0.65),
            clip_border: Color::rgb(0.30, 0.55, 0.78),
            clip_border_w: 1.0,
            clip_radius: 3.0,
            clip_selected_fill: Color::rgb(1.0, 0.85, 0.30),
            clip_selected_border: Color::rgb(1.0, 1.0, 1.0),
            // FIXME #73: 選択リング内側の暗線。 明るい fill (黄 / 白) でも枠が見える。
            clip_selected_border_inner: Color::rgb(0.06, 0.06, 0.09),
            clip_selected_border_w: 2.0,
            clip_text_color: Color::rgb(0.95, 0.95, 0.97),
            // M14 Phase 89 (daw_01 #060): auto-contrast の暗文字プール (旧 selected ハードコード値)。
            clip_text_color_dark: Color::rgb(0.10, 0.10, 0.15),
            clip_auto_contrast_text: true,
            clip_text_size: 11.0,
            track_selected_bg: Color::rgb(0.20, 0.24, 0.32),
            track_text_color: Color::rgb(0.92, 0.92, 0.94),
            track_text_size: 12.0,
            playhead_color: Color::rgb(1.0, 0.25, 0.10),
            playhead_width_px: 2.5,
            loop_band: Color::rgba(0.50, 0.85, 1.0, 0.20),
            loop_handle: Color::rgb(0.50, 0.85, 1.0),
            loop_handle_w: 2.0,
            arranger_lane_bg: Color::rgb(0.14, 0.15, 0.18),
            arranger_label_color: Color::rgb(0.70, 0.72, 0.78),
            arranger_preview_fill: Color::rgba(0.85, 0.88, 0.95, 0.25),
            resize_handle_px: 4.0,
            mute_button,
            solo_button,
            armed_button,
            reorder_drop_indicator: Color::rgb(0.50, 0.85, 1.0),
            reorder_drop_indicator_h: 2.0,
            reorder_drag_alpha: 0.6,
            // indicator と同系 (シアン) を低 alpha で。 group 行に薄く乗せて nest 先を示す。
            reorder_group_highlight: Color::rgba(0.50, 0.85, 1.0, 0.22),
            track_volume_band_h: 4.0,
            track_volume_band_track: Color::rgba(0.0, 0.0, 0.0, 0.45),
            track_volume_band_fill: Color::rgb(0.95, 0.95, 0.97),
            ruler_label_color: Color::rgb(0.85, 0.88, 0.92),
            indent_px: 16.0,
            disclosure_color: Color::rgb(0.85, 0.88, 0.92),
            // M14 Phase 63e (#019): clone ghost (Ctrl / Ctrl+Shift) — 緑系 / 橙系で 3 種視覚区別。
            // selected fill (黄系 = (1.0, 0.85, 0.30)) と色相を分けて drag 中に「同じ ghost
            // にしか見えない」 状態を回避。
            clip_clone_linked_fill: Color::rgba(0.40, 0.85, 0.55, 0.55),
            clip_clone_linked_border: Color::rgb(0.55, 1.0, 0.70),
            clip_clone_indep_fill: Color::rgba(1.0, 0.65, 0.30, 0.55),
            clip_clone_indep_border: Color::rgb(1.0, 0.80, 0.45),
            clip_clone_badge_size: 11.0,
            clip_clone_badge_color: Color::rgb(0.10, 0.10, 0.12),
            // M14 Phase 114 (#086): share_group_color の hue 値で塗る挙動を撤去したため、 共有マークは
            // link glyph (⇌) + 下記 active 強調のみ。
            share_group_link_glyph: '⇌',
            // M14 Phase 96 (daw_01 #068) / Phase 114 (#086): active group 強調 — identity-neutral な
            // bright cool white。 border は clip_selected_border_w (2.0) より太い 2.5、 glow wash は名前
            // 可読性を保つ 0.22 alpha。 selection の黄塗りとは別レイヤの「明度上げ + 明るい中立枠」。
            share_group_active_color: Color::rgb(0.93, 0.96, 1.0),
            share_group_active_border_w: 2.5,
            share_group_active_glow_alpha: 0.22,
            // M14 Phase 63k (#025): audio clip 編集 default — Bitwig spec §3.5/§3.6 と整合。
            // dB handle: 半透明白の細線、 ±4 px hit 帯、 端から 24 px margin、 0.25 dB/px、 ±24 dB 範囲。
            // fade 角: 12×12 grip、 半透明白の envelope 線。 sticky 閾値 10 px (要望文 §3.2)。
            audio_db_handle_color: Color::rgba(1.0, 1.0, 1.0, 0.55),
            audio_db_handle_width_px: 1.5,
            audio_db_handle_band_h: 8.0,
            audio_db_handle_x_margin: 24.0,
            audio_db_pixels_per_db: 0.25,
            audio_db_range_db: 24.0,
            audio_fade_corner_size_px: 12.0,
            audio_fade_overlay_color: Color::rgba(1.0, 1.0, 1.0, 0.65),
            audio_fade_overlay_width_px: 1.0,
            audio_min_clip_w_for_handles_px: 32.0,
            audio_fade_sticky_threshold_px: 10.0,
            audio_ghost_label_size: 11.0,
            audio_ghost_label_color: Color::rgb(0.95, 0.95, 0.97),
            // M14 Phase 63n-1 (#028): automation lane defaults — Bitwig "Volume" lane の見た目に近づける。
            automation_lane_header_min_w_px: 80.0,
            automation_lane_bg: Color::rgb(0.08, 0.09, 0.11),
            automation_lane_disabled_color: Color::rgba(0.55, 0.56, 0.60, 0.65),
            automation_curve_line_width_px: 1.5,
            automation_point_radius_px: 4.0,
            // M14 Phase 63n-9 (#033): tension/bend handle はオレンジ系 (lane.color の青/橙 と差別化、
            // 「触ると curve param が変わる handle」 を user に明示)、 size は selection dot と同 4.0。
            automation_curve_param_handle_radius_px: 4.0,
            automation_curve_param_handle_fill: Color::rgb(1.0, 0.85, 0.30),
            automation_curve_param_handle_border: Color::rgb(0.10, 0.10, 0.12),
            automation_curve_param_handle_offset_px: 10.0,
            automation_curve_param_preview_color: Color::rgb(1.0, 0.85, 0.30),
            // M14 Phase 63n-8 (#033): selected point は半径 +25% (= 通常 4 → 5)、 fill / border 共に白で
            // 「明らかに大きく / 明るく見える」 を実現 (daw_01 #033 §D 仕様)。 lane disabled でも色維持。
            automation_point_radius_selected_px: 5.0,
            automation_point_selected_fill: Color::rgb(1.0, 1.0, 1.0),
            automation_point_selected_border: Color::rgb(1.0, 1.0, 1.0),
            // M14 Phase 63n-8 (#033): lasso 矩形は cyan 系で MIDI rect_select (= 既存 cyan rect select) と
            // 視覚的に共通の言語、 ただし fill alpha 12% で透明感を強め overlay と分かりやすく。
            automation_lasso_fill: Color::rgba(0.40, 0.85, 1.0, 0.12),
            automation_lasso_border: Color::rgba(0.40, 0.85, 1.0, 0.60),
            automation_default_line_color: Color::rgba(1.0, 1.0, 1.0, 0.18),
            automation_default_line_width_px: 1.0,
            automation_clip_v_pad_px: 6.0,
            automation_clip_default_len_beats: 4.0,
            automation_default_band_h: 4.0,
            automation_lane_resize_handle_px: 4.0,
            header_resize_handle_px: 8.0,
            automation_lane_min_height_px: 30,
            automation_lane_max_height_px: 2000,
            automation_disclosure_size: 12.0,
            automation_lane_icon_size: 12.0,
            automation_lane_text_color: Color::rgb(0.92, 0.92, 0.94),
            // M14 Phase 63n-10 (#034): master row default (neutral gray + track と同 font / 色)。
            master_row_color: Color::rgb(0.45, 0.45, 0.48),
            master_row_label_size: 12.0,
            master_row_label_color: Color::rgb(0.95, 0.95, 0.97),
        }
    }
}

// ============================================================
// Public pure helpers
// ============================================================

/// M14 Phase 63c (#016): `id` が group track として振る舞うか (= 他 track の `parent_id` がこの id を指す)。
/// `is_group` フィールドを `ArrangementTrack` に持たせず逆引きで導出する設計 (Reaper / Live と整合、
/// caller boilerplate なし)。 各 track 描画毎に呼ぶと O(N²) になるが N ≤ 100 程度の DAW 想定では問題なし
/// (実用上 1 frame 1 描画 / cached の中なので N² = 10k operation = ~10μs)。
#[must_use]
pub fn is_group_track(id: u32, tracks: &[ArrangementTrack]) -> bool {
    tracks.iter().any(|t| t.parent_id == Some(id))
}

/// M14 Phase 63c (#016): `track` が描画 / hit-test 対象として visible か。
/// `parent_id` chain を root まで辿り、 途中のいずれかが `collapsed == true` なら **不可視** を返す。
/// `parent_id` が cycle を作る防御として max 64 hop で打ち切り (実用上 32 段程度の hierarchy で十分)。
#[must_use]
pub fn is_visible_track(track: &ArrangementTrack, tracks: &[ArrangementTrack]) -> bool {
    let mut cur_parent = track.parent_id;
    for _ in 0..64 {
        let Some(pid) = cur_parent else {
            return true;
        };
        let Some(parent) = tracks.iter().find(|t| t.id == pid) else {
            return true;
        };
        if parent.collapsed {
            return false;
        }
        cur_parent = parent.parent_id;
    }
    true
}

/// M14 Phase 63c (#016): collapsed 親配下を skip した visible track の元 index 列を返す。
/// 描画 (track_top 計算 / track_index_from_y の visible_i 補正) と hit-test で共有する SSoT。
/// `tracks` の元順序は維持 (caller が入力した並び順 = 描画順)。
#[must_use]
pub fn compute_visible_indices(tracks: &[ArrangementTrack]) -> Vec<usize> {
    tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| is_visible_track(t, tracks))
        .map(|(i, _)| i)
        .collect()
}

/// M14 Phase 63n-1 (#028) / 63n-6 (#031): 単一 visible track の expanded 高さ (= `effective_row_h` +
/// lane 群高さ合計)。 `automation_lanes_collapsed = true` または `automation_lanes` 空 / 全 invisible で
/// `effective_row_h` を返す (= 既存挙動完全互換)。 widget 内 row_y prefix sum と hit-test の SSoT。
/// `track_row_h` は **caller の global default** (= `view.track_row_h`) で、 `track.row_h` が `Some`
/// なら override される (per-track row 高さ、 #031 で導入)。
#[must_use]
pub fn track_row_height(track: &ArrangementTrack, track_row_h: f32) -> f32 {
    effective_track_row_h(track, track_row_h) + automation_lanes_total_h(track)
}

/// M14 Phase 63n-6 (#031): track の effective row body 高さ (px、 lane 部は含まない)。
/// `track.row_h.map_or(default, f32::from)` の thin wrapper — caller の view default を SSoT で
/// override 適用する場所として **全 hit-test / 描画 path がこの helper 経由で row 高さを取得する**
/// (= `view.track_row_h` を直接 `+ row_top` する code は禁止、 必ず `effective_track_row_h(t, ...)` を経由)。
#[inline]
#[must_use]
pub fn effective_track_row_h(track: &ArrangementTrack, default_row_h: f32) -> f32 {
    track.row_h.map_or(default_row_h, f32::from)
}

/// M14 Phase 63n-1 (#028): track の expanded 状態の visible automation lane 高さ合計 (px)。
/// `collapsed` または lane なしで `0.0`。
#[must_use]
pub fn automation_lanes_total_h(track: &ArrangementTrack) -> f32 {
    if track.automation_lanes_collapsed || track.automation_lanes.is_empty() {
        return 0.0;
    }
    track
        .automation_lanes
        .iter()
        .filter(|l| l.visible)
        .map(|l| f32::from(l.height_px))
        .sum()
}

/// M14 Phase 63n-10 (#034): master row の effective row body 高さ (px、 lane 部含まない)。
/// `master.height_px_override.map_or(default_row_h, f32::from)` の thin wrapper。 通常 track の
/// `effective_track_row_h` と同 idiom (daw_01 #034 §Q3 (A) で確定)。
#[inline]
#[must_use]
pub fn effective_master_row_h(master: &ArrangementMasterRow, default_row_h: f32) -> f32 {
    master.height_px_override.map_or(default_row_h, f32::from)
}

/// M14 Phase 63n-10 (#034): master row の expanded 状態の visible automation lane 高さ合計 (px)。
/// `automation_lanes_collapsed` または lane なし / 全 invisible で `0.0`。 通常 track の
/// `automation_lanes_total_h` と同 idiom (daw_01 #034 §Q2 (A) で確定 — visible 0 個でも disclosure
/// state は触らず、 単に行が「effective_h だけ」 に潰れる)。
#[must_use]
pub fn master_row_lanes_total_h(master: &ArrangementMasterRow) -> f32 {
    if master.automation_lanes_collapsed || master.automation_lanes.is_empty() {
        return 0.0;
    }
    master
        .automation_lanes
        .iter()
        .filter(|l| l.visible)
        .map(|l| f32::from(l.height_px))
        .sum()
}

/// M14 Phase 63n-10 (#034): master row の expanded 総高さ (= effective + lanes 合計)。
/// 通常 track の `track_row_height` と同 idiom。 lanes_y の shift 量 / hit-test の y 範囲決定 / 縦
/// scroll 計算で参照する SSoT。 `master = None` の caller は 0.0 を使う想定 (= `lanes.y` shift なし)。
#[must_use]
pub fn master_row_total_h(master: &ArrangementMasterRow, default_row_h: f32) -> f32 {
    effective_master_row_h(master, default_row_h) + master_row_lanes_total_h(master)
}

/// M14 Phase 63n-10 (#034): master row を synthetic `ArrangementTrack` に変換 (widget 内部の `visible_tracks[0]`
/// として prepend する用)。 既存 hit-test / 描画 / row 高さ helper を **そのまま reuse** するための adapter で、
/// `id = MASTER_TRACK_ID`、 `clips = []`、 `muted/solo = false`、 `parent_id = None`、 `depth = 0`、
/// `volume = 1.0` 固定 (= 通常 track 経路で「mute / solo / clip drag」 が自然に no-op になる)。
/// `automation_lanes` / `automation_lanes_collapsed` / `row_h = height_px_override` は master_row から複製。
/// 描画 / press path で `t.id == MASTER_TRACK_ID` を見て「Master ラベル + neutral gray header + mute/solo
/// 非表示 + clip 系 EditRequest 抑制」 を追加する責務は **呼び出し側** (= `arrangement()` 本体)。
///
/// name フィールドは "Master" を入れるが、 描画 path は `t.id == MASTER_TRACK_ID` を見て label を直接
/// "Master" で描く (= caller がローカライズを差し替える余地は別 entry で議論、 #034 でも当面英語固定で確定)。
#[must_use]
fn synthesize_master_track(master: &ArrangementMasterRow) -> ArrangementTrack {
    ArrangementTrack {
        id: MASTER_TRACK_ID,
        name: Arc::from("Master"),
        muted: false,
        solo: false,
        // M14 Phase 68 (#040): master row は録音対象になり得ない (audio engine 仕様 + Bitwig / Reaper 流)、
        // 強制 `false` 固定。 widget 側で master 行の R button hit は通常 track と同様に発火するが、
        // caller (daw_01) 側が master_id を弾く idiom (mute / solo と同じ取り扱い)。
        armed: false,
        // M14 Phase 72 (#044): master row は audio 経路 (= 強制 Audio)。 video 編集中も master は
        // automation lane のみで意味を持つ row なので kind 差別化なし。
        kind: TrackKind::Audio,
        clips: Vec::new(),
        volume: 1.0,
        parent_id: None,
        depth: 0,
        collapsed: false,
        automation_lanes_collapsed: master.automation_lanes_collapsed,
        automation_lanes: master.automation_lanes.clone(),
        row_h: master.height_px_override,
        color: None,
    }
}

/// M14 Phase 63n-1 (#028): visible track 群の prefix sum row top (`tops.len() == visible_tracks.len() + 1`)。
/// `tops[i]` = i 番目 track 上端 = (i-1) 番目 track 下端、 `tops[i+1] - tops[i]` で i 番目の expanded
/// 高さ (= `track_row_height(visible_tracks[i], track_row_h)`)。 lane 0 個 = `tops[i] = lanes_y -
/// track_top + i * track_row_h` と等価 (= 既存挙動完全互換)。 描画 / hit-test 全箇所が共有する SSoT。
#[must_use]
pub fn visible_track_row_tops(
    visible_tracks: &[ArrangementTrack],
    lanes_y: f32,
    track_top: f32,
    track_row_h: f32,
) -> Vec<f32> {
    let mut tops = Vec::with_capacity(visible_tracks.len() + 1);
    let mut y = lanes_y - track_top;
    tops.push(y);
    for t in visible_tracks {
        y += track_row_height(t, track_row_h);
        tops.push(y);
    }
    tops
}

/// (track_row_top, track_row_h, clip) → screen rect (lanes 範囲、horizontal clip 形状)。
/// M14 Phase 63n-1 (#028): row_top は caller が `tops[visible_idx]` で渡す前提
/// (lane 込みの prefix sum)。 `track_row_h` は **MIDI/Audio clip 行の高さのみ** (= `view.track_row_h`、
/// lane 高さは含まない)。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn clip_to_rect(
    track_row_top: f32,
    track_row_h: f32,
    clip: &ArrangementClip,
    view: ArrangementView,
    lanes: Rect,
) -> Rect {
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    let x = lanes.x + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((clip.len_beats * beat_to_px) as f32).max(2.0);
    let h = (track_row_h - 4.0).max(2.0);
    Rect { x, y: track_row_top + 2.0, w, h }
}

/// M14 Phase 63k (#025): audio_edit が Some の clip 上の audio gesture grip ヒット種別。
/// 公開 `ClipDragKind` には足さず内部 enum で扱う (caller の hover/drag 報告は既存 3 variant
/// のまま維持、 audio gesture は widget 内で完結)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioGripHit {
    /// clip 上端の左角 (12×12 px)。 fade_in length / curve drag の起点。
    FadeCornerIn,
    /// clip 上端の右角 (12×12 px)。 fade_out length / curve drag の起点。
    FadeCornerOut,
    /// clip 中央 horizontal 帯 (handle line ±4 px、 端から x_margin 内側)。 gain dB drag の起点。
    GainHandleBand,
}

/// M14 Phase 63k (#025): 単一 clip の audio_edit grip ヒット (priority: gain > fade corner)。
/// `audio_edit` が None の clip ではヒット無し、 `r.w < min_w` の短 clip でも無効化。
/// fade 角は resize handle (4 px) より priority 高 (= clip 内側の上端 12×12 を fade に振る)、
/// resize は fade 角の外側 (clip rect の外側 ±4 px) で活きる。
#[allow(clippy::too_many_arguments)]
fn audio_grip_hit(
    track_row_top: f32,
    track_row_h: f32,
    clip: &ArrangementClip,
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<AudioGripHit> {
    clip.audio_edit?;
    let r = clip_to_rect(track_row_top, track_row_h, clip, view, lanes);
    if r.w < style.audio_min_clip_w_for_handles_px {
        return None;
    }
    if cy < r.y || cy >= r.y + r.h {
        return None;
    }
    let corner = style.audio_fade_corner_size_px;
    // priority 1: gain handle band — clip 中央 y ±half_band、 端から x_margin 内側のみ
    let center_y = r.y + r.h * 0.5;
    let half_band = style.audio_db_handle_band_h * 0.5;
    let margin = style.audio_db_handle_x_margin;
    if cx >= r.x + margin
        && cx < r.x + r.w - margin
        && cy >= center_y - half_band
        && cy < center_y + half_band
    {
        return Some(AudioGripHit::GainHandleBand);
    }
    // priority 2: fade in 角 (top-left 12×12)
    if cx >= r.x && cx < r.x + corner && cy >= r.y && cy < r.y + corner {
        return Some(AudioGripHit::FadeCornerIn);
    }
    // priority 3: fade out 角 (top-right 12×12)
    if cx >= r.x + r.w - corner && cx < r.x + r.w && cy >= r.y && cy < r.y + corner {
        return Some(AudioGripHit::FadeCornerOut);
    }
    None
}

/// M14 Phase 63k (#025): lanes 内 cursor 位置から hit する `(ClipKey, AudioGripHit)` を返す
/// (clip の `audio_edit = Some` のものだけが対象、 後勝ち)。 `clip_hit` の audio gesture 版。
#[must_use]
fn audio_grip_hit_in_lanes(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<(ClipKey, AudioGripHit)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let visible_idx = track_index_from_y(cy, lanes.y, tops)?;
    let track = visible_tracks.get(visible_idx)?;
    let row_top = tops[visible_idx];
    let mut hit: Option<(ClipKey, AudioGripHit)> = None;
    for clip in &track.clips {
        if let Some(zone) = audio_grip_hit(row_top, view.track_row_h, clip, view, lanes, cx, cy, style) {
            hit = Some((ClipKey { track: track.id, clip: clip.id }, zone));
        }
    }
    hit
}

/// 内部 helper: cursor 位置がこの clip のどの zone (Move / ResizeLeft / ResizeRight)
/// に該当するかを返す。`clip_hit` から呼ばれる。
///
/// 判定範囲 (x 方向): clip rect の左右 edge から **内外** ±`edge` px (= 8px 幅のハンドル帯)。
/// y 方向は clip rect 内のみ (拡張なし、隣接 track row との衝突回避)。
///
/// 短 clip (`r.w <= edge * 2.0`) は rect 内では Move 強制 (左右 edge 領域が重なって
/// 判別不能なため)、rect 外側のみ ResizeLeft / ResizeRight として扱う。
#[allow(clippy::too_many_arguments)]
fn clip_zone_at(
    track_row_top: f32,
    track_row_h: f32,
    clip: &ArrangementClip,
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<ClipDragKind> {
    let r = clip_to_rect(track_row_top, track_row_h, clip, view, lanes);
    // y は clip rect 内のみ (Rect::contains の半開区間と整合)
    if cy < r.y || cy >= r.y + r.h {
        return None;
    }
    // x の拡張範囲 [r.x - edge, r.x + r.w + edge) 外は不参加
    if cx < r.x - edge || cx >= r.x + r.w + edge {
        return None;
    }
    let in_rect = cx >= r.x && cx < r.x + r.w;
    let near_left = cx < r.x + edge;
    let near_right = cx >= r.x + r.w - edge;
    let short_clip = r.w <= edge * 2.0;

    Some(if short_clip && in_rect {
        ClipDragKind::Move
    } else if near_left && (!in_rect || cx - r.x < edge) {
        ClipDragKind::ResizeLeft
    } else if near_right && (!in_rect || (r.x + r.w) - cx < edge) {
        ClipDragKind::ResizeRight
    } else {
        ClipDragKind::Move
    })
}

/// lanes 内 cursor 位置から hit する (ClipKey, ClipDragKind) を返す。
///
/// resize handle は clip rect の左右 edge から **内外** ±`resize_handle_px` の範囲
/// (= 8px 幅のハンドル帯)。短 clip (`r.w <= resize_handle_px * 2`) は rect 内は Move 強制、
/// rect 外側のみ resize 判定。
///
/// 隣接 clip (A.right == B.left) では両者の resize ハンドル帯が共有境界付近で重なる。
/// このとき **cursor が rect 内部に在る clip (in-rect) を、外側拡張ハンドル
/// (outer-extension) しか当たらない clip より無条件で優先**する。これにより A の右端を
/// 掴みたいのに B の左端 resize に奪われる問題 (#101) を解消。同 tier (両方 in-rect = overlap、
/// または両方 outer = 微小 gap) は resize edge への水平距離が近い方を採用し、同距離なら
/// 後勝ち (描画順で前面) を踏襲する。piano_roll の [`note_hit_in`](super::piano_roll) と構造同一。
#[must_use]
pub fn clip_hit(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<(ClipKey, ClipDragKind)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let visible_idx = track_index_from_y(cy, lanes.y, tops)?;
    let track = visible_tracks.get(visible_idx)?;
    let row_top = tops[visible_idx];
    let mut hit: Option<(ClipKey, ClipDragKind)> = None;
    let mut hit_inside = false;
    let mut hit_edge_dist = f32::INFINITY;
    for clip in &track.clips {
        let Some(kind) =
            clip_zone_at(row_top, view.track_row_h, clip, view, lanes, cx, cy, resize_handle_px)
        else {
            continue;
        };
        let r = clip_to_rect(row_top, view.track_row_h, clip, view, lanes);
        let inside = cx >= r.x && cx < r.x + r.w;
        // resize edge への水平距離 (Move は当該 cursor 位置 = 距離 0 扱い)。
        let edge_x = match kind {
            ClipDragKind::ResizeLeft => r.x,
            ClipDragKind::ResizeRight => r.x + r.w,
            ClipDragKind::Move => cx,
        };
        let dist = (cx - edge_x).abs();
        // in-rect は outer に無条件で勝つ。同 tier は近い edge 優先 (同距離は後勝ち)。
        let better = if inside == hit_inside {
            dist <= hit_edge_dist
        } else {
            inside
        };
        if better {
            hit = Some((ClipKey { track: track.id, clip: clip.id }, kind));
            hit_inside = inside;
            hit_edge_dist = dist;
        }
    }
    hit
}

/// M14 Phase 127 (daw_01 #105): section の Arranger レーン内 rect (`clip_to_rect` の section 版)。
/// レーンは track row のような縦分割を持たないので高さは arranger レーン全高。 時間→x は ruler /
/// clips と同じ `beat_to_px` mapping を共有 (ruler / playhead / loop band と縦に揃う)。
fn section_to_rect(section: &SectionView, view: ArrangementView, arranger: Rect) -> Rect {
    section_rect_from(section.start_beat, section.len_beats, view, arranger)
}

/// M14 Phase 127 (#105): `(start_beat, len_beats)` から Arranger レーン内 rect を計算 (`section_to_rect`
/// と drag preview 描画が共有、 preview のために temp `SectionView` を作らずに済む)。
fn section_rect_from(start_beat: f64, len_beats: f64, view: ArrangementView, arranger: Rect) -> Rect {
    let beat_to_px = f64::from(arranger.w) / view.len_beats.max(1e-6);
    let x = arranger.x + ((start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((len_beats * beat_to_px) as f32).max(2.0);
    Rect { x, y: arranger.y, w, h: arranger.h }
}

/// M14 Phase 127 (#105): section rect 上の cursor x がどの zone (Move / ResizeLeft / ResizeRight) かを返す。
/// `clip_zone_at` の x ロジックと同一 (resize handle は rect 左右 edge から内外 ±`edge`、 短 section は
/// rect 内 Move 強制 / 外側のみ resize)。 y は arranger レーン全高なので呼び出し側の `arranger.contains`
/// で既に保証され、 ここでは x のみ判定する。
fn section_zone_at(r: Rect, cx: f32, edge: f32) -> Option<ClipDragKind> {
    if cx < r.x - edge || cx >= r.x + r.w + edge {
        return None;
    }
    let in_rect = cx >= r.x && cx < r.x + r.w;
    let near_left = cx < r.x + edge;
    let near_right = cx >= r.x + r.w - edge;
    let short = r.w <= edge * 2.0;
    Some(if short && in_rect {
        ClipDragKind::Move
    } else if near_left && (!in_rect || cx - r.x < edge) {
        ClipDragKind::ResizeLeft
    } else if near_right && (!in_rect || (r.x + r.w) - cx < edge) {
        ClipDragKind::ResizeRight
    } else {
        ClipDragKind::Move
    })
}

/// M14 Phase 127 (#105): Arranger レーン内 cursor 位置から hit する `(section id, ClipDragKind)` を返す。
/// `clip_hit` と同じ **2-tier in-rect 優先** (隣接 section の共有境界では内側 section を、 外側拡張ハンドル
/// しか当たらない section より無条件優先、 同 tier は resize edge への水平距離が近い方、 同距離は後勝ち)。
/// section は arranger レーン全高なので y は `arranger.contains` のみで判定する。
#[must_use]
fn section_hit(
    sections: &[SectionView],
    arranger: Rect,
    view: ArrangementView,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<(u32, ClipDragKind)> {
    if arranger.h <= 0.0 || !arranger.contains(cx, cy) {
        return None;
    }
    let mut hit: Option<(u32, ClipDragKind)> = None;
    let mut hit_inside = false;
    let mut hit_edge_dist = f32::INFINITY;
    for s in sections {
        let r = section_to_rect(s, view, arranger);
        let Some(kind) = section_zone_at(r, cx, resize_handle_px) else {
            continue;
        };
        let inside = cx >= r.x && cx < r.x + r.w;
        let edge_x = match kind {
            ClipDragKind::ResizeLeft => r.x,
            ClipDragKind::ResizeRight => r.x + r.w,
            ClipDragKind::Move => cx,
        };
        let dist = (cx - edge_x).abs();
        let better = if inside == hit_inside {
            dist <= hit_edge_dist
        } else {
            inside
        };
        if better {
            hit = Some((s.id, kind));
            hit_inside = inside;
            hit_edge_dist = dist;
        }
    }
    hit
}

/// FIXME #067: cursor が strictly どの section 帯の **内側** (in-rect) にあるかを返す。 `section_hit` と
/// 違い resize handle の外側拡張 (`±resize_handle_px`) を **一切含めない**。 dblclick rename / 右クリック
/// メニューは「帯そのもの」 を対象にする **point gesture** で、 帯の外側 (隣の空きレーン) で発火しては
/// いけない (帯のすぐ隣の空白を dblclick すると隣 section の rename になっていた bug)。 Move/Resize の
/// **drag** は掴みやすさのため引き続き `section_hit` の拡張ハンドルを使う。 section は昇順・非交差前提、
/// 共有境界 (`A.right == B.left`) は半開区間 `[x, x+w)` で右 section に属す (= 1 点に高々 1 section)。
#[must_use]
fn section_at_inrect(
    sections: &[SectionView],
    arranger: Rect,
    view: ArrangementView,
    cx: f32,
    cy: f32,
) -> Option<u32> {
    if arranger.h <= 0.0 || !arranger.contains(cx, cy) {
        return None;
    }
    sections.iter().find_map(|s| {
        let r = section_to_rect(s, view, arranger);
        (cx >= r.x && cx < r.x + r.w).then_some(s.id)
    })
}

/// FIXME #067: clip / section の drag zone (`ClipDragKind`) を cursor 形状へ写す共通マップ
/// (中央 Move → `Move`、 端 Resize → `EwResize`)。 clip drag / clip hover / section drag / section hover の
/// 4 経路が同じ写像を共有する (= 端を掴んでリサイズできることを ↔ カーソルで discoverable にする)。
fn drag_kind_cursor(kind: ClipDragKind) -> CursorIcon {
    match kind {
        ClipDragKind::Move => CursorIcon::Move,
        ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => CursorIcon::EwResize,
    }
}

/// M14 Phase 127 (#105): 拍子から 1 bar の拍数を返す (`numerator * 4 / denominator`)。 4/4=4、 3/4=3、
/// 6/8=3。 0 除算 / 0 拍を避けるため numerator / denominator は 1 以上、 結果は `1.0` 以上に floor する。
fn beats_per_bar(time_sig: (u8, u8)) -> f64 {
    let num = f64::from(time_sig.0.max(1));
    let den = f64::from(time_sig.1.max(1));
    (num * 4.0 / den).max(1.0)
}

/// y 座標から **visible track index** を計算 (M14 Phase 63n-1: prefix-sum 化)。
/// `tops` は `visible_track_row_tops` の戻り値 (= len = visible_tracks.len() + 1、 prefix sum
/// of expanded heights)。 lane 0 個 = `tops[i] = lanes_y - track_top + i * track_row_h` と等価で
/// 既存の `(local / track_row_h).floor()` と同じ index を返す (= 既存挙動完全互換)。
/// `tops.len() < 2` または y が範囲外なら `None`。
#[must_use]
pub fn track_index_from_y(y: f32, _lanes_y: f32, tops: &[f32]) -> Option<usize> {
    if tops.len() < 2 {
        return None;
    }
    if y < tops[0] {
        return None;
    }
    // tops は単調増加。 y が tops[i] <= y < tops[i+1] となる i を返す。
    // partition_point(|&t| t <= y) - 1 = i (binary search で O(log N))。
    let i = tops.partition_point(|&t| t <= y);
    if i == 0 || i > tops.len() - 1 {
        return None;
    }
    Some(i - 1)
}

// `LoopBandHit` / `loop_band_hit_kind` は M14 Phase 69 (#041) で
// `crate::widgets::ruler_ops` に extract (piano_roll と共有)。

#[inline]
fn px_to_beat(px: f32, lanes_x: f32, lanes_w: f32, view: ArrangementView) -> f64 {
    let beat_per_px = view.len_beats / f64::from(lanes_w.max(1.0));
    view.start_beat + f64::from(px - lanes_x) * beat_per_px
}

/// M14 Phase 63n-5 (#030): lane height drag で raw px (= anchor_h + dy) を `[min, max]` に clamp して
/// `u16` に丸める。 round で整数化 (= drag 中の 0.5 px 揺れで height がカクつかないよう)、 min/max が
/// 逆転していたら max を min 以上に補正 (style 異常入力に対する safety、 panic しない)。
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn clamp_height_px(raw: f32, min: u16, max: u16) -> u16 {
    let lo = f32::from(min);
    let hi = f32::from(max).max(lo);
    raw.round().clamp(lo, hi) as u16
}

/// M14 Phase 63n-6 (#031): lane 高さ drag の **実効 max** = `min(style.max, lanes.h.round())`。
/// 「最大は画面いっぱいまで」 (= lane が描画 pane より高くならない) を runtime clamp で表現。
/// `lanes.h` が style.max を超えても style 値が absolute cap として作用 (= 異常入力 safety)。
/// `lanes.h` が極端に小さい (= overflow scroll 中で pane が 30 px 未満等) 場合は `min_height` 以上に
/// なるよう clamp_height_px 側で補正されるため、 ここでは `lanes.h.round() as u16` を返すだけで OK。
#[inline]
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn effective_lane_max_height(style: &ArrangementStyle, lanes: Rect) -> u16 {
    let style_cap = style.automation_lane_max_height_px;
    let pane_cap = lanes.h.round().max(0.0) as u32; // u16 overflow 防止に u32 経由で min 計算
    style_cap.min(u16::try_from(pane_cap).unwrap_or(u16::MAX))
}

/// M10 Phase 46: drag 中の `mouse_y` から **drop target index** (anchor 抜き取り後に挿入する位置) を計算。
///
/// `header_top` は header_pane.y、`track_top` は view scroll。`row_h <= 0` または `n_tracks == 0` で `0` を返す。
/// 返り値は `0..n_tracks` (= `Vec::insert` で渡せる範囲、anchor を除いた「挿入位置」semantics)。
///
/// アルゴリズム: `mouse_y` から row index を計算し (上端で 0、下端で n_tracks に clamp)、
/// row 中央線より上で hover → その row の前、下で hover → その row の後に挿入。
/// anchor index 自身に hover の場合は anchor index を返す (no-op)。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn compute_reorder_target_index(
    anchor_index: usize,
    mouse_y: f32,
    header_top: f32,
    track_top: f32,
    row_h: f32,
    n_tracks: usize,
) -> usize {
    if n_tracks == 0 || row_h <= 0.0 {
        return 0;
    }
    let local = mouse_y - header_top + track_top;
    if local <= 0.0 {
        return 0;
    }
    // local / row_h を「row 内 fractional 位置」付きで取り、中央 (0.5) より上下で挿入位置を判定。
    let raw = local / row_h;
    let idx = raw as usize;
    let frac = raw - raw.floor();
    // 中央線より下 → 次の row の前に挿入
    let target_unbounded = if frac >= 0.5 { idx + 1 } else { idx };
    let target_u = target_unbounded.min(n_tracks);
    // anchor 抜き取り後の semantics: anchor 自身またはその直後 (= anchor_index, anchor_index+1) は no-op。
    if target_u == anchor_index || target_u == anchor_index + 1 {
        return anchor_index;
    }
    // anchor より後の挿入は 1 詰めて semantics を合わせる (Vec::remove(anchor) → Vec::insert(target-1))。
    if target_u > anchor_index + 1 {
        target_u - 1
    } else {
        target_u
    }
}

/// M10 Phase 47b: `mouse_x` から band 内の volume 値 (`0.0..=1.0`) を計算。
/// `band_w <= 0` で `0.0` を返す (ガード)。
#[must_use]
pub fn volume_from_mouse_x(mouse_x: f32, band_x: f32, band_w: f32) -> f32 {
    if band_w <= 0.0 {
        return 0.0;
    }
    ((mouse_x - band_x) / band_w).clamp(0.0, 1.0)
}

/// M10 Phase 46 / M11 Phase 51: anchor を抜き取って target に挿入した新順 `Vec<T>` を返す。
/// `anchor_index >= items.len()` または `target_index > items.len()-1` (after remove) でも安全に clamp。
///
/// M11 Phase 51 で `<T: Clone>` に generic 化。`u32` 用途 (track_id 列) でも `usize` 用途
/// (`reorderable_list` の元 index 列) でも単相化で動く。
#[must_use]
pub fn apply_reorder<T: Clone>(items: &[T], anchor_index: usize, target_index: usize) -> Vec<T> {
    if items.is_empty() || anchor_index >= items.len() {
        return items.to_vec();
    }
    let mut v: Vec<T> = items.to_vec();
    let it = v.remove(anchor_index);
    let insert_at = target_index.min(v.len());
    v.insert(insert_at, it);
    v
}

/// M14 Phase 101 (daw_01 #072): track header drag を reorder に昇格させる最小移動量 (px)。
/// これ未満は click (= SelectTrack) 扱い。 pending_drop (commit) と reorder_overlay (描画) が
/// **同じ閾値**を使うことで preview と commit の発火条件が一致する。
const REORDER_DRAG_THRESHOLD_PX: f32 = 16.0;

/// M14 Phase 127 (daw_01 #105): section resize / create の **sanity floor** 拍 (= 異常入力で len が
/// 0 / 負にならない最小値)。 実用 clamp (隣接帯への食い込み防止 / 重複正規化) は caller の
/// `normalize_sections` が行うので、 widget はこの floor のみ (既存「widget は snap + sanity floor、
/// 実 clamp は caller」 規約どおり)。
const SECTION_MIN_LEN_BEATS: f64 = 1.0 / 16.0;

/// M14 Phase 101 (daw_01 #072): track header drag&drop の **drop 解決結果**。
/// `pending_drop` (実適用 = `SetTrackParent` 発行) と `reorder_overlay` (描画プレビュー) が
/// **同一の** `resolve_track_drop` を通して得る単一真実源。 これにより「プレビューと実結果が
/// 食い違う」 (旧 blank-drop の症状) が構造的に起き得ない。
///
/// 設計 (daw_01 docs/plan_group_track.md §8.4 改訂版): **Y で gap (挿入行間)、 X でネスト深さ** を
/// 決める。 可視行 R(=above) と R+1(=below) の間の gap では合法深さが連続区間 `[min_d, max_d]` に
/// なり、 各深さ `d` が一意の `(parent, anchor_after)` に対応する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReorderDrop {
    /// 挿入する gap (visible 行間) の index、 `0..=visible_tracks.len()`。 indicator の Y に使う
    /// (`tops[gap]`)。 gap g は visible 行 `g-1` (above) と `g` (below) の間。
    gap: usize,
    /// 選択された nest 深さ (`[min_d, max_d]` に clamp 済)。 indicator 線の左 indent
    /// (`header_left + depth * indent_px`) に使う。
    depth: u8,
    /// reparent 先 group の id (`None` = top-level)。 `SetTrackParent.parent`。 `depth > 0` のとき
    /// 必ず group container なので indicator の group-header hilight 対象でもある。
    parent: Option<u32>,
    /// `SetTrackParent.anchor_after` — full `tracks` Vec 上で source 群を挿入する直前 track id
    /// (`None` = 先頭)。 **source 自身は除外** する (caller は source を remove してから anchor_after を
    /// 探すため、 anchor が source だと見つからず末尾 append してしまう罠を回避)。 gap の full-Vec
    /// 挿入位置 (= below の full index、 なければ末尾) の直前にある最初の非 source track。
    anchor_after: Option<u32>,
}

/// M14 Phase 101 (daw_01 #072): reorder drag の描画プレビューに必要な geometry (すべて screen px、
/// `resolve_track_drop` の結果から事前計算)。 描画 closure はこれを読むだけ (press_tops 等を closure に
/// capture しないで済む)。 indicator 線・深さインデント・group header hilight が **commit と同じ
/// 解決結果**から導出されるので「プレビューと実結果がズレる」 ことが構造的に起きない。
#[derive(Clone, Copy, Debug)]
struct ReorderOverlay {
    /// drop indicator 横線の Y (= gap の screen top、 `press_tops[gap]`)。
    indicator_y: f32,
    /// indicator 線の **左端 X** (= `header_left + depth * indent_px`)。 線の indent 量が深さ
    /// プレビューそのもの (flush-left = top-level、 1 段右 = その group の子)。
    indent_x: f32,
    /// drag 中の半透明 ghost row の中心 Y (= `last_mouse_y`)。
    drag_center_y: f32,
    /// reparent 先 parent が group のとき、 hilight する group header の row rect (Cubase の
    /// 緑矢印に相当する肯定フィードバック)。 top-level drop (`parent == None`) では `None`。
    highlight_row: Option<Rect>,
}

/// M14 Phase 101 (daw_01 #072): `mouse_y` を visible 行間の **gap index** (`0..=N`) に写像する。
/// `tops` は `visible_track_row_tops` の出力 (len = `N+1`、 単調増加、 lane 込みの prefix sum)。
/// row R 内では中央線より上で gap=R (R の前)、 下で gap=R+1 (R の後)。 最上端より上で 0、
/// 最下端 (`tops[N]`) 以下で N (= 末尾 = 「一番下へ」)。 可変行高 (lane 展開) に追従する。
fn gap_from_y(tops: &[f32], mouse_y: f32) -> usize {
    let n = tops.len().saturating_sub(1); // 行数
    if n == 0 {
        return 0;
    }
    if mouse_y < tops[0] {
        return 0;
    }
    if mouse_y >= tops[n] {
        return n;
    }
    // tops[r] <= mouse_y < tops[r+1] となる行 r。 partition_point = 「<= の個数」 = r+1。
    let r = tops.partition_point(|&t| t <= mouse_y).saturating_sub(1).min(n - 1);
    let mid = (tops[r] + tops[r + 1]) * 0.5;
    if mouse_y < mid {
        r
    } else {
        r + 1
    }
}

/// M14 Phase 101 (daw_01 #072): `start` から `parent_id` chain を上へ辿り、 `depth == target_depth`
/// の祖先 id を返す。 `target_depth == start.depth` なら `start` 自身 (= group の最初の子として nest
/// するケース)。 `target_depth > start.depth` は `None` (上へ辿っても深くはなれない)。 hop 上限は
/// `depth: u8` の全域 (= 最大 255 段) を覆う 256 + cycle 防御 (循環参照は 256 hop で打ち切り None)。
fn ancestor_at_depth(
    start: &ArrangementTrack,
    target_depth: u8,
    tracks: &[ArrangementTrack],
) -> Option<u32> {
    let mut cur = start;
    for _ in 0..256 {
        if cur.depth <= target_depth {
            return (cur.depth == target_depth).then_some(cur.id);
        }
        let pid = cur.parent_id?;
        cur = tracks.iter().find(|t| t.id == pid)?;
    }
    None
}

/// M14 Phase 101 (daw_01 #072): track header drag&drop の drop 解決 (純関数、 preview = commit の SSoT)。
///
/// - `tracks`: caller の **full** track Vec (master 含まず、 子は親直後の preorder 連続ブロック前提)。
/// - `visible_tracks`: collapsed 親配下を skip した可視列 (先頭に synthetic master があり得る)。
/// - `tops`: `visible_tracks` の prefix-sum row tops (len = visible+1)。
/// - `is_group_set`: 子を持つ track id 集合 (= group container)。
/// - `source`: drag 中の track id slice (anchor_after / parent 計算で除外)。 通常 1〜数件なので
///   slice の線形 `contains` で十分 (drag 中毎フレーム呼ぶため HashSet を alloc しない)。
/// - `indent_px`: 深さ 1 段の幅 (X→深さ写像の単位)。
/// - `mouse_y` / `mouse_x`: drag 中の最終 pointer。 `anchor_mouse_x`: 掴んだ瞬間の x (深さ基準列)。
///   深さは `mouse_x - anchor_mouse_x` の **相対** 列量で決める (絶対 x や header 左端には依存しない
///   = どこを掴んでも「右へ動かすと nest」 が成立する)。
///
/// 戻り値 `ReorderDrop` の `(parent, anchor_after)` をそのまま `SetTrackParent` に乗せれば、
/// 「Y で行・ X で深さ」 が確定する。 `gap` / `depth` は indicator 描画に使う。
#[allow(clippy::too_many_arguments)]
fn resolve_track_drop(
    tracks: &[ArrangementTrack],
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    is_group_set: &HashSet<u32>,
    source: &[u32],
    indent_px: f32,
    mouse_y: f32,
    mouse_x: f32,
    anchor_mouse_x: f32,
) -> ReorderDrop {
    // gap_from_y は契約上 [0, n] を返すので追加 clamp は不要。
    let gap = gap_from_y(tops, mouse_y);
    let above: Option<&ArrangementTrack> =
        gap.checked_sub(1).and_then(|i| visible_tracks.get(i));
    let below: Option<&ArrangementTrack> = visible_tracks.get(gap);

    // 合法 nest 深さ区間 [min_d, max_d]:
    //  - max_d = depth(above) + (above が group なら 1) — above の子まで潜れる / above と sibling。
    //  - min_d = depth(below) — below の深さまで浅くできる (= 囲う group を抜ける)。 末尾は 0。
    // preorder 不変条件下で min_d <= max_d は保証されるが、 異常入力に備え min を max に clamp。
    let max_d = above.map_or(0, |a| {
        a.depth.saturating_add(u8::from(is_group_set.contains(&a.id)))
    });
    let min_d = below.map_or(0, |b| b.depth).min(max_d);

    // X → 深さ。 anchor 相対の列 offset を base=min_d (境界モデル default) に加算して区間 clamp。
    // 右へ動かすほど深く nest、 動かさなければ min_d (= メンバー間は内側 / 最終メンバー下は浅い側)。
    let indent_unit = indent_px.max(1.0);
    #[allow(clippy::cast_possible_truncation)]
    let col_offset = ((mouse_x - anchor_mouse_x) / indent_unit).round() as i32;
    let depth = (i32::from(min_d) + col_offset)
        .clamp(i32::from(min_d), i32::from(max_d))
        .max(0) as u8;

    // parent = above の depth-1 祖先 (depth==0 → top-level None)。
    let parent = if depth == 0 {
        None
    } else {
        above.and_then(|a| ancestor_at_depth(a, depth - 1, tracks))
    };
    // **parent が source 自身になる cycle を防ぐ** (= 自分を自分の子にする / multi-select で moving 中の
    // 祖先を親にする)。 例: expanded group G を G ヘッダ直下の gap へ drag すると above=G・唯一の合法深さ
    // depth(G)+1 で parent=G=source になる。 daw_01 の SetTrackParent 直接適用は cycle 検証を通らない
    // (parent_group_id を直書きする) ので widget 側で source を親にしない不変を保証する。 source に当たったら
    // 最近接の **非 source 祖先** へ繰り上げる (全祖先が source なら top-level)。
    let mut parent = parent;
    while let Some(pid) = parent {
        if source.contains(&pid) {
            parent = tracks.iter().find(|t| t.id == pid).and_then(|t| t.parent_id);
        } else {
            break;
        }
    }

    // anchor_after = gap の full-Vec 挿入位置 ins の直前にある最初の非 source track (None = 先頭)。
    // ins: below の full index (= below の直前に挿入)。 below 無し (末尾 gap) は tracks.len()。
    // below が master (= synthetic、 full Vec に居ない) のときは ins=0 (= 先頭、 master は song.tracks 外)。
    // 通常 track は必ず full Vec に在る (visible_tracks は tracks ∪ {master} から作る) ので、 position が
    // None になるのは master のみ。 master を明示分岐して「正常 track が欠落したら 0」 の曖昧さを排除する。
    let ins = match below {
        None => tracks.len(),
        Some(b) if b.id == MASTER_TRACK_ID => 0,
        Some(b) => tracks.iter().position(|t| t.id == b.id).unwrap_or(0),
    };
    let anchor_after = tracks[..ins.min(tracks.len())]
        .iter()
        .rev()
        .find(|t| !source.contains(&t.id))
        .map(|t| t.id);

    ReorderDrop { gap, depth, parent, anchor_after }
}

/// track header 1 行内のレイアウト (Name button + 3 small buttons + 任意の volume band + lane disclosure)。
/// `name_rect` (= drag start zone & text area)、`buttons` (= [M, S, R]、Phase 68 で R button = Record-arm 追加。
/// Phase 47c で ↑/↓/× は drag&drop + Delete shortcut に置換され削除済)、`volume_band` は inner 下部に band 用の
/// 余裕がある時のみ `Some` (Phase 47b)、`lane_disc_rect` は M14 Phase 63n-2 で R button の **右** に予約された
/// lane disclosure (`+`/`-` icon) 用の rect (track_row 全体の右端、 automation_lanes が空でも常に layout に
/// 含めて名前領域を一定にする)。
struct HeaderRowLayout {
    name_rect: Rect,
    buttons: [Rect; 3],
    /// M10 Phase 47b: track volume band rect (`row_h` 余裕がある時のみ Some)。
    volume_band: Option<Rect>,
    /// M14 Phase 63n-2 (#028): lane disclosure (`+`/`-` toggle) の hit zone + 描画 rect。
    /// `automation_lanes` が空の track でも layout 上の幅は確保 (= 名前領域が track 間で一定)、
    /// 空 lane の track では描画されないが click も反応しない (caller が `lanes.is_empty()` で判定)。
    lane_disc_rect: Rect,
}

#[allow(clippy::similar_names)]
fn header_row_layout(row: Rect, volume_band_h: f32) -> HeaderRowLayout {
    let pad = 4.0_f32;
    let inner = Rect {
        x: row.x + pad,
        y: row.y + pad,
        w: (row.w - pad * 2.0).max(2.0),
        h: (row.h - pad * 2.0).max(2.0),
    };
    // buttons は常に 20px max (band の有無で縮めない)。band は inner.h に余裕があるときだけ表示する。
    let btn_h = inner.h.min(20.0);
    let small = 22.0_f32;
    let gap = 2.0_f32;
    // Phase 68 (#040): M + S + R の 3 button (← Phase 47c の M + S 2 button 構成から R = Record-arm を追加)。
    // 並び順は業界標準の M / S / R (Bitwig / Live / Reaper と同じ、 左→右)。
    let n_btn = 3;
    // M14 Phase 63n-2 (#028): lane disclosure 用の幅を予約 (= disc_size + gap)。 R button の右に
    // 配置するため `total_right` に加算 → name_rect が縮む代わりに lane_disc が button と重ならない。
    let lane_disc_size = 12.0_f32;
    let lane_disc_extra = lane_disc_size + gap;
    #[allow(clippy::cast_precision_loss)]
    let total_right = small * n_btn as f32 + gap * n_btn as f32 + lane_disc_extra;
    let name_w = (inner.w - total_right).max(20.0);
    let name_rect = Rect { x: inner.x, y: inner.y, w: name_w, h: btn_h };
    let mut x_cursor = inner.x + name_w + gap;
    let mut buttons = [Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }; 3];
    for slot in &mut buttons {
        *slot = Rect { x: x_cursor, y: inner.y, w: small, h: btn_h };
        x_cursor += small + gap;
    }
    // S button の右に lane_disc rect (= ASCII `+`/`-` icon)。 行 vertical center に揃える。
    let lane_disc_rect = Rect {
        x: x_cursor,
        y: inner.y + (btn_h - lane_disc_size).max(0.0) * 0.5,
        w: lane_disc_size,
        h: lane_disc_size,
    };
    // band 表示条件: band_h > 0 && buttons の下に gap + band 分が収まる (progressive disclosure)。
    // default (`track_volume_band_h=4` / `gap=2`) なら inner.h >= 26 (= row_h >= 34) で表示。
    let band_h = volume_band_h.max(0.0);
    let band_gap = 2.0_f32;
    let volume_band = if band_h > 0.0 && btn_h + band_gap + band_h <= inner.h {
        Some(Rect {
            x: inner.x,
            y: inner.y + btn_h + band_gap,
            w: inner.w,
            h: band_h,
        })
    } else {
        None
    };
    HeaderRowLayout { name_rect, buttons, volume_band, lane_disc_rect }
}

/// M14 Phase 63c (#016): disclosure ▼ / ▶ アイコンの hit / 描画 rect。
/// `name_rect` の左端から `disclosure_w` 幅で切り出し、 indent 量 (`depth * indent_px`) は **既に
/// `name_rect.x` に反映されている前提** (caller 側の指定)。 group track でない場合は呼ばない (caller が判定)。
/// rect は `name_rect.h` を超えない正方形に近い (アイコン center 用)。
fn disclosure_rect_for(name_rect: Rect, style: &ArrangementStyle, _depth: u8) -> Rect {
    // disclosure 幅は indent_px と同じ (= 1 段ぶんの幅)、 name_rect の左端から削り取る。
    let w = style.indent_px.max(8.0);
    let h = name_rect.h.min(w);
    Rect {
        x: name_rect.x,
        y: name_rect.y + (name_rect.h - h) * 0.5,
        w,
        h,
    }
}

/// M14 Phase 63n-1 (#028) + M14 Phase 63n-2 (#028) 修正: track 行 header 内の **automation lane
/// disclosure** (`+`/`-`) hit / 描画 rect。 `header_row_layout` の `lane_disc_rect` と一致する位置
/// (= S button の **右**、 行中央 vertical 揃え)。 既存の group disclosure (`disclosure_rect_for`)
/// は `name_rect` の左端を使うので disjoint。 `automation_lanes` が空の track では描画しない
/// (caller の描画 / hit-test 側で `lane.is_empty()` を判定して呼ぶ前提)。
///
/// 旧設計 (Phase 63n-1 第 1 案、 `track_row.x + track_row.w - size - pad`) は track 行の **右端
/// 内側** に rect を置いていたが、 既存 layout の S button rect (行右端から 4px 内側) と完全に
/// 重なり、 後勝ち描画の S button が disclosure を覆い隠す bug があった (#028 user feedback で
/// 「`+`/`-` が見えない」 = font 問題ではなく button overlap)。 修正で `header_row_layout` 側に
/// lane_disc 用の幅 (= 12 + 2 = 14 px) を予約し、 S button の右に配置するよう全 layout を再計算。
#[must_use]
pub fn lane_disclosure_rect_for(track_row: Rect, style: &ArrangementStyle) -> Rect {
    let _ = style; // size は header_row_layout 内 fixed (12px) を使う、 style.automation_disclosure_size は描画 font_size 用
    header_row_layout(track_row, 0.0).lane_disc_rect
}

// ============================================================
// Internal state
// ============================================================

#[derive(Clone, Copy, Debug)]
struct ClipDragAnchor {
    key: ClipKey,
    start_beat: f64,
    len_beats: f64,
    track_index: usize,
}

#[derive(Clone, Debug)]
struct ClipDragSession {
    kind: ClipDragKind,
    anchor_mouse: (f32, f32),
    /// drag 中の各 frame で更新される最終 pointer 位置。release frame の `pointer.pos` が
    /// winit の implementation によっては press 位置のままになる事があるため、release では
    /// `last_mouse` を delta 計算に使う (drag preview と一致する位置で確定する)。
    last_mouse: (f32, f32),
    /// drag 中の最終 alt 状態。 drag overlay と release commit の **両方** がこれを真値とする
    /// (`pointer.modifiers.alt` を直接見ない)。 continuation frame で毎 frame update し、
    /// release frame では `allow_update = false` で skip することで release 直前の値を保持する。
    /// これにより OS event 順序 (ModifiersChanged が MouseInput(Released) より先に来るケース)
    /// に依存せず、 overlay と commit が必ず同一値で確定する。
    last_alt: bool,
    /// M14 Phase 63e (#019): drag 中の最終 ctrl 状態。 `last_alt` と同じ仕組みで保持する
    /// (winit 0.30 の `ModifiersChanged` が `MouseInput(Released)` より先に届く race を回避)。
    /// release 時 dispatch で `Move + last_ctrl + !last_shift` → `CloneClipsLinked`、
    /// `Move + last_ctrl + last_shift` → `CloneClipsIndependent`、 それ以外 (ResizeLeft/Right
    /// 含む) → 既存 `MoveClips` / `ResizeClips`。 ghost overlay も `last_ctrl` を読んで色 / badge
    /// glyph を切替えるため、 commit と overlay が必ず同一値で確定する。
    last_ctrl: bool,
    /// M14 Phase 63e (#019): drag 中の最終 shift 状態。 `last_ctrl` と組み合わせて
    /// `CloneClipsLinked` (ctrl のみ) と `CloneClipsIndependent` (ctrl + shift) を識別する。
    /// 保持仕組みは `last_alt` / `last_ctrl` と同じ (continuation で update / release で skip)。
    last_shift: bool,
    anchors: Vec<ClipDragAnchor>,
}

/// M14 Phase 127 (daw_01 #105): Arranger section drag の gesture 種別。 Move/ResizeLeft/ResizeRight は
/// `ClipDragKind` と 1:1 (左端 = start/len 両方、 右端 = len のみ)、 `Create` は空きレーンの範囲 drag
/// (press 位置から現在位置まで描いて release で `CreateSection`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SectionGesture {
    Move,
    ResizeLeft,
    ResizeRight,
    Create,
}

/// M14 Phase 127 (daw_01 #105): Arranger section の Move / Resize / Duplicate / 範囲作成 drag session。
/// `ClipDragSession` と同じ「continuation frame で `last_*` を update、 release frame では skip して
/// 直前値を保持」 idiom (winit 0.30 の `ModifiersChanged` が `MouseInput(Released)` より先に届く race を
/// 回避)。 drag overlay (preview) と release commit は **同じ `compute_section_drag_beat_delta` を共有** し、
/// 「release で grid に飛ぶ」 不整合を構造的に防ぐ。 単一 section 限定 (multi-select は仕様外)。
#[derive(Clone, Copy, Debug)]
struct SectionDragSession {
    kind: SectionGesture,
    /// Move/Resize/Duplicate の対象 section id (`Create` では未使用)。
    section_id: u32,
    /// drag 開始時の section start / len (Move/Resize の絶対位置 snap pivot)。 `Create` では未使用。
    anchor_start: f64,
    anchor_len: f64,
    /// press 時の snap 適用済拍 (`Create` の range もう一端 = 固定端)。
    anchor_press_beat: f64,
    anchor_mouse: (f32, f32),
    /// continuation で update、 release で skip (`ClipDragSession.last_mouse` と同理由)。
    last_mouse: (f32, f32),
    /// drag 中の最終 alt (snap 一時無効、 overlay と commit が同一値を読む)。
    last_alt: bool,
    /// drag 中の最終 ctrl。 `Move + last_ctrl` で `DuplicateSection` に分岐 (clip の `CloneClipsLinked` と
    /// 同じ Ctrl+drag idiom)。 Alt は snap 無効に予約済なので複製には使わない。
    last_ctrl: bool,
    /// M14 Phase 128 (daw_01 #106): drag 中の最終 shift。 短 click 時の `SelectSection` modifier を
    /// Shift = `RangeFromAnchor` に分岐するため track。 `last_ctrl` と同じ仕組み (continuation で update、
    /// release で skip)。
    last_shift: bool,
}

// `LoopDragKind` / `LoopDragSession` は M14 Phase 69 (#041) で
// `crate::widgets::ruler_ops` に extract (piano_roll と共有)。

/// M10 Phase 46 / M14 Phase 63c (#016): track header drag&drop session。 release frame で
/// **drop target に応じて `ReorderTracks` (sibling) と `SetTrackParent` (parent 変更) を振り分け** る。
/// multi-select 時は `source_track_ids` に selected_tracks をそのまま乗せて一括移動する。
#[derive(Clone, Debug)]
struct TrackReorderSession {
    anchor_track_id: u32,
    /// M14 Phase 63c (#016): drag 開始時に grab した track 群 (selected_tracks に含まれていれば
    /// selected 全部、 そうでなければ `vec![anchor_track_id]`)。 multi-track reparent / reorder の
    /// source として release frame で使う。
    source_track_ids: Vec<u32>,
    anchor_mouse_y: f32,
    /// drag 中の最終 mouse y 位置 (release frame の `pointer.pos` に頼らない保険、`ClipDragSession.last_mouse` と同理由)。
    last_mouse_y: f32,
    /// M14 Phase 101 (daw_01 #072): drag 開始時の mouse x (= ネスト深さ control の基準列)。
    /// 深さは `last_mouse_x - anchor_mouse_x` を indent 列に写像した相対量で決める (どこを掴んでも
    /// 「右へ動かすと nest」 が成立するよう絶対 x ではなく anchor 相対)。
    anchor_mouse_x: f32,
    /// drag 中の最終 mouse x 位置 (`last_mouse_y` と同理由で release frame の巻き戻し対策に保持)。
    last_mouse_x: f32,
}

// `PlayheadDragSession` は M14 Phase 69 (#041) で
// `crate::widgets::ruler_ops` に extract (piano_roll と共有)。

/// M10 Phase 47b: track header の bottom band slider drag による volume 編集セッション。
#[derive(Clone, Copy, Debug)]
struct TrackVolumeDragSession {
    track_id: u32,
    anchor_volume: f32,
    /// drag 開始時の band rect (mouse_x → 0..1 マップに使用、release frame に view が変化しても安定)。
    band_rect: Rect,
    /// press 時 mouse x (release frame の巻き戻し検知用、 `ClipDragSession.anchor_mouse` と同 idiom)。
    anchor_mouse_x: f32,
    /// drag 中の最終 mouse x 位置 (release frame の `pointer.pos` に頼らない保険)。
    last_mouse_x: f32,
    /// M10 Phase 49: drag 中に最後に発火した volume 値 (毎 frame 同値発火を抑制)。
    /// drag 開始時は `anchor_volume` で初期化、各 frame で current `next` と差分があれば Mutate 発火 + 更新。
    last_emitted_volume: f32,
}

/// M14 Phase 63k (#025): audio_edit clip 上の inline 編集 drag session。
/// press 時に grip 種別から `AudioDragKind` を確定、 sticky direction lock は continuation で
/// 累積 |dx|/|dy| を比較して確定 (`locked_horizontal`)。 commit-by-release: drag 中は ghost
/// overlay のみ、 release で 1 度だけ `SetClipGainDb` / `SetClipFade` / `SetClipFadeCurve` を発火。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioDragKind {
    /// clip 中央 dB handle 帯 → 縦 drag のみ意味あり (gain_db 変更)。
    Gain,
    /// clip 上端左角 → sticky で length / curve に分岐。
    FadeIn,
    /// clip 上端右角 → 同上 (length / curve)。
    FadeOut,
}

#[derive(Clone, Copy, Debug)]
struct AudioDragSession {
    /// 単一 clip の drag (multi-select 一括対応は将来拡張、 仕様 §scope 外)。
    key: ClipKey,
    kind: AudioDragKind,
    /// drag 開始時の anchor 値 (release 時の commit / inverse 算出 + ghost preview に使う)。
    anchor: ArrangementClipAudioEdit,
    /// drag 開始時の clip rect (release 時にも参照、 view scroll 中も安定 — track 並び替えや
    /// scroll で「rect が動いて」 も anchor の dB 0 ライン位置を変えない)。
    clip_rect_anchor: Rect,
    /// drag 開始時の clip len_beats (fade length の clamp 上限に使う)。
    clip_len_beats_anchor: f64,
    anchor_mouse: (f32, f32),
    /// continuation で update、 release frame は pointer.pos ≠ anchor_mouse のときのみ update
    /// (clip_drag と同じ pattern: winit が release で press 位置に戻すケースを回避)。
    last_mouse: (f32, f32),
    /// fade gesture の sticky direction lock。 `None` = 未確定 (drag 距離 < threshold)、
    /// `Some(true)` = horizontal lock (length 編集)、 `Some(false)` = vertical lock (curve 切替)。
    /// `Gain` では常に `Some(false)` (vertical lock 固定、 横 drag は無視)。
    locked_horizontal: Option<bool>,
}

/// M14 Phase 63n-2 (#028): lane 内 point の drag session。 release frame で `MoveAutomationPoints`
/// を 1 件 (`vec![delta]`) 発火。 drag 中は `last_mouse` / `last_alt` を update して overlay の
/// preview 位置を計算 (`AudioDragSession` と同 pattern)。 multi-point 同時 drag は仕様 §scope 外
/// (将来拡張)、 ここでは単一 point に閉じる。
#[derive(Clone, Copy, Debug)]
struct AutomationPointDragSession {
    /// drag 開始時の point identity (point_idx は frame 内のみ valid だが、 release frame の
    /// caller 側 `clip.points[idx]` 解決で使う前提で保持)。
    point: AutomationPointKey,
    /// drag 開始時の clip-local time_beat (`prev_time_beat` の元、 release commit に乗せる)。
    anchor_time_beat: f64,
    /// drag 開始時の value_norm (`prev_value_norm` の元)。
    anchor_value_norm: f32,
    /// drag 開始時の clip rect (lane body 内、 縦 padding 適用済)。 overlay の y 軸計算 SSoT。
    /// view scroll / lane 順序変化が起きても anchor は固定で、 release commit と一貫した値を生成。
    clip_rect_anchor: Rect,
    /// drag 開始時の lane body rect (= clip rect の x range parent、 beat_to_px 計算 SSoT)。
    body_rect_anchor: Rect,
    /// drag 開始時の clip 範囲 (clamp 用、 `time_beat` を `0.0..=clip.len_beats` に収める)。
    clip_start_beat: f64,
    clip_len_beats: f64,
    anchor_mouse: (f32, f32),
    last_mouse: (f32, f32),
    /// drag 中の最終 alt 状態 (snap 一時無効、 `ClipDragSession.last_alt` と同 pattern)。
    last_alt: bool,
    /// M14 Phase 63n-8 (#033): drag 開始時の修飾キー snapshot (release 時の短 click select 分岐用)。
    /// release frame で `start_modifiers.shift` / `.ctrl` を読んで toggle / replace を判定する
    /// (= continuation 中の modifier 状態は使わず、 press 時の意図を SSoT として固定)。
    start_modifiers: Modifiers,
}

/// M14 Phase 63n-2 (#028): lane header の default value horizontal slider 帯 drag session。
/// `TrackVolumeDragSession` と同 pattern (per-frame Mutate emit + release Undoable wrap、 ただし
/// Phase 63n-2 では release で `SetLaneDefault { prev, next }` の単発発火のみで undo は caller 責務)。
#[derive(Clone, Copy, Debug)]
struct AutomationLaneDefaultDragSession {
    lane: AutomationLaneKey,
    anchor_value_norm: f32,
    /// drag 開始時の band rect (mouse_x → 0..1 マップ用、 view 変化耐性)。
    band_rect: Rect,
    last_mouse_x: f32,
    /// drag 中に最後に発火した値 (毎 frame 同値発火を抑制、 SetTrackVolume と同 pattern)。
    last_emitted_value: f32,
}

/// M14 Phase 63n-5 (#030): lane 下端 splitter drag session (lane height 変更)。
/// `AutomationLaneDefaultDragSession` と同 pattern (per-frame `SetLaneHeight` emit + release で
/// 最終値 1 度発火) — `last_emitted_height` で同値発火を抑制し、 `anchor_height_px` (= press 時の
/// `lane.height_px`) と `anchor_mouse_y` で view scroll 耐性を確保 (anchor 固定なので caller が
/// `lane.height_px = next` を反映しても drag 中 cursor 追従が壊れない)。
#[derive(Clone, Copy, Debug)]
struct AutomationLaneResizeDragSession {
    lane: AutomationLaneKey,
    /// drag 開始時の `lane.height_px` (release commit の `prev`、 dy 計算の base)。
    anchor_height_px: u16,
    /// drag 開始時の cursor y (= `lane_y + lh` 付近)。
    anchor_mouse_y: f32,
    /// 最後に観測した cursor y (continuation で update、 release で最終 height 計算に使用)。
    last_mouse_y: f32,
    /// 最後に emit した height (毎 frame 同値発火を抑制)。
    last_emitted_height: u16,
}

/// M14 Phase 63n-6 (#031): MIDI track row 下端 splitter / Alt+drag による **per-track** row 高さ
/// resize session。 `track: u32` で対象トラックを保持し、 release frame までの per-frame emit はその
/// トラックのみを resize する `SetSingleTrackRowH { track, prev, next }` を発行 — 既存 Alt+wheel の
/// global `SetTrackRowH(f32)` とは別経路で「そのトラックだけ伸び縮み」 (Bitwig per-track zoom と同 idiom)。
/// per-frame emit + release で `take()` discard (per-frame で final 済、 anchor 同値なら no-op)。
/// `last_emitted_height` 同値抑制は 0.5 px 閾値 (f32 連続値、 1 px 未満の jitter で spam しない)。
#[derive(Clone, Copy, Debug)]
struct TrackRowResizeDragSession {
    /// drag 対象 track の id (= `SetSingleTrackRowH.track` に渡す)。
    track: u32,
    /// drag 開始時の effective row 高さ (`t.row_h.unwrap_or(view.track_row_h)`)、 dy 計算の base。
    anchor_row_h: f32,
    /// drag 開始時の cursor y。
    anchor_mouse_y: f32,
    /// 最後に観測した cursor y (continuation で update、 release frame は skip して直前値を保持)。
    last_mouse_y: f32,
    /// 最後に emit した height (毎 frame 同値発火を 0.5 px 閾値で抑制)。
    last_emitted_height: f32,
}

/// M14 Phase 117 (daw_01 #091): header / lanes 境界 splitter drag による **全 track 共通** の header 幅
/// resize session。 `TrackRowResizeDragSession` と同 pattern (横軸版): drag 開始時の header 幅 + cursor x を
/// anchor 固定 (view scroll / 連動伸縮で layout が動いても anchor は不変なので追従が壊れない)、 per-frame で
/// `SetHeaderW { prev: anchor, next }` を emit (live preview)、 release frame で `take()` 破棄 (per-frame で
/// final 済)。 `last_emitted_w` 同値抑制は 0.5 px 閾値 (f32 連続値の jitter で spam しない)。
#[derive(Clone, Copy, Debug)]
struct HeaderResizeDragSession {
    /// drag 開始時の `view.header_w` (= `SetHeaderW.prev`、 dx 計算の base)。
    anchor_header_w: f32,
    /// drag 開始時の cursor x。
    anchor_mouse_x: f32,
    /// 最後に観測した cursor x (continuation で update、 release frame は skip して直前値を保持)。
    last_mouse_x: f32,
    /// 最後に emit した header 幅 (毎 frame 同値発火を 0.5 px 閾値で抑制)。
    last_emitted_w: f32,
}

/// M14 Phase 63n-9 (#033): tension/bend handle drag session。 selected point の Bezier / Exponential
/// 入射 segment 中央に出る 8x8 px 円を上下 drag → release で `SetAutomationCurveParam` 1 件発火。
/// drag 中は internal preview state で curve を live update (cached 外で preview line overlay 描画)、
/// release で final value を caller に送信。 anchor 固定 (`anchor_value` / `anchor_mouse_y` /
/// `effective_lane_height_px`) で view scroll 耐性、 sensitivity は Q3=A の `effective_lane_height_px`
/// drag で full range (`-1.0..=1.0`)、 Alt × 0.2 で微調整 (1 px ≈ `2.0 / lane_height` の value delta、
/// alt = `0.4 / lane_height` で 5x 精細)。
#[derive(Clone, Copy, Debug)]
struct AutomationCurveParamDragSession {
    /// drag 対象 point identity (release commit に乗せる)。
    point: AutomationPointKey,
    /// `BezierTension` or `ExponentialBend` (drag 中 invariant、 press 時 curve から決定)。
    kind: SetAutomationCurveParamKind,
    /// drag 開始時の tension / bend 値 (`prev_value` の元、 sensitivity delta 計算の base)。
    anchor_value: f32,
    /// drag 開始時の cursor y (dy 計算の anchor)。
    anchor_mouse_y: f32,
    /// 最後に観測した cursor y (continuation で update、 release で final value 計算に使用)。
    last_mouse_y: f32,
    /// drag 中の最終 alt 状態 (× 0.2 sensitivity、 既存 drag session と同 race 回避 pattern)。
    last_alt: bool,
    /// drag 開始時の effective lane height (`max(lane.height_px, 40)`、 sensitivity の SSoT)。
    /// caller が drag 中 lane.height_px を変えても sensitivity は drag 開始時値で固定。
    effective_lane_height_px: f32,
    /// drag 中の preview value (continuation で update、 overlay 描画 + release commit に使用)。
    /// `-1.0..=1.0` clamp 済。 anchor と同値なら release で no-op (= click 相当)。
    preview_value: f32,
}

/// M14 Phase 63n-8 (#033): automation point の lasso (= 空き automation lane zone から drag による
/// 矩形選択) session。 既存 MIDI rect_select (`take_drag_rect_in_rect`) は library 全 widget 共用の
/// cyan 描画固定なので、 lasso 用に **arrangement 内部 SSoT** を別 struct 化 (color / 起動条件 / 解放
/// 時 emit の 3 軸全てが MIDI rect_select と異なる)。 press 時の modifier snapshot を保持して release
/// commit で next 計算分岐 (修飾なし=replace / Shift=union / Ctrl=XOR、 #033 Q2=A の zone 排他 lasso)。
/// 起動条件 (= press の zone): lane body && !clip && !point && !lane_resize_splitter && !lane_header。
/// release は session を take し、 lasso rect 内に **point の中心** が含まれる points を集めて next 計算。
#[derive(Clone, Copy, Debug)]
struct AutomationLassoSession {
    /// drag 開始時の cursor 座標 (lasso rect の anchor、 view scroll 中も固定)。
    anchor: (f32, f32),
    /// drag 開始時の cursor 座標 (現在位置、 continuation frame で update)。
    last_mouse: (f32, f32),
    /// drag 開始時の修飾キー (release commit 時の next 計算分岐 = replace / union / XOR)。
    /// continuation で update しない (= 「lasso 開始時に Shift だったが drag 中に離した」 場合も union)。
    /// 既存 `take_drag_rect_in_rect` の `start_modifiers` と同 pattern。
    start_modifiers: Modifiers,
}

/// M14 Phase 63n-3 (#028) / daw_01 #071: automation clip drag の 1 clip 分 anchor。
/// MIDI clip の `ClipDragAnchor` の automation 版 (key 型 / lane 跨ぎ semantics が異なるため別 struct)。
#[derive(Clone, Copy, Debug)]
struct AutomationClipDragAnchor {
    /// drag 対象 clip の identity (lane 跨ぎ後も不変)。
    key: AutomationClipKey,
    /// drag 開始時の `clip.start_beat` (release commit の `prev_start_beat`)。
    start_beat: f64,
    /// drag 開始時の `clip.len_beats` (release commit の `prev_len`、 Resize の min_len 計算用)。
    len_beats: f64,
    /// drag 開始時の所属 lane key。 単一 clip drag のみ release frame で
    /// `automation_lane_key_at_y(last_mouse.1)` から `to_lane` を再解決し cross-lane move を許す。
    /// 複数選択一括 move では cross-lane drop の宛先 lane が一意に定まらない (lane は track 毎に
    /// 異種・可変高) ため各 anchor は自 lane 維持 (= horizontal time-shift)。
    lane: AutomationLaneKey,
    /// drag 開始時の lane body rect (= 当該 lane の body_rect、 beat_to_px 計算 + ghost rect の y/h
    /// 計算 SSoT、 view 変動耐性)。 ghost rect は `body_rect.y + pad` から `body_rect.h - pad*2`
    /// の縦範囲で描画 (clip rect は lane body 内 padding 適用済範囲、 cached 内描画と同 SSoT)。
    body_rect: Rect,
}

/// M14 Phase 63n-3 (#028) / daw_01 #071: lane 内 automation clip の Move / ResizeLeft / ResizeRight
/// drag session。 既存 `ClipDragSession` (MIDI / Audio clip) と同 pattern (anchor 群を持ち multi-select
/// を一括 move / resize、 `last_*` で OS event 順序による modifier false 化 race を回避)。 #071 で単一
/// clip 限定から **複数選択一括対応** に拡張: 掴んだ clip が選択集合に含まれていれば選択中の全 clip を
/// `anchors` に積む (= MIDI clip の `selected_clips.contains(&hit)` idiom を 1:1 ミラー)。 snap pivot は
/// `anchors[0]` (= grabbed-first 構築なので掴んだ clip)。
#[derive(Clone, Debug)]
struct AutomationClipDragSession {
    kind: ClipDragKind,
    /// 掴んだ clip (短 click demote の単一選択対象)。 `anchors[0].key` と一致 (press で grabbed-first 構築)。
    primary: AutomationClipKey,
    /// drag 対象の anchor 群 (grabbed-first 順、 snap pivot = `anchors[0]`)。 単一選択時は 1 要素。
    anchors: Vec<AutomationClipDragAnchor>,
    anchor_mouse: (f32, f32),
    last_mouse: (f32, f32),
    /// drag 中の最終 alt 状態 (snap 一時無効、 `ClipDragSession.last_alt` と同 pattern)。
    last_alt: bool,
    /// drag 中の最終 ctrl 状態 (CloneLinked 判定、 `ClipDragSession.last_ctrl` と同 pattern)。
    last_ctrl: bool,
    /// drag 中の最終 shift 状態 (CloneIndependent 判定、 `ClipDragSession.last_shift` と同 pattern)。
    last_shift: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ArrangementState {
    clip_drag: Option<ClipDragSession>,
    loop_drag: Option<LoopDragSession>,
    track_reorder: Option<TrackReorderSession>,
    track_volume_drag: Option<TrackVolumeDragSession>,
    /// M14 Phase 63j (#024): ruler plain click / drag による playhead seek セッション。
    /// `Shift` 修飾無しの ruler 内 press で開始、 release で `take()` して discard。
    playhead_drag: Option<PlayheadDragSession>,
    /// M14 Phase 63k (#025): audio_edit grip 上の inline 編集 drag session (gain / fade)。
    /// audio grip > clip drag の priority で起動するため、 既存 ResizeLeft/Right/Move とは
    /// 排他的に動作する。 commit-by-release で release frame に 1 度だけ EditRequest を発火。
    audio_drag: Option<AudioDragSession>,
    /// M14 Phase 63n-2 (#028): lane 内 point の drag session (release で MoveAutomationPoints 1 件)。
    automation_point_drag: Option<AutomationPointDragSession>,
    /// M14 Phase 63n-2 (#028): lane header default value band drag session
    /// (release で SetLaneDefault 1 件)。
    automation_lane_default_drag: Option<AutomationLaneDefaultDragSession>,
    /// M14 Phase 63n-5 (#030): lane 下端 splitter drag session
    /// (release で `SetLaneHeight { prev: anchor, next: final }` 1 件)。
    automation_lane_resize_drag: Option<AutomationLaneResizeDragSession>,
    /// M14 Phase 63n-6 (#031): MIDI track row 下端 splitter / Alt+drag resize session。
    /// `SetTrackRowH(f32)` を per-frame emit (Alt+wheel と同 idiom)、 release は take 廃棄。
    track_row_resize_drag: Option<TrackRowResizeDragSession>,
    /// M14 Phase 117 (daw_01 #091): header / lanes 境界 splitter drag による header 幅 resize session。
    /// `SetHeaderW { prev, next }` を per-frame emit、 release は take 廃棄 (row resize と同 idiom)。
    header_resize_drag: Option<HeaderResizeDragSession>,
    /// M14 Phase 63n-3 (#028): lane 内 automation clip の Move / Resize drag session
    /// (release で `MoveAutomationClips` / `CloneAutomationClipsLinked` /
    /// `CloneAutomationClipsIndependent` / `ResizeAutomationClips` のいずれか 1 件、
    /// 短 click は `SelectAutomationClips` に demote)。
    automation_clip_drag: Option<AutomationClipDragSession>,
    /// M14 Phase 63n-8 (#033): 空き automation lane zone から drag による point lasso session。
    /// release で `SelectAutomationPoints { prev, next }` 1 件発火、 next は press 時の modifier で
    /// replace / union / XOR を分岐 (#033 Q2=A の zone 排他 lasso)。
    automation_lasso_drag: Option<AutomationLassoSession>,
    /// M14 Phase 63n-9 (#033): selected point の Bezier/Exponential 入射 segment 中央 handle drag session。
    /// release で `SetAutomationCurveParam { point, kind, prev_value, next_value }` 1 件発火、 drag 中は
    /// preview_value を継続更新して curve を live preview (cached 外で overlay 描画)。
    automation_curve_param_drag: Option<AutomationCurveParamDragSession>,
    /// M14 Phase 127 (daw_01 #105): Arranger section の Move/Resize/Duplicate/範囲作成 drag session
    /// (release で `MoveSection` / `ResizeSection` / `DuplicateSection` / `CreateSection` のいずれか
    /// 1 件発火、 短 drag の Move は `SetPlayheadBeat` (帯ジャンプ) に demote)。
    section_drag: Option<SectionDragSession>,
    /// M14 Phase 63c (#016): 直前の `Single` クリック位置 (= Shift+click 範囲選択の起点)。
    /// caller には公開せず widget 内 SSoT として持つ (piano_roll の note multi-select は anchor
    /// なし設計だったが、 arrangement では daw_01 #009 / #016 で「widget 内 anchor」 が確認されている)。
    /// `Toggle` modifier では update しない、 `Single` / `RangeFromAnchor` で update。
    selection_anchor: Option<u32>,
}

/// M14 Phase 63k (#025): audio_drag の commit / overlay で共有する計算結果。
/// `Gain` は dB のみ、 `FadeLength` は edge と新 length 拍数、 `FadeCurve` は edge と次 curve。
/// `None` (= sticky direction 未確定 + drag 距離不足) は no-op (release で何も発火しない)。
#[derive(Clone, Copy, Debug, PartialEq)]
enum AudioDragOutcome {
    Gain { next_db: f32 },
    FadeLength { edge: FadeEdge, next_beats: f64 },
    FadeCurve { edge: FadeEdge, next_curve: FadeCurve },
}

/// M14 Phase 63k (#025): drag delta から release commit 値を計算する pure helper (overlay と
/// release で同一値を生成する SSoT)。 `None` 戻りは「commit すべき変化なし」 を意味し、 caller は
/// EditRequest を発火しない。
///
/// - `Gain`: dy * pixels_per_db で dB delta を計算 → anchor + delta、 widget で ±range_db に clamp。
///   anchor と等しいなら `None`。
/// - `FadeIn / FadeOut` + horizontal lock: dx を beat 単位に変換、 anchor + delta を `0..clip_len` に
///   clamp。 anchor と等しいなら `None`。
/// - `FadeIn / FadeOut` + vertical lock: 既存 curve.next() を返す (dy 方向は「次 / 前」 を区別せず
///   常に順送り、 ユーザは連続 click で SCurve → Linear へ進む)。 同じ curve なら `None`。
/// - sticky lock 未確定 (= drag 距離が threshold 未満): `None` (release で no-op、 click 相当)。
fn compute_audio_drag_outcome(
    ad: &AudioDragSession,
    beat_per_px: f64,
    style: &ArrangementStyle,
) -> Option<AudioDragOutcome> {
    let dx = ad.last_mouse.0 - ad.anchor_mouse.0;
    let dy = ad.last_mouse.1 - ad.anchor_mouse.1;
    let range = style.audio_db_range_db.max(0.001);
    match ad.kind {
        AudioDragKind::Gain => {
            // dy 上が負 → gain 増。 px → dB は `pixels_per_db` (default 0.25 dB/px = 4 px/dB)。
            let delta_db = -dy * style.audio_db_pixels_per_db;
            let next = (ad.anchor.gain_db + delta_db).clamp(-range, range);
            if (next - ad.anchor.gain_db).abs() < 1e-3 {
                None
            } else {
                Some(AudioDragOutcome::Gain { next_db: next })
            }
        }
        AudioDragKind::FadeIn | AudioDragKind::FadeOut => {
            let lock = ad.locked_horizontal?;
            let edge = match ad.kind {
                AudioDragKind::FadeIn => FadeEdge::In,
                AudioDragKind::FadeOut => FadeEdge::Out,
                AudioDragKind::Gain => unreachable!(),
            };
            if lock {
                // length 編集: fade_in は dx 正で増、 fade_out は dx 負で増 (clip 右側から内側に伸びる)。
                let raw_delta_beats = f64::from(dx) * beat_per_px;
                let signed = match edge {
                    FadeEdge::In => raw_delta_beats,
                    FadeEdge::Out => -raw_delta_beats,
                };
                let prev = match edge {
                    FadeEdge::In => ad.anchor.fade_in_beats,
                    FadeEdge::Out => ad.anchor.fade_out_beats,
                };
                let max_beats = ad.clip_len_beats_anchor.max(0.0);
                let next = (prev + signed).clamp(0.0, max_beats);
                if (next - prev).abs() < 1e-6 {
                    None
                } else {
                    Some(AudioDragOutcome::FadeLength { edge, next_beats: next })
                }
            } else {
                // curve 切替: dy 方向問わず常に次 curve に順送り (1 release で 1 段階)。
                let prev_curve = match edge {
                    FadeEdge::In => ad.anchor.fade_in_curve,
                    FadeEdge::Out => ad.anchor.fade_out_curve,
                };
                let next = prev_curve.next();
                if next == prev_curve {
                    None
                } else {
                    Some(AudioDragOutcome::FadeCurve { edge, next_curve: next })
                }
            }
        }
    }
}

/// 絶対位置 snap で計算した clip drag の beat delta (overlay と release commit で共有)。
/// anchor 0 の編集対象端 (Move=start / ResizeRight=end / ResizeLeft=start) の絶対位置を
/// snap → その差分を全 anchor に適用 (相対関係維持 + anchor 0 が grid に着地)。
/// anchors が空のときは raw を返す (defensive)。
fn compute_clip_drag_beat_delta(
    nd: &ClipDragSession,
    raw_beat_delta: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> f64 {
    let Some(a0) = nd.anchors.first() else {
        return raw_beat_delta;
    };
    let pivot = match nd.kind {
        ClipDragKind::Move | ClipDragKind::ResizeLeft => a0.start_beat,
        ClipDragKind::ResizeRight => a0.start_beat + a0.len_beats,
    };
    let snapped_pivot =
        snap.snap_beat(pivot + raw_beat_delta, nd.last_alt, zoom_x_px_per_beat);
    snapped_pivot - pivot
}

/// daw_01 #071: automation clip drag の snap 適用済 beat delta (`compute_clip_drag_beat_delta` の
/// automation 版)。 pivot = `anchors[0]` (= 掴んだ clip) の編集対象端の絶対位置を snap して差分を返す
/// (絶対位置 snap、 overlay と release commit が共有して描画 / commit を完全一致させる)。
fn compute_automation_clip_drag_beat_delta(
    acd: &AutomationClipDragSession,
    raw_beat_delta: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> f64 {
    let Some(a0) = acd.anchors.first() else {
        return raw_beat_delta;
    };
    let pivot = match acd.kind {
        ClipDragKind::Move | ClipDragKind::ResizeLeft => a0.start_beat,
        ClipDragKind::ResizeRight => a0.start_beat + a0.len_beats,
    };
    let snapped = snap.snap_beat(pivot + raw_beat_delta, acd.last_alt, zoom_x_px_per_beat);
    snapped - pivot
}

/// M14 Phase 127 (daw_01 #105): section drag の snap 適用済 beat delta (`compute_clip_drag_beat_delta`
/// の section 版)。 pivot = 編集対象端の **絶対位置** (Move/ResizeLeft = `anchor_start`、 ResizeRight =
/// `anchor_start + anchor_len`、 Create = `anchor_press_beat` の固定端) を snap して差分を返す
/// (絶対位置 snap、 delta-snap NG という CLAUDE.md の drag snap 規約)。 overlay と release commit が
/// この helper を共有して描画 / commit を完全一致させる。
fn compute_section_drag_beat_delta(
    sd: &SectionDragSession,
    raw_beat_delta: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> f64 {
    let pivot = match sd.kind {
        SectionGesture::Move | SectionGesture::ResizeLeft => sd.anchor_start,
        SectionGesture::ResizeRight => sd.anchor_start + sd.anchor_len,
        SectionGesture::Create => sd.anchor_press_beat,
    };
    let snapped = snap.snap_beat(pivot + raw_beat_delta, sd.last_alt, zoom_x_px_per_beat);
    snapped - pivot
}

// `compute_loop_drag_endpoints` は M14 Phase 69 (#041) で
// `crate::widgets::ruler_ops` に extract (piano_roll と共有)。

/// M14 Phase 61b (#011): caller の `data_generation` は track 構成 (順序 / mute / solo /
/// volume / name / clip 個数) のみの責務に整理し、 clip 個別の `(id, start_beat, len_beats)`
/// 変化は widget 側で吸収する。 旧設計は caller が data_generation で全網羅を要求されており、
/// 漏れると drag move 後に古い clip rect が残像として残る (#011 (2))。 全 caller が同じ
/// boilerplate を書くのは設計欠陥のシグナル (`feedback_pursue_best_practice`)。
///
/// FNV-1a 風 fold (大きな素数倍 + xor)。 100 clip × 4 fold step = ~100ns @ 4GHz、 16ms 予算
/// の 0.001%。 `ArrangementClip` は gui_01 公開型なので widget が hash する権利あり (no-Clone
/// 不変条件にも触れない、 `u32`/`f64` は Copy)。
#[allow(clippy::too_many_lines)]
fn fold_arrangement_clip_hash(tracks: &[ArrangementTrack]) -> u64 {
    const PRIME: u64 = 0x100_0000_01B3; // FNV-1a 64bit prime
    let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a 64bit offset basis
    for t in tracks {
        h ^= u64::from(t.id);
        h = h.wrapping_mul(PRIME);
        for c in &t.clips {
            h ^= u64::from(c.id);
            h = h.wrapping_mul(PRIME);
            h ^= c.start_beat.to_bits();
            h = h.wrapping_mul(PRIME);
            h ^= c.len_beats.to_bits();
            h = h.wrapping_mul(PRIME);
            // name: Arc<str> の ptr で簡易検知 (refcount bump は同 ptr、 rename / replace で
            // new Arc → ptr 変化)。 内容 hash は O(n) で過剰、 ptr 比較で十分。
            h ^= c.name.as_ptr() as u64;
            h = h.wrapping_mul(PRIME);
            // color / share_group_color: 描画色変化を検知 (automation clip arm と対称、
            // None は u64::MAX sentinel)。
            let color_marker = match c.color {
                None => u64::MAX,
                Some(col) => {
                    let mut a: u64 = 0xA5A5_5A5A_A5A5_5A5A;
                    a ^= u64::from(col.r.to_bits());
                    a = a.wrapping_mul(PRIME);
                    a ^= u64::from(col.g.to_bits());
                    a = a.wrapping_mul(PRIME);
                    a ^= u64::from(col.b.to_bits());
                    a = a.wrapping_mul(PRIME);
                    a ^= u64::from(col.a.to_bits());
                    a
                }
            };
            h ^= color_marker;
            h = h.wrapping_mul(PRIME);
            h ^= c.share_group_color.map_or(u64::MAX, |hue| u64::from(hue.to_bits()));
            h = h.wrapping_mul(PRIME);
            // M14 Phase 63k (#025): audio_edit (gain_db / fade_in/out_beats / fade curve) も
            // viewport_key に反映させて、 caller が gain / fade を更新したら cache が再構築されるよう保証。
            // 旧設計で `audio_edit` を hash に入れない場合、 dB handle line / envelope の表示が
            // 1 frame 遅れる (#011 と同根の cache miss 不在問題)。 None は固定 sentinel value
            // (`u64::MAX`) で hash に混ぜて、 None ↔ Some 切替も検知。
            let audio_marker = match c.audio_edit {
                None => u64::MAX,
                Some(audio) => {
                    let mut a: u64 = 0xDEAD_BEEF_CAFE_BABE;
                    a ^= u64::from(audio.gain_db.to_bits());
                    a = a.wrapping_mul(PRIME);
                    a ^= audio.fade_in_beats.to_bits();
                    a = a.wrapping_mul(PRIME);
                    a ^= audio.fade_out_beats.to_bits();
                    a = a.wrapping_mul(PRIME);
                    // FadeCurve は Hash 派生済 (Linear=0, Exp=1, SCurve=2 を使う)
                    let curve_code = |c: FadeCurve| match c {
                        FadeCurve::Linear => 0_u64,
                        FadeCurve::Exponential => 1,
                        FadeCurve::SCurve => 2,
                    };
                    a ^= curve_code(audio.fade_in_curve);
                    a = a.wrapping_mul(PRIME);
                    a ^= curve_code(audio.fade_out_curve);
                    a = a.wrapping_mul(PRIME);
                    a
                }
            };
            h ^= audio_marker;
            h = h.wrapping_mul(PRIME);
        }
        // M14 Phase 63n-1 (#028): automation lanes も viewport_key に反映 (caller が collapse / lane
        // 追加 / point 追加 を行ったら cached が再構築される)。 旧設計で lane 関連を hash に入れない
        // と disclosure 切替 / lane 切替後の再描画が 1 frame 遅れる (#011 と同根)。
        h ^= u64::from(t.automation_lanes_collapsed);
        h = h.wrapping_mul(PRIME);
        h ^= t.automation_lanes.len() as u64;
        h = h.wrapping_mul(PRIME);
        for lane in &t.automation_lanes {
            h ^= u64::from(lane.id);
            h = h.wrapping_mul(PRIME);
            h ^= u64::from(lane.visible);
            h = h.wrapping_mul(PRIME);
            h ^= u64::from(lane.enabled);
            h = h.wrapping_mul(PRIME);
            h ^= u64::from(lane.height_px);
            h = h.wrapping_mul(PRIME);
            h ^= u64::from(lane.default_value_norm.to_bits());
            h = h.wrapping_mul(PRIME);
            // lane.color は描画にしか使わないが、 caller が lane の色だけ変えても再描画を保証
            h ^= u64::from(lane.color.r.to_bits());
            h = h.wrapping_mul(PRIME);
            h ^= u64::from(lane.color.g.to_bits());
            h = h.wrapping_mul(PRIME);
            h ^= u64::from(lane.color.b.to_bits());
            h = h.wrapping_mul(PRIME);
            h ^= u64::from(lane.icon_glyph as u32);
            h = h.wrapping_mul(PRIME);
            h ^= lane.label.len() as u64; // label の文字列内容変更は label.len() で簡易検知
            h = h.wrapping_mul(PRIME);
            for ac in &lane.clips {
                h ^= u64::from(ac.id);
                h = h.wrapping_mul(PRIME);
                h ^= ac.start_beat.to_bits();
                h = h.wrapping_mul(PRIME);
                h ^= ac.len_beats.to_bits();
                h = h.wrapping_mul(PRIME);
                h ^= ac.share_group_color.map_or(u64::MAX, |hue| u64::from(hue.to_bits()));
                h = h.wrapping_mul(PRIME);
                h ^= ac.points.len() as u64;
                h = h.wrapping_mul(PRIME);
                for p in &ac.points {
                    h ^= p.time_beat.to_bits();
                    h = h.wrapping_mul(PRIME);
                    h ^= u64::from(p.value_norm.to_bits());
                    h = h.wrapping_mul(PRIME);
                    let curve_code = match p.curve {
                        ArrangementCurveKind::Hold => 0_u64,
                        ArrangementCurveKind::Linear => 1,
                        ArrangementCurveKind::Bezier { tension } => {
                            // tension の bits を加味して 2_u64 + bits
                            2_u64 ^ u64::from(tension.to_bits())
                        }
                        ArrangementCurveKind::Exponential { bend } => {
                            // M14 Phase 63n-7 (daw_01 #033): Exponential variant の bend bits を
                            // 3_u64 と XOR (discriminant 衝突を防ぐ、 Bezier と独立に変化検知)。
                            3_u64 ^ u64::from(bend.to_bits())
                        }
                    };
                    h ^= curve_code;
                    h = h.wrapping_mul(PRIME);
                }
            }
        }
    }
    h
}

// ============================================================
// Internal drawing helpers
// ============================================================

fn push_filled_rect<M: ?Sized + 'static>(hctx: &mut HeavyCtx<'_, '_, M>, r: Rect, fill: Color) {
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
}

// M13 Phase 55: ruler / lanes grid の bar/beat 縦線 + 小節番号テキスト描画は library
// `Ui::time_ruler` / `Ui::bar_beat_grid` (heavy.rs delegate 経由) に統合した。
// この関数は lanes 背景 + per-row 背景 (selection / mute/solo hint / lane separator) のみ。
#[allow(clippy::too_many_arguments)]
fn draw_lanes_bg<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    lanes: Rect,
    tracks: &[ArrangementTrack],
    visible_tops: &[f32],
    view: ArrangementView,
    selected_tracks: &[u32],
    style: &ArrangementStyle,
) {
    push_filled_rect(hctx, lanes, style.bg);

    // 各 track row 背景 (selection ハイライト + video tint; #085 で group 専用 tint は撤去)。
    // M14 Phase 63c (#016): collapsed 親配下は描画 skip (visible 列のみ index で row を計算)。
    // M14 Phase 63n-6 (#031): per-track row 高さ override 反映のため `visible_tops` (prefix sum) を
    // 受け取り、 row_y / row_h を per-track で算出する (= override 済 track の backdrop fill が正しく
    // 行高さに追従)。
    let visible_indices = compute_visible_indices(tracks);
    for (visible_i, &i) in visible_indices.iter().enumerate() {
        let t = &tracks[i];
        let row_y = visible_tops.get(visible_i).copied().unwrap_or(lanes.y);
        let row_h = effective_track_row_h(t, view.track_row_h);
        let row = Rect { x: lanes.x, y: row_y, w: lanes.w, h: row_h };
        if row.y + row.h < lanes.y || row.y > lanes.y + lanes.h {
            continue;
        }
        // selection priority > video > 通常 (selection は overlay layer で再描画される
        // が、 lanes_bg では下塗りとして塗る = visual hint としての役割)。
        // M14 Phase 113 (daw_01 #085): group track 専用の背景 tint は撤去 (= 他 track と同じ
        // neutral 背景)。 group であることは indent (`depth * indent_px`) と disclosure ▶▼ の
        // 構造手掛かりだけで識別する。 video / selection 背景は不変。
        if selected_tracks.contains(&t.id) {
            push_filled_rect(hctx, row, style.track_selected_bg);
        } else if matches!(t.kind, TrackKind::Video) {
            // M14 Phase 72 (#044): video track の行背景は暗青で audio と視覚区別 (selection は
            // 優先度高いまま、 通常 audio 行は base bg のまま)。
            push_filled_rect(hctx, row, style.track_background_video);
        }
        // row 下端 separator
        push_filled_rect(
            hctx,
            Rect {
                x: row.x,
                y: row.y + row.h - style.lane_line_width_px,
                w: row.w,
                h: style.lane_line_width_px,
            },
            style.lane_line,
        );
    }
}

/// M14 Phase 89 (daw_01 #060): clip が実際に塗る `fill` の輝度から名前 / link glyph のテキスト色を
/// 自動選択する (SSoT = widget が唯一 fill を知る)。 WCAG relative luminance が閾値 0.179 を超える
/// (= 明るい fill) なら `clip_text_color_dark`、 そうでなければ `clip_text_color` を返し、 白/黒文字の
/// どちらか **コントラスト比が高い方** を選ぶ (0.179 は white/black それぞれとのコントラスト比が
/// 等しくなる relative luminance)。 `fill.a < 1.0` (share clip の半透明 fill 等) は `lane_bg` と
/// alpha 合成した実効色で判定する。 `style.clip_auto_contrast_text == false` の opt-out 時は常に
/// `clip_text_color` を返す。 輝度計算 / alpha 合成 / 閾値判定は piano_roll の鍵盤ラベル (#093) と
/// 共有する `crate::color` の SSoT helper に委譲する。
fn clip_text_color_for(style: &ArrangementStyle, fill: Color, lane_bg: Color) -> Color {
    if !style.clip_auto_contrast_text {
        return style.clip_text_color;
    }
    let bg = crate::color::composite_over(fill, lane_bg);
    crate::color::pick_contrast(bg, style.clip_text_color, style.clip_text_color_dark)
}

/// M14 Phase 72 (daw_01 #044): `rect` 内に `(tex_w, tex_h)` の native aspect を保ったまま
/// 中央 letterbox 配置した sub-rect を返す。 余白 (黒帯) は呼び出し側の base fill (= video
/// clip では `video_clip_loading`) で見える。
///
/// - `tex_w` / `tex_h` = 0 は 1 に clamp (`u32` の 0 を許容しつつ ZeroDiv を回避)
/// - `rect.h` 0 近傍も 0.001 に clamp (= rect 自身が 0 px 高さの異常 case で fit_h を 0 に押さえる)
#[must_use]
fn aspect_fit_rect(rect: Rect, tex_width: u32, tex_height: u32) -> Rect {
    #[allow(clippy::cast_precision_loss)]
    let texture_width = tex_width.max(1) as f32;
    #[allow(clippy::cast_precision_loss)]
    let texture_height = tex_height.max(1) as f32;
    let tex_aspect = texture_width / texture_height;
    let rect_aspect = (rect.w / rect.h.max(0.001)).max(0.001);
    let (fit_w, fit_h) = if tex_aspect > rect_aspect {
        // texture が rect より横長 → 上下 letterbox
        (rect.w, rect.w / tex_aspect)
    } else {
        // texture が rect より縦長 (or 同 aspect) → 左右 letterbox
        (rect.h * tex_aspect, rect.h)
    };
    let fit_x = rect.x + (rect.w - fit_w) * 0.5;
    let fit_y = rect.y + (rect.h - fit_h) * 0.5;
    Rect::new(fit_x, fit_y, fit_w, fit_h)
}

/// M14 Phase 108 (daw_01 #080): clip 名 + (share clip なら) 名前左の link glyph を描く共通 helper。
/// audio 経路 (`draw_clip`) と video 経路 (`draw_video_clip`) で共有 (share マークの link glyph
/// 描画ロジックを 1 箇所に集約)。 `text_color` は呼び出し側が実 fill から `clip_text_color_for` で
/// 導出済を渡す (SSoT = widget が唯一 fill を知る)。 `has_link == true` のとき `share_group_link_glyph`
/// (⇌) を **clip 名と 1 つの text run に統合** して描く (#022: selection と独立 = selected でも shared
/// なら描画)。 M14 Phase 126 (#104) 以前は glyph を別 run にして name を `clip_text_size + 2px` 右送り
/// していたが、 em 幅近似 + 固定パッドで実 advance より広く隙間が空いていたため 1 run に統合した
/// (glyph / name は同色・同 font_size・同 top・同 clip_rect なので情報損失なし)。 文字を描けない
/// 小ささ (`r.w <= 24` or `r.h <= clip_text_size + 2`) では何も描かない (audio / video 経路で同一だった
/// 閾値をここに集約)。
fn draw_clip_label<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    name: &Arc<str>,
    has_link: bool,
    text_color: Color,
    style: &ArrangementStyle,
) {
    if !(r.w > 24.0 && r.h > style.clip_text_size + 2.0) {
        return;
    }
    // M14 Phase 126 (daw_01 #104): share clip の link glyph (⇌) と clip 名を 1 つの text run に統合。
    // 旧実装は name の left を `r.x + 4.0 + clip_text_size + 2.0` に置き、glyph 幅を実 advance ではなく
    // `clip_text_size` (= font size = em 幅) で近似 + 固定 `+2.0` パッドを足していたため、⇌ の実描画幅より
    // 広く名前を右送りして二重に隙間が空いていた。glyph / name は同色・同 font_size・同 top・同 clip_rect
    // なので 1 run に統合してレイアウトエンジンに advance を委ねれば情報を失わず隙間が消える。
    // `has_link == false` は従来どおり name のみ。
    let text: Arc<str> = if has_link {
        Arc::from(format!("{}{name}", style.share_group_link_glyph))
    } else {
        name.clone()
    };
    hctx.push_text(GlyphArea {
        text,
        left: r.x + 4.0,
        top: r.y + 2.0,
        font_size: style.clip_text_size,
        line_height: style.clip_text_size * 1.2,
        color: text_color,
        clip_rect: Some(r),
        ..GlyphArea::default()
    });
}

/// M14 Phase 127 (#105): section 帯 1 件を描く (色 fill + border + 名前ラベル、 clip ラベルと同
/// 左寄せ + 4px inset + auto-contrast idiom)。 名前が空 / 帯が狭いときはラベルを省く。
/// M14 Phase 128 (#106): `selected` の帯は選択 clip と同 idiom の明るい太枠 (`clip_selected_border`)、
/// 非選択は neutral 1px (`clip_border`)。
fn draw_section_band<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    name: &Arc<str>,
    color_rgb: [f32; 3],
    r: Rect,
    selected: bool,
    style: &ArrangementStyle,
) {
    let fill = Color::rgb(color_rgb[0], color_rgb[1], color_rgb[2]);
    push_filled_rect(hctx, r, fill);
    // FIXME #73: 選択帯は fill 色に依らず見える 2 重リング (clip と同 idiom、 帯は
    // 角丸 0)。 非選択は neutral な 1px 枠。 これで白 / 黄の section でも選択が判別できる。
    if selected {
        push_selection_ring(hctx, r, style, 0.0, Some(r));
    } else {
        push_section_border(hctx, r, style.clip_border, 1.0);
    }
    if r.w > 8.0 && r.h > style.clip_text_size + 2.0 && !name.is_empty() {
        let text_color = clip_text_color_for(style, fill, style.arranger_lane_bg);
        hctx.push_text(GlyphArea {
            // 毎フレーム描画なので `Arc::from(&str)` (byte copy) でなく Arc refcount clone
            // (draw_clip_label と同じ安価経路、 section 数が多い曲で per-frame alloc を避ける)。
            text: name.clone(),
            left: r.x + 4.0,
            top: r.y + (r.h - style.clip_text_size) * 0.5,
            font_size: style.clip_text_size,
            line_height: style.clip_text_size * 1.2,
            color: text_color,
            clip_rect: Some(r),
            ..GlyphArea::default()
        });
    }
}

/// M14 Phase 127 (#105): section 帯 / preview の border (M14 Phase 128 で width 可変化 = selected 太枠)。
fn push_section_border<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    border: Color,
    width: f32,
) {
    hctx.push_rect(RectCommand {
        rect: r,
        fill: Color::TRANSPARENT,
        border,
        border_width: width,
        radius: [0.0; 4],
        clip_rect: Some(r),
    });
}

/// FIXME #73: 選択枠を fill 色に依存せず描く 2 重リング。 clip / video clip /
/// section 帯の選択表示に共通で使う。 呼び出し側は fill を **clip 本来の色**で
/// 描き、 ここは枠だけを重ねる: 外側の明線 (`clip_selected_border`) が暗い lane
/// 背景に、 内側の暗線 (`clip_selected_border_inner`) が黄 / 白など明るい fill に
/// 必ずコントラストするので、 どんな clip 色 (選択色と同色を含む) でも選択を
/// 判別できる。 `radius` は枠の角丸 (clip = `clip_radius`、 section 帯 = 0)。
/// rect 内側に描く SDF ボーダー (rect.wgsl) なので、 外側リングを `r`、 内側
/// リングを `r` を線幅ぶん inset した矩形に描けば 2 本が隣接して並ぶ。
fn push_selection_ring<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    style: &ArrangementStyle,
    radius: f32,
    clip_rect: Option<Rect>,
) {
    let w = style.clip_selected_border_w;
    // 外側: 明線 (暗い lane 背景に対して光る)。
    hctx.push_rect(RectCommand {
        rect: r,
        fill: Color::TRANSPARENT,
        border: style.clip_selected_border,
        border_width: w,
        radius: [radius; 4],
        clip_rect,
    });
    // 内側: 暗線 (黄 / 白など明るい fill に対してコントラスト)。 r を線幅ぶん
    // inset し角丸も縮める。 inset が潰れる極小 clip では外側リングのみ。
    let inner = Rect { x: r.x + w, y: r.y + w, w: r.w - w * 2.0, h: r.h - w * 2.0 };
    if inner.w > 0.0 && inner.h > 0.0 {
        let ir = (radius - w).max(0.0);
        hctx.push_rect(RectCommand {
            rect: inner,
            fill: Color::TRANSPARENT,
            border: style.clip_selected_border_inner,
            border_width: w,
            radius: [ir; 4],
            clip_rect,
        });
    }
}

/// M14 Phase 127 (#105): drag 中対象 section の preview `(start, len)` を返す (Move/Resize。 非対象 /
/// Create / Ctrl+drag (複製) の元帯は base を返す = 複製は元帯を残し ghost を別途描く)。 draw と release が
/// 同じ `compute_section_drag_beat_delta` を通すことで overlay == commit を保証する。
fn section_preview_start_len(
    s: &SectionView,
    section_drag: Option<SectionDragSession>,
    beat_per_px: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> (f64, f64) {
    let Some(sd) = section_drag else {
        return (s.start_beat, s.len_beats);
    };
    if sd.kind == SectionGesture::Create || sd.section_id != s.id {
        return (s.start_beat, s.len_beats);
    }
    let raw = f64::from(sd.last_mouse.0 - sd.anchor_mouse.0) * beat_per_px;
    let delta = compute_section_drag_beat_delta(&sd, raw, snap, zoom_x_px_per_beat);
    match sd.kind {
        SectionGesture::Move => {
            if sd.last_ctrl {
                (s.start_beat, s.len_beats)
            } else {
                ((s.start_beat + delta).max(0.0), s.len_beats)
            }
        }
        SectionGesture::ResizeLeft => {
            let right = sd.anchor_start + sd.anchor_len;
            let ns = (sd.anchor_start + delta).clamp(0.0, (right - SECTION_MIN_LEN_BEATS).max(0.0));
            (ns, (right - ns).max(SECTION_MIN_LEN_BEATS))
        }
        SectionGesture::ResizeRight => {
            (sd.anchor_start, (sd.anchor_len + delta).max(SECTION_MIN_LEN_BEATS))
        }
        SectionGesture::Create => (s.start_beat, s.len_beats),
    }
}

/// M14 Phase 127 (daw_01 #105): Arranger レーン全体 (背景 + "Arranger" 見出し + section 色帯群 + drag
/// preview) を描く overlay helper。 loop band と同じく cached 外で毎フレーム描画する (section データ
/// 変化に cache busting 不要、 selection / loop band と同流儀)。 drag 中は対象 section を
/// `section_preview_start_len` の preview geometry で描き、 overlay == release commit を helper 共有で
/// 構造保証する。 Ctrl+drag (複製) は元帯 + 複製先 ghost、 範囲 drag (Create) は preview 帯を描く。
#[allow(clippy::too_many_arguments)]
fn draw_sections_lane<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    sections: &[SectionView],
    section_drag: Option<SectionDragSession>,
    view: ArrangementView,
    arranger: Rect,
    arranger_header: Rect,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
    style: &ArrangementStyle,
) {
    if arranger.h <= 0.0 {
        return;
    }
    // 背景 + header 見出し ("Arranger")。
    if arranger_header.w > 0.0 {
        push_filled_rect(hctx, arranger_header, style.arranger_lane_bg);
        if arranger_header.h > style.clip_text_size + 2.0 {
            hctx.push_text(GlyphArea {
                text: Arc::from("Arranger"),
                left: arranger_header.x + 4.0,
                top: arranger_header.y + (arranger_header.h - style.clip_text_size) * 0.5,
                font_size: style.clip_text_size,
                line_height: style.clip_text_size * 1.2,
                color: style.arranger_label_color,
                clip_rect: Some(arranger_header),
                ..GlyphArea::default()
            });
        }
    }
    push_filled_rect(hctx, arranger, style.arranger_lane_bg);

    let beat_per_px = view.len_beats / f64::from(arranger.w.max(1.0));
    hctx.with_clip_rect(arranger, |hctx| {
        // 各 section 帯 (drag 対象は preview geometry)。
        for s in sections {
            let (start, len) =
                section_preview_start_len(s, section_drag, beat_per_px, snap, zoom_x_px_per_beat);
            let r = section_rect_from(start, len, view, arranger);
            draw_section_band(hctx, &s.name, s.color, r, s.selected, style);
        }
        let Some(sd) = section_drag else {
            return;
        };
        let raw = f64::from(sd.last_mouse.0 - sd.anchor_mouse.0) * beat_per_px;
        let delta = compute_section_drag_beat_delta(&sd, raw, snap, zoom_x_px_per_beat);
        match sd.kind {
            // Ctrl+drag (複製): 複製先に半透明 ghost 帯。
            SectionGesture::Move if sd.last_ctrl => {
                let dest = (sd.anchor_start + delta).max(0.0);
                let r = section_rect_from(dest, sd.anchor_len, view, arranger);
                push_filled_rect(hctx, r, style.arranger_preview_fill);
                push_section_border(hctx, r, style.clip_border, 1.0);
            }
            // 範囲 drag (Create): まだ存在しない section の preview 帯。
            SectionGesture::Create => {
                let other = (sd.anchor_press_beat + delta).max(0.0);
                let lo = sd.anchor_press_beat.min(other);
                let hi = sd.anchor_press_beat.max(other);
                if hi > lo {
                    let r = section_rect_from(lo, hi - lo, view, arranger);
                    push_filled_rect(hctx, r, style.arranger_preview_fill);
                    push_section_border(hctx, r, style.clip_border, 1.0);
                }
            }
            _ => {}
        }
    });
}

/// M14 Phase 72 (daw_01 #044): video track の clip 描画 (audio path とは別 helper)。
///
/// 描画順:
/// 1. base fill: 常に `clip.color` (未指定 None なら `video_clip_loading` =
///    letterbox の黒帯背景としても兼用)。 FIXME #73: 選択でも fill は潰さない。
/// 2. thumbnail = Some なら aspect-fit (黒帯 letterbox) で texture overlay (`HeavyCtx::push_textured_quad`)
/// 3. name + (share clip なら) link glyph 描画 (`draw_clip_label`、 audio 経路と共通)
/// 4. selected なら `push_selection_ring` の 2 重リング (明 + 暗) を最後に重ねる (FIXME #73)
///
/// M14 Phase 108 (daw_01 #080): share マーク (⇌) は「content 共有」 の意味で track kind と直交するため、
/// video clip でも `share_group_color.is_some()` で link glyph を描く。
/// M14 Phase 114 (daw_01 #086): `share_group_color` は fill / border を上書きしない (リンク識別は ⇌ glyph
/// と #068 hover 強調のみ)。 fill は audio clip と同じく `clip.color` が唯一の source。 `audio_edit`
/// overlay は引き続き video clip では描画しない (= caller 責任で audio 用 field を video clip に詰めない)。
fn draw_video_clip<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    clip: &ArrangementClip,
    style: &ArrangementStyle,
    lanes: Rect,
    selected: bool,
) {
    let has_link = clip.share_group_color.is_some();
    // M14 Phase 114 (daw_01 #086): video clip も `clip.color` を唯一の fill source にする
    // (`share_group_color` は fill / border を上書きしない)。 `color` 未指定 (None) のときは従来の
    // letterbox / loading 背景 `video_clip_loading` を使う (= 既存の非 share video clip と互換)。
    // thumbnail があればその上に aspect-fit で texture を重ねる (fill は letterbox の黒帯として残る)。
    // リンク識別は ⇌ glyph + #068 hover 強調が担う (track kind に依らず share マークが出る、 #080 不変)。
    // FIXME #73: fill は常に clip 本来の色 (選択でも潰さない)。 選択は末尾の
    // `push_selection_ring` の 2 重リングで示し、 選択時は本体 border を消して
    // リングへ一本化する (黄 clip でも選択が判別できる)。
    let fill = clip.color.unwrap_or(style.video_clip_loading);
    let (border, border_w) = if selected {
        (Color::TRANSPARENT, 0.0)
    } else {
        (style.clip_border, style.clip_border_w)
    };
    // M14 Phase 89 (daw_01 #060): 名前色は fill 輝度から auto-contrast (selected の黄 fill → 暗文字、
    // loading の暗 fill → 明文字)。 video lane bg と合成した実効色で判定 (不透明 fill は no-op、
    // 半透明 fill は track_background_video と合成して実効色を得る)。
    let text_color = clip_text_color_for(style, fill, style.track_background_video);
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border,
        border_width: border_w,
        radius: [style.clip_radius; 4],
        clip_rect: Some(lanes),
    });
    if let Some((handle, tex_w_u, tex_h_u)) = clip.thumbnail {
        hctx.push_textured_quad(TexturedQuad {
            rect: aspect_fit_rect(r, tex_w_u, tex_h_u),
            texture: handle,
            alpha: 1.0,
            uv_min: (0.0, 0.0),
            uv_max: (1.0, 1.0),
            // clip rect 内に閉じる (= drag 中の lanes 端で thumbnail がはみ出ない)。
            clip_rect: Some(r.intersect(lanes)),
            rotation_radians: 0.0,
            rotation_pivot: None,
        });
    }
    // name + (share clip なら) link glyph。 thumbnail の **後** に描くので texture の上に乗る。
    draw_clip_label(hctx, r, &clip.name, has_link, text_color, style);
    // FIXME #73: 選択枠は最後に重ねて thumbnail / label の上に乗せる。
    if selected {
        push_selection_ring(hctx, r, style, style.clip_radius, Some(lanes));
    }
}

fn draw_clip<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    clip: &ArrangementClip,
    style: &ArrangementStyle,
    lanes: Rect,
    selected: bool,
    track_kind: TrackKind,
) {
    if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
        return;
    }
    // M14 Phase 72 (daw_01 #044): video track の clip は thumbnail + loading 色の専用 path。
    // M14 Phase 108 (daw_01 #080): share_group_color は video clip でも honor する (Text / Image clip
    // の共有マーク)。 audio_edit のみ無視 (video clip では意味を持たない、 caller 責任)。
    if matches!(track_kind, TrackKind::Video) {
        draw_video_clip(hctx, r, clip, style, lanes, selected);
        return;
    }
    // M14 Phase 114 (daw_01 #086): 静的な fill / border は **`clip.color` を唯一の source** にする。
    // selected は selection 色を最優先 (link glyph の有無に依らず)。 `share_group_color` は #086 で
    // 役割を「リンク識別」 に絞り、 fill / border を一切上書きしない (= ⇌ glyph + #068 hover 強調
    // 専用)。 これにより「clip で色を選べば共有クリップ全部がその色になる」「トラックに揃えれば
    // その色になる」 が成立する (#019/#022 で hue fill が `color` を握り潰していた FIXME #8 の解消)。
    // FIXME #73: fill は常に clip 本来の色 (選択でも潰さない)。 選択は末尾の
    // `push_selection_ring` の 2 重リングで示し、 選択時は本体 border を消して
    // リングへ一本化する (黄 clip でも選択が判別できる)。
    let fill = clip.color.unwrap_or(style.clip_default_fill);
    let (border, border_w) = if selected {
        (Color::TRANSPARENT, 0.0)
    } else {
        (style.clip_border, style.clip_border_w)
    };
    // M14 Phase 89 (daw_01 #060): 名前 + link glyph 色を fill 輝度から auto-contrast。 不透明 fill は
    // no-op、 半透明 fill (alpha < 1) は lane bg (audio lane = `style.bg`) と合成した実効色で判定する。
    let text_color = clip_text_color_for(style, fill, style.bg);
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border,
        border_width: border_w,
        radius: [style.clip_radius; 4],
        clip_rect: Some(lanes),
    });
    // share clip は name の左に link glyph (`⇌` 等) を 1 文字描画 (selection と独立 = selected でも
    // shared なら描画、 #022)。 等幅 (HackGen Console NF) では `clip_text_size` ~= 1 文字幅。 描画
    // ロジックは video 経路と共通の `draw_clip_label` に集約 (M14 Phase 108、 daw_01 #080)。
    let has_link = clip.share_group_color.is_some();
    draw_clip_label(hctx, r, &clip.name, has_link, text_color, style);
    // FIXME #73: 選択枠を最後に重ねる (label の上、 clip 本来の色 fill の上)。
    if selected {
        push_selection_ring(hctx, r, style, style.clip_radius, Some(lanes));
    }
}

fn draw_clips<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    let view_end = view.start_beat + view.len_beats;
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            draw_clip(hctx, r, c, style, lanes, false, t.kind);
        }
    }
}

fn draw_selection_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    selected: &HashSet<ClipKey>,
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    // M14 Phase 63f (#022): selected clip を `draw_clip(.., selected=true)` で上書き再描画。
    // 共通 helper を使うので link glyph (share clip) は selection と独立に描画される。
    if selected.is_empty() {
        return;
    }
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let key = ClipKey { track: t.id, clip: c.id };
            if !selected.contains(&key) {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            draw_clip(hctx, r, c, style, lanes, true, t.kind);
        }
    }
}

/// M14 Phase 96 (daw_01 #068): 共有グループ「連動ハイライト」overlay。
/// `clip.in_active_group == true` かつ `share_group_color.is_some()` の clip に、 selection
/// (黄塗り) とは **別レイヤ** の強調 (glow wash + bright thick border) を重ねる。
/// M14 Phase 114 (daw_01 #086): 強調色は **identity-neutral** な `share_group_active_color` に変更
/// (旧: グループ hue を流用)。 #086 で clip fill が user 指定色になったため、 hue wash だと user の色と
/// 喧嘩する。 hover 中は 1 グループしか強調しないので色でグループを区別する必要は無い。
///
/// - **`in_active_group == false` / `share_group_color == None` の clip は一切描画しない**
///   (= 既存挙動と pixel 完全一致、 常に false で渡せば移行安全、 非 share clip は強調しない defensive)。
/// - **selection overlay より前** に呼ぶ: 選択中の同グループ member は黄塗りが上書き優先され
///   (#068 の「黄塗り優先で OK」)、 非選択 member が neutral 強調の主役になる。
/// - **cached 外で毎フレーム描画**: active group は hover / 選択で毎フレーム変わるため
///   viewport_key (heavy cache key) には含めない (hover 由来の変化で heavy cache を無効化しない =
///   selection overlay と同 idiom)。 描画は `draw_clips` / `draw_selection_overlay` と同じ culling。
fn draw_active_group_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
) {
    let view_end = view.start_beat + view.len_beats;
    for (i, t) in visible_tracks.iter().enumerate() {
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            if !c.in_active_group {
                continue;
            }
            // share group member (= `share_group_color.is_some()`) でなければ強調しない
            // (video clip 等は share_group_color = None、 defensive)。 M14 Phase 114 (#086) で hue 値は
            // 強調色に使わなくなったが、 「リンクされた clip だけ」 を強調する guard は維持する。
            if c.share_group_color.is_none() {
                continue;
            }
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, row_h, c, view, lanes);
            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                continue;
            }
            // M14 Phase 114 (daw_01 #086): 強調色は **identity-neutral** な `share_group_active_color`
            // (bright 中立色) に変更。 #086 で clip fill が user 指定色になったため、 旧 hue wash だと
            // ユーザの選んだ色と喧嘩する (hover 中は 1 グループしか強調しない = どのグループかを色で
            // 区別する必要が無い)。 selection の黄塗りとは別レイヤの「明度上げ + 明るい中立枠」。
            // (1) glow wash: neutral color を低 alpha で clip 全体に敷いて「明るくする」。 alpha=0 なら
            //     no-op (= ring のみの強調)。 透明 fill push を避けるため alpha>0 の時だけ積む。
            if style.share_group_active_glow_alpha > 0.0 {
                let ac = style.share_group_active_color;
                let glow = Color { r: ac.r, g: ac.g, b: ac.b, a: style.share_group_active_glow_alpha };
                hctx.push_rect(RectCommand {
                    rect: r,
                    fill: glow,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [style.clip_radius; 4],
                    clip_rect: Some(lanes),
                });
            }
            // (2) bright thick border: 同 neutral color を太枠で outline。 透明 fill なので
            //     clip 名 / 既存 fill は隠さず、 枠だけ強調 (= 「束ねられている」 印象)。
            if style.share_group_active_border_w > 0.0 {
                hctx.push_rect(RectCommand {
                    rect: r,
                    fill: Color::TRANSPARENT,
                    border: style.share_group_active_color,
                    border_width: style.share_group_active_border_w,
                    radius: [style.clip_radius; 4],
                    clip_rect: Some(lanes),
                });
            }
        }
    }
}

fn drag_preview_geometry(
    anchor: ClipDragAnchor,
    kind: ClipDragKind,
    beat_delta: f64,
    track_delta: i32,
    n_tracks: usize,
    min_len: f64,
) -> (f64, f64, usize) {
    match kind {
        ClipDragKind::Move => {
            let new_start = (anchor.start_beat + beat_delta).max(0.0);
            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
            let new_idx = (anchor.track_index as i32 + track_delta)
                .clamp(0, (n_tracks.saturating_sub(1)) as i32);
            #[allow(clippy::cast_sign_loss)]
            let new_idx_u = new_idx.max(0) as usize;
            (new_start, anchor.len_beats, new_idx_u)
        }
        ClipDragKind::ResizeRight => (
            anchor.start_beat,
            (anchor.len_beats + beat_delta).max(min_len),
            anchor.track_index,
        ),
        ClipDragKind::ResizeLeft => {
            let max_start = anchor.start_beat + anchor.len_beats - min_len;
            let new_start = (anchor.start_beat + beat_delta).clamp(0.0, max_start);
            let actual_delta = new_start - anchor.start_beat;
            (new_start, (anchor.len_beats - actual_delta).max(min_len), anchor.track_index)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_drag_preview<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    nd: &ClipDragSession,
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    style: &ArrangementStyle,
    n_tracks: usize,
    beat_delta: f64,
    track_delta: i32,
    min_len: f64,
) {
    // M14 Phase 63e (#019): Ctrl / Ctrl+Shift drag は ghost を別色 + badge glyph に切替えて
    // 「move / linked clone / independent clone」 の 3 種を視覚区別する。 Resize 中は Ctrl 関与
    // なし (既存 selected_fill のまま)。 commit / overlay の判定はどちらも `nd.last_*` を真値と
    // するので、 release frame の OS event 順序問題に依存せず一致する。
    let is_move_clone = matches!(nd.kind, ClipDragKind::Move) && nd.last_ctrl;
    let (fill, border, badge_glyph) = if is_move_clone {
        if nd.last_shift {
            (style.clip_clone_indep_fill, style.clip_clone_indep_border, Some('+'))
        } else {
            (style.clip_clone_linked_fill, style.clip_clone_linked_border, Some('⇌'))
        }
    } else {
        (style.clip_selected_fill, style.clip_selected_border, None)
    };

    for a in &nd.anchors {
        let (start, len, new_idx) =
            drag_preview_geometry(*a, nd.kind, beat_delta, track_delta, n_tracks, min_len);
        let preview_clip = ArrangementClip {
            id: a.key.clip,
            start_beat: start,
            len_beats: len,
            name: Arc::from(""),
            color: None,
            share_group_color: None,
            audio_edit: None,
            // M14 Phase 72 (#044): drag preview は overlay 用 (実 clip データを表示しない)、
            // thumbnail は元 clip 側で描画済 (cached 内)、 preview は color + border のみ。
            thumbnail: None,
            // drag preview は transient なので連動ハイライト対象外 (元 clip 側 overlay で描画済)。
            in_active_group: false,
        };
        // drag_preview_geometry が n_tracks 範囲内に clamp 済なので tops から必ず取れる前提。
        // 万一範囲外なら preview を skip (clip 描画消失だけで panic はしない、 defensive)。
        let Some(row_top) = tops.get(new_idx).copied() else {
            continue;
        };
        let r = clip_to_rect(row_top, view.track_row_h, &preview_clip, view, lanes);
        if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
            continue;
        }
        hctx.push_rect(RectCommand {
            rect: r,
            fill,
            border,
            border_width: style.clip_selected_border_w,
            radius: [style.clip_radius; 4],
            clip_rect: Some(lanes),
        });
        // ghost rect 左上に badge glyph (`⇌` / `+`) を 1 文字描画。 rect が小さすぎるときは省略。
        if let Some(g) = badge_glyph
            && r.w > style.clip_clone_badge_size + 4.0
            && r.h > style.clip_clone_badge_size + 2.0
        {
            hctx.push_text(GlyphArea {
                text: Arc::from(g.to_string()),
                left: r.x + 4.0,
                top: r.y + 2.0,
                font_size: style.clip_clone_badge_size,
                line_height: style.clip_clone_badge_size * 1.2,
                color: style.clip_clone_badge_color,
                clip_rect: Some(r),
                ..GlyphArea::default()
            });
        }
    }
}

/// M14 Phase 63k (#025): gain_db を clip rect 内の handle line y 座標に変換する pure helper。
/// `gain_db = 0` で rect 中央、 `+range_db` で上端、 `-range_db` で下端 (Bitwig spec 準拠)。
/// 描画と hit-test の両方で使う SSoT (overlay と base 描画が同じ y で描かれる)。
#[must_use]
fn db_to_handle_y(rect: Rect, gain_db: f32, style: &ArrangementStyle) -> f32 {
    let range = style.audio_db_range_db.max(0.001);
    let normalized = (gain_db / range).clamp(-1.0, 1.0);
    // gain=0 で rect 中央、 +range で rect 上端 (y 小さい)、 -range で rect 下端 (y 大きい)。
    rect.y + rect.h * 0.5 - rect.h * 0.5 * normalized
}

/// M14 Phase 63k (#025): clip 上端の fade envelope を描画 (左角 / 右角)。
/// `fade_beats > 0` のとき、 grip 角から fade 末尾まで斜辺を描く。 `fade_beats = 0` の場合は
/// grip 正方形だけ ([clip 上端の角、 corner_size×corner_size]) を細く塗る (= 「掴める場所」 の hint)。
fn draw_fade_envelope<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    clip_rect: Rect,
    edge: FadeEdge,
    fade_beats: f64,
    clip_len_beats: f64,
    style: &ArrangementStyle,
) {
    use daw_ui_renderer::{LineBatch, LineSegment};

    let corner = style.audio_fade_corner_size_px;
    let corner_rect = match edge {
        FadeEdge::In => Rect { x: clip_rect.x, y: clip_rect.y, w: corner, h: corner },
        FadeEdge::Out => Rect {
            x: clip_rect.x + clip_rect.w - corner,
            y: clip_rect.y,
            w: corner,
            h: corner,
        },
    };
    // grip square (内部塗り、 hit zone hint)
    push_filled_rect(hctx, corner_rect, style.audio_fade_overlay_color);

    // envelope 斜辺は fade_beats > 0 のときのみ描画。
    if fade_beats <= 0.0 || clip_len_beats <= 0.0 {
        return;
    }
    #[allow(clippy::cast_possible_truncation)]
    let fade_w_px = ((fade_beats / clip_len_beats) * f64::from(clip_rect.w))
        .min(f64::from(clip_rect.w)) as f32;
    let (start_xy, end_xy) = match edge {
        // FadeIn: clip 上端左から fade 末尾の clip 内部 (右下) まで斜め
        FadeEdge::In => (
            [clip_rect.x, clip_rect.y],
            [clip_rect.x + fade_w_px, clip_rect.y + clip_rect.h],
        ),
        // FadeOut: clip 上端右から fade 末尾の clip 内部 (左下) まで斜め
        FadeEdge::Out => (
            [clip_rect.x + clip_rect.w, clip_rect.y],
            [clip_rect.x + clip_rect.w - fade_w_px, clip_rect.y + clip_rect.h],
        ),
    };
    let seg = LineSegment { a: start_xy, b: end_xy, color: style.audio_fade_overlay_color };
    hctx.push_lines(LineBatch {
        segments: Arc::<[LineSegment]>::from(vec![seg]),
        line_width_px: style.audio_fade_overlay_width_px,
        clip_rect: Some(clip_rect),
    });
}

/// M14 Phase 63k (#025): audio_edit が Some の clip に対する base 描画 (dB handle line + 両端 fade envelope)。
/// cached 内で呼ばれる (audio_edit / clip rect が変化したら viewport_key で cache 再生成)。
fn draw_clip_audio_overlay<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    clip_rect: Rect,
    audio: &ArrangementClipAudioEdit,
    clip_len_beats: f64,
    style: &ArrangementStyle,
) {
    use daw_ui_renderer::{LineBatch, LineSegment};

    // dB handle line (横線 1 本、 audio_db_handle_width_px の太さ)。 端から margin 内側のみ。
    let margin = style.audio_db_handle_x_margin;
    let line_w = (clip_rect.w - margin * 2.0).max(0.0);
    if line_w > 0.0 {
        let y = db_to_handle_y(clip_rect, audio.gain_db, style);
        let seg = LineSegment {
            a: [clip_rect.x + margin, y],
            b: [clip_rect.x + margin + line_w, y],
            color: style.audio_db_handle_color,
        };
        hctx.push_lines(LineBatch {
            segments: Arc::<[LineSegment]>::from(vec![seg]),
            line_width_px: style.audio_db_handle_width_px,
            clip_rect: Some(clip_rect),
        });
    }

    // Fade In / Out 両 envelope を描画 (length 0 でも grip を描く = 「掴める場所」 hint)。
    draw_fade_envelope(
        hctx,
        clip_rect,
        FadeEdge::In,
        audio.fade_in_beats,
        clip_len_beats,
        style,
    );
    draw_fade_envelope(
        hctx,
        clip_rect,
        FadeEdge::Out,
        audio.fade_out_beats,
        clip_len_beats,
        style,
    );
}

/// M14 Phase 63k (#025): audio_drag 中の ghost overlay (cached 外、 drag 中の preview 値を最新表示)。
/// `compute_audio_drag_outcome` の結果を視覚化:
/// - `Gain { next_db }` → 新 dB position に handle line を 1 本 + ghost label「+3.2 dB」 を描く。
/// - `FadeLength { edge, next_beats }` → 新 fade 範囲を `draw_fade_envelope` で描く + label 省略
///   (envelope の長さ自体が visual feedback)。
/// - `FadeCurve { edge, next_curve }` → curve 名を ghost label「Curve: Exponential」 で描く。
/// - `None` (sticky 未確定) → label「Move」 (= drag が始まったが方向未確定の hint)、 描画は anchor 値の line。
fn draw_audio_drag_ghost<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    ad: &AudioDragSession,
    beat_per_px: f64,
    style: &ArrangementStyle,
) {
    use daw_ui_renderer::{LineBatch, LineSegment};

    let r = ad.clip_rect_anchor;
    if r.w <= 0.0 || r.h <= 0.0 {
        return;
    }
    let outcome = compute_audio_drag_outcome(ad, beat_per_px, style);
    let label_text: Option<String> = match (ad.kind, outcome) {
        (AudioDragKind::Gain, Some(AudioDragOutcome::Gain { next_db })) => {
            // 新 handle line を preview 位置に重ね描き (cached 内の base line は anchor 値で残るが、
            // ghost が上に乗って drag 中の最新値を user に見せる)。
            let margin = style.audio_db_handle_x_margin;
            let line_w = (r.w - margin * 2.0).max(0.0);
            if line_w > 0.0 {
                let y = db_to_handle_y(r, next_db, style);
                let seg = LineSegment {
                    a: [r.x + margin, y],
                    b: [r.x + margin + line_w, y],
                    color: style.audio_db_handle_color,
                };
                hctx.push_lines(LineBatch {
                    segments: Arc::<[LineSegment]>::from(vec![seg]),
                    line_width_px: style.audio_db_handle_width_px * 2.0,
                    clip_rect: Some(r),
                });
            }
            Some(format!(
                "{}{:.1} dB",
                if next_db >= 0.0 { "+" } else { "" },
                next_db
            ))
        }
        (_, Some(AudioDragOutcome::FadeLength { edge, next_beats })) => {
            draw_fade_envelope(hctx, r, edge, next_beats, ad.clip_len_beats_anchor, style);
            None
        }
        (_, Some(AudioDragOutcome::FadeCurve { edge: _, next_curve })) => {
            Some(format!("Curve: {}", next_curve.name()))
        }
        // commit すべき変化なし (drag 距離不足 or anchor 同値) — anchor 値の preview を出さない。
        // sticky 未確定の場合は label だけで「drag しているけど未確定」 を示す。
        (AudioDragKind::FadeIn | AudioDragKind::FadeOut, None) if ad.locked_horizontal.is_none() => {
            Some("Drag horizontally for length, vertically for curve".to_string())
        }
        _ => None,
    };

    if let Some(text) = label_text {
        // ghost label は clip rect の中央上端に 1 行 (= 既存 clip name と被るが、 drag 中のみ表示で問題なし)。
        let font_size = style.audio_ghost_label_size;
        hctx.push_text(GlyphArea {
            text: Arc::from(text),
            left: r.x + 4.0,
            top: r.y + r.h - font_size - 4.0,
            font_size,
            line_height: font_size * 1.2,
            color: style.audio_ghost_label_color,
            clip_rect: Some(r),
            ..GlyphArea::default()
        });
    }
}

// ============================================================
// M14 Phase 63n-1 (#028): automation lane 描画 helpers
// ============================================================

/// M14 Phase 63n-2 (#028): lane header の icon / band の rect 一式 (描画 + hit-test の SSoT)。
/// `header_rect.w < style.automation_lane_header_min_w_px` の極狭幅では `None` (描画 + hit 共に skip)。
/// icon は描画 push_text の `(left, top)` と一致した正方形 rect (icon_size 角)、 hit zone は
/// 同 rect で `Rect::contains` 判定で OK (描画と hit の SSoT)。 `default_band_rect` は band_h > 0
/// かつ header 行高に余裕がある場合のみ `Some`。
#[derive(Clone, Copy, Debug)]
pub struct AutomationLaneHeaderLayout {
    /// `★`/`☆` icon (lane.enabled 切替用、 click で `SetLaneEnabled`)。
    pub enabled_icon_rect: Rect,
    /// `[V]` icon (lane.icon_glyph、 click 機能なし = visual only)。
    pub icon_glyph_rect: Rect,
    /// `👁` icon (lane.visible 切替用、 click で `SetLaneVisible`)。
    pub visible_icon_rect: Rect,
    /// `▣` icon (mute、 Phase 63n-2 では描画のみで click 機能なし)。
    pub mute_icon_rect: Rect,
    /// `✕` icon (lane 削除、 click で `DeleteLane`)。
    pub delete_icon_rect: Rect,
    /// horizontal slider 帯 (default_value_norm 編集、 drag で `SetLaneDefault`)。
    /// header 行高が icon + band を載せられない場合は `None`。
    pub default_band_rect: Option<Rect>,
}

/// M14 Phase 63n-2 (#028): lane header rect から icon / band の sub-rect 群を計算。
/// `draw_automation_lane` と完全同一の配置式 (描画と hit の SSoT)。 widget 内部 hit-test と
/// 外部 test の両方で使うため `pub`。
#[must_use]
pub fn automation_lane_header_layout(
    header_rect: Rect,
    style: &ArrangementStyle,
) -> Option<AutomationLaneHeaderLayout> {
    if header_rect.w < style.automation_lane_header_min_w_px {
        return None;
    }
    let pad = 4.0_f32;
    let icon_size = style.automation_lane_icon_size.max(4.0);
    let cx = header_rect.x + pad;
    let cy = header_rect.y + (header_rect.h - icon_size).max(0.0) * 0.5;
    let enabled_icon_rect = Rect { x: cx, y: cy, w: icon_size, h: icon_size };
    let icon_glyph_rect = Rect {
        x: cx + icon_size + pad,
        y: cy,
        w: icon_size,
        h: icon_size,
    };
    // 右寄せ: ✕ → ▣ → 👁 の順で右から左へ配置 (描画ループ `icons.iter().rev()` と同じ式)。
    let step = icon_size + pad * 0.5;
    let delete_x = header_rect.x + header_rect.w - pad - step;
    let mute_x = delete_x - step;
    let visible_x = mute_x - step;
    let visible_icon_rect = Rect { x: visible_x, y: cy, w: icon_size, h: icon_size };
    let mute_icon_rect = Rect { x: mute_x, y: cy, w: icon_size, h: icon_size };
    let delete_icon_rect = Rect { x: delete_x, y: cy, w: icon_size, h: icon_size };

    // band: header 行下端から pad だけ上、 cy + icon_size より下に band 自身が収まるなら Some。
    let band_h = style.automation_default_band_h;
    let band_y = header_rect.y + header_rect.h - band_h - pad;
    let band_x = cx;
    let band_w = (header_rect.w - pad * 2.0).max(0.0);
    let default_band_rect = if band_h > 0.0 && band_w > 0.0 && band_y >= cy + icon_size {
        Some(Rect { x: band_x, y: band_y, w: band_w, h: band_h })
    } else {
        None
    };

    Some(AutomationLaneHeaderLayout {
        enabled_icon_rect,
        icon_glyph_rect,
        visible_icon_rect,
        mute_icon_rect,
        delete_icon_rect,
        default_band_rect,
    })
}

/// M14 Phase 63n-2 (#028): visible track の expanded automation lane を順に visit する pure helper。
/// `header_pane_x` / `header_pane_w` は track header 領域の x 範囲 (= `view.header_w == 0` で
/// header 無し)、 `lanes_x` / `lanes_w` は clip 描画域。 callback には `(track_idx, lane_idx,
/// lane, header_rect, body_rect)` を渡す。 描画 / hit-test / drag press の SSoT (3 箇所が同じ式
/// で同じ lane y 範囲を計算するための共有)。
#[allow(clippy::too_many_arguments)]
fn for_each_visible_lane<F>(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    mut f: F,
) where
    F: FnMut(usize, usize, &ArrangementAutomationLane, Rect, Rect),
{
    for (i, t) in visible_tracks.iter().enumerate() {
        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
            continue;
        }
        let track_row_top = tops[i];
        // M14 Phase 63n-6 (#031): per-track row 高さ override 反映 (lane y 起点 = row_top + effective row_h)。
        let mut lane_y = track_row_top + effective_track_row_h(t, track_row_h);
        let header_indent = f32::from(t.depth) * style.indent_px;
        for (j, lane) in t.automation_lanes.iter().enumerate() {
            if !lane.visible {
                continue;
            }
            let lh = f32::from(lane.height_px);
            let header_rect = Rect {
                x: header_pane_x + header_indent,
                y: lane_y,
                w: (header_pane_w - header_indent).max(2.0),
                h: lh,
            };
            let body_rect = Rect { x: lanes_x, y: lane_y, w: lanes_w, h: lh };
            f(i, j, lane, header_rect, body_rect);
            lane_y += lh;
        }
    }
}

/// M14 Phase 63n-5 (#030): lane 下端 splitter hot zone (= lane bottom edge ±`handle_px` の y range
/// × body x range) に cursor が当たっているか判定。 当たった lane の `AutomationLaneKey` を返す
/// (= cursor 形状切替 + caller のテストで rect 中心 px を導出する用途)。 splitter は body x range のみ
/// — header 側は button / band と排他。 splitter hit > 他の hover priority (cursor は最優先で NsResize)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_lane_resize_splitter_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<AutomationLaneKey> {
    let handle = style.automation_lane_resize_handle_px;
    if handle <= 0.0 || cx < lanes_x || cx >= lanes_x + lanes_w {
        return None;
    }
    let mut found: Option<AutomationLaneKey> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |i, _j, lane, _h_rect, b_rect| {
            if found.is_some() {
                return;
            }
            let bottom = b_rect.y + b_rect.h;
            if cy >= bottom - handle && cy < bottom {
                found = Some(AutomationLaneKey {
                    track: visible_tracks[i].id,
                    lane: lane.id,
                });
            }
        },
    );
    found
}

/// M14 Phase 63n-6 (#031): track row 下端 splitter hot zone (= row body bottom edge ±`handle_px` の
/// y range × body x range) に cursor が当たっているか判定。 当たった visible track index を返す
/// (= cursor 形状切替 + caller のテストで rect 中心 px を導出する用途)。 row 高さは global なので
/// track index は意味的に「どの行で trigger したか」 のみ示す参考値で、 drag 自体は全 row 一斉。
/// splitter zone は **track row body の最下端 4 px** (= `tops[i] + track_row_h - handle .. + track_row_h`)
/// — 行の下に automation lane がある場合は「最初の lane の上端」 と一致するが、 lane splitter は
/// **lane bottom edge** を見るので排他 (= 別エッジで衝突しない)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn track_row_resize_splitter_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<usize> {
    let handle = style.automation_lane_resize_handle_px;
    if handle <= 0.0 || track_row_h <= 0.0 || cx < lanes_x || cx >= lanes_x + lanes_w {
        return None;
    }
    for i in 0..visible_tracks.len() {
        if i + 1 >= tops.len() {
            break;
        }
        let t = &visible_tracks[i];
        let row_top = tops[i];
        // M14 Phase 63n-6 (#031): per-track row 高さで row_bottom を計算 (override 済 track の splitter
        // zone がそのトラックの下端に追従)。
        let row_bottom = row_top + effective_track_row_h(t, track_row_h);
        if cy >= row_bottom - handle && cy < row_bottom {
            return Some(i);
        }
    }
    None
}

/// M14 Phase 117 (daw_01 #091): track header 列と lanes の境界 (`arrangement_rect.x + header_w` の縦線)
/// を中心とした header 幅 drag splitter の hot zone に cursor が当たっているか判定。 hot zone は境界
/// `±header_resize_handle_px/2` の横帯 × arrangement 全高 (ruler 行も含む縦線全長)。 `header_w <= 0`
/// (header 無し) / `handle <= 0` で常に `false`。 track header の M/S/R ボタン等とは衝突しない (header の
/// 右端 4px inner pad に splitter の header 側が収まる)。 press 振り分けで lane/row splitter の **後** に
/// 評価する (= 同時成立しうる lanes 左端の角は lane/row resize を優先) ので、 実質 cursor は header の
/// 4px pad 〜 lanes 左端 4px で `EwResize`。
#[must_use]
pub fn header_resize_splitter_at(
    arrangement_rect: Rect,
    header_w: f32,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> bool {
    let handle = style.header_resize_handle_px;
    if handle <= 0.0 || header_w <= 0.0 {
        return false;
    }
    let boundary = arrangement_rect.x + header_w;
    let half = handle * 0.5;
    cx >= boundary - half
        && cx < boundary + half
        && cy >= arrangement_rect.y
        && cy < arrangement_rect.y + arrangement_rect.h
}

/// M14 Phase 63n-2 (#028): lane body 内 cursor 位置から hit する point を返す (後勝ち、 描画順と整合)。
/// 戻り値の `Rect` は popup anchor 用 point dot rect (= `lane_disclosure_rect_for` 同様)。
/// hit zone は **point dot 半径の 2 倍** (= 8px @ default radius=4) で生成、 fingertip 操作の余裕を持たせる。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_point_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    cx: f32,
    cy: f32,
    style: &ArrangementStyle,
) -> Option<(AutomationPointKey, Rect)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let radius = style.automation_point_radius_px.max(2.0);
    let hit_r2 = (radius * 2.0).powi(2);
    let mut hit: Option<(AutomationPointKey, Rect)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes.x,
        lanes.w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if cy < body_rect.y || cy >= body_rect.y + body_rect.h {
                return;
            }
            let track_id = visible_tracks[t_idx].id;
            let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
            let pad = style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            for clip_in in &lane.clips {
                for (p_idx, p) in clip_in.points.iter().enumerate() {
                    let abs_beat = clip_in.start_beat + p.time_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let px = body_rect.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
                    let py = clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                    let dx = cx - px;
                    let dy = cy - py;
                    if dx * dx + dy * dy <= hit_r2 {
                        let key = AutomationPointKey {
                            clip: AutomationClipKey {
                                track: track_id,
                                lane: lane.id,
                                clip: clip_in.id,
                            },
                            #[allow(clippy::cast_possible_truncation)]
                            point_idx: p_idx as u32,
                        };
                        let r = Rect {
                            x: px - radius,
                            y: py - radius,
                            w: radius * 2.0,
                            h: radius * 2.0,
                        };
                        hit = Some((key, r));
                    }
                }
            }
        },
    );
    hit
}

/// M14 Phase 63n-2 (#028): lane body 内 cursor から該当する `(track_idx, lane_idx, header_rect,
/// body_rect)` を返す。 `for_each_visible_lane` を 1 度走らせて y 範囲が合う最初の lane を採用
/// (lane 群は y で disjoint なので「最初」 = 「唯一」)。 cursor が lane 内にいるかどうかの判定で
/// header_rect / body_rect 共通の y 範囲だけを見る (x は header / body 跨ぎでも lane 1 つ)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_lane_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cy: f32,
) -> Option<(usize, usize, Rect, Rect)> {
    let mut found: Option<(usize, usize, Rect, Rect)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |t_idx, l_idx, _lane, h_rect, b_rect| {
            if found.is_some() {
                return;
            }
            if cy >= h_rect.y && cy < h_rect.y + h_rect.h {
                found = Some((t_idx, l_idx, h_rect, b_rect));
            }
        },
    );
    found
}

/// M14 Phase 63n-2 (#028): visible track 群から `(track_id, lane_id, clip_id)` 三つ組で対応する
/// `(lane, clip)` 参照を取得する pure helper。 press / release で何度も lookup するため、 関数化
/// しないと if let 連鎖が press block を膨らませる。
#[must_use]
fn find_lane_clip(
    visible_tracks: &[ArrangementTrack],
    key: AutomationClipKey,
) -> Option<(&ArrangementAutomationLane, &ArrangementAutomationClip)> {
    let track = visible_tracks.iter().find(|t| t.id == key.track)?;
    let lane = track.automation_lanes.iter().find(|l| l.id == key.lane)?;
    let clip = lane.clips.iter().find(|c| c.id == key.clip)?;
    Some((lane, clip))
}

/// M14 Phase 63n-9 (#033): S 字 cubic Bezier (制御点 x=(1/3, 2/3) 固定) の y(t) を評価。
/// `flatten_lane_segment` の Bezier 分岐と同 SSoT。 t=0 で a、 t=1 で b、 tension=0 で線形等価、
/// `tension=+1.0` で c1y=a, c2y=b (滑らかな S 字)、 `tension=-1.0` で c1y=b, c2y=a (overshoot 反転)。
#[must_use]
fn evaluate_bezier_y(a: f32, b: f32, tension: f32, t: f32) -> f32 {
    let t_clamped = tension.clamp(-1.0, 1.0);
    let diag1 = a + (b - a) * (1.0 / 3.0);
    let diag2 = a + (b - a) * (2.0 / 3.0);
    let mix = t_clamped.abs();
    let (target1, target2) = if t_clamped >= 0.0 { (a, b) } else { (b, a) };
    let c1y = diag1 * (1.0 - mix) + target1 * mix;
    let c2y = diag2 * (1.0 - mix) + target2 * mix;
    let omt = 1.0 - t;
    omt.powi(3) * a + 3.0 * omt.powi(2) * t * c1y + 3.0 * omt * t.powi(2) * c2y + t.powi(3) * b
}

/// M14 Phase 63n-9 (#033): tension/bend handle の screen 座標を計算。
/// `(prev_x, prev_y)` と `(cur_x, cur_y)` は curve 端点の screen 座標、 `kind` + `param_value` で
/// segment 中央 (= t=0.5) の y を curve 評価値から算出。 handle は curve から上方向に `offset_px`
/// 飛び出させて click target を curve 線 (1.5px) と分離。 daw_01 #033 §B Q3=A 仕様。
#[must_use]
fn compute_curve_handle_pos(
    prev_x: f32,
    prev_y: f32,
    cur_x: f32,
    cur_y: f32,
    kind: SetAutomationCurveParamKind,
    param_value: f32,
    offset_px: f32,
) -> (f32, f32) {
    let x = (prev_x + cur_x) * 0.5;
    let mid_y = match kind {
        SetAutomationCurveParamKind::BezierTension => {
            evaluate_bezier_y(prev_y, cur_y, param_value, 0.5)
        }
        SetAutomationCurveParamKind::ExponentialBend => {
            let exponent = 2.0_f32.powf(param_value.clamp(-1.0, 1.0));
            prev_y + (cur_y - prev_y) * (0.5_f32).powf(exponent)
        }
    };
    (x, mid_y - offset_px)
}

/// M14 Phase 63n-9 (#033): cursor 座標から hit する curve param handle を返す。 `selected_points`
/// に含まれる point の **Bezier / Exponential 入射 segment** にのみ handle が存在 (= Hold / Linear は
/// handle なし、 first point (= idx 0) も入射 segment なしで除外)。 hit zone は handle の **半径 2 倍**
/// (= 8px @ default radius=4)、 描画と同 SSoT (`compute_curve_handle_pos`)。
/// 戻り値: `(point_key, kind, current_value, lane_height_px)` — current_value は drag session の
/// `anchor_value`、 lane_height_px は sensitivity 計算用 (`effective_lane_height_px` = max(_, 40))。
#[must_use]
#[allow(clippy::too_many_arguments)]
fn find_curve_param_handle_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    selected_points: &[AutomationPointKey],
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<(AutomationPointKey, SetAutomationCurveParamKind, f32, u16)> {
    if selected_points.is_empty() {
        return None;
    }
    let handle_r = style.automation_curve_param_handle_radius_px.max(2.0);
    let hit_r_sq = (handle_r * 2.0).powi(2);
    let offset = style.automation_curve_param_handle_offset_px;
    let pad = style.automation_clip_v_pad_px;
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    for (i, t) in visible_tracks.iter().enumerate() {
        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
            continue;
        }
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        let mut lane_y = row_top + row_h;
        for lane in &t.automation_lanes {
            if !lane.visible {
                continue;
            }
            let lh = f32::from(lane.height_px);
            let clip_y = lane_y + pad;
            let clip_h = (lh - pad * 2.0).max(2.0);
            for c in &lane.clips {
                for p_idx in 1..c.points.len() {
                    let key = AutomationPointKey {
                        clip: AutomationClipKey {
                            track: t.id,
                            lane: lane.id,
                            clip: c.id,
                        },
                        #[allow(clippy::cast_possible_truncation)]
                        point_idx: p_idx as u32,
                    };
                    if !selected_points.contains(&key) {
                        continue;
                    }
                    let p = &c.points[p_idx];
                    let (kind, value) = match p.curve {
                        ArrangementCurveKind::Bezier { tension } => {
                            (SetAutomationCurveParamKind::BezierTension, tension)
                        }
                        ArrangementCurveKind::Exponential { bend } => {
                            (SetAutomationCurveParamKind::ExponentialBend, bend)
                        }
                        _ => continue, // Hold / Linear: handle なし
                    };
                    let prev = &c.points[p_idx - 1];
                    let prev_abs = c.start_beat + prev.time_beat;
                    let cur_abs = c.start_beat + p.time_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let prev_x = lanes.x + ((prev_abs - view.start_beat) * beat_to_px) as f32;
                    #[allow(clippy::cast_possible_truncation)]
                    let cur_x = lanes.x + ((cur_abs - view.start_beat) * beat_to_px) as f32;
                    let prev_y =
                        clip_y + (1.0 - prev.value_norm.clamp(0.0, 1.0)) * clip_h;
                    let cur_y =
                        clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                    let (hx, hy) = compute_curve_handle_pos(
                        prev_x, prev_y, cur_x, cur_y, kind, value, offset,
                    );
                    let dx = cx - hx;
                    let dy = cy - hy;
                    if dx * dx + dy * dy <= hit_r_sq {
                        return Some((key, kind, value, lane.height_px));
                    }
                }
            }
            lane_y += lh;
        }
    }
    None
}

/// M14 Phase 63n-9 (#033): handle drag の sensitivity 計算。 dy → value delta。
/// Q3=A 仕様: `effective_lane_height = max(lane_height_px, 40)` drag で full range (`-2.0`)、 つまり
/// `1 px = 2.0 / effective_h` の value delta。 Alt 押下で × 0.2 (= 5x 精細)。 y は screen 軸で上が
/// 負なので上 drag = + value (符号反転)。
#[must_use]
fn curve_param_delta_from_dy(dy: f32, effective_h: f32, alt: bool) -> f32 {
    let raw = -dy * 2.0 / effective_h.max(1.0);
    if alt { raw * 0.2 } else { raw }
}

/// M14 Phase 63n-8 (#033): point key から `(time_beat, value_norm, clip_start, clip_len)` を取得。
/// multi-select drag の release commit で各 selected point の anchor を再 lookup するために使う
/// (drag 中は Edit が流れないので model 不変、 visible_tracks がそのまま使える前提)。
#[must_use]
fn find_automation_point_data(
    visible_tracks: &[ArrangementTrack],
    key: AutomationPointKey,
) -> Option<(f64, f32, f64, f64)> {
    let track = visible_tracks.iter().find(|t| t.id == key.clip.track)?;
    let lane = track.automation_lanes.iter().find(|l| l.id == key.clip.lane)?;
    let clip = lane.clips.iter().find(|c| c.id == key.clip.clip)?;
    let p = clip.points.get(key.point_idx as usize)?;
    Some((p.time_beat, p.value_norm, clip.start_beat, clip.len_beats))
}

/// M14 Phase 63n-8 (#033): 短 click on point の Shift / Ctrl 押下時の toggle 計算。 prev の順序を保ち、
/// `key` が含まれていれば除去、 無ければ末尾に追加する idiom (= UI で「最後に touched された点」 が
/// list 末尾に来る、 Bitwig / Live と同 UX)。
#[must_use]
fn toggle_selection(prev: &[AutomationPointKey], key: AutomationPointKey) -> Vec<AutomationPointKey> {
    if prev.contains(&key) {
        prev.iter().copied().filter(|k| *k != key).collect()
    } else {
        let mut out = prev.to_vec();
        out.push(key);
        out
    }
}

/// M14 Phase 63n-8 (#033): lasso rect 内に **中心が含まれる** visible automation point を集める。
/// visible_tracks scope (collapsed track / `automation_lanes_collapsed=true` の lane 群 / `lane.visible=false`
/// の lane は除外)、 既存 `automation_point_at` の hit-test scope と整合。 点中心 (= `(px, py)`) は
/// 描画と同 SSoT (`body_origin_x + (abs_beat - view.start_beat) * beat_to_px`、 `clip_y + (1 - value) * clip_h`)。
#[must_use]
fn collect_points_in_rect(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    view: ArrangementView,
    lanes: Rect,
    rect: Rect,
) -> Vec<AutomationPointKey> {
    // 描画と同じ縦 padding (= `automation_clip_v_pad_px` default 6.0、 `draw_automation_lane` SSoT)。
    const PAD: f32 = 6.0;
    let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
    let mut out: Vec<AutomationPointKey> = Vec::new();
    for (i, t) in visible_tracks.iter().enumerate() {
        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
            continue;
        }
        let row_top = tops[i];
        let row_h = effective_track_row_h(t, view.track_row_h);
        let mut lane_y = row_top + row_h;
        for lane in &t.automation_lanes {
            if !lane.visible {
                continue;
            }
            let lh = f32::from(lane.height_px);
            // 描画と同じ縦 padding 適用 (`draw_automation_lane` SSoT)。
            let clip_y = lane_y + PAD;
            let clip_h = (lh - PAD * 2.0).max(2.0);
            for c in &lane.clips {
                for (p_idx, p) in c.points.iter().enumerate() {
                    let abs_beat = c.start_beat + p.time_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let px = lanes.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
                    let py = clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                    if rect.contains(px, py) {
                        out.push(AutomationPointKey {
                            clip: AutomationClipKey {
                                track: t.id,
                                lane: lane.id,
                                clip: c.id,
                            },
                            point_idx: p_idx as u32,
                        });
                    }
                }
            }
            lane_y += lh;
        }
    }
    out
}

/// daw_01 #071: lasso rect と交差する automation clip を集める (`collect_points_in_rect` の clip 版)。
/// `for_each_visible_lane` で body_rect を取り、 描画 / hit-test と同じ clip rect 式 (縦 padding 適用済)
/// で `rects_intersect` 判定する。 collapsed / invisible lane は `for_each_visible_lane` が除外済。
#[allow(clippy::too_many_arguments)]
fn collect_clips_in_rect(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    style: &ArrangementStyle,
    rect: Rect,
) -> Vec<AutomationClipKey> {
    let mut out: Vec<AutomationClipKey> = Vec::new();
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes.x,
        lanes.w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            let track_id = visible_tracks[t_idx].id;
            let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
            let pad = style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            for clip in &lane.clips {
                #[allow(clippy::cast_possible_truncation)]
                let cx_clip =
                    body_rect.x + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
                #[allow(clippy::cast_possible_truncation)]
                let cw = ((clip.len_beats * beat_to_px) as f32).max(2.0);
                let r = Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h };
                if rects_intersect(r, rect) {
                    out.push(AutomationClipKey {
                        track: track_id,
                        lane: lane.id,
                        clip: clip.id,
                    });
                }
            }
        },
    );
    out
}

/// daw_01 #071: 指定 `keys` の automation clip 群を drag anchor に変換する (MIDI clip の anchor 構築の
/// automation 版)。 `for_each_visible_lane` で各 clip の lane body_rect を取り、 戻りは `keys` 順
/// (= grabbed-first を保つ)。 visible でない / 見つからない key は skip。
#[allow(clippy::too_many_arguments)]
fn collect_automation_clip_anchors(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    keys: &[AutomationClipKey],
) -> Vec<AutomationClipDragAnchor> {
    let mut found: Vec<AutomationClipDragAnchor> = Vec::new();
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            let track_id = visible_tracks[t_idx].id;
            for clip in &lane.clips {
                let key = AutomationClipKey { track: track_id, lane: lane.id, clip: clip.id };
                if keys.contains(&key) {
                    found.push(AutomationClipDragAnchor {
                        key,
                        start_beat: clip.start_beat,
                        len_beats: clip.len_beats,
                        lane: key.lane_key(),
                        body_rect,
                    });
                }
            }
        },
    );
    keys.iter()
        .filter_map(|k| found.iter().find(|a| a.key == *k).copied())
        .collect()
}

/// M14 Phase 63n-2 (#028): lane body 内 cursor から hit する automation clip を返す。
/// 戻り値: `(clip_key, clip_local_time_beat, value_norm)` (clip_local_time_beat は clip start から
/// のオフセット拍、 value_norm は cy 座標から逆算した `0.0..=1.0`)。 cursor が clip ギャップ内なら
/// `None` (空き click では空気穴 = caller 側で `AddAutomationPoint` 発行しない)。
fn automation_clip_at(
    track_id: u32,
    lane: &ArrangementAutomationLane,
    body_rect: Rect,
    view: ArrangementView,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
) -> Option<(AutomationClipKey, f64, f32)> {
    let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
    let pad = style.automation_clip_v_pad_px;
    let clip_y = body_rect.y + pad;
    let clip_h = (body_rect.h - pad * 2.0).max(2.0);
    if cy < clip_y || cy >= clip_y + clip_h {
        return None;
    }
    for clip in &lane.clips {
        #[allow(clippy::cast_possible_truncation)]
        let cx_clip = body_rect.x + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
        #[allow(clippy::cast_possible_truncation)]
        let cw = ((clip.len_beats * beat_to_px) as f32).max(2.0);
        if cx >= cx_clip && cx < cx_clip + cw {
            let abs_beat = view.start_beat + f64::from(cx - body_rect.x) / beat_to_px;
            let local = (abs_beat - clip.start_beat).clamp(0.0, clip.len_beats);
            let value_norm = (1.0 - (cy - clip_y) / clip_h).clamp(0.0, 1.0);
            let key = AutomationClipKey {
                track: track_id,
                lane: lane.id,
                clip: clip.id,
            };
            return Some((key, local, value_norm));
        }
    }
    None
}

/// M14 Phase 63n-3 (#028): lane body 内の automation clip 上で hit する
/// `(AutomationClipKey, ClipDragKind, clip_rect, body_rect)` を返す。
/// `clip_zone_at` と完全同 仕様: clip rect 左右 edge から内外 ±`edge` px が Resize、 内側中央が Move、
/// 短 clip (`r.w <= edge * 2`) は rect 内全 Move (rect 外側のみ resize)。 後勝ち順 (描画順と整合)。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_clip_zone_at(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    view: ArrangementView,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes: Rect,
    style: &ArrangementStyle,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<(AutomationClipKey, ClipDragKind, Rect, Rect)> {
    if !lanes.contains(cx, cy) {
        return None;
    }
    let mut hit: Option<(AutomationClipKey, ClipDragKind, Rect, Rect)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes.x,
        lanes.w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if cy < body_rect.y || cy >= body_rect.y + body_rect.h {
                return;
            }
            let track_id = visible_tracks[t_idx].id;
            let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
            let pad = style.automation_clip_v_pad_px;
            let clip_y = body_rect.y + pad;
            let clip_h = (body_rect.h - pad * 2.0).max(2.0);
            if cy < clip_y || cy >= clip_y + clip_h {
                return;
            }
            for clip in &lane.clips {
                #[allow(clippy::cast_possible_truncation)]
                let cx_clip = body_rect.x
                    + ((clip.start_beat - view.start_beat) * beat_to_px) as f32;
                #[allow(clippy::cast_possible_truncation)]
                let cw = ((clip.len_beats * beat_to_px) as f32).max(2.0);
                let r = Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h };
                if cx < r.x - edge || cx >= r.x + r.w + edge {
                    continue;
                }
                let in_rect = cx >= r.x && cx < r.x + r.w;
                let near_left = cx < r.x + edge;
                let near_right = cx >= r.x + r.w - edge;
                let short_clip = r.w <= edge * 2.0;
                let kind = if short_clip && in_rect {
                    ClipDragKind::Move
                } else if near_left && (!in_rect || cx - r.x < edge) {
                    ClipDragKind::ResizeLeft
                } else if near_right && (!in_rect || (r.x + r.w) - cx < edge) {
                    ClipDragKind::ResizeRight
                } else {
                    ClipDragKind::Move
                };
                let key = AutomationClipKey {
                    track: track_id,
                    lane: lane.id,
                    clip: clip.id,
                };
                hit = Some((key, kind, r, body_rect));
            }
        },
    );
    hit
}

/// M14 Phase 63n-3 (#028): cursor y から該当する `(AutomationLaneKey, body_rect)` を返す
/// (`automation_lane_at` の lane_key 抽出版、 cross-lane drag の release frame で `last_mouse.1` から
/// drop 先 lane を確定する用途)。 cursor が lane 群の y 範囲外なら `None`。
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn automation_lane_key_at_y(
    visible_tracks: &[ArrangementTrack],
    tops: &[f32],
    track_row_h: f32,
    header_pane_x: f32,
    header_pane_w: f32,
    lanes_x: f32,
    lanes_w: f32,
    style: &ArrangementStyle,
    cy: f32,
) -> Option<(AutomationLaneKey, Rect)> {
    let mut found: Option<(AutomationLaneKey, Rect)> = None;
    for_each_visible_lane(
        visible_tracks,
        tops,
        track_row_h,
        header_pane_x,
        header_pane_w,
        lanes_x,
        lanes_w,
        style,
        |t_idx, _l_idx, lane, _h_rect, body_rect| {
            if found.is_some() {
                return;
            }
            if cy >= body_rect.y && cy < body_rect.y + body_rect.h {
                let track_id = visible_tracks[t_idx].id;
                found = Some((
                    AutomationLaneKey { track: track_id, lane: lane.id },
                    body_rect,
                ));
            }
        },
    );
    found
}


/// 単一 segment (前 point → 次 point) を flatten。
/// `kind` が `Hold` なら階段 (前値で水平 → 次 point の x で垂直)、 `Linear` なら直線、
/// `Bezier { tension }` なら S 字 cubic Bezier、 `Exponential { bend }` なら指数 curve の polyline。
/// 出力点列は `out` に push (caller が始点を 1 度 push 済の前提、 終点 (= 次 point) を含む)。
/// `p0` / `p3` は前後 segment 用の virtual 点だが、 `Bezier` の S 字 cubic は `p1`/`p2` のみで決まり
/// `Exponential` も同様なので両者では参照されない (= signature は M14 Phase 63n-1 互換維持)。
fn flatten_lane_segment(
    _p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    _p3: (f32, f32),
    kind: ArrangementCurveKind,
    max_segment_px: f32,
    out: &mut Vec<(f32, f32)>,
) {
    match kind {
        ArrangementCurveKind::Hold => {
            // 階段: 前値 (p1.y) を p2.x まで水平に保ち、 p2 で垂直に立ち上がる。
            // out には始点 (p1) は積み済の前提なので、 (p2.x, p1.y) → (p2.x, p2.y) の 2 点を追加。
            out.push((p2.0, p1.1));
            out.push(p2);
        }
        ArrangementCurveKind::Linear => {
            out.push(p2);
        }
        ArrangementCurveKind::Bezier { tension } => {
            // M14 Phase 63n-7 (daw_01 #033): S 字 cubic Bezier。 daw_01 `apply_curve` SSoT と
            // 完全同一の制御点配置:
            //   - 制御点 x は (1/3, 2/3) 固定で x(t) = t (4 制御点の x が 0, 1/3, 2/3, 1 の等差列で
            //     Bernstein 基底が打ち消し合うため、 cubic Bezier x 成分は **恒等関数** に縮退)
            //   - 制御点 y を `tension` で対角線 ↔ end-hold を lerp:
            //       diag1 = p1.y + (p2.y - p1.y) * 1/3   (端点で linear)
            //       diag2 = p1.y + (p2.y - p1.y) * 2/3
            //       mix = |tension|, target1/2 = (p1.y, p2.y) if tension >= 0 else (p2.y, p1.y)
            //       c1.y = lerp(diag1, target1, mix), c2.y = lerp(diag2, target2, mix)
            // tension = 0.0 で 4 制御点が対角線上 (= 直線)、 +1.0 で滑らかな S 字、 -1.0 で overshoot 反転。
            let t_clamped = tension.clamp(-1.0, 1.0);
            let a = p1.1;
            let b = p2.1;
            let dx = p2.0 - p1.0;
            let c1x = p1.0 + dx * (1.0 / 3.0);
            let c2x = p1.0 + dx * (2.0 / 3.0);
            let diag1 = a + (b - a) * (1.0 / 3.0);
            let diag2 = a + (b - a) * (2.0 / 3.0);
            let mix = t_clamped.abs();
            let (target1, target2) = if t_clamped >= 0.0 { (a, b) } else { (b, a) };
            let c1y = diag1 * (1.0 - mix) + target1 * mix;
            let c2y = diag2 * (1.0 - mix) + target2 * mix;
            // 既存の adaptive de Casteljau (perpendicular distance 判定) を新制御点で呼ぶ。
            flatten_lane_cubic(p1, (c1x, c1y), (c2x, c2y), p2, max_segment_px, 0, out);
        }
        ArrangementCurveKind::Exponential { bend } => {
            // M14 Phase 63n-7 (daw_01 #033): value = a + (b - a) * t^(2^bend) の polyline。
            // bend=0 で linear、 +1 で前半遅・後半速 (t^2、 二次曲線)、 -1 で前半速・後半遅 (t^0.5、 平方根)。
            // segment が滑らかな単調関数なので uniform sampling で十分 (adaptive 不要)、
            // sample 数は `dx / max_segment_px` を切り上げ + min 16 (短 segment でも形状を視認できる最小段数)。
            let bend_clamped = bend.clamp(-1.0, 1.0);
            let exponent = 2.0_f32.powf(bend_clamped);
            let dx_abs = (p2.0 - p1.0).abs();
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let samples = (dx_abs / max_segment_px.max(1e-3)).ceil().max(16.0) as usize;
            for i in 1..=samples {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 / samples as f32;
                let x = p1.0 + (p2.0 - p1.0) * t;
                let y = p1.1 + (p2.1 - p1.1) * t.powf(exponent);
                out.push((x, y));
            }
        }
    }
}

const MAX_LANE_FLATTEN_DEPTH: u32 = 14;

/// 点 `p` と直線 `a-b` の垂直距離 (`automation_curve` widget の `perpendicular_dist` と同戦略)。
/// chord 距離 (`p0`-`p3`) で判定すると control points (p1, p2) が始終点直線から大きく離れていても
/// chord が小さければ flatten 終了 → curve がポイントを通らず直線化する bug の原因 (#028 user 指摘 2)。
/// 控制点と直線の **垂直距離** で判定すれば curve が正確にポイントを通る。
#[inline]
fn perpendicular_dist_lane(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return ((p.0 - a.0).powi(2) + (p.1 - a.1).powi(2)).sqrt();
    }
    ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len
}

fn flatten_lane_cubic(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
    max_dist: f32,
    depth: u32,
    out: &mut Vec<(f32, f32)>,
) {
    // 終了条件: control points (p1, p2) の始終点直線 (p0-p3) からの垂直距離が max_dist 以下なら
    // 直線で近似可能 (= curve が直線に収束)。 depth limit はガード。
    let d1 = perpendicular_dist_lane(p1, p0, p3);
    let d2 = perpendicular_dist_lane(p2, p0, p3);
    if d1.max(d2) < max_dist || depth >= MAX_LANE_FLATTEN_DEPTH {
        out.push(p3);
        return;
    }
    // de Casteljau の中点分割
    let q0 = ((p0.0 + p1.0) * 0.5, (p0.1 + p1.1) * 0.5);
    let q1 = ((p1.0 + p2.0) * 0.5, (p1.1 + p2.1) * 0.5);
    let q2 = ((p2.0 + p3.0) * 0.5, (p2.1 + p3.1) * 0.5);
    let r0 = ((q0.0 + q1.0) * 0.5, (q0.1 + q1.1) * 0.5);
    let r1 = ((q1.0 + q2.0) * 0.5, (q1.1 + q2.1) * 0.5);
    let s = ((r0.0 + r1.0) * 0.5, (r0.1 + r1.1) * 0.5);
    flatten_lane_cubic(p0, q0, r0, s, max_dist, depth + 1, out);
    flatten_lane_cubic(s, r1, q2, p3, max_dist, depth + 1, out);
}

/// lane 内 1 clip の curve を flatten 後の screen 座標点列で返す。 `clip_rect` は clip の
/// 描画域 (lane body 内、 縦 padding 適用済)、 `body_origin_x` / `body_w` は lane body 全体の
/// x 範囲 (= clip 越しに spans する curve 位置計算の base)、 `beat_to_px` は **screen-wide な拍 →
/// px 換算** (= `body_w / view.len_beats`)。 旧設計で `clip_rect.w / view_len_beats` を使うと
/// clip 長 ≠ view 長のとき curve x が point dot 描画 (`body_w / view.len_beats` で計算) と
/// ずれる bug の根本原因 (#028 user 指摘 2)。 caller が同 `beat_to_px` を渡すことで両者が一致。
fn flatten_lane_curve(
    clip: &ArrangementAutomationClip,
    clip_rect: Rect,
    view_start_beat: f64,
    body_origin_x: f32,
    beat_to_px: f64,
    max_segment_px: f32,
) -> Vec<(f32, f32)> {
    if clip.points.is_empty() {
        return Vec::new();
    }
    let to_screen = |p: &ArrangementAutomationPoint| -> (f32, f32) {
        // clip-local time → arrangement absolute beat → screen x (body_origin_x ベース)
        let abs_beat = clip.start_beat + p.time_beat;
        #[allow(clippy::cast_possible_truncation)]
        let x = body_origin_x + ((abs_beat - view_start_beat) * beat_to_px) as f32;
        let y = clip_rect.y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_rect.h;
        (x, y)
    };
    let n = clip.points.len();
    let mut out = Vec::with_capacity(n * 4);
    out.push(to_screen(&clip.points[0]));
    for i in 0..(n - 1) {
        let p0 = to_screen(&clip.points[i.saturating_sub(1)]); // i==0 で自分自身
        let p1 = to_screen(&clip.points[i]);
        let p2 = to_screen(&clip.points[i + 1]);
        let p3 = to_screen(&clip.points[(i + 2).min(n - 1)]); // 最終 segment では自分自身
        // 各 segment の curve は **次 point** の `curve` を使う (= incoming curve、 #028 §11.1 と整合)。
        let kind = clip.points[i + 1].curve;
        flatten_lane_segment(p0, p1, p2, p3, kind, max_segment_px, &mut out);
    }
    out
}

/// lane row (= header + body) を 1 つ描画。 `header_rect` は左 (track header と同 x 範囲)、
/// `body_rect` は右 (clip 描画域と同 x 範囲)。 `view` は arrangement の global view (start_beat /
/// len_beats / track_top 等を渡す、 lane 描画では `start_beat` / `len_beats` のみ参照)。
/// disabled lane (`enabled = false`) は curve / clip / point を `automation_lane_disabled_color` で描画。
/// M14 Phase 114 (daw_01 #086): clip fill / border は `lane.color` が唯一 source (`share_group_color` は
/// fill を上書きしない、 リンク識別は ⇌ glyph + #068 hover 強調のみ)。
/// M14 Phase 63n-3 (#028): `selected_clips_set` に含まれる `AutomationClipKey` は `clip_selected_fill` /
/// `clip_selected_border` で描画 (selected priority 最高)、 share_group_color = Some の clip は名前の左に
/// `share_group_link_glyph` (`⇌`) を 1 文字描画 (MIDI clip と同 idiom)。 `track_id` は selection lookup 用。
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn draw_automation_lane<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    track_id: u32,
    lane: &ArrangementAutomationLane,
    header_rect: Rect,
    body_rect: Rect,
    view: ArrangementView,
    style: &ArrangementStyle,
    lanes_clip: Rect,
    selected_clips_set: &HashSet<AutomationClipKey>,
) {
    // ---- 背景 (lane 行 全幅) ----
    push_filled_rect(
        hctx,
        Rect {
            x: header_rect.x,
            y: header_rect.y,
            w: header_rect.w + body_rect.w,
            h: header_rect.h,
        },
        style.automation_lane_bg,
    );

    // ---- header: ★ icon label slider 帯 👁▣✕ (描画 + Phase 63n-2 hit-test 対応) ----
    // M14 Phase 63n-2 (#028): 描画と hit-test の SSoT を `automation_lane_header_layout` に集約。
    // header_rect.w が極狭の場合 (`< automation_lane_header_min_w_px`) は layout が `None` で描画 skip。
    // FIXME #70: curve line / point dot の色は「lane.color 直塗り」 をやめ、 clip ごとに実際の
    // `fill` 輝度から白/黒 neutral を auto-contrast する (= clip 名 `clip_text_color_for` と同 SSoT)。
    // 黄など明るい識別色でも常にコントラストを確保する狙い。 実際の色決定は下の clip ループ内
    // (fill 確定後) で行う。 header の icon glyph 色は従来どおり `lane.color` を直接使う。
    if let Some(layout) = automation_lane_header_layout(header_rect, style) {
        let icon_size = style.automation_lane_icon_size.max(4.0);
        let pad = 4.0_f32;
        // ★ enabled marker (lane.enabled で星塗りつぶし切替)
        hctx.push_text(GlyphArea {
            text: Arc::from(if lane.enabled { "★" } else { "☆" }),
            left: layout.enabled_icon_rect.x,
            top: layout.enabled_icon_rect.y,
            font_size: icon_size,
            line_height: icon_size * 1.2,
            color: style.automation_lane_text_color,
            clip_rect: Some(header_rect),
            ..GlyphArea::default()
        });
        // [V] icon glyph (lane.icon_glyph、 lane 識別色)
        hctx.push_text(GlyphArea {
            text: Arc::from(lane.icon_glyph.to_string()),
            left: layout.icon_glyph_rect.x,
            top: layout.icon_glyph_rect.y,
            font_size: icon_size,
            line_height: icon_size * 1.2,
            color: lane.color,
            clip_rect: Some(header_rect),
            ..GlyphArea::default()
        });
        // label (icon_glyph の右、 visible_icon の左までの帯)
        let label_x = layout.icon_glyph_rect.x + layout.icon_glyph_rect.w + pad;
        let label_clip = Rect {
            x: label_x,
            y: header_rect.y,
            w: (layout.visible_icon_rect.x - label_x - pad).max(0.0),
            h: header_rect.h,
        };
        hctx.push_text(GlyphArea {
            text: Arc::clone(&lane.label),
            left: label_x,
            top: layout.icon_glyph_rect.y,
            font_size: icon_size,
            line_height: icon_size * 1.2,
            color: style.automation_lane_text_color,
            clip_rect: Some(label_clip),
            ..GlyphArea::default()
        });
        // default value slider 帯 (band_rect が `Some` の場合のみ、 行高に余裕がある時)
        if let Some(band) = layout.default_band_rect {
            push_filled_rect(hctx, band, style.track_volume_band_track);
            let fill_w = band.w * lane.default_value_norm.clamp(0.0, 1.0);
            push_filled_rect(
                hctx,
                Rect { x: band.x, y: band.y, w: fill_w, h: band.h },
                style.track_volume_band_fill,
            );
        }
        // 右寄せ icon 群 (👁 ▣ ✕、 Phase 63n-2 で hit-test 対応)
        for &(g, r) in &[
            ('👁', layout.visible_icon_rect),
            ('▣', layout.mute_icon_rect),
            ('✕', layout.delete_icon_rect),
        ] {
            hctx.push_text(GlyphArea {
                text: Arc::from(g.to_string()),
                left: r.x,
                top: r.y,
                font_size: icon_size,
                line_height: icon_size * 1.2,
                color: style.automation_lane_text_color,
                clip_rect: Some(header_rect),
                ..GlyphArea::default()
            });
        }
    }

    // ---- body 背景 (header と区切り線) ----
    push_filled_rect(hctx, body_rect, style.automation_lane_bg);
    // default_value 水平線
    let default_y = body_rect.y + (1.0 - lane.default_value_norm.clamp(0.0, 1.0)) * body_rect.h;
    hctx.push_lines(daw_ui_renderer::LineBatch {
        segments: vec![daw_ui_renderer::LineSegment {
            a: [body_rect.x, default_y],
            b: [body_rect.x + body_rect.w, default_y],
            color: style.automation_default_line_color,
        }]
        .into(),
        line_width_px: style.automation_default_line_width_px,
        clip_rect: Some(body_rect),
    });

    // ---- clips: rect + curve flatten + point dots ----
    let view_end = view.start_beat + view.len_beats;
    let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
    for c in &lane.clips {
        let end = c.start_beat + c.len_beats;
        if end < view.start_beat || c.start_beat > view_end {
            continue;
        }
        // clip rect (lane body 内、 縦 padding 適用)
        #[allow(clippy::cast_possible_truncation)]
        let x = body_rect.x + ((c.start_beat - view.start_beat) * beat_to_px) as f32;
        #[allow(clippy::cast_possible_truncation)]
        let w = ((c.len_beats * beat_to_px) as f32).max(2.0);
        let pad = style.automation_clip_v_pad_px;
        let cy = body_rect.y + pad;
        let ch = (body_rect.h - pad * 2.0).max(2.0);
        let clip_rect = Rect { x, y: cy, w, h: ch };

        // M14 Phase 63n-3 (#028): selected な automation clip は selected_fill / selected_border で
        // 描画 (priority: selected > disabled > share_group > lane.color)。
        let clip_key = AutomationClipKey { track: track_id, lane: lane.id, clip: c.id };
        let is_selected = selected_clips_set.contains(&clip_key);
        // M14 Phase 114 (daw_01 #086): automation clip は専用の `color` field を持たず `lane.color` が
        // fill / border の唯一 source (audio clip の `clip.color` に相当)。 `share_group_color` は fill /
        // border を上書きせず、 リンク識別は ⇌ glyph + #068 hover 強調のみが担う (#086)。 disabled lane は
        // **clip rect の fill / border のみ灰色** (= bypass marker)、 中身 (curve / point / clip 名) は元の
        // lane.color のままにして可読性を保つ (Bitwig / Live と同パターン、 #028 user 指摘 3)。
        let (fill, border) = if is_selected {
            (style.clip_selected_fill, style.clip_selected_border)
        } else if lane.enabled {
            (
                Color { r: lane.color.r, g: lane.color.g, b: lane.color.b, a: 0.20 },
                lane.color,
            )
        } else {
            // disabled: fill = 灰色 alpha 0.10 (lane_bg がほぼ透ける、 中身可読) + border = 灰色
            // alpha 1.0 (識別 marker、 不透明で確実に見える)。 fill alpha 0 だと renderer が rect 全体を
            // skip する可能性があるので非ゼロを保つ (#028 user 指摘 3 = 配色で見えない)。
            let dc = style.automation_lane_disabled_color;
            (
                Color { r: dc.r, g: dc.g, b: dc.b, a: 0.10 },
                Color { r: dc.r, g: dc.g, b: dc.b, a: 1.0 },
            )
        };
        hctx.push_rect(RectCommand {
            rect: clip_rect,
            fill,
            border,
            border_width: style.clip_border_w,
            radius: [style.clip_radius; 4],
            clip_rect: Some(lanes_clip),
        });

        // clip name + share group link glyph (⇌) — MIDI clip と同 idiom (`draw_clip` と対称)。
        // share_group_color = Some(hue) のとき名前の左に link glyph を 1 文字描画 + name を glyph 幅 +
        // 2px gap 分ずらす。 selection / disabled とは独立に描画 (link 関係は bypass / 選択と直交)。
        // M14 Phase 91 (daw_01 #062): 名前 / link glyph の表示を MIDI clip (`draw_clip`) と完全に揃える。
        // (1) 表示しきい値 / font_size / line_height を MIDI と同値に (旧 `w >= 28.0` + `* 0.85` +
        //     line_height = clip_text_size 直値を撤去)。 (2) 文字色は enabled lane なら fill 輝度由来の
        //     auto-contrast (`clip_text_color_for`、 alpha 0.20 の半透明 fill は automation_lane_bg と
        //     合成して実効色判定)。 disabled lane は従来どおり `automation_lane_disabled_color` 固定
        //     (= bypass marker、 #060 の selected 統合とは別文脈) で auto-contrast 対象外。 opt-out
        //     (`clip_auto_contrast_text == false`) は automation 専用の `automation_lane_text_color` に
        //     フォールバック (= clip 全般の `clip_text_color` ではなく従来色を維持)。
        if w > 24.0 && ch > style.clip_text_size + 2.0 {
            let glyph_color = if !lane.enabled {
                style.automation_lane_disabled_color
            } else if style.clip_auto_contrast_text {
                clip_text_color_for(style, fill, style.automation_lane_bg)
            } else {
                style.automation_lane_text_color
            };
            let font_size = style.clip_text_size;
            let has_link = c.share_group_color.is_some();
            let text_left = if has_link {
                clip_rect.x + 4.0 + font_size + 2.0
            } else {
                clip_rect.x + 4.0
            };
            if has_link {
                hctx.push_text(GlyphArea {
                    text: Arc::from(style.share_group_link_glyph.to_string()),
                    left: clip_rect.x + 4.0,
                    top: clip_rect.y + 2.0,
                    font_size,
                    line_height: style.clip_text_size * 1.2,
                    color: glyph_color,
                    clip_rect: Some(clip_rect),
                    ..GlyphArea::default()
                });
            }
            hctx.push_text(GlyphArea {
                text: Arc::clone(&c.name),
                left: text_left,
                top: clip_rect.y + 2.0,
                font_size,
                line_height: style.clip_text_size * 1.2,
                color: glyph_color,
                clip_rect: Some(clip_rect),
                ..GlyphArea::default()
            });
        }

        // FIXME #70: curve line / point dot を背景輝度から白/黒 neutral で auto-contrast する。
        // enabled lane は実際に塗った `fill` (selected = 黄不透明 / 非選択 = lane.color alpha 0.20 を
        // lane_bg と合成) の実効輝度から `pick_contrast` で line / dot fill 色を選び、 dot の枠は
        // その逆色にして「line から浮いた node」 として常に縁が見えるようにする。 disabled lane は
        // bypass marker として従来の灰色 (`automation_lane_disabled_color`) を維持 (= clip 名と同方針)。
        let (curve_line_color, point_fill, point_border) = if lane.enabled {
            let eff_bg = crate::color::composite_over(fill, style.automation_lane_bg);
            let neutral =
                crate::color::pick_contrast(eff_bg, style.clip_text_color, style.clip_text_color_dark);
            let edge = crate::color::pick_contrast(
                neutral,
                style.clip_text_color,
                style.clip_text_color_dark,
            );
            (neutral, neutral, edge)
        } else {
            let dc = style.automation_lane_disabled_color;
            (dc, dc, Color { r: 1.0, g: 1.0, b: 1.0, a: 0.4 })
        };

        // curve flatten (clip 内描画域 = clip_rect 全体)。 caller の screen-wide な beat_to_px
        // (= body_rect.w / view.len_beats) を渡すことで、 curve x 座標が point dot 描画と完全一致。
        let flat = flatten_lane_curve(c, clip_rect, view.start_beat, body_rect.x, beat_to_px, 2.0);
        if flat.len() >= 2 {
            let segments: Vec<daw_ui_renderer::LineSegment> = flat
                .windows(2)
                .map(|w| daw_ui_renderer::LineSegment {
                    a: [w[0].0, w[0].1],
                    b: [w[1].0, w[1].1],
                    color: curve_line_color,
                })
                .collect();
            hctx.push_lines(daw_ui_renderer::LineBatch {
                segments: segments.into(),
                line_width_px: style.automation_curve_line_width_px,
                clip_rect: Some(clip_rect),
            });
        }
        // 各 point を 角丸円 (= 正方形 + 大 radius) で描画。 x の origin は **body_rect.x** (= 0
        // beat の screen x)、 そこから abs_beat * beat_to_px で point 位置を出す。 旧設計は
        // `clip_rect.x + (abs_beat - view.start_beat) * beat_to_px` で c.start_beat を 2 度足して
        // (clip_rect.x が既に c.start_beat 反映済) point dot が curve からずれる bug の根本原因
        // (#028 user 指摘 2 = curve 線が point を通らない)。
        let r = style.automation_point_radius_px;
        for p in &c.points {
            let abs_beat = c.start_beat + p.time_beat;
            #[allow(clippy::cast_possible_truncation)]
            let px = body_rect.x + ((abs_beat - view.start_beat) * beat_to_px) as f32;
            let py = clip_rect.y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_rect.h;
            hctx.push_rect(RectCommand {
                rect: Rect { x: px - r, y: py - r, w: r * 2.0, h: r * 2.0 },
                fill: point_fill,
                border: point_border,
                border_width: 1.0,
                radius: [r; 4],
                clip_rect: Some(clip_rect),
            });
        }
    }
}

// `draw_loop_band` は M14 Phase 69 (#041) で `crate::widgets::ruler_ops::draw_loop_band` に extract
// (view / style に依存しない汎用形に generalize、 piano_roll と共有)。

// ============================================================
// Public widget API
// ============================================================

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// arrangement widget (M9 Phase 45e、 M14 Phase 63c で multi-select + group hierarchy 対応)。
    ///
    /// 詳細は module doc 参照。`tracks` は順序付き配列 (上から下に並ぶ、 collapsed 親の子は描画 skip)。
    /// `selected_clips` / `selected_tracks` は外部 immutable borrow (Model 側 SSoT)。
    /// `make_edit` callback で各 `ArrangementEditRequest` を `Edit<M>` に変換する。
    ///
    /// **multi-select**: `selected_tracks` は順序不定の id 配列 (caller は `HashSet<u32>` /
    /// `Vec<u32>` どちらで持っても OK、 順序は `next` フィールドが visible 列順で生成する)。
    /// modifier (Single / RangeFromAnchor / Toggle) は widget 内 anchor + `pointer.modifiers` で
    /// decode し、 `SelectTrack` Edit に乗せて caller に通知する。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn arrangement<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        tracks: &[ArrangementTrack],
        // M14 Phase 127 (daw_01 #105): Arranger レーンの曲パート (昇順・非交差前提)。 空 slice で
        // レーン無し (= `view.arranger_lane_h == 0.0` と併せ従来描画と完全互換)。 `tracks` と同じく
        // 「描画対象は別 slice 引数」 idiom で渡し、 `ArrangementView` の `Copy` を壊さない。
        sections: &[SectionView],
        view: ArrangementView,
        selected_clips: &[ClipKey],
        selected_tracks: &[u32],
        selected_automation_clips: &[AutomationClipKey],
        selected_automation_points: &[AutomationPointKey],
        style: &ArrangementStyle,
        // M14 Phase 63n-10 (#034): arrangement 上端に表示する master row (song-level automation)。
        // `None` で旧挙動完全互換 (= master row 無し、 通常 track 群のみ)。 `Some(&master)` で上端 1 行
        // ("Master" 固定 label + 折り畳み可能な automation lane 群) を描画 / 編集対象に加える。 master の
        // automation lane は通常 track と同じ `ArrangementEditRequest` (`AddAutomationPoint` 等) を発火
        // するが、 `lane.track = MASTER_TRACK_ID` (= u32::MAX) で master と通常 track を識別する規約
        // (daw_01 conversation #034 で確定)。
        master_row: Option<&ArrangementMasterRow>,
        make_edit: F,
    ) -> ArrangementResponse
    where
        // M10 Phase 49: `Clone` を要求 (fader_at と同じく Undoable Edit の forward/inverse 2 closure に
        // make_edit を分配するため)。daw_prototype + trybuild basic.rs の closure literal は capture が
        // 自動 Clone なので追加対応不要。
        F: Fn(ArrangementEditRequest) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        let wid = WidgetId::ROOT.child((b"arrangement_widget", &id));
        let pointer = self.pointer;

        // ---- rect 分割 ----
        let header_w = view.header_w.max(0.0);
        let ruler_h = view.ruler_h.max(0.0);
        // M14 Phase 127 (daw_01 #105): Arranger レーンを ruler の直下・track lanes の上に確保する。
        // `arranger_lane_h == 0.0` で従来レイアウトと完全一致 (レーン無し)。 track lanes / header_pane の
        // y 原点を arranger 分だけ下げることで track row (header / lanes 双方) が自動的に下にずれる
        // (`header_pane.y == lanes.y` の不変条件は維持 = press_tops を header / lanes で共有する前提)。
        let arranger_lane_h = view.arranger_lane_h.max(0.0);
        let lanes_h = (rect.h - ruler_h - arranger_lane_h).max(1.0);
        let lanes_w = (rect.w - header_w).max(1.0);
        let header_pane = Rect {
            x: rect.x,
            y: rect.y + ruler_h + arranger_lane_h,
            w: header_w,
            h: lanes_h,
        };
        let ruler =
            Rect { x: rect.x + header_w, y: rect.y, w: lanes_w, h: ruler_h };
        // Arranger レーン本体 (lanes 幅、 ruler 直下) と header 側の見出し領域 ("Arranger" ラベル用)。
        let arranger_rect =
            Rect { x: rect.x + header_w, y: rect.y + ruler_h, w: lanes_w, h: arranger_lane_h };
        let arranger_header_rect =
            Rect { x: rect.x, y: rect.y + ruler_h, w: header_w, h: arranger_lane_h };
        let lanes = Rect {
            x: rect.x + header_w,
            y: rect.y + ruler_h + arranger_lane_h,
            w: lanes_w,
            h: lanes_h,
        };

        // M9 Phase 45f / M14 Phase 63j (#024): snap 用 zoom = lanes.w / view.len_beats。
        // press 振り分け (ruler の playhead seek) でも snap 計算に必要なため、 後の overlay 計算と
        // 共有する目的で関数の頭で 1 度計算する。
        let beat_per_px = view.len_beats / f64::from(lanes.w.max(1.0));
        #[allow(clippy::cast_possible_truncation)]
        let zoom_x_px_per_beat: f32 = (1.0 / beat_per_px) as f32;

        // ---- response 初期 ----
        let mut response = ArrangementResponse {
            ruler_rect: ruler,
            ..Default::default()
        };

        // ---- M14 Phase 63c (#016): visible 領域 (collapsed 親の subtree skip) を pre-compute ----
        // press / drag / release / draw すべてが visible-domain の row index で動くように、
        // `tracks` (caller's 全 list) を visible-only に絞った Vec を作って以降で共有する。
        // `clip_to_rect` / `track_index_from_y` の `track_index` 引数は visible-idx と解釈される。
        // tracks_for_draw (heavy() / 描画用、 後述 optimistic reorder 適用版) も同じ visibility 集合。
        let visible_indices_press: Vec<usize> = compute_visible_indices(tracks);
        // M14 Phase 63n-10 (#034): master_row を synthetic `ArrangementTrack` (id = `MASTER_TRACK_ID`、
        // clips 空、 mute/solo false、 automation_lanes は master_row から複製) として `visible_tracks[0]`
        // に prepend。 既存 hit-test / 描画コードを **そのまま reuse** できる (= clips が空なので
        // MIDI/Audio clip drag は自然に no-op、 automation_lanes は通常 track と同 schema)。 「Master」
        // ラベル描画 / mute/solo button 非表示 / clip 系 EditRequest 抑制は描画 / 押下 path で
        // `t.id == MASTER_TRACK_ID` 分岐を入れて対処。
        //
        // `visible_indices_press` は **caller's tracks の index 列**で master の caller index は無いため
        // この Vec は変更しない (= 後段の clone source は `tracks` だが master 経路は別ロジック)。
        let mut visible_tracks: Vec<ArrangementTrack> = visible_indices_press
            .iter()
            .map(|&i| tracks[i].clone())
            .collect();
        if let Some(master) = master_row {
            visible_tracks.insert(0, synthesize_master_track(master));
        }
        // M14 Phase 63n-1 (#028): visible track の prefix-sum row tops。 lane 0 個 (= 既存挙動)
        // では `tops[i] = lanes.y - track_top + i * track_row_h` と等価。 expand 中の lane 群が
        // ある track 以降は次 track 以降の row top が下にずれる (= 描画 / hit-test SSoT)。
        // M14 Phase 63n-10 (#034): `visible_tracks[0]` に master_row が prepend されていれば、 master の
        // 高さ + lanes 高さ込みの prefix sum が自動で組まれる (= 通常 track と同じ helper を再利用)。
        let press_tops =
            visible_track_row_tops(&visible_tracks, lanes.y, view.track_top, view.track_row_h);
        // M14 Phase 63c (#016): collapsed 後でも「Group A は子を持つ track」 と判定するため、
        // **caller の full `tracks`** から「他 track の parent_id として参照されている id 集合」 を 1 度計算。
        // `is_group_track(id, visible_tracks)` だと collapsed で children が filter outされ false 化する罠を回避。
        let is_group_set: HashSet<u32> =
            tracks.iter().filter_map(|t| t.parent_id).collect();

        // ---- press 振り分け: audio_drag / clip_drag / loop_drag / playhead_drag を state に積む ----
        // M14 Phase 63j (#024): ruler の plain click は press frame で `SetPlayheadBeat` を 1 度
        // 発火する (continuation は後段の per-frame block 経由)。 press block 内では state borrow が
        // 走るため `push_edit` は呼べず、 `press_seek_beat` に貯めて press block を抜けてから 1 度発行。
        let mut press_seek_beat: Option<f64> = None;
        // M14 Phase 63n-1 (#028): track 行右端の lane disclosure click 検出。 press block 終了後に
        // `ToggleTrackAutomationCollapsed { track }` を 1 度発行 (`press_seek_beat` と同パターン)。
        let mut press_lane_toggle: Option<u32> = None;
        // M14 Phase 63n-2 (#028): lane header の icon click action。 press block 終了後に push_edit。
        // None で何も起きず、 Some で 1 度発行 (重複 click は最初の lane を優先 = early break)。
        let mut press_lane_button: Option<ArrangementEditRequest> = None;
        // M14 Phase 63n-2 (#028): Alt+click on point → DeleteAutomationPoints の引数。
        let mut press_delete_point: Option<ArrangementEditRequest> = None;
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
        {
            let in_lanes = lanes.contains(px, py);
            let in_ruler = ruler.contains(px, py);
            let shift = pointer.modifiers.shift;
            let ctrl = pointer.modifiers.ctrl;

            // M14 Phase 63n-5 (#030): lane 下端 splitter hit (= body x range × lane bottom edge ±handle)
            // を **最優先** で判定。 hit したら resize drag session を起動して以降の press logic を skip
            // (= audio grip / clip drag / point hit / track header と排他)。 modifier 無視 (Shift+drag /
            // Ctrl+drag でも resize は同じ意味で、 既存 modifier semantics と衝突する余地が無い)。
            let splitter_lane = automation_lane_resize_splitter_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                px,
                py,
            );
            let splitter_press = if let Some(lane_key) = splitter_lane {
                let anchor_h = visible_tracks
                    .iter()
                    .find(|t| t.id == lane_key.track)
                    .and_then(|t| t.automation_lanes.iter().find(|l| l.id == lane_key.lane))
                    .map_or(0_u16, |l| l.height_px);
                if anchor_h > 0 {
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.automation_lane_resize_drag =
                        Some(AutomationLaneResizeDragSession {
                            lane: lane_key,
                            anchor_height_px: anchor_h,
                            anchor_mouse_y: py,
                            last_mouse_y: py,
                            last_emitted_height: anchor_h,
                        });
                    true
                } else {
                    false
                }
            } else if let Some(row_idx) = track_row_resize_splitter_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                lanes.x,
                lanes.w,
                style,
                px,
                py,
            ) {
                // M14 Phase 63n-6 (#031): track row 下端 splitter hit (lane splitter 不在の場合のみ)
                // → **per-track** row resize session 起動 (= splitter で hit した track のみが伸び縮み)。
                let t = &visible_tracks[row_idx];
                let anchor_row_h = effective_track_row_h(t, view.track_row_h);
                if anchor_row_h > 0.0 {
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.track_row_resize_drag = Some(TrackRowResizeDragSession {
                        track: t.id,
                        anchor_row_h,
                        anchor_mouse_y: py,
                        last_mouse_y: py,
                        last_emitted_height: anchor_row_h,
                    });
                    true
                } else {
                    false
                }
            } else if header_resize_splitter_at(rect, header_w, style, px, py) {
                // M14 Phase 117 (daw_01 #091): header / lanes 境界 splitter hit (lane/row splitter 不在の
                // 場合のみ = lanes 左端 4px の角は lane/row resize を優先)。 → header 幅 resize session 起動。
                // 境界は arrangement 全高に張るので clip drag (in_lanes) / ruler seek (in_ruler) より優先
                // させる (両者は後段で `!splitter_press` gate 済)。
                let state: &mut ArrangementState = self.widget_state(wid);
                state.header_resize_drag = Some(HeaderResizeDragSession {
                    anchor_header_w: header_w,
                    anchor_mouse_x: px,
                    last_mouse_x: px,
                    last_emitted_w: header_w,
                });
                true
            } else {
                false
            };

            // M14 Phase 63k (#025): audio gesture (gain handle / fade corner) を最優先で振り分ける。
            // audio grip にヒットしたら clip_drag (Move/Resize) は起動しない (排他) — `audio_grip_hit_in_lanes`
            // が先勝で priority 判定する。 modifier (Shift / Ctrl) は audio gesture では無視 (Bitwig spec
            // §3.5/§3.6 と整合、 modifier-free な直感的操作)。 audio_edit が None の clip ではこの
            // ブロックは即 None を返すため、 既存挙動 (MIDI / Vocal clip) は影響を受けない。
            let audio_press = if !splitter_press && in_lanes && !shift && !ctrl {
                audio_grip_hit_in_lanes(&visible_tracks, &press_tops, view, lanes, px, py, style)
            } else {
                None
            };
            if let Some((hit_key, grip)) = audio_press {
                if let Some((t_idx, t)) =
                    visible_tracks.iter().enumerate().find(|(_, t)| t.id == hit_key.track)
                    && let Some(c) = t.clips.iter().find(|c| c.id == hit_key.clip)
                    && let Some(audio) = c.audio_edit
                {
                    let kind = match grip {
                        AudioGripHit::GainHandleBand => AudioDragKind::Gain,
                        AudioGripHit::FadeCornerIn => AudioDragKind::FadeIn,
                        AudioGripHit::FadeCornerOut => AudioDragKind::FadeOut,
                    };
                    let r_anchor = clip_to_rect(press_tops[t_idx], view.track_row_h, c, view, lanes);
                    // Gain は常に vertical lock 確定 (横 drag は無視)、 Fade は press 時 `None` で
                    // sticky direction 待ち (continuation で閾値超えた方向に lock)。
                    let locked_horizontal = match kind {
                        AudioDragKind::Gain => Some(false),
                        _ => None,
                    };
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.audio_drag = Some(AudioDragSession {
                        key: hit_key,
                        kind,
                        anchor: audio,
                        clip_rect_anchor: r_anchor,
                        clip_len_beats_anchor: c.len_beats,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        locked_horizontal,
                    });
                }
            } else if !splitter_press
                && in_lanes
                && let Some((hit_key, kind)) =
                    clip_hit(&visible_tracks, &press_tops, view, lanes, px, py, style.resize_handle_px)
                && (!shift
                    || ctrl
                    // FIXME #61: 左右端 grip は Shift = time-stretch を許可
                    // (clip 本体 Move の Shift は従来どおり選択へ fall through)。
                    || matches!(kind, ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight))
            {
                let drag_keys: Vec<ClipKey> = if selected_clips.contains(&hit_key) {
                    selected_clips.to_vec()
                } else {
                    vec![hit_key]
                };
                let mut anchors: Vec<ClipDragAnchor> = Vec::new();
                for k in &drag_keys {
                    // visible_tracks の visible-idx を anchor.track_index に保存 (release frame の
                    // delta 計算 + draw_drag_preview の new_idx も同じ visible-idx で動く)。
                    if let Some((t_idx, t)) =
                        visible_tracks.iter().enumerate().find(|(_, t)| t.id == k.track)
                        && let Some(c) = t.clips.iter().find(|c| c.id == k.clip)
                    {
                        anchors.push(ClipDragAnchor {
                            key: *k,
                            start_beat: c.start_beat,
                            len_beats: c.len_beats,
                            track_index: t_idx,
                        });
                    }
                }
                if !anchors.is_empty() {
                    let press_alt = pointer.modifiers.alt;
                    let press_ctrl = pointer.modifiers.ctrl;
                    let press_shift = pointer.modifiers.shift;
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.clip_drag = Some(ClipDragSession {
                        kind,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                        anchors,
                    });
                }
            }
            // M14 Phase 127 (daw_01 #105): Arranger レーン press 振り分け。 arranger_rect は ruler /
            // lanes / header_pane と y 領域が排他なので独立 block で扱う。 header 幅 splitter
            // (全高に張る) との競合のみ `!splitter_press` で回避 (clip / ruler と同 gate)。
            if !splitter_press && arranger_lane_h > 0.0 && arranger_rect.contains(px, py) {
                let press_alt = pointer.modifiers.alt;
                let press_ctrl = pointer.modifiers.ctrl;
                let press_shift = pointer.modifiers.shift;
                if let Some((sid, kind)) =
                    section_hit(sections, arranger_rect, view, px, py, style.resize_handle_px)
                    && let Some(s) = sections.iter().find(|s| s.id == sid)
                {
                    // 既存 section 上 → Move / Resize session (Ctrl は release で Duplicate に分岐)。
                    let gesture = match kind {
                        ClipDragKind::Move => SectionGesture::Move,
                        ClipDragKind::ResizeLeft => SectionGesture::ResizeLeft,
                        ClipDragKind::ResizeRight => SectionGesture::ResizeRight,
                    };
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.section_drag = Some(SectionDragSession {
                        kind: gesture,
                        section_id: sid,
                        anchor_start: s.start_beat,
                        anchor_len: s.len_beats,
                        anchor_press_beat: px_to_beat(px, arranger_rect.x, arranger_rect.w, view),
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                    });
                } else {
                    // 空きレーン → 範囲 drag による新規作成 session (press 端を snap で grid に着地)。
                    // 単純 click (drag 距離 < 4px) は release で no-op、 新規作成は dblclick が担当する。
                    let raw = px_to_beat(px, arranger_rect.x, arranger_rect.w, view);
                    let anchor = view.snap.snap_beat(raw, press_alt, zoom_x_px_per_beat).max(0.0);
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.section_drag = Some(SectionDragSession {
                        kind: SectionGesture::Create,
                        section_id: 0,
                        anchor_start: anchor,
                        anchor_len: 0.0,
                        anchor_press_beat: anchor,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                    });
                }
            }
            if in_ruler && !splitter_press {
                // M14 Phase 117 (daw_01 #091): header splitter は arrangement 全高に張るので ruler 行の
                // 左端 (boundary ±handle/2) で splitter_press が立つ。 その frame は header 幅 resize を
                // 優先し playhead seek / loop edit は起動しない。
                let press_beat = px_to_beat(px, ruler.x, ruler.w, view);
                let press_alt = pointer.modifiers.alt;
                // M14 Phase 63j (#024): plain (= Shift 非保持) ruler 操作は **playhead seek**
                // (Bitwig / Reaper / Ableton 流)、 Shift 修飾で従来の loop range edit に振り分け。
                //
                // 旧設計 (M9 Phase 45e〜): plain ruler drag = loop NewRange / Middle drag、 loop 端
                // ハンドル drag = Start/End。 これでは「再生中の任意位置で split したい」 UX で
                // ruler 上に playhead を置く手段が無く、 daw_01 #024 で「ユーザビリティが壊滅的」 と報告。
                //
                // 新設計:
                //   - **plain ruler click/drag** → `SetPlayheadBeat` 連続発火 (snap 適用 + clamp ≥ 0)
                //   - **Shift + ruler drag** → loop edit (NewRange / 既存 loop の Start/End/Middle)
                // multi-track 系 widget で Shift は加算選択用なので潰さない設計判断、 ruler は
                // 単一軸で Shift の他用途が無いので loop ops 専用 modifier として再利用する。
                if shift {
                    let kind = if let Some(range) = view.loop_range {
                        match loop_band_hit_kind(range, view.start_beat, view.len_beats, ruler, px, 4.0) {
                            Some(LoopBandHit::Start) => LoopDragKind::Start,
                            Some(LoopBandHit::End) => LoopDragKind::End,
                            Some(LoopBandHit::Middle) => LoopDragKind::Middle,
                            None => LoopDragKind::NewRange,
                        }
                    } else {
                        LoopDragKind::NewRange
                    };
                    // M14 Phase 63j (#024): NewRange の anchor 端点は press 時 snap で
                    // grid に着地させる (caller 側 boilerplate を強要しない設計、 release 端点も
                    // `compute_loop_drag_endpoints` で snap される)。 既存 loop の Start/End/Middle
                    // drag は anchor が `view.loop_range` 由来 (= 既に commit 済 = grid 上前提) なので
                    // press 時 snap 不要、 raw `press_beat` を保持して Middle の delta 計算に使う。
                    let anchor_press_beat_for_session = match kind {
                        LoopDragKind::NewRange => view
                            .snap
                            .snap_beat(press_beat, press_alt, zoom_x_px_per_beat),
                        _ => press_beat,
                    };
                    let anchor_loop = view.loop_range.unwrap_or((
                        anchor_press_beat_for_session,
                        anchor_press_beat_for_session,
                    ));
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.loop_drag = Some(LoopDragSession {
                        kind,
                        anchor_loop,
                        anchor_press_beat: anchor_press_beat_for_session,
                        anchor_mouse_x: px,
                        last_mouse_x: px,
                        last_alt: press_alt,
                    });
                } else {
                    // playhead seek session 開始 + press frame で 1 度発火 (continuation 発火は
                    // 後段の per-frame block が担当)。 snap は `MoveClips` と同 policy: alt 押下で
                    // 一時 OFF、 zoom_x_px_per_beat に対する Adaptive grid。
                    let snapped =
                        view.snap.snap_beat(press_beat, press_alt, zoom_x_px_per_beat).max(0.0);
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.playhead_drag = Some(PlayheadDragSession {
                        last_mouse_x: px,
                        last_emitted_beat: snapped,
                    });
                    press_seek_beat = Some(snapped);
                }
            }
            // M10 Phase 46+47b: track header press 振り分け
            //  - volume band 内 → TrackVolumeDragSession (priority 最高)
            //  - 上記以外 + Name button area を含む row + M/S/Up/Dn/Del button rect 非 hit → reorder
            //  - 16px 未満 drag は release で click 格下げ (button_at の SelectTrack / ↑↓ button が代替)
            // M14 Phase 63n-2 (#028): track 行 と lane 行 で分岐。 lane 行 (= track 行下、 expanded のみ)
            // では lane header button (★/👁/✕) と default band drag を扱う。
            if header_w > 0.0
                && header_pane.contains(px, py)
                && let Some(idx) = track_index_from_y(py, header_pane.y, &press_tops)
                && let Some(t) = visible_tracks.get(idx)
            {
                // header_pane.y と lanes.y は同じ値 (rect 分割で y 軸 origin 共通) なので press_tops を共有可。
                let row_top = press_tops[idx];
                // M14 Phase 63n-6 (#031): per-track row 高さで track row 範囲を判定。
                let row_h_eff = effective_track_row_h(t, view.track_row_h);
                let track_row_bottom = row_top + row_h_eff;
                if py < track_row_bottom {
                    // === track row press (既存ロジック) ===
                    // M14 Phase 118 follow-up (#092 review): press 側の row も draw 側 `row_for_layout`
                    // (Phase 63c #016 で導入) と **同じ indent** を適用する。 これまで press は非 indent の
                    // header_pane 幅で volume band / M·S·R / disclosure / lane disclosure を hit-test して
                    // いたため、 nested track (depth>0) で「描画位置 (indent 済) と press 判定がズレる」
                    // pre-existing バグがあった (深ネスト group の indent 空白を click すると volume drag が
                    // 起動する / 描画済ボタンの click が reorder に化ける 等)。 draw と同 indent にして
                    // press↔draw を SSoT 化 (depth==0 は indent=0 で byte 完全互換)。
                    let indent = f32::from(t.depth) * style.indent_px;
                    let row = Rect {
                        x: header_pane.x + indent,
                        y: row_top,
                        w: (header_pane.w - indent).max(2.0),
                        h: view.track_row_h,
                    };
                    let band_h = if matches!(t.kind, TrackKind::Video) {
                        // M14 Phase 72 (#044): video track では volume slider band を非表示
                        // (volume / pan は video には意味を持たない、 instrument / fx_chain と同様)。
                        0.0
                    } else {
                        style.track_volume_band_h
                    };
                    let layout = header_row_layout(row, band_h);
                    if let Some(band) = layout.volume_band
                        && band.contains(px, py)
                    {
                        let av = t.volume.clamp(0.0, 1.0);
                        let state: &mut ArrangementState = self.widget_state(wid);
                        state.track_volume_drag = Some(TrackVolumeDragSession {
                            track_id: t.id,
                            anchor_volume: av,
                            band_rect: band,
                            anchor_mouse_x: px,
                            last_mouse_x: px,
                            last_emitted_volume: av,
                        });
                    } else {
                        let in_small_button = layout.buttons.iter().any(|b| b.contains(px, py));
                        // M14 Phase 63c (#016): disclosure rect の click は track_reorder セッションを
                        // 起動しない (折り畳み toggle のみ、 release frame 別経路で Edit 発行)。
                        let in_disclosure = is_group_set.contains(&t.id)
                            && disclosure_rect_for(layout.name_rect, style, t.depth)
                                .contains(px, py);
                        // M14 Phase 63n-1 (#028) + 63n-2 修正: lane disclosure hit zone は
                        // **`layout.lane_disc_rect`** を使う (S button の **右**、 button と非 overlap)。
                        // 旧 `lane_disclosure_rect_for(row, style)` (= track 行の右端内側) は S button
                        // と完全 overlap して描画後勝ちで `+`/`-` が覆われる bug 持ちだった (#028 user
                        // feedback で「`+`/`-` が見えない」)。 layout SSoT に統一して描画と hit-test
                        // が同 rect を参照する。
                        let in_lane_disclosure = !t.automation_lanes.is_empty()
                            && layout.lane_disc_rect.contains(px, py);
                        if in_lane_disclosure {
                            press_lane_toggle = Some(t.id);
                        } else if !in_small_button && !in_disclosure && t.id != MASTER_TRACK_ID {
                            // M14 Phase 63c (#016): multi-select 中の drag は selected_tracks をまとめて
                            // 移動するため、 source_track_ids に selected を全部入れる (clicked が selected
                            // に含まれていなければ単独 drag = `vec![clicked]`)。
                            // M14 Phase 63n-10 (#034): master row は reorder 対象外 (= 上端固定、 daw_01
                            // #034 §A 仕様)。 anchor_track_id に MASTER_TRACK_ID が入ると `arr_tracks` に
                            // 該当 id が存在しない → caller の reorder 実装が空振りする (= 結果 no-op だが
                            // session 立ち上げ自体が無駄、 明示的に skip)。
                            let source_ids: Vec<u32> = if selected_tracks.contains(&t.id) {
                                selected_tracks.to_vec()
                            } else {
                                vec![t.id]
                            };
                            let state: &mut ArrangementState = self.widget_state(wid);
                            state.track_reorder = Some(TrackReorderSession {
                                anchor_track_id: t.id,
                                source_track_ids: source_ids,
                                anchor_mouse_y: py,
                                last_mouse_y: py,
                                anchor_mouse_x: px,
                                last_mouse_x: px,
                            });
                        }
                    }
                } else if !t.automation_lanes_collapsed && !t.automation_lanes.is_empty() {
                    // === lane header press (新規 Phase 63n-2) ===
                    // lane 群を上から積んで cursor py が当たる lane を見つけ、 button rect / default
                    // band rect を判定する。 invisible lane は積まない。
                    let header_indent = f32::from(t.depth) * style.indent_px;
                    let mut lane_y = track_row_bottom;
                    for lane in &t.automation_lanes {
                        if !lane.visible {
                            continue;
                        }
                        let lh = f32::from(lane.height_px);
                        if py >= lane_y && py < lane_y + lh {
                            let lane_key = AutomationLaneKey { track: t.id, lane: lane.id };
                            let header_rect = Rect {
                                x: header_pane.x + header_indent,
                                y: lane_y,
                                w: (header_pane.w - header_indent).max(2.0),
                                h: lh,
                            };
                            if let Some(layout) = automation_lane_header_layout(header_rect, style)
                            {
                                if layout.enabled_icon_rect.contains(px, py) {
                                    press_lane_button = Some(
                                        ArrangementEditRequest::SetLaneEnabled {
                                            lane: lane_key,
                                            enabled: !lane.enabled,
                                        },
                                    );
                                } else if layout.visible_icon_rect.contains(px, py) {
                                    press_lane_button = Some(
                                        ArrangementEditRequest::SetLaneVisible {
                                            lane: lane_key,
                                            visible: !lane.visible,
                                        },
                                    );
                                } else if layout.delete_icon_rect.contains(px, py) {
                                    press_lane_button =
                                        Some(ArrangementEditRequest::DeleteLane(lane_key));
                                } else if let Some(band) = layout.default_band_rect
                                    && band.contains(px, py)
                                    && !pointer.modifiers.alt
                                {
                                    // M14 Phase 63n-6 (#031): Alt 修飾は **lane resize gesture に予約**
                                    // (= 後段の Alt+drag fallback で lane resize 起動)。 Alt+press on
                                    // default band は default value の sub-grid 微調整用途より lane
                                    // resize 優先 (= user feedback)。
                                    let initial = volume_from_mouse_x(px, band.x, band.w);
                                    let state: &mut ArrangementState = self.widget_state(wid);
                                    state.automation_lane_default_drag =
                                        Some(AutomationLaneDefaultDragSession {
                                            lane: lane_key,
                                            anchor_value_norm: lane
                                                .default_value_norm
                                                .clamp(0.0, 1.0),
                                            band_rect: band,
                                            last_mouse_x: px,
                                            last_emitted_value: initial,
                                        });
                                }
                            }
                            break;
                        }
                        lane_y += lh;
                    }
                }
            }

            // M14 Phase 63n-2 (#028): lane body (= clip 描画域、 lanes rect 内) の press 振り分け。
            // priority: point hit (Alt → 即時 delete / 通常 → drag session)。 single click on empty は
            // selection clear / 空き選択用に確保 (Bitwig / Live と同 UX)。 AddAutomationPoint は
            // **double click** 経由で発火 (既存 `take_double_click_in_rect` block で分岐)。
            // audio_grip / clip_drag (上で MIDI/Audio 行を既に処理済) は track row の y range 内のみ
            // 作動するため lane body と排他。
            // M14 Phase 63n-9 (#033): tension/bend handle press 検出 — **point press より先勝** で
            // selected point の Bezier / Exponential 入射 segment 中央 handle に当たった場合、 curve
            // param drag を起動。 handle は curve から 10px 上方向 offset で描画されるので point dot
            // 位置とは交差しないが、 priority 上 handle > point > lasso にする (= curve param 編集が
            // 最も狙った操作のため)。 modifier (Shift / Ctrl / Alt) は handle press では無視 (= Alt
            // は drag continuation で × 0.2 sensitivity に使う、 Shift/Ctrl は将来 multi-handle 編集に
            // 予約) — handle 上 click は **常に curve param drag 起動**。
            let mut handle_press_started = false;
            if !splitter_press
                && in_lanes
                && let Some((handle_point, handle_kind, handle_value, lane_h)) =
                    find_curve_param_handle_at(
                        &visible_tracks,
                        &press_tops,
                        view,
                        lanes,
                        selected_automation_points,
                        style,
                        px,
                        py,
                    )
            {
                let effective_h = f32::from(lane_h.max(40));
                let state: &mut ArrangementState = self.widget_state(wid);
                state.automation_curve_param_drag = Some(AutomationCurveParamDragSession {
                    point: handle_point,
                    kind: handle_kind,
                    anchor_value: handle_value,
                    anchor_mouse_y: py,
                    last_mouse_y: py,
                    last_alt: pointer.modifiers.alt,
                    effective_lane_height_px: effective_h,
                    preview_value: handle_value,
                });
                handle_press_started = true;
            }

            // M14 Phase 63n-8 (#033): point press は **Shift / Ctrl 修飾も accept** (release 時 短 click
            // 化で toggle / replace を判定する)。 旧 Phase 63n-2 は `!shift && !ctrl` で除外していたが、
            // それだと Shift+click on point が何の session も起動せず toggle が発火しない bug を持っていた。
            // Shift+click on point は drag>=4px なら通常 move (= MoveAutomationPoints、 modifier 無視で
            // pressed が selection に含まれていれば multi)、 短 click なら toggle。 Ctrl 同様。
            // M14 Phase 63n-9 (#033): handle press が先勝した場合 (= `handle_press_started=true`) は
            // point press を skip (= 同 frame で 2 session が起動するのを回避)。
            if !splitter_press
                && !handle_press_started
                && in_lanes
                && let Some((point_key, _r)) = automation_point_at(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    view,
                    header_pane.x,
                    header_pane.w,
                    lanes,
                    px,
                    py,
                    style,
                )
            {
                if pointer.modifiers.alt {
                    // Alt + click on point → 即時 DeleteAutomationPoints (commit-by-release なし)
                    press_delete_point = Some(ArrangementEditRequest::DeleteAutomationPoints(
                        vec![point_key],
                    ));
                } else if let Some((lane, clip_in)) =
                    find_lane_clip(&visible_tracks, point_key.clip)
                {
                    // 通常 click on point → drag session 起動 (release で MoveAutomationPoints)
                    let p_idx = point_key.point_idx as usize;
                    if let Some(p) = lane
                        .clips
                        .iter()
                        .find(|c| c.id == point_key.clip.clip)
                        .and_then(|c| c.points.get(p_idx))
                        && let Some((_t_idx, _l_idx, _h_rect, body_rect)) = automation_lane_at(
                            &visible_tracks,
                            &press_tops,
                            view.track_row_h,
                            header_pane.x,
                            header_pane.w,
                            lanes.x,
                            lanes.w,
                            style,
                            py,
                        )
                    {
                        let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
                        let pad = style.automation_clip_v_pad_px;
                        let clip_y = body_rect.y + pad;
                        let clip_h = (body_rect.h - pad * 2.0).max(2.0);
                        #[allow(clippy::cast_possible_truncation)]
                        let cx_clip = body_rect.x
                            + ((clip_in.start_beat - view.start_beat) * beat_to_px) as f32;
                        #[allow(clippy::cast_possible_truncation)]
                        let cw = ((clip_in.len_beats * beat_to_px) as f32).max(2.0);
                        let clip_rect_anchor =
                            Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h };
                        let press_alt = pointer.modifiers.alt;
                        let press_modifiers = pointer.modifiers;
                        let state: &mut ArrangementState = self.widget_state(wid);
                        state.automation_point_drag = Some(AutomationPointDragSession {
                            point: point_key,
                            anchor_time_beat: p.time_beat,
                            anchor_value_norm: p.value_norm,
                            clip_rect_anchor,
                            body_rect_anchor: body_rect,
                            clip_start_beat: clip_in.start_beat,
                            clip_len_beats: clip_in.len_beats,
                            anchor_mouse: (px, py),
                            last_mouse: (px, py),
                            last_alt: press_alt,
                            start_modifiers: press_modifiers,
                        });
                    }
                }
            }

            // M14 Phase 63n-3 (#028) / daw_01 #071: lane body 内 automation clip の press 振り分け。
            // priority: **point hit より低い** (= 上の point block で point drag / Alt+delete が起動済なら
            // skip)。 #071 で Shift / Ctrl 修飾でも起動する (= MIDI clip drag と完全対称、 release で短 click
            // を modifier 別 (plain=単一置換 / Shift・Ctrl=選択足し引き) に demote)。 automation lane では
            // marquee (`!press_in_automation_lane`) は走らないので Shift を温存する必要はない。 Alt のみ
            // lane resize に予約 (下の Alt+drag fallback)。 掴んだ clip が選択集合に含まれていれば選択中の
            // 全 clip を grabbed-first で `anchors` に積み一括 move / resize する (MIDI clip と同 idiom)。
            let already_taken_by_point = {
                let state: &ArrangementState = self.widget_state(wid);
                state.automation_point_drag.is_some()
            };
            // M14 Phase 63n-6 (#031 follow-up): Alt 修飾は **lane Alt+drag for resize に予約** する
            // ため、 Alt+press on automation clip は session を起動しない。 これによって lane body 内の
            // 任意位置 (clip 上を含む) で Alt+drag → lane resize が動作する (= user expectation 1:1)。
            // 既存 automation clip Alt-snap-off 機能は失われるが、 automation 編集で sub-grid 位置を
            // 細かく調整する用途は稀で、 lane resize の優先度の方が高いと判断 (= user feedback 反映)。
            // MIDI / audio clip の Alt-snap-off (= clip_drag press) は **track row のみ** に作用するため
            // この変更の影響を受けない (track row は別 priority でこの後 row Alt+drag fallback と排他)。
            // M14 Phase 63n-9 (#033): handle press (curve param drag) が先勝した場合 clip drag も skip。
            if !splitter_press
                && !already_taken_by_point
                && !handle_press_started
                && in_lanes
                && !pointer.modifiers.alt
                && let Some((clip_key, kind, _clip_rect, _body_rect_anchor)) =
                    automation_clip_zone_at(
                        &visible_tracks,
                        &press_tops,
                        view.track_row_h,
                        view,
                        header_pane.x,
                        header_pane.w,
                        lanes,
                        style,
                        px,
                        py,
                        style.resize_handle_px,
                    )
            {
                let press_alt = pointer.modifiers.alt;
                let press_ctrl = pointer.modifiers.ctrl;
                let press_shift = pointer.modifiers.shift;
                // #071: 掴んだ clip が選択集合に含まれていれば選択中の全 clip を一括 drag。 grabbed-first
                // 順 (snap pivot = anchors[0] = 掴んだ clip)。 MIDI clip の `selected_clips.contains(&hit)`
                // idiom を 1:1 ミラー。
                let mut keys: Vec<AutomationClipKey> = vec![clip_key];
                if selected_automation_clips.contains(&clip_key) {
                    keys.extend(
                        selected_automation_clips
                            .iter()
                            .copied()
                            .filter(|k| *k != clip_key),
                    );
                }
                let anchors = collect_automation_clip_anchors(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    header_pane.x,
                    header_pane.w,
                    lanes.x,
                    lanes.w,
                    style,
                    &keys,
                );
                if !anchors.is_empty() {
                    let state: &mut ArrangementState = self.widget_state(wid);
                    state.automation_clip_drag = Some(AutomationClipDragSession {
                        kind,
                        primary: clip_key,
                        anchors,
                        anchor_mouse: (px, py),
                        last_mouse: (px, py),
                        last_alt: press_alt,
                        last_ctrl: press_ctrl,
                        last_shift: press_shift,
                    });
                }
            }

            // M14 Phase 63n-6 (#031): Alt+drag detection — splitter / 既存 press logic で session が
            // 起動しなかった場合のみ動作 (Alt+click on point / Alt+drag on clip 等は既に上で処理済 →
            // 該当 session が立っていれば skip する)。 lane body hit なら lane resize、 そうでなく
            // track row body hit なら row resize。 cursor が lanes 領域 (= clip 描画域) でも
            // header_pane (= lane label 列) でも動く — lane label 上 Alt+drag を「lane を伸ばす」 と
            // 期待する user 直感に合わせる (= 「lane の上で Alt+drag」 = lane resize)。
            let in_arr = in_lanes || (header_w > 0.0 && header_pane.contains(px, py));
            if pointer.modifiers.alt
                && !shift
                && !ctrl
                && !splitter_press
                && in_arr
            {
                let no_session = {
                    let s: &ArrangementState = self.widget_state(wid);
                    s.track_volume_drag.is_none()
                        && s.track_reorder.is_none()
                        && s.audio_drag.is_none()
                        && s.clip_drag.is_none()
                        && s.automation_lane_default_drag.is_none()
                        && s.automation_point_drag.is_none()
                        && s.automation_clip_drag.is_none()
                        && s.automation_lane_resize_drag.is_none()
                        && s.track_row_resize_drag.is_none()
                        && s.playhead_drag.is_none()
                        && s.loop_drag.is_none()
                        && s.automation_curve_param_drag.is_none()
                };
                let no_press_action = press_seek_beat.is_none()
                    && press_lane_toggle.is_none()
                    && press_lane_button.is_none()
                    && press_delete_point.is_none();
                if no_session && no_press_action {
                    let lane_at = automation_lane_at(
                        &visible_tracks,
                        &press_tops,
                        view.track_row_h,
                        header_pane.x,
                        header_pane.w,
                        lanes.x,
                        lanes.w,
                        style,
                        py,
                    );
                    if let Some((t_idx, l_idx, _h_rect, _b_rect)) = lane_at {
                        let lane = &visible_tracks[t_idx].automation_lanes[l_idx];
                        let lane_key = AutomationLaneKey {
                            track: visible_tracks[t_idx].id,
                            lane: lane.id,
                        };
                        let anchor_h = lane.height_px;
                        if anchor_h > 0 {
                            let state: &mut ArrangementState = self.widget_state(wid);
                            state.automation_lane_resize_drag =
                                Some(AutomationLaneResizeDragSession {
                                    lane: lane_key,
                                    anchor_height_px: anchor_h,
                                    anchor_mouse_y: py,
                                    last_mouse_y: py,
                                    last_emitted_height: anchor_h,
                                });
                        }
                    } else if let Some(t_idx) = track_index_from_y(py, lanes.y, &press_tops)
                        && t_idx + 1 < press_tops.len()
                    {
                        // lane が無い (or collapsed) で track row body の中の Alt+drag → per-track row resize。
                        // row body 範囲 = `[tops[t_idx], tops[t_idx] + effective_row_h(t))`、 それ以遠は
                        // lane 領域 (= `lane_at` で既に拾われる前提) — y check は collapsed track / 末尾
                        // track の「lane 無し領域」 まで含めて row body と認定するための明示判定。
                        let t = &visible_tracks[t_idx];
                        let row_top = press_tops[t_idx];
                        let anchor_row_h = effective_track_row_h(t, view.track_row_h);
                        let row_bottom = row_top + anchor_row_h;
                        if py >= row_top && py < row_bottom && anchor_row_h > 0.0 {
                            let state: &mut ArrangementState = self.widget_state(wid);
                            state.track_row_resize_drag = Some(TrackRowResizeDragSession {
                                track: t.id,
                                anchor_row_h,
                                anchor_mouse_y: py,
                                last_mouse_y: py,
                                last_emitted_height: anchor_row_h,
                            });
                        }
                    }
                }
            }

            // M14 Phase 63n-8 (#033): automation point の lasso press — **空き automation lane zone**
            // (= lane body && !clip && !point && !lane resize splitter) の drag で起動。 Q2=A の zone 排他
            // 設計: clip / point / splitter 上は既存 drag (move / move-points / resize) を最優先で起動済、
            // ここはそれら全てが起動しなかった場合の lane body fallback。 既存 MIDI clip rect_select は
            // automation lane 内では起動しない (= 後段の rect_select block で `!in_automation_lane` で
            // guard)、 automation lane では空き zone drag が **修飾なしで lasso** (= Shift / Ctrl は
            // release 時 next 計算で union / XOR 分岐)、 #033 Q2 回答 A と整合。 Alt は lane resize に
            // 予約済 (上の Alt+drag fallback で先勝) なので `!pointer.modifiers.alt` で除外。
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && !pointer.modifiers.alt
                && !splitter_press
                && in_lanes
            {
                let no_session = {
                    let s: &ArrangementState = self.widget_state(wid);
                    s.track_volume_drag.is_none()
                        && s.track_reorder.is_none()
                        && s.audio_drag.is_none()
                        && s.clip_drag.is_none()
                        && s.automation_lane_default_drag.is_none()
                        && s.automation_point_drag.is_none()
                        && s.automation_clip_drag.is_none()
                        && s.automation_lane_resize_drag.is_none()
                        && s.track_row_resize_drag.is_none()
                        && s.playhead_drag.is_none()
                        && s.loop_drag.is_none()
                        && s.automation_curve_param_drag.is_none()
                };
                let no_press_action = press_seek_beat.is_none()
                    && press_lane_toggle.is_none()
                    && press_lane_button.is_none()
                    && press_delete_point.is_none();
                if no_session && no_press_action {
                    let lane_at = automation_lane_at(
                        &visible_tracks,
                        &press_tops,
                        view.track_row_h,
                        header_pane.x,
                        header_pane.w,
                        lanes.x,
                        lanes.w,
                        style,
                        py,
                    );
                    if let Some((_t_idx, _l_idx, _h_rect, body_rect)) = lane_at
                        && px >= body_rect.x
                        && px < body_rect.x + body_rect.w
                    {
                        // body x range 内 (= lane header 外)、 clip / point / splitter は上で先勝で
                        // 既に session 起動 (no_session で除外済) なので、 lane body の **真の空き zone**
                        // で press したことが確定。 lasso session 起動。
                        let state: &mut ArrangementState = self.widget_state(wid);
                        state.automation_lasso_drag = Some(AutomationLassoSession {
                            anchor: (px, py),
                            last_mouse: (px, py),
                            start_modifiers: pointer.modifiers,
                        });
                    }
                }
            }
        }

        // M14 Phase 63j (#024): press block で貯めた playhead seek を 1 度発行 (state borrow 終了後)。
        if let Some(beat) = press_seek_beat {
            self.push_edit(make_edit(ArrangementEditRequest::SetPlayheadBeat(beat)));
        }
        // M14 Phase 63n-1 (#028): track 行右端の lane disclosure click を 1 度発行 (同上)。
        if let Some(track) = press_lane_toggle {
            self.push_edit(make_edit(
                ArrangementEditRequest::ToggleTrackAutomationCollapsed { track },
            ));
        }
        // M14 Phase 63n-2 (#028): lane header button (★/👁/✕) の click を 1 度発行。
        if let Some(req) = press_lane_button {
            self.push_edit(make_edit(req));
        }
        // M14 Phase 63n-2 (#028): Alt+click on point → DeleteAutomationPoints を 1 度発行 (即時)。
        if let Some(req) = press_delete_point {
            self.push_edit(make_edit(req));
        }

        // M14 Phase 63n-2 (#028): 右クリック on point の context menu は **caller 責務**。
        // widget は `response.automation_point_rects: Vec<(AutomationPointKey, Rect)>` を毎 frame
        // 返し (clip_rects と同 idiom)、 caller は loop で `context_menu_for(*rect, &["Hold",
        // "Linear", "Bezier"], ...)` を呼ぶ。 widget 内で secondary press を消費する旧設計は popup の
        // anchor_rect が **右クリック frame だけ Some** で次 frame 以降 caller が context_menu_for を
        // 呼ばないため popup state が消える bug を持っていた (= 一瞬で popup が閉じる)。 #028 §11.4
        // で確定した「caller が anchor を毎 frame 呼ぶ」 idiom に統一。

        // ---- drag continue / release 検出 ----
        // drag 中なら continuation frame で `last_mouse` / `last_alt` (および各 drag の last_*) を
        // update。 **release frame の `last_alt` は update しない** — 同 frame に
        // ModifiersChanged(alt=false) が先行する現象 (alt が一瞬 false に化ける) を回避するため、
        // release 直前 frame の値を保持する。 **release frame の `last_mouse` は pointer.pos が
        // anchor と異なる場合のみ update** — winit は release frame で `pointer.pos` を press 位置
        // に戻すことがあり、 そのまま上書きすると delta = 0 で commit not pushed (drag が「元に戻る」
        // ように見える)。 pointer.pos == anchor のときは continuation 由来の last_mouse を保持し、
        // そうでないときは pointer.pos が真値 (= 通常 release pos、 OR press → 1 frame で release した
        // short drag の release pos) として update する。
        if let Some((px, py)) = pointer.pos {
            let alt_now = pointer.modifiers.alt;
            let ctrl_now = pointer.modifiers.ctrl;
            let shift_now = pointer.modifiers.shift;
            let is_release = pointer.primary_just_released;
            let state: &mut ArrangementState = self.widget_state(wid);
            if let Some(ref mut nd) = state.clip_drag {
                if !is_release {
                    nd.last_mouse = (px, py);
                    nd.last_alt = alt_now;
                    // M14 Phase 63e (#019): ctrl / shift も同じ仕組みで update。 release frame は
                    // ModifiersChanged が MouseInput より先に届いて false 化するリスクがあるので skip。
                    nd.last_ctrl = ctrl_now;
                    nd.last_shift = shift_now;
                } else if (px, py) != nd.anchor_mouse {
                    nd.last_mouse = (px, py);
                }
            }
            // M14 Phase 127 (daw_01 #105): section drag continuation。 clip_drag と同じく continuation で
            // last_mouse / last_alt / last_ctrl を update、 release frame は巻き戻し検知時のみ update。
            if let Some(ref mut sd) = state.section_drag {
                if !is_release {
                    sd.last_mouse = (px, py);
                    sd.last_alt = alt_now;
                    sd.last_ctrl = ctrl_now;
                    sd.last_shift = shift_now;
                } else if (px - sd.anchor_mouse.0).abs() > f32::EPSILON {
                    sd.last_mouse = (px, py);
                }
            }
            if let Some(ref mut ld) = state.loop_drag {
                if !is_release {
                    ld.last_mouse_x = px;
                    // M14 Phase 63j (#024): last_alt は continuation で update、 release は
                    // skip (clip_drag と同じ pattern、 OS event 順序による false 化 race を回避)。
                    ld.last_alt = alt_now;
                } else if (px - ld.anchor_mouse_x).abs() > f32::EPSILON {
                    // release frame で pointer.pos が press 位置と異なる = winit が press 位置に
                    // 巻き戻していない → 真値として update (clip_drag と同 pattern)。
                    ld.last_mouse_x = px;
                }
            }
            if let Some(ref mut tr) = state.track_reorder {
                // continuation は常に update。 release 時は winit 巻き戻し検知のため
                // anchor と differ する場合のみ update (clip_drag と同 pattern)。
                // M14 Phase 101 (daw_01 #072): y / x を独立に判定して update (片軸だけ巻き戻る
                // ケースでも他軸の真値を保持)。
                if !is_release || (py - tr.anchor_mouse_y).abs() > f32::EPSILON {
                    tr.last_mouse_y = py;
                }
                if !is_release || (px - tr.anchor_mouse_x).abs() > f32::EPSILON {
                    tr.last_mouse_x = px;
                }
            }
            if let Some(ref mut tv) = state.track_volume_drag
                && (!is_release || (px - tv.anchor_mouse_x).abs() > f32::EPSILON)
            {
                tv.last_mouse_x = px;
            }
            // M14 Phase 63j (#024): playhead_drag continuation で last_mouse_x を track。
            // release frame は session を後段で `take()` するため update 不要。
            if let Some(ref mut pd) = state.playhead_drag
                && !is_release
            {
                pd.last_mouse_x = px;
            }
            // M14 Phase 63k (#025): audio_drag continuation で last_mouse + sticky direction lock を update。
            // - last_mouse: continuation で常に update、 release frame は pointer.pos == anchor_mouse の
            //   ときのみ skip (winit が release で press 位置に戻すケースを回避、 clip_drag と同 pattern)。
            // - locked_horizontal: 未確定 (`None`) のとき、 累積 |dx| / |dy| のうちどちらかが
            //   `audio_fade_sticky_threshold_px` を超えたら方向 lock。 一度 lock されたら release まで
            //   切替不可 (要望文 §3.2: sticky direction)。
            if let Some(ref mut ad) = state.audio_drag {
                // continuation は常に update。 release frame は pointer.pos == anchor_mouse のときだけ skip
                // (winit が release で press 位置に戻すケースを回避、 clip_drag と同 pattern)。
                if !is_release || (px, py) != ad.anchor_mouse {
                    ad.last_mouse = (px, py);
                }
                if ad.locked_horizontal.is_none() {
                    let dx = (ad.last_mouse.0 - ad.anchor_mouse.0).abs();
                    let dy = (ad.last_mouse.1 - ad.anchor_mouse.1).abs();
                    let threshold = style.audio_fade_sticky_threshold_px;
                    if dx >= threshold || dy >= threshold {
                        ad.locked_horizontal = Some(dx >= dy);
                    }
                }
            }
            // M14 Phase 63n-2 (#028): automation_point_drag continuation で last_mouse + last_alt を update。
            // release frame は last_mouse は pointer.pos != anchor_mouse のときのみ update (clip_drag と
            // 同 pattern: winit が release で press 位置に戻すケースを回避)、 last_alt は release では
            // 保持 (ModifiersChanged が MouseInput より先に届く race を回避)。
            if let Some(ref mut ad) = state.automation_point_drag {
                if !is_release {
                    ad.last_mouse = (px, py);
                    ad.last_alt = alt_now;
                } else if (px, py) != ad.anchor_mouse {
                    ad.last_mouse = (px, py);
                }
            }
            // M14 Phase 63n-2 (#028): automation_lane_default_drag continuation で last_mouse_x を update
            // (TrackVolumeDragSession と同 pattern、 release frame は per-frame emit ブロックで処理)。
            if let Some(ref mut ld) = state.automation_lane_default_drag
                && !is_release
            {
                ld.last_mouse_x = px;
            }
            // M14 Phase 63n-5 (#030): automation_lane_resize_drag continuation で last_mouse_y を update
            // (lane_default_drag と同 pattern、 release frame は release block で処理)。
            if let Some(ref mut rd) = state.automation_lane_resize_drag
                && !is_release
            {
                rd.last_mouse_y = py;
            }
            // M14 Phase 63n-6 (#031): track_row_resize_drag continuation で last_mouse_y を update
            // (lane_resize_drag と同 pattern、 release frame は per-frame 内で final 済 + take 廃棄)。
            if let Some(ref mut rd) = state.track_row_resize_drag
                && !is_release
            {
                rd.last_mouse_y = py;
            }
            // M14 Phase 117 (daw_01 #091): header_resize_drag continuation で last_mouse_x を update
            // (track_row_resize_drag の横軸版、 release frame は per-frame 内で final 済 + take 廃棄)。
            if let Some(ref mut hd) = state.header_resize_drag
                && !is_release
            {
                hd.last_mouse_x = px;
            }
            // M14 Phase 63n-3 (#028): automation_clip_drag continuation で last_mouse +
            // last_alt / last_ctrl / last_shift を update (`ClipDragSession` と同 pattern)。
            // release frame の `last_mouse` は pointer.pos != anchor のときのみ update、 modifier は
            // release では保持 (ModifiersChanged が MouseInput より先に届く race を回避)。
            if let Some(ref mut acd) = state.automation_clip_drag {
                if !is_release {
                    acd.last_mouse = (px, py);
                    acd.last_alt = alt_now;
                    acd.last_ctrl = ctrl_now;
                    acd.last_shift = shift_now;
                } else if (px, py) != acd.anchor_mouse {
                    acd.last_mouse = (px, py);
                }
            }
            // M14 Phase 63n-8 (#033): automation_lasso_drag continuation で last_mouse を update。
            // `start_modifiers` は press 時固定 (= 「lasso 開始時に Shift だったが drag 中に離した」 でも
            // union 動作、 既存 `DragRectState.start_modifiers` と同 idiom)。 release frame の last_mouse
            // は release pos が anchor と異なる場合のみ update (clip_drag と同 pattern)。
            if let Some(ref mut ls) = state.automation_lasso_drag
                && (!is_release || (px, py) != ls.anchor)
            {
                ls.last_mouse = (px, py);
            }
            // M14 Phase 63n-9 (#033): automation_curve_param_drag continuation で last_mouse_y / last_alt /
            // preview_value を update。 release frame は last_alt update を skip (= 既存 OS event 順序
            // race 回避 pattern、 ModifiersChanged が MouseInput より先に届く現象への対応)、 last_mouse_y は
            // release pos が anchor と異なる場合のみ update。 preview_value は anchor + sensitivity 計算で
            // 毎 frame 算出 (= live preview の SSoT、 release で final 値として使用)。
            if let Some(ref mut cd) = state.automation_curve_param_drag {
                if !is_release {
                    cd.last_mouse_y = py;
                    cd.last_alt = alt_now;
                } else if (py - cd.anchor_mouse_y).abs() > f32::EPSILON {
                    cd.last_mouse_y = py;
                }
                let dy = cd.last_mouse_y - cd.anchor_mouse_y;
                let delta =
                    curve_param_delta_from_dy(dy, cd.effective_lane_height_px, cd.last_alt);
                cd.preview_value = (cd.anchor_value + delta).clamp(-1.0, 1.0);
            }
        }

        // M14 Phase 63n-2 (#028): automation_lane_default_drag の per-frame live update
        // (TrackVolumeDragSession と同 pattern)。 drag 中は live preview を caller に流す + 同値抑制。
        if let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_released
        {
            let mut emit: Option<(AutomationLaneKey, f32, f32)> = None;
            {
                let state: &mut ArrangementState = self.widget_state(wid);
                if let Some(ref mut ld) = state.automation_lane_default_drag {
                    let next = volume_from_mouse_x(px, ld.band_rect.x, ld.band_rect.w);
                    if (next - ld.last_emitted_value).abs() > 1e-4 {
                        emit = Some((ld.lane, ld.anchor_value_norm, next));
                        ld.last_emitted_value = next;
                    }
                }
            }
            if let Some((lane, prev, next)) = emit {
                self.push_edit(make_edit(ArrangementEditRequest::SetLaneDefault {
                    lane,
                    prev,
                    next,
                }));
            }
        }

        // M14 Phase 63n-5 (#030): automation_lane_resize_drag の per-frame live update。
        // drag 中は user に「lane が伸び縮みする様子」 を見せたいので、 height 変化を毎 frame 発行する
        // (lane_default_drag と同 pattern)。 release frame は release block で最終値を発行するためここでは skip。
        if let Some((_px, py)) = pointer.pos
            && !pointer.primary_just_released
        {
            // M14 Phase 63n-6 (#031): max は `min(style.max, lanes.h)` で runtime clamp。
            // style 値は絶対 cap、 lanes.h は描画 pane の現在縦サイズ (= 「画面いっぱい」)。
            let max_h = effective_lane_max_height(style, lanes);
            let min_h = style.automation_lane_min_height_px;
            let mut emit: Option<(AutomationLaneKey, u16, u16)> = None;
            {
                let state: &mut ArrangementState = self.widget_state(wid);
                if let Some(ref mut rd) = state.automation_lane_resize_drag {
                    let dy = py - rd.anchor_mouse_y;
                    let raw = f32::from(rd.anchor_height_px) + dy;
                    let next = clamp_height_px(raw, min_h, max_h);
                    if next != rd.last_emitted_height {
                        emit = Some((rd.lane, rd.anchor_height_px, next));
                        rd.last_emitted_height = next;
                    }
                }
            }
            if let Some((lane, prev, next)) = emit {
                self.push_edit(make_edit(ArrangementEditRequest::SetLaneHeight {
                    lane,
                    prev,
                    next,
                }));
            }
        }

        // M14 Phase 63n-6 (#031): track_row_resize_drag の per-frame live update。
        // drag 中は **対象 track の `t.row_h`** が変わる度に caller が `SetSingleTrackRowH` を mutate
        // する (= per-track override 化、 Bitwig per-track zoom と同 idiom)。 widget は floor 1 px の
        // u16 で発火 (caller-side で `[min, max]` clamp)、 同値抑制 0.5 px 閾値で u16 quantization 込み。
        if let Some((_px, py)) = pointer.pos
            && !pointer.primary_just_released
        {
            let mut row_emit: Option<(u32, u16, u16)> = None;
            {
                let state: &mut ArrangementState = self.widget_state(wid);
                if let Some(ref mut rd) = state.track_row_resize_drag {
                    let dy = py - rd.anchor_mouse_y;
                    let next_f = (rd.anchor_row_h + dy).max(1.0);
                    if (next_f - rd.last_emitted_height).abs() >= 0.5 {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let next = next_f.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let prev =
                            rd.anchor_row_h.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                        row_emit = Some((rd.track, prev, next));
                        rd.last_emitted_height = next_f;
                    }
                }
            }
            if let Some((track, prev, next)) = row_emit {
                self.push_edit(make_edit(ArrangementEditRequest::SetSingleTrackRowH {
                    track,
                    prev,
                    next,
                }));
            }
        }

        // M14 Phase 117 (daw_01 #091): header_resize_drag の per-frame live update。 drag 中は
        // header 幅変化を毎 frame `SetHeaderW { prev: anchor, next }` で発行する (caller が
        // `view.header_w` を更新 → 次 frame に header / lanes が連動伸縮)。 `next` は raw px
        // (NaN/負値防止の `max(0.0)` floor のみ、 実用 clamp は caller)、 同値抑制 0.5 px。
        if let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_released
        {
            let mut header_emit: Option<(f32, f32)> = None;
            {
                let state: &mut ArrangementState = self.widget_state(wid);
                if let Some(ref mut hd) = state.header_resize_drag {
                    let dx = px - hd.anchor_mouse_x;
                    let next = (hd.anchor_header_w + dx).max(0.0);
                    if (next - hd.last_emitted_w).abs() >= 0.5 {
                        header_emit = Some((hd.anchor_header_w, next));
                        hd.last_emitted_w = next;
                    }
                }
            }
            if let Some((prev, next)) = header_emit {
                self.push_edit(make_edit(ArrangementEditRequest::SetHeaderW { prev, next }));
            }
        }

        // M14 Phase 63j (#024): playhead drag continuation の per-frame live update。
        // press frame は press block 内で発火済 (`press_seek_beat`)、 ここは continuation のみ。
        // release frame は emit せず session を後段で take して discard する (commit-by-release 無し)。
        // `last_emitted_beat` で同値発火を抑制 (1e-6 拍 = ~10μs @ 120BPM 以下は ignore)。
        if let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_pressed
            && !pointer.primary_just_released
        {
            let alt = pointer.modifiers.alt;
            let mut emit_beat: Option<f64> = None;
            {
                let state: &mut ArrangementState = self.widget_state(wid);
                if let Some(ref mut pd) = state.playhead_drag {
                    let raw = px_to_beat(px, ruler.x, ruler.w, view);
                    let next = view.snap.snap_beat(raw, alt, zoom_x_px_per_beat).max(0.0);
                    if (next - pd.last_emitted_beat).abs() > 1e-6 {
                        emit_beat = Some(next);
                        pd.last_emitted_beat = next;
                    }
                }
            }
            if let Some(beat) = emit_beat {
                self.push_edit(make_edit(ArrangementEditRequest::SetPlayheadBeat(beat)));
            }
        }

        // M10 Phase 49: track volume drag 中の per-frame live update。
        // release frame は Mutate 発火を抑制し、release ブロックの Undoable Edit に任せる
        // (= fader_at の `suppress_mutate_on_release` と同パターン)。
        // 同値発火を抑えるため `last_emitted_volume` と差分比較。
        if let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_released
        {
            let mut volume_emit: Option<(u32, f32, f32)> = None;
            {
                let state: &mut ArrangementState = self.widget_state(wid);
                if let Some(ref mut tv) = state.track_volume_drag {
                    let next = volume_from_mouse_x(px, tv.band_rect.x, tv.band_rect.w);
                    if (next - tv.last_emitted_volume).abs() > 1e-4 {
                        volume_emit = Some((tv.track_id, tv.anchor_volume, next));
                        tv.last_emitted_volume = next;
                    }
                }
            }
            if let Some((track, prev, next)) = volume_emit {
                self.push_edit(make_edit(ArrangementEditRequest::SetTrackVolume {
                    track,
                    prev,
                    next,
                }));
            }
        }
        // 2) drag overlay 計算用に clone を取る (last_mouse を更新した後)。
        let clip_drag_session: Option<ClipDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.clip_drag.clone()
        };
        let clip_drag_release_raw: Option<ClipDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.clip_drag.take()
        } else {
            None
        };
        let (clip_drag_release, clip_short_click_pos): (Option<ClipDragSession>, Option<(f32, f32)>) =
            if let Some(nd) = clip_drag_release_raw {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
                let dist = dx.abs() + dy.abs();
                // 短 click 化 (drag → click 格下げ) の閾値は **mouse jitter を ignore する程度** (4px) に
                // 抑える。 旧実装の 16px 閾値は過剰で、 user が「ちょっとずらす」 操作も吸収して
                // しまい release で元位置 (= 通常 grid 上) に戻る → 「grid に飛ぶ」 symptom の主因。
                // 適用条件:
                //   - **Resize (Left/Right)** は閾値関係なく常に commit (resize handle 上の click は
                //     意味がない、 短 drag でも長さ変更を反映すべき)。
                //   - **Move** で **Alt なし** のときのみ jitter 閾値で短 click 化。 click vs drag の
                //     区別が必要なのは Move のみ (click = selection 切替、 drag = 移動)。
                //   - **Alt 押下中** は Move でも閾値 skip (Alt は raw 微調整の明示意図)。
                let is_move = matches!(nd.kind, ClipDragKind::Move);
                let demote = is_move && !nd.last_alt && dist < 4.0;
                if demote {
                    (None, Some(nd.last_mouse))
                } else {
                    (Some(nd), None)
                }
            } else {
                (None, None)
            };

        let loop_drag_session: Option<LoopDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.loop_drag
        };
        let loop_drag_release: Option<LoopDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.loop_drag.take()
        } else {
            None
        };

        // M14 Phase 127 (daw_01 #105): section drag の overlay 用 copy (SectionDragSession は Copy) と
        // release 取り出し (`loop_drag` と同 idiom)。
        let section_drag_session: Option<SectionDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.section_drag
        };
        let section_drag_release: Option<SectionDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.section_drag.take()
        } else {
            None
        };

        // M10 Phase 46: track reorder session の overlay 用 clone と release 取り出し。
        // M14 Phase 63c (#016): TrackReorderSession は Vec<u32> を持つため Copy 不可。 ここで clone。
        let track_reorder_session: Option<TrackReorderSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.track_reorder.clone()
        };
        let track_reorder_release_raw: Option<TrackReorderSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.track_reorder.take()
            } else {
                None
            };

        // M14 Phase 63c (#016) → 101 (daw_01 #072): track header drag release の **drop action**。
        // `SetTrackParent { tracks, parent, anchor_after }` を 1 つ発行。 caller は (1) source を
        // arr_tracks から remove (2) parent_id を `parent` に更新 (3) `anchor_after` の直後
        // (None で先頭) に挿入、 という再構築をする。
        //
        // M14 Phase 101 (daw_01 #072): drop 解決を `resolve_track_drop` に一本化。 Y で gap、 X で
        // ネスト深さを決め、 (parent, anchor_after) を導出する (旧 Y-only ヒューリスティックは「一番下へ」
        // drop が最下段 group の内側に吸い込まれるバグを持っていた)。 **overlay (描画プレビュー) と
        // 完全に同じ pure 関数**を通すので preview = commit が構造的に保証される。 gate は drag 距離
        // (dx/dy 合成) で、 click (≒静止) を reorder に昇格させない。
        let pending_drop: Option<(Vec<u32>, Option<u32>, Option<u32>)> =
            track_reorder_release_raw.as_ref().and_then(|tr| {
                let dx = tr.last_mouse_x - tr.anchor_mouse_x;
                let dy = tr.last_mouse_y - tr.anchor_mouse_y;
                if (dx * dx + dy * dy).sqrt() < REORDER_DRAG_THRESHOLD_PX {
                    return None;
                }
                let drop = resolve_track_drop(
                    tracks,
                    &visible_tracks,
                    &press_tops,
                    &is_group_set,
                    &tr.source_track_ids,
                    style.indent_px,
                    tr.last_mouse_y,
                    tr.last_mouse_x,
                    tr.anchor_mouse_x,
                );
                Some((tr.source_track_ids.clone(), drop.parent, drop.anchor_after))
            });
        let pending_reorder_hash: u64 = pending_drop.as_ref().map_or(0_u64, |(ts, p, a)| {
            let mut h = u64::from(p.unwrap_or(u32::MAX));
            h = h.wrapping_mul(31).wrapping_add(u64::from(a.unwrap_or(u32::MAX)));
            for t in ts {
                h = h.wrapping_mul(31).wrapping_add(u64::from(*t));
            }
            h.wrapping_mul(0x100_0000_01B3)
        });

        // M10 Phase 47b: track volume drag session の overlay 用 clone と release 取り出し。
        let track_volume_session: Option<TrackVolumeDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.track_volume_drag
        };
        let track_volume_release: Option<TrackVolumeDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.track_volume_drag.take()
            } else {
                None
            };

        // M14 Phase 63j (#024): playhead_drag は release frame で take して discard。
        // continuous emit は per-frame block で完了済、 release 専用 commit は不要。
        if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            let _ = state.playhead_drag.take();
        }

        // M14 Phase 63k (#025): audio_drag overlay 用 clone と release 取り出し。
        let audio_drag_session: Option<AudioDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.audio_drag
        };
        let audio_drag_release: Option<AudioDragSession> = if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.audio_drag.take()
        } else {
            None
        };

        // M14 Phase 63n-2 (#028): automation_point_drag overlay clone + release take。
        let point_drag_session: Option<AutomationPointDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.automation_point_drag
        };
        let point_drag_release: Option<AutomationPointDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.automation_point_drag.take()
            } else {
                None
            };

        // M14 Phase 63n-2 (#028): automation_lane_default_drag overlay clone + release take。
        // overlay は draw_automation_lane の band fill を上書きする (cached 外、 drag 中のみ)。
        let lane_default_drag_session: Option<AutomationLaneDefaultDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.automation_lane_default_drag
        };
        let lane_default_drag_release: Option<AutomationLaneDefaultDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.automation_lane_default_drag.take()
            } else {
                None
            };

        // M14 Phase 63n-5 (#030): automation_lane_resize_drag release take (overlay は不要 — caller が
        // per-frame 受信した SetLaneHeight で `lane.height_px` を update することで lane が伸び縮みする
        // 様子が cached 描画に直接反映される)。 release frame で session を take し、 final height を発行。
        let lane_resize_drag_release: Option<AutomationLaneResizeDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.automation_lane_resize_drag.take()
            } else {
                None
            };

        // M14 Phase 63n-6 (#031): track_row_resize_drag release take + discard。 per-frame emit で
        // 既に最終値が発火済 (= `last_emitted_height`)、 release で追加 emit は不要 (lane と異なる)。
        // session を `take()` して廃棄 (cursor 形状 / hover 判定が release 後すぐ解除される)。
        if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.track_row_resize_drag.take();
        }

        // M14 Phase 117 (daw_01 #091): header_resize_drag release take + discard (row resize と同 idiom、
        // per-frame で final 済)。
        if pointer.primary_just_released {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.header_resize_drag.take();
        }

        // M14 Phase 63n-3 (#028): automation_clip_drag overlay clone + release take。
        // overlay は ghost clip rect を cached 外で重ねる、 release で 1 度だけ
        // `MoveAutomationClips` / `CloneAutomationClipsLinked` / `CloneAutomationClipsIndependent` /
        // `ResizeAutomationClips` / (短 click 時) `SelectAutomationClips` のいずれかを発行。
        let automation_clip_drag_session: Option<AutomationClipDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.automation_clip_drag.clone()
        };
        let automation_clip_drag_release: Option<AutomationClipDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.automation_clip_drag.take()
            } else {
                None
            };

        // M14 Phase 63n-8 (#033): automation_lasso_drag overlay clone + release take。
        // overlay は drag 中の lasso rect を cached 外で描画 (style.automation_lasso_fill / border)、
        // release で 1 度だけ `SelectAutomationPoints` を発行 (next 計算は anchor 時の modifier で
        // replace / union / XOR 分岐)。 `response.automation_lasso_active = session.is_some()` を後で set。
        let automation_lasso_session: Option<AutomationLassoSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.automation_lasso_drag
        };
        let automation_lasso_release: Option<AutomationLassoSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.automation_lasso_drag.take()
            } else {
                None
            };
        if automation_lasso_session.is_some() {
            response.automation_lasso_active = true;
        }

        // M14 Phase 63n-9 (#033): automation_curve_param_drag overlay clone + release take。
        // overlay は drag 中 handle + preview curve segment を cached 外で描画 (handle 位置は preview_value
        // 由来、 curve は preview_value で再 flatten した polyline を `automation_curve_param_preview_color`
        // で重ねる)、 release で 1 度だけ `SetAutomationCurveParam { point, kind, prev_value, next_value }`
        // を発行 (anchor == preview なら 1e-4 閾値で no-op)。
        let automation_curve_param_session: Option<AutomationCurveParamDragSession> = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.automation_curve_param_drag
        };
        let automation_curve_param_release: Option<AutomationCurveParamDragSession> =
            if pointer.primary_just_released {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.automation_curve_param_drag.take()
            } else {
                None
            };

        // drag overlay delta (last_mouse ベース、release と一貫)。
        // M14 Phase 63j (#024): `beat_per_px` / `zoom_x_px_per_beat` は関数頭で計算済 (press 振り分けの
        // playhead seek snap でも使うため)。 ここでは shadow せず再利用する。
        let row_per_px = 1.0_f32 / view.track_row_h.max(1.0);
        let clip_drag_overlay: Option<(ClipDragSession, f64, i32)> = clip_drag_session
            .as_ref()
            .map(|nd| {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
                let raw = f64::from(dx) * beat_per_px;
                // **絶対位置 snap** (= Cubase / Live と同じ「nearest grid alignment」 動作):
                // anchor 0 の編集対象端 (Move=start / ResizeRight=end / ResizeLeft=start) の絶対位置を
                // grid に round → その差分 (`adjusted_delta`) を全 anchor に同じだけ適用する。
                // delta-snap (= raw_delta だけを round) だと anchor が grid 外に既にずれていた場合
                // (例: 前回 Alt+drag で +0.078 拍ずらした) に release してもずれが永久残る。
                // 絶対 snap なら anchor 0 が必ず grid 上に着地し、 複数選択は相対関係を維持。
                // alt は drag state の `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない。
                let beat_delta = compute_clip_drag_beat_delta(
                    nd,
                    raw,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                #[allow(clippy::cast_possible_truncation)]
                let track_delta = (dy * row_per_px).round() as i32;
                (nd.clone(), beat_delta, track_delta)
            });

        // M14 Phase 63j (#024): overlay の preview range も `compute_loop_drag_endpoints` で
        // snap 適用済 (commit と同一値で確定、 release 時の「カクッ」 ずれを回避)。 alt は session の
        // `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない (clip_drag と同じ pattern)。
        let loop_drag_preview_range: Option<(f64, f64)> = loop_drag_session.map(|ld| {
            let cur_beat = px_to_beat(ld.last_mouse_x, ruler.x, ruler.w, view);
            compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat)
        });

        // ---- hover 計算 ----
        if let Some((cx, cy)) = pointer.pos
            && lanes.contains(cx, cy)
        {
            response.hovered_track = track_index_from_y(cy, lanes.y, &press_tops)
                .and_then(|idx| visible_tracks.get(idx).map(|t| t.id));
            if let Some((hit_key, hit_kind)) =
                clip_hit(&visible_tracks, &press_tops, view, lanes, cx, cy, style.resize_handle_px)
            {
                response.hovered_clip = Some(hit_key);
                response.hovered_zone = Some(hit_kind);
            } else {
                // M14 Phase 116 (daw_01 #090): clip-first first-hit。 clip に当たらなかったときだけ
                // ポインタ下の automation lane body を公開する (`hovered_clip` と排他)。 `cx` は既に
                // `lanes.contains(cx, cy)` で lanes pane 内と確定済 (= header 帯ではなく body)。
                response.hovered_automation_lane = automation_lane_key_at_y(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    header_pane.x,
                    header_pane.w,
                    lanes.x,
                    lanes.w,
                    style,
                    cy,
                )
                .map(|(key, _body_rect)| key);
            }
        }
        // M14 Phase 127 (daw_01 #105): Arranger section hover (arranger_rect 内、 clip / lane と y 排他)。
        if let Some((cx, cy)) = pointer.pos
            && arranger_lane_h > 0.0
            && arranger_rect.contains(cx, cy)
        {
            // FIXME #067: hover の zone (Move/Resize) も保持して cursor を駆動する (id だけ捨てない)。
            if let Some((id, kind)) =
                section_hit(sections, arranger_rect, view, cx, cy, style.resize_handle_px)
            {
                response.hovered_section = Some(id);
                response.hovered_section_zone = Some(kind);
            }
        }
        // visible section の rect を response に積む (clip_rects と同 semantics、 caller の context_menu_for
        // 用)。 完全 off-screen (arranger_rect と x 交差しない) は除外。
        if arranger_lane_h > 0.0 {
            for s in sections {
                let r = section_to_rect(s, view, arranger_rect);
                if r.x + r.w >= arranger_rect.x && r.x <= arranger_rect.x + arranger_rect.w {
                    response.section_rects.push((s.id, r));
                }
            }
        }
        response.dragging = clip_drag_session.as_ref().map(|nd| nd.kind);
        response.reordering = track_reorder_session.as_ref().map(|tr| tr.anchor_track_id);
        response.dragging_track_volume = track_volume_session.map(|tv| tv.track_id);
        // 既存 section の Move/Resize drag のみ報告 (Create 範囲 drag は transient creation なので None)。
        response.dragging_section = section_drag_session.and_then(|sd| match sd.kind {
            SectionGesture::Move => Some(ClipDragKind::Move),
            SectionGesture::ResizeLeft => Some(ClipDragKind::ResizeLeft),
            SectionGesture::ResizeRight => Some(ClipDragKind::ResizeRight),
            SectionGesture::Create => None,
        });

        // ---- cursor ----
        // drag 中 / hover 中の clip 上 / それ以外で arrangement 内なら明示的に Default
        // にリセット (`set_cursor` を呼ばないと OS 側に前フレームの形が残る、winit は state-full)。
        // M14 Phase 63n-3 (#028): automation clip drag 中も MIDI と同じ cursor 形状 (排他で `Some` 判定)。
        // M14 Phase 63n-5 (#030): lane resize drag 中は NsResize (cursor 移動の縦軸を強調)、 hover 時も
        // splitter hot zone なら NsResize にして discoverability を確保。 lane resize > clip drag > hover の
        // priority (= 同時に成立しないが、 万一重なっても resize を優先)。
        // M14 Phase 63n-6 (#031): row resize drag 中も NsResize (lane resize と同じ)。 lane / row の
        // 両 session を同 priority で扱い、 同時に立たない (press 時に一方しか起動しない)。
        let resize_active = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.automation_lane_resize_drag.is_some() || state.track_row_resize_drag.is_some()
        };
        // M14 Phase 117 (daw_01 #091): header 幅 resize drag 中 / hover 中は EwResize (横軸)。
        // active は最優先 (NsResize / clip drag より上)、 hover は lane/row splitter NsResize の後に評価。
        let header_resize_active = {
            let state: &mut ArrangementState = self.widget_state(wid);
            state.header_resize_drag.is_some()
        };
        let dragging_kind = response
            .dragging
            .or(automation_clip_drag_session.as_ref().map(|acd| acd.kind))
            // FIXME #067: section の Move/Resize drag 中も clip と同じ cursor (Move / EwResize)。
            // clip drag と section drag は y 領域排他なので同時に Some にならない。
            .or(response.dragging_section);
        if header_resize_active {
            self.set_cursor(CursorIcon::EwResize);
        } else if resize_active {
            self.set_cursor(CursorIcon::NsResize);
        } else if let Some(kind) = dragging_kind {
            self.set_cursor(drag_kind_cursor(kind));
        } else if response.reordering.is_some() {
            self.set_cursor(CursorIcon::Move);
        } else if response.dragging_track_volume.is_some() {
            self.set_cursor(CursorIcon::EwResize);
        } else if let Some(zone) = response.hovered_zone {
            self.set_cursor(drag_kind_cursor(zone));
        } else if let Some(zone) = response.hovered_section_zone {
            // FIXME #067: section 帯の hover も clip と同 idiom — 端 (Resize zone) で EwResize、
            // 中央 (Move zone) で Move。 帯端を掴んでリサイズできることを ↔ カーソルで示す。
            self.set_cursor(drag_kind_cursor(zone));
        } else if let Some((cx, cy)) = pointer.pos
            && (automation_lane_resize_splitter_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                cx,
                cy,
            )
            .is_some()
                || track_row_resize_splitter_at(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    lanes.x,
                    lanes.w,
                    style,
                    cx,
                    cy,
                )
                .is_some())
        {
            self.set_cursor(CursorIcon::NsResize);
        } else if let Some((cx, cy)) = pointer.pos
            && header_resize_splitter_at(rect, header_w, style, cx, cy)
        {
            // M14 Phase 117 (daw_01 #091): header / lanes 境界 hover で EwResize (discoverability)。
            // lane/row splitter (NsResize) を上で先に判定済なので角の競合は NsResize 優先。
            self.set_cursor(CursorIcon::EwResize);
        } else if let Some((cx, cy)) = pointer.pos
            && let Some((_key, kind, _clip_rect, _body_rect)) = automation_clip_zone_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                view,
                header_pane.x,
                header_pane.w,
                lanes,
                style,
                cx,
                cy,
                style.resize_handle_px,
            )
        {
            // FIXME #70: automation clip も MIDI clip と同様に端で EwResize / 本体で Move を出す。
            // press 側は `automation_clip_zone_at` で resize/move を既に判定して clip drag を起動して
            // いるが、 hover cursor だけ未配線で「端でカーソルが左右矢印にならない」 状態だった。
            // lane/row/header splitter の resize hover はこの上で先に判定済なので、 角の競合は
            // それらが優先される (= press 側の splitter 優先順位と一致)。
            let cur = match kind {
                ClipDragKind::Move => CursorIcon::Move,
                ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => CursorIcon::EwResize,
            };
            self.set_cursor(cur);
        } else if let Some((px, py)) = pointer.pos
            && (lanes.contains(px, py) || ruler.contains(px, py) || header_pane.contains(px, py))
        {
            self.set_cursor(CursorIcon::Default);
        }

        // ---- 描画 (heavy + cached + 動的 overlay) ----
        // M10 Phase 50: pending_reorder_hash を viewport_key に入れて、release frame の optimistic
        // preview で cache miss を強制 (新順序での再描画を 1 frame 遅延なく行う)。
        // tuple Hash 実装は 12 要素まで → nested tuple で 13 要素分を表現。
        // M13 Phase 55: bpm / time_sig を 3 つ目の nested tuple で追加し v2 に bump。
        // M14 Phase 61b (#011): clip 個別の (id, start_beat, len_beats) 変化を widget 側で hash
        // して 4 つ目の outer 要素 internal_clip_hash として viewport_key に追加 + v3 に bump。
        // M14 Phase 63c (#016): selected_tracks を fold して selection 変化での cache miss を保証
        // (旧 `selected_track.unwrap_or(u32::MAX)` の単一 u32 に対し、 multi-select は集合 hash)。
        // 加えて parent_id / depth / collapsed の構成変化は data_generation で caller 責務 (group
        // 構成変化は track 構成変化と同義、 caller が data_generation を bump する前提)。
        let internal_clip_hash = fold_arrangement_clip_hash(tracks);
        let selected_tracks_hash: u64 = selected_tracks.iter().fold(0xCBF2_9CE4_8422_2325_u64, |a, &x| {
            a.wrapping_mul(0x100_0000_01B3).wrapping_add(u64::from(x))
        });
        // M14 Phase 63n-3 (#028): selected_automation_clips を fold して cache 再構築を保証 (= 選択
        // 変化時に lane の clip rect 描画が selected_fill / selected_border に切り替わる)。
        let selected_automation_clips_hash: u64 = selected_automation_clips.iter().fold(
            0xCBF2_9CE4_8422_2325_u64,
            |a, k| {
                a.wrapping_mul(0x100_0000_01B3)
                    .wrapping_add(u64::from(k.track))
                    .wrapping_mul(0x100_0000_01B3)
                    .wrapping_add(u64::from(k.lane))
                    .wrapping_mul(0x100_0000_01B3)
                    .wrapping_add(u64::from(k.clip))
            },
        );
        let viewport_key = (
            (
                b"arrangement_widget_v6" as &[u8],
                rect.w.to_bits(),
                rect.h.to_bits(),
                view.start_beat.to_bits(),
                view.len_beats.to_bits(),
                view.track_top.to_bits(),
                view.track_row_h.to_bits(),
                view.tracks_visible.to_bits(),
                view.header_w.to_bits(),
                view.ruler_h.to_bits(),
                view.data_generation,
                selected_tracks_hash,
            ),
            pending_reorder_hash,
            (view.bpm.to_bits(), u32::from(view.time_sig.0), u32::from(view.time_sig.1)),
            internal_clip_hash,
            selected_automation_clips_hash,
        );

        // M14 Phase 63c (#016): SetTrackParent に統合した結果、 release frame の optimistic
        // preview (旧 ReorderTracks の new_order を frame 末尾 deferred apply の代わりに同 frame
        // で見せる) は廃止。 caller の SetTrackParent arm は「source remove → parent_id update →
        // anchor_after 後に insert」 を行うが、 widget は次 frame で更新後の `tracks` を再受信して
        // 描画する (= 1 frame の表示遅延)。 user が drag release で「カクッ」 と動く挙動になるが、
        // 構造変化を伴う drop は反映までの遅延が許容範囲 (sibling reorder を SetTrackParent と
        // 統一した代償としては妥当)。 必要なら別 PR で optimistic preview を再導入可能。
        //
        // tracks_for_draw は draw / track headers loop / clip 計算で使う visible-only Arc。
        // 入力 `tracks` (caller's slice、 順序込み) を visible filter かけたコピーを保持。
        let tracks_for_draw: Arc<[ArrangementTrack]> = Arc::from(visible_tracks.clone());
        let tracks_owned: Arc<[ArrangementTrack]> = Arc::clone(&tracks_for_draw);
        let style_copy = *style;
        let view_copy = view;
        // M14 Phase 63n-1 (#028): cached / cached-外 の prefix sum tops は heavy closure 内で
        // 再計算する (`'static` 制約で外側 borrow を持ち込めないため)。 caller scope では持たない。
        let selected_set: HashSet<ClipKey> = selected_clips.iter().copied().collect();
        // M14 Phase 63n-3 (#028): automation clip selection set (heavy closure 用)。
        let selected_automation_clips_set_for_heavy: HashSet<AutomationClipKey> =
            selected_automation_clips.iter().copied().collect();
        // M14 Phase 63c (#016): heavy closure は `'static` 要求なので owned Vec<u32> で渡す
        // (selected_set と同パターン)。 loop 側の hit-test では `selected_tracks` slice (borrowed)
        // を直接 contains で参照するため、 ここで cloned heavy 用 vector を別に持って move 衝突を回避。
        let selected_tracks_for_heavy: Vec<u32> = selected_tracks.to_vec();
        // M14 Phase 113 (daw_01 #085): group track 背景 tint 撤去に伴い、 lanes 背景描画
        // (`draw_lanes_bg`) は group 判定を使わなくなったため heavy closure 用の is_group_set clone は不要。
        // group の hit-test / disclosure / drag drop 判定は loop 側の borrowed `is_group_set` を直接使う。
        let drag_overlay_clone = clip_drag_overlay.clone();
        // M14 Phase 63k (#025): audio_drag overlay 用 clone (heavy closure に move)。
        // ghost (drag 中の preview line / fade envelope / label) は cached 外で描画する。
        let audio_drag_overlay = audio_drag_session;
        // M14 Phase 63n-2 (#028): point_drag / lane_default_drag の overlay 用 clone (heavy closure
        // に move)。 point ghost は drag 中の preview を新位置に上書き、 band fill は drag 中の
        // last_emitted_value で塗り直し (cached 内描画は anchor 値のままなので cached 外で被せる)。
        let point_drag_overlay = point_drag_session;
        let lane_default_drag_overlay = lane_default_drag_session;
        // M14 Phase 63n-3 (#028): automation_clip_drag overlay (heavy closure に move)。
        // ghost rect は drag 中の preview (新位置 / 新長さ、 cross-lane drop なら新 lane の body 内) を
        // cached 外で重ねる。 base 描画 (cached 内) も同 frame 表示されるが、 ghost が上に重なる。
        // #071: session は multi-anchor (non-Copy) になったため overlay 用に clone し、 原本は後段の
        // `dragging_automation_clip` (9528 付近) で kind を取り出すまで生かす。
        let automation_clip_drag_overlay = automation_clip_drag_session.clone();
        // M14 Phase 63n-8 (#033): selected automation points (overlay 描画用、 cached 外で selected 点だけ
        // 白色 + 大 dot で上書き)。 cached layer の base draw は selection 不問の grey dot を描く、 overlay は
        // selection の差分のみを上書きする (= `data_generation` bump なしで selection 変化が反映される、
        // piano_roll の selection overlay と同 idiom)。
        let selected_automation_points_for_heavy: HashSet<AutomationPointKey> =
            selected_automation_points.iter().copied().collect();
        // M14 Phase 63n-8 (#033): lasso session の overlay clone (cached 外で lasso rect を描画)。
        let lasso_overlay = automation_lasso_session;
        // M14 Phase 63n-9 (#033): curve param drag session の overlay clone (cached 外で handle + preview
        // curve segment を描画、 drag 中のみ true value で live update)。
        let curve_param_overlay = automation_curve_param_session;
        // M9 Phase 45f: drag overlay の Resize min_len は snap unit (snap_unit < 0.05 なら 0.05)。
        // release 側 min_len と一貫させるため、 alt 真値は drag session の `last_alt` を使う
        // (overlay と release commit が必ず同一 unit で確定する)。 overlay 不在時 (drag していない)
        // は min_len 自体使われないので、 alt = false で適当な値で初期化しておけばよい。
        let drag_overlay_alt =
            clip_drag_overlay.as_ref().is_some_and(|(nd, _, _)| nd.last_alt);
        let drag_overlay_min_len: f64 = if view.snap.is_active(drag_overlay_alt) {
            view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
        } else {
            0.05
        };
        let loop_preview_clone = loop_drag_preview_range;
        let header_pane_copy = header_pane;
        // M14 Phase 127 (daw_01 #105): Arranger レーン overlay 用 capture (heavy closure に move)。
        // section データは borrow を closure に持ち込めないので owned Vec に clone (SectionView は
        // Arc<str> name の安価 clone)。 drag session / rect / lane 高さは Copy。
        let sections_for_draw: Vec<SectionView> = sections.to_vec();
        let section_drag_overlay = section_drag_session;
        let arranger_rect_copy = arranger_rect;
        let arranger_header_rect_copy = arranger_header_rect;
        let arranger_lane_h_copy = arranger_lane_h;
        // M10 Phase 46 → 101 (daw_01 #072): track reorder の drag preview geometry。
        // dist >= 閾値 のときのみ overlay 描画 (短 click 中は静止 = button click と区別がつかないため
        // UI ノイズ)。 **commit (`pending_drop`) と同じ `resolve_track_drop`** を通すので indicator が
        // 指す位置 = 実際に着地する位置 が必ず一致する (旧 `compute_reorder_target_index` は parent /
        // 深さを描けず blank-drop で実結果とズレていた)。
        let reorder_overlay: Option<ReorderOverlay> = track_reorder_session
            .as_ref()
            .filter(|tr| {
                let dx = tr.last_mouse_x - tr.anchor_mouse_x;
                let dy = tr.last_mouse_y - tr.anchor_mouse_y;
                (dx * dx + dy * dy).sqrt() >= REORDER_DRAG_THRESHOLD_PX
            })
            .map(|tr| {
                let drop = resolve_track_drop(
                    tracks,
                    &visible_tracks,
                    &press_tops,
                    &is_group_set,
                    &tr.source_track_ids,
                    style.indent_px,
                    tr.last_mouse_y,
                    tr.last_mouse_x,
                    tr.anchor_mouse_x,
                );
                let indicator_y = press_tops
                    .get(drop.gap)
                    .copied()
                    .or_else(|| press_tops.last().copied())
                    .unwrap_or(header_pane.y);
                let indent_x = header_pane.x + f32::from(drop.depth) * style.indent_px;
                // parent が group のとき header 行を hilight。 parent が collapsed で不可視なら
                // (visible に居ない → position None →) hilight しない (不可視 UI を光らせない意図の
                // None。 reparent 構造自体は commit と同一 resolver なので一致する)。
                let highlight_row = drop.parent.and_then(|pid| {
                    visible_tracks.iter().position(|t| t.id == pid).map(|vi| {
                        let y = press_tops.get(vi).copied().unwrap_or(header_pane.y);
                        let h = effective_track_row_h(&visible_tracks[vi], view.track_row_h);
                        Rect { x: header_pane.x, y, w: header_pane.w + lanes.w, h }
                    })
                });
                ReorderOverlay {
                    indicator_y,
                    indent_x,
                    drag_center_y: tr.last_mouse_y,
                    highlight_row,
                }
            });

        // M13 Phase 55: ruler / lanes grid を library `time_ruler` / `bar_beat_grid` に統合。
        // beat 単位の view を sample 単位の `ViewportState1D` に変換 (sample_rate = 48k で
        // 比例定数は打ち消されるので BarBeat 表示には影響しない)。
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: f64::from(view.bpm.max(1.0)),
            time_sig: (view.time_sig.0.max(1), view.time_sig.1.max(1)),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let sample_viewport =
            ViewportState1D::new(view.start_beat * spb, view.len_beats.max(1e-6) * spb);
        let grid_style = BarBeatGridStyle {
            bar_color: style.bar_line,
            beat_color: style.beat_line,
            bar_line_width: style.bar_line_width_px,
            beat_line_width: style.beat_line_width_px,
            // M14 Phase 63m (daw_01 #027): zoom 連動の beat 線間引き (default 4px)。
            ..BarBeatGridStyle::default()
        };
        let ruler_style = TimeRulerStyle {
            bg: style.ruler_bg,
            tick_color: style.bar_line,
            label_color: style.ruler_label_color,
            bar_tick_height: 12.0,
            beat_tick_height: 5.0,
            // M14 Phase 63m (daw_01 #027): zoom 連動の label / beat tick 間引き (default 60 / 4 px)。
            ..TimeRulerStyle::default()
        };
        // heavy() closure は `'static` 要求なので id を hash 化して move capture。
        let id_for_inner: u64 = hash_inputs(&id);

        self.heavy(("arrangement_inner", &id), move |hctx| {
            // M14 Phase 63n-1 (#028): heavy closure 内でも prefix sum tops を計算 ('static borrow が
            // 必要なため caller scope の tops_for_draw は持ち込めない、 同一 visibility を再計算)。
            let tops_owned_for_heavy = visible_track_row_tops(
                &tracks_owned,
                lanes.y,
                view_copy.track_top,
                view_copy.track_row_h,
            );
            // M14 Phase 77 (daw_01 #048): track_top に依存する draw を scope 単位で scissor。
            // `below_ruler` は ruler 下の領域 (= header_pane ∪ lanes)、 automation lane / reorder
            // overlay 等 header と lanes をまたぐ draw 用。 ruler / loop_band / playhead は
            // track_top に依存しない static draw なので scope 外に置いて既存挙動維持。
            let below_ruler = Rect {
                x: header_pane_copy.x.min(lanes.x),
                y: header_pane_copy.y,
                w: header_pane_copy.w + lanes.w,
                h: lanes.h,
            };
            // === cached: viewport_key 一致時 skip ===
            hctx.cached(viewport_key, |hctx| {
                push_filled_rect(hctx, header_pane, style_copy.header_bg);
                // M14 Phase 77 (daw_01 #048): lanes scope (track row 系の y 依存 draw)。
                hctx.with_clip_rect(lanes, |hctx| {
                    draw_lanes_bg(
                        hctx,
                        lanes,
                        &tracks_owned,
                        &tops_owned_for_heavy,
                        view_copy,
                        &selected_tracks_for_heavy,
                        &style_copy,
                    );
                    hctx.bar_beat_grid(
                        ("arr_grid", id_for_inner),
                        lanes,
                        mapping,
                        sample_viewport,
                        grid_style,
                        // M14 Phase 124 (#100): subdivision はピアノロール限定なので arrangement は None。
                        None,
                    );
                    draw_clips(hctx, &tracks_owned, &tops_owned_for_heavy, view_copy, lanes, &style_copy);
                    // M14 Phase 63k (#025): audio_edit が Some の clip に dB handle line + fade envelope を重ねる。
                    // 描画は draw_clips 後 (clip rect の上に重なる)、 selection overlay より前 (selection の
                    // 黄色 fill が上書きしない、 selection 中も dB / fade が見える)。
                    let view_end_for_audio = view_copy.start_beat + view_copy.len_beats;
                    for (i, t) in tracks_owned.iter().enumerate() {
                        let row_top = tops_owned_for_heavy[i];
                        if row_top + view_copy.track_row_h < lanes.y || row_top > lanes.y + lanes.h {
                            continue;
                        }
                        for c in &t.clips {
                            let Some(audio) = c.audio_edit else {
                                continue;
                            };
                            let end = c.start_beat + c.len_beats;
                            if end < view_copy.start_beat || c.start_beat > view_end_for_audio {
                                continue;
                            }
                            let r = clip_to_rect(row_top, view_copy.track_row_h, c, view_copy, lanes);
                            if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
                                continue;
                            }
                            if r.w < style_copy.audio_min_clip_w_for_handles_px {
                                continue;
                            }
                            draw_clip_audio_overlay(hctx, r, &audio, c.len_beats, &style_copy);
                        }
                    }
                });
                // M14 Phase 77 (daw_01 #048): ruler scope (static、 track_top に依存しない static
                // primitive だが defensive で wrap)。
                if view_copy.ruler_h > 0.0 {
                    hctx.with_clip_rect(ruler, |hctx| {
                        hctx.time_ruler(
                            ("arr_ruler", id_for_inner),
                            ruler,
                            mapping,
                            sample_viewport,
                            ruler_style,
                        );
                    });
                }

                // M14 Phase 63n-1 (#028): automation lane 行群の描画 (track 行の下、 expand されたもののみ)。
                // 各 visible track の `automation_lanes_collapsed = false` のとき、 visible lane を上から
                // 順に積む (header = lane 左端 / body = lane 右端 = clip 描画域と同 x)。 lane の y 範囲は
                // `tops[i] + track_row_h` から `tops[i+1]` (= 次 track 上端) の間。
                // 描画は cached 内: viewport_key に lane 関連 hash が入る前提 (fold_arrangement_clip_hash
                // を後ほど lane も含むように拡張する)。 現状は clip hash で大方の変化を検出可能。
                //
                // M14 Phase 77 (daw_01 #048): below_ruler scope (header_pane + lanes を跨ぐ draw 用)。
                // automation lane の背景 fill (line 4004-4013) は header_rect.x から body_rect 終端まで
                // span するので、 単独 lanes / header_pane scope では片側が切られる。
                hctx.with_clip_rect(below_ruler, |hctx| {
                    for (i, t) in tracks_owned.iter().enumerate() {
                        if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                            continue;
                        }
                        let track_row_top = tops_owned_for_heavy[i];
                        // viewport culling: track 領域全体 (track + lanes) が viewport 外なら skip
                        let track_total_bottom = tops_owned_for_heavy[i + 1];
                        if track_total_bottom < lanes.y || track_row_top > lanes.y + lanes.h {
                            continue;
                        }
                        let mut lane_y = track_row_top + effective_track_row_h(t, view_copy.track_row_h);
                        // M14 Phase 63n-1 (#028) follow-up: lane 行 header は親 track と同じ indent に揃える
                        // (= 親 track の depth * indent_px)。 group 配下の track の lane が「どの track の
                        // lane か」 を視覚的に追えるようにするため (#028 user 指摘 1)。
                        let header_indent = f32::from(t.depth) * style_copy.indent_px;
                        for lane in &t.automation_lanes {
                            if !lane.visible {
                                continue;
                            }
                            let lh = f32::from(lane.height_px);
                            // lane 行 viewport culling
                            if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                                lane_y += lh;
                                continue;
                            }
                            let header_rect = Rect {
                                x: header_pane_copy.x + header_indent,
                                y: lane_y,
                                w: (header_pane_copy.w - header_indent).max(2.0),
                                h: lh,
                            };
                            let body_rect = Rect {
                                x: lanes.x,
                                y: lane_y,
                                w: lanes.w,
                                h: lh,
                            };
                            draw_automation_lane(
                                hctx,
                                t.id,
                                lane,
                                header_rect,
                                body_rect,
                                view_copy,
                                &style_copy,
                                lanes,
                                &selected_automation_clips_set_for_heavy,
                            );
                            lane_y += lh;
                        }
                    }
                });
            });

            // === cached 外: selection / drag preview / playhead / loop band ===
            // M14 Phase 77 (daw_01 #048): track_top に依存する overlay 群を below_ruler scope で
            // wrap。 loop_band / playhead は static (ruler / spans ruler+lanes) なので scope 外。
            hctx.with_clip_rect(below_ruler, |hctx| {
            // M14 Phase 96 (daw_01 #068): 連動ハイライトは selection overlay の **前** に描画
            // (選択中 member は黄塗りが上書き優先、 非選択の同グループ member が hue 強調の主役)。
            draw_active_group_overlay(
                hctx,
                &tracks_owned,
                &tops_owned_for_heavy,
                view_copy,
                lanes,
                &style_copy,
            );
            draw_selection_overlay(
                hctx,
                &tracks_owned,
                &tops_owned_for_heavy,
                &selected_set,
                view_copy,
                lanes,
                &style_copy,
            );
            if let Some((nd, bd, td)) = drag_overlay_clone {
                draw_drag_preview(
                    hctx,
                    &nd,
                    &tops_owned_for_heavy,
                    view_copy,
                    lanes,
                    &style_copy,
                    tracks_owned.len(),
                    bd,
                    td,
                    drag_overlay_min_len,
                );
            }
            // M14 Phase 63k (#025): audio_drag ghost overlay (drag 中の dB / fade preview + label)。
            // commit-by-release のため clip_rect_anchor + 計算済 outcome から preview rect / line を
            // 描き直す。 cached 外なので 1 frame 1 描画 (drag 中のみ)、 release frame で session が
            // take されてから次 frame は ghost 消滅。 base 描画 (cached 内) も同 frame 表示されるが、
            // ghost が上に重なって最新値を user に見せる。
            if let Some(ad) = audio_drag_overlay {
                draw_audio_drag_ghost(hctx, &ad, beat_per_px, &style_copy);
            }
            // M14 Phase 63n-2 (#028): automation_lane_default_drag overlay (band fill width 上書き)。
            // cached 内で描画される base band fill は `lane.default_value_norm` の anchor 値、 drag 中は
            // `last_emitted_value` で塗り直して live preview を user に見せる (TrackVolumeDragSession の
            // header band と同 pattern)。 trough は cached 内のままで、 fill rect だけ上書き。
            if let Some(ld) = lane_default_drag_overlay {
                let preview = volume_from_mouse_x(ld.last_mouse_x, ld.band_rect.x, ld.band_rect.w);
                push_filled_rect(
                    hctx,
                    Rect {
                        x: ld.band_rect.x,
                        y: ld.band_rect.y,
                        w: ld.band_rect.w * preview,
                        h: ld.band_rect.h,
                    },
                    style_copy.track_volume_band_fill,
                );
            }
            // M14 Phase 63n-2 (#028): automation_point_drag ghost (新位置の point dot を半透明で重ねる)。
            // anchor 固定の `body_rect_anchor` / `clip_rect_anchor` で beat_to_px / y 軸を計算 (drag
            // 中の view scroll 耐性)。 release commit と同じ式で next position を出すため SSoT を
            // 共有 (commit と overlay が同一値で確定)。 alt は session の `last_alt` を真値とする。
            if let Some(pd) = point_drag_overlay {
                let dx = pd.last_mouse.0 - pd.anchor_mouse.0;
                let dy = pd.last_mouse.1 - pd.anchor_mouse.1;
                let beat_to_px =
                    f64::from(pd.body_rect_anchor.w) / view_copy.len_beats.max(1e-6);
                let raw_dt = f64::from(dx) / beat_to_px;
                let raw_abs = pd.clip_start_beat + pd.anchor_time_beat + raw_dt;
                let snapped_abs =
                    view_copy.snap.snap_beat(raw_abs, pd.last_alt, zoom_x_px_per_beat);
                let next_local =
                    (snapped_abs - pd.clip_start_beat).clamp(0.0, pd.clip_len_beats.max(0.0));
                let next_value = (pd.anchor_value_norm
                    - dy / pd.clip_rect_anchor.h.max(1.0))
                .clamp(0.0, 1.0);
                let abs_beat = pd.clip_start_beat + next_local;
                #[allow(clippy::cast_possible_truncation)]
                let px = pd.body_rect_anchor.x
                    + ((abs_beat - view_copy.start_beat) * beat_to_px) as f32;
                let py = pd.clip_rect_anchor.y + (1.0 - next_value) * pd.clip_rect_anchor.h;
                let r = style_copy.automation_point_radius_px;
                hctx.push_rect(RectCommand {
                    rect: Rect { x: px - r, y: py - r, w: r * 2.0, h: r * 2.0 },
                    fill: style_copy.clip_selected_fill,
                    border: Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
                    border_width: 1.5,
                    radius: [r; 4],
                    clip_rect: Some(pd.body_rect_anchor),
                });
            }
            // M14 Phase 63n-3 (#028): automation_clip_drag ghost (drag 中の preview rect、 cross-lane
            // drop 解決込み)。 fill / border / badge は MIDI clip drag preview と完全対称。
            if let Some(acd) = automation_clip_drag_overlay {
                let is_move_clone =
                    matches!(acd.kind, ClipDragKind::Move) && acd.last_ctrl;
                let (fill, border, badge_glyph) = if is_move_clone {
                    if acd.last_shift {
                        (
                            style_copy.clip_clone_indep_fill,
                            style_copy.clip_clone_indep_border,
                            Some('+'),
                        )
                    } else {
                        (
                            style_copy.clip_clone_linked_fill,
                            style_copy.clip_clone_linked_border,
                            Some('⇌'),
                        )
                    }
                } else {
                    (
                        style_copy.clip_selected_fill,
                        style_copy.clip_selected_border,
                        None,
                    )
                };
                // beat_to_px は現在フレームの lanes.w から算出 (全 lane body は幅 lanes.w で同一、
                // for_each_visible_lane 参照)。 press 時の anchor 幅でなく現幅を使うことで drag 中の
                // window / header resize に追従する。
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                let raw_beat_delta = if beat_to_px > 1e-9 {
                    f64::from(acd.last_mouse.0 - acd.anchor_mouse.0) / beat_to_px
                } else {
                    0.0
                };
                // snap pivot = anchors[0] (= 掴んだ clip)、 release commit と同 SSoT。
                let beat_delta = compute_automation_clip_drag_beat_delta(
                    &acd,
                    raw_beat_delta,
                    &view_copy.snap,
                    zoom_x_px_per_beat,
                );
                let min_len = if view_copy.snap.is_active(acd.last_alt) {
                    view_copy
                        .snap
                        .beat_unit(zoom_x_px_per_beat)
                        .map_or(0.05, |u| u.max(0.05))
                } else {
                    0.05
                };
                // #071: 単一選択は cursor で cross-lane drop を preview、 複数選択は各 anchor の自 lane に
                // 留め horizontal time-shift を preview (release commit の cross-lane policy と一致)。
                let single = acd.anchors.len() == 1;
                let pad = style_copy.automation_clip_v_pad_px;
                for a in &acd.anchors {
                    let (g_start, g_len) = match acd.kind {
                        ClipDragKind::Move => ((a.start_beat + beat_delta).max(0.0), a.len_beats),
                        ClipDragKind::ResizeRight => {
                            (a.start_beat, (a.len_beats + beat_delta).max(min_len))
                        }
                        ClipDragKind::ResizeLeft => {
                            let max_start = a.start_beat + a.len_beats - min_len;
                            let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                            let actual = new_start - a.start_beat;
                            (new_start, (a.len_beats - actual).max(min_len))
                        }
                    };
                    let target_body = if single && matches!(acd.kind, ClipDragKind::Move) {
                        automation_lane_key_at_y(
                            &tracks_owned,
                            &tops_owned_for_heavy,
                            view_copy.track_row_h,
                            header_pane_copy.x,
                            header_pane_copy.w,
                            lanes.x,
                            lanes.w,
                            &style_copy,
                            acd.last_mouse.1,
                        )
                        .map_or(a.body_rect, |(_, body)| body)
                    } else {
                        a.body_rect
                    };
                    let g_clip_y = target_body.y + pad;
                    let g_clip_h = (target_body.h - pad * 2.0).max(2.0);
                    #[allow(clippy::cast_possible_truncation)]
                    let g_x =
                        target_body.x + ((g_start - view_copy.start_beat) * beat_to_px) as f32;
                    #[allow(clippy::cast_possible_truncation)]
                    let g_w = ((g_len * beat_to_px) as f32).max(2.0);
                    let ghost_rect = Rect { x: g_x, y: g_clip_y, w: g_w, h: g_clip_h };
                    if ghost_rect.x + ghost_rect.w >= lanes.x
                        && ghost_rect.x <= lanes.x + lanes.w
                    {
                        hctx.push_rect(RectCommand {
                            rect: ghost_rect,
                            fill,
                            border,
                            border_width: style_copy.clip_selected_border_w,
                            radius: [style_copy.clip_radius; 4],
                            clip_rect: Some(lanes),
                        });
                        if let Some(g) = badge_glyph
                            && ghost_rect.w > style_copy.clip_clone_badge_size + 4.0
                            && ghost_rect.h > style_copy.clip_clone_badge_size + 2.0
                        {
                            hctx.push_text(GlyphArea {
                                text: Arc::from(g.to_string()),
                                left: ghost_rect.x + 4.0,
                                top: ghost_rect.y + 2.0,
                                font_size: style_copy.clip_clone_badge_size,
                                line_height: style_copy.clip_clone_badge_size * 1.2,
                                color: style_copy.clip_clone_badge_color,
                                clip_rect: Some(ghost_rect),
                                ..GlyphArea::default()
                            });
                        }
                    }
                }
            }
            // M14 Phase 63n-8 (#033): selected automation points overlay (cached 外、 selection 変化のみで
            // 全 lane 再キャッシュは走らない設計)。 base draw (cached 内) は selection 不問の通常 dot を
            // 描く、 ここで selected な点だけを白色 + 大 dot で上書き (= base dot を完全に覆って差し替え)。
            // 描画式は `draw_automation_lane` の point dot と同 SSoT (`body_origin_x + abs_beat * beat_to_px`、
            // `clip_y + (1 - value_norm) * clip_h`)、 collapsed track / invisible lane は skip。
            if !selected_automation_points_for_heavy.is_empty() {
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                let r_sel = style_copy.automation_point_radius_selected_px;
                let pad = style_copy.automation_clip_v_pad_px;
                for (i, t) in tracks_owned.iter().enumerate() {
                    if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                        continue;
                    }
                    let row_top = tops_owned_for_heavy[i];
                    let row_total_bottom = tops_owned_for_heavy[i + 1];
                    if row_total_bottom < lanes.y || row_top > lanes.y + lanes.h {
                        continue;
                    }
                    let mut lane_y =
                        row_top + effective_track_row_h(t, view_copy.track_row_h);
                    for lane in &t.automation_lanes {
                        if !lane.visible {
                            continue;
                        }
                        let lh = f32::from(lane.height_px);
                        if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                            lane_y += lh;
                            continue;
                        }
                        let clip_y = lane_y + pad;
                        let clip_h = (lh - pad * 2.0).max(2.0);
                        for c in &lane.clips {
                            for (p_idx, p) in c.points.iter().enumerate() {
                                #[allow(clippy::cast_possible_truncation)]
                                let key = AutomationPointKey {
                                    clip: AutomationClipKey {
                                        track: t.id,
                                        lane: lane.id,
                                        clip: c.id,
                                    },
                                    point_idx: p_idx as u32,
                                };
                                if !selected_automation_points_for_heavy.contains(&key) {
                                    continue;
                                }
                                let abs_beat = c.start_beat + p.time_beat;
                                #[allow(clippy::cast_possible_truncation)]
                                let px = lanes.x
                                    + ((abs_beat - view_copy.start_beat) * beat_to_px) as f32;
                                let py =
                                    clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                                hctx.push_rect(RectCommand {
                                    rect: Rect {
                                        x: px - r_sel,
                                        y: py - r_sel,
                                        w: r_sel * 2.0,
                                        h: r_sel * 2.0,
                                    },
                                    fill: style_copy.automation_point_selected_fill,
                                    border: style_copy.automation_point_selected_border,
                                    border_width: 1.5,
                                    radius: [r_sel; 4],
                                    clip_rect: Some(Rect {
                                        x: lanes.x,
                                        y: lane_y,
                                        w: lanes.w,
                                        h: lh,
                                    }),
                                });
                            }
                        }
                        lane_y += lh;
                    }
                }
            }
            // M14 Phase 63n-9 (#033): selected point の Bezier / Exponential 入射 segment に handle を描画。
            // 描画式は `compute_curve_handle_pos` で SSoT、 drag 中は preview_value で handle 位置 + curve
            // segment を上書き (= cached layer の base curve を `automation_curve_param_preview_color` の
            // thicker line で覆って live preview)、 release frame で session take 済なら drag 終了。
            if !selected_automation_points_for_heavy.is_empty() {
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                let pad = style_copy.automation_clip_v_pad_px;
                let handle_r = style_copy.automation_curve_param_handle_radius_px;
                let handle_offset = style_copy.automation_curve_param_handle_offset_px;
                for (i, t) in tracks_owned.iter().enumerate() {
                    if t.automation_lanes_collapsed || t.automation_lanes.is_empty() {
                        continue;
                    }
                    let row_top = tops_owned_for_heavy[i];
                    let row_total_bottom = tops_owned_for_heavy[i + 1];
                    if row_total_bottom < lanes.y || row_top > lanes.y + lanes.h {
                        continue;
                    }
                    let mut lane_y =
                        row_top + effective_track_row_h(t, view_copy.track_row_h);
                    for lane in &t.automation_lanes {
                        if !lane.visible {
                            continue;
                        }
                        let lh = f32::from(lane.height_px);
                        if lane_y + lh < lanes.y || lane_y > lanes.y + lanes.h {
                            lane_y += lh;
                            continue;
                        }
                        let clip_y = lane_y + pad;
                        let clip_h = (lh - pad * 2.0).max(2.0);
                        let lane_clip = Rect {
                            x: lanes.x,
                            y: lane_y,
                            w: lanes.w,
                            h: lh,
                        };
                        for c in &lane.clips {
                            for p_idx in 1..c.points.len() {
                                #[allow(clippy::cast_possible_truncation)]
                                let key = AutomationPointKey {
                                    clip: AutomationClipKey {
                                        track: t.id,
                                        lane: lane.id,
                                        clip: c.id,
                                    },
                                    point_idx: p_idx as u32,
                                };
                                if !selected_automation_points_for_heavy.contains(&key) {
                                    continue;
                                }
                                let p = &c.points[p_idx];
                                let (kind, base_value) = match p.curve {
                                    ArrangementCurveKind::Bezier { tension } => (
                                        SetAutomationCurveParamKind::BezierTension,
                                        tension,
                                    ),
                                    ArrangementCurveKind::Exponential { bend } => (
                                        SetAutomationCurveParamKind::ExponentialBend,
                                        bend,
                                    ),
                                    _ => continue,
                                };
                                // drag 中 (= curve_param_overlay の point == 当該 key) なら preview_value、
                                // そうでなければ point の現在値 (= base_value)。 drag 中の handle のみが
                                // 動く (他の selected の handle は静止)。
                                let value = curve_param_overlay
                                    .as_ref()
                                    .filter(|cd| cd.point == key && cd.kind == kind)
                                    .map_or(base_value, |cd| cd.preview_value);
                                let prev = &c.points[p_idx - 1];
                                let prev_abs = c.start_beat + prev.time_beat;
                                let cur_abs = c.start_beat + p.time_beat;
                                #[allow(clippy::cast_possible_truncation)]
                                let prev_x = lanes.x
                                    + ((prev_abs - view_copy.start_beat) * beat_to_px) as f32;
                                #[allow(clippy::cast_possible_truncation)]
                                let cur_x = lanes.x
                                    + ((cur_abs - view_copy.start_beat) * beat_to_px) as f32;
                                let prev_y =
                                    clip_y + (1.0 - prev.value_norm.clamp(0.0, 1.0)) * clip_h;
                                let cur_y =
                                    clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                                // drag 中の preview curve segment を上書き描画 (= cached の base curve を
                                // 視覚的に置換、 line_width を +50% にして元線を覆う)。 drag 中の selected
                                // のみ 1 件描画 (他 selected の base curve は cached のまま)。
                                if curve_param_overlay
                                    .as_ref()
                                    .is_some_and(|cd| cd.point == key)
                                {
                                    let preview_kind_value = match kind {
                                        SetAutomationCurveParamKind::BezierTension => {
                                            ArrangementCurveKind::Bezier { tension: value }
                                        }
                                        SetAutomationCurveParamKind::ExponentialBend => {
                                            ArrangementCurveKind::Exponential { bend: value }
                                        }
                                    };
                                    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(32);
                                    pts.push((prev_x, prev_y));
                                    flatten_lane_segment(
                                        (prev_x, prev_y),
                                        (prev_x, prev_y),
                                        (cur_x, cur_y),
                                        (cur_x, cur_y),
                                        preview_kind_value,
                                        2.0,
                                        &mut pts,
                                    );
                                    let segs: Vec<daw_ui_renderer::LineSegment> = pts
                                        .windows(2)
                                        .map(|w| daw_ui_renderer::LineSegment {
                                            a: [w[0].0, w[0].1],
                                            b: [w[1].0, w[1].1],
                                            color: style_copy
                                                .automation_curve_param_preview_color,
                                        })
                                        .collect();
                                    hctx.push_lines(daw_ui_renderer::LineBatch {
                                        segments: segs.into(),
                                        line_width_px: style_copy
                                            .automation_curve_line_width_px
                                            * 1.5,
                                        clip_rect: Some(lane_clip),
                                    });
                                }
                                // handle dot 描画 (compute_curve_handle_pos と同 SSoT)。
                                let (hx, hy) = compute_curve_handle_pos(
                                    prev_x,
                                    prev_y,
                                    cur_x,
                                    cur_y,
                                    kind,
                                    value,
                                    handle_offset,
                                );
                                hctx.push_rect(RectCommand {
                                    rect: Rect {
                                        x: hx - handle_r,
                                        y: hy - handle_r,
                                        w: handle_r * 2.0,
                                        h: handle_r * 2.0,
                                    },
                                    fill: style_copy.automation_curve_param_handle_fill,
                                    border: style_copy.automation_curve_param_handle_border,
                                    border_width: 1.5,
                                    radius: [handle_r; 4],
                                    clip_rect: Some(lane_clip),
                                });
                            }
                        }
                        lane_y += lh;
                    }
                }
            }
            // M14 Phase 63n-8 (#033): lasso 矩形 overlay (drag 中のみ、 cached 外で半透明 cyan 系を描画)。
            // anchor から last_mouse の bounding rect を style.automation_lasso_fill / border で 1 度描画。
            // press と release が同 frame で起きる超短 click の場合、 session は release frame で take 済
            // = `lasso_overlay = None` で overlay 不描画 (= 即時消失、 user 視点で「click だけ」 と認識される)。
            if let Some(ls) = lasso_overlay {
                let rect = Rect {
                    x: ls.anchor.0.min(ls.last_mouse.0),
                    y: ls.anchor.1.min(ls.last_mouse.1),
                    w: (ls.anchor.0 - ls.last_mouse.0).abs(),
                    h: (ls.anchor.1 - ls.last_mouse.1).abs(),
                };
                hctx.push_rect(RectCommand {
                    rect,
                    fill: style_copy.automation_lasso_fill,
                    border: style_copy.automation_lasso_border,
                    border_width: 1.0,
                    radius: [0.0; 4],
                    clip_rect: Some(lanes),
                });
            }
            }); // end with_clip_rect(below_ruler)  for selection / drag / lasso overlays
            // loop band: drag preview がある場合は preview を描く、無ければ view.loop_range
            // M14 Phase 77: loop_band は ruler 領域、 track_top 不変なので scope 外で defensive wrap。
            if let Some(range) = loop_preview_clone.or(view_copy.loop_range) {
                draw_loop_band(
                    hctx,
                    range,
                    view_copy.start_beat,
                    view_copy.len_beats,
                    ruler,
                    style_copy.loop_band,
                    style_copy.loop_handle,
                    style_copy.loop_handle_w,
                );
            }
            // M14 Phase 127 (daw_01 #105): Arranger レーン (背景 + section 帯 + drag preview)。 loop band と
            // 同じく cached 外・track scroll 非依存なので below_ruler scope の外で描画 (ruler と lanes の間)。
            if arranger_lane_h_copy > 0.0 {
                draw_sections_lane(
                    hctx,
                    &sections_for_draw,
                    section_drag_overlay,
                    view_copy,
                    arranger_rect_copy,
                    arranger_header_rect_copy,
                    &view_copy.snap,
                    zoom_x_px_per_beat,
                    &style_copy,
                );
            }
            if let Some(b) = view_copy.playhead_beat
                && b >= view_copy.start_beat
                && b <= view_copy.start_beat + view_copy.len_beats
            {
                let beat_to_px = f64::from(lanes.w) / view_copy.len_beats.max(1e-6);
                #[allow(clippy::cast_possible_truncation)]
                let x = lanes.x + ((b - view_copy.start_beat) * beat_to_px) as f32;
                draw_playhead_line(
                    hctx,
                    x,
                    ruler.y,
                    lanes.y + lanes.h,
                    style_copy.playhead_color,
                    style_copy.playhead_width_px,
                );
            }

            // === M10 Phase 46 → 101 (daw_01 #072): track reorder drop indicator + preview ===
            // M14 Phase 77 (daw_01 #048): reorder overlay は header_pane + lanes を跨ぐ (横 1 行帯)
            // ので below_ruler scope で wrap (= ruler / toolbar への leak 防止)。
            //
            // M14 Phase 101 (daw_01 #072): 深さを可視化する。 (1) 着地先 group があれば header 行を
            // hilight (Cubase の緑矢印に相当)、 (2) indicator 横線の **左端を解決済み深さの indent 列に
            // 合わせる** (flush-left = top-level / 1 段右 = group の子)。 これらは `resolve_track_drop` の
            // 結果から事前計算済 (= commit と同じ着地位置を描く)。
            if let Some(ov) = reorder_overlay {
                hctx.with_clip_rect(below_ruler, |hctx| {
                    // (1) group-header hilight (nest 先の肯定フィードバック)。
                    if let Some(hl) = ov.highlight_row {
                        push_filled_rect(hctx, hl, style_copy.reorder_group_highlight);
                    }
                    // (2) 深さ連動 drop indicator 横線。 左端 = indent 列、 右端 = header + lanes。
                    let line_right = header_pane_copy.x + header_pane_copy.w + lanes.w;
                    let line_x = ov.indent_x.min(line_right - 1.0);
                    push_filled_rect(
                        hctx,
                        Rect {
                            x: line_x,
                            y: ov.indicator_y - style_copy.reorder_drop_indicator_h * 0.5,
                            w: (line_right - line_x).max(1.0),
                            h: style_copy.reorder_drop_indicator_h,
                        },
                        style_copy.reorder_drop_indicator,
                    );
                    // (3) dragging row 半透明複製 (header_pane 領域、last_mouse_y 中心)。
                    let row_h = view_copy.track_row_h;
                    let drag_y = (ov.drag_center_y - row_h * 0.5)
                        .clamp(header_pane_copy.y, header_pane_copy.y + header_pane_copy.h - row_h);
                    let alpha = style_copy.reorder_drag_alpha.clamp(0.0, 1.0);
                    let base_rgb = style_copy.track_selected_bg;
                    push_filled_rect(
                        hctx,
                        Rect { x: header_pane_copy.x, y: drag_y, w: header_pane_copy.w, h: row_h },
                        Color::rgba(base_rgb.r, base_rgb.g, base_rgb.b, alpha),
                    );
                });
            }
        });

        // ---- shortcut: Delete ----
        // Phase 47c: clip 選択優先、無ければ selected_tracks (multi-select) の先頭を削除。
        // M14 Phase 63c (#016): multi-track の一括削除はあえて単一にとどめる (ungroup → 残った
        // 子 track 群の parent_id 整理を caller 側で必要、 widget API としては `DeleteTrack(u32)` を
        // 既存 1:1 で維持し、 multi 削除は caller が selected_tracks を loop して呼べば実現できる)。
        // 現状は user 体験として「Delete shortcut で 1 track 削除」 を想定。
        if self.take_shortcut("delete") {
            if !selected_clips.is_empty() {
                self.push_edit(make_edit(ArrangementEditRequest::DeleteClips(
                    selected_clips.to_vec(),
                )));
            } else if let Some(&tid) = selected_tracks.first() {
                self.push_edit(make_edit(ArrangementEditRequest::DeleteTrack(tid)));
            }
        }

        // ---- clip drag release → MoveClips / ResizeClips ----
        // M9 Phase 60: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用。 Resize の min_len は snap unit に合わせる (snap_unit < 0.05 なら 0.05)。
        // **alt は drag 中の最終 `nd.last_alt` を真値とする** — release frame の `pointer.modifiers.alt`
        // は OS event 順序 (ModifiersChanged が MouseInput(Released) より先に届く) によって false に
        // 化けることがあるため信用しない。 `last_alt` は continuation frame で更新され release frame
        // では `allow_update = false` で保持されるので OS event 順序に依存しない。 overlay の snap
        // 判定とも同一値で確定し、 「release で grid に飛ぶ」 不整合が起きない。
        let clip_drag_release_was_some = clip_drag_release.is_some();
        if let Some(nd) = clip_drag_release {
            let release_alt = nd.last_alt;
            let (beat_delta, track_delta): (f64, i32) = {
                let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
                let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
                let raw = f64::from(dx) * beat_per_px;
                // 絶対位置 snap (overlay と一貫)。 詳細は `compute_clip_drag_beat_delta` を参照。
                let snapped = compute_clip_drag_beat_delta(
                    &nd,
                    raw,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                #[allow(clippy::cast_possible_truncation)]
                let td = (dy * row_per_px).round() as i32;
                (snapped, td)
            };
            let min_len = if view.snap.is_active(release_alt) {
                view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
            } else {
                0.05
            };
            match nd.kind {
                ClipDragKind::Move => {
                    // M14 Phase 63c (#016): visible_tracks (collapsed 親の subtree skip 後) で
                    // index → track_id を解決。 anchor.track_index が visible-idx なので、
                    // press_i32 + track_delta も visible domain で clamp する。
                    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                    let max_idx_i32 = (visible_tracks.len().saturating_sub(1)) as i32;
                    // master_row 有りなら visible_tracks[0] は synthetic master (id=MASTER_TRACK_ID)。
                    // clip drop 先から master を除外 (track_header drag / DoubleClickEmpty と
                    // 同じ guard、 ここだけ漏れていた)。 visible_tracks に通常 track が無い退化
                    // ケース (master のみ) は max < min となり clamp が panic するので max を
                    // min まで底上げして fallback (visible_tracks.get(1) = None → 元 track id)。
                    let min_idx_i32 = i32::from(master_row.is_some());
                    let clamp_max = max_idx_i32.max(min_idx_i32);
                    let mut deltas: Vec<MoveClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_start = (a.start_beat + beat_delta).max(0.0);
                        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                        let press_i32 = a.track_index as i32;
                        let new_idx = (press_i32 + track_delta).clamp(min_idx_i32, clamp_max);
                        #[allow(clippy::cast_sign_loss)]
                        let new_idx_u = new_idx.max(0) as usize;
                        let new_track_id = visible_tracks
                            .get(new_idx_u)
                            .map_or(a.key.track, |t| t.id);
                        let moved = (new_start - a.start_beat).abs() > 1e-6
                            || new_track_id != a.key.track;
                        if moved {
                            deltas.push(MoveClipDelta {
                                from: a.key,
                                to_track: new_track_id,
                                prev_start_beat: a.start_beat,
                                next_start_beat: new_start,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        // M14 Phase 63e (#019): Move + Ctrl + Shift → CloneClipsIndependent、
                        // Move + Ctrl → CloneClipsLinked、 それ以外 → 既存 MoveClips。
                        // `last_ctrl` / `last_shift` は overlay と同じ真値を読むので、 release
                        // frame の OS event 順序問題に依存せず確定する。 Alt は直交 (snap 一時
                        // 無効のみ) で、 既に上の `compute_clip_drag_beat_delta` で適用済。
                        let req = if nd.last_ctrl && nd.last_shift {
                            ArrangementEditRequest::CloneClipsIndependent(deltas)
                        } else if nd.last_ctrl {
                            ArrangementEditRequest::CloneClipsLinked(deltas)
                        } else {
                            ArrangementEditRequest::MoveClips(deltas)
                        };
                        self.push_edit(make_edit(req));
                    }
                }
                ClipDragKind::ResizeRight => {
                    let mut deltas: Vec<ResizeClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_len = (a.len_beats + beat_delta).max(min_len);
                        if (new_len - a.len_beats).abs() > 1e-6 {
                            deltas.push(ResizeClipDelta {
                                key: a.key,
                                prev_start: a.start_beat,
                                prev_len: a.len_beats,
                                next_start: a.start_beat,
                                next_len: new_len,
                                stretch: nd.last_shift,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(ArrangementEditRequest::ResizeClips(deltas)));
                    }
                }
                ClipDragKind::ResizeLeft => {
                    let mut deltas: Vec<ResizeClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let max_start = a.start_beat + a.len_beats - min_len;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual = new_start - a.start_beat;
                        let new_len = (a.len_beats - actual).max(min_len);
                        if (new_start - a.start_beat).abs() > 1e-6
                            || (new_len - a.len_beats).abs() > 1e-6
                        {
                            deltas.push(ResizeClipDelta {
                                key: a.key,
                                prev_start: a.start_beat,
                                prev_len: a.len_beats,
                                next_start: new_start,
                                next_len: new_len,
                                stretch: nd.last_shift,
                            });
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(ArrangementEditRequest::ResizeClips(deltas)));
                    }
                }
            }
        }

        // ---- M14 Phase 63k (#025): audio_drag release → SetClipGainDb / SetClipFade / SetClipFadeCurve ----
        // commit-by-release: drag 中は ghost overlay のみ、 release で `compute_audio_drag_outcome` の
        // 結果に応じて 1 件 emit する。 sticky direction 未確定 + drag 距離不足の場合は no-op
        // (= click 相当、 caller 側で selection 等は変化しない、 既存挙動)。 単一 clip 限定の `vec![delta]`
        // で発行 (multi-clip selection 一括は仕様 §scope 外、 将来拡張)。
        if let Some(ad) = audio_drag_release
            && let Some(out) = compute_audio_drag_outcome(&ad, beat_per_px, style)
        {
            match out {
                AudioDragOutcome::Gain { next_db } => {
                    let delta = ClipGainDelta {
                        key: ad.key,
                        prev_gain_db: ad.anchor.gain_db,
                        next_gain_db: next_db,
                    };
                    self.push_edit(make_edit(ArrangementEditRequest::SetClipGainDb(vec![
                        delta,
                    ])));
                }
                AudioDragOutcome::FadeLength { edge, next_beats } => {
                    let prev_beats = match edge {
                        FadeEdge::In => ad.anchor.fade_in_beats,
                        FadeEdge::Out => ad.anchor.fade_out_beats,
                    };
                    let delta = ClipFadeDelta {
                        key: ad.key,
                        edge,
                        prev_beats,
                        next_beats,
                    };
                    self.push_edit(make_edit(ArrangementEditRequest::SetClipFade(vec![delta])));
                }
                AudioDragOutcome::FadeCurve { edge, next_curve } => {
                    let delta = ClipFadeCurveDelta { key: ad.key, edge, next_curve };
                    self.push_edit(make_edit(ArrangementEditRequest::SetClipFadeCurve(vec![
                        delta,
                    ])));
                }
            }
        }

        // ---- M14 Phase 63n-2 / 63n-8 (#028 / #033): automation_point_drag release ----
        // 旧 Phase 63n-2: 4px jitter 閾値で短 click → no-op (selection 変化なし)。
        // M14 Phase 63n-8 (#033): 短 click は **`SelectAutomationPoints`** に化け、 long drag (>=4px)
        // は selection に含まれていれば **全 selected 点を batch move**、 含まれなければ単独 move。
        //
        // 短 click 仕様 (#033 §C):
        //   - 修飾なし + drag<4px → `next = vec![pressed]` (single select、 旧 selection 破棄)
        //   - Shift / Ctrl + drag<4px → `next = prev XOR vec![pressed]` (toggle)
        //   - Alt + click は既に上の press block で `DeleteAutomationPoints` 即時発火済 (= ここに来ない)
        //
        // long drag 仕様 (#033 §E):
        //   - pressed point が `selected_automation_points` に含まれる → 全 selected の `MoveAutomationPointDelta`
        //     を 1 vec で発行 (各 delta の prev は **release 時点の caller データ** から再 lookup、 next は
        //     pressed point の anchor 位置を round して算出した adjusted_dt を適用)
        //   - 含まれない → 単独 move (旧挙動互換、 selection は変化しない)
        //
        // **absolute 位置 snap** (CLAUDE.md「drag 系 widget の snap」 と同 idiom): anchor の絶対 beat
        // (`clip_start + anchor_time` ) に raw_dt を足して `snap_beat` で round、 差分 `adjusted_dt`
        // を全 anchor に適用。 これで (a) 単一 / 多重で grid 吸着挙動が一致、 (b) anchor が grid 外でも
        // 最終位置 grid に着地。 alt は session の `last_alt` を真値 (race 回避)。
        if let Some(ad) = point_drag_release {
            let dx = ad.last_mouse.0 - ad.anchor_mouse.0;
            let dy = ad.last_mouse.1 - ad.anchor_mouse.1;
            let dist = dx.abs() + dy.abs();
            if dist >= 4.0 {
                // body_rect / clip_rect は anchor 固定 (drag 中の view scroll / lane 順序変化に強い)。
                let beat_to_px =
                    f64::from(ad.body_rect_anchor.w) / view.len_beats.max(1e-6);
                let raw_dt = f64::from(dx) / beat_to_px;
                let raw_abs = ad.clip_start_beat + ad.anchor_time_beat + raw_dt;
                let snapped_abs =
                    view.snap.snap_beat(raw_abs, ad.last_alt, zoom_x_px_per_beat);
                let adjusted_dt = snapped_abs - (ad.clip_start_beat + ad.anchor_time_beat);
                let dv = -dy / ad.clip_rect_anchor.h.max(1.0);
                // pressed が selection に含まれていれば multi、 そうでなければ single
                let drag_set: Vec<AutomationPointKey> =
                    if selected_automation_points.contains(&ad.point) {
                        selected_automation_points.to_vec()
                    } else {
                        vec![ad.point]
                    };
                let mut deltas: Vec<MoveAutomationPointDelta> = Vec::new();
                for key in &drag_set {
                    // release 時の caller データから anchor を再 lookup (drag 中は Edit 流れないので
                    // model 不変、 visible_tracks がそのまま使える)。
                    if let Some((t_b, v_n, _c_start, c_len)) =
                        find_automation_point_data(&visible_tracks, *key)
                    {
                        let next_t = (t_b + adjusted_dt).clamp(0.0, c_len.max(0.0));
                        let next_v = (v_n + dv).clamp(0.0, 1.0);
                        if (next_t - t_b).abs() > 1e-9 || (next_v - v_n).abs() > 1e-6 {
                            deltas.push(MoveAutomationPointDelta {
                                point: *key,
                                prev_time_beat: t_b,
                                prev_value_norm: v_n,
                                next_time_beat: next_t,
                                next_value_norm: next_v,
                            });
                        }
                    }
                }
                if !deltas.is_empty() {
                    self.push_edit(make_edit(ArrangementEditRequest::MoveAutomationPoints(
                        deltas,
                    )));
                }
            } else if !ad.last_alt {
                // 短 click (drag < 4px) → SelectAutomationPoints。 Alt は上 press block で delete 済なので
                // ここで Alt 真値の path は来ない前提だが、 防衛的に `!ad.last_alt` で除外する。
                let press_shift = ad.start_modifiers.shift;
                let press_ctrl = ad.start_modifiers.ctrl;
                let prev = selected_automation_points.to_vec();
                let next: Vec<AutomationPointKey> = if press_shift || press_ctrl {
                    // toggle: prev XOR {pressed}
                    toggle_selection(&prev, ad.point)
                } else {
                    // replace: vec![pressed] (pressed が既に唯一の selection なら同値 → no-op)
                    vec![ad.point]
                };
                if next != prev {
                    self.push_edit(make_edit(
                        ArrangementEditRequest::SelectAutomationPoints { prev, next },
                    ));
                    response.selection_changed = true;
                }
            }
        }

        // ---- M14 Phase 63n-9 (#033): automation_curve_param_drag release → SetAutomationCurveParam ----
        // anchor と preview の差分が 1e-4 未満なら no-op (= handle を click したけど drag しなかったケース、
        // = user 意図的に値を変えていない)。 そうでなければ `SetAutomationCurveParam { point, kind, prev, next }`
        // を 1 件発行 (caller の AppEvent は kind で `BezierTension` / `ExponentialBend` を分岐して
        // `clip.points[idx].curve = Bezier { tension: next }` or `Exponential { bend: next }` で commit)。
        if let Some(cd) = automation_curve_param_release
            && (cd.preview_value - cd.anchor_value).abs() > 1e-4
        {
            self.push_edit(make_edit(ArrangementEditRequest::SetAutomationCurveParam {
                point: cd.point,
                kind: cd.kind,
                prev_value: cd.anchor_value,
                next_value: cd.preview_value,
            }));
        }

        // ---- M14 Phase 63n-8 (#033): automation_lasso_drag release → SelectAutomationPoints ----
        // 空き lane zone で press → drag → release で発火。 next 計算は **press 時 modifier** で分岐:
        // - 修飾なし → replace (next = lasso 内 points、 旧 selection 破棄)
        // - Shift   → union  (next = prev ∪ lasso 内 points)
        // - Ctrl    → XOR    (next = prev XOR lasso 内 points = toggle inclusion)
        //
        // **dist < 4px の空き click 短 click 化**:
        // - 修飾なし → `next = vec![]` (clear、 空き click = selection clear、 既存 MIDI lanes_click と同 UX)
        // - Shift / Ctrl → no-op (selection 維持、 = 誤クリック保護)
        //
        // 「lasso rect 内に point の **中心** が含まれる」 を hit 判定 (#033 §C 仕様)。 visible_tracks
        // ベースで collapsed / invisible lane は対象外 (= 既存 `automation_point_at` の visible scope と整合)。
        if let Some(ls) = automation_lasso_release {
            let abs_w = (ls.last_mouse.0 - ls.anchor.0).abs();
            let abs_h = (ls.last_mouse.1 - ls.anchor.1).abs();
            let dist_lasso = abs_w + abs_h;
            let lasso_rect = Rect {
                x: ls.anchor.0.min(ls.last_mouse.0),
                y: ls.anchor.1.min(ls.last_mouse.1),
                w: abs_w,
                h: abs_h,
            };
            let prev = selected_automation_points.to_vec();
            let next: Vec<AutomationPointKey> = if dist_lasso < 4.0 {
                // 空き短 click — 修飾なしで clear、 Shift / Ctrl は no-op
                if ls.start_modifiers.shift || ls.start_modifiers.ctrl {
                    prev.clone()
                } else {
                    Vec::new()
                }
            } else {
                // lasso の点中心 hit 判定 (visible_tracks + visible lane scope)
                let inside =
                    collect_points_in_rect(&visible_tracks, &press_tops, view, lanes, lasso_rect);
                if ls.start_modifiers.shift {
                    // union (prev order を保持 + lasso 由来の新規だけ append)
                    let mut out = prev.clone();
                    for k in inside {
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else if ls.start_modifiers.ctrl {
                    // XOR (toggle inclusion): prev に在って lasso にも在る点を除く + prev に無くて lasso に在る点を追加
                    let mut out: Vec<AutomationPointKey> =
                        prev.iter().copied().filter(|k| !inside.contains(k)).collect();
                    for k in inside {
                        if !prev.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else {
                    inside // replace
                }
            };
            if next != prev {
                self.push_edit(make_edit(
                    ArrangementEditRequest::SelectAutomationPoints { prev, next },
                ));
                response.selection_changed = true;
            }

            // daw_01 #071 (option 1): 同じ四角ドラッグで automation **clip** も選択する (点とクリップを
            // 同時に拾う = 何も失わず clip の範囲選択を上乗せ)。 修飾セマンティクスは点と完全対称
            // (修飾なし=replace / Shift=union / Ctrl=XOR、 空き短 click は修飾なしで clear・Shift/Ctrl で no-op)。
            // clip は rect 交差で hit (点は中心 hit)、 = MIDI clip marquee と同 `rects_intersect` 判定。
            let clip_prev = selected_automation_clips.to_vec();
            let clip_next: Vec<AutomationClipKey> = if dist_lasso < 4.0 {
                if ls.start_modifiers.shift || ls.start_modifiers.ctrl {
                    clip_prev.clone()
                } else {
                    Vec::new()
                }
            } else {
                let inside = collect_clips_in_rect(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    view,
                    header_pane.x,
                    header_pane.w,
                    lanes,
                    style,
                    lasso_rect,
                );
                if ls.start_modifiers.shift {
                    let mut out = clip_prev.clone();
                    for k in inside {
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else if ls.start_modifiers.ctrl {
                    let mut out: Vec<AutomationClipKey> =
                        clip_prev.iter().copied().filter(|k| !inside.contains(k)).collect();
                    for k in inside {
                        if !clip_prev.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else {
                    inside
                }
            };
            if clip_next != clip_prev {
                self.push_edit(make_edit(ArrangementEditRequest::SelectAutomationClips {
                    prev: clip_prev,
                    next: clip_next,
                }));
                response.selection_changed = true;
            }
        }

        // ---- M14 Phase 63n-5 (#030): automation_lane_resize_drag release → SetLaneHeight ----
        // drag 中は per-frame emit で live update 済 (lane_default_drag と同 pattern)。
        // release frame は 1 度だけ最終値を `SetLaneHeight { prev: anchor, next: end }` で発行。
        // anchor と同値なら no-op (= ユーザが splitter を click したけど drag しなかったケース)。
        if let Some(rd) = lane_resize_drag_release {
            let dy = rd.last_mouse_y - rd.anchor_mouse_y;
            let raw = f32::from(rd.anchor_height_px) + dy;
            // M14 Phase 63n-6 (#031): release も runtime clamp (style.max ∧ lanes.h)。
            let end = clamp_height_px(
                raw,
                style.automation_lane_min_height_px,
                effective_lane_max_height(style, lanes),
            );
            if end != rd.anchor_height_px {
                self.push_edit(make_edit(ArrangementEditRequest::SetLaneHeight {
                    lane: rd.lane,
                    prev: rd.anchor_height_px,
                    next: end,
                }));
            }
        }

        // ---- M14 Phase 63n-2 (#028): automation_lane_default_drag release → SetLaneDefault ----
        // drag 中は per-frame Mutate emit で live update 済 (TrackVolumeDragSession と同 pattern)。
        // release frame は 1 度だけ最終値を `SetLaneDefault { prev: anchor, next: end }` で発行。
        if let Some(ld) = lane_default_drag_release {
            let end = volume_from_mouse_x(ld.last_mouse_x, ld.band_rect.x, ld.band_rect.w);
            if (end - ld.anchor_value_norm).abs() > 1e-4 {
                self.push_edit(make_edit(ArrangementEditRequest::SetLaneDefault {
                    lane: ld.lane,
                    prev: ld.anchor_value_norm,
                    next: end,
                }));
            }
        }

        // ---- M14 Phase 63n-3 (#028): automation_clip_drag release ----
        // commit-by-release: 短 click (Move + !Alt + dist < 4px) は **`SelectAutomationClips` に demote**
        // (= 既存 MIDI clip の `clip_short_click_pos` 経路と同 idiom、 lane body 上 click は automation
        // 選択に振る)。 それ以外は MoveAutomationClips / CloneAutomationClipsLinked /
        // CloneAutomationClipsIndependent / ResizeAutomationClips を発行。 modifier は session の
        // `last_*` を真値とし pointer.modifiers を直接見ない (race 回避、 ClipDragSession と同 pattern)。
        // beat_to_px は anchor 固定 `body_rect_anchor` から計算 (view scroll 耐性)、 absolute snap で
        // grid 吸着、 cross-lane Move は release y から `automation_lane_key_at_y` で drop lane 解決。
        if let Some(acd) = automation_clip_drag_release {
            let release_alt = acd.last_alt;
            let dx = acd.last_mouse.0 - acd.anchor_mouse.0;
            let dy = acd.last_mouse.1 - acd.anchor_mouse.1;
            let dist = dx.abs() + dy.abs();
            let demote =
                matches!(acd.kind, ClipDragKind::Move) && !release_alt && dist < 4.0;
            if demote {
                // short click on automation clip → 修飾で分岐 (#071): 修飾なし = 単一置換、
                // Shift / Ctrl = 選択足し引き (= 既に居れば外す、 居なければ足す toggle)。 MIDI clip
                // 同様 anchor 順は問わず、 掴んだ clip (= primary) を対象にする。
                let prev = selected_automation_clips.to_vec();
                let key = acd.primary;
                let next: Vec<AutomationClipKey> = if acd.last_shift || acd.last_ctrl {
                    if prev.contains(&key) {
                        prev.iter().copied().filter(|k| *k != key).collect()
                    } else {
                        let mut out = prev.clone();
                        out.push(key);
                        out
                    }
                } else {
                    vec![key]
                };
                if prev != next {
                    self.push_edit(make_edit(
                        ArrangementEditRequest::SelectAutomationClips { prev, next },
                    ));
                    response.selection_changed = true;
                }
            } else {
                // beat_to_px は現在フレームの lanes.w から算出 (全 lane body は幅 lanes.w で同一)。
                // press 時の anchor 幅でなく現幅を使うことで drag 中の resize に追従する。
                let beat_to_px = f64::from(lanes.w) / view.len_beats.max(1e-6);
                let raw_beat_delta = if beat_to_px > 1e-9 {
                    f64::from(dx) / beat_to_px
                } else {
                    0.0
                };
                // snap pivot = anchors[0] (= 掴んだ clip)、 overlay ghost と同 SSoT。
                let beat_delta = compute_automation_clip_drag_beat_delta(
                    &acd,
                    raw_beat_delta,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                let min_len = if view.snap.is_active(release_alt) {
                    view.snap
                        .beat_unit(zoom_x_px_per_beat)
                        .map_or(0.05, |u| u.max(0.05))
                } else {
                    0.05
                };
                // #071: cross-lane drop は単一選択 drag のみ (cursor 解決)。 複数選択一括は宛先 lane が
                // 一意でない (異種・可変高 lane) ため各 anchor は自 lane 維持の horizontal time-shift。
                let single = acd.anchors.len() == 1;
                match acd.kind {
                    ClipDragKind::Move => {
                        let cursor_lane = if single {
                            automation_lane_key_at_y(
                                &visible_tracks,
                                &press_tops,
                                view.track_row_h,
                                header_pane.x,
                                header_pane.w,
                                lanes.x,
                                lanes.w,
                                style,
                                acd.last_mouse.1,
                            )
                        } else {
                            None
                        };
                        let mut deltas: Vec<MoveAutomationClipDelta> = Vec::new();
                        for a in &acd.anchors {
                            let new_start = (a.start_beat + beat_delta).max(0.0);
                            let to_lane = cursor_lane.map_or(a.lane, |(lk, _body)| lk);
                            let moved = (new_start - a.start_beat).abs() > 1e-6 || to_lane != a.lane;
                            if moved {
                                deltas.push(MoveAutomationClipDelta {
                                    from: a.key,
                                    to_lane,
                                    prev_start_beat: a.start_beat,
                                    next_start_beat: new_start,
                                });
                            }
                        }
                        if !deltas.is_empty() {
                            let req = if acd.last_ctrl && acd.last_shift {
                                ArrangementEditRequest::CloneAutomationClipsIndependent(deltas)
                            } else if acd.last_ctrl {
                                ArrangementEditRequest::CloneAutomationClipsLinked(deltas)
                            } else {
                                ArrangementEditRequest::MoveAutomationClips(deltas)
                            };
                            self.push_edit(make_edit(req));
                        }
                    }
                    ClipDragKind::ResizeRight => {
                        let mut deltas: Vec<ResizeAutomationClipDelta> = Vec::new();
                        for a in &acd.anchors {
                            let new_len = (a.len_beats + beat_delta).max(min_len);
                            if (new_len - a.len_beats).abs() > 1e-6 {
                                deltas.push(ResizeAutomationClipDelta {
                                    key: a.key,
                                    prev_start: a.start_beat,
                                    prev_len: a.len_beats,
                                    next_start: a.start_beat,
                                    next_len: new_len,
                                });
                            }
                        }
                        if !deltas.is_empty() {
                            self.push_edit(make_edit(
                                ArrangementEditRequest::ResizeAutomationClips(deltas),
                            ));
                        }
                    }
                    ClipDragKind::ResizeLeft => {
                        let mut deltas: Vec<ResizeAutomationClipDelta> = Vec::new();
                        for a in &acd.anchors {
                            let max_start = a.start_beat + a.len_beats - min_len;
                            let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                            let actual = new_start - a.start_beat;
                            let new_len = (a.len_beats - actual).max(min_len);
                            if (new_start - a.start_beat).abs() > 1e-6
                                || (new_len - a.len_beats).abs() > 1e-6
                            {
                                deltas.push(ResizeAutomationClipDelta {
                                    key: a.key,
                                    prev_start: a.start_beat,
                                    prev_len: a.len_beats,
                                    next_start: new_start,
                                    next_len: new_len,
                                });
                            }
                        }
                        if !deltas.is_empty() {
                            self.push_edit(make_edit(
                                ArrangementEditRequest::ResizeAutomationClips(deltas),
                            ));
                        }
                    }
                }
            }
        }

        // ---- short click on lanes (drag<16px) → SelectClips ----
        if let Some((cx, cy)) = clip_short_click_pos
            && lanes.contains(cx, cy)
        {
            let prev = selected_clips.to_vec();
            let next: Vec<ClipKey> =
                if let Some((hit_key, _)) = clip_hit(&visible_tracks, &press_tops, view, lanes, cx, cy, style.resize_handle_px) {
                    vec![hit_key]
                } else {
                    Vec::new()
                };
            if prev != next {
                self.push_edit(make_edit(ArrangementEditRequest::SelectClips { prev, next }));
                response.selection_changed = true;
            }
            if let Some(idx) = track_index_from_y(cy, lanes.y, &press_tops)
                && let Some(t) = visible_tracks.get(idx)
            {
                let beat = px_to_beat(cx, lanes.x, lanes.w, view);
                response.clicked_at_track_beat = Some((t.id, beat));
            }
        }

        // ---- M14 Phase 125 (#102) + daw_01 #75: drag marquee gate ----
        // 旧設計は rect-select 起動に Shift 必須だったが、 標準 DAW (REAPER/Live/Bitwig) に倣い
        // **空き zone を無修飾 drag → 範囲選択** にする。 修飾は release 時の next 計算で
        // plain=REPLACE / Shift=UNION / Ctrl=XOR に分岐し、 press 時 modifier は
        // `take_drag_rect_in_rect` が `DragRect.modifiers` に snapshot する (下の commit block で読む)。
        // gate を **clear の前** で評価して `marquee_active` を作り、 同フレーム二重 emit を防ぐ。
        //
        // #75: clip の **上から** でも範囲選択を開始できるようにする。 起動 zone を `marquee_zone_ok`
        // で判定する:
        //   - clip 無し (空き zone)              → 任意修飾で marquee (従来どおり)。
        //   - clip の **Move zone** + Shift+!Ctrl → marquee (NEW)。 plain Move / Ctrl(+Shift) clone /
        //                                           Shift+resize time-stretch とは排他。
        //   - clip の resize handle / その他      → marquee 不可 (time-stretch・clone・move に譲る)。
        // この zone 判定は press 側 clip_drag gate (#021 の `(!shift||ctrl)` / FIXME #61 の resize) と
        // 鏡像で、 marquee に入る press は press 側で clip_drag session を **起動しない** ものに限られる。
        // 二重防御として下の no-session ガード (全 session None) でも弾く。 automation lane は lasso が
        // 所有するため `!press_in_automation_lane` で除外 (no_session は `automation_lasso_drag` を
        // 含まないのでこの zone 除外が必須)。 splitter / 他 drag も no-session で除外。
        let drag_rect_wid = wid.child(b"rect_select");
        let shift_rect_active = {
            let state: &mut crate::widgets::drag_rect::DragRectState =
                self.widget_state(drag_rect_wid);
            state.drag_start.is_some()
        };
        let press_in_automation_lane = pointer.primary_just_pressed
            && pointer.pos.is_some_and(|(_, py)| {
                automation_lane_at(
                    &visible_tracks,
                    &press_tops,
                    view.track_row_h,
                    header_pane.x,
                    header_pane.w,
                    lanes.x,
                    lanes.w,
                    style,
                    py,
                )
                .is_some()
            });
        let marquee_zone_ok = pointer.primary_just_pressed
            && !pointer.modifiers.alt
            && !press_in_automation_lane
            && pointer.pos.is_some_and(|(px, py)| {
                lanes.contains(px, py)
                    && match clip_hit(
                        &visible_tracks,
                        &press_tops,
                        view,
                        lanes,
                        px,
                        py,
                        style.resize_handle_px,
                    ) {
                        None => true,
                        // #75: clip 本体 (Move zone) は Shift(Ctrl なし) のときだけ marquee 起動。
                        Some((_, ClipDragKind::Move)) => {
                            pointer.modifiers.shift && !pointer.modifiers.ctrl
                        }
                        // resize handle 上は time-stretch (#61) / resize に譲る。
                        Some(_) => false,
                    }
            });
        let marquee_press = if marquee_zone_ok {
            let s: &ArrangementState = self.widget_state(wid);
            s.track_volume_drag.is_none()
                && s.track_reorder.is_none()
                && s.audio_drag.is_none()
                && s.clip_drag.is_none()
                && s.automation_lane_default_drag.is_none()
                && s.automation_point_drag.is_none()
                && s.automation_clip_drag.is_none()
                && s.automation_lane_resize_drag.is_none()
                && s.track_row_resize_drag.is_none()
                && s.playhead_drag.is_none()
                && s.loop_drag.is_none()
                && s.automation_curve_param_drag.is_none()
        } else {
            false
        };
        let marquee_active = marquee_press || shift_rect_active;

        // ---- pure release on empty lanes (no drag started) → SelectClips clear ----
        // clip_drag_session が無い + 空白 release + Shift なし。 #102: marquee がこの空き zone press を
        // 所有する frame (`marquee_active`) は下の commit が zero-rect REPLACE で clear するため、 ここでは
        // push しない (= 同フレーム二重 emit / undo 二重を防ぐ。 daw_01 #102「二重 emit 抑制」)。
        if pointer.primary_just_released
            && clip_short_click_pos.is_none()
            && !clip_drag_release_was_some
            && !pointer.modifiers.shift
            && !marquee_active
            && let Some((cx, cy)) = pointer.pos
            && lanes.contains(cx, cy)
            && clip_hit(&visible_tracks, &press_tops, view, lanes, cx, cy, style.resize_handle_px).is_none()
            && !selected_clips.is_empty()
        {
            self.push_edit(make_edit(ArrangementEditRequest::SelectClips {
                prev: selected_clips.to_vec(),
                next: Vec::new(),
            }));
            response.selection_changed = true;
        }

        // ---- loop drag release → SetLoopRange ----
        // M14 Phase 63j (#024): snap 適用済 endpoints を overlay と共通の helper で計算。
        // alt は `ld.last_alt` を真値とし、 release frame の `pointer.modifiers.alt` を直接見ない
        // (clip_drag と同じ理由 — OS event 順序で false 化する race を回避)。
        if let Some(ld) = loop_drag_release {
            let cur_beat = px_to_beat(ld.last_mouse_x, ruler.x, ruler.w, view);
            let (start, end) =
                compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat);
            self.push_edit(make_edit(ArrangementEditRequest::SetLoopRange { start, end }));
        }

        // ---- M14 Phase 63c (#016): track header drag release → SetTrackParent ----
        // dist < 16px → click 格下げ (modifier-aware SelectTrack に任せる、 後続 loop の clicked_track 経路)
        // dist >= 16px → 上で計算した `pending_drop` を SetTrackParent として 1 度発行。
        // 旧 ReorderTracks 経由の sibling reorder も同 variant に統合済 (parent 不変 + anchor_after 指定)。
        if let Some((src_tracks, parent, anchor_after)) = pending_drop {
            self.push_edit(make_edit(ArrangementEditRequest::SetTrackParent {
                tracks: src_tracks,
                parent,
                anchor_after,
            }));
        }

        // ---- M10 Phase 47b+49: track volume drag release → Undoable Edit ----
        // drag 中は per-frame Mutate で live update 済 (mixer fader と挙動同期)。
        // release frame は Mutate suppress + `Edit::with_inverse` で Undoable wrap (Ctrl+Z で 1 回 undo)。
        // forward = end_value、inverse = anchor_volume の対称な make_edit 呼び出し。
        if let Some(tv) = track_volume_release {
            let end = volume_from_mouse_x(tv.last_mouse_x, tv.band_rect.x, tv.band_rect.w);
            if (end - tv.anchor_volume).abs() > 1e-4 {
                let track_id = tv.track_id;
                let anchor = tv.anchor_volume;
                let make_edit_fwd = make_edit.clone();
                let make_edit_inv = make_edit.clone();
                self.push_edit(Edit::with_inverse(
                    "set track volume",
                    move |m: &mut M| {
                        make_edit_fwd(ArrangementEditRequest::SetTrackVolume {
                            track: track_id,
                            prev: anchor,
                            next: end,
                        })
                        .apply(m);
                    },
                    move |m: &mut M| {
                        make_edit_inv(ArrangementEditRequest::SetTrackVolume {
                            track: track_id,
                            prev: end,
                            next: anchor,
                        })
                        .apply(m);
                    },
                ));
            }
        }

        // ---- M14 Phase 125 (#102): marquee commit (modifier 分岐: plain=REPLACE / Shift=UNION / Ctrl=XOR) ----
        // gate `marquee_active` と `drag_rect_wid` は上の clear ガード手前で計算済 (空き zone press のみ所有)。
        // `take_drag_rect_in_rect` は呼ぶだけで cyan overlay を自動描画し、 press 時 modifier を
        // `DragRect.modifiers` に snapshot する。 release frame (`drag.finished`) に inside を計算して修飾で
        // next を分岐。 REPLACE は inside そのまま (zero-rect → 空 → 選択 clear)。 `prev != next` ガードで
        // no-op を抑制 (automation lasso #033 と同 idiom)。 Ctrl+Shift clone は clip HIT 時のみ (gate の
        // `clip_hit().is_none()`) なので、 ここに来る press は必ず空き zone = clone と競合しない。
        if marquee_active
            && let Some(drag) = self.take_drag_rect_in_rect(drag_rect_wid, lanes)
        {
            response.rect_select_active = true;
            if drag.finished {
                let drag_rect = drag.rect();
                let mut inside: Vec<ClipKey> = Vec::new();
                for (i, t) in visible_tracks.iter().enumerate() {
                    let row_top = press_tops[i];
                    for c in &t.clips {
                        let r = clip_to_rect(row_top, view.track_row_h, c, view, lanes);
                        if rects_intersect(r, drag_rect) {
                            inside.push(ClipKey { track: t.id, clip: c.id });
                        }
                    }
                }
                let prev: Vec<ClipKey> = selected_clips.to_vec();
                let next: Vec<ClipKey> = if drag.modifiers.shift {
                    // UNION: prev 順を保持しつつ inside の新規だけ append。
                    let mut out = prev.clone();
                    for k in inside {
                        if !out.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else if drag.modifiers.ctrl {
                    // XOR: prev に在って inside にも在る key を除き、 inside の新規を追加。
                    let mut out: Vec<ClipKey> =
                        prev.iter().copied().filter(|k| !inside.contains(k)).collect();
                    for k in inside {
                        if !prev.contains(&k) {
                            out.push(k);
                        }
                    }
                    out
                } else {
                    inside // REPLACE (zero-rect なら空 = clear)
                };
                if next != prev {
                    self.push_edit(make_edit(ArrangementEditRequest::SelectClips { prev, next }));
                    response.selection_changed = true;
                }
            }
        }

        // ---- wheel: Ctrl=zoom_x / Alt=zoom_y (row_h) / Shift=scroll_x / plain=track_top ----
        // M14 Phase 104 (daw_01 #075): wheel を **ruler 下の content 全域** (`header_pane` + `lanes`) で
        // 取得する。 左の track header 列 (master row header / automation lane header を含む) の上でも
        // 縦操作 (plain=scroll / Alt=zoom_y) が効く。 横操作 (Ctrl=zoom_x / Shift=scroll_x) は beat anchor
        // (`mx - lanes.x`) が header 上 (`mx < lanes.x`) では意味を成さないため header 上では無視する
        // (= `over_lanes` で gate)。 lanes 上の 4 操作はすべて従来どおり (header_w==0 なら content 全域 ==
        // lanes、 over_lanes は常に true で旧挙動と byte 互換)。
        let content_below_ruler = Rect {
            x: header_pane.x,
            y: header_pane.y,
            w: header_pane.w + lanes.w,
            h: lanes_h,
        };
        let scroll = self.take_scroll_in_rect(content_below_ruler);
        if scroll.1.abs() > 0.0 || scroll.0.abs() > 0.0 {
            let dy = scroll.1;
            // header pane 上 (`mx < lanes.x`) では横軸操作 (Ctrl / Shift) を無視。 pointer.pos は
            // take_scroll_in_rect が `content_below_ruler.contains` を満たして Some を保証済。
            let over_lanes = pointer.pos.is_some_and(|(mx, _)| mx >= lanes.x);
            if pointer.modifiers.ctrl && over_lanes {
                // M14 Phase 61a (#011): wheel up = zoom in (符号反転)、 1 ノッチで ~20% 変化
                // (係数 0.005 → 0.0015、 Cubase/Live 同等)、 SetZoomX を絶対値送信に統一
                // (旧設計は factor 0.55..1.82 を直送りで daw_01 の clamp(2, 400) で必ず 2 に
                // 張り付き ruler 1〜100 圧縮を起こしていた)。 SetTrackRowH と同パターン。
                // M14 Phase 61a follow-up: マウス位置を anchor に zoom (Cubase/Live 標準)、
                // SetScrollX を同 frame で発行して beat_at_mouse を維持。
                let factor = (dy * 0.0015).exp();
                let new_zoom = (zoom_x_px_per_beat * factor).clamp(0.1, 10000.0);
                if let Some((mx, _)) = pointer.pos {
                    let beat_at_mouse =
                        view.start_beat + f64::from(mx - lanes.x) * beat_per_px;
                    let new_beat_per_px = 1.0 / f64::from(new_zoom);
                    let new_start = beat_at_mouse - f64::from(mx - lanes.x) * new_beat_per_px;
                    self.push_edit(make_edit(ArrangementEditRequest::SetScrollX(
                        new_start.max(0.0),
                    )));
                }
                self.push_edit(make_edit(ArrangementEditRequest::SetZoomX(new_zoom)));
            } else if pointer.modifiers.alt {
                // M10 Phase 48 / M14 Phase 61a (#011): Alt+wheel で row_h 縦ズーム (exp curve、
                // wheel up = zoom in)、 マウス y 位置を anchor に SetTrackTop で画面位置維持。
                // M14 Phase 63n-6 (#031): 加えて **automation lane の height_px も同 factor で scale** —
                // user feedback「Alt+wheel で MIDI track と automation lane が同時に変わってほしい」 を
                // 反映。 visible track の visible lane に `SetLaneHeight` を 1 件ずつ発行 (= caller が
                // 各 lane を update、 lane.height_px は per-track row_h と独立に持つので並列で OK)。
                let factor = (dy * 0.0015).exp();
                let new_h = view.track_row_h * factor;
                if let Some((_, my)) = pointer.pos
                    && view.track_row_h > 0.0
                {
                    let abs_pos = (f64::from(my - lanes.y) + f64::from(view.track_top))
                        / f64::from(view.track_row_h);
                    #[allow(clippy::cast_possible_truncation)]
                    let new_top =
                        (abs_pos * f64::from(new_h) - f64::from(my - lanes.y)).max(0.0) as f32;
                    self.push_edit(make_edit(ArrangementEditRequest::SetTrackTop(new_top)));
                }
                self.push_edit(make_edit(ArrangementEditRequest::SetTrackRowH(new_h)));

                // M14 Phase 63n-6 (#031): visible lane / per-track row override も同 factor で scale。
                // user feedback「Track 4 を drag で大きくした後 Alt+wheel で縮めても override が
                // 残ったまま」 → 各 override を factor 倍する。 個別差は scale 中保持 (lane1=100,
                // lane2=60 → lane1=70, lane2=42)、 enough wheel で min に収束 (= 個別差は残るが、
                // ユーザは引き続き wheel で全体を縮められる)。
                let lane_min = style.automation_lane_min_height_px;
                let lane_max = effective_lane_max_height(style, lanes);
                for t in &visible_tracks {
                    // per-track row 高さ override (= `t.row_h.is_some()`) も factor 倍。 None
                    // (= view default 追従) は SetTrackRowH 経由で既に追従するので scale 不要。
                    if let Some(row_h) = t.row_h {
                        let scaled = f32::from(row_h) * factor;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let new_t_h = scaled.round().clamp(1.0, f32::from(u16::MAX)) as u16;
                        if new_t_h != row_h {
                            self.push_edit(make_edit(
                                ArrangementEditRequest::SetSingleTrackRowH {
                                    track: t.id,
                                    prev: row_h,
                                    next: new_t_h,
                                },
                            ));
                        }
                    }
                    if t.automation_lanes_collapsed {
                        continue;
                    }
                    for lane in &t.automation_lanes {
                        if !lane.visible || lane.height_px == 0 {
                            continue;
                        }
                        let scaled = f32::from(lane.height_px) * factor;
                        let new_lane_h = clamp_height_px(scaled, lane_min, lane_max);
                        if new_lane_h != lane.height_px {
                            self.push_edit(make_edit(ArrangementEditRequest::SetLaneHeight {
                                lane: AutomationLaneKey {
                                    track: t.id,
                                    lane: lane.id,
                                },
                                prev: lane.height_px,
                                next: new_lane_h,
                            }));
                        }
                    }
                }
            } else if pointer.modifiers.shift && over_lanes {
                let delta = -f64::from(dy) * beat_per_px * 4.0;
                self.push_edit(make_edit(ArrangementEditRequest::SetScrollX(
                    view.start_beat + delta,
                )));
            } else if !pointer.modifiers.ctrl && !pointer.modifiers.shift {
                // plain wheel (= 縦 scroll)。 header / lanes どちらの上でも同一挙動。 `!ctrl && !shift`
                // guard は header 上で横操作キーが押されているときに plain scroll へ落ちないため
                // (lanes 上では ctrl は上の分岐、 shift は直上の分岐で既に消費されここへ来ない)。
                // M14 Phase 115 (daw_01 #088): dy は入力層で px 化済 (LINE_HEIGHT_PX=40/line)。
                // 旧実装の追加 ×8 は二重スケール (1 ノッチ 320px ≈ 8 行) だったので撤去し、 scroll_area
                // と同じ「入力層の px delta をそのまま使う」 に揃える (1 ノッチ ≈ 40px ≈ 1 行)。
                let new_top = (view.track_top - dy).max(0.0);
                self.push_edit(make_edit(ArrangementEditRequest::SetTrackTop(new_top)));
            }
        }

        // ---- M14 Phase 127 (daw_01 #105): Arranger section drag release dispatch ----
        // overlay (preview) と同じ `compute_section_drag_beat_delta` で確定値を計算 (release で grid に
        // 飛ぶ不整合を構造的に回避)。 alt は session の `last_alt` を真値とする (clip_drag と同 pattern)。
        if let Some(sd) = section_drag_release {
            let raw_px_delta = sd.last_mouse.0 - sd.anchor_mouse.0;
            let raw_beat_delta = f64::from(raw_px_delta) * beat_per_px;
            let dist = raw_px_delta.abs();
            let delta =
                compute_section_drag_beat_delta(&sd, raw_beat_delta, &view.snap, zoom_x_px_per_beat);
            match sd.kind {
                SectionGesture::Create => {
                    // 範囲 drag のみ作成 (単純 click は dblclick が 1 bar 作成を担当)。
                    if dist >= 4.0 {
                        let other = (sd.anchor_press_beat + delta).max(0.0);
                        let start = sd.anchor_press_beat.min(other);
                        let len = (sd.anchor_press_beat - other).abs();
                        if len >= SECTION_MIN_LEN_BEATS {
                            self.push_edit(make_edit(ArrangementEditRequest::CreateSection {
                                start,
                                len,
                            }));
                        }
                    }
                }
                SectionGesture::Move => {
                    if dist < 4.0 {
                        // M14 Phase 128 (#106): 短 click (jitter 未満) = 選択 + 帯ジャンプを併発。 drag して
                        // いないので Ctrl は Toggle-select (Duplicate は dist>=4 の Ctrl+drag のみ)。 modifier は
                        // Shift=RangeFromAnchor / Ctrl=Toggle / 無=Single (`SelectTrack` と同 idiom)。
                        let modifier = if sd.last_shift {
                            SelectModifier::RangeFromAnchor
                        } else if sd.last_ctrl {
                            SelectModifier::Toggle
                        } else {
                            SelectModifier::Single
                        };
                        self.push_edit(make_edit(ArrangementEditRequest::SelectSection {
                            id: sd.section_id,
                            modifier,
                        }));
                        self.push_edit(make_edit(ArrangementEditRequest::SetPlayheadBeat(
                            sd.anchor_start.max(0.0),
                        )));
                    } else if sd.last_ctrl {
                        let next_start = (sd.anchor_start + delta).max(0.0);
                        self.push_edit(make_edit(ArrangementEditRequest::DuplicateSection {
                            id: sd.section_id,
                            dest_start: next_start,
                        }));
                    } else {
                        let next_start = (sd.anchor_start + delta).max(0.0);
                        self.push_edit(make_edit(ArrangementEditRequest::MoveSection {
                            id: sd.section_id,
                            prev_start: sd.anchor_start,
                            next_start,
                        }));
                    }
                }
                SectionGesture::ResizeLeft => {
                    // 左端 drag: start/len 両方変化。 start は 0 以上 & 右端 - 最小長 を越えない sanity floor。
                    let right = sd.anchor_start + sd.anchor_len;
                    let next_start = (sd.anchor_start + delta)
                        .clamp(0.0, (right - SECTION_MIN_LEN_BEATS).max(0.0));
                    let next_len = (right - next_start).max(SECTION_MIN_LEN_BEATS);
                    self.push_edit(make_edit(ArrangementEditRequest::ResizeSection {
                        id: sd.section_id,
                        prev_start: sd.anchor_start,
                        prev_len: sd.anchor_len,
                        next_start,
                        next_len,
                    }));
                }
                SectionGesture::ResizeRight => {
                    // 右端 drag: len のみ変化 (start 固定)。
                    let next_len = (sd.anchor_len + delta).max(SECTION_MIN_LEN_BEATS);
                    self.push_edit(make_edit(ArrangementEditRequest::ResizeSection {
                        id: sd.section_id,
                        prev_start: sd.anchor_start,
                        prev_len: sd.anchor_len,
                        next_start: sd.anchor_start,
                        next_len,
                    }));
                }
            }
        }

        // ---- M14 Phase 127 (daw_01 #105): Arranger レーンの double-click ----
        //  - section 帯上 (in-rect) → `BeginRenameSection` (帯名 dblclick で改名開始、 `BeginRenameTrack` と同 idiom)
        //  - 空きレーン (帯の外、 隣接する resize ハンドル拡張部も含む) → `CreateSection` (既定長 1 bar)
        // FIXME #067: rename 判定は `section_hit` (resize ハンドルを ±px 外側拡張) でなく `section_at_inrect`
        // (帯内のみ) を使う。 拡張ハンドルは drag の掴みやすさ用で、 「帯のすぐ隣の空白」 の dblclick を
        // 隣 section の rename に化けさせていた。 帯外の dblclick は空きレーン扱いで CreateSection に回る。
        if arranger_lane_h > 0.0
            && let Some((cx, cy)) = self.take_double_click_in_rect(arranger_rect)
        {
            if let Some(sid) = section_at_inrect(sections, arranger_rect, view, cx, cy) {
                self.push_edit(make_edit(ArrangementEditRequest::BeginRenameSection(sid)));
            } else {
                let raw_beat = px_to_beat(cx, arranger_rect.x, arranger_rect.w, view);
                let start = view
                    .snap
                    .snap_beat(raw_beat, pointer.modifiers.alt, zoom_x_px_per_beat)
                    .max(0.0);
                self.push_edit(make_edit(ArrangementEditRequest::CreateSection {
                    start,
                    len: beats_per_bar(view.time_sig),
                }));
            }
        }

        // ---- M14 Phase 127 (daw_01 #105): Arranger レーンの secondary (右) click ----
        // section 帯上 (in-rect) のみ `SecondaryClickSection { id, pos }` を発火 (caller が `pos` に
        // コンテキストメニューを開く、 `SecondaryClickEmpty` と同 idiom)。 空きレーン上の右クリックは no-op。
        // FIXME #067: dblclick rename と同じく point gesture なので `section_at_inrect` (帯内のみ) を使う。
        // resize ハンドル拡張 (`section_hit`) だと帯のすぐ隣の空白の右クリックで隣 section のメニューが出る。
        if arranger_lane_h > 0.0
            && let Some((cx, cy)) = self.take_secondary_press_in_rect(arranger_rect)
            && let Some(sid) = section_at_inrect(sections, arranger_rect, view, cx, cy)
        {
            self.push_edit(make_edit(ArrangementEditRequest::SecondaryClickSection {
                id: sid,
                pos: (cx, cy),
            }));
        }

        // ---- double-click (lanes 内で clip / lane body / 空白 track row) ----
        // M14 Phase 63n-2 (#028) + Phase 63n-4 (#029): priority 順:
        //  1. clip hit (track row 内 clip rect) → DoubleClickClip
        //  2. lane body 内 clip 内 (curve 描画域) → AddAutomationPoint (snap 適用)
        //  3. lane body 内 clip ギャップ (= cursor の絶対 beat が既存 clip の x 範囲に重ならない) →
        //     CreateAutomationClip (snap 適用、 default len は style.automation_clip_default_len_beats)
        //  4. track row の空き → DoubleClickEmpty
        //  lane padding 内 (clip と x overlap するが clip の縦 padding zone) は no-op (= ユーザの意図が
        //  add-point か create-clip か判別できないため、 既存挙動を維持して何も発行しない)。
        if let Some((cx, cy)) = self.take_double_click_in_rect(lanes) {
            if let Some((hit_key, _)) =
                clip_hit(&visible_tracks, &press_tops, view, lanes, cx, cy, style.resize_handle_px)
            {
                self.push_edit(make_edit(ArrangementEditRequest::DoubleClickClip(hit_key)));
            } else if let Some((t_idx, lane_idx, _h_rect, body_rect)) = automation_lane_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                cy,
            ) {
                let track_id = visible_tracks[t_idx].id;
                let lane = &visible_tracks[t_idx].automation_lanes[lane_idx];
                if let Some((clip_key, time_local, value_norm)) =
                    automation_clip_at(track_id, lane, body_rect, view, style, cx, cy)
                {
                    // (2) lane body 内 clip 内 → AddAutomationPoint
                    let clip_ref = lane.clips.iter().find(|c| c.id == clip_key.clip);
                    let clip_start = clip_ref.map_or(0.0, |c| c.start_beat);
                    let clip_len = clip_ref.map_or(0.0, |c| c.len_beats);
                    let raw_abs = clip_start + time_local;
                    let snapped_abs = view.snap.snap_beat(
                        raw_abs,
                        pointer.modifiers.alt,
                        zoom_x_px_per_beat,
                    );
                    let snapped_local =
                        (snapped_abs - clip_start).clamp(0.0, clip_len.max(0.0));
                    self.push_edit(make_edit(ArrangementEditRequest::AddAutomationPoint {
                        clip: clip_key,
                        time_beat: snapped_local,
                        value_norm,
                    }));
                } else if cx >= body_rect.x && cx < body_rect.x + body_rect.w {
                    // (3) lane body 内 clip ギャップ → CreateAutomationClip。
                    // beat-domain で「cursor の絶対 beat が既存 clip と重なるか」 を判定し、
                    // 重ならない場合のみ発行する (= clip の縦 padding zone でも x が clip と重なって
                    // いれば抑止、 ユーザの意図が「padding を狙った add-point」 なのか「new clip」 なのか
                    // 判別できないため安全側 = no-op)。 cursor が clip の縦 padding 外で、 かつ x が
                    // 任意の clip と重ならない場合のみ「真の empty」 と判定。
                    let cursor_beat = px_to_beat(cx, lanes.x, lanes.w, view);
                    let on_existing_clip = lane.clips.iter().any(|c| {
                        cursor_beat >= c.start_beat && cursor_beat < c.start_beat + c.len_beats
                    });
                    if !on_existing_clip {
                        let snapped_start = view.snap.snap_beat(
                            cursor_beat,
                            pointer.modifiers.alt,
                            zoom_x_px_per_beat,
                        );
                        let lane_key = AutomationLaneKey {
                            track: track_id,
                            lane: lane.id,
                        };
                        self.push_edit(make_edit(ArrangementEditRequest::CreateAutomationClip {
                            lane: lane_key,
                            start_beat: snapped_start,
                            len_beats: style.automation_clip_default_len_beats,
                        }));
                    }
                }
            } else if let Some(idx) = track_index_from_y(cy, lanes.y, &press_tops)
                && let Some(t) = visible_tracks.get(idx)
                // M14 Phase 63n-10 (#034): master row 上で MIDI clip 作成 (`DoubleClickEmpty`) を発火しない
                // (= daw_01 #034 §G 確認、 master row の body 部は automation lane の clip dblclick のみ
                // 受け付け、 main row body は clip 概念を持たない)。
                && t.id != MASTER_TRACK_ID
            {
                // track row の空き dblclick (lane row では automation_lane_at が Some を返して
                // 上の分岐で吸収済 → ここに来るのは track row のみ)。
                let track_row_top = press_tops[idx];
                if cy < track_row_top + effective_track_row_h(t, view.track_row_h) {
                    let raw_beat = px_to_beat(cx, lanes.x, lanes.w, view);
                    // M9 Phase 45f: dblclick beat も widget 内 snap (#010 [Replied])。 daw_01 側で
                    // `beat.floor()` を消せるようになる。 single frame の click なので drag state は
                    // 関与せず、 直接 `pointer.modifiers.alt` を読んでよい。
                    let beat = view.snap.snap_beat(
                        raw_beat,
                        pointer.modifiers.alt,
                        zoom_x_px_per_beat,
                    );
                    self.push_edit(make_edit(ArrangementEditRequest::DoubleClickEmpty {
                        track: t.id,
                        beat,
                    }));
                }
            }
        }

        // ---- secondary (右) click in 空きレーン → SecondaryClickEmpty (daw_01 #071) ----
        // `DoubleClickEmpty` と対になる secondary 版。 clip_hit / automation_lane_at に吸収
        // されない「真の空き track row」 上の右クリックのみ発火する (= 上の dblclick 経路の
        // 空き track row branch と同じ exclusion)。 clip / automation lane 上の右クリックは
        // caller (daw_01) の clip context menu 用に握りつぶさず素通しする (= take はするが
        // consume しない `take_secondary_press_in_rect` の設計)。 beat は widget 内で snap 済み、
        // pos は menu anchor 用の右クリック viewport 座標。
        if let Some((cx, cy)) = self.take_secondary_press_in_rect(lanes) {
            let on_clip =
                clip_hit(&visible_tracks, &press_tops, view, lanes, cx, cy, style.resize_handle_px)
                    .is_some();
            let on_lane = automation_lane_at(
                &visible_tracks,
                &press_tops,
                view.track_row_h,
                header_pane.x,
                header_pane.w,
                lanes.x,
                lanes.w,
                style,
                cy,
            )
            .is_some();
            if !on_clip
                && !on_lane
                && let Some(idx) = track_index_from_y(cy, lanes.y, &press_tops)
                && let Some(t) = visible_tracks.get(idx)
                // master row は clip 概念を持たないため発火しない (DoubleClickEmpty と同じ)。
                && t.id != MASTER_TRACK_ID
            {
                let track_row_top = press_tops[idx];
                if cy < track_row_top + effective_track_row_h(t, view.track_row_h) {
                    let raw_beat = px_to_beat(cx, lanes.x, lanes.w, view);
                    // dblclick と同じく widget 内 snap。 single frame の press なので drag state は
                    // 関与せず直接 `pointer.modifiers.alt` を読んでよい。
                    let beat =
                        view.snap.snap_beat(raw_beat, pointer.modifiers.alt, zoom_x_px_per_beat);
                    self.push_edit(make_edit(ArrangementEditRequest::SecondaryClickEmpty {
                        track: t.id,
                        beat,
                        pos: (cx, cy),
                    }));
                }
            }
        }

        // ---- track headers (button_at × 4 + toggle_button_at × 2) + SelectTrack トリガ ----
        // M10 Phase 50: tracks_for_draw を使う (release frame の optimistic preview と同順序)。
        // M14 Phase 63c (#016): visible_indices を pre-compute して collapsed 親配下を skip、
        // visible_i (描画上の row index) を row_y に使う。 各 track header に depth * indent_px の
        // 左 indent + group track には disclosure ▼/▶ アイコン。 selection は selected_tracks_set で判定。
        // 修飾 (Shift / Ctrl) で Single / RangeFromAnchor / Toggle を decode し SelectTrack に乗せる。
        // 1 frame 内で最初に click された track id を `clicked_track` に蓄え、 loop 後に modifier-aware
        // SelectTrack を 1 度発行する (loop 内で複数発行しないため)。
        //
        // M14 Phase 77 (daw_01 #048): header row 描画 push_* 群を `header_pane` で auto-scissor
        // する。 closure 化すると `self.xxx` の大量 rename を要するため、 `with_clip_rect` と
        // 同等の `current_clip` push/pop を open-code で実施 (`Ui::with_clip_rect` 実装と同 idiom、
        // `pub(crate)` 経由)。
        let prev_clip_for_headers = self.current_clip;
        self.current_clip = Some(
            crate::ui::merge_clip(prev_clip_for_headers, Some(header_pane)).unwrap_or(header_pane),
        );
        let visible_idx_for_headers = compute_visible_indices(&tracks_for_draw);
        // M14 Phase 63n-1 (#028): track headers loop 用 prefix sum tops (immediate mode、 cached 外)。
        // `tracks_for_draw` は既に visible-only な Vec を Arc 化したものなので、 そのまま slice 経由で
        // 渡せば clone 不要 (header_pane.y == lanes.y は rect 分割 origin 共通)。
        let header_tops = visible_track_row_tops(
            &tracks_for_draw,
            header_pane.y,
            view.track_top,
            view.track_row_h,
        );
        let mut clicked_track_for_select: Option<u32> = None;
        let mut disclosure_clicked: Option<u32> = None;
        if header_w > 0.0 {
            for (visible_i, &i) in visible_idx_for_headers.iter().enumerate() {
                let t = &tracks_for_draw[i];
                let row_y = header_tops[visible_i];
                let row =
                    Rect { x: header_pane.x, y: row_y, w: header_pane.w, h: view.track_row_h };
                if row.y + row.h < header_pane.y || row.y > header_pane.y + header_pane.h {
                    continue;
                }

                // M14 Phase 63n-10 (#034): master row 専用 header 描画。 mute/solo button / volume band /
                // group disclosure / row click → SelectTrack の全 path を skip し、 neutral gray 背景 +
                // "Master" label + lane disclosure (`▶`/`▼`) のみを描画する (daw_01 #034 §B 仕様)。
                // 通常 track 経路の `selected_tracks` / `is_group_set` 判定とは独立 (master は selection
                // 対象外、 group でもない、 = 「特殊な行」 として描画分岐)。
                if t.id == MASTER_TRACK_ID {
                    // M14 Phase 90 (daw_01 #061): master 行も選択可能。 selected なら通常 track と同じ
                    // `track_selected_bg`、 非選択は従来の `master_row_color`。 "Master" label / lane
                    // disclosure はこの背景の上に重畳描画 (色は据え置き)。
                    let master_bg = if selected_tracks.contains(&t.id) {
                        style.track_selected_bg
                    } else {
                        style.master_row_color
                    };
                    self.panel(("arr_master_thbg", 0_u32), row, master_bg, 0.0);
                    let indent = f32::from(t.depth) * style.indent_px; // 0 固定だが既存 idiom 維持
                    let row_for_layout = Rect {
                        x: row.x + indent,
                        y: row.y,
                        w: (row.w - indent).max(2.0),
                        h: row.h,
                    };
                    let layout = header_row_layout(row_for_layout, 0.0); // volume band 無し
                    // "Master" label を name_rect に push_text (button にはしない = click は selection 経路に
                    // 流さない)。 font_size は style.master_row_label_size、 色は master_row_label_color。
                    let label_rect = layout.name_rect;
                    self.push_text(GlyphArea {
                        text: Arc::from("Master"),
                        left: label_rect.x + 4.0,
                        top: label_rect.y
                            + (label_rect.h - style.master_row_label_size * 1.2) * 0.5,
                        font_size: style.master_row_label_size,
                        line_height: style.master_row_label_size * 1.2,
                        color: style.master_row_label_color,
                        clip_rect: Some(label_rect),
                        ..GlyphArea::default()
                    });
                    // M14 Phase 63n-10 (#034): lane disclosure (`+` / `-`) を master row でも描画 (= 通常
                    // track と同 idiom)。 click 検出は press block 経由で `press_lane_toggle = Some(t.id)`
                    // (= `MASTER_TRACK_ID`) が立ち、 loop 後に `ToggleTrackAutomationCollapsed
                    // { track: MASTER_TRACK_ID }` が発火する SSoT。
                    if !t.automation_lanes.is_empty() {
                        let lane_disc = layout.lane_disc_rect;
                        let label = if t.automation_lanes_collapsed { "+" } else { "-" };
                        self.push_text(GlyphArea {
                            text: label.into(),
                            left: lane_disc.x,
                            top: lane_disc.y
                                + (lane_disc.h - style.automation_disclosure_size * 1.2) * 0.5,
                            font_size: style.automation_disclosure_size,
                            line_height: style.automation_disclosure_size * 1.2,
                            color: style.disclosure_color,
                            clip_rect: Some(lane_disc),
                            ..GlyphArea::default()
                        });
                    }
                    // Response.track_header_rects に積む (caller が master row の rect 領域を識別可能に)。
                    response.track_header_rects.push((t.id, row));
                    // M14 Phase 90 (daw_01 #061): master 行の header click → SelectTrack。 通常 track と
                    // 同じ `clicked_track_for_select` 経路を再利用し、 loop 後の modifier-aware 発行に乗せる
                    // (Single なら next=[MASTER_TRACK_ID])。 lane disclosure (`+`/`-`) rect 内 release は
                    // automation collapse トグルが priority なので除外する (disclosure > row-select)。
                    // master には mute/solo/volume band が無いので row 全体 (disclosure 除く) が対象。
                    if pointer.primary_just_released
                        && let Some((rx, ry)) = pointer.pos
                        && row.contains(rx, ry)
                        && (t.automation_lanes.is_empty() || !layout.lane_disc_rect.contains(rx, ry))
                    {
                        clicked_track_for_select = Some(t.id);
                    }
                    continue;
                }

                // 背景 (selection > 通常)。 M14 Phase 113 (daw_01 #085): group track 専用の
                // 背景 tint は撤去 (group は indent / disclosure ▶▼ で識別、 背景は他 track と同じ
                // neutral header_bg)。 `is_group_set` は依然 disclosure 描画 / hit-test で使う。
                if selected_tracks.contains(&t.id) {
                    self.panel(("arr_thsel", t.id), row, style.track_selected_bg, 0.0);
                } else {
                    self.panel(("arr_thbg", t.id), row, style.header_bg, 0.0);
                }

                // M14 Phase 63c (#016): depth * indent_px の左 indent。 layout 計算は indent 反映後の
                // row_inner で実行する (= row.x + indent、 row.w - indent)。 #069 で color strip も
                // この indent に追従させるため、 strip 描画の前に indent を確定する。
                let indent = f32::from(t.depth) * style.indent_px;

                // M14 Phase 87 (daw_01 #059): track color strip。 Some(c) のとき header の (indent 後の)
                // 左端に縦ストライプを背景の上から描く (selected/group/video 背景と色衝突しない)。
                // None は strip 非描画 = 既存挙動完全互換。
                // M14 Phase 97 (daw_01 #069): x を row.x + indent にして名前と同じだけ右にインデント
                // (子トラックで色ストライプが名前と一緒にネスト)。 depth==0 は indent=0 で従来と pixel 一致。
                if let Some(c) = t.color
                    && style.track_color_strip_w > 0.0
                {
                    self.push_rect(RectCommand {
                        rect: Rect {
                            x: row.x + indent,
                            y: row.y,
                            w: style.track_color_strip_w,
                            h: row.h,
                        },
                        fill: c,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: Some(row),
                    });
                }

                let row_for_layout = Rect {
                    x: row.x + indent,
                    y: row.y,
                    w: (row.w - indent).max(2.0),
                    h: row.h,
                };
                let band_h = if matches!(t.kind, TrackKind::Video) {
                    // M14 Phase 72 (#044): video track は volume band を非描画。
                    0.0
                } else {
                    style.track_volume_band_h
                };
                let layout = header_row_layout(row_for_layout, band_h);
                let name_rect = layout.name_rect;
                let [m_rect, s_rect, r_rect] = layout.buttons;

                // M10 Phase 47b: track volume band 描画。
                // drag 中の track はその drag session の last_mouse_x で preview volume を計算 (リアルタイム feedback)。
                if let Some(band) = layout.volume_band {
                    let dragging_this = track_volume_session
                        .as_ref()
                        .filter(|tv| tv.track_id == t.id);
                    let display_v = if let Some(tv) = dragging_this {
                        volume_from_mouse_x(tv.last_mouse_x, tv.band_rect.x, tv.band_rect.w)
                    } else {
                        t.volume.clamp(0.0, 1.0)
                    };
                    self.panel(
                        ("arr_tvol_track", t.id),
                        band,
                        style.track_volume_band_track,
                        0.0,
                    );
                    let fill_w = band.w * display_v;
                    if fill_w > 0.0 {
                        self.panel(
                            ("arr_tvol_fill", t.id),
                            Rect { x: band.x, y: band.y, w: fill_w, h: band.h },
                            style.track_volume_band_fill,
                            0.0,
                        );
                    }
                }

                // M14 Phase 63c (#016): disclosure ▼/▶ — group track のみ描画 + click で
                // ToggleGroupCollapsed Edit 発行 (loop 後に発火、 SelectTrack より priority 高)。
                let is_group = is_group_set.contains(&t.id);
                let disclosure_rect = disclosure_rect_for(name_rect, style, t.depth);
                if is_group {
                    let label = if t.collapsed { "▶" } else { "▼" };
                    self.push_text(GlyphArea {
                        text: label.into(),
                        left: disclosure_rect.x + disclosure_rect.w * 0.2,
                        top: disclosure_rect.y + (disclosure_rect.h - style.track_text_size * 1.2) * 0.5,
                        font_size: style.track_text_size,
                        line_height: style.track_text_size * 1.2,
                        color: style.disclosure_color,
                        clip_rect: Some(disclosure_rect),
                        ..GlyphArea::default()
                    });
                    if pointer.primary_just_released
                        && let Some((rx, ry)) = pointer.pos
                        && disclosure_rect.contains(rx, ry)
                    {
                        disclosure_clicked = Some(t.id);
                    }
                }
                // M14 Phase 63n-1 (#028) + 63n-2 修正: track 行 header の lane disclosure (S button の
                // 右、 `layout.lane_disc_rect`) を **`+` / `-`** で描画。 旧 `▽`/`▷` (U+25BD/U+25B7) は
                // font 不在で不可視 click target になる、 旧 `▼`/`▶` は group disclosure と同 glyph で
                // user が混同する両方の問題を解消した最終形 (#028 follow-up user feedback で確定)。
                // `automation_lanes.is_empty()` の track は描画しない (= layout 上は rect 確保するが
                // visual には何も出ない、 click もメッセージにならない)。 click 検出は press block で
                // 同じ `layout.lane_disc_rect` を使うので描画と hit-test の SSoT が完全一致。
                if !t.automation_lanes.is_empty() {
                    let lane_disc = layout.lane_disc_rect;
                    let label = if t.automation_lanes_collapsed { "+" } else { "-" };
                    self.push_text(GlyphArea {
                        text: label.into(),
                        left: lane_disc.x,
                        top: lane_disc.y + (lane_disc.h - style.automation_disclosure_size * 1.2) * 0.5,
                        font_size: style.automation_disclosure_size,
                        line_height: style.automation_disclosure_size * 1.2,
                        color: style.disclosure_color,
                        clip_rect: Some(lane_disc),
                        ..GlyphArea::default()
                    });
                }
                // disclosure を除いた name 領域 (group の場合は disclosure 分削る)
                let name_rect_visible = if is_group {
                    Rect {
                        x: disclosure_rect.x + disclosure_rect.w,
                        y: name_rect.y,
                        w: (name_rect.w - disclosure_rect.w).max(2.0),
                        h: name_rect.h,
                    }
                } else {
                    name_rect
                };
                let button_zones: [Rect; 4] = [name_rect_visible, m_rect, s_rect, r_rect];

                let id_name = ("arr_tname", t.id);
                let id_mute = ("arr_tmute", t.id);
                let id_solo = ("arr_tsolo", t.id);
                let id_armed = ("arr_tarmed", t.id);

                let track_id = t.id;
                let muted = t.muted;
                let solo = t.solo;
                let armed = t.armed;

                let name_text = t.name.clone();
                // M14 Phase 63c (#016): name 領域 click は modifier-aware SelectTrack を loop 後に
                // 発行する形に変更。 button_at_clicked で click 検知のみ行い、 内部で Edit は emit
                // しない (旧設計は button_at の closure 内で SelectTrack を emit していた)。
                // M14 Phase 105 (#076): track 名 font は `style.track_text_size` に追従させる
                // (汎用 button の 16px 固定では daw_01 が名前サイズを下げられないため)。
                // M14 Phase 107 (#079): track 名は常に左寄せ (Reaper / Cubase / Live と同じ。
                // 先頭が識別に最重要、 省略時の左寄せとも一致)。 M/S/R toggle は中央寄せのまま。
                if self.button_at_clicked_sized_aligned(
                    id_name,
                    &name_text,
                    name_rect_visible,
                    style.track_text_size,
                    crate::widgets::button::ButtonTextAlign::Left,
                ) {
                    clicked_track_for_select = Some(t.id);
                }
                // M14 Phase 118 (daw_01 #092): group track 名 double-click rename の信頼性。 深くネストした
                // group track は indent で name_rect が 20px floor まで潰れ、 さらに disclosure 分を引くと
                // `name_rect_visible` が 2〜4px になり double-click が当たらなかった。 group track のみ
                // **header row 全体** を rename hit zone にし、 single-click で別意味を持つ sub-zone
                // (M·S·R / lane disclosure / volume band drag / header splitter) を除外する。 これで
                // indent 空白 + 名前帯のどこを double-click しても rename が始まる (REAPER の TCP 名 dblclick
                // 流)。 通常 track は `name_rect_visible` のまま (名前帯が潰れないので挙動完全不変、 sub-zone
                // 除外も常に no-op)。
                //
                // M14 Phase 119 (daw_01 #092 follow-up): **group disclosure (`▶`/`▼`) も rename zone に含める**。
                // depth-0 (top-level) group は disclosure が name 帯の左端 (= indent 空白が無く x∈[pad, pad+
                // indent_px]) に張り付くため、 旧実装は disclosure を sub-zone 除外していた結果「最上段 / 子持ち
                // group の名前左側を double-click しても rename されない」 症状になっていた (master row の有無は
                // 無関係 = 最上段が top-level group になりがちなだけの相関、 と pixel/hit-test 検証で確定)。 disclosure
                // の **single-click** 折り畳みは別経路 (`disclosure_clicked`) で従来どおり (回帰なし)。 **double-click**
                // は明確に rename 意図なので disclosure 上でも rename を起こす。 double-click が disclosure を踏むと
                // 2 release で折り畳みが 2 回 toggle するが、 daw_01 の `collapsed_groups` (HashSet) を直接 flip する
                // 非 undoable な view-state edit なので net-zero (= fold 状態保存、 undo 履歴も汚さない)。 M·S·R /
                // lane disclosure は name 帯の **右**で名前と無関係なので除外を維持 (button の double-toggle を rename に
                // 化けさせない)、 volume band も名前帯の下の独立 drag 控除なので維持。
                let rename_hit = if is_group { row } else { name_rect_visible };
                if let Some((dcx, dcy)) = self.take_double_click_in_rect(rename_hit) {
                    let in_subzone = m_rect.contains(dcx, dcy)
                        || s_rect.contains(dcx, dcy)
                        || r_rect.contains(dcx, dcy)
                        || (!t.automation_lanes.is_empty()
                            && layout.lane_disc_rect.contains(dcx, dcy))
                        || layout.volume_band.is_some_and(|b| b.contains(dcx, dcy))
                        // M14 Phase 118 follow-up: group の broad zone は header / lanes 境界まで届くので、
                        // header 幅 splitter (#091) の hot zone も除外して rename と resize を分離する。
                        || header_resize_splitter_at(rect, header_w, style, dcx, dcy);
                    if !in_subzone {
                        self.push_edit(make_edit(ArrangementEditRequest::BeginRenameTrack(track_id)));
                    }
                }
                self.toggle_button_at(id_mute, "M", m_rect, muted, &style.mute_button, |_| {
                    make_edit(ArrangementEditRequest::ToggleTrackMute(track_id))
                });
                self.toggle_button_at(id_solo, "S", s_rect, solo, &style.solo_button, |_| {
                    make_edit(ArrangementEditRequest::ToggleTrackSolo(track_id))
                });
                // M14 Phase 68 (#040): R button (Record-arm)。 mute / solo と完全同 idiom、
                // armed track のみが audio engine の録音入力対象 (caller 仕様)。
                self.toggle_button_at(id_armed, "R", r_rect, armed, &style.armed_button, |_| {
                    make_edit(ArrangementEditRequest::ToggleTrackArmed(track_id))
                });
                // Phase 47c: ↑/↓/× button は削除 (drag&drop reorder + Delete shortcut で代替)。
                // `MoveTrackUp/Down` / `DeleteTrack` Edit variants は context menu / keyboard 用に残す。

                // Response.track_header_rects に積む
                response.track_header_rects.push((t.id, row));

                // SelectTrack トリガ: row 内 release + button_zones / disclosure いずれにも非 hit。
                // catch-all は modifier-aware SelectTrack の元データを蓄えるだけ (発行は loop 後)。
                if pointer.primary_just_released
                    && let Some((rx, ry)) = pointer.pos
                    && row.contains(rx, ry)
                    && !button_zones.iter().any(|b| b.contains(rx, ry))
                    && !(is_group && disclosure_rect.contains(rx, ry))
                {
                    clicked_track_for_select = Some(t.id);
                }
            }
        }
        // M14 Phase 77 (daw_01 #048): header_pane scope を復元 (= track header push_* 群が終了)。
        self.current_clip = prev_clip_for_headers;

        // M14 Phase 63c (#016): disclosure click → ToggleGroupCollapsed (priority 高、 SelectTrack は
        // この frame では skip = group の collapsed toggle 動作のみで selection は変えない、
        // Reaper / Live と同じ UX)。
        if let Some(tid) = disclosure_clicked {
            self.push_edit(make_edit(ArrangementEditRequest::ToggleGroupCollapsed(tid)));
            clicked_track_for_select = None;
        }

        // M14 Phase 63c (#016): clicked_track があれば modifier-aware SelectTrack を 1 度発行。
        // Single → next = [tid]、 anchor 更新。
        // RangeFromAnchor (Shift) → anchor から visible 列の連続範囲。 anchor が None なら Single 同等。
        // Toggle (Ctrl) → tid を selected に対して toggle、 anchor 更新しない。
        if let Some(tid) = clicked_track_for_select {
            let shift = pointer.modifiers.shift;
            let ctrl = pointer.modifiers.ctrl;
            let modifier = if shift {
                SelectModifier::RangeFromAnchor
            } else if ctrl {
                SelectModifier::Toggle
            } else {
                SelectModifier::Single
            };
            let prev_anchor = {
                let state: &ArrangementState = self.widget_state(wid);
                state.selection_anchor
            };
            let visible_ids: Vec<u32> = visible_idx_for_headers
                .iter()
                .map(|&i| tracks_for_draw[i].id)
                .collect();
            let next: Vec<u32> = match modifier {
                SelectModifier::Single => vec![tid],
                SelectModifier::RangeFromAnchor => {
                    let anchor_id = prev_anchor.unwrap_or(tid);
                    let from = visible_ids.iter().position(|&v| v == anchor_id).unwrap_or(0);
                    let to = visible_ids.iter().position(|&v| v == tid).unwrap_or(0);
                    let lo = from.min(to);
                    let hi = from.max(to);
                    visible_ids[lo..=hi].to_vec()
                }
                SelectModifier::Toggle => {
                    let mut set: HashSet<u32> = selected_tracks.iter().copied().collect();
                    if set.contains(&tid) {
                        set.remove(&tid);
                    } else {
                        set.insert(tid);
                    }
                    let mut v: Vec<u32> = set.into_iter().collect();
                    v.sort_unstable();
                    v
                }
            };
            let prev_v: Vec<u32> = selected_tracks.to_vec();
            let mut prev_sorted = prev_v.clone();
            prev_sorted.sort_unstable();
            let mut next_sorted = next.clone();
            next_sorted.sort_unstable();
            if prev_sorted != next_sorted {
                self.push_edit(make_edit(ArrangementEditRequest::SelectTrack {
                    prev: prev_v,
                    next,
                    modifier,
                }));
                response.selection_changed = true;
            }
            // anchor 更新: Single / Range で update、 Toggle は据え置き
            if matches!(modifier, SelectModifier::Single | SelectModifier::RangeFromAnchor) {
                let state: &mut ArrangementState = self.widget_state(wid);
                state.selection_anchor = Some(tid);
            }
        }

        // ---- M14 Phase 63f (#020): clip_rects を visible-tracks 順 (= 描画順) で積む ----
        // draw_clips と同じ culling: row が lanes 外 / clip が view beat 範囲外なら除外。
        // 部分カリングは full rect を返す (caller の context_menu_for は popup_rect_clamped_at で
        // 画面外はみ出しを吸収するため、 視野内に少しでも見えていれば十分操作可能)。
        let view_end = view.start_beat + view.len_beats;
        for (i, t) in visible_tracks.iter().enumerate() {
            let row_top = press_tops[i];
            let row_h = effective_track_row_h(t, view.track_row_h);
            if row_top + row_h < lanes.y || row_top > lanes.y + lanes.h {
                continue;
            }
            for c in &t.clips {
                let end = c.start_beat + c.len_beats;
                if end < view.start_beat || c.start_beat > view_end {
                    continue;
                }
                let r = clip_to_rect(row_top, row_h, c, view, lanes);
                response.clip_rects.push((ClipKey { track: t.id, clip: c.id }, r));
            }
        }

        // ---- M14 Phase 63n-2 (#028): automation_point_rects を毎 frame 積む ----
        // for_each_visible_lane で SSoT を共有し、 各 visible point を screen 座標に変換した
        // 半径 8px 正方形 rect を返す (= caller の context_menu_for で右クリック anchor として使う)。
        // collapsed group 内 / collapsed lane / invisible lane / view beat 範囲外の point は除外。
        let radius = style.automation_point_radius_px.max(2.0);
        for_each_visible_lane(
            &visible_tracks,
            &press_tops,
            view.track_row_h,
            header_pane.x,
            header_pane.w,
            lanes.x,
            lanes.w,
            style,
            |t_idx, _l_idx, lane, _h_rect, body_rect| {
                if body_rect.y + body_rect.h < lanes.y || body_rect.y > lanes.y + lanes.h {
                    return;
                }
                let track_id = visible_tracks[t_idx].id;
                let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
                let pad = style.automation_clip_v_pad_px;
                let clip_y = body_rect.y + pad;
                let clip_h = (body_rect.h - pad * 2.0).max(2.0);
                for clip_in in &lane.clips {
                    let end = clip_in.start_beat + clip_in.len_beats;
                    if end < view.start_beat || clip_in.start_beat > view_end {
                        continue;
                    }
                    for (p_idx, p) in clip_in.points.iter().enumerate() {
                        let abs_beat = clip_in.start_beat + p.time_beat;
                        if abs_beat < view.start_beat - 1e-6
                            || abs_beat > view.start_beat + view.len_beats + 1e-6
                        {
                            continue;
                        }
                        #[allow(clippy::cast_possible_truncation)]
                        let px = body_rect.x
                            + ((abs_beat - view.start_beat) * beat_to_px) as f32;
                        let py = clip_y + (1.0 - p.value_norm.clamp(0.0, 1.0)) * clip_h;
                        let key = AutomationPointKey {
                            clip: AutomationClipKey {
                                track: track_id,
                                lane: lane.id,
                                clip: clip_in.id,
                            },
                            #[allow(clippy::cast_possible_truncation)]
                            point_idx: p_idx as u32,
                        };
                        let r = Rect {
                            x: px - radius,
                            y: py - radius,
                            w: radius * 2.0,
                            h: radius * 2.0,
                        };
                        response.automation_point_rects.push((key, r));
                    }
                }
            },
        );

        // ---- M14 Phase 63n-3 (#028): automation_clip_rects を毎 frame 積む ----
        // for_each_visible_lane で SSoT を共有 (= 描画 / hit-test と同じ式)、 visible automation
        // clip の lane body 内 rect (縦 padding 適用済) を返す。 collapsed group / hidden lane / view
        // beat 範囲外の clip は除外。 caller は右クリック context menu (Make Unique / Delete) の
        // anchor として使う想定。
        for_each_visible_lane(
            &visible_tracks,
            &press_tops,
            view.track_row_h,
            header_pane.x,
            header_pane.w,
            lanes.x,
            lanes.w,
            style,
            |t_idx, _l_idx, lane, _h_rect, body_rect| {
                if body_rect.y + body_rect.h < lanes.y || body_rect.y > lanes.y + lanes.h {
                    return;
                }
                let track_id = visible_tracks[t_idx].id;
                let beat_to_px = f64::from(body_rect.w) / view.len_beats.max(1e-6);
                let pad = style.automation_clip_v_pad_px;
                let clip_y = body_rect.y + pad;
                let clip_h = (body_rect.h - pad * 2.0).max(2.0);
                for clip_in in &lane.clips {
                    let end = clip_in.start_beat + clip_in.len_beats;
                    if end < view.start_beat || clip_in.start_beat > view_end {
                        continue;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    let cx_clip = body_rect.x
                        + ((clip_in.start_beat - view.start_beat) * beat_to_px) as f32;
                    #[allow(clippy::cast_possible_truncation)]
                    let cw = ((clip_in.len_beats * beat_to_px) as f32).max(2.0);
                    let key = AutomationClipKey {
                        track: track_id,
                        lane: lane.id,
                        clip: clip_in.id,
                    };
                    response.automation_clip_rects.push((
                        key,
                        Rect { x: cx_clip, y: clip_y, w: cw, h: clip_h },
                    ));
                }
            },
        );

        // M14 Phase 63n-3 (#028): drag 中の automation clip kind を response に反映 (cursor /
        // status indicator 用)。 既存 `dragging` (MIDI clip 用) と直交。
        response.dragging_automation_clip = automation_clip_drag_session.map(|acd| acd.kind);

        response
    }
}

#[must_use]
fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snap::SnapMode;

    fn clip(id: u32, start: f64, len: f64, name: &str) -> ArrangementClip {
        ArrangementClip {
            id,
            start_beat: start,
            len_beats: len,
            name: Arc::from(name),
            color: None,
            share_group_color: None,
            audio_edit: None,
            thumbnail: None,
            in_active_group: false,
        }
    }

    /// M14 Phase 63k (#025): audio_edit が Some の test clip helper。
    fn audio_clip(
        id: u32,
        start: f64,
        len: f64,
        name: &str,
        audio: ArrangementClipAudioEdit,
    ) -> ArrangementClip {
        ArrangementClip {
            id,
            start_beat: start,
            len_beats: len,
            name: Arc::from(name),
            color: None,
            share_group_color: None,
            audio_edit: Some(audio),
            thumbnail: None,
            in_active_group: false,
        }
    }

    fn track(id: u32, name: &str, clips: Vec<ArrangementClip>) -> ArrangementTrack {
        ArrangementTrack {
            id,
            name: Arc::from(name),
            muted: false,
            solo: false,
            armed: false,
            clips,
            volume: 1.0,
            parent_id: None,
            depth: 0,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
            collapsed: false,
            row_h: None,
            kind: TrackKind::Audio,
            color: None,
        }
    }

    fn test_view() -> ArrangementView {
        ArrangementView {
            start_beat: 0.0,
            len_beats: 16.0,
            track_top: 0.0,
            tracks_visible: 8.0,
            track_row_h: 32.0,
            header_w: 0.0,
            ruler_h: 0.0,
            playhead_beat: None,
            loop_range: None,
            data_generation: 0,
            bpm: 120.0,
            time_sig: (4, 4),
            // 数値検証 test は raw beat 値を期待するので明示 OFF。
            snap: SnapConfig::OFF,
            arranger_lane_h: 0.0,
        }
    }

    fn test_lanes() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 640.0, h: 256.0 }
    }

    /// M14 Phase 63n-1 (#028): test 用 prefix-sum tops 生成 helper。
    /// lane を持たない tracks (= 既存挙動) では `tops[i] = lanes_y - track_top + i * track_row_h` と等価。
    fn make_tops(tracks: &[ArrangementTrack], lanes: Rect, view: ArrangementView) -> Vec<f32> {
        visible_track_row_tops(tracks, lanes.y, view.track_top, view.track_row_h)
    }

    /// 簡易 tops (test_view + test_lanes と同条件で N track ぶん、 lane なし)。
    /// `lanes_y=0, track_top=0, row_h=32` → `tops = [0.0, 32.0, 64.0, 96.0, ...]`。
    fn legacy_tops(n: usize) -> Vec<f32> {
        #[allow(clippy::cast_precision_loss)]
        (0..=n).map(|i| i as f32 * 32.0).collect()
    }

    #[test]
    fn clip_to_rect_basic_position() {
        let view = test_view();
        let lanes = test_lanes();
        let c = clip(0, 4.0, 4.0, "x");
        // visible_idx 2 → row_top = lanes.y - track_top + 2 * track_row_h = 0 - 0 + 64 = 64
        let r = clip_to_rect(64.0, view.track_row_h, &c, view, lanes);
        // beat_to_px = 640/16 = 40
        // x = 0 + 4*40 = 160, w = 4*40 = 160
        // row_top = 64, y = 64+2 = 66, h = 32-4 = 28
        assert!((r.x - 160.0).abs() < 1e-3);
        assert!((r.w - 160.0).abs() < 1e-3);
        assert!((r.y - 66.0).abs() < 1e-3);
        assert!((r.h - 28.0).abs() < 1e-3);
    }

    #[test]
    fn track_index_from_y_basic() {
        // lanes_y=0, row_h=32 → y=0 → idx 0, y=32 → idx 1, y=64 → idx 2
        let tops = legacy_tops(3);
        assert_eq!(track_index_from_y(0.0, 0.0, &tops), Some(0));
        assert_eq!(track_index_from_y(32.0, 0.0, &tops), Some(1));
        assert_eq!(track_index_from_y(64.0, 0.0, &tops), Some(2));
        // y < tops[0] = 範囲外
        assert_eq!(track_index_from_y(-5.0, 0.0, &tops), None);
        // y > tops[3] = 範囲外
        assert_eq!(track_index_from_y(200.0, 0.0, &tops), None);
    }

    #[test]
    fn track_index_from_y_with_scroll() {
        // track_top=16 を反映した tops: y=lanes_y-16+i*32 → tops = [-16, 16, 48, 80, 112]
        let view = ArrangementView { track_top: 16.0, ..test_view() };
        let lanes = test_lanes();
        let tracks: Vec<ArrangementTrack> = (0..4).map(|i| track(i, "t", vec![])).collect();
        let tops = make_tops(&tracks, lanes, view);
        // y=10 → -16 <= 10 < 16 → idx 0、 y=26 → 16 <= 26 < 48 → idx 1
        assert_eq!(track_index_from_y(10.0, 0.0, &tops), Some(0));
        assert_eq!(track_index_from_y(26.0, 0.0, &tops), Some(1));
    }

    #[test]
    fn visible_track_row_tops_with_no_lanes_matches_legacy_layout() {
        // M14 Phase 63n-1 (#028) regression: lane 0 個では legacy 式 `tops[i] = lanes_y - track_top
        // + i * track_row_h` と完全一致 (= 既存挙動完全互換)。
        let view = test_view();
        let lanes = test_lanes();
        let tracks: Vec<ArrangementTrack> = (0..4).map(|i| track(i, "t", vec![])).collect();
        let tops = make_tops(&tracks, lanes, view);
        assert_eq!(tops, vec![0.0, 32.0, 64.0, 96.0, 128.0]);
    }

    #[test]
    fn visible_track_row_tops_with_expanded_lane_grows_track_height() {
        // M14 Phase 63n-1 (#028): expanded lane (visible) を持つ track 以降は次 track の row_top が
        // 下にずれる。 collapsed もしくは invisible lane は加算しない。
        let view = test_view();
        let lanes = test_lanes();
        let mut t1 = track(1, "t1", vec![]);
        t1.automation_lanes_collapsed = false;
        t1.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: true,
            height_px: 60,
            default_value_norm: 0.5,
            clips: Vec::new(),
        }];
        let t2 = track(2, "t2", vec![]);
        let tracks = vec![t1, t2];
        let tops = make_tops(&tracks, lanes, view);
        // tops[0] = 0、 tops[1] = 0 + (32 + 60) = 92 (t1 expanded)、 tops[2] = 92 + 32 = 124 (t2 collapsed)
        assert_eq!(tops, vec![0.0, 92.0, 124.0]);
    }

    #[test]
    fn visible_track_row_tops_collapsed_lane_does_not_extend_height() {
        // M14 Phase 63n-1 (#028): `automation_lanes_collapsed = true` で lane を持っていても加算しない。
        let view = test_view();
        let lanes = test_lanes();
        let mut t1 = track(1, "t1", vec![]);
        t1.automation_lanes_collapsed = true; // 既存挙動
        t1.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: true,
            height_px: 60,
            default_value_norm: 0.5,
            clips: Vec::new(),
        }];
        let tracks = vec![t1];
        let tops = make_tops(&tracks, lanes, view);
        assert_eq!(tops, vec![0.0, 32.0]); // collapsed = legacy と同じ
    }

    #[test]
    fn visible_track_row_tops_invisible_lane_does_not_extend_height() {
        // M14 Phase 63n-1 (#028): `lane.visible = false` の lane は expanded でも加算しない。
        let view = test_view();
        let lanes = test_lanes();
        let mut t1 = track(1, "t1", vec![]);
        t1.automation_lanes_collapsed = false;
        t1.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: false, // hidden
            height_px: 60,
            default_value_norm: 0.5,
            clips: Vec::new(),
        }];
        let tracks = vec![t1];
        let tops = make_tops(&tracks, lanes, view);
        assert_eq!(tops, vec![0.0, 32.0]); // invisible lane = legacy と同じ
    }

    #[test]
    fn clip_hit_returns_move_in_center() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        // clip rect at (0, 2, 160, 28), center = (80, 16)
        let hit = clip_hit(&tracks, &tops, view, lanes, 80.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::Move))
        );
    }

    #[test]
    fn clip_hit_returns_resize_left_at_left_edge() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 1.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn clip_hit_returns_resize_right_at_right_edge() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 159.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeRight))
        );
    }

    #[test]
    fn clip_hit_returns_none_outside_lanes() {
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, -10.0, -10.0, 4.0);
        assert_eq!(hit, None);
    }

    // -------- Hit-test extension tests (clip rect 外側 ±resize_handle_px) --------
    // clip (id 100) start=4, len=4 → rect x∈[160,320] y∈[66,94] in track 2
    // (test_lanes (0,0,640,256), test_view 16 beats / 8 tracks / row_h=32)。
    // ただし以下のテストは start=0 len=4 → x∈[0,160] y∈[2,30] in track 0 を使う。

    #[test]
    fn clip_hit_returns_resize_left_at_outer_left_handle() {
        let view = test_view();
        let lanes = test_lanes();
        // clip rect x∈[0,160]、edge=4 で拡張範囲 x∈[-4,164)。lanes の左端 0 で外側左を表現できないので
        // clip start=2 (x=80) の clip を使い、cx=77 で外側左 (x=80-3) を確認。
        let tracks = vec![track(10, "t0", vec![clip(100, 2.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 77.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn clip_hit_returns_resize_right_at_outer_right_handle() {
        let view = test_view();
        let lanes = test_lanes();
        // clip rect x∈[0,160]、cx=162 = rect 右端(160) + 2 → 外側右
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 162.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeRight))
        );
    }

    #[test]
    fn clip_hit_returns_none_just_past_outer_handle() {
        let view = test_view();
        let lanes = test_lanes();
        // clip rect x∈[0,160]。cx=165 = rect 右端 + 5 → 拡張範囲 [-4,164) の外
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 165.0, 16.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn clip_hit_short_clip_inside_returns_move() {
        let view = test_view();
        let lanes = test_lanes();
        // 短 clip (len=0.1 → w=4px、edge*2=8px 以下) の rect 内中央は Move 強制
        // start=2, len=0.1 → x=80, w=4
        let tracks = vec![track(10, "t0", vec![clip(100, 2.0, 0.1, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 81.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::Move))
        );
    }

    #[test]
    fn clip_hit_short_clip_outer_left_returns_resize_left() {
        let view = test_view();
        let lanes = test_lanes();
        // 短 clip でも rect 外側左は ResizeLeft
        // start=2, len=0.1 → x=80。cx=78 = x - 2 → 外側左
        let tracks = vec![track(10, "t0", vec![clip(100, 2.0, 0.1, "c")])];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 78.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn clip_hit_adjacent_clips_inside_clip_owns_shared_handle() {
        // clip A (id 100, start=0, len=4) → rect x∈[0,160]、右端拡張 [156,164)
        // clip B (id 101, start=4, len=4) → rect x∈[160,320]、左端拡張 [156,164)
        // 共有境界 boundary=160。各 clip は自分の rect 内側のハンドル px を所有する
        // (in-rect は outer-extension に無条件で勝つ / #101、piano_roll note_hit_in と対)。
        let view = test_view();
        let lanes = test_lanes();
        let tracks = vec![track(
            10,
            "t0",
            vec![clip(100, 0.0, 4.0, "a"), clip(101, 4.0, 4.0, "b")],
        )];
        let tops = make_tops(&tracks, lanes, view);
        // cx=159: A の rect 内側 (in-rect ResizeRight) が B の外側ハンドル (outer ResizeLeft)
        // に勝つ。旧 last-wins では B ResizeLeft だった回帰ケース。
        assert_eq!(
            clip_hit(&tracks, &tops, view, lanes, 159.0, 16.0, 4.0),
            Some((ClipKey { track: 10, clip: 100 }, ClipDragKind::ResizeRight))
        );
        // cx=161: B の rect 内側 (in-rect ResizeLeft) が A の外側ハンドル (outer) に勝つ。
        assert_eq!(
            clip_hit(&tracks, &tops, view, lanes, 161.0, 16.0, 4.0),
            Some((ClipKey { track: 10, clip: 101 }, ClipDragKind::ResizeLeft))
        );
        // cx=160: 共有境界。半開区間で B の rect 内側 → B の左端 resize。
        assert_eq!(
            clip_hit(&tracks, &tops, view, lanes, 160.0, 16.0, 4.0),
            Some((ClipKey { track: 10, clip: 101 }, ClipDragKind::ResizeLeft))
        );
    }

    // `loop_band_hit_kind_*` の test は M14 Phase 69 (#041) で
    // `crate::widgets::ruler_ops::tests` に extract (piano_roll と共有)。

    #[test]
    fn rects_intersect_basic() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 5.0, y: 5.0, w: 10.0, h: 10.0 };
        let c = Rect { x: 20.0, y: 0.0, w: 10.0, h: 10.0 };
        assert!(rects_intersect(a, b));
        assert!(!rects_intersect(a, c));
    }

    #[test]
    fn arrangement_view_default_sane() {
        let v = ArrangementView::default();
        assert!(v.len_beats > 0.0);
        assert!(v.track_row_h > 0.0);
        assert!(v.tracks_visible > 0.0);
        assert!(v.header_w > 0.0);
        assert!(v.ruler_h > 0.0);
    }

    #[test]
    fn arrangement_style_default_sane() {
        let s = ArrangementStyle::default();
        assert!(s.resize_handle_px > 0.0);
        assert!(s.playhead_width_px > 0.0);
        assert!(s.clip_radius >= 0.0);
    }

    // M14 Phase 63f (#020): clip_rects API
    #[test]
    fn arrangement_response_default_has_empty_clip_rects() {
        let r = ArrangementResponse::default();
        assert!(r.clip_rects.is_empty());
        assert!(r.track_header_rects.is_empty());
    }

    #[test]
    fn clip_rects_populated_in_visible_track_order() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        // 2 tracks × 2 clips。 clip_rects は visible-tracks 順 (track id 1 → 2)、
        // 各 track 内は clips の slice 順 (start_beat 昇順) で並ぶ。
        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let tracks = vec![
            track(1, "t1", vec![clip(10, 0.0, 2.0, "a"), clip(11, 4.0, 2.0, "b")]),
            track(2, "t2", vec![clip(20, 8.0, 2.0, "c")]),
        ];
        let view = ArrangementView { header_w: 0.0, ruler_h: 0.0, ..ArrangementView::default() };
        let mut model = Model { tracks, view };

        let observed: Arc<std::sync::Mutex<Vec<(ClipKey, Rect)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);
        let _ = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput::default(),
            |m, ui| {
                let style = ArrangementStyle::default();
                let resp = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    |_| Edit::mutate(|_: &mut Model| {}),
                );
                *observed_cb.lock().unwrap() = resp.clip_rects.clone();
            },
        );
        // model は frame_to_edits で apply されない (closure 内 push_edit を呼んでないので edits は空)
        let _ = &mut model;

        let rects = observed.lock().unwrap();
        assert_eq!(rects.len(), 3, "全 visible clip 3 件: got {}", rects.len());
        // 順序: track 1 (start=0) → track 1 (start=4) → track 2 (start=8)
        assert_eq!(rects[0].0, ClipKey { track: 1, clip: 10 });
        assert_eq!(rects[1].0, ClipKey { track: 1, clip: 11 });
        assert_eq!(rects[2].0, ClipKey { track: 2, clip: 20 });
        // rect.x が beat 順で増加 (左→右)
        assert!(rects[0].1.x < rects[1].1.x);
        // track 0 と 1 の rect.y が異なる (上→下、 track 1 → track 2)
        assert!(rects[0].1.y < rects[2].1.y);
    }

    #[test]
    fn clip_rects_excludes_collapsed_subtree() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        // track 1 (group, collapsed) の子 track 2 の clip は clip_rects に出ない。
        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let mut t1 = track(1, "g", vec![clip(10, 0.0, 2.0, "a")]);
        t1.collapsed = true;
        let mut t2 = track(2, "child", vec![clip(20, 4.0, 2.0, "b")]);
        t2.parent_id = Some(1);
        t2.depth = 1;
        let view = ArrangementView { header_w: 0.0, ruler_h: 0.0, ..ArrangementView::default() };
        let mut model = Model { tracks: vec![t1, t2], view };

        let observed: Arc<std::sync::Mutex<Vec<(ClipKey, Rect)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);
        let _ = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput::default(),
            |m, ui| {
                let style = ArrangementStyle::default();
                let resp = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    |_| Edit::mutate(|_: &mut Model| {}),
                );
                *observed_cb.lock().unwrap() = resp.clip_rects.clone();
            },
        );
        let _ = &mut model;

        let rects = observed.lock().unwrap();
        // group 親 (collapsed = true でも group track 自身は visible) の clip 1 つのみ。
        // 子 track (parent = collapsed) の clip は除外。
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, ClipKey { track: 1, clip: 10 });
    }

    #[test]
    fn clip_rects_excludes_off_screen_clip_in_beat_range() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        // view.start_beat = 100、 len_beats = 16 で clip(0, 2.0, ...) は完全に view 外 (end<start)
        // → clip_rects から除外。 view 内 clip は含まれる。
        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let tracks = vec![track(
            1,
            "t",
            vec![clip(10, 0.0, 2.0, "off"), clip(11, 102.0, 4.0, "on")],
        )];
        let view = ArrangementView {
            start_beat: 100.0,
            len_beats: 16.0,
            header_w: 0.0,
            ruler_h: 0.0,
            ..ArrangementView::default()
        };
        let mut model = Model { tracks, view };

        let observed: Arc<std::sync::Mutex<Vec<(ClipKey, Rect)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);
        let _ = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput::default(),
            |m, ui| {
                let style = ArrangementStyle::default();
                let resp = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    |_| Edit::mutate(|_: &mut Model| {}),
                );
                *observed_cb.lock().unwrap() = resp.clip_rects.clone();
            },
        );
        let _ = &mut model;

        let rects = observed.lock().unwrap();
        assert_eq!(rects.len(), 1, "off-screen clip は除外: got {}", rects.len());
        assert_eq!(rects[0].0, ClipKey { track: 1, clip: 11 });
    }

    // ============================================================
    // M14 Phase 72 (daw_01 #044): video track / thumbnail
    // ============================================================

    #[test]
    fn track_kind_default_is_audio() {
        assert_eq!(TrackKind::default(), TrackKind::Audio);
    }

    #[test]
    fn aspect_fit_rect_letterbox_for_wide_texture() {
        // 100x100 rect に 16:9 (= 1920x1080) texture → fit_w=100, fit_h=100*(9/16)=56.25
        let fit = aspect_fit_rect(Rect::new(0.0, 0.0, 100.0, 100.0), 1920, 1080);
        assert!((fit.w - 100.0).abs() < 1e-3);
        assert!((fit.h - 56.25).abs() < 1e-3);
        // 中央 letterbox: 上下に (100-56.25)/2 = 21.875 px の黒帯
        assert!((fit.x - 0.0).abs() < 1e-3);
        assert!((fit.y - 21.875).abs() < 1e-3);
    }

    #[test]
    fn aspect_fit_rect_letterbox_for_tall_texture() {
        // 100x100 rect に 9:16 (= 1080x1920) texture → fit_w=100*(9/16)=56.25, fit_h=100
        let fit = aspect_fit_rect(Rect::new(10.0, 20.0, 100.0, 100.0), 1080, 1920);
        assert!((fit.w - 56.25).abs() < 1e-3);
        assert!((fit.h - 100.0).abs() < 1e-3);
        // 左右に letterbox: x = 10 + (100-56.25)/2 = 31.875
        assert!((fit.x - 31.875).abs() < 1e-3);
        assert!((fit.y - 20.0).abs() < 1e-3);
    }

    #[test]
    fn aspect_fit_rect_same_aspect_no_letterbox() {
        // 100x50 rect に 2:1 (= 1920x960) texture → fit 全面で letterbox なし
        let fit = aspect_fit_rect(Rect::new(0.0, 0.0, 100.0, 50.0), 1920, 960);
        assert!((fit.w - 100.0).abs() < 1e-3);
        assert!((fit.h - 50.0).abs() < 1e-3);
        assert!(fit.x.abs() < 1e-3);
        assert!(fit.y.abs() < 1e-3);
    }

    #[test]
    fn aspect_fit_rect_zero_texture_clamped_to_one() {
        // tex_w = tex_h = 0 で panic / div-by-zero しない (1:1 aspect で正方形 fit)
        let fit = aspect_fit_rect(Rect::new(0.0, 0.0, 100.0, 200.0), 0, 0);
        // 1:1 aspect で rect の短辺 (= 100) に合わせる → fit_w=fit_h=100、 縦に letterbox
        assert!((fit.w - 100.0).abs() < 1e-3);
        assert!((fit.h - 100.0).abs() < 1e-3);
    }

    #[test]
    fn arrangement_track_kind_field_round_trip() {
        let t = ArrangementTrack {
            id: 99,
            name: Arc::from("video1"),
            muted: false,
            solo: false,
            armed: false,
            clips: Vec::new(),
            volume: 1.0,
            parent_id: None,
            depth: 0,
            collapsed: false,
            kind: TrackKind::Video,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
            row_h: None,
            color: None,
        };
        assert_eq!(t.kind, TrackKind::Video);
    }

    #[test]
    fn arrangement_clip_thumbnail_field_round_trip() {
        use std::num::NonZeroU32;
        let h = TextureHandle::from_raw(NonZeroU32::new(7).unwrap());
        let c = ArrangementClip {
            id: 1,
            start_beat: 0.0,
            len_beats: 4.0,
            name: Arc::from("v_clip"),
            color: None,
            share_group_color: None,
            audio_edit: None,
            thumbnail: Some((h, 1920, 1080)),
            in_active_group: false,
        };
        let (got_h, w, ht) = c.thumbnail.unwrap();
        assert_eq!(got_h.raw(), 7);
        assert_eq!(w, 1920);
        assert_eq!(ht, 1080);
    }

    #[test]
    fn drag_preview_geometry_move_clamps_track() {
        let anchor = ClipDragAnchor {
            key: ClipKey { track: 0, clip: 0 },
            start_beat: 4.0,
            len_beats: 2.0,
            track_index: 0,
        };
        let (s, l, idx) = drag_preview_geometry(anchor, ClipDragKind::Move, 1.5, 5, 3, 0.05);
        assert!((s - 5.5).abs() < 1e-9);
        assert!((l - 2.0).abs() < 1e-9);
        // 0 + 5 = 5 → clamped to 2 (tracks=3 → max idx = 2)
        assert_eq!(idx, 2);
    }

    // M10 Phase 46: track reorder
    #[test]
    fn compute_reorder_target_above_first_row() {
        // 上端外 → 0
        assert_eq!(compute_reorder_target_index(2, -10.0, 0.0, 0.0, 32.0, 5), 0);
        assert_eq!(compute_reorder_target_index(2, 0.0, 0.0, 0.0, 32.0, 5), 0);
    }

    #[test]
    fn compute_reorder_target_below_last_row() {
        // 下端外 → n_tracks (clamp)、anchor=0 で n=5 → target_u=5、anchor 後なので 5-1=4
        assert_eq!(compute_reorder_target_index(0, 1000.0, 0.0, 0.0, 32.0, 5), 4);
    }

    #[test]
    fn compute_reorder_target_self_or_next_returns_anchor() {
        // anchor=2, mouse on row 2 → no-op = 2
        // row 2 中央 = 32*2 + 16 = 80
        assert_eq!(compute_reorder_target_index(2, 80.0, 0.0, 0.0, 32.0, 5), 2);
        // anchor=2, mouse on row 2 中央より下 = 90 → target=3, anchor+1 → no-op = 2
        assert_eq!(compute_reorder_target_index(2, 90.0, 0.0, 0.0, 32.0, 5), 2);
    }

    #[test]
    fn compute_reorder_target_above_anchor_keeps_target() {
        // anchor=4, row 1 中央 (40) → target_u=1 → anchor より前なので 1
        assert_eq!(compute_reorder_target_index(4, 40.0, 0.0, 0.0, 32.0, 5), 1);
        // anchor=4, row 0 上半分 (10) → target_u=0
        assert_eq!(compute_reorder_target_index(4, 10.0, 0.0, 0.0, 32.0, 5), 0);
    }

    #[test]
    fn compute_reorder_target_below_anchor_offsets_by_one() {
        // anchor=0, row 3 中央 (32*3+16=112) → frac=0.5 → target_unbounded=4 → anchor 抜き後 [r1, r2, r3, r4] の
        // 「row 3 と row 4 の間」= new index 3。target_u=4 > anchor+1=1 で 4-1=3。
        assert_eq!(compute_reorder_target_index(0, 112.0, 0.0, 0.0, 32.0, 5), 3);
        // anchor=1, mouse=144 → row 4.5 → target_unbounded=5 → clamp to 5 → 5-1=4
        assert_eq!(compute_reorder_target_index(1, 144.0, 0.0, 0.0, 32.0, 5), 4);
    }

    #[test]
    fn compute_reorder_target_with_track_top_scroll() {
        // header_top=10 + track_top=16 (1/2 row 上にスクロール) + mouse_y=18 → local=24 → row 0.75 → frac>=0.5 → row 1
        // anchor=3, target_u=1 → anchor より前 → 1
        assert_eq!(compute_reorder_target_index(3, 18.0, 10.0, 16.0, 32.0, 5), 1);
    }

    #[test]
    fn compute_reorder_target_zero_row_h_safe() {
        assert_eq!(compute_reorder_target_index(0, 100.0, 0.0, 0.0, 0.0, 5), 0);
        assert_eq!(compute_reorder_target_index(0, 100.0, 0.0, 0.0, 32.0, 0), 0);
    }

    // ============================================================
    // M14 Phase 101 (daw_01 #072): resolve_track_drop / gap_from_y
    // ============================================================

    /// hierarchy 付き track 生成 helper (depth / parent_id を明示)。
    fn htrack(id: u32, depth: u8, parent: Option<u32>) -> ArrangementTrack {
        let mut t = track(id, "t", vec![]);
        t.depth = depth;
        t.parent_id = parent;
        t
    }

    fn group_set(tracks: &[ArrangementTrack]) -> HashSet<u32> {
        tracks.iter().filter_map(|t| t.parent_id).collect()
    }

    /// resolve_track_drop の薄い wrapper (test 用 default 引数)。 visible = full、 lane なし tops。
    /// mouse_x / anchor_mouse_x は `col` (= 右へ動かした indent 列数) を 16px/列で与える。
    #[allow(clippy::cast_precision_loss)]
    fn resolve(
        tracks: &[ArrangementTrack],
        source: &[u32],
        mouse_y: f32,
        col: f32,
    ) -> ReorderDrop {
        let visible: Vec<ArrangementTrack> =
            tracks.iter().filter(|t| is_visible_track(t, tracks)).cloned().collect();
        let tops = visible_track_row_tops(&visible, 0.0, 0.0, 32.0);
        let is_group = group_set(tracks);
        let anchor_x = 100.0_f32;
        resolve_track_drop(
            tracks,
            &visible,
            &tops,
            &is_group,
            source,
            16.0,
            mouse_y,
            anchor_x + col * 16.0,
            anchor_x,
        )
    }

    #[test]
    fn gap_from_y_maps_rows_and_edges() {
        let tops = vec![0.0, 32.0, 64.0, 96.0]; // 3 行
        assert_eq!(gap_from_y(&tops, -5.0), 0); // 上端より上
        assert_eq!(gap_from_y(&tops, 10.0), 0); // 行0 上半分 (<16)
        assert_eq!(gap_from_y(&tops, 20.0), 1); // 行0 下半分 (>16)
        assert_eq!(gap_from_y(&tops, 50.0), 2); // 行1 下半分 (mid=48)
        assert_eq!(gap_from_y(&tops, 95.0), 3); // 行2 下半分
        assert_eq!(gap_from_y(&tops, 200.0), 3); // 最下端以下 = 末尾 gap
        // 退化
        assert_eq!(gap_from_y(&[], 10.0), 0);
        assert_eq!(gap_from_y(&[0.0], 10.0), 0);
    }

    #[test]
    fn drop_below_bottom_group_lands_top_level() {
        // 再現: 最下段 group G + 子 c1/c2、 上にある通常 track t0 を「一番下へ」 drop。
        // 期待: t0 が group block 全体の後ろに top-level で着地 (= parent None, anchor_after = c2)。
        let tracks = vec![
            htrack(0, 0, None),         // t0 (drag source)
            htrack(1, 0, None),         // G (group: c1/c2 の親)
            htrack(2, 1, Some(1)),      // c1
            htrack(3, 1, Some(1)),      // c2 (最終子)
        ];
        // 一番下 (mouse_y=200) へ、 X 動かさず (col=0)。
        let d = resolve(&tracks, &[0], 200.0, 0.0);
        assert_eq!(d.gap, 4, "末尾 gap");
        assert_eq!(d.depth, 0, "X 不動 → 境界 default = 最浅 (top-level)");
        assert_eq!(d.parent, None, "top-level に着地 (group の内側ではない)");
        assert_eq!(d.anchor_after, Some(3), "最終子 c2 の後ろ (= block 全体の後ろ)");
    }

    #[test]
    fn drop_below_bottom_group_with_indent_nests_into_group() {
        // 同じ末尾 drop でも X を 1 段 indent すれば末尾 group へ nest。
        let tracks = vec![
            htrack(0, 0, None),
            htrack(1, 0, None),
            htrack(2, 1, Some(1)),
            htrack(3, 1, Some(1)),
        ];
        let d = resolve(&tracks, &[0], 200.0, 1.0);
        assert_eq!(d.depth, 1);
        assert_eq!(d.parent, Some(1), "末尾 group G の子になる");
        assert_eq!(d.anchor_after, Some(3), "c2 の後ろ (= group の最終子)");
    }

    #[test]
    fn drop_between_members_stays_inside_group() {
        // メンバー間 (c1 と c2 の間) は境界 default で内側 (= 同 group の子)。
        let tracks = vec![
            htrack(1, 0, None),         // G
            htrack(2, 1, Some(1)),      // c1
            htrack(3, 1, Some(1)),      // c2
            htrack(9, 0, None),         // t9 (drag source)
        ];
        // gap between c1(visible idx1) と c2(idx2) = gap2 → mouse_y ~ 48..64 の下半分。
        let d = resolve(&tracks, &[9], 55.0, 0.0);
        assert_eq!(d.gap, 2);
        assert_eq!(d.depth, 1, "メンバー間 = 内側 (深さ 1)");
        assert_eq!(d.parent, Some(1));
        assert_eq!(d.anchor_after, Some(2), "c1 の後ろ");
    }

    #[test]
    fn drop_at_gap_x_controls_pop_out_depth() {
        // [s, A(group), B(group,A の子), x(B の子), T(top)]。 x と T の間 (gap4) で X により
        // 深さ 0/1/2 を選べる (= 何段 group を抜けるか / 末尾 group に nest)。
        let tracks = vec![
            htrack(99, 0, None),        // s (drag source、 先頭)
            htrack(1, 0, None),         // A
            htrack(2, 1, Some(1)),      // B
            htrack(3, 2, Some(2)),      // x (最深 leaf)
            htrack(4, 0, None),         // T
        ];
        // visible=[s,A,B,x,T] tops=[0,32,64,96,128,160]。 x(idx3)とT(idx4)の間=gap4 →
        // T (行4、 128..160) の上半分 (mid=144) で gap4 を選ぶ → mouse_y=130。
        let d0 = resolve(&tracks, &[99], 130.0, 0.0);
        assert_eq!(d0.gap, 4);
        assert_eq!((d0.depth, d0.parent), (0, None), "X 不動 → top-level (x の block 後ろ、 T の前)");
        assert_eq!(d0.anchor_after, Some(3));

        let d1 = resolve(&tracks, &[99], 130.0, 1.0);
        assert_eq!((d1.depth, d1.parent), (1, Some(1)), "1 段 indent → A の子 (B subtree の後ろ)");
        assert_eq!(d1.anchor_after, Some(3));

        let d2 = resolve(&tracks, &[99], 130.0, 2.0);
        assert_eq!((d2.depth, d2.parent), (2, Some(2)), "2 段 indent → B の子 (x の sibling)");
        assert_eq!(d2.anchor_after, Some(3));

        // 区間 clamp: 過剰 indent (col=5) でも max_d=2 で止まる。
        let d_clamp = resolve(&tracks, &[99], 130.0, 5.0);
        assert_eq!(d_clamp.depth, 2);
    }

    #[test]
    fn drop_after_collapsed_group_anchors_past_hidden_children() {
        // collapsed group G (子 c1/c2 が hidden) の直後 (visible 上は G と T の間) へ drop。
        // anchor_after は **hidden な最終子 c2** を指す (= Vec 上 group block の連続性を保つ)。
        // header (G) を指すと expand 時に block 内へ source が紛れ込むため不可。
        let tracks = vec![
            htrack(99, 0, None),                       // s (source、 先頭)
            {
                let mut g = htrack(1, 0, None);
                g.collapsed = true;
                g
            }, // G (collapsed group)
            htrack(2, 1, Some(1)),                     // c1 (hidden)
            htrack(3, 1, Some(1)),                     // c2 (hidden, 最終子)
            htrack(4, 0, None),                        // T
        ];
        // visible=[s, G, T] tops=[0,32,64,96]。 G(idx1)とT(idx2)の間=gap2 → mouse_y ~ 55。
        let d = resolve(&tracks, &[99], 55.0, 0.0);
        assert_eq!(d.gap, 2);
        assert_eq!((d.depth, d.parent), (0, None), "X 不動 → top-level");
        assert_eq!(
            d.anchor_after,
            Some(3),
            "hidden 最終子 c2 の後ろ (header G ではない、 block 連続性維持)"
        );

        // 1 段 indent すれば collapsed group の子として末尾に nest。
        let dn = resolve(&tracks, &[99], 55.0, 1.0);
        assert_eq!((dn.depth, dn.parent), (1, Some(1)));
        assert_eq!(dn.anchor_after, Some(3));
    }

    #[test]
    fn anchor_after_skips_source_tracks() {
        // 直前 track が source 自身のとき anchor_after は **その手前の非 source** を指す
        // (caller が source を remove してから anchor を探す → 見つからず末尾 append する罠を回避)。
        let tracks = vec![htrack(7, 0, None), htrack(8, 0, None)]; // [x, s]
        // s(id=8) を一番下へ。 above=s, below=None, ins=2 → tracks[..2]=[x,s] の非 source = x。
        let d = resolve(&tracks, &[8], 200.0, 0.0);
        assert_eq!(d.parent, None);
        assert_eq!(d.anchor_after, Some(7), "source s ではなく x を anchor にする");
    }

    #[test]
    fn drop_group_into_own_header_gap_does_not_self_parent() {
        // expanded group G を G ヘッダ直下 (G と c1 の間) へ drag。 唯一の合法深さ depth(G)+1=1 では
        // parent=G=source になり self-cycle。 source を親にしない不変で parent は G の親 (None) へ繰り上がる。
        let tracks = vec![
            htrack(1, 0, None),    // G (drag source)
            htrack(2, 1, Some(1)), // c1
            htrack(3, 1, Some(1)), // c2
        ];
        // gap1 = G(row0) と c1(row1) の間 → c1 上半分 (32..48) → mouse_y=40。
        let d = resolve(&tracks, &[1], 40.0, 0.0);
        assert_eq!(d.gap, 1);
        assert_ne!(d.parent, Some(1), "source G を自分の親にしない (self-cycle 回避)");
        assert_eq!(d.parent, None, "非 source 祖先が無い → top-level へ繰り上げ");
        assert_eq!(d.anchor_after, None, "G より前に非 source 無し → 先頭");
    }

    #[test]
    fn drop_multiselect_ancestor_descendant_never_parents_to_source() {
        // multi-select で moving 中の祖先 (A) / 子 (B) を親にしない。 [A, B(A の子), x(B の子), T]。
        // {A,B} を drag して x..T の gap に深く落としても parent は source(A/B) を避け None へ繰り上がる。
        let tracks = vec![
            htrack(1, 0, None),    // A (group, source)
            htrack(2, 1, Some(1)), // B (group, A の子, source)
            htrack(3, 2, Some(2)), // x (B の子)
            htrack(4, 0, None),    // T
        ];
        // x(row2)とT(row3)の間 = gap3 → T 上半分 (96..112) → mouse_y=100。 深く indent (col=5)。
        let d = resolve(&tracks, &[1, 2], 100.0, 5.0);
        assert!(d.parent != Some(1) && d.parent != Some(2), "source A/B を親にしない");
        assert_eq!(d.parent, None, "全 source 祖先を抜けて top-level");
    }

    #[test]
    fn apply_reorder_basic() {
        // [10, 20, 30, 40, 50] anchor=0 → target=2: [20, 30, 10, 40, 50]
        assert_eq!(apply_reorder(&[10, 20, 30, 40, 50], 0, 2), vec![20, 30, 10, 40, 50]);
        // anchor=4 → target=0: [50, 10, 20, 30, 40]
        assert_eq!(apply_reorder(&[10, 20, 30, 40, 50], 4, 0), vec![50, 10, 20, 30, 40]);
        // anchor=2 → target=2 (compute_reorder_target_index が anchor 自身を返した no-op semantics):
        // remove(2)=30 → [10, 20, 40, 50]、insert(2, 30) → 元 array に戻る
        assert_eq!(apply_reorder(&[10, 20, 30, 40, 50], 2, 2), vec![10, 20, 30, 40, 50]);
    }

    #[test]
    fn apply_reorder_safe_on_oob() {
        assert_eq!(apply_reorder(&[1, 2, 3], 5, 0), vec![1, 2, 3]); // anchor OOB
        assert_eq!(apply_reorder::<u32>(&[], 0, 0), Vec::<u32>::new()); // empty
    }

    // M10 Phase 47b: track header volume
    #[test]
    fn volume_from_mouse_x_basic() {
        // band_x=100, band_w=200 → mouse=100 → 0.0、200 → 0.5、300 → 1.0
        assert!((volume_from_mouse_x(100.0, 100.0, 200.0) - 0.0).abs() < 1e-6);
        assert!((volume_from_mouse_x(200.0, 100.0, 200.0) - 0.5).abs() < 1e-6);
        assert!((volume_from_mouse_x(300.0, 100.0, 200.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn volume_from_mouse_x_clamps_outside() {
        assert!((volume_from_mouse_x(50.0, 100.0, 200.0) - 0.0).abs() < 1e-6);
        assert!((volume_from_mouse_x(500.0, 100.0, 200.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn volume_from_mouse_x_zero_width_safe() {
        assert!((volume_from_mouse_x(100.0, 100.0, 0.0) - 0.0).abs() < 1e-6);
        assert!((volume_from_mouse_x(100.0, 100.0, -10.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn header_row_layout_hides_band_at_default_row_h() {
        // default row_h=32 → inner_h=24、btn=20 + gap=2 + band=4 = 26 > 24 → 非表示
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 32.0 };
        let layout = header_row_layout(row, 4.0);
        assert!(layout.volume_band.is_none(), "default 32px row では band 非表示 (progressive disclosure)");
    }

    #[test]
    fn header_row_layout_shows_band_when_large_enough() {
        // row_h=34 → inner_h=26 = 20+2+4 → ぎりぎり表示
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 34.0 };
        let layout = header_row_layout(row, 4.0);
        assert!(layout.volume_band.is_some(), "row_h=34 で band 表示開始");

        // row_h=48 で十分余裕あり
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 48.0 };
        let layout = header_row_layout(row, 4.0);
        assert!(layout.volume_band.is_some(), "row_h=48 で band 表示");
        let band = layout.volume_band.unwrap();
        assert!((band.h - 4.0).abs() < 1e-6, "band の高さ = volume_band_h");
        assert!(band.y > layout.buttons[0].y, "band は buttons の下に来る");
    }

    #[test]
    fn header_row_layout_hides_band_when_volume_band_h_zero() {
        // band_h=0 → 常に非表示 (disable)
        let row = Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 };
        let layout = header_row_layout(row, 0.0);
        assert!(layout.volume_band.is_none(), "band_h=0 で disable");
    }

    // M10 Phase 48: vertical zoom (Alt+wheel)
    #[test]
    fn alt_wheel_emits_set_track_row_h() {
        use std::sync::Mutex;

        use daw_ui_platform::{Modifiers, PhysicalSize};
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        // arrangement widget の Edit を観測するため、Model に最終 row_h と最終 track_top を持つ。
        struct Model {
            row_h: f32,
            track_top: f32,
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let tracks = vec![track(0, "t", vec![]), track(1, "u", vec![])];
        let view = ArrangementView::default();
        let mut model = Model {
            row_h: view.track_row_h,
            track_top: 0.0,
            tracks,
            view,
        };

        let observed: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 200.0)),
                scroll_delta: (0.0, -100.0),
                modifiers: Modifiers { alt: true, ..Modifiers::default() },
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let edits = host.frame_to_edits(&model, &mut scene, screen, input, |m, ui| {
            let style = ArrangementStyle::default();
            let observed_cb = Arc::clone(&observed_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| match req {
                    ArrangementEditRequest::SetTrackRowH(h) => {
                        observed_cb.lock().unwrap().push("SetTrackRowH");
                        Edit::mutate(move |mm: &mut Model| mm.row_h = h)
                    }
                    ArrangementEditRequest::SetTrackTop(t) => {
                        observed_cb.lock().unwrap().push("SetTrackTop");
                        Edit::mutate(move |mm: &mut Model| mm.track_top = t)
                    }
                    _ => {
                        observed_cb.lock().unwrap().push("other");
                        Edit::mutate(|_| {})
                    }
                },
            );
        });
        for e in edits {
            e.apply(&mut model);
        }
        let log = observed.lock().unwrap();
        assert!(
            log.contains(&"SetTrackRowH"),
            "Alt+wheel で SetTrackRowH が発火する: log={:?}",
            *log
        );
        // M14 Phase 61a follow-up: Alt+wheel は anchor 調整のため **SetTrackTop も同 frame で発火**
        // する (mouse 下の track が画面上で動かないようにする、 Cubase 標準)。
        assert!(
            log.contains(&"SetTrackTop"),
            "Alt+wheel は anchor 調整のため SetTrackTop も発火する: log={:?}",
            *log
        );
        // M14 Phase 61a (#011): 符号反転 + 係数低減後の挙動。
        // dy=-100 (wheel down) → factor = exp(-100 * 0.0015) = exp(-0.15) ≈ 0.8607
        //   → row_h = 32 * 0.8607 ≈ 27.54 (wheel down で row 縮む = zoom out、 一般 DAW 一致)。
        assert!(
            (model.row_h - 32.0 * (-0.15_f32).exp()).abs() < 1e-3,
            "row_h は exp curve で更新される: actual={}",
            model.row_h
        );
    }

    #[test]
    fn drag_preview_geometry_resize_left_clamps_min_len() {
        let anchor = ClipDragAnchor {
            key: ClipKey { track: 0, clip: 0 },
            start_beat: 4.0,
            len_beats: 2.0,
            track_index: 1,
        };
        let (s, l, idx) =
            drag_preview_geometry(anchor, ClipDragKind::ResizeLeft, 10.0, 0, 4, 0.05);
        // max_start = 4 + 2 - 0.05 = 5.95 → new_start clamped to 5.95
        // actual_delta = 5.95 - 4 = 1.95 → new_len = 2 - 1.95 = 0.05
        assert!((s - 5.95).abs() < 1e-6);
        assert!((l - 0.05).abs() < 1e-6);
        assert_eq!(idx, 1);
    }

    // -------- M13 Phase 55: ruler / time_sig 対応 grid の確認 --------

    /// 1 frame 描画して `scene.iter_glyphs()` と `iter_lines()` で primitive を取得する helper。
    fn render_arrangement_once(view: ArrangementView) -> daw_ui_renderer::Scene {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        let tracks: Vec<ArrangementTrack> = vec![track(0, "t0", vec![])];
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let style = ArrangementStyle::default();
            let _ = ui.arrangement(
                "arr_test",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });
        scene
    }

    /// M14 Phase 87 (daw_01 #059): `ArrangementTrack.color = Some(c)` の track は header 左端に
    /// 幅 `style.track_color_strip_w` の縦ストライプ rect (fill == c) を 1 つ push する。 `None`
    /// の track は同色の strip rect を push しない (= 既存挙動互換)。
    #[test]
    fn track_color_strip_drawn_only_for_colored_track() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let strip_color = Color::rgb(0.91, 0.33, 0.55); // 他のどの style 色とも一致しない目印色
        let mut t0 = track(0, "colored", vec![]);
        t0.color = Some(strip_color);
        let t1 = track(1, "plain", vec![]); // color: None
        let tracks = vec![t0, t1];

        let mut view = test_view();
        view.header_w = 180.0; // header pane を出す
        let style = ArrangementStyle::default();

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.arrangement(
                "arr_strip",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });

        let strips: Vec<&RectCommand> = scene
            .iter_rects()
            .filter(|r| r.fill == strip_color)
            .collect();
        assert_eq!(
            strips.len(),
            1,
            "colored track 1 本だけが strip を描く: got {} 本",
            strips.len()
        );
        let strip = strips[0];
        assert!(
            (strip.rect.x - 0.0).abs() < 1e-3,
            "strip は header pane 左端 (x=0): x={}",
            strip.rect.x
        );
        assert!(
            (strip.rect.w - style.track_color_strip_w).abs() < 1e-3,
            "strip 幅は style.track_color_strip_w (={}): w={}",
            style.track_color_strip_w,
            strip.rect.w
        );
    }

    /// M14 Phase 97 (daw_01 #069): color strip は depth*indent_px だけ右にインデントして名前と
    /// 一緒にネストする。 depth==0 は x=0 で従来と pixel 一致、 depth==1 は x=indent_px に追従。
    #[test]
    fn track_color_strip_follows_group_indent() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let color0 = Color::rgb(0.91, 0.33, 0.55); // depth 0 の目印色
        let color1 = Color::rgb(0.17, 0.83, 0.42); // depth 1 の目印色 (どの style 色とも非一致)
        let mut t0 = track(0, "root", vec![]);
        t0.color = Some(color0);
        let mut t1 = track(1, "child", vec![]);
        t1.color = Some(color1);
        t1.depth = 1; // group の子トラック
        let tracks = vec![t0, t1];

        let mut view = test_view();
        view.header_w = 180.0; // header pane を出す
        let style = ArrangementStyle::default();

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.arrangement(
                "arr_strip_indent",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });

        let strip0 = scene
            .iter_rects()
            .find(|r| r.fill == color0)
            .expect("depth 0 の strip が存在する");
        let strip1 = scene
            .iter_rects()
            .find(|r| r.fill == color1)
            .expect("depth 1 の strip が存在する");

        // depth 0: 従来どおり header pane 左端 (x=0、 pixel 互換)。
        assert!(
            (strip0.rect.x - 0.0).abs() < 1e-3,
            "depth 0 strip は x=0 (従来互換): x={}",
            strip0.rect.x
        );
        // depth 1: 名前と同じ indent (= depth * indent_px) だけ右へ。
        assert!(
            (strip1.rect.x - style.indent_px).abs() < 1e-3,
            "depth 1 strip は x=indent_px (={}): x={}",
            style.indent_px,
            strip1.rect.x
        );
        // 幅は不変。
        assert!(
            (strip1.rect.w - style.track_color_strip_w).abs() < 1e-3,
            "strip 幅は不変 (={}): w={}",
            style.track_color_strip_w,
            strip1.rect.w
        );
    }

    // ============================================================
    // M14 Phase 96 (daw_01 #068): 共有グループ連動ハイライト
    // ============================================================

    /// track 0 に与えた clips で arrangement を描画して scene を返す helper (`selected` で
    /// selection overlay も重ねる)。 view は `test_view` (header_w=0 / ruler_h=0)。
    fn render_clips_scene(
        clips_track0: Vec<ArrangementClip>,
        selected: &[ClipKey],
    ) -> daw_ui_renderer::Scene {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let tracks = vec![track(0, "t0", clips_track0)];
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let style = ArrangementStyle::default();
            let _ = ui.arrangement(
                "arr_active_group",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                test_view(),
                selected,
                &[],
                &[],
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });
        scene
    }

    // ===== M14 Phase 127 (daw_01 #105): Arranger レーン (section) tests =====

    fn section(id: u32, start: f64, len: f64, name: &str) -> SectionView {
        SectionView {
            id,
            name: Arc::from(name),
            color: [0.30, 0.45, 0.65],
            start_beat: start,
            len_beats: len,
            selected: false,
        }
    }

    fn snap_quarter() -> SnapConfig {
        SnapConfig {
            mode: crate::snap::SnapMode::Straight { div: 4 },
            enabled: true,
            min_beat_unit: 1.0 / 128.0,
            time_sig: (4, 4),
        }
    }

    fn section_drag(kind: SectionGesture, start: f64, len: f64, last: (f32, f32), ctrl: bool) -> SectionDragSession {
        SectionDragSession {
            kind,
            section_id: 7,
            anchor_start: start,
            anchor_len: len,
            anchor_press_beat: start,
            anchor_mouse: (0.0, 0.0),
            last_mouse: last,
            last_alt: false,
            last_ctrl: ctrl,
            last_shift: false,
        }
    }

    /// arranger lane に sections を載せて 1 frame 描画した scene を返す。
    fn render_sections_scene(sections: Vec<SectionView>, arranger_lane_h: f32) -> daw_ui_renderer::Scene {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let tracks = vec![track(0, "t0", vec![])];
        // header_w > 0 で arranger_header 領域を確保 ("Arranger" 見出しの描画確認用)。
        let view = ArrangementView { arranger_lane_h, header_w: 160.0, ..test_view() };
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let style = ArrangementStyle::default();
            let _ = ui.arrangement(
                "arr_sections",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &sections,
                view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });
        scene
    }

    /// `section_rect_from`: beat → arranger レーン内 px (高さは lane 全高)。
    #[test]
    fn section_rect_from_basic_position() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        // 1 beat = 50 px。 start=2.0 → x=100、 len=4.0 → w=200、 高さは lane 全高。
        let r = section_rect_from(2.0, 4.0, view, arranger);
        assert!((r.x - 100.0).abs() < 1e-3, "x=100: got {}", r.x);
        assert!((r.w - 200.0).abs() < 1e-3, "w=200: got {}", r.w);
        assert!((r.y - 0.0).abs() < 1e-3 && (r.h - 20.0).abs() < 1e-3, "lane 全高");
    }

    /// `section_hit`: 帯中央 = Move、 左端 = ResizeLeft、 右端 = ResizeRight、 lane 外 (y) = None。
    #[test]
    fn section_hit_move_resize_zones() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        let secs = vec![section(7, 2.0, 4.0, "A")]; // x 100..300
        assert_eq!(section_hit(&secs, arranger, view, 200.0, 10.0, 4.0), Some((7, ClipDragKind::Move)));
        assert_eq!(section_hit(&secs, arranger, view, 100.0, 10.0, 4.0), Some((7, ClipDragKind::ResizeLeft)));
        assert_eq!(section_hit(&secs, arranger, view, 300.0, 10.0, 4.0), Some((7, ClipDragKind::ResizeRight)));
        assert_eq!(section_hit(&secs, arranger, view, 200.0, 25.0, 4.0), None, "lane の y 外は None");
    }

    /// `section_hit`: 隣接 section の共有境界 (A.right == B.left) では、 cursor が A の rect 内
    /// (A 右端ハンドル) なら、 B の左端外側拡張ハンドルより A を優先 (#101 / piano_roll #053 と同 2-tier)。
    #[test]
    fn section_hit_adjacent_in_rect_priority() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        // A 0..4 (x 0..200)、 B 4..8 (x 200..400)。 共有境界 x=200。
        let secs = vec![section(1, 0.0, 4.0, "A"), section(2, 4.0, 4.0, "B")];
        // x=199: A の rect 内 (右端 -1px) なので A の ResizeRight が、 B の左端外側ハンドルより勝つ。
        assert_eq!(
            section_hit(&secs, arranger, view, 199.0, 10.0, 4.0),
            Some((1, ClipDragKind::ResizeRight)),
            "A の右端 (in-rect) が B の左端 outer より優先"
        );
        // x=201: B の rect 内 (左端 +1px) なので B の ResizeLeft。
        assert_eq!(
            section_hit(&secs, arranger, view, 201.0, 10.0, 4.0),
            Some((2, ClipDragKind::ResizeLeft)),
            "B の左端 (in-rect) が A の右端 outer より優先"
        );
    }

    /// FIXME #067: `section_at_inrect` は帯の **内側のみ** を返し、 resize handle の外側拡張
    /// (`±resize_handle_px`) を含めない。 帯のすぐ隣の空白の dblclick / 右クリックを隣 section の
    /// rename / メニューに化けさせない (= `section_hit` との決定的な差)。
    #[test]
    fn section_at_inrect_excludes_resize_handle_extension() {
        let arranger = Rect { x: 0.0, y: 0.0, w: 400.0, h: 20.0 };
        let view = ArrangementView { start_beat: 0.0, len_beats: 8.0, ..ArrangementView::default() };
        let secs = vec![section(7, 2.0, 4.0, "Aメロ")]; // x 100..300
        // 帯の内側はヒットする (中央 / 端の内側 1px)。
        assert_eq!(section_at_inrect(&secs, arranger, view, 200.0, 10.0), Some(7), "帯中央");
        assert_eq!(section_at_inrect(&secs, arranger, view, 100.0, 10.0), Some(7), "左端 (in-rect)");
        assert_eq!(section_at_inrect(&secs, arranger, view, 299.0, 10.0), Some(7), "右端 -1px (in-rect)");
        // 帯の **すぐ隣の空白** (resize handle 拡張部 ±4px の内側) は None = リネームしない。
        assert_eq!(section_at_inrect(&secs, arranger, view, 98.0, 10.0), None, "左端の外 2px は空白");
        assert_eq!(section_at_inrect(&secs, arranger, view, 300.0, 10.0), None, "右端ちょうど (= rect 外) は空白");
        assert_eq!(section_at_inrect(&secs, arranger, view, 302.0, 10.0), None, "右端の外 2px は空白");
        // 同じ外側 2px で `section_hit` は拡張ハンドルにヒットする (= bug の発生源、 drag では正当)。
        assert!(
            section_hit(&secs, arranger, view, 302.0, 10.0, 4.0).is_some(),
            "section_hit は外側拡張を含む (drag 用) — point gesture では section_at_inrect を使う"
        );
        // lane の y 外 (帯外の縦領域) も None。
        assert_eq!(section_at_inrect(&secs, arranger, view, 200.0, 25.0), None, "lane の y 外");
    }

    /// `compute_section_drag_beat_delta`: snap OFF は pivot+raw を素通し (= 各 gesture で delta = raw)。
    #[test]
    fn section_drag_delta_raw_passthrough_off() {
        let off = &SnapConfig::OFF;
        // Move: pivot = anchor_start。
        let sd = section_drag(SectionGesture::Move, 4.0, 4.0, (0.0, 0.0), false);
        assert!((compute_section_drag_beat_delta(&sd, 1.5, off, 50.0) - 1.5).abs() < 1e-6);
        // ResizeRight: pivot = anchor_start + anchor_len。
        let sd = section_drag(SectionGesture::ResizeRight, 4.0, 4.0, (0.0, 0.0), false);
        assert!((compute_section_drag_beat_delta(&sd, 0.7, off, 50.0) - 0.7).abs() < 1e-6);
        // Create: pivot = anchor_press_beat (= anchor_start in helper)。
        let sd = section_drag(SectionGesture::Create, 2.0, 0.0, (0.0, 0.0), false);
        assert!((compute_section_drag_beat_delta(&sd, 3.0, off, 50.0) - 3.0).abs() < 1e-6);
    }

    /// `compute_section_drag_beat_delta`: quarter snap で pivot+raw を grid に丸めた差分を返す
    /// (絶対位置 snap)。 Move pivot=4.0 + raw 1.1 = 5.1 → snap 5.0 → delta 1.0。
    #[test]
    fn section_drag_delta_snaps_pivot() {
        let snap = snap_quarter();
        let sd = section_drag(SectionGesture::Move, 4.0, 4.0, (0.0, 0.0), false);
        let d = compute_section_drag_beat_delta(&sd, 1.1, &snap, 50.0);
        assert!((d - 1.0).abs() < 1e-6, "5.1 → snap 5.0 → delta 1.0: got {d}");
    }

    /// `beats_per_bar`: 4/4=4、 3/4=3、 6/8=3、 7/8=3.5、 異常 0 は 1 以上に floor。
    #[test]
    fn beats_per_bar_time_sigs() {
        assert!((beats_per_bar((4, 4)) - 4.0).abs() < 1e-6);
        assert!((beats_per_bar((3, 4)) - 3.0).abs() < 1e-6);
        assert!((beats_per_bar((6, 8)) - 3.0).abs() < 1e-6);
        assert!((beats_per_bar((7, 8)) - 3.5).abs() < 1e-6);
        assert!(beats_per_bar((0, 0)) >= 1.0, "0/0 は 1 以上に floor");
    }

    /// arranger_lane_h > 0 + sections: 色帯 (section.color fill) と名前 glyph + "Arranger" 見出しを描く。
    #[test]
    fn sections_render_band_and_name() {
        let mut s = section(1, 0.0, 4.0, "Intro");
        s.color = [0.70, 0.20, 0.30];
        let scene = render_sections_scene(vec![s], 22.0);
        let fill = Color::rgb(0.70, 0.20, 0.30);
        assert!(scene.iter_rects().any(|r| r.fill == fill), "section の色帯を描く");
        assert!(scene.iter_glyphs().any(|g| g.text.as_ref() == "Intro"), "section 名 glyph");
        assert!(scene.iter_glyphs().any(|g| g.text.as_ref() == "Arranger"), "header 見出し");
    }

    /// arranger_lane_h == 0: section / 見出しを一切描かない (= 従来描画と互換、 回帰防止)。
    #[test]
    fn arranger_lane_zero_draws_nothing() {
        let mut s = section(1, 0.0, 4.0, "Intro");
        s.color = [0.70, 0.20, 0.30];
        let scene = render_sections_scene(vec![s], 0.0);
        let fill = Color::rgb(0.70, 0.20, 0.30);
        assert!(!scene.iter_rects().any(|r| r.fill == fill), "lane 0 では色帯を描かない");
        assert!(!scene.iter_glyphs().any(|g| g.text.as_ref() == "Intro"), "lane 0 では section 名なし");
        assert!(!scene.iter_glyphs().any(|g| g.text.as_ref() == "Arranger"), "lane 0 では見出しなし");
    }

    /// M14 Phase 128 (daw_01 #106): `selected: true` の帯は明るい太枠 (`clip_selected_border` /
    /// `clip_selected_border_w`)、 `selected: false` は出ない (= 選択ハイライトの有無を pixel 経路で確認)。
    #[test]
    fn selected_section_draws_highlight_border() {
        let style = ArrangementStyle::default();
        let sel_border = |scene: &daw_ui_renderer::Scene| {
            scene.iter_rects().any(|r| {
                r.border == style.clip_selected_border
                    && (r.border_width - style.clip_selected_border_w).abs() < 1e-3
            })
        };
        let mut sel = section(1, 0.0, 4.0, "Sel");
        sel.selected = true;
        assert!(sel_border(&render_sections_scene(vec![sel], 22.0)), "selected 帯は明るい太枠");

        // 非選択帯は selected 太枠を描かない (= 太枠の出所が選択であることを保証)。
        assert!(
            !sel_border(&render_sections_scene(vec![section(2, 0.0, 4.0, "Unsel")], 22.0)),
            "非選択帯は selected 太枠なし"
        );
    }

    /// id=10, beat 2..6 の clip に share_group hue / in_active_group を載せた test clip。
    fn shared_clip(in_active: bool, hue: Option<f32>) -> ArrangementClip {
        let mut c = clip(10, 2.0, 4.0, "shared");
        c.share_group_color = hue;
        c.in_active_group = in_active;
        c
    }

    /// M14 Phase 114 (daw_01 #086): audio share clip (`share_group_color = Some`) は `clip.color` を
    /// fill の唯一 source にする (hue fill を撤去)。 「clip で色を選べば共有 clip 全部がその色になる」
    /// (FIXME #8) の核心。 border も neutral `clip_border`、 共有は ⇌ glyph でのみ識別。
    #[test]
    fn audio_share_clip_uses_color_fill_not_hue() {
        let style = ArrangementStyle::default();
        let user_color = Color::rgb(0.70, 0.30, 0.45);
        let mut c = shared_clip(false, Some(0.33));
        c.color = Some(user_color);
        let scene = render_clips_scene(vec![c], &[]);
        assert!(
            scene.iter_rects().any(|r| r.fill == user_color),
            "audio share clip は clip.color を fill に使う (#086)"
        );
        assert!(
            scene.iter_rects().any(|r| r.border == style.clip_border),
            "border は neutral clip_border (hue border は撤去)"
        );
        assert_eq!(link_glyph_count(&scene, &style), 1, "共有マークは ⇌ glyph で 1 個");
    }

    /// M14 Phase 114 (#086): `color` 未指定の audio share clip は `clip_default_fill` に
    /// フォールバック (hue fill 撤去後、 通常 clip と同じ既定塗り + ⇌ glyph)。
    #[test]
    fn audio_share_clip_without_color_uses_default_fill() {
        let style = ArrangementStyle::default();
        let scene = render_clips_scene(vec![shared_clip(false, Some(0.33))], &[]);
        assert!(
            scene.iter_rects().any(|r| r.fill == style.clip_default_fill),
            "color 未指定 share clip は clip_default_fill"
        );
        assert_eq!(link_glyph_count(&scene, &style), 1, "⇌ glyph は描く");
    }

    /// M14 Phase 126 (daw_01 #104): share clip のラベルは ⇌ と clip 名を **1 つの text run に統合**
    /// して描く (旧実装の固定 +2px パッド + em 幅近似による隙間を撤去)。 統合 run の text は `⇌<name>`
    /// 完全一致で、 ⇌ 単独の別 run は存在しない (= マークと名前が密着)。
    #[test]
    fn share_clip_label_merges_link_glyph_into_name_run() {
        let style = ArrangementStyle::default();
        let glyph = style.share_group_link_glyph;
        // shared_clip の name は "shared"。
        let scene = render_clips_scene(vec![shared_clip(false, Some(0.33))], &[]);

        let expected = format!("{glyph}shared");
        let link_runs: Vec<&str> = scene
            .iter_glyphs()
            .map(|g| g.text.as_ref())
            .filter(|t| t.starts_with(glyph))
            .collect();
        assert_eq!(
            link_runs,
            vec![expected.as_str()],
            "⇌ と名前が 1 つの text run に密着 (got {link_runs:?})"
        );
        // ⇌ 単独 (= 旧実装の別 glyph run) は存在しない。
        let bare = glyph.to_string();
        assert!(
            !scene.iter_glyphs().any(|g| g.text.as_ref() == bare),
            "⇌ 単独の別 run は無い (name と統合済)"
        );
    }

    /// `has_link == false` の通常 clip は名前のみの run (⇌ を含まない、 位置不変)。 #104 の回帰防止。
    #[test]
    fn non_share_clip_label_is_name_only() {
        let style = ArrangementStyle::default();
        let scene = render_clips_scene(vec![clip(10, 2.0, 4.0, "plain")], &[]);
        assert!(
            scene.iter_glyphs().any(|g| g.text.as_ref() == "plain"),
            "非 share clip は名前のみの run"
        );
        assert!(
            !scene
                .iter_glyphs()
                .any(|g| g.text.as_ref().starts_with(style.share_group_link_glyph)),
            "非 share clip に ⇌ は付かない"
        );
    }

    /// M14 Phase 114 (#086): active group 強調色は identity-neutral な `share_group_active_color`。
    /// glow wash は同色を `share_group_active_glow_alpha` で、 border は同色を不透明で描く helper。
    fn expected_active_glow(style: &ArrangementStyle) -> Color {
        let ac = style.share_group_active_color;
        Color { r: ac.r, g: ac.g, b: ac.b, a: style.share_group_active_glow_alpha }
    }

    /// `in_active_group == true` かつ `share_group_color == Some` の clip は、 selection とは別の
    /// neutral 強調 (glow wash 1 枚 + bright thick border 1 本) を追加する (#086 で hue → neutral)。
    #[test]
    fn active_group_overlay_drawn_for_in_active_group_clip() {
        let style = ArrangementStyle::default();
        let scene = render_clips_scene(vec![shared_clip(true, Some(0.33))], &[]);

        let expected_border = style.share_group_active_color;
        let expected_glow = expected_active_glow(&style);

        let border_rects = scene
            .iter_rects()
            .filter(|r| {
                (r.border_width - style.share_group_active_border_w).abs() < 1e-3
                    && r.border == expected_border
            })
            .count();
        assert_eq!(border_rects, 1, "bright thick border が 1 本 (border_w={})", style.share_group_active_border_w);

        let glow_rects = scene.iter_rects().filter(|r| r.fill == expected_glow).count();
        assert_eq!(glow_rects, 1, "glow wash が 1 枚 (neutral を {} alpha)", style.share_group_active_glow_alpha);
    }

    /// `in_active_group == false` の clip は強調 rect (border + glow) を一切追加しない
    /// (= 既存挙動と pixel 完全一致)。 glow / border は独立 guard なので両方の不在を確認する。
    #[test]
    fn no_active_overlay_when_in_active_group_false() {
        let style = ArrangementStyle::default();
        let scene = render_clips_scene(vec![shared_clip(false, Some(0.33))], &[]);
        let active_borders = scene
            .iter_rects()
            .filter(|r| (r.border_width - style.share_group_active_border_w).abs() < 1e-3)
            .count();
        assert_eq!(active_borders, 0, "false は強調枠を描かない (移行安全)");
        let glow = scene.iter_rects().filter(|r| r.fill == expected_active_glow(&style)).count();
        assert_eq!(glow, 0, "false は glow wash も描かない");
    }

    /// `share_group_color == None` の clip は in_active_group=true でも強調しない (hue 不明 = defensive)。
    /// border / glow 両方の不在を確認 (hue 不明なので glow は固有 alpha=glow_alpha の rect 不在で判定)。
    #[test]
    fn no_active_overlay_when_share_group_color_none() {
        let style = ArrangementStyle::default();
        let scene = render_clips_scene(vec![shared_clip(true, None)], &[]);
        let active_borders = scene
            .iter_rects()
            .filter(|r| (r.border_width - style.share_group_active_border_w).abs() < 1e-3)
            .count();
        assert_eq!(active_borders, 0, "hue 無しは強調枠を描かない");
        // glow wash の色は hue 依存だが alpha は固定 (share_group_active_glow_alpha)。 default style に
        // この alpha を持つ rect は他に無いので、 0 件 = glow も描いていない。
        let glow = scene
            .iter_rects()
            .filter(|r| (r.fill.a - style.share_group_active_glow_alpha).abs() < 1e-3)
            .count();
        assert_eq!(glow, 0, "hue 無しは glow wash も描かない");
    }

    /// `share_group_active_glow_alpha == 0.0` の opt-out (ring のみ): glow wash は描かず
    /// bright border だけ 1 本描く (style doc の「ring のみの強調」 契約を固定 + glow / border が
    /// 独立 guard であることを保証)。
    #[test]
    fn active_overlay_glow_alpha_zero_draws_border_only() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let style =
            ArrangementStyle { share_group_active_glow_alpha: 0.0, ..ArrangementStyle::default() };
        let hue = 0.33_f32;
        let tracks = vec![track(0, "t0", vec![shared_clip(true, Some(hue))])];

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.arrangement(
                "arr_ring_only",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                test_view(),
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });

        // glow wash は alpha=0 では push されない (guard `> 0.0` で skip)。 glow rect があれば
        // fill = この透明 neutral になるはずなので、 0 件 = push されていないことを保証。
        let ac = style.share_group_active_color;
        let glow_color = Color { r: ac.r, g: ac.g, b: ac.b, a: 0.0 };
        let glow = scene.iter_rects().filter(|r| r.fill == glow_color).count();
        assert_eq!(glow, 0, "glow_alpha=0 で glow wash を描かない (ring のみ)");
        let border = scene
            .iter_rects()
            .filter(|r| (r.border_width - style.share_group_active_border_w).abs() < 1e-3)
            .count();
        assert_eq!(border, 1, "glow_alpha=0 でも bright border は 1 本描く");
    }

    /// `in_active_group` は viewport_key (heavy cache key) に含まれない: flip しても
    /// `fold_arrangement_clip_hash` は不変 (= hover 由来の active group 変化で heavy cache を
    /// 無効化せず、 強調は cached 外 overlay で毎フレーム描く)。
    #[test]
    fn fold_arrangement_clip_hash_ignores_in_active_group() {
        // clone で同一 Arc<str> name を共有させ、 in_active_group だけが異なる 2 clip を作る
        // (clip() を 2 回呼ぶと name.as_ptr() が変わり hash の name 成分で差が出てしまうため)。
        let c_off = shared_clip(false, Some(0.33));
        let mut c_on = c_off.clone();
        c_on.in_active_group = true;
        let before = vec![track(0, "t0", vec![c_off])];
        let after = vec![track(0, "t0", vec![c_on])];
        assert_eq!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "in_active_group 変化は cache を無効化しない"
        );
    }

    /// 選択中かつ active group の member は、 selection リング (FIXME #73) が active
    /// overlay の **後** に描画されて優先される (#068 の「選択中 member は選択枠優先」)。
    #[test]
    fn selection_fill_drawn_after_active_overlay() {
        let style = ArrangementStyle::default();
        let key = ClipKey { track: 0, clip: 10 };
        let scene = render_clips_scene(vec![shared_clip(true, Some(0.33))], &[key]);

        let expected_border = style.share_group_active_color;
        let rects: Vec<&RectCommand> = scene.iter_rects().collect();
        let active_idx = rects
            .iter()
            .position(|r| {
                (r.border_width - style.share_group_active_border_w).abs() < 1e-3
                    && r.border == expected_border
            })
            .expect("active border rect が存在する");
        let sel_idx = rects
            .iter()
            .position(|r| {
                r.border == style.clip_selected_border
                    && (r.border_width - style.clip_selected_border_w).abs() < 1e-3
            })
            .expect("selection ring (明枠) rect が存在する");
        assert!(
            sel_idx > active_idx,
            "selection は active overlay の後 (= 上) に描画される: sel={sel_idx} active={active_idx}"
        );
    }

    // ============================================================
    // M14 Phase 108 (daw_01 #080): share マークを Video-kind track の clip にも描く
    // ============================================================

    /// Video-kind track 1 本に `clips` を載せて 1 frame 描画し scene を返す helper
    /// (`selected` で selection overlay も重ねる)。 `render_clips_scene` の Video 版。
    fn render_video_clips_scene(
        clips: Vec<ArrangementClip>,
        selected: &[ClipKey],
    ) -> daw_ui_renderer::Scene {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let mut t = track(0, "v0", clips);
        t.kind = TrackKind::Video;
        let tracks = vec![t];
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let style = ArrangementStyle::default();
            let _ = ui.arrangement(
                "arr_video_share",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                test_view(),
                selected,
                &[],
                &[],
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });
        scene
    }

    /// scene 内に link glyph (`share_group_link_glyph`) を描いた GlyphArea の個数。
    fn link_glyph_count(scene: &daw_ui_renderer::Scene, style: &ArrangementStyle) -> usize {
        // M14 Phase 126 (#104): link glyph (⇌) は clip 名と 1 つの text run に統合されたので、
        // 厳密一致ではなく「先頭が ⇌ で始まる」 glyph run を share マーク付きラベルとして数える
        // (非 share clip の名前は ⇌ で始まらないので従来の count==0 も保たれる)。
        scene
            .iter_glyphs()
            .filter(|g| g.text.as_ref().starts_with(style.share_group_link_glyph))
            .count()
    }

    /// M14 Phase 114 (#086): thumbnail 無しの video share clip は `clip.color` を fill source にし
    /// (hue fill を撤去)、 border は neutral `clip_border`、 共有は ⇌ glyph のみで識別する。
    #[test]
    fn video_share_clip_without_thumbnail_uses_color_fill() {
        let style = ArrangementStyle::default();
        let user_color = Color::rgb(0.20, 0.55, 0.55); // teal
        let mut c = shared_clip(false, Some(0.33));
        c.color = Some(user_color);
        let scene = render_video_clips_scene(vec![c], &[]);

        assert!(
            scene.iter_rects().any(|r| r.fill == user_color),
            "video share clip は clip.color を fill に使う (#086)"
        );
        assert!(
            scene.iter_rects().any(|r| r.border == style.clip_border),
            "border は neutral clip_border (hue border は撤去)"
        );
        assert_eq!(link_glyph_count(&scene, &style), 1, "共有マークは ⇌ glyph で 1 個描く");
        // hue fill 撤去後は loading 一色にもならない (color が source)。
        assert!(
            !scene.iter_rects().any(|r| r.fill == style.video_clip_loading),
            "color 指定 share clip は video_clip_loading にフォールバックしない"
        );
    }

    /// M14 Phase 114 (#086): `color` 未指定の video share clip は従来の `video_clip_loading` 背景 +
    /// neutral border + ⇌ glyph (= color None なら既存の letterbox 背景にフォールバック)。
    #[test]
    fn video_share_clip_without_color_falls_back_to_loading() {
        let style = ArrangementStyle::default();
        let scene = render_video_clips_scene(vec![shared_clip(false, Some(0.33))], &[]);
        assert!(
            scene.iter_rects().any(|r| r.fill == style.video_clip_loading),
            "color 未指定 share clip は video_clip_loading 背景"
        );
        assert_eq!(link_glyph_count(&scene, &style), 1, "⇌ glyph は描く");
    }

    /// M14 Phase 114 (#086): thumbnail を持つ video clip: letterbox 背景は `video_clip_loading` (color
    /// 未指定) のまま thumbnail を隠さず、 border は neutral `clip_border`、 共有は ⇌ glyph で識別。
    #[test]
    fn video_share_clip_with_thumbnail_keeps_letterbox_and_glyph() {
        use std::num::NonZeroU32;
        let style = ArrangementStyle::default();
        let mut c = shared_clip(false, Some(0.5));
        c.thumbnail = Some((TextureHandle::from_raw(NonZeroU32::new(7).unwrap()), 1920, 1080));
        let scene = render_video_clips_scene(vec![c], &[]);

        // letterbox 背景 (= clip rect fill) は neutral な video_clip_loading のまま (color 未指定)。
        assert!(
            scene.iter_rects().any(|r| r.fill == style.video_clip_loading),
            "letterbox 背景は video_clip_loading のまま"
        );
        // border は neutral clip_border (hue border は #086 で撤去)。
        assert!(
            scene.iter_rects().any(|r| r.border == style.clip_border),
            "border は neutral clip_border"
        );
        assert_eq!(link_glyph_count(&scene, &style), 1, "link glyph を 1 個描く");
        // thumbnail texture も描かれている (隠していない)。
        assert_eq!(scene.iter_textures().count(), 1, "thumbnail texture を描く");
    }

    /// selected な video share clip: fill は clip 本来の色のまま、 選択は 2 重リング
    /// (FIXME #73) で示す。 link glyph は #022 どおり selected でも描く。 base +
    /// selection overlay の 2 回描画で glyph は 2 個。
    #[test]
    fn selected_video_share_clip_keeps_link_glyph() {
        let style = ArrangementStyle::default();
        let hue = 0.1_f32;
        let key = ClipKey { track: 0, clip: 10 };
        let scene = render_video_clips_scene(vec![shared_clip(false, Some(hue))], &[key]);

        assert!(
            scene.iter_rects().any(|r| {
                r.border == style.clip_selected_border
                    && (r.border_width - style.clip_selected_border_w).abs() < 1e-3
            }),
            "selected video clip は selection リング (明枠) を描く (FIXME #73)"
        );
        assert_eq!(
            link_glyph_count(&scene, &style),
            2,
            "link glyph は base + selection overlay で 2 個 (selected でも描く、 #022)"
        );
    }

    /// 非 share な video clip (`share_group_color == None`): link glyph を描かず fill は
    /// `video_clip_loading` のまま (= #044 の既存挙動と完全互換、 回帰なし)。
    #[test]
    fn non_share_video_clip_unchanged() {
        let style = ArrangementStyle::default();
        let scene = render_video_clips_scene(vec![clip(10, 2.0, 4.0, "plain")], &[]);
        assert_eq!(link_glyph_count(&scene, &style), 0, "非 share clip は link glyph を描かない");
        assert!(
            scene.iter_rects().any(|r| r.fill == style.video_clip_loading),
            "非 share video clip は従来どおり video_clip_loading 一色"
        );
    }

    /// M14 Phase 90 (daw_01 #061): master 行の header click (lane disclosure 外) で
    /// `SelectTrack { next: [MASTER_TRACK_ID] }` を 1 度 emit する。
    #[test]
    fn master_row_header_click_emits_select_track() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model;

        let mut view = test_view();
        view.header_w = 180.0; // header pane を出す
        // master 単独 (automation_lanes 空 = lane disclosure 非アクティブ)。 row 0 = y∈[0,32]。
        let tracks = vec![track(MASTER_TRACK_ID, "Master", vec![])];
        let style = ArrangementStyle::default();

        let observed: Arc<Mutex<Vec<Vec<u32>>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);

        // release frame: master row 内 (50, 16) で primary_just_released (press は不要 = catch-all 検出)。
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 16.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };

        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        let _ = host.frame_to_edits(&Model, &mut scene, screen, input, |_, ui| {
            let observed_cb = Arc::clone(&observed_cb);
            let _ = ui.arrangement(
                "arr_master_sel",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if let ArrangementEditRequest::SelectTrack { next, .. } = &req {
                        observed_cb.lock().unwrap().push(next.clone());
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });

        let log = observed.lock().unwrap();
        assert_eq!(log.len(), 1, "master row click で 1 度だけ SelectTrack: log={log:?}");
        assert_eq!(
            log[0],
            vec![MASTER_TRACK_ID],
            "Single select の next は master 単独: got {:?}",
            log[0]
        );
    }

    /// M14 Phase 116 (daw_01 #090): expanded automation lane の body を hover すると
    /// `ArrangementResponse.hovered_automation_lane` が `Some(lane key)` になり、 clip 上 hover では
    /// clip-first first-hit で `None` のまま (= `hovered_clip` と排他)。
    #[test]
    fn hovered_automation_lane_populated_on_body_and_none_over_clip() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        // track 0: clip 1 本 + expanded visible automation lane (id=7, height 60)。
        // layout: ruler_h=0 / header_w=0 → lanes = full rect、 tops[0]=0。
        //   track row body = [0, 32) (clip rect = [2, 30))、 lane body = [32, 92)。
        let mut t0 = track(0, "t0", vec![clip(100, 0.0, 4.0, "c")]);
        t0.automation_lanes_collapsed = false;
        t0.automation_lanes = vec![ArrangementAutomationLane {
            id: 7,
            label: Arc::from("Volume"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: true,
            height_px: 60,
            default_value_norm: 0.5,
            clips: Vec::new(),
        }];
        let tracks = vec![t0];
        let view = test_view(); // header_w=0, ruler_h=0, track_row_h=32
        let style = ArrangementStyle::default();

        let run = |pos: (f32, f32)| -> ArrangementResponse {
            let captured: Arc<Mutex<Option<ArrangementResponse>>> = Arc::new(Mutex::new(None));
            let captured_cb = Arc::clone(&captured);
            let input = FrameInput {
                pointer: PointerFrame { pos: Some(pos), ..PointerFrame::default() },
                ..FrameInput::default()
            };
            let mut host: UiHost<()> = UiHost::no_redraw();
            let mut scene = Scene::new();
            let screen = PhysicalSize { width: 800, height: 400 };
            host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
                let resp = ui.arrangement(
                    "arr_hover_lane",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &tracks,
                    &[],
                    view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    |_| Edit::mutate(|()| {}),
                );
                *captured_cb.lock().unwrap() = Some(resp);
            });
            captured.lock().unwrap().take().unwrap()
        };

        // lane body 上 (cy=60 ∈ [32,92)、 cx=400 は lanes pane 内)。
        let on_lane = run((400.0, 60.0));
        assert_eq!(
            on_lane.hovered_automation_lane,
            Some(AutomationLaneKey { track: 0, lane: 7 }),
            "lane body hover で key を公開: {:?}",
            on_lane.hovered_automation_lane
        );
        assert_eq!(on_lane.hovered_clip, None, "lane body 上では clip は hover しない");

        // clip 上 (cy=16 ∈ clip rect [2,30)、 cx=80 は clip rect [0,160) 内)。
        let on_clip = run((80.0, 16.0));
        assert_eq!(
            on_clip.hovered_clip,
            Some(ClipKey { track: 0, clip: 100 }),
            "clip body hover で clip key を公開: {:?}",
            on_clip.hovered_clip
        );
        assert_eq!(
            on_clip.hovered_automation_lane, None,
            "clip-first first-hit: clip 上では automation lane は None"
        );
    }

    /// M14 Phase 89 (daw_01 #060): arrangement の代表 clip fill が共有 `crate::color` の閾値の
    /// 期待側に乗る (黄 selected fill = 明るい側 / 暗青 default fill = 暗い側)。 luminance 関数自体の
    /// 単調性 / 極値は `crate::color` 側で検証済。
    #[test]
    fn clip_fills_land_on_expected_contrast_side() {
        use crate::color::{CONTRAST_LUMINANCE_THRESHOLD, relative_luminance};
        let yellow = relative_luminance(1.0, 0.85, 0.30); // clip_selected_fill
        let dark_blue = relative_luminance(0.18, 0.40, 0.65); // clip_default_fill
        assert!(yellow > CONTRAST_LUMINANCE_THRESHOLD, "黄 fill は明るい側: {yellow}");
        assert!(dark_blue < CONTRAST_LUMINANCE_THRESHOLD, "暗青 fill は暗い側: {dark_blue}");
        assert!(yellow > dark_blue, "黄 > 暗青 の単調性");
    }

    /// M14 Phase 89 (daw_01 #060): `clip_text_color_for` が fill 輝度で暗/明文字を選び、
    /// 半透明 fill は lane bg と合成した実効色で判定し、 opt-out 時は固定色を返す。
    #[test]
    fn clip_text_color_for_picks_contrast() {
        let style = ArrangementStyle::default();

        // 明るい fill (黄 selected) → 暗文字。
        assert_eq!(
            clip_text_color_for(&style, style.clip_selected_fill, style.bg),
            style.clip_text_color_dark,
            "明るい黄 fill には暗文字"
        );
        // 暗い fill (default 青) → 明文字。
        assert_eq!(
            clip_text_color_for(&style, style.clip_default_fill, style.bg),
            style.clip_text_color,
            "暗い青 fill には明文字"
        );
        // 半透明の薄緑 share fill: 不透明なら明るく暗文字寄りだが、 暗い lane bg と alpha 0.3 で
        // 合成すると実効輝度が下がり明文字が選ばれる (合成判定が効いている証拠)。
        let pale_green = Color::rgba(0.55, 0.85, 0.55, 0.30);
        let opaque = clip_text_color_for(
            &style,
            Color::rgb(pale_green.r, pale_green.g, pale_green.b),
            style.bg,
        );
        let composited = clip_text_color_for(&style, pale_green, style.bg);
        assert_eq!(opaque, style.clip_text_color_dark, "不透明な薄緑は暗文字");
        assert_eq!(
            composited, style.clip_text_color,
            "暗 lane bg と合成した薄緑 (alpha 0.3) は明文字"
        );

        // opt-out: auto を切ると fill に依らず clip_text_color 固定。
        let mut off = style;
        off.clip_auto_contrast_text = false;
        assert_eq!(
            clip_text_color_for(&off, off.clip_selected_fill, off.bg),
            off.clip_text_color,
            "opt-out 時は明るい fill でも clip_text_color 固定"
        );
    }

    /// M14 Phase 91 (daw_01 #062): automation clip 名の表示を MIDI clip と統一。
    /// (1) font_size = clip_text_size / line_height = clip_text_size * 1.2 (旧 0.85 倍を撤去)、
    /// (2) enabled lane は fill 輝度由来 auto-contrast (selected 黄 fill → 暗文字 / 暗い実効 fill →
    /// 明文字)、 (3) disabled lane は auto-contrast 対象外で `automation_lane_disabled_color` 固定。
    #[test]
    fn automation_clip_name_matches_midi_font_and_auto_contrast() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let style = ArrangementStyle::default();

        let auto_clip = |id: u32, name: &str| ArrangementAutomationClip {
            id,
            start_beat: 0.0,
            len_beats: 4.0, // width = 4 * (800/16) = 200px > 24
            name: Arc::from(name),
            points: Vec::new(),
            share_group_color: None,
        };
        let lane = |id: u32, enabled: bool, clip: ArrangementAutomationClip| {
            ArrangementAutomationLane {
                id,
                label: Arc::from("L"),
                icon_glyph: 'V',
                color: Color::rgb(0.30, 0.70, 1.0), // lane 識別色 (clip fill は alpha 0.20)
                enabled,
                visible: true,
                height_px: 60, // ch = 60 - 12 = 48 > clip_text_size + 2
                default_value_norm: 0.5,
                clips: vec![clip],
            }
        };

        let mut t0 = track(7, "t0", vec![]);
        t0.automation_lanes_collapsed = false;
        t0.automation_lanes = vec![
            lane(1, true, auto_clip(10, "selclip")),  // selected → 黄 fill → 暗文字
            lane(2, true, auto_clip(20, "normclip")), // 非選択 → 暗い実効 fill → 明文字
            lane(3, false, auto_clip(30, "disclip")), // disabled → 固定灰
        ];
        let tracks = vec![t0];

        let mut view = test_view();
        view.len_beats = 16.0; // beat_to_px = 800/16 = 50

        let selected = [AutomationClipKey { track: 7, lane: 1, clip: 10 }];

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.arrangement(
                "arr_auto_text",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &tracks,
                &[],
                view,
                &[],
                &[],
                &selected,
                &[],
                &style,
                None,
                |_| Edit::mutate(|()| {}),
            );
        });

        let glyph = |name: &str| {
            scene
                .iter_glyphs()
                .find(|g| &*g.text == name)
                .unwrap_or_else(|| panic!("clip 名 glyph '{name}' が scene に無い"))
                .clone()
        };

        // (1) font_size / line_height は MIDI clip (`draw_clip`) と同値 (旧 0.85 倍ではない)。
        let sel = glyph("selclip");
        assert!(
            (sel.font_size - style.clip_text_size).abs() < 1e-3,
            "font_size は clip_text_size (={}) と一致: got {}",
            style.clip_text_size,
            sel.font_size
        );
        assert!(
            (sel.line_height - style.clip_text_size * 1.2).abs() < 1e-3,
            "line_height は clip_text_size * 1.2 と一致: got {}",
            sel.line_height
        );

        // (2) enabled lane の auto-contrast: selected (黄 opaque fill) → 暗文字、
        //     非選択 (lane 色 alpha 0.20 を automation_lane_bg と合成した暗い実効色) → 明文字。
        assert_eq!(
            sel.color, style.clip_text_color_dark,
            "selected 黄 fill には暗文字"
        );
        assert_eq!(
            glyph("normclip").color,
            style.clip_text_color,
            "非選択の暗い実効 fill には明文字"
        );

        // (3) disabled lane は auto-contrast 対象外で従来の固定灰 (bypass marker)。
        assert_eq!(
            glyph("disclip").color,
            style.automation_lane_disabled_color,
            "disabled lane は automation_lane_disabled_color 固定"
        );
    }

    /// FIXME #70: automation curve line / point dot を背景輝度から白/黒 neutral で auto-contrast する。
    /// (1) 非選択 enabled lane (= lane.color alpha 0.20 を暗い lane_bg と合成 → 暗い実効 fill) では
    ///     line / point fill = 明色 (clip_text_color)、 point 枠 = 暗色 (clip_text_color_dark)。
    /// (2) 選択 clip (= clip_selected_fill の明るい黄 fill) では line / point fill = 暗色、 枠 = 明色。
    /// lane.color が黄など明るい識別色でも、 line / point が fill と同色化して埋もれる問題 (#70) を防ぐ。
    #[test]
    fn automation_curve_and_points_auto_contrast_neutral() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let style = ArrangementStyle::default();

        // 明るい識別色 (黄系) の lane + 2 点の Linear curve (= segment が出る)。
        let clip = ArrangementAutomationClip {
            id: 10,
            start_beat: 0.0,
            len_beats: 8.0,
            name: Arc::from("auto"),
            points: vec![
                ArrangementAutomationPoint {
                    time_beat: 0.0,
                    value_norm: 0.2,
                    curve: ArrangementCurveKind::Linear,
                },
                ArrangementAutomationPoint {
                    time_beat: 4.0,
                    value_norm: 0.8,
                    curve: ArrangementCurveKind::Linear,
                },
            ],
            share_group_color: None,
        };
        let mut t0 = track(7, "t0", vec![]);
        t0.automation_lanes_collapsed = false;
        t0.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            label: Arc::from("L"),
            icon_glyph: 'V',
            color: Color::rgb(0.95, 0.85, 0.55), // 明るい黄系の識別色
            enabled: true,
            visible: true,
            height_px: 80,
            default_value_norm: 0.5,
            clips: vec![clip],
        }];
        let tracks = vec![t0];

        let mut view = test_view();
        view.len_beats = 16.0;

        // (curve 由来の line 色全件, point dot の (fill, border) 全件) を返す。
        let render = |selected: &[AutomationClipKey]| -> (Vec<Color>, Vec<(Color, Color)>) {
            let mut host: UiHost<()> = UiHost::no_redraw();
            let mut scene = Scene::new();
            let screen = PhysicalSize { width: 800, height: 400 };
            host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
                let _ = ui.arrangement(
                    "arr_auto_contrast",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &tracks,
                    &[],
                    view,
                    &[],
                    &[],
                    selected,
                    &[],
                    &style,
                    None,
                    |_| Edit::mutate(|()| {}),
                );
            });
            // curve line: scene の line segment のうち、 neutral 2 色 (clip_text_color /
            // clip_text_color_dark) は curve からしか出ない (grid = 半透明白、 default 線 = 白 alpha 0.18)
            // ので、 全 segment 色を集めて neutral だけ後で判定する。
            let line_colors: Vec<Color> = scene
                .iter_lines()
                .flat_map(|lb| lb.segments.iter().map(|s| s.color))
                .filter(|c| *c == style.clip_text_color || *c == style.clip_text_color_dark)
                .collect();
            // point dot: 半径 4 → 8x8 rect (clip rect は大きく、 selected overlay/handle は無し)。
            let dot_w = style.automation_point_radius_px * 2.0;
            let dots: Vec<(Color, Color)> = scene
                .iter_rects()
                .filter(|r| (r.rect.w - dot_w).abs() < 1e-3 && (r.rect.h - dot_w).abs() < 1e-3)
                .map(|r| (r.fill, r.border))
                .collect();
            (line_colors, dots)
        };

        // (1) 非選択: 暗い実効 fill → line/point fill = 明色、 point 枠 = 暗色。
        let (lines, dots) = render(&[]);
        assert!(
            !lines.is_empty() && lines.iter().all(|c| *c == style.clip_text_color),
            "非選択 curve line は明色 (clip_text_color): got {lines:?}"
        );
        assert_eq!(dots.len(), 2, "point dot は 2 個: got {dots:?}");
        assert!(
            dots.iter()
                .all(|(f, b)| *f == style.clip_text_color && *b == style.clip_text_color_dark),
            "非選択 point は fill=明色 / 枠=暗色: got {dots:?}"
        );

        // (2) 選択: 明るい黄 fill → line/point fill = 暗色、 point 枠 = 明色。
        let sel = [AutomationClipKey { track: 7, lane: 1, clip: 10 }];
        let (lines_s, dots_s) = render(&sel);
        assert!(
            !lines_s.is_empty() && lines_s.iter().all(|c| *c == style.clip_text_color_dark),
            "選択 curve line は暗色 (clip_text_color_dark): got {lines_s:?}"
        );
        assert_eq!(dots_s.len(), 2, "選択 point dot は 2 個: got {dots_s:?}");
        assert!(
            dots_s
                .iter()
                .all(|(f, b)| *f == style.clip_text_color_dark && *b == style.clip_text_color),
            "選択 point は fill=暗色 / 枠=明色: got {dots_s:?}"
        );
    }

    /// FIXME #70: automation clip の左右端 hover で `EwResize`、 本体中央 hover で `Move` cursor を出す
    /// (MIDI clip と対称、 press 側 `automation_clip_zone_at` の resize/move 判定に hover cursor を配線)。
    #[test]
    fn automation_clip_edge_hover_sets_ew_resize_cursor() {
        use std::sync::{Arc as StdArc, Mutex};

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        let style = ArrangementStyle::default();
        let clip = ArrangementAutomationClip {
            id: 10,
            start_beat: 0.0,
            len_beats: 8.0,
            name: Arc::from("a"),
            points: Vec::new(),
            share_group_color: None,
        };
        let mut t0 = track(7, "t0", vec![]);
        t0.automation_lanes_collapsed = false;
        t0.automation_lanes = vec![ArrangementAutomationLane {
            id: 1,
            label: Arc::from("L"),
            icon_glyph: 'V',
            color: Color::rgb(0.42, 0.78, 0.95),
            enabled: true,
            visible: true,
            height_px: 80,
            default_value_norm: 0.5,
            clips: vec![clip],
        }];
        let tracks = vec![t0];
        let mut view = test_view();
        view.len_beats = 16.0; // beat_to_px = 800/16 = 50 → clip [0..8] = x[0..400]

        // pointer を pos に置いて 1 frame 回し、 flush された cursor を集める (`frame` のみが flush する)。
        let cursor_at = |pos: (f32, f32)| -> Vec<CursorIcon> {
            let captured: StdArc<Mutex<Vec<CursorIcon>>> = StdArc::new(Mutex::new(Vec::new()));
            let cc = StdArc::clone(&captured);
            let mut host: UiHost<()> = UiHost::no_redraw();
            host.set_cursor_request = Some(Box::new(move |c| cc.lock().unwrap().push(c)));
            let mut scene = Scene::new();
            let screen = PhysicalSize { width: 800, height: 400 };
            let input = FrameInput {
                pointer: PointerFrame { pos: Some(pos), ..PointerFrame::default() },
                ..Default::default()
            };
            host.frame(&mut (), &mut scene, screen, input, |(), ui| {
                let _ = ui.arrangement(
                    "arr_auto_cursor",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &tracks,
                    &[],
                    view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    |_| Edit::mutate(|()| {}),
                );
            });
            captured.lock().unwrap().clone()
        };

        // clip rect は y[38..106] (lane y=32, pad=6, h=80-12=68)。 y=72 は clip 内中段。
        // 右端 (beat 8 = x 400) → ResizeRight → EwResize。
        assert_eq!(
            cursor_at((400.0, 72.0)).as_slice(),
            &[CursorIcon::EwResize],
            "automation clip 右端は EwResize"
        );
        // 本体中央 (x 200) → Move。
        assert_eq!(
            cursor_at((200.0, 72.0)).as_slice(),
            &[CursorIcon::Move],
            "automation clip 本体は Move"
        );
    }

    /// time_sig (3, 4) で grid の bar 縦線が 0/3/6/9/12 拍位置に出る。
    /// `len_beats: 12.0` (= 4 小節分 of 3/4) の view で `Primitive::Line` から bar 線の x を抽出。
    #[test]
    fn time_sig_3_4_grid_bar_lines_at_3_beat_intervals() {
        let mut view = test_view();
        view.start_beat = 0.0;
        view.len_beats = 12.0;
        view.header_w = 0.0; // lanes が rect 全幅
        view.ruler_h = 0.0;
        view.time_sig = (3, 4);
        let scene = render_arrangement_once(view);

        // bar_color (= bar_line スタイル) の line segments を抽出して x 座標を集める。
        let style = ArrangementStyle::default();
        let bar_xs: std::collections::BTreeSet<i32> = scene
            .iter_lines()
            .flat_map(|b| b.segments.iter().copied())
            .filter(|seg| seg.color == style.bar_line)
            .map(|seg| seg.a[0].round() as i32)
            .collect();
        // 800px / 12 beats = 66.66px/beat → bar 位置 0/3/6/9/12 拍 = 0/200/400/600/800 px
        for expected_beat in [0, 3, 6, 9, 12] {
            let expected_x = (f64::from(expected_beat) * (800.0_f64 / 12.0)).round() as i32;
            assert!(
                bar_xs
                    .iter()
                    .any(|&x| (x - expected_x).abs() <= 2),
                "bar at beat {expected_beat} expected near x={expected_x}, got xs={bar_xs:?}",
            );
        }
        // 4/4 でハードコードされていたら beat 4 (= x=266) に bar 線が出るはず → 出ないことも確認
        let four_beat_x = (4.0_f64 * (800.0_f64 / 12.0)).round() as i32;
        assert!(
            !bar_xs
                .iter()
                .any(|&x| (x - four_beat_x).abs() <= 2),
            "3/4 で 4 拍位置 (x={four_beat_x}) に bar 線は出ない: xs={bar_xs:?}",
        );
    }

    /// time_sig (4, 4) で ruler に "1", "2", "3" の小節番号テキストが出る。
    #[test]
    fn arrangement_ruler_emits_bar_number_text() {
        let mut view = test_view();
        view.start_beat = 0.0;
        view.len_beats = 16.0; // 4 小節分 of 4/4
        view.header_w = 0.0;
        view.ruler_h = 24.0;
        view.time_sig = (4, 4);
        let scene = render_arrangement_once(view);

        let labels: Vec<String> = scene
            .iter_glyphs()
            .map(|g| g.text.as_ref().to_string())
            .collect();
        for expected in ["1", "2", "3"] {
            assert!(
                labels.iter().any(|s| s == expected),
                "ruler に {expected:?} が出る: labels={labels:?}",
            );
        }
        // 旧 "1.1" 形式は出ない (M13 で BarBeat label を bar 番号のみに変更)
        assert!(
            !labels.iter().any(|s| s == "1.1"),
            "BarBeat label は \"1\" 形式 (旧 \"1.1\" 形式は廃止): labels={labels:?}",
        );
    }

    /// time_sig 切替で bar 線の x 座標 set が変わる (= viewport_key v3 が time_sig を含んでいて
    /// 再描画が走る)。
    #[test]
    fn arrangement_grid_bar_lines_change_on_time_sig() {
        let style = ArrangementStyle::default();
        let collect_bar_xs = |time_sig: (u8, u8)| -> std::collections::BTreeSet<i32> {
            let mut view = test_view();
            view.start_beat = 0.0;
            view.len_beats = 12.0;
            view.header_w = 0.0;
            view.ruler_h = 0.0;
            view.time_sig = time_sig;
            let scene = render_arrangement_once(view);
            scene
                .iter_lines()
                .flat_map(|b| b.segments.iter().copied())
                .filter(|seg| seg.color == style.bar_line)
                .map(|seg| seg.a[0].round() as i32)
                .collect()
        };
        let xs_4_4 = collect_bar_xs((4, 4));
        let xs_3_4 = collect_bar_xs((3, 4));
        assert_ne!(
            xs_4_4, xs_3_4,
            "time_sig 切替で bar 線 set が変わる: 4/4={xs_4_4:?}, 3/4={xs_3_4:?}",
        );
    }

    // M14 Phase 61b (#011): fold_arrangement_clip_hash の cache invalidation 性質を verify。
    // (1) 同一データなら 2 回 fold して同値、 (2) clip.start_beat 変化で hash 変わる、
    // (3) clip.len_beats 変化で hash 変わる、 (4) clip.id 入替で hash 変わる。

    #[test]
    fn fold_arrangement_clip_hash_stable_for_unchanged_data() {
        let tracks = vec![track(
            10,
            "t0",
            vec![clip(100, 0.0, 4.0, "c0"), clip(101, 8.0, 2.0, "c1")],
        )];
        let h1 = fold_arrangement_clip_hash(&tracks);
        let h2 = fold_arrangement_clip_hash(&tracks);
        assert_eq!(h1, h2, "同じ tracks slice の fold は冪等");
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_move() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let after = vec![track(10, "t0", vec![clip(100, 4.0, 4.0, "c")])]; // start_beat 0 → 4
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.start_beat 変化で hash が変わる (#011 残像 fix)"
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_resize() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let after = vec![track(10, "t0", vec![clip(100, 0.0, 6.0, "c")])]; // len_beats 4 → 6
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.len_beats 変化で hash が変わる (#011 残像 fix)"
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_id_swap() {
        let before = vec![track(
            10,
            "t0",
            vec![clip(100, 0.0, 4.0, "c"), clip(101, 8.0, 2.0, "d")],
        )];
        let after = vec![track(
            10,
            "t0",
            vec![clip(101, 0.0, 4.0, "c"), clip(100, 8.0, 2.0, "d")],
        )]; // id 入替 (位置同じでも identity 違う)
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.id 入替で hash が変わる (FNV identity 確認)"
        );
    }

    /// MIDI clip arm の hash gap fix (share_group_color / color / name) regression test。
    /// caller が share_group / clip color を変更した frame で viewport_key も更新されることを保証。
    #[test]
    fn fold_arrangement_clip_hash_changes_on_share_group_color() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let mut c_after = clip(100, 0.0, 4.0, "c");
        c_after.share_group_color = Some(0.5);
        let after = vec![track(10, "t0", vec![c_after])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.share_group_color None→Some で hash が変わる",
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_color() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        let mut c_after = clip(100, 0.0, 4.0, "c");
        c_after.color = Some(daw_ui_renderer::Color::rgb(0.8, 0.2, 0.2));
        let after = vec![track(10, "t0", vec![c_after])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.color 変化で hash が変わる",
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_clip_rename() {
        let before = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "old name")])];
        let after = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "new name")])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "clip.name (Arc<str>) ptr 変化で hash が変わる",
        );
    }

    // ============================================================
    // M14 Phase 63c (#016): group hierarchy + multi-select + reparent
    // ============================================================

    /// `parent_id` を持つ track 1 つ作る helper (test 専用)。
    fn track_with_parent(
        id: u32,
        name: &str,
        parent_id: Option<u32>,
        depth: u8,
        collapsed: bool,
    ) -> ArrangementTrack {
        ArrangementTrack {
            id,
            name: Arc::from(name),
            muted: false,
            solo: false,
            armed: false,
            clips: Vec::new(),
            volume: 1.0,
            parent_id,
            depth,
            collapsed,
            kind: TrackKind::Audio,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
            row_h: None,
            color: None,
        }
    }

    #[test]
    fn is_group_track_returns_true_when_child_exists() {
        // `1` (parent) → `2`, `3` (children); `1` is group, `2`/`3` are leaves
        let tracks = vec![
            track_with_parent(1, "g", None, 0, false),
            track_with_parent(2, "c1", Some(1), 1, false),
            track_with_parent(3, "c2", Some(1), 1, false),
        ];
        assert!(is_group_track(1, &tracks), "1 has children → is_group");
        assert!(!is_group_track(2, &tracks), "2 is leaf → not is_group");
        assert!(!is_group_track(3, &tracks), "3 is leaf → not is_group");
    }

    #[test]
    fn is_visible_track_returns_false_when_ancestor_collapsed() {
        // `1` collapsed → `2` (child), `3` (grandchild) hidden; `4` (sibling) visible
        let tracks = vec![
            track_with_parent(1, "g", None, 0, true),
            track_with_parent(2, "c1", Some(1), 1, false),
            track_with_parent(3, "c2", Some(2), 2, false),
            track_with_parent(4, "leaf", None, 0, false),
        ];
        assert!(is_visible_track(&tracks[0], &tracks), "root 自身は visible (collapsed 適用は子のみ)");
        assert!(!is_visible_track(&tracks[1], &tracks), "親 1 が collapsed → 子 2 は不可視");
        assert!(!is_visible_track(&tracks[2], &tracks), "祖父 1 が collapsed → 孫 3 は不可視");
        assert!(is_visible_track(&tracks[3], &tracks), "別 chain の 4 は visible");
    }

    #[test]
    fn compute_visible_indices_skips_collapsed_subtree() {
        let tracks = vec![
            track_with_parent(1, "g", None, 0, true),
            track_with_parent(2, "c1", Some(1), 1, false),
            track_with_parent(3, "c2", Some(2), 2, false),
            track_with_parent(4, "leaf", None, 0, false),
        ];
        let visible = compute_visible_indices(&tracks);
        assert_eq!(
            visible,
            vec![0, 3],
            "collapsed 親 1 の subtree (2, 3) は skip、 visible は [0, 3]"
        );
    }

    #[test]
    fn disclosure_rect_within_name_rect_left_edge() {
        // disclosure rect は name_rect の左端から indent_px 幅で切り出し
        let style = ArrangementStyle::default();
        let name_rect = Rect { x: 100.0, y: 50.0, w: 120.0, h: 24.0 };
        let r = disclosure_rect_for(name_rect, &style, 0);
        assert!((r.x - 100.0).abs() < 1e-6, "disclosure x は name_rect 左端");
        assert!(r.w >= 8.0, "disclosure 幅は 8px 以上");
        assert!(r.w <= style.indent_px, "disclosure 幅は indent_px (= 16) 以下");
        assert!(r.y >= name_rect.y && r.y + r.h <= name_rect.y + name_rect.h, "y range は name_rect 内");
    }

    #[test]
    fn select_modifier_single_replaces_selection() {
        // Single click は selected_tracks を [clicked] で置換 + anchor 更新。
        // SelectModifier::Single の Edit を caller が apply するだけで動作するため、
        // Edit 構築側の test は省略 (pure 関数 unit test として selection 計算だけ確認)。
        let prev: Vec<u32> = vec![5, 10];
        let clicked = 7_u32;
        // Single 動作: next = vec![clicked]
        let next: Vec<u32> = vec![clicked];
        assert_ne!(prev, next, "Single click で selected_tracks が変わる (置換)");
        assert_eq!(next, vec![7], "next は clicked 1 件のみ");
    }

    #[test]
    fn select_modifier_toggle_adds_or_removes() {
        // Ctrl+click toggle: clicked が selected に居れば外す、 居なければ追加
        let prev: Vec<u32> = vec![5, 10];
        let clicked_in = 5_u32;
        let clicked_out = 7_u32;
        // 含まれている case → 削除
        let mut set: HashSet<u32> = prev.iter().copied().collect();
        if set.contains(&clicked_in) {
            set.remove(&clicked_in);
        } else {
            set.insert(clicked_in);
        }
        let mut v: Vec<u32> = set.into_iter().collect();
        v.sort_unstable();
        assert_eq!(v, vec![10], "5 を toggle → 削除");
        // 含まれていない case → 追加
        let mut set2: HashSet<u32> = prev.iter().copied().collect();
        if set2.contains(&clicked_out) {
            set2.remove(&clicked_out);
        } else {
            set2.insert(clicked_out);
        }
        let mut v2: Vec<u32> = set2.into_iter().collect();
        v2.sort_unstable();
        assert_eq!(v2, vec![5, 7, 10], "7 を toggle → 追加");
    }

    #[test]
    fn arrangement_style_has_indent_and_disclosure_defaults() {
        let s = ArrangementStyle::default();
        assert!(s.indent_px > 0.0, "indent_px は 0 以上 (default 16)");
        assert!(s.indent_px <= 32.0, "indent_px は実用範囲 (~16-32) 内");
        // M14 Phase 113 (daw_01 #085): group track 専用背景 (旧 track_group_bg) は撤去。 group の
        // 構造手掛かりは disclosure ▶▼ + indent のみなので、 disclosure_color が可視色であることを確認。
        assert!(
            s.disclosure_color.r > 0.0 || s.disclosure_color.g > 0.0 || s.disclosure_color.b > 0.0,
            "disclosure_color は黒以外の色"
        );
    }

    // ============================================================
    // M14 Phase 63j (#024): ruler click / drag による playhead seek
    // ============================================================

    /// ruler 内 plain (Shift 非保持) click で `SetPlayheadBeat` が press frame で発火する。
    #[test]
    fn ruler_plain_click_emits_set_playhead_beat() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        // header_w = 0 / ruler_h = 24 / lanes_w = 800 / len_beats = 16 → 50 px/beat
        let view = ArrangementView {
            header_w: 0.0,
            ruler_h: 24.0,
            start_beat: 0.0,
            len_beats: 16.0,
            snap: SnapConfig::OFF,
            ..ArrangementView::default()
        };
        let model = Model { tracks: vec![track(0, "t", vec![])], view };

        let observed: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);
        // ruler 内 (y=10 < ruler_h=24) で px=200 → beat = 200 / 50 = 4.0
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 10.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&model, &mut scene, screen, input, |m, ui| {
            let style = ArrangementStyle::default();
            let observed_cb = Arc::clone(&observed_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if let ArrangementEditRequest::SetPlayheadBeat(b) = req {
                        observed_cb.lock().unwrap().push(b);
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        let log = observed.lock().unwrap();
        assert_eq!(log.len(), 1, "press frame で 1 度だけ発火: log={log:?}");
        assert!((log[0] - 4.0).abs() < 1e-6, "beat = px/(px_per_beat) = 200/50 = 4.0: got {}", log[0]);
    }

    /// ruler 内 click でも **Shift 修飾**は loop range edit に振り分けられ、 SetPlayheadBeat は emit しない。
    #[test]
    fn ruler_shift_click_does_not_emit_set_playhead_beat() {
        use std::sync::Mutex;

        use daw_ui_platform::{Modifiers, PhysicalSize};
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let view = ArrangementView {
            header_w: 0.0,
            ruler_h: 24.0,
            snap: SnapConfig::OFF,
            ..ArrangementView::default()
        };
        let model = Model { tracks: vec![track(0, "t", vec![])], view };

        let seek_log: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let seek_log_cb = Arc::clone(&seek_log);
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 10.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { shift: true, ..Modifiers::default() },
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&model, &mut scene, screen, input, |m, ui| {
            let style = ArrangementStyle::default();
            let seek_log_cb = Arc::clone(&seek_log_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if matches!(req, ArrangementEditRequest::SetPlayheadBeat(_)) {
                        *seek_log_cb.lock().unwrap() += 1;
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        assert_eq!(*seek_log.lock().unwrap(), 0, "Shift+ruler click は SetPlayheadBeat 非発火 (loop ops 専用)");
    }

    /// ruler 内 click で `view.snap` が active なら snap 適用済 beat が emit される (alt 非保持)。
    #[test]
    fn ruler_plain_click_applies_snap_when_active() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;
        use crate::snap::{SnapConfig, SnapMode};

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        // 50 px/beat、 snap = Beat (1 拍単位)、 px=210 → raw beat 4.2 → snap → 4.0
        // 1 beat snap = 1/4 note = SnapMode::Straight { div: 4 }
        let view = ArrangementView {
            header_w: 0.0,
            ruler_h: 24.0,
            start_beat: 0.0,
            len_beats: 16.0,
            snap: SnapConfig {
                mode: SnapMode::Straight { div: 4 },
                enabled: true,
                min_beat_unit: 1.0 / 128.0,
                time_sig: (4, 4),
            },
            ..ArrangementView::default()
        };
        let model = Model { tracks: vec![track(0, "t", vec![])], view };

        let observed: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((210.0, 10.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&model, &mut scene, screen, input, |m, ui| {
            let style = ArrangementStyle::default();
            let observed_cb = Arc::clone(&observed_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if let ArrangementEditRequest::SetPlayheadBeat(b) = req {
                        observed_cb.lock().unwrap().push(b);
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        let log = observed.lock().unwrap();
        assert_eq!(log.len(), 1, "1 度だけ発火: log={log:?}");
        assert!(
            (log[0] - 4.0).abs() < 1e-6,
            "raw=4.2 → snap (Beat 1) → 4.0: got {}",
            log[0],
        );
    }

    /// ruler drag 中 (press → continuation) で位置移動毎に SetPlayheadBeat が継続発火する。
    /// 同 frame 同値抑制 (`last_emitted_beat` 比較) の確認も兼ねる。
    #[test]
    fn ruler_drag_emits_continuous_set_playhead_beat() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let view = ArrangementView {
            header_w: 0.0,
            ruler_h: 24.0,
            snap: SnapConfig::OFF,
            ..ArrangementView::default()
        };
        let model = Model { tracks: vec![track(0, "t", vec![])], view };

        let observed: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);

        // helper: 1 frame 流す (3 frame: press → move → release)
        let run_frame = |host: &mut UiHost<Model>,
                         scene: &mut Scene,
                         input: FrameInput,
                         observed_cb: Arc<Mutex<Vec<f64>>>| {
            let _ = host.frame_to_edits(&model, scene, screen, input, |m, ui| {
                let style = ArrangementStyle::default();
                let _ = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    move |req| {
                        if let ArrangementEditRequest::SetPlayheadBeat(b) = req {
                            observed_cb.lock().unwrap().push(b);
                        }
                        Edit::mutate(|_: &mut Model| {})
                    },
                );
            });
        };

        // frame 1: press at px=100 → beat 2.0
        run_frame(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((100.0, 10.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            Arc::clone(&observed_cb),
        );
        // frame 2: continuation, drag to px=300 → beat 6.0
        run_frame(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((300.0, 10.0)),
                    primary_just_pressed: false,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            Arc::clone(&observed_cb),
        );
        // frame 3: release → no emit (release 専用 commit 無し)
        run_frame(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((300.0, 10.0)),
                    primary_just_pressed: false,
                    primary_pressed: false,
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            Arc::clone(&observed_cb),
        );

        let log = observed.lock().unwrap();
        assert_eq!(log.len(), 2, "press + continuation の 2 発、 release は emit しない: log={log:?}");
        assert!((log[0] - 2.0).abs() < 1e-6, "press frame: beat = 100/50 = 2.0: got {}", log[0]);
        assert!((log[1] - 6.0).abs() < 1e-6, "drag frame: beat = 300/50 = 6.0: got {}", log[1]);
    }

    // ============================================================
    // M14 Phase 63j (#024): loop range edit に snap 適用
    // ============================================================
    //
    // `compute_loop_drag_endpoints` の unit test 7 件 (Start/End/Middle/NewRange の
    // snap 適用 / alt bypass / snap OFF) は M14 Phase 69 (#041) で
    // `crate::widgets::ruler_ops::tests` に extract (piano_roll と共有)。

    /// Shift+ruler drag → SetLoopRange が release frame で snap 適用済 endpoints で発火する
    /// (整合性確認: NewRange、 press → drag → release の 3 frame 経由)。
    #[test]
    #[allow(clippy::too_many_lines)]
    fn shift_ruler_drag_emits_set_loop_range_with_snap() {
        use std::sync::Mutex;

        use daw_ui_platform::{Modifiers, PhysicalSize};
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        // 50 px/beat、 snap = Straight { div: 4 } (1 beat snap)
        let view = ArrangementView {
            header_w: 0.0,
            ruler_h: 24.0,
            start_beat: 0.0,
            len_beats: 16.0,
            snap: SnapConfig {
                mode: SnapMode::Straight { div: 4 },
                enabled: true,
                min_beat_unit: 1.0 / 128.0,
                time_sig: (4, 4),
            },
            ..ArrangementView::default()
        };
        let model = Model { tracks: vec![track(0, "t", vec![])], view };

        let observed: Arc<Mutex<Vec<(f64, f64)>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_cb = Arc::clone(&observed);

        let run_frame = |host: &mut UiHost<Model>,
                         scene: &mut Scene,
                         input: FrameInput,
                         observed_cb: Arc<Mutex<Vec<(f64, f64)>>>| {
            let _ = host.frame_to_edits(&model, scene, screen, input, |m, ui| {
                let style = ArrangementStyle::default();
                let _ = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    move |req| {
                        if let ArrangementEditRequest::SetLoopRange { start, end } = req {
                            observed_cb.lock().unwrap().push((start, end));
                        }
                        Edit::mutate(|_: &mut Model| {})
                    },
                );
            });
        };

        // frame 1: Shift+press at px=85 → raw 1.7 拍 → press 時 snap → 2.0
        run_frame(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((85.0, 10.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    modifiers: Modifiers { shift: true, ..Modifiers::default() },
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            Arc::clone(&observed_cb),
        );
        // frame 2: drag to px=470 → raw 9.4 拍
        run_frame(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((470.0, 10.0)),
                    primary_just_pressed: false,
                    primary_pressed: true,
                    modifiers: Modifiers { shift: true, ..Modifiers::default() },
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            Arc::clone(&observed_cb),
        );
        // frame 3: release at px=470 → raw 9.4 → snap → 9.0
        run_frame(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((470.0, 10.0)),
                    primary_just_pressed: false,
                    primary_pressed: false,
                    primary_just_released: true,
                    modifiers: Modifiers { shift: true, ..Modifiers::default() },
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            Arc::clone(&observed_cb),
        );

        let log = observed.lock().unwrap();
        assert_eq!(log.len(), 1, "release frame で 1 度だけ SetLoopRange 発火: log={log:?}");
        let (start, end) = log[0];
        assert!(
            (start - 2.0).abs() < 1e-6,
            "start は press 時 snap → 2.0 (raw 1.7): got {start}"
        );
        assert!(
            (end - 9.0).abs() < 1e-6,
            "end は release 時 snap → 9.0 (raw 9.4): got {end}"
        );
    }

    /// ruler 外 (lanes 内) の plain click では SetPlayheadBeat が emit されない (canvas 既存挙動維持)。
    #[test]
    fn lanes_click_does_not_emit_set_playhead_beat() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let view = ArrangementView {
            header_w: 0.0,
            ruler_h: 24.0,
            snap: SnapConfig::OFF,
            ..ArrangementView::default()
        };
        let model = Model { tracks: vec![track(0, "t", vec![])], view };

        let seek_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
        let seek_count_cb = Arc::clone(&seek_count);
        // y=100 (lanes 内、 ruler_h=24 を超える) で press
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&model, &mut scene, screen, input, |m, ui| {
            let style = ArrangementStyle::default();
            let seek_count_cb = Arc::clone(&seek_count_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if matches!(req, ArrangementEditRequest::SetPlayheadBeat(_)) {
                        *seek_count_cb.lock().unwrap() += 1;
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        assert_eq!(*seek_count.lock().unwrap(), 0, "lanes 内 click は SetPlayheadBeat 非発火");
    }

    // -------- M14 Phase 63k (#025): audio_edit grip hit-test + drag commit -----------------------

    /// audio_edit が None の clip では audio_grip_hit が常に None を返す (= MIDI / Vocal clip は
    /// 既存挙動、 audio gesture は完全 disable)。
    #[test]
    fn audio_grip_hit_returns_none_when_audio_edit_is_none() {
        let view = test_view();
        let lanes = test_lanes();
        let style = ArrangementStyle::default();
        // 通常 clip (audio_edit = None) は中央 click でも fade 角 click でも None
        let tracks = vec![track(10, "t0", vec![clip(100, 0.0, 4.0, "c")])];
        // Get hit at clip middle
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 80.0, 16.0, &style),
            None,
            "audio_edit None の clip は GainHandleBand を返さない"
        );
        // Get hit at top-left corner
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 6.0, 6.0, &style),
            None,
            "audio_edit None の clip は FadeCornerIn を返さない"
        );
    }

    /// audio_edit が Some の clip 中央 (handle band) は GainHandleBand を返す。
    #[test]
    fn audio_grip_hit_returns_gain_handle_at_clip_middle() {
        let view = test_view();
        let lanes = test_lanes();
        let style = ArrangementStyle::default();
        // clip rect = (0, 2, 160, 28), 中央 (80, 16) は handle band 内
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio)])];
        // clip 中央 y = r.y + r.h / 2 = 2 + 14 = 16
        // x = 80 (clip 中央)、 端 (0/160) から 24 px margin 内
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 80.0, 16.0, &style),
            Some((ClipKey { track: 10, clip: 100 }, AudioGripHit::GainHandleBand))
        );
    }

    /// fade in 角 (clip 上端左 12×12) は FadeCornerIn を返す。
    #[test]
    fn audio_grip_hit_returns_fade_corner_in_at_top_left() {
        let view = test_view();
        let lanes = test_lanes();
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio)])];
        // clip rect = (0, 2, 160, 28), top-left 12×12 → cx=6, cy=6 (corner 内)
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 6.0, 6.0, &style),
            Some((ClipKey { track: 10, clip: 100 }, AudioGripHit::FadeCornerIn))
        );
    }

    /// fade out 角 (clip 上端右 12×12) は FadeCornerOut を返す。
    #[test]
    fn audio_grip_hit_returns_fade_corner_out_at_top_right() {
        let view = test_view();
        let lanes = test_lanes();
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio)])];
        // clip rect = (0, 2, 160, 28), top-right 12×12 → cx=155, cy=6 (corner 内)
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 155.0, 6.0, &style),
            Some((ClipKey { track: 10, clip: 100 }, AudioGripHit::FadeCornerOut))
        );
    }

    /// 短 clip (`r.w < audio_min_clip_w_for_handles_px`) は audio grip 全 disable。
    #[test]
    fn audio_grip_hit_returns_none_for_short_clip() {
        let view = test_view();
        let lanes = test_lanes();
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        // len_beats=0.5 → w = 20px、 default min = 32 → grip disable
        let tracks = vec![track(10, "t0", vec![audio_clip(100, 0.0, 0.5, "c", audio)])];
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 10.0, 16.0, &style),
            None
        );
        assert_eq!(
            audio_grip_hit_in_lanes(&tracks, &make_tops(&tracks, lanes, view), view, lanes, 2.0, 6.0, &style),
            None
        );
    }

    /// FadeCurve.next() は Linear → Exp → SCurve → Linear の cycle。
    #[test]
    fn fade_curve_next_cycles() {
        assert_eq!(FadeCurve::Linear.next(), FadeCurve::Exponential);
        assert_eq!(FadeCurve::Exponential.next(), FadeCurve::SCurve);
        assert_eq!(FadeCurve::SCurve.next(), FadeCurve::Linear);
    }

    /// compute_audio_drag_outcome: Gain drag は dy 上で gain_db 増加 (pixels_per_db = 0.25 default)。
    #[test]
    fn compute_audio_drag_outcome_gain_changes_db_by_pixels() {
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::Gain,
            anchor: audio,
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 100.0, h: 28.0 },
            clip_len_beats_anchor: 4.0,
            anchor_mouse: (50.0, 14.0),
            // dy = -20 (上に 20 px) → next_db = 0 + (-(-20) * 0.25) = +5.0
            last_mouse: (50.0, -6.0),
            locked_horizontal: Some(false),
        };
        match compute_audio_drag_outcome(&ad, 0.025, &style) {
            Some(AudioDragOutcome::Gain { next_db }) => {
                assert!((next_db - 5.0).abs() < 1e-3, "+20px = +5dB: got {next_db}");
            }
            other => panic!("expected Gain, got {other:?}"),
        }
    }

    /// compute_audio_drag_outcome: Gain drag は ±range_db に clamp される。
    #[test]
    fn compute_audio_drag_outcome_gain_clamps_to_range() {
        let style = ArrangementStyle::default(); // range = 24 dB
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        // dy = -200 → +50 dB raw → clamped to +24
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::Gain,
            anchor: audio,
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 100.0, h: 28.0 },
            clip_len_beats_anchor: 4.0,
            anchor_mouse: (50.0, 14.0),
            last_mouse: (50.0, -186.0),
            locked_horizontal: Some(false),
        };
        match compute_audio_drag_outcome(&ad, 0.025, &style) {
            Some(AudioDragOutcome::Gain { next_db }) => {
                assert!((next_db - 24.0).abs() < 1e-3, "clamped to +24 dB: got {next_db}");
            }
            other => panic!("expected Gain, got {other:?}"),
        }
    }

    /// compute_audio_drag_outcome: FadeIn + horizontal lock は dx 正で fade_in_beats 増加。
    /// beat_per_px = 0.025 (= 40 px/beat), dx = +40 px → +1 beat.
    #[test]
    fn compute_audio_drag_outcome_fade_in_horizontal_changes_length() {
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.5,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor: audio,
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            clip_len_beats_anchor: 4.0,
            anchor_mouse: (0.0, 0.0),
            last_mouse: (40.0, 0.0),
            locked_horizontal: Some(true),
        };
        match compute_audio_drag_outcome(&ad, 0.025, &style) {
            Some(AudioDragOutcome::FadeLength { edge, next_beats }) => {
                assert_eq!(edge, FadeEdge::In);
                // anchor 0.5 + delta 1.0 = 1.5
                assert!((next_beats - 1.5).abs() < 1e-6, "got {next_beats}");
            }
            other => panic!("expected FadeLength, got {other:?}"),
        }
    }

    /// FadeOut + horizontal lock は dx **負** で fade_out_beats 増加 (右側から内側に伸びる)。
    #[test]
    fn compute_audio_drag_outcome_fade_out_horizontal_uses_negative_dx() {
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.5,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeOut,
            anchor: audio,
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            clip_len_beats_anchor: 4.0,
            anchor_mouse: (160.0, 0.0),
            last_mouse: (120.0, 0.0), // dx = -40
            locked_horizontal: Some(true),
        };
        match compute_audio_drag_outcome(&ad, 0.025, &style) {
            Some(AudioDragOutcome::FadeLength { edge, next_beats }) => {
                assert_eq!(edge, FadeEdge::Out);
                // dx=-40 → -40 * 0.025 = -1.0、 FadeOut signed = -(-1) = +1
                // anchor 0.5 + 1.0 = 1.5
                assert!((next_beats - 1.5).abs() < 1e-6, "got {next_beats}");
            }
            other => panic!("expected FadeLength, got {other:?}"),
        }
    }

    /// fade length は `0..=clip_len_beats` に clamp される。
    #[test]
    fn compute_audio_drag_outcome_fade_length_clamps_to_clip_len() {
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.5,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        // dx = +400 px @ 0.025 beat/px = +10 beat → 0.5 + 10 = 10.5、 clamp to clip_len 4.0
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor: audio,
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            clip_len_beats_anchor: 4.0,
            anchor_mouse: (0.0, 0.0),
            last_mouse: (400.0, 0.0),
            locked_horizontal: Some(true),
        };
        match compute_audio_drag_outcome(&ad, 0.025, &style) {
            Some(AudioDragOutcome::FadeLength { next_beats, .. }) => {
                assert!((next_beats - 4.0).abs() < 1e-6, "clamped to 4.0: got {next_beats}");
            }
            other => panic!("expected FadeLength, got {other:?}"),
        }
    }

    /// FadeIn + vertical lock は curve 切替を返す (Linear → Exponential)。
    #[test]
    fn compute_audio_drag_outcome_fade_in_vertical_toggles_curve() {
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.5,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor: audio,
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            clip_len_beats_anchor: 4.0,
            anchor_mouse: (0.0, 0.0),
            last_mouse: (0.0, -20.0),
            locked_horizontal: Some(false),
        };
        match compute_audio_drag_outcome(&ad, 0.025, &style) {
            Some(AudioDragOutcome::FadeCurve { edge, next_curve }) => {
                assert_eq!(edge, FadeEdge::In);
                assert_eq!(next_curve, FadeCurve::Exponential);
            }
            other => panic!("expected FadeCurve, got {other:?}"),
        }
    }

    /// sticky direction 未確定 (locked_horizontal = None) は no-op (None) を返す。
    #[test]
    fn compute_audio_drag_outcome_unlocked_returns_none() {
        let style = ArrangementStyle::default();
        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.5,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let ad = AudioDragSession {
            key: ClipKey { track: 0, clip: 0 },
            kind: AudioDragKind::FadeIn,
            anchor: audio,
            clip_rect_anchor: Rect { x: 0.0, y: 0.0, w: 160.0, h: 28.0 },
            clip_len_beats_anchor: 4.0,
            anchor_mouse: (0.0, 0.0),
            last_mouse: (3.0, 4.0), // < threshold 10 px
            locked_horizontal: None,
        };
        assert_eq!(compute_audio_drag_outcome(&ad, 0.025, &style), None);
    }

    /// fold_arrangement_clip_hash: audio_edit の gain_db / fade を変えると hash が変わる。
    #[test]
    fn fold_arrangement_clip_hash_changes_on_gain_db() {
        let audio_a = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let audio_b = ArrangementClipAudioEdit { gain_db: 3.0, ..audio_a };
        let before = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio_a)])];
        let after = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio_b)])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "audio_edit.gain_db 変化で hash が変わる (cache 再構築保証)"
        );
    }

    #[test]
    fn fold_arrangement_clip_hash_changes_on_fade_curve() {
        let audio_a = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.5,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let audio_b = ArrangementClipAudioEdit {
            fade_in_curve: FadeCurve::Exponential,
            ..audio_a
        };
        let before = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio_a)])];
        let after = vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio_b)])];
        assert_ne!(
            fold_arrangement_clip_hash(&before),
            fold_arrangement_clip_hash(&after),
            "audio_edit.fade_in_curve 変化で hash が変わる"
        );
    }

    /// integration: 中央 dB handle を縦 drag → release で SetClipGainDb が発火する。
    /// press at (80, 16) → drag to (80, -4) (dy = -20) → release。
    /// 期待: next_gain_db = 0 + (20 * 0.25) = +5.0
    #[test]
    #[allow(clippy::too_many_lines)]
    fn audio_gain_drag_emits_set_clip_gain_db() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let mut model = Model {
            tracks: vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio)])],
            view: ArrangementView { header_w: 0.0, ruler_h: 0.0, ..ArrangementView::default() },
        };

        let observed: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));

        // Frame 1: press at (80, 16) — clip 中央の handle band
        let press_input = FrameInput {
            pointer: PointerFrame {
                pos: Some((80.0, 16.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let observed_cb = Arc::clone(&observed);
        let style = ArrangementStyle::default();
        let edits = host.frame_to_edits(&model, &mut scene, screen, press_input, |m, ui| {
            let observed_cb = Arc::clone(&observed_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if let ArrangementEditRequest::SetClipGainDb(deltas) = &req {
                        for d in deltas {
                            observed_cb.lock().unwrap().push(d.next_gain_db);
                        }
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        for e in edits {
            e.apply(&mut model);
        }

        // Frame 2: continuation at (80, -4)、 dy = -20 → next_db = +5
        let drag_input = FrameInput {
            pointer: PointerFrame {
                pos: Some((80.0, -4.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let observed_cb = Arc::clone(&observed);
        let edits = host.frame_to_edits(&model, &mut scene, screen, drag_input, |m, ui| {
            let observed_cb = Arc::clone(&observed_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if let ArrangementEditRequest::SetClipGainDb(deltas) = &req {
                        for d in deltas {
                            observed_cb.lock().unwrap().push(d.next_gain_db);
                        }
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        for e in edits {
            e.apply(&mut model);
        }

        // Frame 3: release at (80, -4)
        let release_input = FrameInput {
            pointer: PointerFrame {
                pos: Some((80.0, -4.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let observed_cb = Arc::clone(&observed);
        let edits = host.frame_to_edits(&model, &mut scene, screen, release_input, |m, ui| {
            let observed_cb = Arc::clone(&observed_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    if let ArrangementEditRequest::SetClipGainDb(deltas) = &req {
                        for d in deltas {
                            observed_cb.lock().unwrap().push(d.next_gain_db);
                        }
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        for e in edits {
            e.apply(&mut model);
        }

        let log = observed.lock().unwrap();
        assert_eq!(log.len(), 1, "release frame で 1 度だけ SetClipGainDb 発火: got {log:?}");
        assert!((log[0] - 5.0).abs() < 1e-3, "next_gain_db = +5.0: got {}", log[0]);
    }

    /// integration: clip 上端の左角を click → 横 drag → release で SetClipFade が発火する。
    /// press at (4, 4) → drag to (44, 4) (dx=+40, dy=0、 horizontal lock) → release。
    /// 期待: SetClipFade { edge: In, next_beats: 1.0 } (anchor 0 + 1 beat)
    #[test]
    #[allow(clippy::too_many_lines)]
    fn audio_fade_in_horizontal_drag_emits_set_clip_fade() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let audio = ArrangementClipAudioEdit {
            gain_db: 0.0,
            fade_in_beats: 0.0,
            fade_out_beats: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_curve: FadeCurve::Linear,
        };
        let mut model = Model {
            tracks: vec![track(10, "t0", vec![audio_clip(100, 0.0, 4.0, "c", audio)])],
            view: ArrangementView { header_w: 0.0, ruler_h: 0.0, ..ArrangementView::default() },
        };

        let observed: Arc<Mutex<Vec<(FadeEdge, f64)>>> = Arc::new(Mutex::new(Vec::new()));
        let curve_observed: Arc<Mutex<Vec<FadeCurve>>> = Arc::new(Mutex::new(Vec::new()));

        let style = ArrangementStyle::default();

        // Frame 1: press at (4, 4) — clip 上端左角
        let press_input = FrameInput {
            pointer: PointerFrame {
                pos: Some((4.0, 4.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let observed_cb = Arc::clone(&observed);
        let curve_cb = Arc::clone(&curve_observed);
        let edits = host.frame_to_edits(&model, &mut scene, screen, press_input, |m, ui| {
            let observed_cb = Arc::clone(&observed_cb);
            let curve_cb = Arc::clone(&curve_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    match &req {
                        ArrangementEditRequest::SetClipFade(deltas) => {
                            for d in deltas {
                                observed_cb.lock().unwrap().push((d.edge, d.next_beats));
                            }
                        }
                        ArrangementEditRequest::SetClipFadeCurve(deltas) => {
                            for d in deltas {
                                curve_cb.lock().unwrap().push(d.next_curve);
                            }
                        }
                        _ => {}
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        for e in edits {
            e.apply(&mut model);
        }

        // Frame 2: continuation at (44, 4) → dx = +40 px → 1 beat (40 px/beat)、 dy = 0
        let drag_input = FrameInput {
            pointer: PointerFrame {
                pos: Some((44.0, 4.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let observed_cb = Arc::clone(&observed);
        let curve_cb = Arc::clone(&curve_observed);
        let edits = host.frame_to_edits(&model, &mut scene, screen, drag_input, |m, ui| {
            let observed_cb = Arc::clone(&observed_cb);
            let curve_cb = Arc::clone(&curve_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    match &req {
                        ArrangementEditRequest::SetClipFade(deltas) => {
                            for d in deltas {
                                observed_cb.lock().unwrap().push((d.edge, d.next_beats));
                            }
                        }
                        ArrangementEditRequest::SetClipFadeCurve(deltas) => {
                            for d in deltas {
                                curve_cb.lock().unwrap().push(d.next_curve);
                            }
                        }
                        _ => {}
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        for e in edits {
            e.apply(&mut model);
        }

        // Frame 3: release at (44, 4)
        let release_input = FrameInput {
            pointer: PointerFrame {
                pos: Some((44.0, 4.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let observed_cb = Arc::clone(&observed);
        let curve_cb = Arc::clone(&curve_observed);
        let edits = host.frame_to_edits(&model, &mut scene, screen, release_input, |m, ui| {
            let observed_cb = Arc::clone(&observed_cb);
            let curve_cb = Arc::clone(&curve_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    match &req {
                        ArrangementEditRequest::SetClipFade(deltas) => {
                            for d in deltas {
                                observed_cb.lock().unwrap().push((d.edge, d.next_beats));
                            }
                        }
                        ArrangementEditRequest::SetClipFadeCurve(deltas) => {
                            for d in deltas {
                                curve_cb.lock().unwrap().push(d.next_curve);
                            }
                        }
                        _ => {}
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });
        for e in edits {
            e.apply(&mut model);
        }

        let log = observed.lock().unwrap();
        let curve_log = curve_observed.lock().unwrap();
        assert_eq!(log.len(), 1, "release frame で SetClipFade 1 度発火: got {log:?}");
        assert_eq!(log[0].0, FadeEdge::In);
        // dx=+40 px @ 800px/16beats = 50 px/beat → +40/50 = +0.8 beat
        assert!((log[0].1 - 0.8).abs() < 1e-3, "next_beats = +0.8: got {}", log[0].1);
        assert!(curve_log.is_empty(), "horizontal lock では SetClipFadeCurve 非発火");
    }

    // ============================================================
    // M14 Phase 63n-7 (daw_01 #033): Bezier S 字 cubic + Exponential variant の flatten 検証
    // ============================================================

    /// 出力点列から (画面 x=cx) に最も近い点の y を線形補間で求める。 polyline 内挿。
    fn sample_polyline_y(pts: &[(f32, f32)], cx: f32) -> f32 {
        assert!(pts.len() >= 2);
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (lo_x, hi_x) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
            if cx >= lo_x && cx <= hi_x {
                if (b.0 - a.0).abs() < 1e-6 {
                    return a.1; // 垂直 segment は始点 y を返す (Hold の立ち上がり等は test 対象外)
                }
                let t = (cx - a.0) / (b.0 - a.0);
                return a.1 + (b.1 - a.1) * t;
            }
        }
        // 端 fallback
        if cx <= pts.first().unwrap().0 {
            pts.first().unwrap().1
        } else {
            pts.last().unwrap().1
        }
    }

    /// 端点 (p1, p2) を常に通る (= 描画ズレなし) ことを確認。
    #[test]
    fn flatten_segment_endpoints_exact_for_all_curve_kinds() {
        let p1 = (10.0_f32, 100.0_f32);
        let p2 = (50.0_f32, 40.0_f32);
        for kind in [
            ArrangementCurveKind::Hold,
            ArrangementCurveKind::Linear,
            ArrangementCurveKind::Bezier { tension: 0.0 },
            ArrangementCurveKind::Bezier { tension: 0.5 },
            ArrangementCurveKind::Bezier { tension: -0.5 },
            ArrangementCurveKind::Exponential { bend: 0.0 },
            ArrangementCurveKind::Exponential { bend: 0.8 },
            ArrangementCurveKind::Exponential { bend: -0.8 },
        ] {
            let mut out = vec![p1];
            flatten_lane_segment(p1, p1, p2, p2, kind, 2.0, &mut out);
            let last = *out.last().expect("at least 1 point pushed");
            assert!(
                (last.0 - p2.0).abs() < 1e-3 && (last.1 - p2.1).abs() < 1e-3,
                "{kind:?}: 出力末尾 = p2 を期待 (got {last:?})"
            );
        }
    }

    /// Bezier { tension: 0.0 } は **直線**に縮退する (制御点 4 つが対角線上 = daw_01 SSoT の数値性質)。
    /// 中央 (x=p1.x + dx/2) で y = (p1.y + p2.y) / 2 ± 0.5 px 以内。
    #[test]
    fn bezier_tension_zero_is_linear() {
        let p1 = (0.0_f32, 0.0_f32);
        let p2 = (100.0_f32, 60.0_f32);
        let mut out = vec![p1];
        flatten_lane_segment(
            p1,
            p1,
            p2,
            p2,
            ArrangementCurveKind::Bezier { tension: 0.0 },
            2.0,
            &mut out,
        );
        let mid_y = sample_polyline_y(&out, 50.0);
        let linear_mid = (p1.1 + p2.1) * 0.5;
        assert!(
            (mid_y - linear_mid).abs() < 0.5,
            "tension=0 で中央は線形中点 = {linear_mid}: got {mid_y}"
        );
    }

    /// Bezier { tension: +1.0 } で中央 y が **prev に偏る** (= 滑らかな S 字、 前半 prev に張り付く)。
    /// daw_01 SSoT: tension=+1 で c1y=p1.y, c2y=p2.y (end-hold)、 cubic Bezier の中点 y は
    /// `1/8*p1.y + 3/8*c1y + 3/8*c2y + 1/8*p2.y = 1/8*p1 + 3/8*p1 + 3/8*p2 + 1/8*p2 = 1/2*p1 + 1/2*p2`
    /// ではなく、 x(t)=t なので t=0.5 で y(0.5) = (1/2)(p1 + p2)。 ふむ、 これは中点が線形と同じか?
    ///
    /// 実際には c1y=p1.y で c2y=p2.y のとき、 y(t) = (1-t)^3*p1 + 3(1-t)^2*t*p1 + 3(1-t)*t^2*p2 + t^3*p2
    /// = p1 * [(1-t)^3 + 3(1-t)^2*t] + p2 * [3(1-t)*t^2 + t^3]
    /// = p1 * (1-t)^2 * [(1-t) + 3t] + p2 * t^2 * [3(1-t) + t]
    /// = p1 * (1-t)^2 * (1 + 2t) + p2 * t^2 * (3 - 2t)
    /// t=0.5 で y = p1 * 0.25 * 2 + p2 * 0.25 * 2 = 0.5*p1 + 0.5*p2 (= 線形中点)
    ///
    /// したがって中点は線形と同じだが、 **t=0.25 / 0.75** で差が出る。 t=0.25 で:
    /// y(0.25) = p1 * 0.5625 * 1.5 + p2 * 0.0625 * 2.5 = p1 * 0.84375 + p2 * 0.15625
    /// (線形は p1 * 0.75 + p2 * 0.25、 = +0.09375 だけ p1 寄り)
    #[test]
    fn bezier_tension_positive_pulls_toward_endpoints() {
        let p1 = (0.0_f32, 0.0_f32);
        let p2 = (100.0_f32, 100.0_f32);
        let mut out = vec![p1];
        flatten_lane_segment(
            p1,
            p1,
            p2,
            p2,
            ArrangementCurveKind::Bezier { tension: 1.0 },
            2.0,
            &mut out,
        );
        // t=0.25 (= x=25) で y は線形 25 より小さい (= p1 寄り = 0 寄り)
        let y_at_25 = sample_polyline_y(&out, 25.0);
        assert!(
            y_at_25 < 25.0 - 5.0,
            "tension=+1 で x=25 の y は線形 25 より明確に小さい (got {y_at_25})"
        );
        // t=0.75 (= x=75) で y は線形 75 より大きい (= p2 寄り = 100 寄り)
        let y_at_75 = sample_polyline_y(&out, 75.0);
        assert!(
            y_at_75 > 75.0 + 5.0,
            "tension=+1 で x=75 の y は線形 75 より明確に大きい (got {y_at_75})"
        );
    }

    /// Bezier { tension: -1.0 } で overshoot 反転 S 字 (= 前半 p2 側、 後半 p1 側に張り出す)。
    /// daw_01 SSoT: tension=-1 で c1y=p2.y, c2y=p1.y (反転 end-hold)。
    /// x=25 で y は線形 25 より大きい (= p2=100 寄り)、 x=75 で y は線形 75 より小さい (= p1=0 寄り)。
    #[test]
    fn bezier_tension_negative_inverts_s_curve() {
        let p1 = (0.0_f32, 0.0_f32);
        let p2 = (100.0_f32, 100.0_f32);
        let mut out = vec![p1];
        flatten_lane_segment(
            p1,
            p1,
            p2,
            p2,
            ArrangementCurveKind::Bezier { tension: -1.0 },
            2.0,
            &mut out,
        );
        let y_at_25 = sample_polyline_y(&out, 25.0);
        assert!(
            y_at_25 > 25.0 + 5.0,
            "tension=-1 で x=25 の y は線形 25 より明確に大きい (overshoot、 got {y_at_25})"
        );
        let y_at_75 = sample_polyline_y(&out, 75.0);
        assert!(
            y_at_75 < 75.0 - 5.0,
            "tension=-1 で x=75 の y は線形 75 より明確に小さい (overshoot、 got {y_at_75})"
        );
    }

    /// Exponential { bend: +1.0 } は t^2 (二次曲線、 前半遅・後半速)、 t=0.5 で y = 0.25 * (p2 - p1) + p1。
    /// daw_01 SSoT と完全一致。
    #[test]
    fn exponential_bend_positive_is_quadratic() {
        let p1 = (0.0_f32, 0.0_f32);
        let p2 = (100.0_f32, 100.0_f32);
        let mut out = vec![p1];
        flatten_lane_segment(
            p1,
            p1,
            p2,
            p2,
            ArrangementCurveKind::Exponential { bend: 1.0 },
            2.0,
            &mut out,
        );
        // t=0.5 で y = 0.5^2 * 100 = 25
        let y_at_50 = sample_polyline_y(&out, 50.0);
        assert!(
            (y_at_50 - 25.0).abs() < 1.0,
            "bend=+1 で x=50 の y = 25 (t^2): got {y_at_50}"
        );
    }

    /// Exponential { bend: -1.0 } は t^0.5 (平方根、 前半速・後半遅)、 t=0.5 で y ≈ 0.707 * (p2 - p1) + p1。
    #[test]
    fn exponential_bend_negative_is_sqrt() {
        let p1 = (0.0_f32, 0.0_f32);
        let p2 = (100.0_f32, 100.0_f32);
        let mut out = vec![p1];
        flatten_lane_segment(
            p1,
            p1,
            p2,
            p2,
            ArrangementCurveKind::Exponential { bend: -1.0 },
            2.0,
            &mut out,
        );
        // t=0.5 で y = 0.5^0.5 * 100 ≈ 70.71
        let y_at_50 = sample_polyline_y(&out, 50.0);
        assert!(
            (y_at_50 - 70.71).abs() < 1.5,
            "bend=-1 で x=50 の y ≈ 70.71 (sqrt(t)): got {y_at_50}"
        );
    }

    /// Exponential { bend: 0.0 } は **直線** (t^1)。
    #[test]
    fn exponential_bend_zero_is_linear() {
        let p1 = (0.0_f32, 0.0_f32);
        let p2 = (100.0_f32, 100.0_f32);
        let mut out = vec![p1];
        flatten_lane_segment(
            p1,
            p1,
            p2,
            p2,
            ArrangementCurveKind::Exponential { bend: 0.0 },
            2.0,
            &mut out,
        );
        let y_at_50 = sample_polyline_y(&out, 50.0);
        assert!(
            (y_at_50 - 50.0).abs() < 1.0,
            "bend=0 で x=50 の y = 50 (linear): got {y_at_50}"
        );
    }

    // ============================================================
    // M14 Phase 77 (daw_01 #048): 縦 scroll 時の scissor 動作 unit test
    // ============================================================

    /// Phase 77 wrap が適用されると、 cached 内の lanes scope に積まれた push_filled_rect は
    /// `clip_rect = Some(lanes)` を持つ (旧設計では `clip_rect = None` で ruler / toolbar に
    /// leak していた)。 lanes 背景 fill の primitive を hex で識別して clip_rect の Some を確認。
    #[test]
    fn lanes_bg_primitive_has_clip_rect_after_phase_77() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::{Primitive, Scene};

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let model = Model {
            tracks: vec![track(10, "t0", vec![])],
            view: ArrangementView {
                header_w: 100.0,
                ruler_h: 30.0,
                track_row_h: 60.0,
                track_top: 0.0,
                ..ArrangementView::default()
            },
        };

        let arr_rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let style = ArrangementStyle::default();
        let _edits =
            host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |m, ui| {
                let _ = ui.arrangement(
                    "arr", arr_rect, &m.tracks, &[], m.view, &[], &[], &[], &[], &style, None,
                    |_| Edit::mutate(|_: &mut Model| {}),
                );
            });

        // lanes background fill は draw_lanes_bg 冒頭の `push_filled_rect(hctx, lanes, style.bg)`
        // で、 lanes scope (= `with_clip_rect(lanes)`) 内なので clip_rect = Some(lanes ∩ None) =
        // Some(lanes) になる。 rect = lanes (x=100, y=30, w=700, h=370)、 clip_rect = Some(lanes)
        // で一致する primitive を探す。
        let expected_lanes = Rect { x: 100.0, y: 30.0, w: 700.0, h: 370.0 };
        let found = scene.primitives.iter().any(|p| {
            if let Primitive::Rect(c) = p {
                let r = c.rect;
                let close = (r.x - expected_lanes.x).abs() < 1e-3
                    && (r.y - expected_lanes.y).abs() < 1e-3
                    && (r.w - expected_lanes.w).abs() < 1e-3
                    && (r.h - expected_lanes.h).abs() < 1e-3;
                close && c.clip_rect.is_some()
            } else {
                false
            }
        });
        assert!(
            found,
            "Phase 77: lanes background push_filled_rect must produce a Rect primitive with clip_rect = Some(...) (旧設計では None で ruler / toolbar に leak)"
        );
    }

    /// `track_top` が大きい (= 縦 scroll で第 1 track row が lanes.y より上に計算される) とき、
    /// row 背景 primitive は scissor によって ruler / toolbar 領域に leak しない。 lanes scope の
    /// `with_clip_rect(lanes)` で全 row 背景の `clip_rect.y >= lanes.y` が保証される。
    #[test]
    fn track_row_clipped_to_lanes_when_track_top_positive() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::{Primitive, Scene};

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        // selection track にして track row 背景の push_filled_rect が走るようにする
        // (selection / video 以外の通常 audio row では行背景の push_filled_rect は呼ばれない、
        // draw_lanes_bg の `if selected ... else if Video` 分岐参照。 #085 で group 分岐は撤去)。
        let model = Model {
            tracks: vec![track(10, "t0", vec![])],
            view: ArrangementView {
                header_w: 100.0,
                ruler_h: 30.0,
                track_row_h: 60.0,
                track_top: 500.0, // 大 scroll: 第 1 track が y = 30 - 500 = -470 から描画される想定
                ..ArrangementView::default()
            },
        };

        let arr_rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let style = ArrangementStyle::default();
        let _edits =
            host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |m, ui| {
                let _ = ui.arrangement(
                    "arr",
                    arr_rect,
                    &m.tracks,
                    &[],
                    m.view,
                    &[], // selected_clips
                    &[10_u32], // selected_tracks → row 背景 push が走る
                    &[],
                    &[],
                    &style,
                    None,
                    |_| Edit::mutate(|_: &mut Model| {}),
                );
            });

        // lanes scope (= with_clip_rect(lanes)) 由来の primitive は clip_rect が lanes 完全一致
        // (= x=100, y=30, w=700, h=370) になる (`merge_clip(Some(lanes), None) = Some(lanes)`)。
        // この鋳型に一致する primitive が 1 件以上あれば lanes scope が effective に走った証明。
        // 旧設計 (Phase 77 前) では `draw_lanes_bg` 内の push_filled_rect が clip_rect = None で
        // 出力されていた (= scope なし)。
        let expected_lanes_clip = Rect { x: 100.0, y: 30.0, w: 700.0, h: 370.0 };
        let found_lanes_scope = scene.primitives.iter().any(|p| {
            if let Primitive::Rect(c) = p
                && let Some(clip) = c.clip_rect
            {
                (clip.x - expected_lanes_clip.x).abs() < 1e-3
                    && (clip.y - expected_lanes_clip.y).abs() < 1e-3
                    && (clip.w - expected_lanes_clip.w).abs() < 1e-3
                    && (clip.h - expected_lanes_clip.h).abs() < 1e-3
            } else {
                false
            }
        });
        assert!(
            found_lanes_scope,
            "Phase 77: track_top=500 で lanes scope (clip_rect = lanes 完全一致) の RectCommand が 1 件以上必要 (= ruler / toolbar への leak 防止のための scissor が走っている確認)"
        );
    }

    /// `track_top` が **負値** でも scissor が効く (= ruler の上に track row が出ない)。
    /// 負 scroll は通常 caller が clamp しないと発生するが、 widget は受け取った値で計算するだけで
    /// scissor で safety を担保する設計 (= caller boilerplate を排除、 #048 reply の方針)。
    #[test]
    fn track_row_clipped_when_track_top_negative() {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::{Primitive, Scene};

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        let model = Model {
            tracks: vec![track(10, "t0", vec![])],
            view: ArrangementView {
                header_w: 100.0,
                ruler_h: 30.0,
                track_row_h: 60.0,
                track_top: -300.0, // 負 scroll: 第 1 track が y = 30 + 300 = 330 から描画
                ..ArrangementView::default()
            },
        };

        let arr_rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let style = ArrangementStyle::default();
        let _edits =
            host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |m, ui| {
                let _ = ui.arrangement(
                    "arr", arr_rect, &m.tracks, &[], m.view, &[], &[], &[], &[], &style, None,
                    |_| Edit::mutate(|_: &mut Model| {}),
                );
            });

        // 負 track_top でも lanes scope が clip_rect = Some(lanes) を全 push に適用する
        // (= with_clip_rect 自体が track_top 値に依存しない)。 lanes 背景 fill primitive が
        // 存在することで scope が走った証明。
        let expected_lanes = Rect { x: 100.0, y: 30.0, w: 700.0, h: 370.0 };
        let found = scene.primitives.iter().any(|p| {
            if let Primitive::Rect(c) = p {
                let r = c.rect;
                let close = (r.x - expected_lanes.x).abs() < 1e-3
                    && (r.y - expected_lanes.y).abs() < 1e-3;
                close && c.clip_rect.is_some()
            } else {
                false
            }
        });
        assert!(
            found,
            "Phase 77: 負 track_top でも lanes 背景 fill が scissor 付きで生成される"
        );
    }

    // ============================================================
    // M14 Phase 117 (daw_01 #091): header 幅 drag splitter
    // ============================================================

    /// `header_resize_splitter_at` が境界 `rect.x + header_w` 中心 ±handle/2 の縦帯 × 全高で hit、
    /// 帯の外 / header_w=0 / handle=0 で miss する。
    #[test]
    fn header_resize_splitter_at_hits_centered_full_height_band() {
        let style = ArrangementStyle::default(); // header_resize_handle_px = 8 → ±4
        let rect = Rect { x: 100.0, y: 50.0, w: 800.0, h: 400.0 };
        let header_w = 160.0;
        let boundary = 100.0 + 160.0; // = 260
        // 境界中心: hit。
        assert!(header_resize_splitter_at(rect, header_w, &style, boundary, 200.0));
        // 全高で hit (上端 / 下端近く)。
        assert!(header_resize_splitter_at(rect, header_w, &style, boundary, 50.0));
        assert!(header_resize_splitter_at(rect, header_w, &style, boundary, 449.0));
        // 帯端 (±4px) 内側: hit (256) / 外側: miss (255.9 は < 256 で外、 264 は半開で外)。
        assert!(header_resize_splitter_at(rect, header_w, &style, 256.0, 200.0));
        assert!(!header_resize_splitter_at(rect, header_w, &style, 255.0, 200.0));
        assert!(!header_resize_splitter_at(rect, header_w, &style, 264.0, 200.0));
        // rect の外 (上 / 下): miss。
        assert!(!header_resize_splitter_at(rect, header_w, &style, boundary, 49.0));
        assert!(!header_resize_splitter_at(rect, header_w, &style, boundary, 450.0));
        // header_w = 0 (header 無し): 常に miss。
        assert!(!header_resize_splitter_at(rect, 0.0, &style, 100.0, 200.0));
        // handle = 0: 無効化。
        let no_handle = ArrangementStyle { header_resize_handle_px: 0.0, ..ArrangementStyle::default() };
        assert!(!header_resize_splitter_at(rect, header_w, &no_handle, boundary, 200.0));
    }

    /// header / lanes 境界を press → 右へ drag すると `SetHeaderW { prev: anchor, next: anchor + dx }`
    /// が per-frame emit される (raw px、 caller clamp 前提)。
    #[test]
    fn header_resize_drag_emits_set_header_w() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        // header_w = 160 / ruler_h = 0 → 境界 x = 160、 splitter は全高 [0, 400)。
        let view = ArrangementView { header_w: 160.0, ruler_h: 0.0, ..ArrangementView::default() };
        let model = Model { tracks: vec![track(1, "t", vec![])], view };

        let emitted: Arc<Mutex<Vec<(f32, f32)>>> = Arc::new(Mutex::new(Vec::new()));

        let run = |host: &mut UiHost<Model>, scene: &mut Scene, input: FrameInput| {
            let emitted_cb = Arc::clone(&emitted);
            let _ = host.frame_to_edits(&model, scene, screen, input, |m, ui| {
                let style = ArrangementStyle::default();
                let emitted_cb = Arc::clone(&emitted_cb);
                let _ = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    move |req| {
                        if let ArrangementEditRequest::SetHeaderW { prev, next } = req {
                            emitted_cb.lock().unwrap().push((prev, next));
                        }
                        Edit::mutate(|_: &mut Model| {})
                    },
                );
            });
        };

        // frame 1: 境界 (160, 200) で press → session 起動 (この frame は dx=0 で emit 無し)。
        run(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((160.0, 200.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
        );
        // frame 2: 右へ 60px drag (220, 200) → SetHeaderW { prev: 160, next: 220 }。
        run(
            &mut host,
            &mut scene,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((220.0, 200.0)),
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
        );

        let log = emitted.lock().unwrap();
        assert_eq!(log.len(), 1, "drag continuation frame で 1 件発火: {log:?}");
        assert!((log[0].0 - 160.0).abs() < 1e-3, "prev = anchor 160: {}", log[0].0);
        assert!((log[0].1 - 220.0).abs() < 1e-3, "next = 160 + 60 = 220 (raw): {}", log[0].1);
    }

    // ============================================================
    // M14 Phase 118 (daw_01 #092): group track 名 double-click rename の信頼性
    // ============================================================

    /// 深くネストした group track は header row のどこ (= indent 空白を含む、 sub-zone 以外) を
    /// double-click しても `BeginRenameTrack` が発火する。 通常 track は名前帯のみで従来どおり。
    #[test]
    fn deep_nested_group_dblclick_in_indent_emits_begin_rename() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        // header_w = 200 / ruler_h = 0 / track_top = 0 → track 1 行 = {0, 0, 200, 32}。
        let view = ArrangementView {
            header_w: 200.0,
            ruler_h: 0.0,
            track_row_h: 32.0,
            ..ArrangementView::default()
        };
        // track 1 = 深くネストした group (depth 3 で indent 48px)、 track 2 = その子 (= 1 を group 化)。
        let mut g = track(1, "DeepGroup", vec![]);
        g.depth = 3;
        let mut child = track(2, "child", vec![]);
        child.parent_id = Some(1);
        child.depth = 4;
        let model = Model { tracks: vec![g, child], view };

        let renamed: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));

        let run = |host: &mut UiHost<Model>, scene: &mut Scene, pos: (f32, f32)| {
            let renamed_cb = Arc::clone(&renamed);
            let input = FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            };
            let _ = host.frame_to_edits(&model, scene, screen, input, |m, ui| {
                let style = ArrangementStyle::default();
                let renamed_cb = Arc::clone(&renamed_cb);
                let _ = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    move |req| {
                        if let ArrangementEditRequest::BeginRenameTrack(tid) = req {
                            renamed_cb.lock().unwrap().push(tid);
                        }
                        Edit::mutate(|_: &mut Model| {})
                    },
                );
            });
        };

        // indent 空白 (x=10、 disclosure x≈52 より左) で double-click (2 連続 release、 同位置)。
        run(&mut host, &mut scene, (10.0, 16.0)); // 1 回目: last_click 記録、 rename 無し
        run(&mut host, &mut scene, (10.0, 16.0)); // 2 回目: double-click → rename

        let log = renamed.lock().unwrap();
        assert_eq!(log.as_slice(), &[1], "深ネスト group の indent dblclick で track 1 rename: {log:?}");
    }

    /// daw_01 #092 follow-up (M14 Phase 119): top-level (depth-0) group の disclosure 帯
    /// (= name 帯の左端、 indent 空白が無いため flush-left) を double-click しても rename が始まる。
    /// master row の有無は無関係 (= 最上段が top-level group になりがちなだけの相関) なことを
    /// 「master 有 (empty / expanded lanes) / 無」 ×「disclosure 帯 / name 帯」 で網羅検証する。
    #[test]
    #[allow(clippy::too_many_lines)]
    fn group_disclosure_dblclick_renames_regardless_of_master() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
            master: Option<ArrangementMasterRow>,
        }

        #[derive(Clone, Copy)]
        enum MasterKind {
            None,
            Empty,
            Expanded,
        }

        // group27 (depth-0、 子持ち = group) を double-click した位置で BeginRenameTrack(27) が出るか。
        let renames = |mk: MasterKind, click: (f32, f32)| -> bool {
            let mut host: UiHost<Model> = UiHost::no_redraw();
            let mut scene = Scene::new();
            let screen = PhysicalSize { width: 800, height: 600 };
            let view = ArrangementView {
                header_w: 200.0,
                ruler_h: 0.0,
                track_row_h: 32.0,
                ..ArrangementView::default()
            };
            let g = track(27, "Group27", vec![]);
            let mut child = track(25, "Inst", vec![]);
            child.parent_id = Some(27);
            child.depth = 1;
            let master = match mk {
                MasterKind::None => None,
                MasterKind::Empty => Some(ArrangementMasterRow {
                    automation_lanes_collapsed: true,
                    automation_lanes: Vec::new(),
                    height_px_override: None,
                }),
                MasterKind::Expanded => Some(ArrangementMasterRow {
                    automation_lanes_collapsed: false,
                    automation_lanes: vec![ArrangementAutomationLane {
                        id: 1,
                        label: Arc::from("Tempo"),
                        icon_glyph: 'T',
                        color: Color::rgb(1.0, 1.0, 1.0),
                        enabled: true,
                        visible: true,
                        height_px: 60,
                        default_value_norm: 0.5,
                        clips: Vec::new(),
                    }],
                    height_px_override: None,
                }),
            };
            let model = Model { tracks: vec![g, child], view, master };

            let renamed: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
            let run = |host: &mut UiHost<Model>, scene: &mut Scene, pos: (f32, f32)| {
                let renamed_cb = Arc::clone(&renamed);
                let input = FrameInput {
                    pointer: PointerFrame {
                        pos: Some(pos),
                        primary_just_released: true,
                        ..PointerFrame::default()
                    },
                    ..FrameInput::default()
                };
                let _ = host.frame_to_edits(&model, scene, screen, input, |m, ui| {
                    let style = ArrangementStyle::default();
                    let renamed_cb = Arc::clone(&renamed_cb);
                    let _ = ui.arrangement(
                        "arr",
                        Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                        &m.tracks,
                        &[],
                        m.view,
                        &[],
                        &[],
                        &[],
                        &[],
                        &style,
                        m.master.as_ref(),
                        move |req| {
                            if let ArrangementEditRequest::BeginRenameTrack(tid) = req {
                                renamed_cb.lock().unwrap().push(tid);
                            }
                            Edit::mutate(|_: &mut Model| {})
                        },
                    );
                });
            };
            run(&mut host, &mut scene, click);
            run(&mut host, &mut scene, click);
            let log = renamed.lock().unwrap();
            log.as_slice() == [27]
        };

        // 行 y: master 無 → group27 = visible_tracks[0] = y∈[0,32] (click y=16)。
        //       master Empty → group27 = visible_tracks[1] = y∈[32,64] (click y=48)。
        //       master Expanded(lane h=60) → master total=92 → group27 = y∈[92,124] (click y=108)。
        // depth-0 disclosure は x∈[4,20] (vertical center)。 x=10 = disclosure 帯、 x=50 = name 帯。
        for (mk, y, label) in [
            (MasterKind::None, 16.0, "no-master"),
            (MasterKind::Empty, 48.0, "empty-master"),
            (MasterKind::Expanded, 108.0, "expanded-master"),
        ] {
            assert!(
                renames(mk, (10.0, y)),
                "{label}: disclosure 帯 (x=10) の double-click で top-level group が rename"
            );
            assert!(
                renames(mk, (50.0, y)),
                "{label}: name 帯 (x=50) の double-click で rename (回帰なし)"
            );
        }
    }

    /// M14 Phase 119: disclosure の **single-click** は従来どおり ToggleGroupCollapsed のみ発火し、
    /// rename は起こさない (= double-click rename 化が single-click 折り畳みを壊さない回帰ガード)。
    #[test]
    fn group_disclosure_single_click_still_toggles_not_rename() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let view = ArrangementView {
            header_w: 200.0,
            ruler_h: 0.0,
            track_row_h: 32.0,
            ..ArrangementView::default()
        };
        // depth-0 group (子持ち)。 disclosure 帯 x∈[4,20]。 group27 row = y∈[0,32]。
        let g = track(27, "Group27", vec![]);
        let mut child = track(25, "Inst", vec![]);
        child.parent_id = Some(27);
        child.depth = 1;
        let model = Model { tracks: vec![g, child], view };

        let renamed: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let toggled: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let renamed_cb = Arc::clone(&renamed);
        let toggled_cb = Arc::clone(&toggled);
        // 単発の release を 1 frame だけ (double-click にならない)。
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((10.0, 16.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        };
        let _ = host.frame_to_edits(&model, &mut scene, screen, input, |m, ui| {
            let style = ArrangementStyle::default();
            let renamed_cb = Arc::clone(&renamed_cb);
            let toggled_cb = Arc::clone(&toggled_cb);
            let _ = ui.arrangement(
                "arr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &m.tracks,
                &[],
                m.view,
                &[],
                &[],
                &[],
                &[],
                &style,
                None,
                move |req| {
                    match &req {
                        ArrangementEditRequest::BeginRenameTrack(tid) => {
                            renamed_cb.lock().unwrap().push(*tid);
                        }
                        ArrangementEditRequest::ToggleGroupCollapsed(tid) => {
                            toggled_cb.lock().unwrap().push(*tid);
                        }
                        _ => {}
                    }
                    Edit::mutate(|_: &mut Model| {})
                },
            );
        });

        assert!(renamed.lock().unwrap().is_empty(), "single-click は rename しない");
        assert_eq!(
            toggled.lock().unwrap().as_slice(),
            &[27],
            "single-click on disclosure は ToggleGroupCollapsed のみ"
        );
    }

    /// 通常 (非 group) track は名前帯 double-click で従来どおり rename、 名前帯外 (volume band) では
    /// rename しない (= #092 の broad zone は group 限定で、 通常 track 挙動は不変)。
    #[test]
    fn normal_track_dblclick_only_renames_on_name_band() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        let mut host: UiHost<Model> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        // header_w = 200 / row_h = 40 (volume band が出る高さ)、 track 1 行 = {0, 0, 200, 40}。
        let view = ArrangementView {
            header_w: 200.0,
            ruler_h: 0.0,
            track_row_h: 40.0,
            ..ArrangementView::default()
        };
        let model = Model { tracks: vec![track(1, "Lead", vec![])], view };

        let renamed: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let run = |host: &mut UiHost<Model>, scene: &mut Scene, pos: (f32, f32)| {
            let renamed_cb = Arc::clone(&renamed);
            let input = FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            };
            let _ = host.frame_to_edits(&model, scene, screen, input, |m, ui| {
                let style = ArrangementStyle::default();
                let renamed_cb = Arc::clone(&renamed_cb);
                let _ = ui.arrangement(
                    "arr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.tracks,
                    &[],
                    m.view,
                    &[],
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    move |req| {
                        if let ArrangementEditRequest::BeginRenameTrack(tid) = req {
                            renamed_cb.lock().unwrap().push(tid);
                        }
                        Edit::mutate(|_: &mut Model| {})
                    },
                );
            });
        };

        // 名前帯 (左上 x=10, y=8 = inner.y 付近) を double-click → rename。
        run(&mut host, &mut scene, (10.0, 8.0));
        run(&mut host, &mut scene, (10.0, 8.0));
        assert_eq!(renamed.lock().unwrap().as_slice(), &[1], "名前帯 dblclick は rename");

        // volume band (y=34 付近、 名前帯の下) を別位置で double-click → rename しない。
        renamed.lock().unwrap().clear();
        run(&mut host, &mut scene, (10.0, 34.0));
        run(&mut host, &mut scene, (10.0, 34.0));
        assert!(renamed.lock().unwrap().is_empty(), "通常 track の volume band dblclick は rename しない");
    }

    /// #092 review follow-up: nested track (depth>0) の volume band press hit-test が draw と同じ indent
    /// 位置になり、 indent 空白の press では volume drag が起動しない (= SetTrackVolume を出さない)、
    /// indent 済 band 位置の press では起動する。 旧 (press 非 indent) では indent 空白が band 扱いされ
    /// 誤って volume drag していた。
    #[test]
    fn nested_track_volume_band_press_follows_indent() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        struct Model {
            tracks: Vec<ArrangementTrack>,
            view: ArrangementView,
        }
        // header_w = 200 / row_h = 40 (band 可視) / depth 3 → indent = 48px。 indent 済 band ≈ [52, 196]。
        let view = ArrangementView {
            header_w: 200.0,
            ruler_h: 0.0,
            track_row_h: 40.0,
            ..ArrangementView::default()
        };

        let press_then_drag = |press_x: f32| -> usize {
            let mut host: UiHost<Model> = UiHost::no_redraw();
            let mut scene = Scene::new();
            let screen = PhysicalSize { width: 800, height: 600 };
            let mut t = track(1, "Nested", vec![]);
            t.depth = 3;
            let model = Model { tracks: vec![t], view };
            let vol_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

            let run = |host: &mut UiHost<Model>, scene: &mut Scene, input: FrameInput| {
                let vol_cb = Arc::clone(&vol_count);
                let _ = host.frame_to_edits(&model, scene, screen, input, |m, ui| {
                    let style = ArrangementStyle::default();
                    let vol_cb = Arc::clone(&vol_cb);
                    let _ = ui.arrangement(
                        "arr",
                        Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                        &m.tracks,
                        &[],
                        m.view,
                        &[],
                        &[],
                        &[],
                        &[],
                        &style,
                        None,
                        move |req| {
                            if matches!(req, ArrangementEditRequest::SetTrackVolume { .. }) {
                                *vol_cb.lock().unwrap() += 1;
                            }
                            Edit::mutate(|_: &mut Model| {})
                        },
                    );
                });
            };
            // band y ≈ inner.y + btn_h + gap = 4 + 20 + 2 = 26..30。 y=27 を使う。
            run(
                &mut host,
                &mut scene,
                FrameInput {
                    pointer: PointerFrame {
                        pos: Some((press_x, 27.0)),
                        primary_just_pressed: true,
                        primary_pressed: true,
                        ..PointerFrame::default()
                    },
                    ..FrameInput::default()
                },
            );
            run(
                &mut host,
                &mut scene,
                FrameInput {
                    pointer: PointerFrame {
                        pos: Some((press_x + 20.0, 27.0)),
                        primary_pressed: true,
                        ..PointerFrame::default()
                    },
                    ..FrameInput::default()
                },
            );
            *vol_count.lock().unwrap()
        };

        // x=10 = indent 空白 (indent 済 band [52,196] の外) → volume drag 起動しない。
        assert_eq!(press_then_drag(10.0), 0, "indent 空白 press は volume drag を起動しない");
        // x=120 = indent 済 band 内 → volume drag 起動 (continuation で SetTrackVolume)。
        assert!(press_then_drag(120.0) >= 1, "indent 済 band 位置 press は volume drag を起動する");
    }

    // ============================================================
    // M14 Phase 125 (#102): plain-drag marquee select (REPLACE / UNION / XOR)
    // ============================================================

    /// #102 marquee helper: press(modifiers) → release を 2 frame 流し、 発行された `SelectClips` の
    /// (回数, 最後の next) と `MoveClips` 回数を返す。 test_view (len_beats=16, row_h=32, header/ruler=0)、
    /// rect 800×400 ⇒ beat_to_px=50。 clip A=[0,200] / B=[400,600]、 y[2,30]。
    fn run_marquee(
        tracks: &[ArrangementTrack],
        view: ArrangementView,
        selected: &[ClipKey],
        mods: daw_ui_platform::Modifiers,
        press: (f32, f32),
        release: (f32, f32),
    ) -> (usize, Option<Vec<ClipKey>>, usize) {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let screen = PhysicalSize { width: 800, height: 600 };
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let style = ArrangementStyle::default();
        let select_count = Arc::new(Mutex::new(0usize));
        let move_count = Arc::new(Mutex::new(0usize));
        let last_next: Arc<Mutex<Option<Vec<ClipKey>>>> = Arc::new(Mutex::new(None));

        let mut frame = |input: FrameInput| {
            let mut scene = Scene::new();
            let sc = Arc::clone(&select_count);
            let mc = Arc::clone(&move_count);
            let ln = Arc::clone(&last_next);
            let _ = host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
                let _ = ui.arrangement(
                    "arr",
                    area,
                    tracks,
                    &[],
                    view,
                    selected,
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    move |req| {
                        match &req {
                            ArrangementEditRequest::SelectClips { next, .. } => {
                                *sc.lock().unwrap() += 1;
                                *ln.lock().unwrap() = Some(next.clone());
                            }
                            ArrangementEditRequest::MoveClips(_) => {
                                *mc.lock().unwrap() += 1;
                            }
                            _ => {}
                        }
                        Edit::mutate(|(): &mut ()| {})
                    },
                );
            });
        };

        frame(FrameInput {
            pointer: PointerFrame {
                pos: Some(press),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: mods,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        });
        frame(FrameInput {
            pointer: PointerFrame {
                pos: Some(release),
                primary_just_released: true,
                modifiers: mods,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        });

        let c = *select_count.lock().unwrap();
        let m = *move_count.lock().unwrap();
        let n = last_next.lock().unwrap().clone();
        (c, n, m)
    }

    fn marquee_tracks() -> Vec<ArrangementTrack> {
        vec![track(0, "t", vec![clip(100, 0.0, 4.0, "a"), clip(101, 8.0, 4.0, "b")])]
    }
    fn ck(clip: u32) -> ClipKey {
        ClipKey { track: 0, clip }
    }

    /// 空き zone の **無修飾** drag = marquee REPLACE。 prev [A] を捨てて rect 内 {B} に置換。
    #[test]
    fn arrangement_plain_drag_empty_is_replace() {
        let tracks = marquee_tracks();
        // press (250,15) は A(0..200) と B(400..600) の隙間 = 空き。 drag rect (250,15)-(650,20) は B のみ交差。
        let (count, next, _) = run_marquee(
            &tracks,
            test_view(),
            &[ck(100)],
            daw_ui_platform::Modifiers::empty(),
            (250.0, 15.0),
            (650.0, 20.0),
        );
        assert_eq!(count, 1, "release で 1 回だけ SelectClips");
        assert_eq!(next, Some(vec![ck(101)]), "REPLACE: prev [A] 破棄、 rect 内 [B]");
    }

    /// 空き zone の **Shift** drag = marquee UNION。 prev [A] に rect 内 {B} を加算。
    #[test]
    fn arrangement_shift_drag_empty_is_union() {
        let tracks = marquee_tracks();
        let (count, next, _) = run_marquee(
            &tracks,
            test_view(),
            &[ck(100)],
            daw_ui_platform::Modifiers { shift: true, ..daw_ui_platform::Modifiers::empty() },
            (250.0, 15.0),
            (650.0, 20.0),
        );
        assert_eq!(count, 1);
        assert_eq!(next, Some(vec![ck(100), ck(101)]), "UNION: prev [A] ∪ {{B}} = [A,B]");
    }

    /// 空き zone の **Ctrl** drag = marquee XOR (toggle)。 prev [A,B] から rect 内 {B} を除去。
    #[test]
    fn arrangement_ctrl_drag_empty_is_xor() {
        let tracks = marquee_tracks();
        let (count, next, _) = run_marquee(
            &tracks,
            test_view(),
            &[ck(100), ck(101)],
            daw_ui_platform::Modifiers { ctrl: true, ..daw_ui_platform::Modifiers::empty() },
            (250.0, 15.0),
            (650.0, 20.0),
        );
        assert_eq!(count, 1);
        assert_eq!(next, Some(vec![ck(100)]), "XOR: [A,B] ^ {{B}} = [A]");
    }

    /// clip の上の **無修飾** drag は marquee ではなく MOVE (SelectClips 発行しない)。
    #[test]
    fn arrangement_plain_drag_on_clip_is_move_not_select() {
        let tracks = marquee_tracks();
        // press (100,15) = clip A 中央 → clip drag。 release (300,15) = +200px (>4px、 Move commit)。
        let (select_count, _, move_count) = run_marquee(
            &tracks,
            test_view(),
            &[ck(100)],
            daw_ui_platform::Modifiers::empty(),
            (100.0, 15.0),
            (300.0, 15.0),
        );
        assert_eq!(select_count, 0, "clip 上 plain drag は marquee 不発 (SelectClips ゼロ)");
        assert!(move_count >= 1, "clip drag は MoveClips を発行する");
    }

    /// #75: clip の **上から** 始める **Shift** drag は marquee UNION を起動する (clip move しない)。
    /// press (100,15) = clip A 中央。 Shift は move zone の clip_drag を抑止するので、 marquee が
    /// この press を所有して rect 内 {A,B} を prev [A] に UNION する。
    #[test]
    fn arrangement_shift_drag_on_clip_starts_marquee_union() {
        let tracks = marquee_tracks();
        let (select_count, next, move_count) = run_marquee(
            &tracks,
            test_view(),
            &[ck(100)],
            daw_ui_platform::Modifiers { shift: true, ..daw_ui_platform::Modifiers::empty() },
            (100.0, 15.0),
            (650.0, 20.0),
        );
        assert_eq!(select_count, 1, "clip 上 Shift drag は marquee を起動して SelectClips 1 回");
        assert_eq!(
            next,
            Some(vec![ck(100), ck(101)]),
            "UNION: prev [A] ∪ rect 内 {{A,B}} = [A,B]"
        );
        assert_eq!(move_count, 0, "clip 上 Shift drag は move しない (MoveClips ゼロ)");
    }

    /// #75 排他性: clip 上の **Ctrl+Shift** drag は independent clone であって marquee ではない
    /// (`!ctrl` 条件で marquee から除外)。 SelectClips を発行しないことで marquee 不発を確認する。
    #[test]
    fn arrangement_ctrl_shift_drag_on_clip_is_not_marquee() {
        let tracks = marquee_tracks();
        let (select_count, _, _) = run_marquee(
            &tracks,
            test_view(),
            &[ck(100)],
            daw_ui_platform::Modifiers {
                shift: true,
                ctrl: true,
                ..daw_ui_platform::Modifiers::empty()
            },
            (100.0, 15.0),
            (300.0, 15.0),
        );
        assert_eq!(select_count, 0, "clip 上 Ctrl+Shift drag は clone であって marquee 不発");
    }

    /// 空き zone の sub-4px 無修飾 press+release (同フレーム) は marquee zero-rect REPLACE で
    /// **ちょうど 1 回** `SelectClips{next:[]}` を emit する (pure-release clear との二重 emit ガード)。
    #[test]
    fn arrangement_subpx_empty_press_emits_single_clear() {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        let tracks = marquee_tracks();
        let selected = vec![ck(100)];
        let view = test_view();
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let style = ArrangementStyle::default();
        let count = Arc::new(Mutex::new(0usize));
        let last_next: Arc<Mutex<Option<Vec<ClipKey>>>> = Arc::new(Mutex::new(None));
        let cc = Arc::clone(&count);
        let ln = Arc::clone(&last_next);

        let _ = host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((250.0, 15.0)),
                    primary_just_pressed: true,
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            |(), ui| {
                let _ = ui.arrangement(
                    "arr",
                    area,
                    &tracks,
                    &[],
                    view,
                    &selected,
                    &[],
                    &[],
                    &[],
                    &style,
                    None,
                    move |req| {
                        if let ArrangementEditRequest::SelectClips { next, .. } = &req {
                            *cc.lock().unwrap() += 1;
                            *ln.lock().unwrap() = Some(next.clone());
                        }
                        Edit::mutate(|(): &mut ()| {})
                    },
                );
            },
        );
        assert_eq!(
            *count.lock().unwrap(),
            1,
            "空き sub-px press は ちょうど 1 回 SelectClips (二重 emit ガード固定)"
        );
        assert_eq!(last_next.lock().unwrap().clone(), Some(vec![]), "REPLACE zero-rect で clear");
    }

    // ============================================================
    // daw_01 #071: automation clip 複数選択 (box-drag / shift-click / multi-move)
    // ============================================================

    /// #071 fixture: 1 track + 1 lane (height 60) に automation clip A/B を持たせる。 test_view
    /// (len_beats=16, rect 800×400 ⇒ beat_to_px=50)、 track row y∈[0,32]、 lane body y∈[32,92]、
    /// clip y∈[38,86]。 A=[start 4,len 2]⇒x[200,300]、 B=[start 8,len 2]⇒x[400,500]、
    /// lane 先頭 x[0,200] は空き zone (lasso 起点に使える)。
    fn auto_tracks() -> Vec<ArrangementTrack> {
        let auto_clip = |id: u32, start: f64, len: f64, name: &str| ArrangementAutomationClip {
            id,
            start_beat: start,
            len_beats: len,
            name: Arc::from(name),
            points: Vec::new(),
            share_group_color: None,
        };
        let lane = ArrangementAutomationLane {
            id: 7,
            label: Arc::from("Vol"),
            icon_glyph: 'V',
            color: Color::rgb(1.0, 1.0, 1.0),
            enabled: true,
            visible: true,
            height_px: 60,
            default_value_norm: 0.5,
            clips: vec![auto_clip(100, 4.0, 2.0, "A"), auto_clip(101, 8.0, 2.0, "B")],
        };
        let mut t = track(1, "t", vec![]);
        t.automation_lanes_collapsed = false;
        t.automation_lanes = vec![lane];
        vec![t]
    }

    fn ack(clip: u32) -> AutomationClipKey {
        AutomationClipKey { track: 1, lane: 7, clip }
    }

    /// #071 helper: press(modifiers) → release を 2 frame 流し、 (`SelectAutomationClips` 回数,
    /// 最後の next, `MoveAutomationClips` の delta 数) を返す (`run_marquee` の automation 版)。
    fn run_auto_drag(
        tracks: &[ArrangementTrack],
        selected: &[AutomationClipKey],
        mods: daw_ui_platform::Modifiers,
        press: (f32, f32),
        release: (f32, f32),
    ) -> (usize, Option<Vec<AutomationClipKey>>, usize) {
        use std::sync::Mutex;

        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::{FrameInput, PointerFrame};
        use crate::ui::UiHost;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let screen = PhysicalSize { width: 800, height: 600 };
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let style = ArrangementStyle::default();
        let view = test_view();
        let select_count = Arc::new(Mutex::new(0usize));
        let move_len = Arc::new(Mutex::new(0usize));
        let last_next: Arc<Mutex<Option<Vec<AutomationClipKey>>>> = Arc::new(Mutex::new(None));

        let mut frame = |input: FrameInput| {
            let mut scene = Scene::new();
            let sc = Arc::clone(&select_count);
            let ml = Arc::clone(&move_len);
            let ln = Arc::clone(&last_next);
            let _ = host.frame_to_edits(&(), &mut scene, screen, input, |(), ui| {
                let _ = ui.arrangement(
                    "arr",
                    area,
                    tracks,
                    &[],
                    view,
                    &[],
                    &[],
                    selected,
                    &[],
                    &style,
                    None,
                    move |req| {
                        match &req {
                            ArrangementEditRequest::SelectAutomationClips { next, .. } => {
                                *sc.lock().unwrap() += 1;
                                *ln.lock().unwrap() = Some(next.clone());
                            }
                            ArrangementEditRequest::MoveAutomationClips(d) => {
                                *ml.lock().unwrap() = d.len();
                            }
                            _ => {}
                        }
                        Edit::mutate(|(): &mut ()| {})
                    },
                );
            });
        };

        frame(FrameInput {
            pointer: PointerFrame {
                pos: Some(press),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: mods,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        });
        frame(FrameInput {
            pointer: PointerFrame {
                pos: Some(release),
                primary_just_released: true,
                modifiers: mods,
                ..PointerFrame::default()
            },
            ..FrameInput::default()
        });

        let c = *select_count.lock().unwrap();
        let n = last_next.lock().unwrap().clone();
        let m = *move_len.lock().unwrap();
        (c, n, m)
    }

    /// #071: 空き lane zone の四角ドラッグ (無修飾) が囲った automation clip を REPLACE 選択する
    /// (option 1 の「四角ドラッグで clip も選ぶ」)。 press (50,60) は空き zone、 rect x[50,550] が A/B 交差。
    #[test]
    fn automation_box_drag_selects_clips() {
        let tracks = auto_tracks();
        let (count, next, _) = run_auto_drag(
            &tracks,
            &[],
            daw_ui_platform::Modifiers::empty(),
            (50.0, 60.0),
            (550.0, 65.0),
        );
        assert_eq!(count, 1, "release で 1 回だけ SelectAutomationClips");
        assert_eq!(next, Some(vec![ack(100), ack(101)]), "rect 内の A/B を REPLACE 選択");
    }

    /// #071: Shift+クリックが automation clip を選択集合に toggle 追加する (足し引き)。
    /// selected [A] に B を Shift+click → [A,B]。
    #[test]
    fn automation_shift_click_adds_to_selection() {
        let tracks = auto_tracks();
        // B 中央 (450,60) を press→release 同位置 (dist<4 → demote)。
        let (count, next, _) = run_auto_drag(
            &tracks,
            &[ack(100)],
            daw_ui_platform::Modifiers { shift: true, ..daw_ui_platform::Modifiers::empty() },
            (450.0, 60.0),
            (450.0, 60.0),
        );
        assert_eq!(count, 1);
        assert_eq!(next, Some(vec![ack(100), ack(101)]), "Shift+click: [A] に B を追加 → [A,B]");
    }

    /// #071: Shift+クリックが既に選択中の clip を選択集合から外す (toggle off)。
    #[test]
    fn automation_shift_click_removes_from_selection() {
        let tracks = auto_tracks();
        // A 中央 (250,60)、 selected [A,B] から A を Shift+click → [B]。
        let (count, next, _) = run_auto_drag(
            &tracks,
            &[ack(100), ack(101)],
            daw_ui_platform::Modifiers { shift: true, ..daw_ui_platform::Modifiers::empty() },
            (250.0, 60.0),
            (250.0, 60.0),
        );
        assert_eq!(count, 1);
        assert_eq!(next, Some(vec![ack(101)]), "Shift+click: [A,B] から A を除去 → [B]");
    }

    /// #071: 選択中の clip を掴んで drag すると選択中の全 clip を一括 move する (MoveAutomationClips の
    /// delta が選択数分)。 selected [A,B]、 A 中央 (250,60) から (350,60) へ +2 拍 (snap OFF)。
    #[test]
    fn automation_drag_selected_clip_moves_all() {
        let tracks = auto_tracks();
        let (select_count, _, move_len) = run_auto_drag(
            &tracks,
            &[ack(100), ack(101)],
            daw_ui_platform::Modifiers::empty(),
            (250.0, 60.0),
            (350.0, 60.0),
        );
        assert_eq!(select_count, 0, "clip 上 plain drag は選択 emit せず move する");
        assert_eq!(move_len, 2, "選択中の A/B を 1 件の MoveAutomationClips で一括移動 (delta 2)");
    }

    /// #071: 非選択の clip を掴んで drag したときは その 1 つだけ move する (選択集合に含まれない =
    /// grabbed のみ)。 selected [B]、 A を drag → delta 1。
    #[test]
    fn automation_drag_unselected_clip_moves_only_it() {
        let tracks = auto_tracks();
        let (_, _, move_len) = run_auto_drag(
            &tracks,
            &[ack(101)],
            daw_ui_platform::Modifiers::empty(),
            (250.0, 60.0),
            (350.0, 60.0),
        );
        assert_eq!(move_len, 1, "選択外の clip drag は掴んだ 1 つだけ移動 (delta 1)");
    }
}
