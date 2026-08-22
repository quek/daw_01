//! `arrangement` widget — DAW timeline (track header / ruler / lanes / clip drag) を 1 widget で扱う library widget (M9 Phase 45e)。
//!
//! 設計は piano_roll と完全平行 (heavy + cached + overlay / commit-by-release)。
//! S4b (arch-refactor): 旧 `ui/` 汎用 widget から `daw_gui` へ移設し `common::model` 直読・
//! `Edit<AppData>` 直発行に変更 (mirror 型 + `make_edit` 翻訳層を撤去)。
//!
//! - **schema**: view は [`view_build::build`] が `AppData` から毎フレーム構築する
//!   ([`ClipView`] / [`ArrangementTrack`])。`id` は track / clip 内で安定 (index ではない)。
//! - **描画 + drag state machine + hit-test + shortcut + rect select** は widget 内に閉じる。
//!   heavy() ブロック + cached(viewport_key) で背景を粗粒度キャッシュ、selection / drag preview / playhead /
//!   loop band は cached 外で毎フレーム描画。
//! - **Edit 発行はインライン**: 各 interaction site が `ui.push_edit(Edit::mutate(|app| ...))` を
//!   直接発行する (`app.handle_event(AppEvent::X)` へ流す)。
//! - **commit-by-release**: drag 中は library が overlay 描画、release frame で初めて
//!   `MoveClips` / `ResizeClips` / `SetLoopRange` を発行する。drag 中の Mutate Edit は発行しない。
//! - **track header の Rename / Delete** は widget 内蔵せず、`Response.track_header_rects` を返して
//!   app 側で `context_menu_for` 等を重ねて呼ぶ (#005 設計判断)。
//! - **トラック選択トリガ** は track header 全体 click (button hit zone を除く)。

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;

use daw_ui_platform::{CursorIcon, Modifiers};
use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand, TextureHandle, TexturedQuad};

use daw_ui_core::edit::Edit;
use daw_ui_core::id::WidgetId;
use daw_ui_core::scenegraph::hash_inputs;
use common::snap::SnapConfig;
// r.md #38: fade カーブは model の型をそのまま使う (旧 widget ローカル mirror enum +
// 変換関数 2 本は撤去。 arrangement widget は daw_gui 配下なので mirror は不要 =
// アーキテクチャ不変条件 #8)。
use common::model::FadeCurve;
use common::time::{TimeDisplay, TimeMapping};
use daw_ui_core::ui::Ui;
use daw_ui_core::viewport::ViewportState1D;
use daw_ui_core::widgets::heavy::HeavyCtx;
use daw_ui_core::widgets::{muted_dim_fill, push_muted_hatch};
use daw_ui_core::widgets::playhead::draw_playhead_line;
use crate::widgets::ruler_ops::{
    LoopBandHit, LoopDragKind, LoopDragSession, PlayheadDragSession,
    compute_loop_drag_endpoints, draw_loop_band, loop_band_hit_kind,
};
use crate::widgets::time_grid::{BarBeatGridStyle, TimeGridExt, TimeRulerStyle};
use daw_ui_core::widgets::toggle_button::ToggleButtonStyle;
use daw_ui_core::{
    ChannelLayout, MeterScale, SampleSlices, WaveformRenderMode, WaveformSegment, WaveformSource,
    WaveformStyle, WaveformView,
};

use crate::audio_source_cache::AudioSourceBuffer;

use crate::app::{
    AppData, AppEvent, AutomationPointKeyRef, ClipEventRef, ClipRef, FadeEdgeKind, MoveAutomationClipEntry,
    MoveAutomationPointEntry, ResizeAutomationClipEntry,
};

pub(crate) mod view_build;
mod content_build;
mod draw;
use draw::*;
mod geometry;
use geometry::*;
mod render;
mod release;
mod run;
pub use geometry::{pixel_snapped_scroll_beat, view_len_beats};
pub use run::arrangement;

// ============================================================
// Edit 発行の翻訳ヘルパ (S4b: 旧 arrangement_view::make_edit の変換部を widget 内へ)
// widget の安定 id key / widget curve を AppData 内部表現 (index / model curve) へ写す。
// 発火自体は各 interaction site が `Edit::mutate(...)` を直接 push する。
// ============================================================

/// widget `ClipKey` (安定 id) → `AppData` の index ベース `ClipRef`。
pub(crate) fn clip_key_to_ref(app: &AppData, key: ClipKey) -> Option<ClipRef> {
    let t_idx = app.song_doc.song().tracks.iter().position(|t| t.id == key.track)?;
    let c_idx =
        app.song_doc.song().tracks[t_idx].clips.iter().position(|c| c.id == key.clip)?;
    Some(ClipRef { track: t_idx as u32, clip: c_idx as u32 })
}

/// widget `ClipKey` ↔ `common::model::ClipKey` (field 名だけが違う同一の安定 id ペア)。
pub(crate) fn clip_key_to_model(k: ClipKey) -> common::model::ClipKey {
    common::model::ClipKey { track_id: k.track, clip_id: k.clip }
}

pub(crate) fn clip_key_from_model(k: common::model::ClipKey) -> ClipKey {
    ClipKey { track: k.track_id, clip: k.clip_id }
}

/// r.md #35: Shift+click 範囲選択 (長方形ブロック) 用に、 可視 track 上の全 clip を
/// 「行 = 可視 track index / 時間 = clip の開始〜終了拍」 として並べる。 並び順は
/// 描画順 (行 → track 内 clip 順) なので、 `range_block` の結果もその順になる。
pub(crate) fn clip_range_items(visible_tracks: &[ArrangementTrack]) -> Vec<RangeItem<ClipKey>> {
    let mut out = Vec::new();
    for (row, t) in visible_tracks.iter().enumerate() {
        for c in &t.clips {
            out.push(RangeItem {
                key: ClipKey { track: t.id, clip: c.id },
                row: row as i64,
                start: c.start_beat,
                end: c.start_beat + c.len_beats,
            });
        }
    }
    out
}

fn widget_to_model_clip_key(k: AutomationClipKey) -> common::model::AutomationClipKey {
    common::model::AutomationClipKey { track: k.track, lane: k.lane, clip: k.clip }
}

/// widget `AutomationPointKey` ↔ `AutomationPointKeyRef` (AppData 側の同一 id 表現)。
pub(crate) fn point_key_to_model(k: AutomationPointKey) -> AutomationPointKeyRef {
    AutomationPointKeyRef {
        track_id: k.clip.track,
        lane_id: k.clip.lane,
        clip_id: k.clip.clip,
        point_idx: k.point_idx,
    }
}

pub(crate) fn point_key_from_model(k: AutomationPointKeyRef) -> AutomationPointKey {
    AutomationPointKey {
        clip: AutomationClipKey { track: k.track_id, lane: k.lane_id, clip: k.clip_id },
        point_idx: k.point_idx,
    }
}

