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

use daw_ui_platform::CursorIcon;
use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::snap::SnapConfig;
use crate::time::{TimeDisplay, TimeMapping};
use crate::ui::Ui;
use crate::viewport::ViewportState1D;
use crate::widgets::heavy::HeavyCtx;
use crate::widgets::playhead::draw_playhead_line;
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

/// 1 つの clip。`Arc<str>` で複数 clip 間の name 共有可能。
#[derive(Clone, Debug)]
pub struct ArrangementClip {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    pub name: Arc<str>,
    pub color: Option<Color>,
    /// M14 Phase 63e (#019): 共有 (linked) clip のアクセント色 hue (HSL の H、 `[0.0, 1.0)` 周期)。
    /// `None` で通常 clip (既存の `color` を使う)、 `Some(hue)` で widget が
    /// `(hue, share_group_saturation, share_group_fill_lightness, share_group_border_lightness)`
    /// → RGB 変換した塗り / 枠を描画 + clip 名の左に `share_group_link_glyph` を描く。
    /// caller は `content_id` を `[0.0, 1.0)` に hash して渡す想定 (refcount >= 2 のときだけ
    /// `Some` を入れれば、 同じ content を共有する clip 群が同色 + link icon になる)。
    pub share_group_color: Option<f32>,
    /// M14 Phase 63k (#025): audio clip の inline 編集 (gain_db / fade)。 `Some` で widget が
    /// dB handle / fade 角 / envelope を描画 + grip 領域に drag handler を bind し
    /// `SetClipGainDb` / `SetClipFade` / `SetClipFadeCurve` を発行する。 MIDI / Vocal clip は
    /// `None` で既存挙動 (clip 内 hit zone 全体が Move、 audio 描画なし)。
    pub audio_edit: Option<ArrangementClipAudioEdit>,
}

/// 1 つの track。`clips` は `start_beat` 昇順前提。
///
/// `muted` / `solo` / `collapsed` / `automation_lanes_collapsed` は意味的に独立した bool 状態
/// (集約の余地がない、 Bitwig / Reaper の track schema と整合) なので `clippy::struct_excessive_bools`
/// を allow する。
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
pub struct ArrangementTrack {
    pub id: u32,
    pub name: Arc<str>,
    pub muted: bool,
    pub solo: bool,
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
/// 至る曲線形状)。 daw_01 conversation #028 §11.1 と整合。
/// - `Hold`: 階段 (前の値を保持して垂直立ち上がり)
/// - `Linear`: 直線
/// - `Bezier { tension }`: Catmull-Rom スプライン + tension (`-1.0..=1.0`、 `0.0` で標準 Catmull-Rom)
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ArrangementCurveKind {
    Hold,
    Linear,
    Bezier { tension: f32 },
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
/// `share_group_color` は #019 の audio clip 用 hue (`[0.0, 1.0)`) と同 helper を流用。
#[derive(Clone, Debug)]
pub struct ArrangementAutomationClip {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    pub name: Arc<str>,
    /// clip-local 座標 (clip start = 0)、 `time_beat` 昇順前提 (caller 責務)。
    pub points: Vec<ArrangementAutomationPoint>,
    /// linked clip 識別 hue (`[0.0, 1.0)`)。 `Some(hue)` で widget が hsl_to_rgb で fill / border を
    /// 上書き + clip 名の左に link glyph を描く (audio clip と同 path)。
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
        }
    }
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
    /// min/max の clamp は app 側で実施 (目安 16..96 px)。
    SetTrackRowH(f32),
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
        }
    }
}

/// arrangement の見た目スタイル。`Default` で example 互換の見た目を再現。
#[derive(Clone, Copy, Debug)]
pub struct ArrangementStyle {
    pub bg: Color,
    pub header_bg: Color,
    pub ruler_bg: Color,
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
    pub clip_selected_fill: Color,
    pub clip_selected_border: Color,
    pub clip_selected_border_w: f32,
    pub clip_text_color: Color,
    pub clip_text_size: f32,
    pub track_selected_bg: Color,
    pub track_text_color: Color,
    pub track_text_size: f32,
    pub mute_hint: Color,
    pub solo_hint: Color,
    pub mute_solo_hint_h: f32,
    pub playhead_color: Color,
    pub playhead_width_px: f32,
    pub loop_band: Color,
    pub loop_handle: Color,
    pub loop_handle_w: f32,
    /// resize handle の幅 (px)。clip rect 左右 edge から **内外** この px = resize、
    /// それ以外 (rect 中央) = move。短 clip (`r.w <= resize_handle_px * 2`) は rect 内
    /// すべて Move、rect 外側のみ resize 判定。
    pub resize_handle_px: f32,
    pub mute_button: ToggleButtonStyle,
    pub solo_button: ToggleButtonStyle,
    /// M10 Phase 46: track header drag&drop 時に drop 位置 (target row の上 edge) に描く横 line の色。
    pub reorder_drop_indicator: Color,
    /// drop indicator の縦幅 (px)。
    pub reorder_drop_indicator_h: f32,
    /// drag 中 row 複製の不透明度 (`0.0..1.0`、`1.0` で完全不透明)。RGB は元 row 色 (selected/header) を使う。
    pub reorder_drag_alpha: f32,
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
    /// M14 Phase 63c (#016): group 行 (= 子を持つ track) の背景色。 selection 状態と排他で
    /// `track_group_bg` を背景に塗る (selected が priority)。
    pub track_group_bg: Color,
    /// M14 Phase 63c (#016): ▼ / ▶ disclosure アイコンの色 (group 行の左端)。
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
    // ---- M14 Phase 63e (#019): share group (linked clip group) 描画パラメータ ----
    /// `share_group_color = Some(hue)` の clip 描画で使う HSL の S (`[0.0, 1.0]`、 default = 0.55)。
    pub share_group_saturation: f32,
    /// share clip の rect 塗り (fill) に使う HSL の L (default = 0.55)。
    pub share_group_fill_lightness: f32,
    /// share clip の rect 枠 (border) に使う HSL の L (default = 0.75、 fill より明るくして強調)。
    pub share_group_border_lightness: f32,
    /// share clip の rect 塗り alpha (`[0.0, 1.0]`、 default = 0.85、 微透明にして他 clip と区別)。
    pub share_group_alpha: f32,
    /// share clip の name 左に描く link glyph (default = `'⇌'` U+21CC)。 font に存在しない場合は
    /// caller 側で ASCII (`'~'` 等) に差し替える。
    pub share_group_link_glyph: char,
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
    /// lane 行の背景色 (track 行と差別化、 default は track_group_bg より暗め)。
    pub automation_lane_bg: Color,
    /// disabled lane (= `enabled = false`) の curve / clip / point 描画色 (灰色 bypass 表現)。
    pub automation_lane_disabled_color: Color,
    /// lane curve 線の太さ (px)。 default 1.5。
    pub automation_curve_line_width_px: f32,
    /// lane 内 point の半径 (px)。 default 4.0。
    pub automation_point_radius_px: f32,
    /// lane 内 default_value 水平線の色 (clip ギャップ / curve 範囲外の値表示)。
    pub automation_default_line_color: Color,
    /// default_value 水平線の太さ (px)。 default 1.0。
    pub automation_default_line_width_px: f32,
    /// lane 内 clip rect の縦 padding (px、 上下にこの px を確保した残りを clip rect の高さに)。 default 6.0。
    pub automation_clip_v_pad_px: f32,
    /// lane header の slider 帯 (default_value_norm 表示) の縦幅 (px)。 default 4.0。
    pub automation_default_band_h: f32,
    /// disclosure ▶ / ▼ glyph の描画 font size。 default = `track_text_size`。
    pub automation_disclosure_size: f32,
    /// lane header に描く icon glyph (`★` / `[V]` / `👁` / `▣` / `✕`) の font size。 default = `track_text_size`。
    pub automation_lane_icon_size: f32,
    /// lane header の text color (label + icon、 default = `track_text_color`)。
    pub automation_lane_text_color: Color,
}

impl Default for ArrangementStyle {
    #[allow(clippy::too_many_lines)]
    fn default() -> Self {
        let mute_button = ToggleButtonStyle {
            off_color: Color::rgb(0.18, 0.20, 0.24),
            on_color: Color::rgb(0.55, 0.18, 0.18),
            hint_band: Some(Color::rgb(1.0, 0.30, 0.20)),
            hint_band_h: 3.0,
            border: Color::rgb(0.30, 0.32, 0.36),
            border_width: 1.0,
            radius: 3.0,
            font_size: 11.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
        };
        let solo_button = ToggleButtonStyle {
            off_color: Color::rgb(0.18, 0.20, 0.24),
            on_color: Color::rgb(0.55, 0.50, 0.18),
            hint_band: Some(Color::rgb(1.0, 0.85, 0.20)),
            hint_band_h: 3.0,
            border: Color::rgb(0.30, 0.32, 0.36),
            border_width: 1.0,
            radius: 3.0,
            font_size: 11.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
        };
        Self {
            bg: Color::rgb(0.10, 0.11, 0.13),
            header_bg: Color::rgb(0.14, 0.15, 0.18),
            ruler_bg: Color::rgb(0.16, 0.17, 0.20),
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
            clip_selected_border_w: 2.0,
            clip_text_color: Color::rgb(0.95, 0.95, 0.97),
            clip_text_size: 11.0,
            track_selected_bg: Color::rgb(0.20, 0.24, 0.32),
            track_text_color: Color::rgb(0.92, 0.92, 0.94),
            track_text_size: 12.0,
            mute_hint: Color::rgba(1.0, 0.30, 0.20, 0.60),
            solo_hint: Color::rgba(1.0, 0.85, 0.20, 0.60),
            mute_solo_hint_h: 3.0,
            playhead_color: Color::rgb(1.0, 0.25, 0.10),
            playhead_width_px: 2.5,
            loop_band: Color::rgba(0.50, 0.85, 1.0, 0.20),
            loop_handle: Color::rgb(0.50, 0.85, 1.0),
            loop_handle_w: 2.0,
            resize_handle_px: 4.0,
            mute_button,
            solo_button,
            reorder_drop_indicator: Color::rgb(0.50, 0.85, 1.0),
            reorder_drop_indicator_h: 2.0,
            reorder_drag_alpha: 0.6,
            track_volume_band_h: 4.0,
            track_volume_band_track: Color::rgba(0.0, 0.0, 0.0, 0.45),
            track_volume_band_fill: Color::rgb(0.95, 0.95, 0.97),
            ruler_label_color: Color::rgb(0.85, 0.88, 0.92),
            indent_px: 16.0,
            track_group_bg: Color::rgb(0.16, 0.22, 0.32),
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
            // share_group_color の HSL 変換パラメータ — saturation 0.55 で派手すぎず、 fill L=0.55、
            // border L=0.75 で fill より明るく差をつける (識別性 + コントラスト両立)。
            share_group_saturation: 0.55,
            share_group_fill_lightness: 0.55,
            share_group_border_lightness: 0.75,
            share_group_alpha: 0.85,
            share_group_link_glyph: '⇌',
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
            automation_default_line_color: Color::rgba(1.0, 1.0, 1.0, 0.18),
            automation_default_line_width_px: 1.0,
            automation_clip_v_pad_px: 6.0,
            automation_default_band_h: 4.0,
            automation_disclosure_size: 12.0,
            automation_lane_icon_size: 12.0,
            automation_lane_text_color: Color::rgb(0.92, 0.92, 0.94),
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

/// M14 Phase 63n-1 (#028): 単一 visible track の expanded 高さ (= `track_row_h` + lane 群高さ合計)。
/// `automation_lanes_collapsed = true` または `automation_lanes` 空 / 全 invisible で `track_row_h` を返す
/// (= 既存挙動完全互換)。 widget 内 row_y prefix sum と hit-test の SSoT。
#[must_use]
pub fn track_row_height(track: &ArrangementTrack, track_row_h: f32) -> f32 {
    track_row_h + automation_lanes_total_h(track)
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

/// lanes 内 cursor 位置から hit する (ClipKey, ClipDragKind) を返す (後勝ち)。
///
/// resize handle は clip rect の左右 edge から **内外** ±`resize_handle_px` の範囲
/// (= 8px 幅のハンドル帯)。短 clip (`r.w <= resize_handle_px * 2`) は rect 内は Move 強制、
/// rect 外側のみ resize 判定。
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
    for clip in &track.clips {
        if let Some(kind) =
            clip_zone_at(row_top, view.track_row_h, clip, view, lanes, cx, cy, resize_handle_px)
        {
            hit = Some((ClipKey { track: track.id, clip: clip.id }, kind));
        }
    }
    hit
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

/// loop band の hit 種別 (start handle / end handle / 中央 / 範囲外)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopBandHit {
    Start,
    End,
    Middle,
}

#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn loop_band_hit_kind(
    range: (f64, f64),
    view: ArrangementView,
    ruler: Rect,
    px: f32,
    handle_radius_px: f32,
) -> Option<LoopBandHit> {
    if !ruler.contains(px, ruler.y + ruler.h * 0.5) {
        return None;
    }
    let beat_to_px = f64::from(ruler.w) / view.len_beats.max(1e-6);
    let start_x = ruler.x + ((range.0 - view.start_beat) * beat_to_px) as f32;
    let end_x = ruler.x + ((range.1 - view.start_beat) * beat_to_px) as f32;
    let edge = handle_radius_px.max(1.0);
    if (px - start_x).abs() <= edge {
        Some(LoopBandHit::Start)
    } else if (px - end_x).abs() <= edge {
        Some(LoopBandHit::End)
    } else if px > start_x && px < end_x {
        Some(LoopBandHit::Middle)
    } else {
        None
    }
}

#[inline]
fn px_to_beat(px: f32, lanes_x: f32, lanes_w: f32, view: ArrangementView) -> f64 {
    let beat_per_px = view.len_beats / f64::from(lanes_w.max(1.0));
    view.start_beat + f64::from(px - lanes_x) * beat_per_px
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

/// track header 1 行内のレイアウト (Name button + 2 small buttons + 任意の volume band + lane disclosure)。
/// `name_rect` (= drag start zone & text area)、`buttons` (= [M, S]、Phase 47c で ↑/↓/× は drag&drop +
/// Delete shortcut に置換され削除)、`volume_band` は inner 下部に band 用の余裕がある時のみ `Some` (Phase 47b)、
/// `lane_disc_rect` は M14 Phase 63n-2 で S button の **右** に予約された lane disclosure
/// (`+`/`-` icon) 用の rect (track_row 全体の右端、 automation_lanes が空でも常に layout に含めて
/// 名前領域を一定にする)。
struct HeaderRowLayout {
    name_rect: Rect,
    buttons: [Rect; 2],
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
    // Phase 47c: M + S の 2 button (← Phase 45e の name + M + S + ↑ + ↓ + × の 6 button から削減)。
    // ↑/↓ は drag&drop reorder (Phase 46) で代替、× は Delete shortcut (Phase 47c) で代替。
    let n_btn = 2;
    // M14 Phase 63n-2 (#028): lane disclosure 用の幅を予約 (= disc_size + gap)。 S button の右に
    // 配置するため `total_right` に加算 → name_rect が縮む代わりに lane_disc が button と重ならない。
    let lane_disc_size = 12.0_f32;
    let lane_disc_extra = lane_disc_size + gap;
    #[allow(clippy::cast_precision_loss)]
    let total_right = small * n_btn as f32 + gap * n_btn as f32 + lane_disc_extra;
    let name_w = (inner.w - total_right).max(20.0);
    let name_rect = Rect { x: inner.x, y: inner.y, w: name_w, h: btn_h };
    let mut x_cursor = inner.x + name_w + gap;
    let mut buttons = [Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }; 2];
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopDragKind {
    Start,
    End,
    Middle,
    NewRange,
}

#[derive(Clone, Copy, Debug)]
struct LoopDragSession {
    kind: LoopDragKind,
    anchor_loop: (f64, f64),
    anchor_press_beat: f64,
    /// drag 中の最終 mouse x 位置 (release frame の `pointer.pos` に頼らないための保険、
    /// `ClipDragSession.last_mouse` と同じ理由)。
    last_mouse_x: f32,
    /// M14 Phase 63j (#024): drag 中の最終 alt 状態。 `ClipDragSession.last_alt` と
    /// 同じ仕組みで track (continuation で update、 release で skip)。 release frame の
    /// `pointer.modifiers.alt` が ModifiersChanged 先行で false 化する race を回避。
    last_alt: bool,
}

/// M10 Phase 46 / M14 Phase 63c (#016): track header drag&drop session。 release frame で
/// **drop target に応じて `ReorderTracks` (sibling) と `SetTrackParent` (parent 変更) を振り分け** る。
/// multi-select 時は `source_track_ids` に selected_tracks をそのまま乗せて一括移動する。
#[derive(Clone, Debug)]
struct TrackReorderSession {
    anchor_track_id: u32,
    anchor_index: usize,
    /// M14 Phase 63c (#016): drag 開始時に grab した track 群 (selected_tracks に含まれていれば
    /// selected 全部、 そうでなければ `vec![anchor_track_id]`)。 multi-track reparent / reorder の
    /// source として release frame で使う。
    source_track_ids: Vec<u32>,
    anchor_mouse_y: f32,
    /// drag 中の最終 mouse y 位置 (release frame の `pointer.pos` に頼らない保険、`ClipDragSession.last_mouse` と同理由)。
    last_mouse_y: f32,
}

/// M14 Phase 63j (#024): ruler 上の plain (= Shift 非保持) click / drag による
/// playhead seek セッション。 press frame で `state.playhead_drag = Some(...)` し、
/// continuation frame で毎 frame `SetPlayheadBeat` を発行 (`last_emitted_beat` で重複抑制)。
/// release frame で `take()` して discard (commit-by-release 無し、 既に逐次発行済)。
#[derive(Clone, Copy, Debug)]
struct PlayheadDragSession {
    /// drag 中の最終 mouse x 位置 (release frame の `pointer.pos` に頼らない保険、
    /// `ClipDragSession.last_mouse` と同理由)。 release では emit しないので現状未使用だが、
    /// 他 drag session と field 構成を揃えて将来の visual debug を容易にする。
    last_mouse_x: f32,
    /// drag 中に最後に発火した snap 適用済 beat 値 (毎 frame 同値発火を抑制)。
    /// press frame で初期化済み (= press 即発行値)、 continuation で differ 時のみ更新 + emit。
    last_emitted_beat: f64,
}

/// M10 Phase 47b: track header の bottom band slider drag による volume 編集セッション。
#[derive(Clone, Copy, Debug)]
struct TrackVolumeDragSession {
    track_id: u32,
    anchor_volume: f32,
    /// drag 開始時の band rect (mouse_x → 0..1 マップに使用、release frame に view が変化しても安定)。
    band_rect: Rect,
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

/// M14 Phase 63j (#024): loop drag の overlay と release commit で共有する snap 適用済
/// `(start, end)` 計算。 clip drag と同じ pattern (overlay と commit が必ず同一値で確定):
/// - **Start drag**: 端点 (cur_beat) を grid に round → `min(snapped, anchor_loop.1)` で start 確定
/// - **End drag**: 端点 (cur_beat) を grid に round → `max(snapped, anchor_loop.0)` で end 確定
/// - **Middle drag**: 絶対位置 snap (Cubase / Live の Move pattern と同じ)。 `anchor_loop.0` を pivot
///   として grid に round → その差分 (delta) を両端に適用、 duration 維持。
/// - **NewRange**: `anchor_press_beat` は press 時に snap 済 (caller 側責務)、 ここは cur_beat だけ
///   snap。 両端を独立に snap してから順序正規化。 duration 0 でも問題なし (caller 側で扱う)。
///
/// `last_alt = true` で snap 一時無効 (raw 通過、 `MoveClips` と同じ alt 直交 policy)。
fn compute_loop_drag_endpoints(
    ld: &LoopDragSession,
    cur_beat_raw: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> (f64, f64) {
    let alt = ld.last_alt;
    match ld.kind {
        LoopDragKind::Start => {
            let s = snap.snap_beat(cur_beat_raw, alt, zoom_x_px_per_beat);
            (s.min(ld.anchor_loop.1), ld.anchor_loop.1)
        }
        LoopDragKind::End => {
            let e = snap.snap_beat(cur_beat_raw, alt, zoom_x_px_per_beat);
            (ld.anchor_loop.0, e.max(ld.anchor_loop.0))
        }
        LoopDragKind::Middle => {
            let raw_delta = cur_beat_raw - ld.anchor_press_beat;
            let pivot = ld.anchor_loop.0;
            let snapped_pivot =
                snap.snap_beat(pivot + raw_delta, alt, zoom_x_px_per_beat);
            let delta = snapped_pivot - pivot;
            (ld.anchor_loop.0 + delta, ld.anchor_loop.1 + delta)
        }
        LoopDragKind::NewRange => {
            let other = snap.snap_beat(cur_beat_raw, alt, zoom_x_px_per_beat);
            (ld.anchor_press_beat.min(other), ld.anchor_press_beat.max(other))
        }
    }
}

/// M14 Phase 61b (#011): caller の `data_generation` は track 構成 (順序 / mute / solo /
/// volume / name / clip 個数) のみの責務に整理し、 clip 個別の `(id, start_beat, len_beats)`
/// 変化は widget 側で吸収する。 旧設計は caller が data_generation で全網羅を要求されており、
/// 漏れると drag move 後に古い clip rect が残像として残る (#011 (2))。 全 caller が同じ
/// boilerplate を書くのは設計欠陥のシグナル (`feedback_pursue_best_practice`)。
///
/// FNV-1a 風 fold (大きな素数倍 + xor)。 100 clip × 4 fold step = ~100ns @ 4GHz、 16ms 予算
/// の 0.001%。 `ArrangementClip` は gui_01 公開型なので widget が hash する権利あり (no-Clone
/// 不変条件にも触れない、 `u32`/`f64` は Copy)。
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
fn draw_lanes_bg<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    lanes: Rect,
    tracks: &[ArrangementTrack],
    view: ArrangementView,
    selected_tracks: &[u32],
    is_group_set: &HashSet<u32>,
    style: &ArrangementStyle,
) {
    push_filled_rect(hctx, lanes, style.bg);

    // 各 track row 背景 (selection ハイライト + mute/solo hint band + group_bg)。
    // M14 Phase 63c (#016): collapsed 親配下は描画 skip (visible 列のみ index で row を計算)。
    let visible_indices = compute_visible_indices(tracks);
    for (visible_i, &i) in visible_indices.iter().enumerate() {
        let t = &tracks[i];
        #[allow(clippy::cast_precision_loss)]
        let row_y = lanes.y - view.track_top + visible_i as f32 * view.track_row_h;
        let row = Rect { x: lanes.x, y: row_y, w: lanes.w, h: view.track_row_h };
        if row.y + row.h < lanes.y || row.y > lanes.y + lanes.h {
            continue;
        }
        // selection priority > group_bg > 通常 (selection は overlay layer で再描画される
        // が、 lanes_bg では下塗りとして塗る = visual hint としての役割)。 is_group_set は
        // caller の **full tracks** から計算済 (collapsed 後も group 判定が安定)。
        if selected_tracks.contains(&t.id) {
            push_filled_rect(hctx, row, style.track_selected_bg);
        } else if is_group_set.contains(&t.id) {
            push_filled_rect(hctx, row, style.track_group_bg);
        }
        if t.muted {
            push_filled_rect(
                hctx,
                Rect {
                    x: row.x,
                    y: row.y + row.h - style.mute_solo_hint_h,
                    w: row.w,
                    h: style.mute_solo_hint_h,
                },
                style.mute_hint,
            );
        }
        if t.solo {
            push_filled_rect(
                hctx,
                Rect {
                    x: row.x,
                    y: row.y + row.h - style.mute_solo_hint_h * 2.0 - 1.0,
                    w: row.w,
                    h: style.mute_solo_hint_h,
                },
                style.solo_hint,
            );
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

/// M14 Phase 63e (#019): HSL `(h, s, l, a)` → RGBA `Color` 変換 (`share_group_color` 用)。
/// `h` は `[0.0, 1.0)` 周期 (0=赤, 0.33=緑, 0.66=青)。 caller が範囲外を渡した場合は内部で
/// `rem_euclid(1.0)` してから処理 (defensive)。 standard CSS HSL の chroma-based 算出に従う。
/// 単一文字名 (h/s/l/a/c/x/m) は HSL→RGB の標準表記 (CSS 仕様準拠)、 数学関数として保持。
#[allow(clippy::many_single_char_names)]
fn hsl_to_rgb(h: f32, s: f32, l: f32, a: f32) -> Color {
    let h = h.rem_euclid(1.0);
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h6 = h * 6.0;
    let x = c * (1.0 - (h6.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = if h6 < 1.0 {
        (c, x, 0.0)
    } else if h6 < 2.0 {
        (x, c, 0.0)
    } else if h6 < 3.0 {
        (0.0, c, x)
    } else if h6 < 4.0 {
        (0.0, x, c)
    } else if h6 < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c * 0.5;
    Color::rgba(r1 + m, g1 + m, b1 + m, a)
}

fn draw_clip<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    r: Rect,
    clip: &ArrangementClip,
    style: &ArrangementStyle,
    lanes: Rect,
    selected: bool,
) {
    if r.x + r.w < lanes.x || r.x > lanes.x + lanes.w {
        return;
    }
    // M14 Phase 63e (#019) + Phase 63f (#022): 描画状態は (selected, share_group_color) の
    // 組合せで決まる。 selected は selection 色を最優先 (link glyph の有無に依らず) し、
    // share_group_color は非 selected かつ Some(hue) の場合のみ HSL → RGB で fill / border を
    // 上書き。 caller の `clip.color` は share clip では ignore (group hue で識別する設計)。
    let (fill, border, border_w, text_color) = if selected {
        (
            style.clip_selected_fill,
            style.clip_selected_border,
            style.clip_selected_border_w,
            Color::rgb(0.10, 0.10, 0.15),
        )
    } else if let Some(hue) = clip.share_group_color {
        let fill_c = hsl_to_rgb(
            hue,
            style.share_group_saturation,
            style.share_group_fill_lightness,
            style.share_group_alpha,
        );
        let border_c = hsl_to_rgb(
            hue,
            style.share_group_saturation,
            style.share_group_border_lightness,
            1.0,
        );
        (fill_c, border_c, style.clip_border_w, style.clip_text_color)
    } else {
        (
            clip.color.unwrap_or(style.clip_default_fill),
            style.clip_border,
            style.clip_border_w,
            style.clip_text_color,
        )
    };
    hctx.push_rect(RectCommand {
        rect: r,
        fill,
        border,
        border_width: border_w,
        radius: [style.clip_radius; 4],
        clip_rect: Some(lanes),
    });
    if r.w > 24.0 && r.h > style.clip_text_size + 2.0 {
        // share clip は name の左に link glyph (`⇌` 等) を 1 文字描画。 glyph 幅は font 依存だが、
        // 等幅 (HackGen Console NF) では `clip_text_size` ~= 1 文字幅。 name は glyph + 2px gap
        // だけ右にずらす。 通常 clip (None) は従来通り `r.x + 4.0` で描画。 link glyph は
        // selection と独立 (= selected でも shared なら描画) — #022 で確定。
        let has_link = clip.share_group_color.is_some();
        let text_left = if has_link {
            r.x + 4.0 + style.clip_text_size + 2.0
        } else {
            r.x + 4.0
        };
        if has_link {
            hctx.push_text(GlyphArea {
                text: Arc::from(style.share_group_link_glyph.to_string()),
                left: r.x + 4.0,
                top: r.y + 2.0,
                font_size: style.clip_text_size,
                line_height: style.clip_text_size * 1.2,
                color: text_color,
                clip_rect: Some(r),
            });
        }
        hctx.push_text(GlyphArea {
            text: clip.name.clone(),
            left: text_left,
            top: r.y + 2.0,
            font_size: style.clip_text_size,
            line_height: style.clip_text_size * 1.2,
            color: text_color,
            clip_rect: Some(r),
        });
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
        if row_top + view.track_row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let end = c.start_beat + c.len_beats;
            if end < view.start_beat || c.start_beat > view_end {
                continue;
            }
            let r = clip_to_rect(row_top, view.track_row_h, c, view, lanes);
            draw_clip(hctx, r, c, style, lanes, false);
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
        if row_top + view.track_row_h < lanes.y || row_top > lanes.y + lanes.h {
            continue;
        }
        for c in &t.clips {
            let key = ClipKey { track: t.id, clip: c.id };
            if !selected.contains(&key) {
                continue;
            }
            let r = clip_to_rect(row_top, view.track_row_h, c, view, lanes);
            draw_clip(hctx, r, c, style, lanes, true);
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
        let mut lane_y = track_row_top + track_row_h;
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

/// 単一 segment (前 point → 次 point) を Catmull-Rom + tension で flatten。
/// `kind` が `Hold` なら階段 (前値で水平 → 次 point の x で垂直)、 `Linear` なら直線、
/// `Bezier { tension }` なら Catmull-Rom (tension は端点の引き / 反り具合)。
/// 出力点列は `out` に push (caller が始点を 1 度 push 済の前提、 終点 (= 次 point) を含む)。
/// `prev_pt` / `next_next_pt` は端点処理の virtual 点 (Phase 63n-1 では neighbor が無い場合
/// 自分自身で代用 = `automation_curve` widget と同 idiom、 端点の出入り角は直角)。
fn flatten_lane_segment(
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
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
            // Catmull-Rom → cubic Bezier (B1 = P1 + (P2-P0) * (1-tension)/6、 B2 = P2 - (P3-P1) * (1-tension)/6)。
            // tension = 1.0 で直線化、 tension = 0.0 で標準 Catmull-Rom、 tension = -1.0 で振り増し (over-shoot)。
            let scale = (1.0 - tension.clamp(-1.0, 1.0)) / 6.0;
            let b1 = (p1.0 + (p2.0 - p0.0) * scale, p1.1 + (p2.1 - p0.1) * scale);
            let b2 = (p2.0 - (p3.0 - p1.0) * scale, p2.1 - (p3.1 - p1.1) * scale);
            // 自前 de Casteljau (automation widget の flatten_cubic と同戦略、 chord 距離で再帰分割)。
            flatten_lane_cubic(p1, b1, b2, p2, max_segment_px, 0, out);
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
/// disabled lane (`enabled = false`) は curve / clip / point を `automation_lane_disabled_color` で描画、
/// share_group_color が `Some(hue)` の clip は audio clip と同 hsl 変換で fill / border を上書き。
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn draw_automation_lane<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    lane: &ArrangementAutomationLane,
    header_rect: Rect,
    body_rect: Rect,
    view: ArrangementView,
    style: &ArrangementStyle,
    lanes_clip: Rect,
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
    let curve_color = if lane.enabled {
        lane.color
    } else {
        style.automation_lane_disabled_color
    };
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

        // share_group_color (linked tint) — audio clip と同 hsl 変換。 disabled lane は **clip rect の
        // fill / border のみ灰色** (= bypass marker)、 中身 (curve / point / clip 名) は元の lane.color
        // のままにして可読性を保つ (Bitwig / Live と同パターン、 #028 user 指摘 3)。
        let (fill, border) = if lane.enabled {
            if let Some(hue) = c.share_group_color {
                (
                    hsl_to_rgb(
                        hue,
                        style.share_group_saturation,
                        style.share_group_fill_lightness,
                        style.share_group_alpha * 0.55, // automation clip は lane 上 overlay なので fill を更に薄く
                    ),
                    hsl_to_rgb(
                        hue,
                        style.share_group_saturation,
                        style.share_group_border_lightness,
                        1.0,
                    ),
                )
            } else {
                (
                    Color { r: lane.color.r, g: lane.color.g, b: lane.color.b, a: 0.20 },
                    lane.color,
                )
            }
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

        // clip name (上端、 lane なので小さめ)
        if w >= 28.0 {
            let glyph_color = if lane.enabled {
                style.automation_lane_text_color
            } else {
                style.automation_lane_disabled_color
            };
            hctx.push_text(GlyphArea {
                text: Arc::clone(&c.name),
                left: clip_rect.x + 4.0,
                top: clip_rect.y + 2.0,
                font_size: style.clip_text_size * 0.85,
                line_height: style.clip_text_size,
                color: glyph_color,
                clip_rect: Some(clip_rect),
            });
        }

        // curve flatten (clip 内描画域 = clip_rect 全体)。 caller の screen-wide な beat_to_px
        // (= body_rect.w / view.len_beats) を渡すことで、 curve x 座標が point dot 描画と完全一致。
        let flat = flatten_lane_curve(c, clip_rect, view.start_beat, body_rect.x, beat_to_px, 2.0);
        if flat.len() >= 2 {
            let segments: Vec<daw_ui_renderer::LineSegment> = flat
                .windows(2)
                .map(|w| daw_ui_renderer::LineSegment {
                    a: [w[0].0, w[0].1],
                    b: [w[1].0, w[1].1],
                    color: curve_color,
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
                fill: curve_color,
                border: Color { r: 1.0, g: 1.0, b: 1.0, a: if lane.enabled { 0.85 } else { 0.4 } },
                border_width: 1.0,
                radius: [r; 4],
                clip_rect: Some(clip_rect),
            });
        }
    }
}

fn draw_loop_band<M: ?Sized + 'static>(
    hctx: &mut HeavyCtx<'_, '_, M>,
    range: (f64, f64),
    view: ArrangementView,
    ruler: Rect,
    style: &ArrangementStyle,
) {
    let (lo, hi) = (range.0.min(range.1), range.0.max(range.1));
    let beat_to_px = f64::from(ruler.w) / view.len_beats.max(1e-6);
    #[allow(clippy::cast_possible_truncation)]
    let x0 = ruler.x + ((lo - view.start_beat) * beat_to_px) as f32;
    #[allow(clippy::cast_possible_truncation)]
    let x1 = ruler.x + ((hi - view.start_beat) * beat_to_px) as f32;
    let band_x = x0.max(ruler.x);
    let band_w = (x1.min(ruler.x + ruler.w) - band_x).max(0.0);
    if band_w > 0.0 {
        push_filled_rect(
            hctx,
            Rect { x: band_x, y: ruler.y, w: band_w, h: ruler.h },
            style.loop_band,
        );
    }
    let hw = style.loop_handle_w * 0.5;
    if x0 >= ruler.x - hw && x0 <= ruler.x + ruler.w + hw {
        push_filled_rect(
            hctx,
            Rect { x: x0 - hw, y: ruler.y, w: style.loop_handle_w, h: ruler.h },
            style.loop_handle,
        );
    }
    if x1 >= ruler.x - hw && x1 <= ruler.x + ruler.w + hw {
        push_filled_rect(
            hctx,
            Rect { x: x1 - hw, y: ruler.y, w: style.loop_handle_w, h: ruler.h },
            style.loop_handle,
        );
    }
}

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
        view: ArrangementView,
        selected_clips: &[ClipKey],
        selected_tracks: &[u32],
        style: &ArrangementStyle,
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
        let lanes_h = (rect.h - ruler_h).max(1.0);
        let lanes_w = (rect.w - header_w).max(1.0);
        let header_pane =
            Rect { x: rect.x, y: rect.y + ruler_h, w: header_w, h: lanes_h };
        let ruler =
            Rect { x: rect.x + header_w, y: rect.y, w: lanes_w, h: ruler_h };
        let lanes =
            Rect { x: rect.x + header_w, y: rect.y + ruler_h, w: lanes_w, h: lanes_h };

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
        let visible_tracks: Vec<ArrangementTrack> = visible_indices_press
            .iter()
            .map(|&i| tracks[i].clone())
            .collect();
        // M14 Phase 63n-1 (#028): visible track の prefix-sum row tops。 lane 0 個 (= 既存挙動)
        // では `tops[i] = lanes.y - track_top + i * track_row_h` と等価。 expand 中の lane 群が
        // ある track 以降は次 track 以降の row top が下にずれる (= 描画 / hit-test SSoT)。
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
            // M14 Phase 63k (#025): audio gesture (gain handle / fade corner) を最優先で振り分ける。
            // audio grip にヒットしたら clip_drag (Move/Resize) は起動しない (排他) — `audio_grip_hit_in_lanes`
            // が先勝で priority 判定する。 modifier (Shift / Ctrl) は audio gesture では無視 (Bitwig spec
            // §3.5/§3.6 と整合、 modifier-free な直感的操作)。 audio_edit が None の clip ではこの
            // ブロックは即 None を返すため、 既存挙動 (MIDI / Vocal clip) は影響を受けない。
            let audio_press = if in_lanes && !shift && !ctrl {
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
            } else if in_lanes
                && (!shift || ctrl)
                && let Some((hit_key, kind)) =
                    clip_hit(&visible_tracks, &press_tops, view, lanes, px, py, style.resize_handle_px)
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
            if in_ruler {
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
                        match loop_band_hit_kind(range, view, ruler, px, 4.0) {
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
                let track_row_bottom = row_top + view.track_row_h;
                if py < track_row_bottom {
                    // === track row press (既存ロジック) ===
                    let row =
                        Rect { x: header_pane.x, y: row_top, w: header_pane.w, h: view.track_row_h };
                    let layout = header_row_layout(row, style.track_volume_band_h);
                    if let Some(band) = layout.volume_band
                        && band.contains(px, py)
                    {
                        let av = t.volume.clamp(0.0, 1.0);
                        let state: &mut ArrangementState = self.widget_state(wid);
                        state.track_volume_drag = Some(TrackVolumeDragSession {
                            track_id: t.id,
                            anchor_volume: av,
                            band_rect: band,
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
                        } else if !in_small_button && !in_disclosure {
                            // M14 Phase 63c (#016): multi-select 中の drag は selected_tracks をまとめて
                            // 移動するため、 source_track_ids に selected を全部入れる (clicked が selected
                            // に含まれていなければ単独 drag = `vec![clicked]`)。
                            let source_ids: Vec<u32> = if selected_tracks.contains(&t.id) {
                                selected_tracks.to_vec()
                            } else {
                                vec![t.id]
                            };
                            let state: &mut ArrangementState = self.widget_state(wid);
                            state.track_reorder = Some(TrackReorderSession {
                                anchor_track_id: t.id,
                                anchor_index: idx,
                                source_track_ids: source_ids,
                                anchor_mouse_y: py,
                                last_mouse_y: py,
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
                            let header_rect = Rect {
                                x: header_pane.x + header_indent,
                                y: lane_y,
                                w: (header_pane.w - header_indent).max(2.0),
                                h: lh,
                            };
                            let lane_key = AutomationLaneKey { track: t.id, lane: lane.id };
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
                                {
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
            if in_lanes
                && !shift
                && !ctrl
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
            if let Some(ref mut ld) = state.loop_drag
                && !is_release
            {
                ld.last_mouse_x = px;
                // M14 Phase 63j (#024): last_alt は continuation で update、 release は
                // skip (clip_drag と同じ pattern、 OS event 順序による false 化 race を回避)。
                ld.last_alt = alt_now;
            }
            if let Some(ref mut tr) = state.track_reorder
                && !is_release
            {
                tr.last_mouse_y = py;
            }
            if let Some(ref mut tv) = state.track_volume_drag
                && !is_release
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

        // M14 Phase 63c (#016): track header drag release の **drop action 統合**:
        // 旧 `ReorderTracks` (sibling 並び替え) と `SetTrackParent` (parent 変更) を 1 つの
        // `SetTrackParent { tracks, parent, anchor_after }` に統合。 caller は (1) source を
        // arr_tracks から remove (2) parent_id を `parent` に更新 (3) `anchor_after` の直後
        // (None で先頭) に挿入、 という再構築をすればよい。 これで「Track 5 を Group A header に
        // drop → Group A subtree 末尾に挿入 + parent 化」 などの DAW 標準動作が 1 Edit で表現可能。
        //
        // anchor_after の計算ルール (visible-domain):
        //  - drop target が group → anchor_after = group の visible 列上の last descendant id
        //    (子が無ければ group 自身)、 parent = Some(target.id)
        //  - drop target が regular track:
        //    - top half (mouse y が row 上半分) → anchor_after = visible 列で 1 つ前の track id
        //      (None = 先頭挿入)、 parent = target.parent_id
        //    - bottom half → anchor_after = Some(target.id)、 parent = target.parent_id
        //  - drop on blank (mouse y が rows の外) → anchor_after = visible 列の最後 (or None)、
        //    parent = None (top-level 末尾)
        let pending_drop: Option<(Vec<u32>, Option<u32>, Option<u32>)> = {
            if let Some(ref tr) = track_reorder_release_raw {
                let dy = (tr.last_mouse_y - tr.anchor_mouse_y).abs();
                if dy >= 16.0 {
                    let visible_drop_idx =
                        track_index_from_y(tr.last_mouse_y, header_pane.y, &press_tops);
                    let drop_target = visible_drop_idx.and_then(|i| visible_tracks.get(i));
                    let (parent, anchor_after) = if let Some(target) = drop_target {
                        if is_group_set.contains(&target.id) {
                            // group 化: target subtree の末尾に挿入
                            let last_descendant = visible_tracks
                                .iter()
                                .rev()
                                .find(|t| {
                                    let mut p = t.parent_id;
                                    for _ in 0..64 {
                                        let Some(pid) = p else {
                                            return false;
                                        };
                                        if pid == target.id {
                                            return true;
                                        }
                                        p = tracks
                                            .iter()
                                            .find(|x| x.id == pid)
                                            .and_then(|x| x.parent_id);
                                    }
                                    false
                                })
                                .map(|t| t.id)
                                .or(Some(target.id));
                            (Some(target.id), last_descendant)
                        } else {
                            // 通常 track: top/bottom half で挿入位置を決定
                            #[allow(clippy::cast_precision_loss)]
                            let row_y = header_pane.y - view.track_top
                                + visible_drop_idx.unwrap_or(0) as f32 * view.track_row_h;
                            let local_y = tr.last_mouse_y - row_y;
                            let top_half = local_y < view.track_row_h * 0.5;
                            let prev_id = if top_half {
                                let prev_visible_i = visible_drop_idx
                                    .and_then(|i| i.checked_sub(1));
                                prev_visible_i.and_then(|i| visible_tracks.get(i)).map(|t| t.id)
                            } else {
                                Some(target.id)
                            };
                            (target.parent_id, prev_id)
                        }
                    } else {
                        // blank drop → top-level 末尾
                        let last_id = visible_tracks
                            .iter()
                            .rev()
                            .find(|t| t.parent_id.is_none())
                            .map(|t| t.id);
                        (None, last_id)
                    };
                    Some((tr.source_track_ids.clone(), parent, anchor_after))
                } else {
                    None
                }
            } else {
                None
            }
        };
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
            }
        }
        response.dragging = clip_drag_session.as_ref().map(|nd| nd.kind);
        response.reordering = track_reorder_session.as_ref().map(|tr| tr.anchor_track_id);
        response.dragging_track_volume = track_volume_session.map(|tv| tv.track_id);

        // ---- cursor ----
        // drag 中 / hover 中の clip 上 / それ以外で arrangement 内なら明示的に Default
        // にリセット (`set_cursor` を呼ばないと OS 側に前フレームの形が残る、winit は state-full)。
        if let Some(kind) = response.dragging {
            let cur = match kind {
                ClipDragKind::Move => CursorIcon::Move,
                ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => CursorIcon::EwResize,
            };
            self.set_cursor(cur);
        } else if response.reordering.is_some() {
            self.set_cursor(CursorIcon::Move);
        } else if response.dragging_track_volume.is_some() {
            self.set_cursor(CursorIcon::EwResize);
        } else if let Some(zone) = response.hovered_zone {
            let cur = match zone {
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
        let viewport_key = (
            (
                b"arrangement_widget_v5" as &[u8],
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
        // M14 Phase 63c (#016): heavy closure は `'static` 要求なので owned Vec<u32> で渡す
        // (selected_set と同パターン)。 loop 側の hit-test では `selected_tracks` slice (borrowed)
        // を直接 contains で参照するため、 ここで cloned heavy 用 vector を別に持って move 衝突を回避。
        let selected_tracks_for_heavy: Vec<u32> = selected_tracks.to_vec();
        // M14 Phase 63c (#016): is_group_set を heavy closure に move する用に owned コピー。
        // visible_tracks (filtered) では collapsed 後に children が消えて group 判定が false 化する
        // ため、 caller の full tracks から計算した HashSet を 'static に持ち込む。
        let is_group_set_for_heavy: HashSet<u32> = is_group_set.clone();
        let drag_overlay_clone = clip_drag_overlay.clone();
        // M14 Phase 63k (#025): audio_drag overlay 用 clone (heavy closure に move)。
        // ghost (drag 中の preview line / fade envelope / label) は cached 外で描画する。
        let audio_drag_overlay = audio_drag_session;
        // M14 Phase 63n-2 (#028): point_drag / lane_default_drag の overlay 用 clone (heavy closure
        // に move)。 point ghost は drag 中の preview を新位置に上書き、 band fill は drag 中の
        // last_emitted_value で塗り直し (cached 内描画は anchor 値のままなので cached 外で被せる)。
        let point_drag_overlay = point_drag_session;
        let lane_default_drag_overlay = lane_default_drag_session;
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
        // M10 Phase 46: track reorder の drag preview に必要な情報 (anchor index / 現在 mouse_y / target idx)。
        // dist >= 16px のときのみ overlay 描画 (短 click 中は静止 = button click と区別がつかないため UI ノイズ)。
        let reorder_overlay: Option<(TrackReorderSession, usize)> = track_reorder_session
            .as_ref()
            .filter(|tr| (tr.last_mouse_y - tr.anchor_mouse_y).abs() >= 16.0)
            .map(|tr| {
                let target = compute_reorder_target_index(
                    tr.anchor_index,
                    tr.last_mouse_y,
                    header_pane.y,
                    view.track_top,
                    view.track_row_h,
                    visible_tracks.len(),
                );
                (tr.clone(), target)
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
            // === cached: viewport_key 一致時 skip ===
            hctx.cached(viewport_key, |hctx| {
                push_filled_rect(hctx, header_pane, style_copy.header_bg);
                draw_lanes_bg(
                    hctx,
                    lanes,
                    &tracks_owned,
                    view_copy,
                    &selected_tracks_for_heavy,
                    &is_group_set_for_heavy,
                    &style_copy,
                );
                hctx.bar_beat_grid(
                    ("arr_grid", id_for_inner),
                    lanes,
                    mapping,
                    sample_viewport,
                    grid_style,
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
                if view_copy.ruler_h > 0.0 {
                    hctx.time_ruler(
                        ("arr_ruler", id_for_inner),
                        ruler,
                        mapping,
                        sample_viewport,
                        ruler_style,
                    );
                }

                // M14 Phase 63n-1 (#028): automation lane 行群の描画 (track 行の下、 expand されたもののみ)。
                // 各 visible track の `automation_lanes_collapsed = false` のとき、 visible lane を上から
                // 順に積む (header = lane 左端 / body = lane 右端 = clip 描画域と同 x)。 lane の y 範囲は
                // `tops[i] + track_row_h` から `tops[i+1]` (= 次 track 上端) の間。
                // 描画は cached 内: viewport_key に lane 関連 hash が入る前提 (fold_arrangement_clip_hash
                // を後ほど lane も含むように拡張する)。 現状は clip hash で大方の変化を検出可能。
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
                    let mut lane_y = track_row_top + view_copy.track_row_h;
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
                            lane,
                            header_rect,
                            body_rect,
                            view_copy,
                            &style_copy,
                            lanes,
                        );
                        lane_y += lh;
                    }
                }
            });

            // === cached 外: selection / drag preview / playhead / loop band ===
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
            // loop band: drag preview がある場合は preview を描く、無ければ view.loop_range
            if let Some(range) = loop_preview_clone.or(view_copy.loop_range) {
                draw_loop_band(hctx, range, view_copy, ruler, &style_copy);
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

            // === M10 Phase 46: track reorder drop indicator + dragging row preview ===
            if let Some((tr, target_idx)) = reorder_overlay {
                #[allow(clippy::cast_precision_loss)]
                let indicator_y = header_pane_copy.y - view_copy.track_top
                    + target_idx as f32 * view_copy.track_row_h;
                push_filled_rect(
                    hctx,
                    Rect {
                        x: header_pane_copy.x,
                        y: indicator_y - style_copy.reorder_drop_indicator_h * 0.5,
                        w: header_pane_copy.w + lanes.w,
                        h: style_copy.reorder_drop_indicator_h,
                    },
                    style_copy.reorder_drop_indicator,
                );
                // dragging row 半透明複製 (header_pane 領域、last_mouse_y 中心)。
                let row_h = view_copy.track_row_h;
                let drag_y = (tr.last_mouse_y - row_h * 0.5)
                    .clamp(header_pane_copy.y, header_pane_copy.y + header_pane_copy.h - row_h);
                let alpha = style_copy.reorder_drag_alpha.clamp(0.0, 1.0);
                let base_rgb = style_copy.track_selected_bg;
                push_filled_rect(
                    hctx,
                    Rect { x: header_pane_copy.x, y: drag_y, w: header_pane_copy.w, h: row_h },
                    Color::rgba(base_rgb.r, base_rgb.g, base_rgb.b, alpha),
                );
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
                    let mut deltas: Vec<MoveClipDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_start = (a.start_beat + beat_delta).max(0.0);
                        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                        let press_i32 = a.track_index as i32;
                        let new_idx = (press_i32 + track_delta).clamp(0, max_idx_i32);
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

        // ---- M14 Phase 63n-2 (#028): automation_point_drag release → MoveAutomationPoints ----
        // commit-by-release: 4px jitter 閾値で短 click 化 (= release で何も発火しない、 click 相当)。
        // anchor からの delta が閾値未満なら no-op (point の selection 等は仕様外で変化しない)。
        // alt は drag session の `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない
        // (clip_drag と同じ pattern: ModifiersChanged が MouseInput より先に届く race を回避)。
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
                let next_local =
                    (snapped_abs - ad.clip_start_beat).clamp(0.0, ad.clip_len_beats.max(0.0));
                let next_value = (ad.anchor_value_norm
                    - dy / ad.clip_rect_anchor.h.max(1.0))
                .clamp(0.0, 1.0);
                if (next_local - ad.anchor_time_beat).abs() > 1e-9
                    || (next_value - ad.anchor_value_norm).abs() > 1e-6
                {
                    let delta = MoveAutomationPointDelta {
                        point: ad.point,
                        prev_time_beat: ad.anchor_time_beat,
                        prev_value_norm: ad.anchor_value_norm,
                        next_time_beat: next_local,
                        next_value_norm: next_value,
                    };
                    self.push_edit(make_edit(ArrangementEditRequest::MoveAutomationPoints(
                        vec![delta],
                    )));
                }
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

        // ---- pure release on empty lanes (no drag started) → SelectClips clear ----
        // clip_drag_session が無い + 空白 release + Shift なし
        if pointer.primary_just_released
            && clip_short_click_pos.is_none()
            && !clip_drag_release_was_some
            && !pointer.modifiers.shift
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

        // ---- Shift+drag rect select (lanes 内で加算) ----
        // M14 Phase 63e follow-up (#021): **Ctrl+Shift** は clip_drag 側で
        // `CloneClipsIndependent` を emit する独立コピー意図のため、 rect select セッションを
        // 同時起動しない (`!ctrl` で除外)。 これを入れずに Ctrl+Shift+press したとき:
        //   - 上の press 振り分けで clip_drag 起動 (`(!shift || ctrl)` 経由)
        //   - ここで rect select 起動 → cyan overlay 描画 + release 時 `SelectClips` push
        //   両方走って `CloneClipsIndependent` の後 `SelectClips` で selection が上書きされ、
        //   user 視点「rect select に化けて clone が起きない」 症状になる。
        let drag_rect_wid = wid.child(b"rect_select");
        let shift_rect_active = {
            let state: &mut crate::widgets::drag_rect::DragRectState =
                self.widget_state(drag_rect_wid);
            state.drag_start.is_some()
        };
        let shift_press = pointer.primary_just_pressed
            && pointer.modifiers.shift
            && !pointer.modifiers.ctrl;
        if (shift_press || shift_rect_active)
            && let Some(drag) = self.take_drag_rect_in_rect(drag_rect_wid, lanes)
        {
            response.rect_select_active = true;
            if drag.modifiers.shift && drag.finished {
                let drag_rect = drag.rect();
                let mut set: HashSet<ClipKey> = selected_clips.iter().copied().collect();
                for (i, t) in visible_tracks.iter().enumerate() {
                    let row_top = press_tops[i];
                    for c in &t.clips {
                        let r = clip_to_rect(row_top, view.track_row_h, c, view, lanes);
                        if rects_intersect(r, drag_rect) {
                            set.insert(ClipKey { track: t.id, clip: c.id });
                        }
                    }
                }
                let mut new_keys: Vec<ClipKey> = set.into_iter().collect();
                new_keys.sort_by_key(|a| (a.track, a.clip));
                let mut prev_sorted: Vec<ClipKey> = selected_clips.to_vec();
                prev_sorted.sort_by_key(|a| (a.track, a.clip));
                if prev_sorted != new_keys {
                    self.push_edit(make_edit(ArrangementEditRequest::SelectClips {
                        prev: selected_clips.to_vec(),
                        next: new_keys,
                    }));
                    response.selection_changed = true;
                }
            }
        }

        // ---- wheel: Ctrl=zoom_x / Alt=zoom_y (row_h) / Shift=scroll_x / plain=track_top ----
        let scroll = self.take_scroll_in_rect(lanes);
        if scroll.1.abs() > 0.0 || scroll.0.abs() > 0.0 {
            let dy = scroll.1;
            if pointer.modifiers.ctrl {
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
                // M10 Phase 48: Alt+wheel で row_h 縦ズーム (zoom_x と同じ exp curve)。
                // app 側で 16..96 px 等の clamp を実施。
                // M14 Phase 61a (#011): 符号反転 + 係数 0.005→0.0015 (Ctrl+wheel と一貫、
                // wheel up = zoom in、 row 大きく)。
                // M14 Phase 61a follow-up: マウス y 位置を anchor に zoom、 SetTrackTop を同
                // frame で発行して mouse 下の track が画面上で動かないようにする (Cubase 標準)。
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
            } else if pointer.modifiers.shift {
                let delta = -f64::from(dy) * beat_per_px * 4.0;
                self.push_edit(make_edit(ArrangementEditRequest::SetScrollX(
                    view.start_beat + delta,
                )));
            } else {
                let new_top = (view.track_top - dy * 8.0).max(0.0);
                self.push_edit(make_edit(ArrangementEditRequest::SetTrackTop(new_top)));
            }
        }

        // ---- double-click (lanes 内で clip / lane body / 空白 track row) ----
        // M14 Phase 63n-2 (#028): priority 順:
        //  1. clip hit (track row 内 clip rect) → DoubleClickClip
        //  2. lane body 内 clip 内 (curve 描画域) → AddAutomationPoint (snap 適用)
        //  3. track row の空き → DoubleClickEmpty
        //  lane row の空き (clip ギャップ) は no-op (= 描画はあるが何も発火しない、
        //  既存 DoubleClickEmpty が発火しないよう lane row を track row と区別する)。
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
                // lane body dblclick: clip 内のみ AddAutomationPoint 発火 (clip ギャップは no-op)。
                let track_id = visible_tracks[t_idx].id;
                let lane = &visible_tracks[t_idx].automation_lanes[lane_idx];
                if let Some((clip_key, time_local, value_norm)) =
                    automation_clip_at(track_id, lane, body_rect, view, style, cx, cy)
                {
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
                }
            } else if let Some(idx) = track_index_from_y(cy, lanes.y, &press_tops)
                && let Some(t) = visible_tracks.get(idx)
            {
                // track row の空き dblclick (lane row では automation_lane_at が Some を返して
                // 上の分岐で吸収済 → ここに来るのは track row のみ)。
                let track_row_top = press_tops[idx];
                if cy < track_row_top + view.track_row_h {
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

        // ---- track headers (button_at × 4 + toggle_button_at × 2) + SelectTrack トリガ ----
        // M10 Phase 50: tracks_for_draw を使う (release frame の optimistic preview と同順序)。
        // M14 Phase 63c (#016): visible_indices を pre-compute して collapsed 親配下を skip、
        // visible_i (描画上の row index) を row_y に使う。 各 track header に depth * indent_px の
        // 左 indent + group track には disclosure ▼/▶ アイコン。 selection は selected_tracks_set で判定。
        // 修飾 (Shift / Ctrl) で Single / RangeFromAnchor / Toggle を decode し SelectTrack に乗せる。
        // 1 frame 内で最初に click された track id を `clicked_track` に蓄え、 loop 後に modifier-aware
        // SelectTrack を 1 度発行する (loop 内で複数発行しないため)。
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

                // 背景 (selection > group_bg > 通常)
                if selected_tracks.contains(&t.id) {
                    self.panel(("arr_thsel", t.id), row, style.track_selected_bg, 0.0);
                } else if is_group_set.contains(&t.id) {
                    self.panel(("arr_thgrp", t.id), row, style.track_group_bg, 0.0);
                } else {
                    self.panel(("arr_thbg", t.id), row, style.header_bg, 0.0);
                }

                // M14 Phase 63c (#016): depth * indent_px の左 indent。 layout 計算は indent 反映後の
                // row_inner で実行する (= row.x + indent、 row.w - indent)。
                let indent = f32::from(t.depth) * style.indent_px;
                let row_for_layout = Rect {
                    x: row.x + indent,
                    y: row.y,
                    w: (row.w - indent).max(2.0),
                    h: row.h,
                };
                let layout = header_row_layout(row_for_layout, style.track_volume_band_h);
                let name_rect = layout.name_rect;
                let [m_rect, s_rect] = layout.buttons;

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
                let button_zones: [Rect; 3] = [name_rect_visible, m_rect, s_rect];

                let id_name = ("arr_tname", t.id);
                let id_mute = ("arr_tmute", t.id);
                let id_solo = ("arr_tsolo", t.id);

                let track_id = t.id;
                let muted = t.muted;
                let solo = t.solo;

                let name_text = t.name.clone();
                // M14 Phase 63c (#016): name 領域 click は modifier-aware SelectTrack を loop 後に
                // 発行する形に変更。 button_at_clicked で click 検知のみ行い、 内部で Edit は emit
                // しない (旧設計は button_at の closure 内で SelectTrack を emit していた)。
                if self.button_at_clicked(id_name, &name_text, name_rect_visible) {
                    clicked_track_for_select = Some(t.id);
                }
                if self.take_double_click_in_rect(name_rect_visible).is_some() {
                    self.push_edit(make_edit(ArrangementEditRequest::BeginRenameTrack(track_id)));
                }
                self.toggle_button_at(id_mute, "M", m_rect, muted, &style.mute_button, |_| {
                    make_edit(ArrangementEditRequest::ToggleTrackMute(track_id))
                });
                self.toggle_button_at(id_solo, "S", s_rect, solo, &style.solo_button, |_| {
                    make_edit(ArrangementEditRequest::ToggleTrackSolo(track_id))
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
            if row_top + view.track_row_h < lanes.y || row_top > lanes.y + lanes.h {
                continue;
            }
            for c in &t.clips {
                let end = c.start_beat + c.len_beats;
                if end < view.start_beat || c.start_beat > view_end {
                    continue;
                }
                let r = clip_to_rect(row_top, view.track_row_h, c, view, lanes);
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
        }
    }

    fn track(id: u32, name: &str, clips: Vec<ArrangementClip>) -> ArrangementTrack {
        ArrangementTrack {
            id,
            name: Arc::from(name),
            muted: false,
            solo: false,
            clips,
            volume: 1.0,
            parent_id: None,
            depth: 0,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
            collapsed: false,
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
    fn clip_hit_adjacent_clips_back_wins_at_shared_handle() {
        let view = test_view();
        let lanes = test_lanes();
        // clip A (id 100, start=0, len=4) → x∈[0,160]、右端拡張 [156,164)
        // clip B (id 101, start=4, len=4) → x∈[160,320]、左端拡張 [156,164)
        // cx=161 は両方の拡張ハンドル領域 → 後勝ちで B
        let tracks = vec![track(
            10,
            "t0",
            vec![clip(100, 0.0, 4.0, "a"), clip(101, 4.0, 4.0, "b")],
        )];
        let tops = make_tops(&tracks, lanes, view);
        let hit = clip_hit(&tracks, &tops, view, lanes, 161.0, 16.0, 4.0);
        assert_eq!(
            hit,
            Some((ClipKey { track: 10, clip: 101 }, ClipDragKind::ResizeLeft))
        );
    }

    #[test]
    fn loop_band_hit_kind_start_handle() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        // beat_to_px = 40, range=(2, 6) → start_x=80, end_x=240
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 80.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::Start));
    }

    #[test]
    fn loop_band_hit_kind_end_handle() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 240.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::End));
    }

    #[test]
    fn loop_band_hit_kind_middle() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 160.0, 4.0);
        assert_eq!(hit, Some(LoopBandHit::Middle));
    }

    #[test]
    fn loop_band_hit_kind_outside() {
        let view = test_view();
        let ruler = Rect { x: 0.0, y: 0.0, w: 640.0, h: 24.0 };
        let hit = loop_band_hit_kind((2.0, 6.0), view, ruler, 400.0, 4.0);
        assert_eq!(hit, None);
    }

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
        assert!(s.mute_solo_hint_h > 0.0);
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
                    m.view,
                    &[],
                    &[],
                    &style,
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
                    m.view,
                    &[],
                    &[],
                    &style,
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
                    m.view,
                    &[],
                    &[],
                    &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                view,
                &[],
                &[],
                &style,
                |_| Edit::mutate(|()| {}),
            );
        });
        scene
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
            clips: Vec::new(),
            volume: 1.0,
            parent_id,
            depth,
            collapsed,
            automation_lanes_collapsed: true,
            automation_lanes: Vec::new(),
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
        // group_bg / disclosure_color は色が設定されていれば OK (alpha > 0 で defensive)
        assert!(
            s.track_group_bg.r > 0.0 || s.track_group_bg.g > 0.0 || s.track_group_bg.b > 0.0,
            "track_group_bg は黒以外の色"
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                    m.view,
                    &[],
                    &[],
                    &style,
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

    fn snap_quarter_beat() -> SnapConfig {
        // 1 beat snap = SnapMode::Straight { div: 4 }
        SnapConfig {
            mode: SnapMode::Straight { div: 4 },
            enabled: true,
            min_beat_unit: 1.0 / 128.0,
            time_sig: (4, 4),
        }
    }

    /// `compute_loop_drag_endpoints` の Start drag は moving 端点を grid に snap、 fixed 端点は不変。
    #[test]
    fn loop_endpoints_start_drag_snaps_moving_endpoint() {
        let ld = LoopDragSession {
            kind: LoopDragKind::Start,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 4.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        // cur_beat raw = 1.7 → snap → 2.0 (1 beat grid に round)、 end は 12.0 不変
        let (s, e) = compute_loop_drag_endpoints(&ld, 1.7, &snap, 50.0);
        assert!((s - 2.0).abs() < 1e-6, "Start drag で raw 1.7 → snap 2.0: got {s}");
        assert!((e - 12.0).abs() < 1e-6, "End は不変 12.0: got {e}");
    }

    /// End drag は end 端点を grid に snap、 start 端点は不変。
    #[test]
    fn loop_endpoints_end_drag_snaps_moving_endpoint() {
        let ld = LoopDragSession {
            kind: LoopDragKind::End,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 12.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        // cur_beat raw = 13.4 → snap → 13.0、 start は 4.0 不変
        let (s, e) = compute_loop_drag_endpoints(&ld, 13.4, &snap, 50.0);
        assert!((s - 4.0).abs() < 1e-6, "Start は不変 4.0: got {s}");
        assert!((e - 13.0).abs() < 1e-6, "End drag で raw 13.4 → snap 13.0: got {e}");
    }

    /// Middle drag は両端点を同 delta で平行移動 (duration 維持)、 delta は anchor_loop.0 が
    /// grid に着地するよう計算 (Cubase Move 流の絶対位置 snap)。
    #[test]
    fn loop_endpoints_middle_drag_preserves_duration_with_snap() {
        let ld = LoopDragSession {
            kind: LoopDragKind::Middle,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 6.0, // press 位置 (anchor_loop 内側のどこか)
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        // cur_beat raw = 7.7 → raw_delta = 1.7 → pivot = 4.0、 4.0 + 1.7 = 5.7 → snap → 6.0
        // → delta = 6.0 - 4.0 = 2.0 → new_range = (4.0+2.0, 12.0+2.0) = (6.0, 14.0)
        // duration = 14.0 - 6.0 = 8.0 = 元 duration 維持
        let (s, e) = compute_loop_drag_endpoints(&ld, 7.7, &snap, 50.0);
        assert!((s - 6.0).abs() < 1e-6, "Middle drag で start は snap 6.0: got {s}");
        assert!((e - 14.0).abs() < 1e-6, "Middle drag で end も同 delta 移動 14.0: got {e}");
        assert!(
            ((e - s) - 8.0).abs() < 1e-6,
            "duration 8.0 維持: got {}",
            e - s
        );
    }

    /// NewRange は 両端点を independent に snap (anchor_press_beat は press 時 snap 済前提、
    /// helper は cur_beat だけ snap)、 順序 (min, max) で正規化。
    #[test]
    fn loop_endpoints_newrange_snaps_both_endpoints() {
        // anchor_press_beat = 2.0 (press 時 snap 済の値を caller が渡してくる)
        let ld = LoopDragSession {
            kind: LoopDragKind::NewRange,
            anchor_loop: (2.0, 2.0),
            anchor_press_beat: 2.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        // cur_beat raw = 9.4 → snap → 9.0、 anchor_press = 2.0、 → (2.0, 9.0)
        let (s, e) = compute_loop_drag_endpoints(&ld, 9.4, &snap, 50.0);
        assert!((s - 2.0).abs() < 1e-6, "NewRange start = anchor_press 2.0: got {s}");
        assert!((e - 9.0).abs() < 1e-6, "NewRange end = snap(9.4) = 9.0: got {e}");
    }

    /// NewRange で cur_beat < anchor_press_beat の場合、 (min, max) 順序で正規化される。
    #[test]
    fn loop_endpoints_newrange_normalizes_reversed_drag() {
        let ld = LoopDragSession {
            kind: LoopDragKind::NewRange,
            anchor_loop: (8.0, 8.0),
            anchor_press_beat: 8.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        let snap = snap_quarter_beat();
        // 右から左に drag: cur_beat raw = 3.4 → snap → 3.0、 anchor_press = 8.0
        // → (3.0, 8.0) (min, max で正規化)
        let (s, e) = compute_loop_drag_endpoints(&ld, 3.4, &snap, 50.0);
        assert!((s - 3.0).abs() < 1e-6, "右→左 drag でも min が start: got {s}");
        assert!((e - 8.0).abs() < 1e-6, "max が end: got {e}");
    }

    /// `last_alt = true` で snap 一時無効、 raw 値が通る (`MoveClips` 同 policy)。
    #[test]
    fn loop_endpoints_alt_bypasses_snap() {
        let ld = LoopDragSession {
            kind: LoopDragKind::End,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 12.0,
            last_mouse_x: 0.0,
            last_alt: true, // alt 押下中は snap 無効
        };
        let snap = snap_quarter_beat();
        // cur_beat raw = 13.4 → alt bypass → 13.4 そのまま
        let (s, e) = compute_loop_drag_endpoints(&ld, 13.4, &snap, 50.0);
        assert!((s - 4.0).abs() < 1e-6, "Start は不変 4.0: got {s}");
        assert!((e - 13.4).abs() < 1e-6, "alt bypass で raw 13.4 そのまま: got {e}");
    }

    /// snap OFF 時は raw 値が通る (regression: 既存の non-snap caller を壊さない)。
    #[test]
    fn loop_endpoints_snap_off_returns_raw() {
        let ld = LoopDragSession {
            kind: LoopDragKind::Start,
            anchor_loop: (4.0, 12.0),
            anchor_press_beat: 4.0,
            last_mouse_x: 0.0,
            last_alt: false,
        };
        // cur_beat raw = 2.345 → snap OFF なので 2.345 そのまま
        let (s, e) = compute_loop_drag_endpoints(&ld, 2.345, &SnapConfig::OFF, 50.0);
        assert!((s - 2.345).abs() < 1e-6, "snap OFF で raw 通過: got {s}");
        assert!((e - 12.0).abs() < 1e-6, "End は不変: got {e}");
    }

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
                    m.view,
                    &[],
                    &[],
                    &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
                m.view,
                &[],
                &[],
                &style,
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
}