fn widget_to_model_lane_key(k: AutomationLaneKey) -> common::model::AutomationLaneKey {
    common::model::AutomationLaneKey { track: k.track, lane: k.lane }
}

fn widget_to_model_clip_delta(d: MoveAutomationClipDelta) -> MoveAutomationClipEntry {
    MoveAutomationClipEntry {
        from: widget_to_model_clip_key(d.from),
        to_lane: widget_to_model_lane_key(d.to_lane),
        prev_start_beat: d.prev_start_beat,
        next_start_beat: d.next_start_beat,
    }
}

// ============================================================
// Public types (conversation #005 [Replied] のまま)
// ============================================================

/// clip の identity。track_id + clip_id (どちらも track / track 内 clip で安定)。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ClipKey {
    pub track: u32,
    pub clip: u32,
}

/// M14 Phase 63k (#025): audio clip の inline 編集用フィールド (gain_db)。
/// `ClipView.audio_edit = Some(...)` のとき widget が dB handle line を描画 + 中央帯に
/// drag handler を bind。 MIDI / Vocal clip は `None`。
///
/// r.md #38 で fade は [`ClipView::fades`] へ分離した。 fade は audio 固有ではなく
/// video / image / text の event も同じ形で持つため (= `common::model::EventFade`)、
/// audio 専用フィールドと同居させると 4 content 種別ぶんの重複実装を誘発する。
/// ここに残るのは本当に audio だけの値 (gain_db = dB handle line) のみ。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipViewAudioEdit {
    pub gain_db: f32,
}

/// r.md #38: clip 内 1 event 分の fade 表示情報。 content 種別 (audio / video / image /
/// text) に依らず同じ型で扱う — 4 種とも `event_start_in_clip_beats` /
/// `event_length_beats` / `fade_*_beats` / `fade_*_curve` を同じ意味で持ち、 適用側も
/// 全部 `common::audio_render::fade_curve_at` を通るため。
///
/// caller は `ClipContent::event_fades()` を **そのまま** 写して渡す。 `event_index` は
/// clip 内の event 位置で、 drag の commit 先 (`SetClipFadeBeatsBatch` 等) の宛先になる。
///
/// r.md #68: `fade.start_in_clip_beats` は **content-local 拍** (model の値そのもの)。
/// r.md #44 で一旦「窓ローカル」 (= `content_offset_beats` を引いた値) に畳んでいたが、
/// それだと中身の原点が窓と一緒に動いてしまい、 端 drag の preview で
/// 「クリップ幅 ÷ クリップ長」 のスケールに頼らざるを得なくなる。 換算は
/// [`geometry::ContentMap`] (content 原点 + ビューのズーム) 1 本に集約した。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipEventFade {
    /// clip 内の event index。 fade の編集はこの 1 event だけに効く
    /// (r.md #38 以前は clip 内全 event に broadcast されていて、 掴んだ event と
    /// 書き換わる event が一致しなかった)。
    pub event_index: u32,
    /// 元データ (clip 内位置 / event 長 / fade 長 / curve)。
    pub fade: common::model::EventFade,
}

/// M14 Phase 63k (#025): fade の対象 edge (`In` = event 左端、 `Out` = event 右端)。
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

/// M14 Phase 72 (daw_01 #044) / r.md #68: video / image clip の 1 枚 thumbnail。
///
/// `(width, height)` は texture の native size (= [`daw_ui_renderer::Renderer::texture_size`]
/// と同じ値)。 widget が Renderer 参照を持たない設計と整合させるため caller が同梱で渡す
/// (daw_01 は decode 時の `VideoFrame.width/height` を流用すれば boilerplate ゼロ)。
///
/// r.md #68: `start_in_content_beats` (= この thumbnail が表す event の content-local
/// 開始拍) を持つのは、 **thumbnail も「中身」 だから**。 clip 矩形にフィットさせると
/// 端 drag のたびに絵が動いてしまう (旧 `aspect_fit_rect` は clip 矩形中央に letterbox
/// 配置していたので、 右端を伸ばすとサムネイルが右へ滑っていた)。 中身は content 原点に
/// 固定し、 はみ出す分を clip 矩形で切り抜く。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipThumbnail {
    pub texture: TextureHandle,
    pub width: u32,
    pub height: u32,
    /// この thumbnail が表す event の content-local 開始拍 (= 絵の左端が乗る拍)。
    pub start_in_content_beats: f64,
}

/// 1 つの clip。`Arc<str>` で複数 clip 間の name 共有可能。
#[derive(Clone, Debug)]
pub struct ClipView {
    pub id: u32,
    pub start_beat: f64,
    pub len_beats: f64,
    /// r.md #44 / #68: clip の左端が content の **どの拍に当たるか**
    /// (= `common::model::Clip::content_offset_beats` の素通し)。
    ///
    /// 中身 (波形 / MIDI ノート / fade / thumbnail) の位置は
    /// `start_beat - content_offset_beats` (= content 原点) を原点とする
    /// [`geometry::ContentMap`] 1 本で決まる。 この値を持たずに「窓ローカル座標」 へ
    /// 畳んで渡していた頃は、 端 drag のゴーストが `clip_rect.w / clip_len_beats` を
    /// スケールに使わざるを得ず、 トリムなのに中身が伸縮していた (r.md #68)。
    pub content_offset_beats: f64,
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
    pub audio_edit: Option<ClipViewAudioEdit>,
    /// r.md #38: この clip の各 event の fade。 content 種別 (audio / video / image / text)
    /// に依らず、 空でなければ widget が event ごとに fade を描画し、 handle 領域に
    /// drag handler を bind して `SetClipFadeBeatsBatch` / `SetClipFadeCurveBatch` を
    /// 発行する。 MIDI / Automation clip は空 (fade を持たない)。
    ///
    /// r.md #58: **カーブと掴む正方形で描画層が違う**。 カーブは曲の状態なので cached 層
    /// (`viewport_key` は `fold_arrangement_clip_hash` 経由で fade の全パラメータを含む)、
    /// 掴む正方形はポインタ位置の関数なので cached 外の overlay で、 **マウスが乗っている
    /// clip (+ フェードをドラッグ中の clip) にだけ**描く。 hit zone は hover と無関係に
    /// 常に生きている (`fade_geometry` が描画と hit-test 共通の SSoT)。
    ///
    /// r.md #38 以前は `audio_edit` (= audio の first event) 経由でしか渡らず、
    /// (a) 音声クリップにしか線が出ず、 (b) 複数 event を持つクリップでは 1 本目の
    /// fade しか描かれなかった。
    pub fades: Vec<ClipEventFade>,
    /// M14 Phase 72 (daw_01 #044) / r.md #68: video / image clip 用 thumbnail。
    /// `None` のときは `track.kind == Video` なら [`ArrangementStyle::video_clip_loading`] 単色 rect
    /// 描画、 `Audio` なら field 自体が無視される (= caller が kind と clip 種別を一致させる責任)。
    pub thumbnail: Option<ClipThumbnail>,
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
    /// clip がミュート中なら `true`。 widget は fill alpha を落として
    /// 暗く沈め (`muted_dim_fill`)、 45° の斜線ハッチを重ねて「再生されない」 を示す
    /// (REAPER / Ableton 流)。 caller (daw_01) は `Clip.muted` をそのまま渡す。
    /// `false` のとき描画は既存と完全一致。
    pub muted: bool,
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
    /// できる)。 widget は R button の click で `AppEvent::ToggleTrackArmed(track_id)` を
    /// 発行し、 `track.armed = !track.armed` で反転する (mute / solo と完全同 idiom)。
    pub armed: bool,
    pub clips: Vec<ClipView>,
    /// M10 Phase 47b: track volume (`0.0..=1.0`、`1.0` で unity)。
    /// track header rect 内 buttons の下に horizontal slider band として描画される (`row_h` 余裕がある時のみ)。
    /// 将来 `ClipView.volume` を再導入する場合は `effective = track.volume * clip.volume` の乗算 (DAW 標準)。
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
    /// 重ねる)。 `None` で既存挙動完全互換 (strip 非描画)。 `ClipView.color` の track 版。
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

/// point drag 中の **live 値** (caller がカーソル近くに現値を数値表示する用)。
/// `value_norm` は drag 中の正規化値 (release commit と同じ式で算出)、 `cursor` は ghost dot の
/// 画面座標 (caller はここから少しオフセットして readout を描く)。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct AutomationPointDragInfo {
    pub key: AutomationPointKey,
    pub value_norm: f32,
    pub cursor: (f32, f32),
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
    /// **表示原点**の拍。モデルのスクロール量 ([`Self::scroll_beat_raw`]) を
    /// デバイスピクセル境界へスナップした値 ([`pixel_snapped_scroll_beat`])。
    /// 描画も hit-test もすべてこちらを使う (= 見えている位置がそのまま掴める)。
    ///
    /// r.md #53: スナップしないと追従スクロール中に全クリップの x が毎フレーム任意の
    /// 小数になり、1px 枠線の鮮鋭度が脈動してチラつく。
    pub start_beat: f64,
    /// モデルが持つ**連続値**のスクロール位置 (拍、`ui_prefs.arrange_scroll_beat`)。
    ///
    /// スクロール量を**差分で**更新する経路 (ホイール横スクロール / ドラッグ中の端
    /// 自動スクロール) だけがこちらを基準にする。スナップ済の [`Self::start_beat`] に
    /// 差分を足すと 1px 未満の端数が毎フレーム捨てられ、低速の自動スクロールが
    /// まったく進まなくなる。ブラウザの「layout scroll offset (連続) と visual scroll
    /// offset (ピクセルスナップ済)」と同じ役割分担。
    pub scroll_beat_raw: f64,
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
            scroll_beat_raw: 0.0,
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
/// `app.apply_create_section` / `apply_move_section` / `apply_resize_section` / `apply_duplicate_section`
/// として 1 度発行するだけ)。 破壊的リフロー (clip 分割 / ripple / フルスコープ移動 / 重複正規化) は
/// 全て handler 側が行う。 `ClipView` と同じく `Arc<str>` name を持つので `Copy` ではない。
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
    /// Shift 修飾で開始した端 drag は **time-stretch** (= 内容を
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
    /// r.md #38: 編集対象の event (clip 内 index)。 fade はこの 1 event にだけ効く。
    pub event_index: u32,
    pub edge: FadeEdge,
    pub prev_beats: f64,
    pub next_beats: f64,
}

/// M14 Phase 63k (#025): `SetClipFadeCurve` の delta 1 件 (curve 切替、 release 時に発火)。
/// vertical drag で `next_curve` が `prev → next()` (Linear → Exp → SCurve → Linear) に進む。
#[derive(Clone, Copy, Debug)]
pub struct ClipFadeCurveDelta {
    pub key: ClipKey,
    /// r.md #38: 編集対象の event (clip 内 index)。
    pub event_index: u32,
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

/// 選択 modifier は全選択面 (クリップ / ノート / オートメーション / トラック / セクション /
/// オーディオイベント) 共通なので `crate::widgets::select_modifier` が SSoT。 ここは
/// 既存 caller 向けの re-export (`docs/plan_selection_modifiers.md` §4.2)。
pub use crate::widgets::select_modifier::SelectModifier;
pub(crate) use crate::widgets::select_modifier::{RangeItem, range_block, range_ordered};


/// `Ui::arrangement` の戻り値。
#[derive(Clone, Debug)]
pub struct ArrangementResponse {
    pub hovered_track: Option<u32>,
    /// ポインタ下の clip。 r.md #58 以降、 widget 内部でもフェードの掴む正方形を出す
    /// ゲートに使っている。 **caller はこれ (やそのミラー) を `data_generation` などの
    /// heavy cache キーに混ぜないこと** — 混ぜるとマウスを動かすたびにアレンジ全体が
    /// 再構築される。
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
    /// daw_01 #086: 全 visible automation lane の行 rect (= `for_each_visible_lane` の `body_rect`、
    /// lanes pane 内の縦 `[lane_y, lane_y + height_px)`)。 `automation_clip_rects` と同 semantics
    /// (描画順、 collapsed group / hidden lane / 完全 off-screen は除外)。 caller (daw_01) は `Z`
    /// 縦ズームで「選択 automation clip のレーンを画面いっぱいに framing」 する際の実 y 位置として使う
    /// (= レイアウトを複製せず widget の実 rect を SSoT にする)。
    pub automation_lane_rects: Vec<(AutomationLaneKey, Rect)>,
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
    /// ポインタが今 hover している Arranger section の zone (Move / ResizeLeft / ResizeRight)。
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
    /// 各 visible automation lane の **デフォルト値フィールド** rect
    /// (lane header 内、`automation_lane_header_layout` の `default_field_rect`)。
    /// caller (daw_01) はここに `scrubable_number_at` を overlay して default 値を
    /// ドラッグ/数値入力で編集する (旧スライダー帯は廃止)。 `automation_point_rects`
    /// と同 semantics (描画順 = 上から下、 hidden / collapsed / 行高不足の lane は除外)。
    pub automation_lane_default_rects: Vec<(AutomationLaneKey, Rect)>,
    /// point drag セッション進行中の live 値 (`Some` のとき caller は
    /// `cursor` 近傍に `value_norm` を人間可読単位で表示する)。 release frame では `None`。
    pub automation_point_drag: Option<AutomationPointDragInfo>,
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
            automation_lane_rects: Vec::new(),
            dragging_automation_clip: None,
            automation_lasso_active: false,
            hovered_automation_lane: None,
            hovered_section: None,
            hovered_section_zone: None,
            dragging_section: None,
            section_rects: Vec::new(),
            automation_lane_default_rects: Vec::new(),
            automation_point_drag: None,
        }
    }
}

/// arrangement の見た目スタイル。[`ArrangementStyle::from_theme`] が有効テーマから組み立てる。
///
/// r.md #48: `Default` は持たない。色トークンは runtime 切替可能な
/// [`crate::theme::Theme`] (汎用 [`Palette`](daw_ui_core::theme::Palette) + DAW 固有
/// `DawColors`) が SSoT で、`Default::default()` にすると「どのテーマで描いているか」 を
/// 知らないまま組込みダーク値を焼き込む隠れたグローバル依存になる。
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
    /// 既定は elevation-1 の `panel` で、床 (`window_bg`) の audio 行から 1 段浮いて見える。
    /// lane 描画前に `track.kind == Video` のとき 1 度塗る。
    pub track_background_video: Color,
    /// M14 Phase 72 (daw_01 #044): video clip 内 `thumbnail = None` のときの fallback fill
    /// (= decode 失敗 / loading 中)。 既定は中立な `control` 面で「loading 中」 を控えめに表す。
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
    /// muted clip に重ねる斜線ハッチの色 (半透明)。既定は極性固定の `hatch_ink`
    /// (ユーザー着色 clip の上に乗るのでテーマで反転させない)。
    /// `ClipView.muted == true` のときのみ描画。
    pub clip_muted_hatch_color: Color,
    /// muted ハッチ線の間隔 (px、default 7.0) と線幅 (px、default 1.5)。
    pub clip_muted_hatch_spacing_px: f32,
    pub clip_muted_hatch_width_px: f32,
    /// clip / automation point の **drag ゴースト** (drag 中の半透明
    /// プレビュー) のハイライト塗り色。 かつては選択中 clip 本体の fill にも使って
    /// いたが、 黄色など同系色の clip だと「選択 = 黄塗り」 が clip 本来の色と衝突して
    /// 選択状態が判別できなかった (#73)。 選択表示は fill を潰さず `push_selection_ring`
    /// の 2 重リング (明 + 暗) で示すようにし、 この色は drag ゴースト専用に絞った。
    pub clip_selected_fill: Color,
    /// 選択リングの **外側 (明)** 線。 暗い lane 背景に対して光る。
    pub clip_selected_border: Color,
    /// 選択リングの **内側 (暗)** 線。 黄 / 白など明るい fill に対して
    /// コントラストする。 `clip_selected_border` (明) と対で、 fill 色に依らず
    /// どんな clip でも選択枠が視認できる 2 重リングを成す。
    pub clip_selected_border_inner: Color,
    pub clip_selected_border_w: f32,
    /// clip 名 + link glyph の **明インク側** (暗い fill の上)。 clip の塗りはユーザーが選ぶ
    /// 可変色なので、テーマで反転する `text` ではなく極性固定の `ink_on_dark` を使う。
    pub clip_text_color: Color,
    /// M14 Phase 89 (daw_01 #060): auto-contrast が「暗い文字」を選んだときの色 (明るい fill 上)。
    /// `clip_text_color` (明インク、暗い fill 上) と対をなす極性固定の `ink_on_bright`。
    pub clip_text_color_dark: Color,
    /// M14 Phase 89 (daw_01 #060): clip / video clip の名前 + link glyph の色を、 widget が実際に
    /// 塗る fill の WCAG relative luminance から自動選択するか (default true)。 `true` のとき明るい
    /// fill には `clip_text_color_dark`、 暗い fill には `clip_text_color` を選びコントラストを最大化
    /// する (share clip の半透明 fill は lane bg と合成した実効色で判定)。 `false` で常に
    /// `clip_text_color` 固定 (opt-out)。
    pub clip_auto_contrast_text: bool,
    pub clip_text_size: f32,
    /// 選択トラックの行背景 (header + lanes の全幅)。 `panel_raised` を `accent` 方向へ
    /// 22% だけ寄せた派生で、**どちらのテーマでも「選択行が非選択行より目立つ」** が成り立つ:
    /// ダークでは床より明るい raised が更に accent の青に寄って浮き、ライトでは最も白い
    /// raised が accent の濃青に寄って沈む — どちらも非選択行との差が accent 側に開く。
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
    /// badge glyph の color。 badge は ghost の明るい緑 / 橙 fill の上に乗るので、
    /// 極性固定の暗インク (`ink_on_bright`)。
    pub clip_clone_badge_color: Color,
    // ---- M14 Phase 63e (#019) / Phase 114 (#086): share group (linked clip group) 描画パラメータ ----
    /// share clip の name 左に描く link glyph (default = `'⇌'` U+21CC)。 font に存在しない場合は
    /// caller 側で ASCII (`'~'` 等) に差し替える。 M14 Phase 114 (#086) で `share_group_color` の hue 値で
    /// fill / border を塗る挙動を撤去したため、 共有マークはこの glyph + 下記 active 強調のみが担う。
    pub share_group_link_glyph: char,
    // ---- M14 Phase 96 (daw_01 #068) / Phase 114 (#086): 共有グループ連動ハイライト (active group 強調) ----
    /// M14 Phase 114 (daw_01 #086): `ClipView.in_active_group == true` の clip に重ねる強調色
    /// (glow wash + bright thick border 共通)。 旧 hue 由来から **identity-neutral な中立色** に変更
    /// (clip fill が user 指定色になったので hue wash だと喧嘩する、 hover 中は 1 グループのみ強調 = 色で
    /// 区別する必要が無い)。 既定は DAW トークンの `highlight_ring` (ダークでは明中立色、ライトでは
    /// 暗中立色 = 明るい clip の上でも枠が沈まない)。 glow wash はこの RGB を
    /// `share_group_active_glow_alpha` で、 border はこの色を不透明で描く。
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
    /// audio_edit が Some の clip に重ねる dB handle line の色。 波形 / ユーザー着色 clip の
    /// 上に乗るので極性固定の明インク (`ink_on_dark`) を半透明で。
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
    /// fade envelope (clip の中身領域上端から fade 末尾まで) と grip の描画色。
    /// **暗い clip 色の上で使う明色側** (極性固定の `ink_on_dark` を半透明で)。
    pub audio_fade_overlay_color: Color,
    /// r.md #46: 明るい clip 色の上で使う暗色側 (`audio_fade_overlay_color` と対)。
    /// clip 名の `clip_text_color` / `clip_text_color_dark` と同じ auto-contrast 2 択で、
    /// どちらを前景にするかは clip の実塗り色の WCAG 輝度で決まる。 選ばれなかった方は
    /// 1 px の裏打ちとして敷かれるので、波形の上でも fade の縁が立つ。
    pub audio_fade_overlay_color_dark: Color,
    /// fade envelope 線の太さ (default 1.0 px)。
    pub audio_fade_overlay_width_px: f32,
    /// audio clip 内の **スライス区切り線** (event の trigger 境界) の前景色。
    /// r.md #48: 波形描画は `HeavyCtx::palette()` (汎用パレット) しか手元に無い一方、
    /// この色は DAW 固有トークン (`slice_divider`) なので style 経由で渡す。 裏打ちの暗線は
    /// 汎用 `scrim` を widget が直接引く (2 層で波形の上でも縁が立つ)。
    pub slice_divider_color: Color,
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
    /// ghost label の color。 clip / 波形の上に直接乗るので極性固定の明インク (`ink_on_dark`)。
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
    /// M14 Phase 63n-9 (#033): tension/bend handle の border。 明るい handle fill の上に乗る
    /// 極性固定の暗インク (`ink_on_bright`) で handle を背景から分離する。
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
    /// M14 Phase 63n-8 (#033): selected automation point の fill。 lane 識別色で塗られた clip の上に
    /// 乗る (= 背景が可変) ので、極性固定の明インク (`ink_on_dark`) で curve 色から大きく外して
    /// selected を強調する。 lane が disabled でも変えない (= selected を見失わないため)。
    pub automation_point_selected_fill: Color,
    /// M14 Phase 63n-8 (#033): selected automation point の border。 fill と同じ `ink_on_dark` (上書き
    /// fill と同色 + `automation_point_radius_px` の border_w で枠線扱い)、 widget 側で `border_w = 1.5`
    /// (= 通常 1.0 から +50%) で「枠線が太い」 visual を作る。
    pub automation_point_selected_border: Color,
    /// M14 Phase 63n-8 (#033): lasso 矩形 (空き automation lane zone での drag) の fill (半透明)。
    /// 既定は `accent` の 12% alpha (MIDI rect select と同じ選択の色言語)。 widget は drag 中
    /// cached 外で overlay 描画する。
    pub automation_lasso_fill: Color,
    /// M14 Phase 63n-8 (#033): lasso 矩形の border。 既定は `accent` の 60% alpha + 1px。
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
    /// lane header の default value 数値入力フィールドの縦幅 (px)。 default 18.0
    /// (旧スライダー帯 `automation_default_band_h` を置換、 scrubable_number_at が読める高さ)。
    pub automation_default_field_h: f32,
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
    /// master row header の背景塗り (track.color の代わりに使う中立面、 既定は `control_active`)。
    /// daw_01 #034 §B 仕様。 track 色と differentiate しつつ、床から最も離れたコントロール面を
    /// 使うことで「特殊だが視認可能」 をどちらのテーマでも保つ。
    pub master_row_color: Color,
    /// master row header の "Master" label に使う font size (default = `track_text_size`)。
    /// 通常 track と並んだとき揃って見えるよう同サイズが既定。
    pub master_row_label_size: f32,
    /// master row header の "Master" label の文字色 (default = `track_text_color`)。
    pub master_row_label_color: Color,
}

impl ArrangementStyle {
    /// 有効テーマ (汎用 `Palette` + DAW 固有 `DawColors`) から arrangement のスタイルを組み立てる。
    ///
    /// 極性の原則 (r.md #48):
    /// - **クローム面の上**に乗るもの (track header の文字 / ruler ラベル / lane header / grid) は
    ///   テーマ従属の `text` / `text_dim` / `grid_line`。
    /// - **ユーザー着色 clip・波形・映像 thumbnail の上**に乗るもの (clip 名 / fade 線 / dB handle /
    ///   選択リング / ghost badge) は極性固定インク (`ink_on_dark` / `ink_on_bright` /
    ///   `selection_ring_*` / `hatch_ink`)。ここを `text` にするとライトテーマで clip 名や fade の
    ///   線が消える。
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn from_theme(theme: &crate::theme::Theme) -> Self {
        let p = &*theme.core;
        let d = &theme.daw;
        // M/S/R は共通の小型トグル (角丸 3px / 11px 文字)。off 面・枠・OFF 文字色は
        // パレット既定 (`from_palette`) をそのまま使い、ON 色だけ DAW の意味色で差し替える。
        let mute_button = ToggleButtonStyle {
            on_color: d.record,
            radius: 3.0,
            font_size: 11.0,
            ..ToggleButtonStyle::from_palette(p)
        };
        let solo_button = ToggleButtonStyle {
            on_color: d.solo,
            radius: 3.0,
            font_size: 11.0,
            // r.md #22: ON 色は高輝度の黄 (`solo`)。 明インクでは「S」が埋もれるので、 黄背景には
            // 極性固定の暗インク (`ink_on_bright`) を敷く (mixer/metronome の Solo と同じ)。
            // M/R は ON 色が赤で明インクが読めるため `None` (= `text_color`) のまま。
            on_text_color: Some(p.ink_on_bright),
            ..ToggleButtonStyle::from_palette(p)
        };
        // M14 Phase 68 (#040): R button (Record-arm)。 active = 鮮やかな赤、
        // off = mute / solo と同じ中立コントロール面。
        let armed_button = ToggleButtonStyle {
            on_color: d.record,
            radius: 3.0,
            font_size: 11.0,
            ..ToggleButtonStyle::from_palette(p)
        };
        Self {
            bg: p.window_bg,
            header_bg: p.header,
            track_color_strip_w: 4.0,
            ruler_bg: p.header,
            // M14 Phase 72 (#044): elevation-1 面で audio 行 (床) と視覚区別。
            track_background_video: p.panel,
            // M14 Phase 72 (#044): 中立コントロール面で「loading 中」 を控えめに表現。
            video_clip_loading: p.control,
            bar_line: p.grid_line_strong,
            beat_line: p.grid_line,
            bar_line_width_px: 1.5,
            beat_line_width_px: 1.0,
            lane_line: p.grid_line,
            lane_line_width_px: 1.0,
            clip_default_fill: d.clip_default,
            clip_border: d.clip_default_border,
            clip_border_w: 1.0,
            clip_radius: 3.0,
            clip_muted_hatch_color: p.hatch_ink,
            clip_muted_hatch_spacing_px: 7.0,
            clip_muted_hatch_width_px: 1.5,
            clip_selected_fill: p.selection_warm,
            // 選択の 2 重リング。任意色の clip の上で必ず立つ極性固定ペア (外 = 明 / 内 = 暗)。
            clip_selected_border: p.selection_ring_outer,
            clip_selected_border_inner: p.selection_ring_inner,
            clip_selected_border_w: 2.0,
            // clip 名は user 着色 clip / 波形 / thumbnail の上。テーマ従属の `text` ではなく
            // 極性固定インクの 2 択 (`clip_auto_contrast_text` が実塗り色の輝度で選ぶ)。
            clip_text_color: p.ink_on_dark,
            clip_text_color_dark: p.ink_on_bright,
            clip_auto_contrast_text: true,
            clip_text_size: 11.0,
            // 選択トラックは行全体 (header + lanes) を塗るため、 full accent だと clip を
            // 塗り潰す。 panel_raised を accent 方向へ少しだけブレンドした控えめな選択色。
            // ライトでも raised (最も白い面) が accent の濃青へ寄るので、 非選択行との差は開く。
            track_selected_bg: p.panel_raised.lerp(p.accent, 0.22),
            track_text_color: p.text,
            track_text_size: 12.0,
            playhead_color: d.playhead,
            playhead_width_px: 2.5,
            loop_band: p.loop_band.with_alpha(0.20),
            loop_handle: p.loop_band,
            loop_handle_w: 2.0,
            arranger_lane_bg: p.header,
            arranger_label_color: p.text_dim,
            arranger_preview_fill: p.accent.with_alpha(0.25),
            resize_handle_px: 4.0,
            mute_button,
            solo_button,
            armed_button,
            reorder_drop_indicator: p.loop_band,
            reorder_drop_indicator_h: 2.0,
            reorder_drag_alpha: 0.6,
            // group 行に accent を薄く乗せて nest 先を示す (drop indicator と同じ選択の色言語)。
            reorder_group_highlight: p.accent.with_alpha(0.22),
            track_volume_band_h: 4.0,
            // 溝は「沈んで見える」 ことが意味なので、 header 面の上に極性固定の暗い scrim を敷く
            // (fill = accent との明度差がどちらのテーマでも保たれる)。
            track_volume_band_track: p.scrim.with_alpha(0.45),
            track_volume_band_fill: p.accent,
            ruler_label_color: p.text_dim,
            indent_px: 16.0,
            disclosure_color: p.text_dim,
            // M14 Phase 63e (#019): clone ghost (Ctrl / Ctrl+Shift) — 緑系 / 橙系で 3 種視覚区別。
            // selected fill (selection_warm = 黄系) と色相を分けて drag 中に「同じ ghost
            // にしか見えない」 状態を回避。
            clip_clone_linked_fill: d.ghost_linked.with_alpha(0.55),
            clip_clone_linked_border: d.ghost_linked,
            clip_clone_indep_fill: d.ghost_independent.with_alpha(0.55),
            clip_clone_indep_border: d.ghost_independent,
            clip_clone_badge_size: 11.0,
            // badge は明るい緑 / 橙の ghost fill の上 → 極性固定の暗インク。
            clip_clone_badge_color: p.ink_on_bright,
            // M14 Phase 114 (#086): share_group_color の hue 値で塗る挙動を撤去したため、 共有マークは
            // link glyph (⇌) + 下記 active 強調のみ。
            share_group_link_glyph: '⇌',
            // M14 Phase 96 (daw_01 #068) / Phase 114 (#086): active group 強調 — identity-neutral な
            // 中立色 (`highlight_ring`。 ダーク = 明中立 / ライト = 暗中立)。 border は
            // clip_selected_border_w (2.0) より太い 2.5、 glow wash は名前可読性を保つ 0.22 alpha
            // (= `highlight_glow` と同じ alpha)。 selection の黄塗りとは別レイヤの強調。
            share_group_active_color: d.highlight_ring,
            share_group_active_border_w: 2.5,
            share_group_active_glow_alpha: 0.22,
            // M14 Phase 63k (#025): audio clip 編集 default — Bitwig spec §3.5/§3.6 と整合。
            // dB handle / fade envelope は波形と clip 色の上に乗るので極性固定インク。
            // ±4 px hit 帯、 端から 24 px margin、 0.25 dB/px、 ±24 dB 範囲、 12×12 の fade grip、
            // sticky 閾値 10 px (要望文 §3.2)。
            audio_db_handle_color: p.ink_on_dark.with_alpha(0.55),
            audio_db_handle_width_px: 1.5,
            audio_db_handle_band_h: 8.0,
            audio_db_handle_x_margin: 24.0,
            audio_db_pixels_per_db: 0.25,
            audio_db_range_db: 24.0,
            audio_fade_corner_size_px: 12.0,
            audio_fade_overlay_color: p.ink_on_dark.with_alpha(0.65),
            audio_fade_overlay_color_dark: p.ink_on_bright.with_alpha(0.75),
            audio_fade_overlay_width_px: 1.0,
            // 旧実装は選択色 (`selection_warm`) を借りていた。 DAW 専用トークンに分離済み
            // (値はダークで同一なので見た目は不変)。
            slice_divider_color: d.slice_divider.with_alpha(0.85),
            audio_min_clip_w_for_handles_px: 32.0,
            audio_fade_sticky_threshold_px: 10.0,
            audio_ghost_label_size: 11.0,
            audio_ghost_label_color: p.ink_on_dark,
            // M14 Phase 63n-1 (#028): automation lane defaults — Bitwig "Volume" lane の見た目に近づける。
            automation_lane_header_min_w_px: 80.0,
            automation_lane_bg: p.window_bg,
            automation_lane_disabled_color: p.text_dim.with_alpha(0.65),
            automation_curve_line_width_px: 1.5,
            automation_point_radius_px: 4.0,
            // M14 Phase 63n-9 (#033): tension/bend handle は暖色 (`selection_warm`。 lane.color と差別化して
            // 「触ると curve param が変わる handle」 を明示)、 size は selection dot と同 4.0。
            automation_curve_param_handle_radius_px: 4.0,
            automation_curve_param_handle_fill: p.selection_warm,
            automation_curve_param_handle_border: p.ink_on_bright,
            automation_curve_param_handle_offset_px: 10.0,
            automation_curve_param_preview_color: p.selection_warm,
            // M14 Phase 63n-8 (#033): selected point は半径 +25% (= 通常 4 → 5)、 fill / border 共に
            // 明インクで「明らかに大きく / 明るく見える」 を実現 (daw_01 #033 §D 仕様)。
            // lane disabled でも色維持。
            automation_point_radius_selected_px: 5.0,
            automation_point_selected_fill: p.ink_on_dark,
            automation_point_selected_border: p.ink_on_dark,
            // M14 Phase 63n-8 (#033): lasso 矩形は accent で MIDI rect_select と視覚的に共通の言語、
            // ただし fill alpha 12% で透明感を強め overlay と分かりやすく。
            automation_lasso_fill: p.accent.with_alpha(0.12),
            automation_lasso_border: p.accent.with_alpha(0.60),
            automation_default_line_color: p.grid_line.with_alpha(0.18),
            automation_default_line_width_px: 1.0,
            automation_clip_v_pad_px: 6.0,
            automation_clip_default_len_beats: 4.0,
            automation_default_field_h: 18.0,
            automation_lane_resize_handle_px: 4.0,
            header_resize_handle_px: 8.0,
            automation_lane_min_height_px: 30,
            automation_lane_max_height_px: 2000,
            automation_disclosure_size: 12.0,
            automation_lane_icon_size: 12.0,
            automation_lane_text_color: p.text,
            // M14 Phase 63n-10 (#034): master row default (中立面 + track と同 font / 色)。
            master_row_color: p.control_active,
            master_row_label_size: 12.0,
            master_row_label_color: p.text,
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
    /// drag 開始時の gain 値 (`Gain` の commit / ghost preview に使う)。
    anchor_gain_db: f32,
    /// r.md #38: drag 開始時の fade 値と **その event の識別**。 `Gain` では未使用。
    /// fade の commit 先は clip ではなくこの event 1 つ
    /// (以前は clip 内全 event に broadcast されていた)。
    anchor_fade: Option<ClipEventFade>,
    /// drag 開始時の clip rect (release 時にも参照、 view scroll 中も安定 — track 並び替えや
    /// scroll で「rect が動いて」 も anchor の dB 0 ライン位置を変えない)。
    clip_rect_anchor: Rect,
    /// drag 開始時の content 写像 (content-local 拍 → 画面 x)。 ghost 描画で clip rect から
    /// event 矩形を切り出すのに使う。 **fade 長の clamp には使わない** (それは event 長 =
    /// `anchor_fade.fade.len_beats`)。
    ///
    /// r.md #68: 以前は `clip_len_beats_anchor: f64` を持ち、 `clip_rect.w / clip_len` を
    /// スケールにしていた。 それだと同じ clip の上で波形 (インセット込みの分母) と
    /// fade (インセット無しの分母) が最大 2px ずつずれる。 写像そのものを anchor する。
    content_map_anchor: ContentMap,
    /// r.md #46: drag 開始時の clip 実塗り色。 ghost の fade envelope も base 描画と
    /// 同じ auto-contrast で色を選ぶ (単層の固定色だと明るい clip 上で消える)。
    clip_bg_anchor: Color,
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

/// M14 Phase 63n-5 (#030): lane 下端 splitter drag session (lane height 変更)。
/// per-frame `SetLaneHeight` emit + release で
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
    // (r.md #35) track の選択アンカーは widget state から `SelectionState.track_anchor` へ移設。
    // 全選択面 (clip / note / automation / section / audio event) で同じ場所・同じ更新規則に
    // 揃えるため (`docs/plan_selection_modifiers.md` §4.3)。
    /// edge auto-scroll の移動量ゲート用 press 位置 (primary press 時の screen pos)。
    /// 端スクロールは「press からここまでの移動が `ACTIVATE_PX` 以上」 のときのみ発火させ、端近くの
    /// clip を click-and-hold しただけで view が動くのを防ぐ (実 DAW は実ドラッグで初めて端スクロール)。
    edge_scroll_press: Option<(f32, f32)>,
    /// 直近 primary press 時の modifier snapshot。 track header の選択更新
    /// (release frame で確定する click) が読む — release frame の `pointer.modifiers`
    /// 生読みは「ModifiersChanged が MouseInput(Released) より先に届く」 race で
    /// Ctrl/Shift+click が Single に化ける (drag session の `last_*` と同 class)。
    press_modifiers: Modifiers,
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
            let next = (ad.anchor_gain_db + delta_db).clamp(-range, range);
            if (next - ad.anchor_gain_db).abs() < 1e-3 {
                None
            } else {
                Some(AudioDragOutcome::Gain { next_db: next })
            }
        }
        AudioDragKind::FadeIn | AudioDragKind::FadeOut => {
            let lock = ad.locked_horizontal?;
            let anchor = ad.anchor_fade?;
            let edge = match ad.kind {
                AudioDragKind::FadeIn => FadeEdge::In,
                AudioDragKind::FadeOut => FadeEdge::Out,
                AudioDragKind::Gain => unreachable!(),
            };
            if lock {
                // length 編集: fade_in は dx 正で増、 fade_out は dx 負で増 (event 右端から内側に伸びる)。
                let raw_delta_beats = f64::from(dx) * beat_per_px;
                let signed = match edge {
                    FadeEdge::In => raw_delta_beats,
                    FadeEdge::Out => -raw_delta_beats,
                };
                let prev = match edge {
                    FadeEdge::In => anchor.fade.fade_in_beats,
                    FadeEdge::Out => anchor.fade.fade_out_beats,
                };
                // r.md #38: 上限は **event 長**。 音 (`audio_clip_renderer`) / 映像 / 画像 /
                // 字幕はどれも event 長基準で fade を掛けるので、 clip 長で clamp すると
                // clip より短い event で「絵と音が合わない範囲まで伸ばせる」 状態になる。
                let max_beats = anchor.fade.len_beats.max(0.0);
                let next = (prev + signed).clamp(0.0, max_beats);
                if (next - prev).abs() < 1e-6 {
                    None
                } else {
                    Some(AudioDragOutcome::FadeLength { edge, next_beats: next })
                }
            } else {
                // curve 切替: dy 方向問わず常に次 curve に順送り (1 release で 1 段階)。
                let prev_curve = match edge {
                    FadeEdge::In => anchor.fade.fade_in_curve,
                    FadeEdge::Out => anchor.fade.fade_out_curve,
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

/// arrangement の active drag session について edge auto-scroll の有効軸
/// `(enable_x, enable_y)` を返す。`None` = auto-scroll 非対象 (local 操作 / drag 無し)。
/// 横 = 時間軸 (beat)、縦 = track 方向 (track_top)。clip resize や section / ruler は横のみ、
/// track 並べ替えは縦のみ。
fn arrangement_edge_scroll_axes(state: &ArrangementState) -> Option<(bool, bool)> {
    if let Some(nd) = state.clip_drag.as_ref() {
        // Move は横 + 縦 (track 跨ぎ)、 Resize は横のみ (縦に動かさない)。
        return Some(match nd.kind {
            ClipDragKind::Move => (true, true),
            ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => (true, false),
        });
    }
    if state.section_drag.is_some() {
        return Some((true, false)); // section (リージョン) は arranger lane 上の横移動のみ。
    }
    if state.automation_point_drag.is_some() {
        return Some((true, false)); // automation point: 横 (time)。縦は値で scroll 軸でない。
    }
    if let Some(acd) = state.automation_clip_drag.as_ref() {
        return Some(match acd.kind {
            ClipDragKind::Move => (true, true), // lane 跨ぎ move は縦も。
            ClipDragKind::ResizeLeft | ClipDragKind::ResizeRight => (true, false),
        });
    }
    if state.automation_lasso_drag.is_some() {
        return Some((true, true)); // automation 範囲選択は両軸。
    }
    if state.track_reorder.is_some() {
        return Some((false, true)); // track 並べ替えは縦のみ (横は indent 相対)。
    }
    if state.loop_drag.is_some() || state.playhead_drag.is_some() {
        return Some((true, false)); // ruler の loop / playhead seek は横のみ。
    }
    None
}

/// active drag session の anchor を実スクロール px ぶん逆方向に shift して、掴んでいる
/// 対象がカーソルに追従し続けるようにする (= content space delta)。相対 delta で位置を決める session
/// (clip / section / automation point/clip / lasso) のみ対象。track 並べ替え (live 行 top 再解決) と
/// ruler の loop/playhead (絶対 px→beat 再解決) は自動追従するので shift しない。`dy` は縦スクロール
/// 非対象 session では 0 が渡る (= 無害)。
fn arrangement_compensate_anchor(state: &mut ArrangementState, dx: f32, dy: f32) {
    if let Some(nd) = state.clip_drag.as_mut() {
        nd.anchor_mouse.0 -= dx;
        nd.anchor_mouse.1 -= dy;
    }
    if let Some(sd) = state.section_drag.as_mut() {
        sd.anchor_mouse.0 -= dx;
    }
    if let Some(ad) = state.automation_point_drag.as_mut() {
        ad.anchor_mouse.0 -= dx;
    }
    if let Some(acd) = state.automation_clip_drag.as_mut() {
        acd.anchor_mouse.0 -= dx;
    }
    if let Some(ls) = state.automation_lasso_drag.as_mut() {
        ls.anchor.0 -= dx;
        ls.anchor.1 -= dy;
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

/// clip Move の track 移動量 (visible 行数)。 press 時と現在の pointer y をそれぞれ
/// `tops` (可変行高の prefix sum) 上の visible 行 index に解決して差を取る。
/// overlay (drag ghost) と release commit が共有する。
///
/// 旧実装の「`dy / view.track_row_h` の均一換算」は per-track 行高 override /
/// automation lane 展開で行高が非等間隔になると、 カーソルの指す行と別の行に
/// ghost / commit が着地していた (automation clip drag の y→lane 解決と非対称)。
/// lanes 外へはみ出した y は端の行に clamp (従来の clamp 挙動を維持)。
fn compute_clip_drag_track_delta(nd: &ClipDragSession, tops: &[f32]) -> i32 {
    let idx_at = |y: f32| -> Option<usize> {
        if tops.len() < 2 {
            return None;
        }
        if y < tops[0] {
            return Some(0);
        }
        // tops は単調増加。 y >= tops[last] は最終行に clamp。
        let i = tops.partition_point(|&t| t <= y);
        Some((i - 1).min(tops.len() - 2))
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    match (idx_at(nd.anchor_mouse.1), idx_at(nd.last_mouse.1)) {
        (Some(pressed), Some(current)) => current as i32 - pressed as i32,
        _ => 0,
    }
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
/// の 0.001%。 `ClipView` は gui_01 公開型なので widget が hash する権利あり (no-Clone
/// 不変条件にも触れない、 `u32`/`f64` は Copy)。
#[allow(clippy::too_many_lines)]
fn fold_arrangement_clip_hash(tracks: &[ArrangementTrack]) -> u64 {
    const PRIME: u64 = 0x100_0000_01B3; // FNV-1a 64bit prime
    let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a 64bit offset basis
    for t in tracks {
        h ^= u64::from(t.id);
        h = h.wrapping_mul(PRIME);
        // per-track 行高 override (review): cached 層の全 y 配置が依存する。 抜けると
        // 行リサイズ drag 中に cached (クリップ / 行背景 / grid) が古い位置で凍る
        // (lane.height_px は fold 済みなのに row_h だけ欠けていた非対称)。
        h ^= t.row_h.map_or(u64::MAX, u64::from);
        h = h.wrapping_mul(PRIME);
        for c in &t.clips {
            h ^= u64::from(c.id);
            h = h.wrapping_mul(PRIME);
            h ^= c.start_beat.to_bits();
            h = h.wrapping_mul(PRIME);
            h ^= c.len_beats.to_bits();
            h = h.wrapping_mul(PRIME);
            // r.md #68: content 原点 (= start - offset) が cached 層の中身描画
            // (thumbnail / fade カーブ) の x 写像を決めるので、 offset 単独の変化
            // (bounce / paste / audio editor の窓調整) でも cache を無効化する。
            h ^= c.content_offset_beats.to_bits();
            h = h.wrapping_mul(PRIME);
            // muted は cached 内の fill dim + 斜線ハッチ描画に効く (review — widget 契約
            // #011 「clip 個別変化は widget が吸収」 に合わせ caller hash に頼らない)。
            h ^= u64::from(c.muted);
            h = h.wrapping_mul(PRIME);
            // video thumbnail の decode 完了 (None→Some) / 差し替えを検知 (review)。
            // handle raw 値 + サイズで十分 (内容は immutable texture)。
            let thumb_marker = c.thumbnail.map_or(u64::MAX, |t| {
                let mut a: u64 = 0x7157_B00B_5EED_F00D;
                a ^= u64::from(t.texture.raw_id().get());
                a = a.wrapping_mul(PRIME);
                a ^= (u64::from(t.width) << 32) | u64::from(t.height);
                a = a.wrapping_mul(PRIME);
                // r.md #68: 絵の左端が乗る拍 (= 描画位置) も cache key の一部。
                a ^= t.start_in_content_beats.to_bits();
                a
            });
            h ^= thumb_marker;
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
                    a
                }
            };
            h ^= audio_marker;
            h = h.wrapping_mul(PRIME);
            // r.md #38: fade は content 種別に依らず `fades` (per-event) に移したので、
            // hash も per-event で混ぜる。 混ぜ忘れると fade を編集しても cached が
            // 再構築されず「線が更新されない」 (#011 と同根の cache miss 不在問題)。
            let curve_code = |c: FadeCurve| match c {
                FadeCurve::Linear => 0_u64,
                FadeCurve::Exponential => 1,
                FadeCurve::SCurve => 2,
            };
            h ^= c.fades.len() as u64;
            h = h.wrapping_mul(PRIME);
            for f in &c.fades {
                h ^= u64::from(f.event_index);
                h = h.wrapping_mul(PRIME);
                h ^= f.fade.start_in_clip_beats.to_bits();
                h = h.wrapping_mul(PRIME);
                h ^= f.fade.len_beats.to_bits();
                h = h.wrapping_mul(PRIME);
                h ^= f.fade.fade_in_beats.to_bits();
                h = h.wrapping_mul(PRIME);
                h ^= f.fade.fade_out_beats.to_bits();
                h = h.wrapping_mul(PRIME);
                h ^= curve_code(f.fade.fade_in_curve);
                h = h.wrapping_mul(PRIME);
                h ^= curve_code(f.fade.fade_out_curve);
                h = h.wrapping_mul(PRIME);
            }
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


// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests;
