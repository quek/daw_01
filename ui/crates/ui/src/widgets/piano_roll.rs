//! `piano_roll` widget — 100k notes 級 piano roll の library widget (M9 Phase 41e)。
//!
//! 設計:
//! - **Note schema** (id / start_beat / len_beats / pitch / velocity) は library 公開型。
//!   id は `u32` で生成時に割り当て、move/delete でも不変 (multi-select identity 安定)。
//! - **描画 + drag state machine + hit-test + shortcut + rect select** は widget 内に閉じる。
//!   heavy() ブロック + cached(viewport_key) で背景描画を粗粒度キャッシュ。
//! - **Edit 構築は callback** で user に委譲 (`make_edit: Fn(PianoRollEditRequest) -> Edit<M>`)。
//!   widget 自身は ユーザ Model 型を知らないので no-Clone 不変条件と整合する。
//! - **drag 中は library が overlay 描画、release frame で初めて `PianoRollEditRequest::Move`
//!   / `Resize` を発行** (commit-by-release pattern)。drag 中の Mutate Edit は発行せず、
//!   user の Model.notes は release まで不変。これにより `PianoRollEditRequest` は 5 variants
//!   で完結し、Mutate/Undoable 区別の boilerplate を排除。
//! - **state 配置**: drag anchor / pending_click は内部 `WidgetState` (ephemeral)、
//!   selected_note_ids は外部 `&[NoteId]` (immutable borrow、Model 側 single source of truth)。
//!   selection 変更は `PianoRollEditRequest::Select` Edit を push_edit で発行し、frame 末で
//!   model に apply される (= 次フレームで反映)。`UiHost::frame` の closure が `&M` 制約
//!   のため `&mut` borrow は不可、push_edit ベースが no-Clone 不変条件と整合する設計。
//!
//! # 使い方 (example/piano_roll/src/main.rs を参照)
//!
//! ```ignore
//! use daw_ui_core::{Note, NoteId, PianoRollEditRequest, PianoRollStyle, PianoRollView};
//!
//! ui.piano_roll(
//!     id, rect,
//!     &model.notes, view, &model.selected_note_ids,
//!     &PianoRollStyle::default(),
//!     |req| match req {
//!         PianoRollEditRequest::Add(notes)        => make_add_notes_edit(notes),
//!         PianoRollEditRequest::Delete(notes)     => make_delete_notes_edit(notes),
//!         PianoRollEditRequest::Move(d)           => make_move_notes_edit(d),
//!         PianoRollEditRequest::Resize(d)         => make_resize_notes_edit(d),
//!         PianoRollEditRequest::Select { prev, next } => make_select_notes_edit(prev, next),
//!     },
//! );
//! ```

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::Arc;

use daw_ui_platform::CursorIcon;
use daw_ui_renderer::{theme, Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::snap::SnapConfig;
use crate::time::{TimeDisplay, TimeMapping};
use crate::ui::Ui;
use crate::viewport::ViewportState1D;
use crate::widgets::playhead::draw_playhead_line;
use crate::widgets::ruler_ops::{
    LoopBandHit, LoopDragKind, LoopDragSession, PlayheadDragSession,
    compute_loop_drag_endpoints, loop_band_hit_kind,
};
use crate::widgets::time_grid::{BarBeatGridStyle, SubGridSpec, TimeRulerStyle};

// ============================================================
// Public types
// ============================================================

/// note 識別子。生成時に割り当て、編集中も不変 (multi-select identity 安定)。
pub type NoteId = u32;

/// piano roll の 1 note (library 公開型)。
///
/// schema (M9 Phase 44c で f64 化 + lyric 追加):
/// - `id: NoteId` — 生成時に割り当て (`PianoRollModel::next_note_id` 等)、move/delete でも不変
/// - `start_beat: f64` — 開始位置 (拍単位、0.0 = 最初の拍)。f64 で長尺 song / sample 精度を確保
/// - `len_beats: f64` — 長さ (拍単位)
/// - `pitch: u8` — MIDI 0..127 (実用 36..96 が C2..C7)
/// - `velocity: u8` — MIDI 0..127 (色濃度に使う、`PianoRollStyle::note_fill_fn` で Color に変換)
/// - `lyric: Option<Arc<str>>` — singing synthesis 用歌詞 (VOICEVOX 等)、note 上に表示。
///   `None` なら lyric 表示せず。`Arc<str>` で多数 note 間の文字列共有を可能にする。
///
/// `Copy` ではない (`Arc<str>` のため `Clone` のみ)。closure capture / sort では参照渡しで対応。
#[derive(Clone, Debug)]
pub struct Note {
    pub id: NoteId,
    pub start_beat: f64,
    pub len_beats: f64,
    pub pitch: u8,
    pub velocity: u8,
    pub lyric: Option<Arc<str>>,
    /// note がミュート中なら `true`。 widget は note fill を暗く沈め
    /// (`muted_dim_fill`)、 斜線ハッチを重ねて「再生されない」 を示す。 caller は
    /// `Note.muted` をそのまま渡す。 `false` のとき描画は既存と完全一致。
    pub muted: bool,
    /// 複数クリップ同時表示でのノート毎の描画 / インタラクション属性。
    /// `NoteStyle::default()` (= `color: None` / 非 dim / 非 locked) のとき描画と hit-test は
    /// 既存と完全一致するので、単一クリップ表示や examples は default のままで挙動が変わらない。
    pub style: NoteStyle,
}

/// 複数クリップ同時ピアノロール編集での、ノート毎の描画 / インタラクション属性。
///
/// 既存 (単一クリップ) 挙動は `NoteStyle::default()` (全フィールド既定) で完全に再現されるため、
/// 新フィールドを意識しない caller / examples は default を渡せばよい。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NoteStyle {
    /// note の基底色。`Some(c)` = `c` を velocity で陰影付け (`shade_by_velocity`) して描画
    /// (複数クリップをクリップ色で塗り分ける)。`None` = 既存どおり `PianoRollStyle::note_fill_fn`
    /// (velocity → Color) を使う。
    pub color: Option<Color>,
    /// 対象 (target/active) でない background クリップの note。fill を grid 背景側へ寄せて
    /// 淡色表示し、対象クリップを際立たせる。`muted` とは独立レイヤ (両方立てば二重に沈む)。
    pub dimmed: bool,
    /// lock されたクリップの note。描画はされるが **hit-test (`note_hit`) / marquee 選択から
    /// 除外**され掴めない (参照専用ゴースト)。`dimmed` より強く沈める。
    pub locked: bool,
}

/// move helper の delta タプル: (id, prev_start_beat, prev_pitch, next_start_beat, next_pitch)。
pub type MoveDelta = (NoteId, f64, u8, f64, u8);

/// resize helper の delta タプル: (id, prev_start_beat, prev_len_beats, next_start_beat, next_len_beats)。
/// ResizeRight (右端 drag) は prev_start == next_start、ResizeLeft (左端 drag) は両方変わる。
pub type ResizeDelta = (NoteId, f64, f64, f64, f64);

/// (M14 Phase 64 / daw_01 #018) velocity lane drag の commit タプル: `(id, new_velocity)`。
/// 絶対値 (0..=127 clamp 済)、prev は持たない (`Move` / `Resize` の delta と異なり差分計算不要)。
pub type VelocityUpdate = (NoteId, u8);

/// note drag の種別 (hit-test 結果)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteDragKind {
    /// note 中央 drag = 平行移動 (start_beat + pitch を更新)。
    Move,
    /// note 左端 4px drag = 左端 resize (start_beat + len_beats を更新)。
    ResizeLeft,
    /// note 右端 4px drag = 右端 resize (len_beats のみ更新)。
    ResizeRight,
}

/// (M14 Phase 70 / daw_01 #042) piano_roll に渡す scale 情報。
///
/// `PianoRollView.scale = None` で旧 API 完全互換 (scale 機能 OFF)、 `Some(_)` で Highlight / Fold
/// の各 mode に応じた行ハイライト + (Fold のみ) y↔pitch 写像の再構成が走る。 caller (daw_01) は
/// `ScaleChange { root, scale }` から `PianoRollScale { root, in_scale_mask, mode }` を 1:1 で詰めて
/// 渡す前提。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PianoRollScale {
    /// ルート pitch class (0..=11、 0 = C、 1 = C#、 ..)。`root % 12` で正規化されることを期待。
    pub root: u8,
    /// ルート起点の 12-bit in-scale mask。 bit 0 = root、 bit d (1..=11) が立っていれば root から
    /// d 半音上が in-scale。 例: Major = `0b0000_1010_1011_0101` (bits {0,2,4,5,7,9,11})。
    pub in_scale_mask: u16,
    /// 表示モード。 Highlight は行リスト不変 + 背景 tint、 Fold は in-scale 行のみで構成。
    pub mode: PianoRollScaleMode,
    /// このキーで音名を flat 表記 (Db/Eb/Gb/Ab/Bb) にするか。 caller (daw_01) が
    /// `common::scale::prefers_flats(root, scale)` で五度圏から決めて渡す。 鍵盤
    /// ラベル / root 表示の異名同音綴りに使い、 Bb メジャーの root を `A#` でなく
    /// `Bb` と綴る。 例 / chromatic 文脈や調号が曖昧なスケールでは `false` (sharp 既定)。
    pub prefer_flats: bool,
}

/// (M14 Phase 70 / daw_01 #042) `PianoRollScale.mode` の取り得る値。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PianoRollScaleMode {
    /// 12 行すべて表示 (= 旧 grid と同じ y↔pitch 写像)、 root / in-scale / out-of-scale を
    /// 背景 overlay + ラベル色で塗り分ける。 既存 note の描画 / drag / hit-test は完全に変わらない
    /// (= 純粋な視覚 augmentation)。
    Highlight,
    /// out-of-scale 行を行リストから完全除外 (Ableton K キー相当)。 12 行 → in-scale 音数 + 上端
    /// octave root に圧縮されて、 row 0 = pitch_top **以下** の最も近い in-scale pitch。 既存 note が
    /// out-of-scale なら上下隣の in-scale 行の中間に 0.5 row 高さで描画 (= データは触らず描画のみ変換、
    /// Ableton 流)。 click / drag y → pitch は in-scale 行 only に snap (= widget が emit する
    /// `MoveDelta.next_pitch` は必ず in-scale)。
    Fold,
}

impl PianoRollScale {
    /// `pitch` が in-scale か判定 (= root を 0 とする mask 上で対応 bit が立っているか)。
    #[must_use]
    pub fn is_in_scale(&self, pitch: u8) -> bool {
        let pc = i32::from(pitch).rem_euclid(12) - i32::from(self.root % 12);
        let d = pc.rem_euclid(12) as u8;
        (self.in_scale_mask >> d) & 1 == 1
    }

    /// MIDI pitch → 「scale degree」 (= MIDI 0 から数えた in-scale pitch の連番)。
    /// out-of-scale pitch は **直下の in-scale pitch** の degree を返す (= Fold mode で
    /// out 行を上下隣 in-scale の中間に描画するときの「上隣」 を degree で揃える定義)。
    /// `is_in_scale(pitch)` が true のときは「自分自身の degree」、 false のときは「直下 in-scale
    /// の degree」 を返す。
    #[must_use]
    pub fn pitch_to_scale_degree(&self, pitch: u8) -> i32 {
        let mut d = 0_i32;
        let mut last_in = 0_i32;
        for p in 0..=pitch {
            if self.is_in_scale(p) {
                last_in = d;
                d += 1;
            }
        }
        if self.is_in_scale(pitch) { d - 1 } else { last_in }
    }

    /// scale degree → in-scale MIDI pitch。 degree が範囲外 (負 / 上限超え) は MIDI 0..=127 に
    /// clamp + 上限 127 を返す。 `pitch_to_scale_degree` の逆関数 (in-scale pitch に対しては
    /// 厳密 round-trip、 out-of-scale pitch は経由できない = 必ず in-scale が返る)。
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn scale_degree_to_pitch(&self, degree: i32) -> u8 {
        if degree < 0 {
            // 最低 in-scale pitch
            for p in 0_u8..=127 {
                if self.is_in_scale(p) {
                    return p;
                }
            }
            return 0;
        }
        let mut d = 0_i32;
        for p in 0_u8..=127 {
            if self.is_in_scale(p) {
                if d == degree {
                    return p;
                }
                d += 1;
            }
        }
        // 上限超え: 最高 in-scale pitch
        for p in (0_u8..=127).rev() {
            if self.is_in_scale(p) {
                return p;
            }
        }
        127
    }
}

/// (M14 Phase 70 / daw_01 #042) drag dy (px) から pitch delta (i32) を計算する mode-aware helper。
///
/// - Linear / Highlight: 旧式 `dy * (pitch_visible / grid.h)` = 半音単位 delta。
/// - Fold: `dy / row_h` = scale degree 単位 delta (= 可視 in-scale 行の数で割る)。
///
/// 返り値は `apply_pitch_drag_delta` で anchor pitch に適用される。
#[must_use]
#[allow(clippy::cast_possible_truncation)]
fn compute_pitch_drag_delta(view: PianoRollView, grid: Rect, dy: f32) -> i32 {
    if matches!(view.scale.map(|s| s.mode), Some(PianoRollScaleMode::Fold)) {
        let geom = RowGeometry::compute(view, grid);
        return (-(dy / geom.row_h.max(1.0))).round() as i32;
    }
    let pitch_per_px = view.pitch_visible / grid.h.max(1.0);
    (-(dy * pitch_per_px)).round() as i32
}

/// (M14 Phase 70 / daw_01 #042 + 70b follow-up) anchor pitch に drag delta を適用、 新 pitch を
/// 返す mode-aware helper。
///
/// - **Fold mode**: anchor を scale degree に変換 → delta 加算 → in-scale pitch に逆変換 (= 必ず
///   in-scale 出力、 `last_alt` 関係なし、 元々 scale degree 単位の drag なので Alt で raw 化する
///   意味がない)。
/// - **Linear (None / Highlight, `snap_pitch_during_drag = false`)**: `anchor + delta` を 0..=127
///   に clamp して u8 化 (= 旧挙動)。
/// - **Linear + Highlight + `snap_pitch_during_drag = true` + `!last_alt`**: clamp 後、
///   `snap_to_nearest_in_scale` で最寄り in-scale に吸着 (= Bitwig / Cubase 流の drag preview snap)。
/// - **Linear + `last_alt = true`**: `snap_pitch_during_drag` 無視で raw clamp (= Alt で snap 一時無効)。
///
/// out-of-scale anchor (Fold 中に既存 out note を drag) は「直下 in-scale」 の degree を基点に。
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn apply_pitch_drag_delta(
    anchor_pitch: u8,
    delta: i32,
    view: PianoRollView,
    last_alt: bool,
) -> u8 {
    if let Some(sc) = view.scale
        && matches!(sc.mode, PianoRollScaleMode::Fold)
    {
        let d = sc.pitch_to_scale_degree(anchor_pitch);
        return sc.scale_degree_to_pitch(d + delta);
    }
    let raw = (i32::from(anchor_pitch) + delta).clamp(0, 127) as u8;
    if let Some(sc) = view.scale
        && matches!(sc.mode, PianoRollScaleMode::Highlight)
        && view.snap_pitch_during_drag
        && !last_alt
    {
        return snap_to_nearest_in_scale(raw, sc);
    }
    raw
}

/// (M14 Phase 70b / daw_01 #042 follow-up) `pitch` を最寄り in-scale pitch に snap する。
///
/// `pitch` が既に in-scale ならそのまま返す。 そうでなければ、 上下に向かって最短距離の in-scale
/// pitch を探し、 距離 tie は **上を優先** (Cubase 流の tie-breaker、 daw_01 一次情報リンクと一致)。
///
/// 必ず in-scale pitch (0..=127) を返す。 `in_scale_mask = 0` (= 全 out-of-scale) の degenerate
/// caller には input `pitch` をそのまま返す (= 上下 12 半音以内に in-scale が無いケース)。
#[must_use]
fn snap_to_nearest_in_scale(pitch: u8, scale: PianoRollScale) -> u8 {
    if scale.is_in_scale(pitch) {
        return pitch;
    }
    // 半音単位で上下を同時探索、 in-scale を見つけたら距離記録。 全 12 半音 範囲 (= 1 octave 以内
    // に必ず in-scale がある、 mask が 0 で無い限り) なら必ず見つかる。
    let mut above: Option<(u8, u8)> = None; // (pitch, distance)
    let mut below: Option<(u8, u8)> = None;
    for d in 1_u8..=12 {
        if above.is_none() {
            let p_up_i = i32::from(pitch) + i32::from(d);
            if p_up_i <= 127 {
                let p_up = p_up_i as u8;
                if scale.is_in_scale(p_up) {
                    above = Some((p_up, d));
                }
            }
        }
        if below.is_none() {
            let p_dn_i = i32::from(pitch) - i32::from(d);
            if p_dn_i >= 0 {
                let p_dn = p_dn_i as u8;
                if scale.is_in_scale(p_dn) {
                    below = Some((p_dn, d));
                }
            }
        }
        if above.is_some() && below.is_some() {
            break;
        }
    }
    match (above, below) {
        (Some((a_p, a_d)), Some((b_p, b_d))) => {
            if a_d <= b_d { a_p } else { b_p }
        }
        (Some((a_p, _)), None) => a_p,
        (None, Some((b_p, _))) => b_p,
        (None, None) => pitch, // degenerate: mask=0、 input をそのまま
    }
}

/// piano roll の view 状態 (pan / zoom)。値渡し (Copy) で widget に渡す。
/// pan/zoom の更新は user 側 (widget は描画と drag のみ担う)。
///
/// 拍は `f64` (`Note.start_beat` / `len_beats` と同精度)、pitch は `f32` (実用上 0..127 の範囲なので
/// f32 で十分)。
#[derive(Clone, Copy, Debug)]
pub struct PianoRollView {
    /// 表示 left の拍 (浮動小数で smooth scroll)。
    pub start_beat: f64,
    /// 表示する拍範囲 (= zoom 倍率の逆数)。例: `view_len_beats=4.0` で 1 小節幅。
    pub len_beats: f64,
    /// 表示 top の MIDI ピッチ (浮動小数で smooth scroll)。
    pub pitch_top: f32,
    /// 表示する pitch 範囲 (例: `24.0` で 2 オクターブ)。
    pub pitch_visible: f32,
    /// 鍵盤領域の幅 (px)。`0.0` で keyboard 非表示、grid のみ。
    pub keyboard_w: f32,
    /// notes / id を編集するたびに bump する hook (cache busting)。
    pub notes_generation: u64,
    /// (M9 Phase 45c) velocity lane の高さ (px)。`0.0` で disabled。
    /// `> 0` のとき rect の下から `velocity_lane_h` px を velocity lane として確保し、
    /// 残りを既存の grid + keyboard に配分する (velocity lane は keyboard 領域には被せない、
    /// grid と同じ x 範囲のみ)。
    pub velocity_lane_h: f32,
    /// (M9 Phase 45c) playhead 線を描く拍位置 (拍)。`None` で disabled。
    /// `Some(b)` で `b` が `[start_beat, start_beat + len_beats]` 範囲内なら、
    /// note grid と velocity lane を縦断する 1 本の線が描かれる。
    pub playhead_beat: Option<f64>,
    /// (M13 Phase 55) ruler 領域の高さ (px、`0.0` で ruler 無し → 旧 piano_roll 互換)。
    /// `> 0` のとき rect の top から `ruler_h` px を ruler として確保し、その下に keyboard /
    /// grid を配置 (keyboard と grid の `y` がともに `rect.y + ruler_h` から開始)。
    /// ruler は keyboard 領域には被せず、grid と同じ x 範囲のみ。
    pub ruler_h: f32,
    /// (M13 Phase 55) テンポ (BPM)。`time_ruler` / `bar_beat_grid` に渡す
    /// `TimeMapping::tempo_bpm` に使う。BarBeat 表示の bar 線位置計算では `time_sig` だけで
    /// 足りるが、将来 Seconds/SMPTE 切替で必要になるため field として保持。
    pub bpm: f32,
    /// (M13 Phase 55) 拍子 (numerator, denominator)。`(4, 4)` で 4/4、`(3, 4)` で 3/4、
    /// `(6, 8)` で 6/8。内部で `numerator * 4 / denominator` (= beats_per_bar) に変換。
    pub time_sig: (u8, u8),
    /// (M9 Phase 45f) drag overlay と commit 値の grid 吸着設定 (#010 [Replied])。
    /// `Default::default()` は `Adaptive` ON。 raw 動作を保ちたい caller は `SnapConfig::OFF` を渡す。
    /// drag 中 `pointer.modifiers.alt` で一時無効化。
    pub snap: SnapConfig,
    /// (M14 Phase 69 / daw_01 #041) loop range `(start_beat, end_beat)` の song-global beat。
    /// `Some((s, e))` で ruler に loop band overlay を描画 + Shift+drag による edit (Start/End/Middle
    /// handle drag) を受け付ける。 `None` で loop band 非表示 + edit 不可。 `view.ruler_h > 0.0` の
    /// ときのみ active (`ruler_h = 0.0` では描画 / hit-test ともに skip = 旧 API 完全互換)。
    /// arrangement `view.loop_range` と完全同形 (caller は両 widget で同じ value を渡せる)。
    pub loop_range: Option<(f64, f64)>,
    /// (M14 Phase 70 / daw_01 #042) scale 情報。 `None` で scale 機能 OFF + 旧 API 完全互換、
    /// `Some(PianoRollScale { root, in_scale_mask, mode })` で Highlight / Fold の各動作が active 化。
    /// 詳細は `PianoRollScale` doc 参照。
    pub scale: Option<PianoRollScale>,
    /// (M14 Phase 70b / daw_01 #042 follow-up) `scale.is_some()` かつ `mode = Highlight` の
    /// とき、 既存 note の y-drag preview / release commit を最寄り in-scale pitch に snap する。
    /// `false` (default) で旧挙動完全互換 (raw pitch)、 `true` で業界標準 (Bitwig / Cubase) の
    /// 「drag 中も snap される行に jump」 動作。 `mode = Fold` のときは無視 (= 元々 in-scale)、
    /// `scale = None` でも無視。 drag 中 `pointer.modifiers.alt` (= `nd.last_alt`) で snap 一時
    /// 無効 (= `snap_beat` と同 policy)。 距離 tie は **上を優先** (Cubase 流)。
    pub snap_pitch_during_drag: bool,
    /// (M14 Phase 124 / daw_01 #100) **3 段目グリッド** (subdivision) の線間隔 (拍単位)。
    /// `Some(0.25)` で 1/16、 `Some(2.0/3.0)` で 1/4T (三連)、 `Some(0.75)` で付点 8 分等。
    /// スナップ値に追従させる用途 (caller が `snap` から算出して渡す)。 `None` (default) で
    /// subdivision 非表示 = bar + beat の 2 段 (旧 API 完全互換)。 `interval_beats >= 1.0` を
    /// 渡しても拍線と重複するだけなので caller 側で `None` 化するのが望ましい。 ズーム退避は
    /// widget 内 (`px_per_interval < 6px` で自動的に 2 段に落ちる)。 線色・幅は
    /// [`PianoRollStyle::sub_line`] / [`PianoRollStyle::sub_line_width_px`]。
    pub sub_grid_interval_beats: Option<f64>,
    /// 新規 note の **既定長** (拍)。 (a) Insert shortcut、 (b) 空白ダブルクリック
    /// 作成で **ドラッグせず即放し** したときの note 長に使う。 caller (daw_01) は
    /// `last_note_duration_beats` (= 直近に描いた / 選択した note の長さ) を渡す = Bitwig の
    /// 「直前にドラッグした長さがそのクリップの新既定になる」 挙動を SSoT 1 本で実現する。
    /// ダブルクリック作成でボタンを放さず左右ドラッグしたときは、 ドラッグ長 (snap 済み右端 −
    /// start) が優先され、 この既定長は使われない。 値は widget 側で `0.0625` (1/16) 下限に clamp。
    pub default_note_len_beats: f64,
    /// `start_beat` が左へスクロールできる下限 (song-absolute 拍)。daw_01 では編集対象 clip の
    /// 開始拍 (= `pianoroll_scroll_beat >= 0` を絶対拍に直したもの)。edge auto-scroll が左端で view を
    /// この値で clamp し、「実際に適用される scroll 量」 を正しく算出して掴んでいる対象を追従させる
    /// (caller の `SetPianoRollScrollX(_.max(0))` clamp と一致させ、anchor が過剰 shift して対象が
    /// 飛ぶのを防ぐ)。clip 概念の無い context (example / test) では `0.0`。
    pub min_start_beat: f64,
}

/// piano roll が user に発行する Edit 要求の種別。
///
/// **このタプル/enum は 1 frame 内で消費される一時 ADT** であり、Application::Message
/// のように Model に保存される / Clone 伝染する性質はない。メッセージ型禁止の不変条件と矛盾しない。
///
/// drag 中の連続更新は library が overlay 描画で実現し、release frame でのみ
/// `Move` / `Resize` を発行する (commit-by-release pattern)。`MoveContinue` 等は持たない。
#[derive(Debug)]
pub enum PianoRollEditRequest {
    /// note を追加 (Insert shortcut)。Undoable。
    Add(Vec<Note>),
    /// 選択中 note を削除 (Delete shortcut)。Undoable。
    Delete(Vec<Note>),
    /// drag release で平行移動。Undoable。
    Move(Vec<MoveDelta>),
    /// (M14 Phase 83 / daw_01 #054) Ctrl+drag release で **複製**。payload は `Move` と同形
    /// (`MoveDelta = (id, prev_beat, prev_pitch, new_beat, new_pitch)`) だが、意味は
    /// 「`id` の note を複製して `new_*` 位置へ配置し、元は据え置き」。note は clip 内 raw data で
    /// リンク概念が無いため独立コピー 1 種 (arrangement の Linked/Independent 区別は不要)。
    Copy(Vec<MoveDelta>),
    /// drag release で resize。Undoable。
    Resize(Vec<ResizeDelta>),
    /// rect select (Shift+drag、加算) または click で selection を更新。Undoable。
    Select { prev: Vec<NoteId>, next: Vec<NoteId> },
    /// (M14 Phase 59 / daw_01 #017) note 群の lyric を一括更新。
    /// 1 commit = 1 Edit = 1 undo 単位 (歌詞 inline 編集 + モーラ単位での次 note 自動分配を
    /// 1 つの undo にまとめる)。`lyric == None` で歌詞削除 (空文字列 commit は widget 内で
    /// `None` に正規化済)。`Vec` の順序は分配順 (start_beat asc → 同 beat なら pitch desc)。
    SetLyrics(Vec<(NoteId, Option<String>)>),
    /// (M14 Phase 64 / daw_01 #018) velocity lane 内 drag による velocity 更新。
    /// release frame で 1 batch 発行 (Move / Resize と同じ pattern)。
    /// 単一 note でも `Vec` で渡し、 multi-select 時は drag 起点の note 含む selected 全 note が
    /// 同じ絶対値に set される (Live / Cubase 流の絶対値 mode)。値は `0..=127` clamp 済。
    /// drag<3px の release は no-op (Edit 発行されない、誤操作防止)。
    SetVelocity(Vec<VelocityUpdate>),
    /// (M14 Phase 69 / daw_01 #041) ruler 上 plain (= Shift 非保持) click / drag による
    /// playhead seek 要求。 caller は (a) `view.playhead_beat = Some(beat)` 更新 (b) audio engine への
    /// seek IPC 送信に変換する。 widget は press frame と continuation frame (drag 中) で発火し、
    /// release frame では emit しない (drag 中の最後の値が確定値、 release 専用 commit は無し)。 同
    /// frame 内で同値を 2 回送らないよう session 側で `last_emitted_beat` を保持。
    /// **snap 適用済 + `0.0` 以上に clamp** (`view.snap.snap_beat(raw, alt, zoom)`)。
    /// arrangement `ArrangementEditRequest::SetPlayheadBeat` と完全同形。
    SetPlayheadBeat(f64),
    /// (M14 Phase 69 / daw_01 #041) ruler 上 Shift + drag による loop range edit 要求。
    /// release frame で 1 度だけ発火 (commit-by-release pattern)、 drag 中は overlay 描画のみ。
    /// `(start, end)` は **snap 適用済**、 `compute_loop_drag_endpoints` で overlay と同一値を
    /// 計算 (「release で grid に飛ぶ」 不整合を構造的に回避)。
    /// arrangement `ArrangementEditRequest::SetLoopRange` と完全同形。
    SetLoopRange { start: f64, end: f64 },
    /// edge auto-scroll による横スクロール要求 (delta、拍)。drag 中にポインタが grid 左右端の
    /// hot-zone に入った frame で発火する。caller は `pianoroll_scroll_beat` に delta を加算し `>= 0` に
    /// clamp する (= `SetPianoRollScrollX(scroll + by)`)。clip 相対オフセットを widget が知らずに済むよう
    /// 絶対値でなく delta で渡す。arrangement は絶対 `SetScrollX` を持つが piano roll の scroll は clip
    /// 相対なので delta 形にする。
    ScrollByBeats(f64),
    /// edge auto-scroll による縦 (pitch) スクロール要求 (絶対 top_pitch)。drag 中にポインタが
    /// grid 上下端の hot-zone に入った frame で発火する。widget が `11..=127` を考慮した clamp 後の値を
    /// 送る (caller の `SetPianoRollTopPitch` handler も同 clamp)。`SetPianoRollScrollX` と同じく view 層が
    /// `SetPianoRollTopPitch` に変換する。
    SetTopPitch(u8),
}

/// `Ui::piano_roll` の戻り値。app 側で connection / hover state の表示に使う。
///
/// `Vec<Edit<M>>` は載せない (widget が `ui.push_edit` で内部発行する、fader と同パターン)。
#[derive(Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct PianoRollResponse {
    /// pointer が grid 内にあるか (keyboard 領域は除く)。
    pub hovered: bool,
    /// hover 中の note id (note 上にあるとき)。
    pub hovered_note_id: Option<NoteId>,
    /// hover 中の note hit zone (cursor 表示判断用)。
    pub hovered_zone: Option<NoteDragKind>,
    /// drag 中ならその種別。
    pub dragging: Option<NoteDragKind>,
    /// Shift+drag rect select (加算) が active か (HUD / status bar 表示用)。
    pub rect_select_active: bool,
    /// このフレームで `PianoRollEditRequest::Select` を push_edit したか (= 次フレームで
    /// `selected` が変わる予定であることを app 側 UI に伝える、selection 連動 UI のトリガー)。
    pub selection_changed: bool,
    /// drag<4px の short click の grid 上 (beat: f64, pitch: f32) (snap 前)。Insert 等の代替起点に使える。
    pub clicked_at_beat_pitch: Option<(f64, f32)>,
    /// (M14 Phase 59 / daw_01 #017) 歌詞編集 mode 中の note id。`Some(_)` のとき
    /// piano_roll 内に text_input overlay が出ており、drag/resize/wheel/click は全て
    /// 短絡 (`dragging` 等は同時に None になる)。app 側で「他 UI grey-out」「Ctrl+Z 抑制」
    /// 等の判断に使える (typing_focus による global shortcut 抑制は `text_input_at` が自動)。
    pub lyric_editing: Option<NoteId>,
    /// (M14 Phase 59 / daw_01 #017) 直近 commit frame で「note 数より入力モーラが多くて
    /// 捨てた数」。0 なら通常、`>0` なら daw_01 で status bar / toast 表示等に使える。
    pub lyric_overflow_morae: usize,
    /// (M14 Phase 64 / daw_01 #018) velocity lane 内 drag が active か (HUD / status bar 表示用)。
    /// `true` のとき drag preview が velocity lane に出ており、release で `SetVelocity` 発行 (drag<3px は no-op)。
    pub velocity_dragging: bool,
    /// (M14 Phase 84 / daw_01 #055) 鍵盤レーンを押している間、カーソルが乗っているキーの
    /// pitch (MIDI note number)。押していない / 鍵盤外 / 編集 mode 中は `None`。押下中に別キーへ
    /// drag するとフレームごとに最新キーへ追従 (glissando)。grid 側の note 編集 / rect select とは
    /// 独立 (鍵盤 press は note drag を開始しない)。caller は前フレーム値との差分で note-on/off を
    /// 導出する (`None→Some`=on / `Some(a)→Some(b)`=off+on / `Some→None`=off)。
    pub keyboard_active_pitch: Option<u8>,
    /// 空白ダブルクリック作成 session が active か (押下のまま drag で長さ決定中)。
    /// `true` のとき作成プレビューが grid に出ており、release で `Add` 発行。 caller は `dragging`
    /// と同様に「drag/作成中は wheel zoom/scroll を無効化」する判断に使う。
    pub creating: bool,
}

/// velocity → fill Color の関数。`fn` pointer (closure 不可、Style: Copy 維持のため)。
pub type NoteFillFn = fn(velocity: u8) -> Color;

/// piano roll の見た目スタイル。`Default` で example の見た目を再現。
///
/// `note_fill_fn` は velocity を Color に変換する関数 (default = `default_velocity_color`)。
#[derive(Clone, Copy, Debug)]
pub struct PianoRollStyle {
    /// grid (note 領域) の背景色 = **白鍵レーン**。
    ///
    /// **不変条件 (M14 Phase 63d / daw_01 #017)**: `black_row_overlay` を `bg` に src-over 合成した
    /// 結果が `bg` より **暗く** なるよう値を選ぶこと (= 鍵盤側 `white_key` > `black_key` の
    /// 濃淡関係を grid 側でも保つ)。Ableton Live / Cubase / Reaper / FL Studio 等の主流 DAW 慣習。
    /// `default_black_row_is_darker_than_white_row` test で固定。
    pub bg: Color,
    pub keyboard_bg: Color,
    pub white_key: Color,
    pub black_key: Color,
    /// grid 内の黒鍵 row 帯。`bg` (= 白鍵レーン) に src-over 合成して描画され、
    /// **`bg` よりわずかに暗くする** ために黒系 (`rgba(0,0,0,a)`) の半透明色を使う。
    /// 詳細は `bg` の不変条件 doc 参照 (M14 Phase 63d / daw_01 #017)。
    pub black_row_overlay: Color,
    /// 4 拍ごとの太線 (小節線)。
    pub bar_line: Color,
    /// 1 拍ごとの細線。
    pub beat_line: Color,
    pub bar_line_width_px: f32,
    pub beat_line_width_px: f32,
    /// (M14 Phase 124 / daw_01 #100) **3 段目グリッド** (subdivision) の線色。
    /// `beat_line` より淡くするのが通例 (default `rgba(1,1,1,0.06)`)。
    /// `PianoRollView::sub_grid_interval_beats == Some(_)` のときのみ使用。
    pub sub_line: Color,
    /// (M14 Phase 124 / daw_01 #100) subdivision 線の幅 (px、 default `1.0`)。
    pub sub_line_width_px: f32,
    pub note_fill_fn: NoteFillFn,
    pub note_border_radius_px: f32,
    /// muted note に重ねる斜線ハッチの色 (半透明)。default は
    /// 半透明黒 `rgba(0,0,0,0.40)`。`Note.muted == true` のときのみ描画。
    pub note_muted_hatch_color: Color,
    /// muted note ハッチの線間隔 (px、default 5.0) と線幅 (px、default 1.0)。
    /// note は clip より小さいので clip ハッチより密にする。
    pub note_muted_hatch_spacing_px: f32,
    pub note_muted_hatch_width_px: f32,
    pub note_selected_fill: Color,
    pub note_selected_border: Color,
    pub note_selected_border_w: f32,
    pub note_selected_pad_px: f32,
    /// (M14 Phase 83 / daw_01 #054) Ctrl+drag copy 中の複製 ghost の fill / border 色。
    /// move drag の ghost (`note_selected_*` = 黄) と色を変えて「コピー操作中」 を視覚区別する
    /// (arrangement の clip clone ghost と同 idiom)。`..Default::default()` caller は無修正。
    pub note_clone_ghost_fill: Color,
    pub note_clone_ghost_border: Color,
    /// resize handle の幅 (px)。note rect 左右 edge から **内外** この px = resize、
    /// それ以外 (rect 中央) = move。短 note (`r.w <= resize_handle_px * 2`) は rect 内
    /// すべて Move、rect 外側のみ resize 判定。
    pub resize_handle_px: f32,
    pub c_label_color: Color,
    pub c_label_font_px: f32,
    /// M9 Phase 44c: note 上に重ねて描画する歌詞 (lyric) の色とフォントサイズ。
    /// `Note.lyric == Some(...)` のとき note rect 内に label 描画 (vertical center)。
    /// **(M14 Phase 59)** `lyric_font_px` は **MAX cap** として解釈される。 実 font_size は
    /// `(note_h * 0.75).clamp(7.0, lyric_font_px)` で note の高さに連動する (zoom in / out で
    /// 自動スケール、 daw_01 #017 動作確認で「9px 固定だと小さすぎる」 指摘から)。
    pub lyric_color: Color,
    pub lyric_font_px: f32,
    /// (M14 Phase 59 / daw_01 #017) 歌詞編集モード起動 shortcut name。
    /// `Some(name)` のとき `take_shortcut(name)` を 1 frame 1 度監視し、起動条件
    /// (`selected.len() == 1` かつ `lyric_editing == None`) を満たせば編集モードに入る。
    /// `None` で機能完全無効 (text_input overlay も出ない)。
    ///
    /// 既定値 `Some("piano_roll.edit_lyric")`。caller 側で
    /// `host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L")` を 1 度呼ぶ。
    /// `with_default_bindings()` には**含めない** (修飾なし `L` は他文脈で別意味になりうる
    /// ため caller opt-in 方針)。
    pub lyric_edit_shortcut: Option<&'static str>,
    /// (M9 Phase 45c) playhead 線の色。`PianoRollView::playhead_beat == Some(_)` のときのみ使用。
    pub playhead_color: Color,
    /// (M9 Phase 45c) playhead 線の幅 (px)。bar_line と紛れない程度に太くする。
    pub playhead_width_px: f32,
    /// (M9 Phase 45c) velocity lane の背景色。
    pub velocity_lane_bg: Color,
    /// (M9 Phase 45c) velocity bar の色。selection 反映は将来 phase (drag editing と一緒)。
    pub velocity_bar_color: Color,
    /// (M9 Phase 45c) velocity bar の幅 (px)。
    pub velocity_bar_width_px: f32,
    /// (M13 Phase 55) ruler 領域の背景色 (`view.ruler_h > 0` のとき `time_ruler` の `bg` に渡す)。
    pub ruler_bg: Color,
    /// (M13 Phase 55) ruler の小節番号テキスト色 (`time_ruler` の `label_color` に渡す)。
    pub ruler_label_color: Color,
    /// (M14 Phase 69 / daw_01 #041) ruler 上の loop band (背景帯) 色。 半透明 cyan 系 default。
    /// `view.loop_range == Some(_)` かつ `view.ruler_h > 0.0` のときに描画される。 arrangement と完全同 default。
    pub loop_band: Color,
    /// (M14 Phase 69 / daw_01 #041) loop band 両端 handle bar の色 (不透明 cyan 系 default)。
    pub loop_handle: Color,
    /// (M14 Phase 69 / daw_01 #041) loop band handle bar の幅 (px)。
    pub loop_handle_w: f32,
    /// (M14 Phase 70 / daw_01 #042) `PianoRollView.scale = Some(_)` のとき、 root pitch class の
    /// 行 (grid + keyboard 両方) に重ね描く半透明 tint。 Bitwig 風 warm-yellow default。
    /// `bg` + `black_row_overlay` の 2-pass の **上に** 重ね描く 3rd pass (= in-scale 行は
    /// overlay なしで通常表示)。 mode が Highlight でも Fold でも適用 (Fold では in-scale 行
    /// のみ描画されるので root 行のみ tint される)。
    pub root_row_overlay: Color,
    /// (M14 Phase 70 / daw_01 #042) Highlight mode で out-of-scale 行 (grid + keyboard) に重ね描く
    /// 半透明 dim。 Fold mode では out 行は表示されないので使われない。
    pub out_of_scale_row_overlay: Color,
    /// (M14 Phase 70 / daw_01 #042) 鍵盤レーンの root pitch class ラベル色 (`scale = Some(_)` の
    /// とき active、 Highlight / Fold 共通)。 `scale = None` 時は `c_label_color` を使う旧挙動。
    pub root_label_fg: Color,
    /// (M14 Phase 70 / daw_01 #042) 鍵盤レーンの in-scale (root 以外) ラベル色。 Fold mode で
    /// 全 in-scale 行に label が出るときに使用 (Highlight では root 以外は label を描かないので
    /// 通常使われない)。
    pub in_scale_label_fg: Color,
    /// (M14 Phase 70 / daw_01 #042) 鍵盤レーンの out-of-scale ラベル色 (Highlight でのみ可視、
    /// Fold では out 行は非表示)。 ただし v0 では Highlight mode で out 行に label を描かない
    /// (root 行のみ label) ので、 将来「全 12 行に label」 拡張が来たときの為の予約 field。
    pub out_of_scale_label_fg: Color,
    /// (M14 Phase 117 / daw_01 #093) 鍵盤オクターブラベルを、 その行の **実効背景** (key fill +
    /// root/out overlay の alpha 合成色) の WCAG relative luminance で `label_fg_dark` / `label_fg_light`
    /// に自動反転するか。 default `true`。 `false` で旧挙動 (root=`root_label_fg` / in-scale=
    /// `in_scale_label_fg` / C=`c_label_color` の固定色)。 arrangement の clip 名 auto-contrast (#060) と
    /// 同じ「widget が実際に塗った fill から文字色を導出する」 SSoT を鍵盤ラベルに適用したもの。
    /// warm root 行 (root_row_overlay 重畳の cream) / 白鍵で暗文字、 黒鍵 / dim 行で明文字を選ぶ。
    pub label_auto_contrast: bool,
    /// (M14 Phase 117 / daw_01 #093) `label_auto_contrast` で明るい行背景に選ぶ暗ラベル色 (near-black)。
    pub label_fg_dark: Color,
    /// (M14 Phase 117 / daw_01 #093) `label_auto_contrast` で暗い行背景に選ぶ明ラベル色 (near-white)。
    pub label_fg_light: Color,
}

/// デフォルト velocity color (青系の濃淡 0.5..0.95)。`PianoRollStyle::note_fill_fn` の初期値。
#[must_use]
pub fn default_velocity_color(velocity: u8) -> Color {
    let t = f32::from(velocity) / 127.0;
    Color::rgba(0.35 + t * 0.35, 0.55 + t * 0.30, 0.85 + t * 0.10, 1.0)
}

/// クリップ基底色 `base` を velocity で陰影付けする (hue は保ち明度のみ変える)。
/// 低 velocity ほど暗く (係数 0.55..1.0)。`NoteStyle::color = Some` の note に使う。
/// alpha は `base` を維持。`note_fill_fn` (velocity → 青の濃淡) のクリップ色版に相当。
#[must_use]
pub fn shade_by_velocity(base: Color, velocity: u8) -> Color {
    let t = f32::from(velocity) / 127.0;
    let k = 0.55 + t * 0.45;
    Color::rgba(base.r * k, base.g * k, base.b * k, base.a)
}

/// `color` を背景 `bg` 側へ `amount` (0..1) だけ寄せて淡色化する (lerp)。
/// `amount=0` で `color` のまま、`1` で `bg`。非対象 (dimmed) / lock クリップの note を
/// 沈めて対象クリップを際立たせるのに使う。alpha は不透明 (`bg` 上の grid 線が透けないよう) に保つ。
#[must_use]
pub fn dim_toward(color: Color, bg: Color, amount: f32) -> Color {
    let a = amount.clamp(0.0, 1.0);
    Color::rgba(
        color.r + (bg.r - color.r) * a,
        color.g + (bg.g - color.g) * a,
        color.b + (bg.b - color.b) * a,
        color.a,
    )
}

/// note の最終 fill を決める (color/dim/lock/mute を統合)。`draw_notes` と
/// `draw_velocity_lane` が共有して描画一致を保証する。`bg` は dim の寄せ先 (grid 背景)。
#[must_use]
fn note_fill_color(note: &Note, note_fill_fn: NoteFillFn, bg: Color) -> Color {
    let base = match note.style.color {
        Some(c) => shade_by_velocity(c, note.velocity),
        None => note_fill_fn(note.velocity),
    };
    // lock は dim より強く沈める (参照専用を明示)、次いで非対象 dimmed。
    let base = if note.style.locked {
        dim_toward(base, bg, 0.72)
    } else if note.style.dimmed {
        dim_toward(base, bg, 0.48)
    } else {
        base
    };
    if note.muted {
        crate::widgets::muted_dim_fill(base)
    } else {
        base
    }
}

impl Default for PianoRollStyle {
    fn default() -> Self {
        Self {
            // M14 Phase 63d / daw_01 #017: 鍵盤側 `white_key (0.92) > black_key (0.10)` の濃淡に
            // 揃え、grid の **白鍵レーン (= bg) を黒鍵レーン (= bg + overlay) より明るく** する。
            // 旧値 `bg=(0.12)` + `overlay=rgba(1,1,1,0.04)` は黒鍵 row が約 (0.155) で bg より
            // 明るくなる (鍵盤と逆) symptom があった。Live / Cubase / Reaper 慣習に合わせる。
            // 階層: ruler_bg(0.13) < velocity_lane_bg(0.16) < bg(0.18) < keyboard_bg(0.22)
            // → grid (note 配置領域) が最も明るく、 周辺 panel が段階的に暗い。
            bg: theme::PANEL_RAISED,
            keyboard_bg: theme::PANEL_RAISED,
            // white_key / black_key は物理ピアノ鍵盤のメタファ。 寒色 dark テーマには白鍵に
            // 充てる明 surface token が無いため、 意図的な literal のまま残す (token 化しない)。
            white_key: Color::rgb(0.92, 0.93, 0.95),
            black_key: Color::rgb(0.10, 0.11, 0.13),
            // src-over 合成で黒鍵 row を bg より暗く。
            black_row_overlay: theme::BACKDROP.with_alpha(0.25),
            bar_line: theme::GRID_LINE_STRONG,
            beat_line: theme::GRID_LINE,
            bar_line_width_px: 1.5,
            beat_line_width_px: 1.0,
            // M14 Phase 124 / daw_01 #100: subdivision 線は beat_line より淡く。
            sub_line: theme::GRID_LINE.with_alpha(0.04),
            sub_line_width_px: 1.0,
            note_fill_fn: default_velocity_color,
            note_border_radius_px: 1.5,
            note_muted_hatch_color: theme::BACKDROP.with_alpha(0.40),
            note_muted_hatch_spacing_px: 5.0,
            note_muted_hatch_width_px: 1.0,
            note_selected_fill: theme::SELECTION_WARM,
            // 選択ノードのリングは velocity 着色されたノート上で確実に立つ意図的な pure-white。
            note_selected_border: Color::WHITE,
            note_selected_border_w: 2.0,
            note_selected_pad_px: 2.0,
            // M14 Phase 83 / daw_01 #054: copy ghost は move ghost (黄) と区別する緑系
            // (arrangement の clone linked ghost と同系統)。
            note_clone_ghost_fill: theme::GHOST_LINKED.with_alpha(0.85),
            note_clone_ghost_border: theme::GHOST_LINKED,
            resize_handle_px: 4.0,
            c_label_color: theme::TEXT_FAINT,
            c_label_font_px: 11.0,
            // M9 Phase 45c: playhead / velocity lane defaults
            // playhead は bar_line (白 alpha 0.3) と紛れないよう強い赤系 + 太め
            playhead_color: theme::PLAYHEAD,
            playhead_width_px: 2.5,
            velocity_lane_bg: theme::PANEL,
            velocity_bar_color: theme::ACCENT,
            velocity_bar_width_px: 3.0,
            lyric_color: theme::TEXT_ON_BRIGHT,
            // M14 Phase 59: MAX cap (実 font_size = note_h * 0.75 で note 高さスケール)。
            // 旧 9.0 固定 → 24.0 max にして zoom in 時の readable 化。
            lyric_font_px: 24.0,
            // M14 Phase 59 / daw_01 #017: 歌詞編集 (L キー) shortcut。caller が `bind("L")` する想定。
            lyric_edit_shortcut: Some("piano_roll.edit_lyric"),
            // M13 Phase 55: ruler 領域 (`view.ruler_h > 0` のときのみ描画)
            ruler_bg: theme::HEADER,
            ruler_label_color: theme::TEXT_DIM,
            // M14 Phase 69 / daw_01 #041: arrangement と同 default 値 (cyan ~0.20 alpha 帯 + 不透明 handle)。
            loop_band: theme::LOOP_BAND.with_alpha(0.20),
            loop_handle: theme::LOOP_BAND,
            loop_handle_w: 2.0,
            // M14 Phase 70 / daw_01 #042 + 70a (follow-up): Bitwig 風 warm-yellow tint (root 行)
            // + 黒 dim (out 行)。 alpha は daw_01 実機 smoke test (#042 follow-up) で「白鍵 row 上
            // の root tint が見えない / 黒鍵 row との dim 差が 0.015 で out 認識が立たない」 指摘
            // を受けて引き上げ済。 control: dark theme (widget bg 0.18、 黒鍵 row ≈ 0.135) で
            // 「在ることが分かる」 を最低基準にする。 alpha 0.18 / 0.32 だと不可視レベル。
            root_row_overlay: theme::SELECTION_WARM.with_alpha(0.32),
            // out_of_scale_row_overlay: alpha 0.50 で 黒鍵 row との差が ≈ 0.045 に拡大、 dim 認識成立。
            out_of_scale_row_overlay: theme::BACKDROP.with_alpha(0.50),
            // 鍵盤レーンラベル色: root は warm-yellow を強調 (0.95, 0.78, 0.40)、 in-scale は Fold
            // mode で全行に label が出るため keyboard_bg (0.22) 上で読める明度 (0.78〜0.85)、
            // out-of-scale は dim (0.45 程度、 Highlight mode で root 行以外の label 描画は v0 では
            // 出ないが、 将来「全 12 行 label」 拡張用に予約)。
            root_label_fg: theme::SELECTION_WARM,
            in_scale_label_fg: theme::TEXT_DIM,
            out_of_scale_label_fg: theme::TEXT_FAINT,
            // M14 Phase 117 / daw_01 #093: 鍵盤ラベルの auto-contrast (default on)。 両極は white 鍵
            // (0.92) / black 鍵 (0.10) / warm cream root 行のいずれでも WCAG コントラスト比が十分立つ
            // near-black / near-white。
            label_auto_contrast: true,
            label_fg_dark: theme::TEXT_ON_BRIGHT,
            label_fg_light: theme::TEXT,
        }
    }
}

// ============================================================
// Public pure functions (app 側で hit-test / 座標変換に使える)
// ============================================================

/// 1 つの note の screen 座標 rect を返す (pan/zoom 適用後、grid 外も含む raw rect)。
/// app 側で「click 位置から note を逆引き」「drag preview rect」等の座標計算に使える。
///
/// Note は `Clone` のみ (Arc<str> lyric のため) なので参照渡し。
#[must_use]
pub fn note_to_rect(note: &Note, view: PianoRollView, grid: Rect) -> Rect {
    note_geometry_to_rect(note.start_beat, note.len_beats, note.pitch, view, grid)
}

/// 内部 helper: note geometry から rect を計算。`note_to_rect` と drag preview から呼ばれる。
/// 拍は f64、最終的な pixel 座標は f32 にcast (描画用)。
fn note_geometry_to_rect(
    start_beat: f64,
    len_beats: f64,
    pitch: u8,
    view: PianoRollView,
    grid: Rect,
) -> Rect {
    let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
    let x = grid.x + ((start_beat - view.start_beat) * beat_to_px) as f32;
    let w = ((len_beats * beat_to_px) as f32).max(1.5);
    // (M14 Phase 70 / daw_01 #042) Fold mode は y↔pitch 写像が in-scale 行 only に圧縮される。
    // RowGeometry::compute で linear / fold どちらの mode でも統一的に y 座標を返せる。
    let geom = RowGeometry::compute(view, grid);
    let (y, h) = geom.pitch_to_y_and_h(pitch);
    Rect { x, y, w, h }
}

/// (M14 Phase 70 / daw_01 #042) y↔pitch 写像の統合 helper。
///
/// Highlight (scale=None / scale.mode=Highlight): 旧 linear 写像と完全同一
/// (`pitch_to_px = grid.h / pitch_visible`、 12 行 1 octave)。
///
/// Fold (scale.mode=Fold): 可視 MIDI pitch 範囲 `[pitch_top - pitch_visible, pitch_top]` 内の
/// in-scale pitch のみを enumerate、 等高で並べる。 row 0 = pitch_top **以下** の最も近い
/// in-scale pitch。 out-of-scale な pitch を query すると nearest in-scale above / below の
/// **中間** に 0.5 row 高さで配置 (= Ableton 流の挟まる描画)。
struct RowGeometry {
    /// Fold mode 中の可視 in-scale pitches (top→bottom 順、 row 0 が先頭)。 linear mode では空。
    fold_rows: Vec<u8>,
    /// 行高 (px): linear なら `grid.h / pitch_visible`、 Fold なら `grid.h / fold_rows.len()`。
    row_h: f32,
    /// linear mode で使う `view.pitch_top` (fold mode では row 0 の pitch が代替役なので未使用)。
    pitch_top: f32,
    /// grid 上端 y。
    grid_y: f32,
    /// Fold mode が active か。
    fold: bool,
    /// scale 情報 (Fold 中の in-scale 判定 + 中間描画位置算出に使う)。
    scale: Option<PianoRollScale>,
}

impl RowGeometry {
    fn compute(view: PianoRollView, grid: Rect) -> Self {
        let fold = matches!(view.scale.map(|s| s.mode), Some(PianoRollScaleMode::Fold));
        if fold {
            let sc = view.scale.expect("fold => scale is Some");
            // 可視 MIDI pitch 範囲 [floor(pitch_top - pitch_visible), ceil(pitch_top)] 内の
            // in-scale pitch を上→下 (= pitch desc) で enumerate。
            // 可視 MIDI pitch 範囲 `[pitch_top - pitch_visible, pitch_top]` を **両端 inclusive** で
            // 解釈、 in-scale pitch を上→下で enumerate。 `floor(pitch_top)` を上限、
            // `ceil(pitch_top - pitch_visible)` を下限とすると整数境界 (e.g., 72/60) で off-by-one
            // にならず、 1 octave 表示 (pitch_visible=12) でちょうど 1 octave 分が拾える。
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let hi_pitch = view.pitch_top.floor() as i32;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let lo_pitch = (view.pitch_top - view.pitch_visible).ceil() as i32;
            let mut rows: Vec<u8> = Vec::new();
            let mut p = hi_pitch;
            while p >= lo_pitch.max(0) {
                if (0..=127).contains(&p) && sc.is_in_scale(p as u8) {
                    rows.push(p as u8);
                }
                p -= 1;
            }
            let n = rows.len().max(1);
            let row_h = grid.h / n as f32;
            Self {
                fold_rows: rows,
                row_h,
                pitch_top: view.pitch_top,
                grid_y: grid.y,
                fold: true,
                scale: view.scale,
            }
        } else {
            let row_h = grid.h / view.pitch_visible.max(1e-6);
            Self {
                fold_rows: Vec::new(),
                row_h,
                pitch_top: view.pitch_top,
                grid_y: grid.y,
                fold: false,
                scale: view.scale,
            }
        }
    }

    /// pitch → (y_top, height) を返す。 Fold mode で out-of-scale の場合は中間描画位置 + 半行高。
    fn pitch_to_y_and_h(&self, pitch: u8) -> (f32, f32) {
        if !self.fold {
            let y = self.grid_y + (self.pitch_top - f32::from(pitch)) * self.row_h;
            return (y, (self.row_h - 1.0).max(2.0));
        }
        // Fold: pitch が in-scale なら直接 row index を引く
        if let Some(idx) = self.fold_rows.iter().position(|&p| p == pitch) {
            let y = self.grid_y + idx as f32 * self.row_h;
            return (y, (self.row_h - 1.0).max(2.0));
        }
        // out-of-scale: nearest in-scale above (= row index 小、 pitch 大) / below (= row 大、 pitch 小)
        // を探して中間 y。 高さは 0.5 row。
        let above_idx = self.fold_rows.iter().position(|&p| p < pitch);
        match above_idx {
            None => {
                // 全 in-scale が pitch **以上** (= 自身が最下行より下)。 最下行の
                // 少し下 (境界を跨いで半行) に描画 — 上端ケース `Some(0)` と対称。
                // 旧実装は row 0 の上に置いており、 下へスクロールアウトした note が
                // grid 最上部に出現 + そこで hit していた (review)。
                let y = self.grid_y + self.fold_rows.len() as f32 * self.row_h
                    - self.row_h * 0.25;
                (y, (self.row_h * 0.5 - 1.0).max(2.0))
            }
            Some(0) => {
                // pitch が row 0 の上、 row 0 の少し上に描画
                let y = self.grid_y - self.row_h * 0.25;
                (y, (self.row_h * 0.5 - 1.0).max(2.0))
            }
            Some(below_i) => {
                // 上隣 = fold_rows[below_i - 1] (pitch 大、 row 上)
                // 下隣 = fold_rows[below_i]     (pitch 小、 row 下)
                let above_i = below_i - 1;
                let y_above = self.grid_y + above_i as f32 * self.row_h;
                let y_below = self.grid_y + below_i as f32 * self.row_h;
                let y_mid = (y_above + y_below) * 0.5;
                (y_mid + self.row_h * 0.25, (self.row_h * 0.5 - 1.0).max(2.0))
            }
        }
    }

    /// cursor y → pitch (f32)。 Highlight (linear) では `pitch_top - (y - grid_y) / row_h` 同等、
    /// Fold では row index に近い in-scale pitch を返す (= 必ず in-scale)。
    fn y_to_pitch_f(&self, y: f32) -> f32 {
        if !self.fold || self.fold_rows.is_empty() {
            return self.pitch_top - (y - self.grid_y) / self.row_h.max(1e-6);
        }
        let raw = ((y - self.grid_y) / self.row_h).floor();
        // fold_rows.len() は MIDI 0..=127 範囲なので 128 を超えない、 i32 wrap は実用上発生しない。
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap
        )]
        let last_i = (self.fold_rows.len() - 1) as i32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let idx = (raw as i32).clamp(0, last_i) as usize;
        f32::from(self.fold_rows[idx])
    }

    /// cursor y → pitch (整数 MIDI、どの鍵盤キー上か)。鍵盤レーン click のピッチ確定用 (daw_01 #055)。
    /// `y_to_pitch_f` の行範囲 `(p-1, p]` を `ceil` で整数化し 0..=127 に clamp する。
    /// fold mode では `y_to_pitch_f` が既に in-scale 整数を返すので `ceil` は no-op。
    fn y_to_pitch(&self, y: f32) -> u8 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let p = self.y_to_pitch_f(y).ceil().clamp(0.0, 127.0) as u8;
        p
    }

    /// scale が Some なら root pitch class を返す。
    fn root_pc(&self) -> Option<u8> {
        self.scale.map(|sc| sc.root % 12)
    }
}

/// (M14 Phase 70 / daw_01 #042) pitch class (0..=11) → 音名 (sharp 表記)。
/// `0 = C、 1 = C#、 ... 11 = B`。 キーの調号に応じた異名同音綴り (Db / Eb 等) は
/// [`pitch_class_name_spelled`] を使う (`PianoRollScale.prefer_flats` 経由)。
#[must_use]
pub fn pitch_class_name(pc: u8) -> &'static str {
    pitch_class_name_spelled(pc, false)
}

/// pitch class (0..=11) → 音名。 `prefer_flats` で flat 表記 (Db/Eb/Gb/Ab/Bb) と
/// sharp 表記 (C#/D#/F#/G#/A#) を切り替える (= キーの調号に追従した異名同音綴り)。
/// 白鍵 (C D E F G A B) はどちらでも同じ。
#[must_use]
pub fn pitch_class_name_spelled(pc: u8, prefer_flats: bool) -> &'static str {
    const SHARP: [&str; 12] =
        ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    const FLAT: [&str; 12] =
        ["C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B"];
    let i = (pc % 12) as usize;
    if prefer_flats { FLAT[i] } else { SHARP[i] }
}

/// 内部 helper: cursor 位置がこの note のどの zone (Move / ResizeLeft / ResizeRight)
/// に該当するかを返す。`note_hit` / `note_hover_cursor` から共通で呼ばれる。
///
/// 判定範囲 (x 方向): note rect の左右 edge から **内外** ±`edge` px (= 8px 幅のハンドル帯)。
/// y 方向は note rect 内のみ (拡張なし、隣接 pitch との衝突回避)。
///
/// 短 note (`r.w <= edge * 2.0`) は rect 内では Move 強制 (左右 edge 領域が重なって
/// 判別不能なため)、rect 外側のみ ResizeLeft / ResizeRight として扱う。
fn note_zone_at(
    note: &Note,
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<NoteDragKind> {
    let r = note_to_rect(note, view, grid);
    // y は note rect 内のみ (Rect::contains の半開区間と整合)
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
    let short_note = r.w <= edge * 2.0;

    Some(if short_note && in_rect {
        NoteDragKind::Move
    } else if !in_rect {
        // rect 外 (外側拡張ハンドル) は「rect のどちら側か」で決める。 near_left
        // 先行評価だと幅 < edge の極短 note で右外側帯 [r.x+r.w, r.x+edge) が
        // ResizeLeft に化ける (review — doc の「外側は左右それぞれの端」 と整合)。
        if cx < r.x {
            NoteDragKind::ResizeLeft
        } else {
            NoteDragKind::ResizeRight
        }
    } else if near_left {
        NoteDragKind::ResizeLeft
    } else if near_right {
        NoteDragKind::ResizeRight
    } else {
        NoteDragKind::Move
    })
}

/// visible note slice に対する hit-test 本体。`note_hit` (visible 絞り込み後) と
/// `note_hover_cursor` が共有し、「drag で掴む note」と「hover カーソルが指す note」を
/// 構造的に一致させる (SSoT)。
///
/// 隣接 note (A.right == B.left) では両者の resize ハンドル帯が共有境界付近で重なる。
/// このとき **cursor が rect 内部に在る note (in-rect) を、外側拡張ハンドル
/// (outer-extension) しか当たらない note より無条件で優先**する。これにより A の右端を
/// 掴みたいのに B の左端 resize に奪われる問題 (daw_01 #053) を解消。同 tier
/// (両方 in-rect = overlap、または両方 outer = 微小 gap) は resize edge への水平距離が
/// 近い方を採用し、同距離なら後勝ち (描画順で前面) を踏襲する。
fn note_hit_in(
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    edge: f32,
) -> Option<(NoteId, NoteDragKind)> {
    let mut hit: Option<(NoteId, NoteDragKind)> = None;
    let mut hit_inside = false;
    let mut hit_edge_dist = f32::INFINITY;
    for note in visible {
        // lock されたクリップの note は参照専用ゴースト = hit-test から
        // 除外して掴めなくする (描画はされる)。hover カーソルも note_hit_in 共有なので一致する。
        if note.style.locked {
            continue;
        }
        let Some(kind) = note_zone_at(note, view, grid, cx, cy, edge) else {
            continue;
        };
        let r = note_to_rect(note, view, grid);
        let inside = cx >= r.x && cx < r.x + r.w;
        // resize edge への水平距離 (Move は当該 cursor 位置 = 距離 0 扱い)。
        let edge_x = match kind {
            NoteDragKind::ResizeLeft => r.x,
            NoteDragKind::ResizeRight => r.x + r.w,
            NoteDragKind::Move => cx,
        };
        let dist = (cx - edge_x).abs();
        // in-rect は outer に無条件で勝つ。同 tier は近い edge 優先 (同距離は後勝ち)。
        let better = if inside == hit_inside {
            dist <= hit_edge_dist
        } else {
            inside
        };
        if better {
            hit = Some((note.id, kind));
            hit_inside = inside;
            hit_edge_dist = dist;
        }
    }
    hit
}

/// note hit-test (visible filtering 後)。grid 内の cursor 位置で hit する note の id と
/// hit zone (Move / ResizeLeft / ResizeRight) を返す。
///
/// resize handle は note rect の左右 edge から **内外** ±`resize_handle_px` の範囲
/// (= 8px 幅のハンドル帯)。短 note (`r.w <= resize_handle_px * 2`) は rect 内は Move 強制、
/// rect 外側のみ resize 判定。隣接 note でハンドル帯が重なる座標の解決規則は
/// [`note_hit_in`] を参照 (in-rect 優先 / daw_01 #053)。
///
/// `notes` は start_beat 昇順にソート済を仮定 (二分探索で visible 範囲を絞る)。
#[must_use]
pub fn note_hit(
    notes: &[Note],
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<(NoteId, NoteDragKind)> {
    if !grid.contains(cx, cy) {
        return None;
    }
    let view_start = view.start_beat;
    let view_end = view_start + view.len_beats;
    let s_idx = notes.partition_point(|n| n.start_beat + n.len_beats < view_start);
    let e_idx = s_idx + notes[s_idx..].partition_point(|n| n.start_beat <= view_end);
    let visible = &notes[s_idx..e_idx];
    note_hit_in(visible, view, grid, cx, cy, resize_handle_px)
}

/// hover 中の note hit zone から cursor 形状を決める。
/// note rect 左右 edge の内外 ±`resize_handle_px` = `EwResize`、中央 = `Move`、
/// grid 外やどの note の判定範囲外も None。
///
/// hit 解決は [`note_hit`] と同一 ([`note_hit_in`] 経由) なので、表示カーソルが指す note と
/// 実際に drag で掴む note は常に一致する。
#[must_use]
pub fn note_hover_cursor(
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    cx: f32,
    cy: f32,
    resize_handle_px: f32,
) -> Option<CursorIcon> {
    if !grid.contains(cx, cy) {
        return None;
    }
    note_hit_in(visible, view, grid, cx, cy, resize_handle_px).map(|(_, kind)| match kind {
        NoteDragKind::Move => CursorIcon::Move,
        NoteDragKind::ResizeLeft | NoteDragKind::ResizeRight => CursorIcon::EwResize,
    })
}

/// 2 つの矩形が交差するか (Shift+drag rect select で使う)。接するだけは交差扱いしない。
#[must_use]
pub fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// 黒鍵判定 (C# / D# / F# / G# / A#)。
#[must_use]
pub fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

/// 日本語テキストをモーラ単位で分割する (歌唱合成用)。
///
/// `Ui::piano_roll` の歌詞 inline 編集 (M14 Phase 59 / daw_01 #017) が「`Enter` で
/// commit text を分割して次 note へ自動分配」するために使う公開 helper。daw_01 以外
/// (将来のボーカルエディタ等) でも再利用可。
///
/// # 分割ルール (REAPER VOICEVOX script に準拠)
///
/// - 基本: 1 char = 1 モーラ
/// - **小書きかな** (`ぁぃぅぇぉ ゃゅょ っ ゎ ァィゥェォ ャュョ ッ ヮ`) は **直前の char と結合**。
///   - 例: `"きゃ"` → 1 モーラ (`["きゃ"]`)
///   - 例: `"しゅんかん"` → 4 モーラ (`["しゅ", "ん", "か", "ん"]`)
/// - 連続小書きは結合先 char に積まれる (`"きゃっ"` → `["きゃっ"]`)
/// - 先頭 char が小書きかなの場合は単独 1 モーラ (defensive、通常入力では発生しない)
/// - ASCII / 漢字 / その他はそのまま 1 char = 1 モーラ
///
/// # 例
///
/// ```
/// use daw_ui_core::split_into_morae;
/// assert_eq!(split_into_morae("あいうえ"), vec!["あ", "い", "う", "え"]);
/// assert_eq!(split_into_morae("しゅんかん"), vec!["しゅ", "ん", "か", "ん"]);
/// assert_eq!(split_into_morae(""), Vec::<String>::new());
/// ```
#[must_use]
pub fn split_into_morae(text: &str) -> Vec<String> {
    const SMALL_KANA: &[char] = &[
        'ぁ', 'ぃ', 'ぅ', 'ぇ', 'ぉ', 'ゃ', 'ゅ', 'ょ', 'っ', 'ゎ', 'ァ', 'ィ', 'ゥ', 'ェ', 'ォ',
        'ャ', 'ュ', 'ョ', 'ッ', 'ヮ',
    ];
    let mut out: Vec<String> = Vec::new();
    for c in text.chars() {
        if SMALL_KANA.contains(&c)
            && let Some(last) = out.last_mut()
        {
            last.push(c);
        } else {
            out.push(c.to_string());
        }
    }
    out
}

/// `from_id` を起点に、(start_beat asc → 同 beat なら pitch desc) 順で `count` 個の
/// 後続 note id を返す (起点 note 自身を `out[0]` に含む)。
///
/// `Ui::piano_roll` の歌詞 inline 編集 (M14 Phase 59 / daw_01 #017) が「Enter で commit
/// したモーラを起点 note + 後続 note に順次分配」するために使う内部 helper。
///
/// - `from_id` が見つからなければ空 Vec
/// - 後続 note が `count - 1` 個に満たなければ Vec の長さは `count` より小さくなる
/// - 順序は (start_beat asc, pitch desc): 同拍なら **高 pitch が先** (歌唱メロディと整合、
///   高い音を先に拾う)
///
/// `notes` が start_beat 昇順ソート済前提でも、同 beat 内の pitch 順までは保証されないため
/// 関数内で安定 sort をかける。
fn collect_next_notes_for_lyric(notes: &[Note], from_id: NoteId, count: usize) -> Vec<NoteId> {
    if count == 0 {
        return Vec::new();
    }
    let mut sorted: Vec<&Note> = notes.iter().collect();
    sorted.sort_by(|a, b| {
        a.start_beat
            .partial_cmp(&b.start_beat)
            .unwrap_or(std::cmp::Ordering::Equal)
            // pitch desc (高 pitch を先に)
            .then(b.pitch.cmp(&a.pitch))
    });
    let Some(pos) = sorted.iter().position(|n| n.id == from_id) else {
        return Vec::new();
    };
    sorted[pos..].iter().take(count).map(|n| n.id).collect()
}

// ============================================================
// Internal state (widget 内部のみ、`pub(crate)`)
// ============================================================

/// drag 開始時の各対象 note の値スナップショット。
#[derive(Clone, Copy, Debug)]
struct NoteDragAnchor {
    id: NoteId,
    start_beat: f64,
    pitch: u8,
    len_beats: f64,
}

/// 1 度の note drag セッション (move / resize / 複数同時)。
#[derive(Clone, Debug)]
struct NoteDragSession {
    kind: NoteDragKind,
    /// drag 開始時のマウス位置 (screen)。
    anchor_mouse: (f32, f32),
    anchors: Vec<NoteDragAnchor>,
    /// drag 中の最終 pointer 位置 (M9 Phase 60、 arrangement.rs と同パターン)。
    /// winit は release frame で `pointer.pos` を press 位置に戻すことがあり、 そのまま delta
    /// 計算すると beat_delta = 0 で commit されない / 「元に戻る」 ように見える bug への対策。
    /// release では `last_mouse` を delta 計算に使う = drag preview と一致する位置で確定。
    last_mouse: (f32, f32),
    /// drag 中の最終 alt 状態。 drag overlay と release commit の **両方** がこれを真値とする
    /// (`pointer.modifiers.alt` を直接見ない)。 continuation frame で毎 frame update し、
    /// release frame では `allow_update = false` で skip することで release 直前の値を保持する。
    /// これにより OS event 順序 (ModifiersChanged が MouseInput(Released) より先に来るケース)
    /// に依存せず、 overlay と commit が必ず同一値で確定する。
    last_alt: bool,
    /// (M14 Phase 83 / daw_01 #054) drag 中の最終 ctrl 状態。 `last_alt` と完全同型の
    /// careful-update (continuation frame で update / release frame は skip) で保持し、
    /// release frame の `Move` ↔ `Copy` 分岐と copy overlay の色が必ず同一値で確定する
    /// (OS の ModifiersChanged が Released より先に届くと ctrl が false に化けるのを回避)。
    last_ctrl: bool,
}

/// 空白ダブルクリック作成で「ドラッグした」と判定する左右いずれかの最小移動量 (px)。
/// これ未満は jitter とみなし既定長で作成 (note_drag の short-click 閾値 4px と同値)。
/// 左方向も対象 (左ドラッグで既定長より短く作るとき右へ振る手間を不要にする)。
const NOTE_CREATE_DRAG_PX: f32 = 4.0;

/// ドラッグで決める note 長の下限 (拍)。snap unit 無効時の floor。
/// daw_01 add_note 側が更に `0.0625` (1/16) に clamp するが、 widget preview と commit を
/// 一致させるため widget でも下限を設ける (resize の `0.05` floor と同値)。
const NOTE_CREATE_MIN_LEN: f64 = 0.05;

/// 空白ダブルクリック作成 session (Bitwig 流の「ダブルクリックのボタンを放さず
/// 左右ドラッグで note 長を決める」)。
///
/// `take_double_click_press_in_rect` が返した「2 度目の press」 が空白 grid 上だったときに開始。
/// press → drag → release で完結し、 release frame で **1 個の `PianoRollEditRequest::Add`** を
/// 発行する (= note 作成と長さ確定が 1 undo step に収まる)。 note drag (Move/Resize) と同 frame
/// に両方 active にならない (作成 press は空白 grid なので note_hit が None)。
///
/// **カーソル warp (Ableton Live 流)**: press 時にカーソルを既定長ノートの **右端** へ warp し、
/// `anchor_mouse` をその右端に置く。 これでカーソル＝掴んでいる右端が一致し、 「ドラッグ開始で
/// 右端がカーソル位置 (最短) に飛ぶ」 違和感を消す。 warp は非同期反映なので `warp_settled` で
/// 着地まで last_mouse 追従を止める (warp ジャンプを長さに混入させない)。
///
/// 長さの決め方 (右端 = start+default を起点にした相対 resize、 絶対位置 snap):
/// - `dragged == false` (まだ閾値ぶん動かしていない / 即放し) → `view.default_note_len_beats`。
/// - `dragged == true` (左右いずれかに閾値ぶん drag した) → `max(min_len, snap((start+default) +
///   raw_delta) − start)`。 右ドラッグで伸長、 左ドラッグで右端から短縮 (min_len まで)。
///
/// start_beat / pitch は press 時に確定 (= クリック位置の snap 済み beat と行 pitch)。 長さ軸
/// (左右) のみ扱い、 pitch (上下) は固定 (Bitwig は上下 drag で velocity だが本 #82 は長さに限定)。
#[derive(Clone, Copy, Debug)]
struct NoteCreateSession {
    /// 作成 note の開始拍 (song-absolute、 press 時に snap 済みで確定)。
    start_beat: f64,
    /// 作成 note の pitch (press 時の行で確定)。
    pitch: u8,
    /// **既定長ノートの右端** の screen x (= warp 先 = カーソルを移動した先)。 長さ計算の
    /// 起点 (raw_delta = last_mouse − anchor_mouse) かつ warp 着地判定の終点。 y は press y。
    anchor_mouse: (f32, f32),
    /// press した screen x (warp 前のカーソル位置)。 warp 着地判定 (press→anchor の中点越え) に使う。
    press_x: f32,
    /// drag 中の最終 pointer 位置 (note_drag と同パターン: winit release frame の pos 巻き戻し対策)。
    /// warp 着地までは anchor_mouse のまま保持 (= 既定長表示、 warp ジャンプを長さに混入させない)。
    last_mouse: (f32, f32),
    /// drag 中の最終 alt 状態 (snap 一時無効)。 continuation で update、 release で保持
    /// (`NoteDragSession.last_alt` と同 careful-update)。
    last_alt: bool,
    /// 左右いずれかに作成閾値 (4px) ぶん drag したか (latch)。 false の間は既定長、 true で右端追従長。
    dragged: bool,
    /// カーソル warp が着地したか。 press 後カーソルが press_x→anchor_mouse.x の中点を越えたら true。
    /// false の間 (warp 未反映) は last_mouse 追従を止めて既定長を保ち、 warp ジャンプ由来の
    /// `PointerMoved` を長さ計算に混入させない (= ドラッグ開始直後の一瞬の最短化を防ぐ)。
    warp_settled: bool,
}

/// (M14 Phase 64 / daw_01 #018) velocity lane 内 drag session。
///
/// note drag (Move/Resize) と独立: vel_area での press → release で完結する別状態。
/// 同 frame に両方は active にならない (pointer は press 時に grid か vel_area のどちらか)。
///
/// **絶対値 mode**: pointer.y を毎 frame `0..=127` に直接 map (Live / Cubase 流)。 anchor velocity
/// は短 click 判定 (drag<3px) と「変化なし note を Edit から除外」 用に保持。 spec 上 prev は
/// 持たないので `SetVelocity` は new value のみ伝える。
#[derive(Clone, Debug)]
struct VelocityDragSession {
    /// 影響範囲 ids: drag 起点で hit した note が selected に含まれるなら selected 全部、
    /// 含まれなければ単一 hit の id のみ。 起動時に固定し、drag 中の selected 変化には追従しない
    /// (= "drag 開始時点の意図" を保持)。 順序は selected の順 (overlay の id-list 共有用)。
    target_ids: Vec<NoteId>,
    /// drag 開始時の各 target の velocity (短 click 判定 + 「変化なし note 除外」 用)。
    anchor_velocities: Vec<(NoteId, u8)>,
    /// drag 開始時のマウス位置 (screen px)。 短 click 判定の基準。
    anchor_mouse: (f32, f32),
    /// drag 中の最終 pointer 位置 (note_drag と同パターン: winit release frame の pos 巻き戻し対策)。
    last_mouse: (f32, f32),
}

/// (M14 Phase 64 / daw_01 #018) `pointer.y` から絶対 velocity (0..=127) を計算。
///
/// `vel_area.y` (lane top) = 127、 `vel_area.y + vel_area.h` (lane bottom) = 0 として
/// 線形 map。 範囲外は clamp (lane の上を超えて drag したら 127、 下を超えたら 0)。
/// `vel_area.h <= 0` (= disabled) なら 0 を返す (defensive)。
fn velocity_from_y(py: f32, vel_area: Rect) -> u8 {
    if vel_area.h <= 0.0 {
        return 0;
    }
    let t = (1.0 - (py - vel_area.y) / vel_area.h).clamp(0.0, 1.0);
    (t * 127.0).round() as u8
}

/// (M14 Phase 64 / daw_01 #018) velocity lane 内の hit-test。
///
/// `cx` 位置にある note の velocity bar に hit するかを判定。 各 note の bar 中央 x は
/// `vel_area.x + (n.start_beat - view.start_beat) * beat_to_px`。 hit zone は **bar 中央から
/// 左右 ± `(velocity_bar_width_px / 2 + tolerance)` px**。 後勝ち (visible 順で前面)。
///
/// `cy` が `vel_area` 内かは caller 側で判定済み前提 (この関数は x 方向のみ判定)。
/// 戻り値 `None` は「この cx に bar 無し」 (lane 余白のクリック)。
fn velocity_bar_hit(
    visible: &[Note],
    view: PianoRollView,
    vel_area: Rect,
    cx: f32,
    bar_width: f32,
    tolerance: f32,
) -> Option<NoteId> {
    let beat_to_px = f64::from(vel_area.w) / view.len_beats.max(1e-6);
    let half_w = bar_width * 0.5 + tolerance;
    let mut hit: Option<NoteId> = None;
    for n in visible {
        let nx = vel_area.x + ((n.start_beat - view.start_beat) * beat_to_px) as f32;
        if (cx - nx).abs() <= half_w {
            hit = Some(n.id);
        }
    }
    hit
}

/// 絶対位置 snap で計算した note drag の beat delta (overlay と release commit で共有)。
/// anchor 0 の編集対象端 (Move=start / ResizeRight=end / ResizeLeft=start) の絶対位置を
/// snap → その差分を全 anchor に適用 (相対関係維持 + anchor 0 が grid に着地)。 anchors が
/// 空のときは raw を返す (defensive)。
fn compute_note_drag_beat_delta(
    nd: &NoteDragSession,
    raw_beat_delta: f64,
    snap: &SnapConfig,
    zoom_x_px_per_beat: f32,
) -> f64 {
    let Some(a0) = nd.anchors.first() else {
        return raw_beat_delta;
    };
    let pivot = match nd.kind {
        NoteDragKind::Move | NoteDragKind::ResizeLeft => a0.start_beat,
        NoteDragKind::ResizeRight => a0.start_beat + a0.len_beats,
    };
    let snapped_pivot =
        snap.snap_beat(pivot + raw_beat_delta, nd.last_alt, zoom_x_px_per_beat);
    snapped_pivot - pivot
}

/// M14 Phase 61b (#011): arrangement と同根。 caller の `notes_generation` は note 数や編集
/// epoch のみで bump しがちで、 個別 note の `(id, start_beat, len_beats, pitch, velocity,
/// style)` 変化が漏れると drag 残像 / stale な dim・lock・色が発生する。 widget 内部で全
/// visible note を fold して viewport_key に追加する (caller boilerplate 強要回避、
/// `feedback_pursue_best_practice`)。
///
/// fold するのは **cached 層 (`draw_notes`) が実際に描画へ使う field だけ**。 `lyric` は
/// 含めない: 歌詞は cache の外の `draw_lyrics` が毎フレーム描画するので key に不要で、
/// さらに caller (daw_gui) は `Option<Arc<str>>` を毎フレーム `Arc::from` で再構築するため
/// identity (ptr) hash は「内容不変でも毎フレーム key が変わる」= cache を恒常 miss させて
/// 大規模クリップの背景全再描画を毎フレーム走らせてしまう。 歌詞の内容変化は caller の
/// `notes_generation` (内容 hash) が担う。
///
/// 5000 note × 8 fold step = ~7μs @ 4GHz、 16ms 予算の 0.05%。
fn fold_piano_roll_note_hash(notes: &[Note]) -> u64 {
    const PRIME: u64 = 0x100_0000_01B3; // FNV-1a 64bit prime
    let mut h: u64 = 0xCBF2_9CE4_8422_2325; // FNV-1a 64bit offset basis
    for n in notes {
        h ^= u64::from(n.id);
        h = h.wrapping_mul(PRIME);
        h ^= n.start_beat.to_bits();
        h = h.wrapping_mul(PRIME);
        h ^= n.len_beats.to_bits();
        h = h.wrapping_mul(PRIME);
        h ^= u64::from(n.pitch);
        h = h.wrapping_mul(PRIME);
        h ^= u64::from(n.velocity);
        h = h.wrapping_mul(PRIME);
        // mute / dim / lock / クリップ色は cached 層の note fill 描画の入力なので
        // fold する (トグルや対象クリップ切替が scroll 等の別 invalidation を
        // 待たず即時反映されるように)。
        h ^= u64::from(n.muted)
            | (u64::from(n.style.dimmed) << 1)
            | (u64::from(n.style.locked) << 2);
        h = h.wrapping_mul(PRIME);
        if let Some(c) = n.style.color {
            h ^= (u64::from(c.r.to_bits()) << 32) | u64::from(c.g.to_bits());
            h = h.wrapping_mul(PRIME);
            h ^= (u64::from(c.b.to_bits()) << 32) | u64::from(c.a.to_bits());
            h = h.wrapping_mul(PRIME);
        }
    }
    h
}

/// piano_roll widget の永続状態 (`UiHost.state` に置かれる)。
#[derive(Debug, Default)]
pub(crate) struct PianoRollState {
    /// note drag (Move / ResizeLeft / ResizeRight) の anchor。drag release で None に戻す。
    note_drag: Option<NoteDragSession>,
    /// (M14 Phase 59 / daw_01 #017) 歌詞編集中の note id (text_input overlay 表示中)。
    /// L キー検知で `Some(selected[0])` に遷移 (`selected.len() == 1` のときのみ)、
    /// Enter で 1) commit + 分配 → 2) 次 note へ移動 or `None` 復帰、Esc で `None`、
    /// 編集対象 note が消失したら defensive で `None`。`Some(_)` のとき drag/resize/wheel/click
    /// は全て短絡 (typing_focus が立つので global shortcut も自動抑制)。
    lyric_editing: Option<NoteId>,
    /// (M14 Phase 64 / daw_01 #018) velocity lane 内 drag session。
    /// vel_area での press → release で完結。 note_drag と同時には active にならない。
    velocity_drag: Option<VelocityDragSession>,
    /// (M14 Phase 69 / daw_01 #041) ruler 上の plain (= Shift 非保持) click / drag による
    /// playhead seek session。 press / continuation で `SetPlayheadBeat` を逐次発火、
    /// release で take して discard (commit-by-release 無し、 arrangement #024 と同 idiom)。
    playhead_drag: Option<PlayheadDragSession>,
    /// (M14 Phase 69 / daw_01 #041) ruler 上の Shift + drag による loop range edit session。
    /// release frame で `compute_loop_drag_endpoints` 経由で snap 適用済 endpoints を計算し、
    /// `SetLoopRange` を 1 度だけ発火 (arrangement #024 と同 idiom)。
    loop_drag: Option<LoopDragSession>,
    /// (M14 Phase 84 / daw_01 #055) 鍵盤レーン press session。press 開始が kbd rect 内のとき
    /// `true`、release で `false`。grid の note drag とは独立 (領域が x で排他)。押下中の pitch は
    /// 毎フレーム pointer.y から計算するので held 値は持たず、「press 開始が鍵盤か」だけを track する。
    keyboard_pressing: bool,
    /// 空白ダブルクリック作成 session (押下のまま drag で note 長を決める)。
    /// 2 度目の press で `Some`、release で `take` して 1 個の `Add` を発行 → `None`。
    note_create: Option<NoteCreateSession>,
    /// edge auto-scroll の pitch (縦) 方向の端数アキュムレータ (semitone)。
    /// top_pitch は u8 なので sub-semitone のスクロールを表現できない。drag 中に毎フレーム
    /// `dy_px * pitch_per_px` を貯め、|累積| ≥ 1 で整数 semitone ぶん `SetTopPitch` を発火する
    /// (= zone 内側ほどゆっくり、外側ほど速く滑らかにスクロール)。縦 zone を外れた frame で 0 に reset。
    edge_pitch_accum: f32,
    /// edge auto-scroll の移動量ゲート用 press 位置。press からの移動が `ACTIVATE_PX`
    /// 以上のときのみ端スクロールを許可し、端近くの note を click-and-hold しただけで view が動くのを防ぐ。
    edge_scroll_press: Option<(f32, f32)>,
}

// ============================================================
// Internal helpers (描画)
// ============================================================

/// note rect を生成 (角丸 + radius 指定)。
fn note_rect_command(rect: Rect, fill: Color, radius_px: f32) -> RectCommand {
    RectCommand {
        rect,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [radius_px; 4],
        clip_rect: None,
    }
}

/// drag preview の shifted note geometry を計算 (drag 中の表示用、元 Note は不変)。
/// kind に応じて start_beat / pitch / len_beats を delta で更新した tuple を返す
/// (Note を返さないのは Note が `Arc<str>` lyric を持つので Copy できないため、
/// drag preview で必要な geometry 3 つだけ返す)。
fn drag_preview_geometry(
    anchor: NoteDragAnchor,
    kind: NoteDragKind,
    beat_delta: f64,
    pitch_delta: i32,
    min_len: f64,
    view: PianoRollView,
    last_alt: bool,
) -> (f64, f64, u8) {
    match kind {
        NoteDragKind::Move => (
            (anchor.start_beat + beat_delta).max(0.0),
            anchor.len_beats,
            apply_pitch_drag_delta(anchor.pitch, pitch_delta, view, last_alt),
        ),
        NoteDragKind::ResizeRight => (
            anchor.start_beat,
            (anchor.len_beats + beat_delta).max(min_len),
            anchor.pitch,
        ),
        NoteDragKind::ResizeLeft => {
            let max_start = anchor.start_beat + anchor.len_beats - min_len;
            let new_start = (anchor.start_beat + beat_delta).clamp(0.0, max_start);
            let actual_delta = new_start - anchor.start_beat;
            (new_start, (anchor.len_beats - actual_delta).max(min_len), anchor.pitch)
        }
    }
}

/// note_create session の現在の `(start_beat, len_beats, pitch)` を計算
/// (drag preview と release commit で共有 = 描画と確定が必ず一致)。
///
/// **モデル: 既定長ノートの「右端」を掴んで動かす相対 resize** (Ableton Live / ダブルクリックで
/// カーソルが右端へ warp し、 そこを掴んでいる感覚)。 `anchor_mouse` は warp 先 (= 右端 screen x)
/// なので、 ドラッグ開始時 (cursor == anchor) の `raw_delta == 0` で長さは既定長のまま (= 最短へ
/// 飛ばない)。 そこからの移動量ぶんだけ右端が動く (cursor がそのまま右端に追従)。
///
/// - `dragged == false` (即放し / jitter 以下 / warp 未着地): 長さ = `view.default_note_len_beats`
///   (= caller の `last_note_duration_beats`)。 `0.0625` (1/16) 下限。
/// - `dragged == true` (warp 着地後に左右いずれかへ閾値ぶん drag した): 右端 pivot = `start + default`、
///   warp 先からの移動量 `raw_delta = (last_mouse.x − anchor_mouse.x) × beat_per_px` を pivot に足して
///   **絶対位置 snap** (`snap(pivot + raw_delta)`、 ui/CLAUDE.md の delta-snap NG ガイドライン /
///   note_drag ResizeRight と同方式。 anchor が右端なので実効的に `snap(cursor 位置)` = 右端が
///   cursor に一致)。 長さ = `max(min_len, snapped_right − start)`。 右ドラッグで伸長、 左ドラッグで
///   右端から短縮 (min_len まで)。 alt は session の `last_alt` を真値とし `pointer.modifiers.alt` を
///   直接見ない (overlay と commit の一致)。
fn note_create_geometry(
    nc: &NoteCreateSession,
    view: PianoRollView,
    beat_per_px: f64,
    zoom_x_px_per_beat: f32,
) -> (f64, f64, u8) {
    let default_len = view.default_note_len_beats.max(0.0625);
    let len = if nc.dragged {
        let raw_delta = f64::from(nc.last_mouse.0 - nc.anchor_mouse.0) * beat_per_px;
        let pivot = nc.start_beat + default_len;
        let right = view.snap.snap_beat(pivot + raw_delta, nc.last_alt, zoom_x_px_per_beat);
        let min_len = if view.snap.is_active(nc.last_alt) {
            view.snap
                .beat_unit(zoom_x_px_per_beat)
                .map_or(NOTE_CREATE_MIN_LEN, |u| u.max(NOTE_CREATE_MIN_LEN))
        } else {
            NOTE_CREATE_MIN_LEN
        };
        (right - nc.start_beat).max(min_len)
    } else {
        default_len
    };
    (nc.start_beat, len, nc.pitch)
}

// ============================================================
// Public widget API
// ============================================================

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// piano roll widget (M9 Phase 41e)。
    ///
    /// - `id`: 同 frame 内で複数 piano_roll を並べる場合は `(label, index)` 等で一意に。
    /// - `rect`: 描画矩形 (左端 `style.keyboard_w` 分は keyboard 領域、残りが grid)。
    /// - `notes`: 描画対象の note 列 (start_beat 昇順ソート前提、二分探索で visible filter)。
    /// - `view`: pan/zoom 状態 (値渡し)。pan/zoom 更新は user 側責務 (widget は描画のみ)。
    /// - `selected`: 選択中 note の id 集合 (immutable borrow、Model 側 single source of truth)。
    /// - `style`: 見た目スタイル (`PianoRollStyle::default()` で example の見た目を再現)。
    /// - `make_edit`: 各種 Edit 要求を `Edit<M>` に変換する callback。
    ///   `Add` / `Delete` / `Move` / `Resize` / `Select` の 5 variant を dispatch する。
    ///
    /// 戻り値 `PianoRollResponse` で hover / drag / selection 変化を取得できる。
    ///
    /// # 操作
    /// - **note 中央 drag** = move (release で `PianoRollEditRequest::Move` 発行、Undoable)
    /// - **note 中央 Ctrl+drag** = copy (release で `PianoRollEditRequest::Copy` 発行、Undoable、daw_01 #054)
    /// - **note 左右端 drag** = resize (release で `PianoRollEditRequest::Resize` 発行、Undoable)
    /// - **note click** (drag<4px) = selection 1 個 (`PianoRollEditRequest::Select` 発行)
    /// - **空白 drag** (無修飾) = rect marquee select、**REPLACE** (rect 内 note ids で置換、#102)。
    ///   `Shift+drag` = **UNION** (既存 `selected` ∪ rect 内)、`Ctrl+drag` = **XOR** (toggle)。
    ///   いずれも release で `PianoRollEditRequest::Select` 発行 (Undoable)
    /// - **空白 click** (無修飾、drag<4px) = selection clear (= zero-rect の REPLACE marquee)
    /// - **Insert** shortcut = pointer 位置に新規 note 追加 (`PianoRollEditRequest::Add`)。
    ///   `id` は user 側で `next_note_id` 等で割り当て、`make_edit` callback 内で参照する
    ///   ため、widget は **id=0 placeholder で `Add(vec![note_with_id_0])` を渡す**。
    ///   user 側で id を上書きしてから push (= user 側で `m.next_note_id` を bump)。
    /// - **Delete** shortcut = selected を一括削除 (`PianoRollEditRequest::Delete`)
    /// - **note hover** で cursor を `Move` / `EwResize` に切替
    ///
    /// # pan/zoom について
    ///
    /// widget は pan/zoom を自前で扱わない (view ownership は app 側)。
    /// user は widget の **外** で `if resp.dragging.is_none() { /* pan logic */ }` のように
    /// drag 中でないとき pan を実装する。または `note_hit(...)` を呼んで note 上でない
    /// press のみ pan を始める (= 現 example の semantics)。
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn piano_roll<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        notes: &[Note],
        view: PianoRollView,
        selected: &[NoteId],
        style: &PianoRollStyle,
        make_edit: F,
    ) -> PianoRollResponse
    where
        F: Fn(PianoRollEditRequest) -> Edit<M> + Send + Sync + 'static,
    {
        let wid = WidgetId::ROOT.child((b"piano_roll_widget", &id));
        let pointer = self.pointer;

        // ===== M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 mode =====
        // Frame 開始時、lyric_editing が selected と sync しているか defensive check。
        // 編集対象 note が消失したら自動で None に戻す (note 削除等のため)。
        let mut lyric_editing: Option<NoteId> = {
            let state: &mut PianoRollState = self.widget_state(wid);
            if let Some(eid) = state.lyric_editing
                && !notes.iter().any(|n| n.id == eid)
            {
                state.lyric_editing = None;
            }
            state.lyric_editing
        };
        // L キー検知: lyric_editing == None かつ selected.len() == 1 のときのみ起動。
        // `"piano_roll.edit_lyric"` は `is_typing_only_shortcut` に追加済 (M14 Phase 59)。
        // 編集中 (typing_focus = true) は shortcut layer を素通りして text_input に届く
        // (= `'l'` 文字としてタイプ可能)。take_shortcut は frame 頭の typing_lock 判定後
        // pending_shortcuts に積まれた name を引くので、編集中は false を返す。
        // 選択条件 (`selected.len() == 1`) を `take_shortcut` より **先** に評価する —
        // 逆順だと条件を満たさない instance が L を黙って消費し、 同 frame の後続
        // instance (条件を満たす方) から shortcut を奪う (review)。
        if lyric_editing.is_none()
            && selected.len() == 1
            && let Some(name) = style.lyric_edit_shortcut
            && self.take_shortcut(name)
        {
            lyric_editing = Some(selected[0]);
            // 編集モードに入る瞬間、stale な note_drag セッションを clear (drag 中に L
            // を押した稀なケース対策)。
            let state: &mut PianoRollState = self.widget_state(wid);
            state.lyric_editing = lyric_editing;
            state.note_drag = None;
        }
        // Esc 検知: 編集モード中の Esc は "escape" shortcut が global で consume するため
        // text_input の自前 Esc ハンドラ経由ではなく piano_roll が明示的に handle する
        // (= take_shortcut("escape") で消費 → lyric_editing = None で即時 cancel)。
        // これで「編集中の Esc → 1 frame で完全 cancel」を保証 (text_input の blur 検出
        // 経路 (resp.focused = false) は外 click 等の defensive fallback として残す)。
        if let Some(edit_id_for_esc) = lyric_editing
            && self.take_shortcut("escape")
        {
            // text_input の focus を明示的に clear (text_input id は ("piano_roll_lyric", edit_id))
            let ti_wid =
                WidgetId::ROOT.child((b"text_input", &("piano_roll_lyric", edit_id_for_esc)));
            self.clear_focus_if_focused(ti_wid);
            lyric_editing = None;
            self.widget_state::<PianoRollState>(wid).lyric_editing = None;
        }
        let editing_mode = lyric_editing.is_some();

        // grid / keyboard / velocity lane / ruler レイアウト
        // M13 Phase 55: rect の top から `ruler_h` 分を ruler 領域、その下に main_h
        // (= keyboard + grid)、最下段に vel_h (velocity lane)。`ruler_h = 0.0` で旧互換。
        let kbd_w = view.keyboard_w.max(0.0);
        let ruler_h = view.ruler_h.max(0.0).min(rect.h * 0.5);
        let vel_h = view.velocity_lane_h.max(0.0).min((rect.h - ruler_h) * 0.5);
        let main_h = (rect.h - ruler_h - vel_h).max(1.0);
        let ruler = Rect {
            x: rect.x + kbd_w,
            y: rect.y,
            w: (rect.w - kbd_w).max(1.0),
            h: ruler_h,
        };
        let grid = Rect {
            x: rect.x + kbd_w,
            y: rect.y + ruler_h,
            w: (rect.w - kbd_w).max(1.0),
            h: main_h,
        };
        let kbd = Rect { x: rect.x, y: rect.y + ruler_h, w: kbd_w, h: main_h };
        let vel_area = Rect {
            x: rect.x + kbd_w,
            y: rect.y + ruler_h + main_h,
            w: (rect.w - kbd_w).max(1.0),
            h: vel_h,
        };

        // visible filter (二分探索)
        let view_end_beat = view.start_beat + view.len_beats;
        let s_idx =
            notes.partition_point(|n| n.start_beat + n.len_beats < view.start_beat);
        let e_idx = s_idx
            + notes[s_idx..].partition_point(|n| n.start_beat <= view_end_beat);
        let visible: &[Note] = &notes[s_idx..e_idx];

        // ----- press 振り分け (state 更新) -----
        // 空き grid の drag は marquee (take_drag_rect_in_rect) が drag state を握るので (#102、
        // gate の `note_hit().is_none()` で除外)、 ここでは「Shift なし note hit」だけ widget が drag を
        // 始める。 この **!shift gate** は load-bearing: marquee gate が `note_hit().is_none()` を持つ前提で、
        // Shift+note press が note drag を起動しない (= marquee にも行かない) ことで成立する。
        // M14 Phase 59: editing_mode 中は drag/click を全短絡。
        let just_pressed_on_note = !editing_mode
            && pointer.primary_just_pressed
            && !pointer.modifiers.shift
            && pointer.pos.is_some_and(|(px, py)| grid.contains(px, py));

        if just_pressed_on_note
            && let Some((px, py)) = pointer.pos
            && let Some((hit_id, kind)) =
                note_hit(notes, view, grid, px, py, style.resize_handle_px)
        {
            let drag_ids: Vec<NoteId> = if selected.contains(&hit_id) {
                selected.to_vec()
            } else {
                vec![hit_id]
            };
            let anchors: Vec<NoteDragAnchor> = drag_ids
                .iter()
                .filter_map(|id_target| {
                    notes.iter().find(|n| n.id == *id_target).map(|n| NoteDragAnchor {
                        id: n.id,
                        start_beat: n.start_beat,
                        pitch: n.pitch,
                        len_beats: n.len_beats,
                    })
                })
                .collect();
            if !anchors.is_empty() {
                let press_alt = pointer.modifiers.alt;
                let press_ctrl = pointer.modifiers.ctrl;
                let state: &mut PianoRollState = self.widget_state(wid);
                state.note_drag = Some(NoteDragSession {
                    kind,
                    anchor_mouse: (px, py),
                    anchors,
                    last_mouse: (px, py),
                    last_alt: press_alt,
                    last_ctrl: press_ctrl,
                });
            }
        }

        // ----- 鍵盤レーン press session (M14 Phase 84 / daw_01 #055) -----
        // press 開始が kbd rect 内なら keyboard preview session を開始する (note drag とは x 領域で
        // 排他、grid.contains で gate される just_pressed_on_note とは独立)。release で終了。
        // editing_mode 中は無効 (歌詞 typing 優先)。pitch は後段の response 計算で毎フレーム算出。
        {
            let state: &mut PianoRollState = self.widget_state(wid);
            if pointer.primary_just_pressed
                && !editing_mode
                && pointer.pos.is_some_and(|(px, py)| kbd.contains(px, py))
            {
                state.keyboard_pressing = true;
            }
            if pointer.primary_just_released {
                state.keyboard_pressing = false;
            }
        }

        // ----- velocity lane press 振り分け (M14 Phase 64 / daw_01 #018) -----
        // vel_h > 0 のとき vel_area 内 press でかつ velocity bar 上なら velocity_drag 開始。
        // bar 上でなければ何もしない (= lane 余白 click は no-op で selection も変えない)。
        // editing_mode / Shift 押下中 / note_drag 既に active のときは skip (排他)。
        let just_pressed_in_vel_lane = !editing_mode
            && pointer.primary_just_pressed
            && !pointer.modifiers.shift
            && vel_h > 0.0
            && pointer.pos.is_some_and(|(px, py)| vel_area.contains(px, py));
        if just_pressed_in_vel_lane
            && self.widget_state::<PianoRollState>(wid).note_drag.is_none()
            && let Some((px, py)) = pointer.pos
            && let Some(hit_id) = velocity_bar_hit(
                visible,
                view,
                vel_area,
                px,
                style.velocity_bar_width_px,
                4.0,
            )
        {
            let target_ids: Vec<NoteId> = if selected.contains(&hit_id) {
                selected.to_vec()
            } else {
                vec![hit_id]
            };
            let anchor_velocities: Vec<(NoteId, u8)> = target_ids
                .iter()
                .filter_map(|id_target| {
                    notes
                        .iter()
                        .find(|n| n.id == *id_target)
                        .map(|n| (n.id, n.velocity))
                })
                .collect();
            if !anchor_velocities.is_empty() {
                let final_targets: Vec<NoteId> =
                    anchor_velocities.iter().map(|(id, _)| *id).collect();
                let state: &mut PianoRollState = self.widget_state(wid);
                state.velocity_drag = Some(VelocityDragSession {
                    target_ids: final_targets,
                    anchor_velocities,
                    anchor_mouse: (px, py),
                    last_mouse: (px, py),
                });
            }
        }

        // ----- drag continue (描画用 delta を計算) + release 検出 -----
        // 拍は f64、pixel は f32 なので変換を 1 箇所で吸収。 view.len_beats==0 は他の helper
        // ([:369, :651]) と同じく 1e-6 floor で防御 (0 除算で inf が伝播するのを回避)。
        let safe_len_beats = view.len_beats.max(1e-6);
        let beat_per_px: f64 = safe_len_beats / f64::from(grid.w.max(1.0));
        // SnapConfig::Adaptive 用 zoom = grid.w / view.len_beats。
        let zoom_x_px_per_beat: f32 = (1.0 / beat_per_px) as f32;
        let pitch_per_px = view.pitch_visible / grid.h.max(1.0);

        // ----- 空白ダブルクリック作成 press の検出 -----
        // 「double-click の 2 度目の press」が空白 grid 上 (note_hit なし) なら note 作成 session を
        // 開始する。press 即時に取るので、このままボタンを放さず drag すれば長さを決められる
        // (Bitwig 流「continue to hold the mouse down, and then drag left or right to ... lengthen」)。
        // start_beat (snap) と pitch (行 ceil = Insert と同式) を press 位置で確定。長さ軸 (左右)
        // のみ扱い pitch は固定。editing_mode / note_drag 既存中は skip。`note_create_press` は
        // 下の marquee gate でも参照し、この press を marquee が二重に所有しないよう抑制する。
        let note_create_press: Option<(f32, f32)> = if editing_mode {
            None
        } else {
            self.take_double_click_press_in_rect(grid)
        };
        if let Some((px, py)) = note_create_press
            && note_hit(notes, view, grid, px, py, style.resize_handle_px).is_none()
            && self.widget_state::<PianoRollState>(wid).note_drag.is_none()
        {
            let press_alt = pointer.modifiers.alt;
            let raw_start = (view.start_beat + f64::from(px - grid.x) * beat_per_px).max(0.0);
            let start_beat = view
                .snap
                .snap_beat(raw_start, press_alt, zoom_x_px_per_beat)
                .max(0.0);
            // Insert / 旧 dbl-click 作成と同じ ceil 逆写像 (#012)。Fold mode も RowGeometry が吸収。
            let geom = RowGeometry::compute(view, grid);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pitch = (geom.y_to_pitch_f(py).ceil() as i32).clamp(0, 127) as u8;
            // warp 先 = 既定長ノートの右端の screen x (Ableton Live 流)。
            // カーソルをここへ動かし、 anchor をここに置く = カーソル＝掴んでいる右端が一致。
            let default_len = view.default_note_len_beats.max(0.0625);
            #[allow(clippy::cast_possible_truncation)]
            let warp_x = grid.x + ((start_beat + default_len - view.start_beat) / beat_per_px) as f32;
            self.warp_cursor(warp_x, py);
            let state: &mut PianoRollState = self.widget_state(wid);
            state.note_create = Some(NoteCreateSession {
                start_beat,
                pitch,
                anchor_mouse: (warp_x, py),
                press_x: px,
                last_mouse: (warp_x, py),
                last_alt: press_alt,
                dragged: false,
                warp_settled: false,
            });
        }

        // ----- ruler press 振り分け (M14 Phase 69 / daw_01 #041) -----
        // arrangement #024 と完全同 idiom: plain (= Shift 非保持) は playhead seek、
        // Shift 押下で loop range edit (NewRange / Start/End/Middle drag)。
        // editing_mode 中 / ruler_h <= 0 のときは一切処理しない (= 既存挙動完全互換)。
        // grid / vel_area とは y 軸で完全分離されているので note_drag / velocity_drag と
        // 競合せず、 振り分け順序の制約はない (= 独立 block)。
        //
        // `view.ruler_h <= 0.0` のとき `ruler_h = view.ruler_h.max(0.0).min(rect.h * 0.5)` が
        // 0.0 になり ruler.h も 0、 ruler.contains は y 1 行を判定するが帯がないので普通の
        // pointer.pos は決して入らない (= defensive で skip しなくても安全)。 ただし明示的に
        // gate しておく方が読みやすいので `ruler_h > 0.0` 条件を入れる。
        let mut press_seek_beat: Option<f64> = None;
        if !editing_mode
            && ruler_h > 0.0
            && pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && ruler.contains(px, py)
        {
            let press_beat =
                view.start_beat + f64::from(px - ruler.x) * beat_per_px;
            let press_alt = pointer.modifiers.alt;
            if pointer.modifiers.shift {
                // Shift + ruler drag → loop range edit (NewRange / Start/End/Middle handle)。
                let kind = if let Some(range) = view.loop_range {
                    match loop_band_hit_kind(
                        range,
                        view.start_beat,
                        view.len_beats,
                        ruler,
                        px,
                        4.0,
                    ) {
                        Some(LoopBandHit::Start) => LoopDragKind::Start,
                        Some(LoopBandHit::End) => LoopDragKind::End,
                        Some(LoopBandHit::Middle) => LoopDragKind::Middle,
                        None => LoopDragKind::NewRange,
                    }
                } else {
                    LoopDragKind::NewRange
                };
                // NewRange の anchor 端点は press 時 snap で grid に着地 (release 端点も
                // `compute_loop_drag_endpoints` で snap される、 arrangement #024 と同 idiom)。
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
                let state: &mut PianoRollState = self.widget_state(wid);
                state.loop_drag = Some(LoopDragSession {
                    kind,
                    anchor_loop,
                    anchor_press_beat: anchor_press_beat_for_session,
                    anchor_mouse_x: px,
                    last_mouse_x: px,
                    last_alt: press_alt,
                });
            } else {
                // plain (Shift 非保持) ruler click/drag → playhead seek session。
                let snapped = view
                    .snap
                    .snap_beat(press_beat, press_alt, zoom_x_px_per_beat)
                    .max(0.0);
                let state: &mut PianoRollState = self.widget_state(wid);
                state.playhead_drag = Some(PlayheadDragSession {
                    last_mouse_x: px,
                    last_emitted_beat: snapped,
                });
                press_seek_beat = Some(snapped);
            }
        }
        if let Some(beat) = press_seek_beat {
            self.push_edit(make_edit(PianoRollEditRequest::SetPlayheadBeat(beat)));
        }

        // drag 継続中は毎 continuation frame で `last_mouse` / `last_alt` を update。
        // **release frame の `last_alt` は update しない** — 同 frame に ModifiersChanged(alt=false)
        // が先行する現象 (alt が一瞬 false に化ける) を回避するため、 release 直前 frame の値を保持する。
        // **release frame の `last_mouse` は pointer.pos が anchor と異なる場合のみ update** —
        // winit は release frame で `pointer.pos` を press 位置に戻すことがあり、 そのまま上書きすると
        // delta=0 で commit not pushed (drag が「元に戻る」 ように見える)。 pointer.pos == anchor のときは
        // continuation 由来の last_mouse を保持し、 そうでないときは pointer.pos が真値 (= 通常 release
        // pos、 OR press → 1 frame で release した short drag の release pos) として update する。
        if let Some((px, py)) = pointer.pos {
            let alt_now = pointer.modifiers.alt;
            let ctrl_now = pointer.modifiers.ctrl;
            let state: &mut PianoRollState = self.widget_state(wid);
            if let Some(ref mut nd) = state.note_drag {
                if !pointer.primary_just_released {
                    nd.last_mouse = (px, py);
                    nd.last_alt = alt_now;
                    nd.last_ctrl = ctrl_now;
                } else if (px, py) != nd.anchor_mouse {
                    nd.last_mouse = (px, py);
                }
            }
            // note_create continuation。 まず warp 着地判定: press_x → anchor (右端) の
            // 中点をカーソルが越えたら settled。 settled までは last_mouse を更新せず anchor のまま
            // 保持する (warp の非同期ジャンプ由来の `PointerMoved` を長さに混入させない = ドラッグ
            // 開始直後の一瞬の最短化を防ぐ)。 settled 後は note_drag と同じ winit release-frame 巻き
            // 戻し対策で last_mouse / last_alt を update し、 左右いずれかに作成閾値 (4px) ぶん動いたら
            // `dragged` を latch (一度立てば解除しない)。 **左方向も latch 対象**: 右端から左へ短縮
            // するとき一度右へ振り直す手間を不要にする (Bitwig「drag left or right to shorten or lengthen」)。
            if let Some(ref mut nc) = state.note_create {
                if !nc.warp_settled {
                    let mid = (nc.press_x + nc.anchor_mouse.0) * 0.5;
                    // 右端 (anchor) は press_x 以上にあるので、 中点以上 = warp が反映された。
                    if px >= mid {
                        nc.warp_settled = true;
                    }
                }
                if nc.warp_settled {
                    if !pointer.primary_just_released {
                        nc.last_mouse = (px, py);
                        nc.last_alt = alt_now;
                        if (px - nc.anchor_mouse.0).abs() >= NOTE_CREATE_DRAG_PX {
                            nc.dragged = true;
                        }
                    } else if (px, py) != nc.anchor_mouse {
                        nc.last_mouse = (px, py);
                        if (px - nc.anchor_mouse.0).abs() >= NOTE_CREATE_DRAG_PX {
                            nc.dragged = true;
                        }
                    }
                }
            }
            // velocity_drag 側も同様に last_mouse update (note_drag と同じ winit release frame
            // pos 巻き戻し対策)。 alt は velocity drag の挙動に影響しない (絶対値 mode 固定)。
            // continuation frame は常に update、 release frame は pointer.pos が anchor と異なる
            // ときのみ update (winit が release frame で pointer.pos を press 位置に巻き戻す bug 対策)。
            if let Some(ref mut vd) = state.velocity_drag
                && (!pointer.primary_just_released || (px, py) != vd.anchor_mouse)
            {
                vd.last_mouse = (px, py);
            }
            // (M14 Phase 69 / daw_01 #041) loop_drag continuation:
            // last_mouse_x / last_alt を update (arrangement と完全同 idiom)。 release frame で alt を
            // 上書きしないのは ModifiersChanged 先行の race 回避 (note_drag と同根)。
            if let Some(ref mut ld) = state.loop_drag {
                if pointer.primary_just_released {
                    // release frame は winit が pointer.pos を press 位置に巻き戻す場合があるため、
                    // anchor_mouse_x と同値 (= exact f32 equality) のときは update を skip し、
                    // continuation 由来の last_mouse_x を保持する (note_drag の `(px, py) != nd.anchor_mouse`
                    // tuple 比較と同 idiom)。 ここは exact equality が意味を持つ (winit の巻き戻しは
                    // bit-perfect な復元なので f32::EPSILON より厳しい比較を要求するわけではない)。
                    #[allow(clippy::float_cmp)]
                    let pos_moved = px != ld.anchor_mouse_x;
                    if pos_moved {
                        ld.last_mouse_x = px;
                    }
                } else {
                    ld.last_mouse_x = px;
                    ld.last_alt = alt_now;
                }
            }
            // playhead_drag は release では emit しないので last_mouse_x の release frame 巻き戻し
            // を気にする必要が無い (continuation の最後の `last_emitted_beat` が真値)。 ただし
            // continuation frame では update して将来の visual debug を可能にする。
            if let Some(ref mut pd) = state.playhead_drag
                && !pointer.primary_just_released
            {
                pd.last_mouse_x = px;
            }
        }

        // ---- ドラッグ端オートスクロール (piano roll) ----
        // drag 中、pointer が grid 端の hot-zone に入ったら view を自動スクロールし、掴んでいる対象が
        // カーソルに追従し続ける。横 (beat) は `ScrollByBeats` delta、縦 (pitch) は `SetTopPitch` 絶対値
        // (top_pitch は u8 なので端数を `edge_pitch_accum` に貯めて整数 semitone 単位で発火)。note drag /
        // create は相対 delta なので実スクロール px ぶん anchor を逆 shift して追従させる。ruler の
        // loop/playhead は絶対 px→beat 再解決で自動追従するため shift しない。`request_redraw` で次フレーム
        // を確保し、カーソルを端で止めたままでもスクロール継続させる。
        if pointer.primary_pressed && !pointer.primary_just_released {
            // 移動量ゲート: press からの移動が ACTIVATE_PX 以上のときのみ端スクロールを許可。
            let moved_enough = {
                let state: &mut PianoRollState = self.widget_state(wid);
                if pointer.primary_just_pressed {
                    state.edge_scroll_press = pointer.pos;
                    // 新しい drag の開始で pitch アキュムレータをリセット (前 drag の端数が
                    // 残って次 drag 初回フレームで pitch がジャンプするのを防ぐ)。
                    state.edge_pitch_accum = 0.0;
                }
                let gate = crate::widgets::edge_scroll::ACTIVATE_PX;
                matches!((state.edge_scroll_press, pointer.pos),
                    (Some(p), Some(c)) if (c.0 - p.0).powi(2) + (c.1 - p.1).powi(2) >= gate * gate)
            };
            let axes: Option<(bool, bool)> = if moved_enough {
                let state: &mut PianoRollState = self.widget_state(wid);
                if let Some(nd) = state.note_drag.as_ref() {
                    Some(match nd.kind {
                        NoteDragKind::Move => (true, true), // 移動は横 + 縦 (pitch)。
                        NoteDragKind::ResizeLeft | NoteDragKind::ResizeRight => (true, false),
                    })
                } else if state.note_create.as_ref().is_some_and(|nc| nc.warp_settled)
                    || state.loop_drag.is_some()
                    || state.playhead_drag.is_some()
                {
                    // 新規作成 (warp 着地後) / ruler の loop / playhead: いずれも横軸のみ。
                    Some((true, false))
                } else {
                    None
                }
            } else {
                None
            };
            let drag_rect_wid = wid.child(b"rect_select");
            let marquee_active = moved_enough
                && axes.is_none()
                && {
                    let st: &mut crate::widgets::drag_rect::DragRectState =
                        self.widget_state(drag_rect_wid);
                    st.drag_start.is_some()
                };
            if let Some((ax, ay)) = axes.or_else(|| marquee_active.then_some((true, true))) {
                let cfg = crate::widgets::edge_scroll::EdgeScrollCfg::default();
                let (dx, dy) = crate::widgets::edge_scroll::edge_scroll_delta(
                    pointer.pos,
                    grid,
                    cfg,
                    ax,
                    ay,
                );
                // 横: view を min_start_beat で clamp して **実際に適用される** delta 拍を求める
                // (arrangement と同パターン)。view 層の `SetPianoRollScrollX(_.max(0))` clamp と一致し、
                // 左端で anchor が要求 px 分だけ過剰 shift して対象が飛ぶ runaway を防ぐ。
                // `applied_beat_px` は beat (横) 軸の anchor 補正量 (単位は px)。
                let (scroll_by_beats, applied_beat_px) = if dx == 0.0 || beat_per_px <= 1e-6 {
                    (0.0, 0.0)
                } else {
                    let new_start =
                        (view.start_beat + f64::from(dx) * beat_per_px).max(view.min_start_beat);
                    let actual = new_start - view.start_beat;
                    #[allow(clippy::cast_possible_truncation)]
                    let px = (actual / beat_per_px) as f32;
                    (actual, px)
                };
                // 縦 pitch: 端数を accum に貯め整数 semitone 単位で SetTopPitch。
                let mut new_top_pitch: Option<u8> = None;
                let mut applied_pitch_px = 0.0_f32;
                {
                    let state: &mut PianoRollState = self.widget_state(wid);
                    if dy != 0.0 && pitch_per_px > 1e-6 {
                        let px_per_semitone = 1.0 / pitch_per_px;
                        state.edge_pitch_accum += dy * pitch_per_px; // 下=正 (lower pitch へ)。
                        #[allow(clippy::cast_possible_truncation)]
                        let step = state.edge_pitch_accum.trunc();
                        if step != 0.0 {
                            state.edge_pitch_accum -= step;
                            // 下スクロール (step > 0) = lower pitch を出す = top_pitch 減。
                            let cur = view.pitch_top;
                            let next = (cur - step).clamp(11.0, 127.0);
                            let applied = cur - next;
                            if applied != 0.0 {
                                #[allow(
                                    clippy::cast_possible_truncation,
                                    clippy::cast_sign_loss
                                )]
                                let next_u = next.round() as u8;
                                new_top_pitch = Some(next_u);
                                applied_pitch_px = applied * px_per_semitone;
                            }
                        }
                    } else {
                        // 縦 zone 外の frame は accum をリセット (stale 防止)。
                        state.edge_pitch_accum = 0.0;
                    }
                }
                let scrolled_x = scroll_by_beats != 0.0;
                if scrolled_x {
                    self.push_edit(make_edit(PianoRollEditRequest::ScrollByBeats(
                        scroll_by_beats,
                    )));
                }
                if let Some(p) = new_top_pitch {
                    self.push_edit(make_edit(PianoRollEditRequest::SetTopPitch(p)));
                }
                if scrolled_x || new_top_pitch.is_some() {
                    if marquee_active {
                        let st: &mut crate::widgets::drag_rect::DragRectState =
                            self.widget_state(drag_rect_wid);
                        if let Some(s) = st.drag_start.as_mut() {
                            s.0 -= applied_beat_px;
                            s.1 -= applied_pitch_px;
                        }
                    } else {
                        let state: &mut PianoRollState = self.widget_state(wid);
                        if let Some(nd) = state.note_drag.as_mut() {
                            nd.anchor_mouse.0 -= applied_beat_px;
                            nd.anchor_mouse.1 -= applied_pitch_px;
                        }
                        if let Some(nc) = state.note_create.as_mut() {
                            // 新規作成は横のみ。anchor と warp 判定基準 press_x を同 shift。
                            nc.anchor_mouse.0 -= applied_beat_px;
                            nc.press_x -= applied_beat_px;
                        }
                        // loop/playhead: 絶対 px→beat 再解決で自動追従 → shift 不要。
                    }
                    self.request_redraw();
                }
            }
        }

        // (M14 Phase 69 / daw_01 #041) playhead_drag continuation の per-frame live update。
        // press frame は press block 内で発火済 (`press_seek_beat`)、 ここは continuation のみ。
        // release frame は emit せず、 後段で take して discard する (commit-by-release 無し)。
        // `last_emitted_beat` で同値発火を抑制 (1e-6 拍 = ~10μs @ 120BPM 以下は ignore)。
        // editing_mode 中は press block 自体が skip されているので playhead_drag が立つことは無く、
        // ここも naturally skip。
        if !editing_mode
            && let Some((px, _py)) = pointer.pos
            && !pointer.primary_just_pressed
            && !pointer.primary_just_released
        {
            let alt = pointer.modifiers.alt;
            let mut emit_beat: Option<f64> = None;
            {
                let state: &mut PianoRollState = self.widget_state(wid);
                if let Some(ref mut pd) = state.playhead_drag {
                    let raw = view.start_beat + f64::from(px - ruler.x) * beat_per_px;
                    let next = view
                        .snap
                        .snap_beat(raw, alt, zoom_x_px_per_beat)
                        .max(0.0);
                    if (next - pd.last_emitted_beat).abs() > 1e-6 {
                        emit_beat = Some(next);
                        pd.last_emitted_beat = next;
                    }
                }
            }
            if let Some(beat) = emit_beat {
                self.push_edit(make_edit(PianoRollEditRequest::SetPlayheadBeat(beat)));
            }
        }

        let drag_session: Option<NoteDragSession> = {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.note_drag.clone()
        };
        let velocity_drag_session: Option<VelocityDragSession> = {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.velocity_drag.clone()
        };
        // note_create overlay 用 clone と release 用 take。
        let note_create_session: Option<NoteCreateSession> = {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.note_create
        };
        let note_create_release: Option<NoteCreateSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.note_create.take()
        } else {
            None
        };
        // (M14 Phase 69 / daw_01 #041) loop_drag overlay & release 用 clone / take。
        let loop_drag_session: Option<LoopDragSession> = {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.loop_drag
        };
        let loop_drag_release: Option<LoopDragSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.loop_drag.take()
        } else {
            None
        };
        // playhead_drag は release frame で take して discard (commit-by-release 無し)。
        if pointer.primary_just_released {
            let state: &mut PianoRollState = self.widget_state(wid);
            let _ = state.playhead_drag.take();
        }
        // drag release で取り出すが、drag 距離が 16px 未満なら **click に格下げ** する
        // (= 短い「press → release」は note 中央上の click として selection 切替に振り向ける)。
        let drag_release_raw: Option<NoteDragSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.note_drag.take()
        } else {
            None
        };
        // (M14 Phase 64 / daw_01 #018) velocity_drag release: drag<3px は「click 単発 = no-op」
        // として扱い SetVelocity 発行しない。 後段の commit ブロックで dist 判定 + Edit 発行。
        let velocity_drag_release: Option<VelocityDragSession> = if pointer.primary_just_released {
            let state: &mut PianoRollState = self.widget_state(wid);
            state.velocity_drag.take()
        } else {
            None
        };
        let (drag_release, drag_short_click_pos): (Option<NoteDragSession>, Option<(f32, f32)>) =
            if let Some(nd) = drag_release_raw {
                // dist 判定 / delta 計算は両者とも `nd.last_mouse` を真値とする (pointer.pos の
                // winit-bug 化を上の continuation block で吸収済み)。 click 短縮は pointer.pos
                // ではなく last_mouse 基準。
                // 短 click 化 (drag → click 格下げ) の閾値は **mouse jitter を ignore する程度** (4px)。
                //   - Resize (Left/Right) は常に commit (resize handle 上 click は意味なし)
                //   - Move + Alt なしのみ jitter 閾値で短 click 化 (click=selection / drag=移動 の区別)
                //   - Alt 押下中は Move でも閾値 skip (raw 微調整の明示意図)
                let dist = (nd.last_mouse.0 - nd.anchor_mouse.0).abs()
                    + (nd.last_mouse.1 - nd.anchor_mouse.1).abs();
                let is_move = matches!(nd.kind, NoteDragKind::Move);
                let demote = is_move && !nd.last_alt && dist < 4.0;
                if demote {
                    (None, pointer.pos)
                } else {
                    (Some(nd), None)
                }
            } else {
                (None, None)
            };

        // drag 中の delta (pointer から計算)。beat_delta は f64、pitch_delta は i32 (Highlight/Linear:
        // 半音単位、 Fold: scale degree 単位 — `apply_pitch_drag_delta` で吸収)。
        // M9 Phase 45f: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用 (相対関係維持)。 alt 押下で snap 一時無効化。
        // alt は drag state の `last_alt` を真値とし、 `pointer.modifiers.alt` を直接見ない
        // (release frame の commit と必ず同一値で確定するため)。
        let drag_overlay: Option<(NoteDragSession, f64, i32)> = drag_session
            .as_ref()
            .and_then(|nd| pointer.pos.map(|p| (nd.clone(), p)))
            .map(|(nd, (px, py))| {
                let dx = px - nd.anchor_mouse.0;
                let dy = py - nd.anchor_mouse.1;
                let raw = f64::from(dx) * beat_per_px;
                // 絶対位置 snap (詳細は `compute_note_drag_beat_delta` 参照、 arrangement と同パターン)。
                let beat_delta = compute_note_drag_beat_delta(
                    &nd,
                    raw,
                    &view.snap,
                    zoom_x_px_per_beat,
                );
                let pitch_delta = compute_pitch_drag_delta(view, grid, dy);
                (nd, beat_delta, pitch_delta)
            });

        // ----- Response 初期値 + hover 計算 -----
        let mut response = PianoRollResponse {
            hovered: pointer
                .pos
                .is_some_and(|(px, py)| grid.contains(px, py)),
            ..Default::default()
        };
        if let Some((cx, cy)) = pointer.pos
            && grid.contains(cx, cy)
            && let Some((hover_id, hover_kind)) =
                note_hit(notes, view, grid, cx, cy, style.resize_handle_px)
        {
            response.hovered_note_id = Some(hover_id);
            response.hovered_zone = Some(hover_kind);
        }
        response.dragging = drag_session.as_ref().map(|nd| nd.kind);
        response.velocity_dragging = velocity_drag_session.is_some();
        // 作成 session 中は creating=true (caller の wheel 無効化用)。
        response.creating = note_create_session.is_some();
        // (M14 Phase 84 / daw_01 #055) 鍵盤レーン press 中の pitch を held-value で返す。
        // session 中 (press 開始が kbd) かつ まだ押下中 (primary_pressed) かつ pointer が kbd 内の
        // ときだけ Some。release frame は primary_pressed=false で None (= note-off)、kbd 外への drag
        // も None。pitch は毎フレーム pointer.y から計算するので glissando に追従する。
        if self.widget_state::<PianoRollState>(wid).keyboard_pressing
            && pointer.primary_pressed
            && let Some((px, py)) = pointer.pos
            && kbd.contains(px, py)
        {
            response.keyboard_active_pitch =
                Some(RowGeometry::compute(view, grid).y_to_pitch(py));
        }

        // hover 中の cursor 形状要求 (drag 中は drag kind、note hover (拡張範囲含む) は
        // hover_cursor、その他 widget 内は Default に明示 reset で stale cursor を防ぐ)。
        // winit は state-full なので set_cursor を呼ばないと前フレームの形状が残る (ui.rs:999)。
        if response.creating {
            // 作成中は右端を伸ばす操作なので EwResize (resize と同じ)。
            self.set_cursor(CursorIcon::EwResize);
        } else if response.dragging.is_some() {
            let cursor = match response.dragging {
                Some(NoteDragKind::Move) => CursorIcon::Move,
                Some(NoteDragKind::ResizeLeft | NoteDragKind::ResizeRight) => {
                    CursorIcon::EwResize
                }
                None => CursorIcon::Default,
            };
            self.set_cursor(cursor);
        } else if let Some((cx, cy)) = pointer.pos
            && let Some(cursor) =
                note_hover_cursor(visible, view, grid, cx, cy, style.resize_handle_px)
        {
            self.set_cursor(cursor);
        } else if pointer.pos.is_some_and(|(px, py)| rect.contains(px, py)) {
            self.set_cursor(CursorIcon::Default);
        }

        // ----- M14 Phase 125 (#102): plain-drag marquee gate (空き grid press を marquee が所有) -----
        // 旧設計は rect-select 起動に Shift 必須だったが、 空き grid を無修飾 drag → 範囲選択にする
        // (標準 DAW 慣習)。 note 上の plain drag は移動のまま。 修飾は release 時の next 計算で
        // plain=REPLACE / Shift=UNION / Ctrl=XOR に分岐。 gate を **pending_click 計算の前** で評価して
        // `marquee_active` を作り、 空き click clear (pending_click) が marquee の zero-rect REPLACE と
        // 同フレーム二重 emit するのを防ぐ (空き clear は下の :2219 で marquee :2380 より先に消費される
        // ため、 前方 bool での抑制が必須 — daw_01 #102「二重 emit 抑制」)。 `note_hit().is_none()` は
        // load-bearing: note MOVE は !shift gate なので hit-test 無しだと Shift+note press が誤って marquee
        // 起動する。 Alt は除外、 `note_drag` が press 時 None (= 真の空き press) を要求。
        let drag_rect_wid = wid.child(b"rect_select");
        let shift_rect_active = {
            let state: &mut crate::widgets::drag_rect::DragRectState =
                self.widget_state(drag_rect_wid);
            state.drag_start.is_some()
        };
        let marquee_press = if !editing_mode
            && pointer.primary_just_pressed
            && !pointer.modifiers.alt
            // この press が「ダブルクリック作成」 のものなら marquee を起動しない
            // (作成 session が press を所有。 二重所有を防ぐ load-bearing gate)。
            && note_create_press.is_none()
            && let Some((px, py)) = pointer.pos
            && grid.contains(px, py)
            && note_hit(notes, view, grid, px, py, style.resize_handle_px).is_none()
        {
            let s: &PianoRollState = self.widget_state(wid);
            s.note_drag.is_none()
        } else {
            false
        };
        let marquee_active = marquee_press || shift_rect_active;

        // ----- pending click 判定 -----
        // 2 通り: (a) drag が起こらなかった pure release、(b) drag は始まったが <16px で
        // click に格下げされた release。どちらも grid 上の click として selection 切替の
        // trigger に使う。M14 Phase 59: editing_mode 中は click を発火しない。
        // (M14 Phase 64 / daw_01 #018) velocity_drag_release 中も click 扱いしない (drag<3px no-op
        // でも selection を変えない / 通常 release は SetVelocity 発行で完結)。
        // (M14 Phase 64) vel_area / ruler / keyboard 等 grid 外の release は selection に影響させない
        // = `grid.contains(pos)` で gate (旧: 無条件 release で grid 外なら selection clear する
        // latent bug を修正)。 grid 内の空白 release は従来どおり selection clear。
        let pending_click: Option<(f32, f32)> = if editing_mode
            || drag_release.is_some()
            || velocity_drag_release.is_some()
            || marquee_active
            // 作成 release frame は Add で新規 note を選択するので、 ここで
            // 空白 click 扱いして selection clear を emit しない (二重 emit 抑制)。
            || note_create_release.is_some()
        {
            // #102: marquee がこの空き grid press を所有する frame は marquee 側が zero-rect REPLACE で
            // clear する。 ここで pending_click を立てると同フレーム二重 emit になるため None。
            None
        } else if let Some(p) = drag_short_click_pos {
            Some(p)
        } else if pointer.primary_just_released
            && !pointer.modifiers.shift
            && let Some((px, py)) = pointer.pos
            && grid.contains(px, py)
        {
            Some((px, py))
        } else {
            None
        };

        // ----- 描画 (heavy ブロック + cached + 動的 overlay) -----
        // M9 Phase 45c: viewport_key に vel_h を追加 (velocity lane 高さ変化で cache 無効化)。
        // M13 Phase 55: ruler_h / bpm / time_sig を追加 + v2 に bump (cache 構造変化)。
        // tuple Hash impl は 12 要素まで → bpm + time_sig を 1 つの組に纏めて 12 要素に収める。
        // M14 Phase 61b (#011): note 個別の (id, start_beat, len_beats, pitch, velocity, lyric)
        // 変化を widget 側で hash して 2 要素 outer tuple に wrap + v3 に bump (arrangement の
        // clip drag 残像と同根の予防、 caller の notes_generation は note 数や編集 epoch のみで
        // 不十分なケースを吸収)。
        let internal_note_hash = fold_piano_roll_note_hash(visible);
        // (M14 Phase 70 / daw_01 #042) `view.scale` を hash に含めて、 scale 切替 (root / mask / mode)
        // が起きたとき cache invalidate されるようにする。 None は (0, 0, 0) で表現 (= scale OFF
        // が連続するときは同じ hash 寄与で cache hit、 None ↔ Some の遷移時は差分が出る)。
        // (M14 Phase 70 / daw_01 #042) scale 切替 (root / mask / mode) で cache invalidate。
        // (M14 Phase 70b / daw_01 #042 follow-up) snap_pitch_during_drag toggle も cache key に
        // 含める (= drag preview の経路は cached 内には含まれないが、 future-proof と test 容易さ
        // のため含める判断、 cost は u8 1 byte 増加のみ)。
        let scale_key = view.scale.map_or((0_u8, 0_u16, 0_u8), |sc| {
            let mode_tag: u8 = match sc.mode {
                PianoRollScaleMode::Highlight => 1,
                PianoRollScaleMode::Fold => 2,
            };
            (sc.root, sc.in_scale_mask, mode_tag)
        });
        let snap_drag_key: u8 = u8::from(view.snap_pitch_during_drag);
        // (M14 Phase 124 / daw_01 #100) subdivision 間隔を cache key に含める。 cached() は
        // viewport_key 一致時に内側 (bar_beat_grid 含む) を完全 skip するので、 bar_beat_grid 内の
        // input_hash だけでは足りず、 ここで invalidate 経路を張る必要がある。 None=0 / Some=bits。
        let sub_grid_key: u64 = view.sub_grid_interval_beats.map_or(0, f64::to_bits);
        let viewport_key = (
            (
                b"piano_roll_widget_v3" as &[u8],
                view.start_beat.to_bits(),
                view.len_beats.to_bits(),
                view.pitch_top.to_bits(),
                view.pitch_visible.to_bits(),
                grid.w.to_bits(),
                grid.h.to_bits(),
                kbd.w.to_bits(),
                view.notes_generation,
                vel_h.to_bits(),
                ruler_h.to_bits(),
                (
                    view.bpm.to_bits(),
                    u32::from(view.time_sig.0),
                    u32::from(view.time_sig.1),
                    scale_key,
                    snap_drag_key,
                    sub_grid_key,
                ),
            ),
            internal_note_hash,
        );

        // M13 Phase 55: library `time_ruler` / `bar_beat_grid` を呼ぶための共通 mapping。
        // beat 単位 view を sample 単位 ViewportState1D に変換 (sample_rate = 48k は BarBeat
        // 表示で比例定数として打ち消されるダミー)。
        let mapping = TimeMapping {
            sample_rate: 48_000.0,
            tempo_bpm: f64::from(view.bpm.max(1.0)),
            time_sig: (view.time_sig.0.max(1), view.time_sig.1.max(1)),
            display: TimeDisplay::BarBeat,
        };
        let spb = mapping.samples_per_beat();
        let sample_viewport =
            ViewportState1D::new(view.start_beat * spb, view.len_beats.max(1e-6) * spb);
        let grid_style_pr = BarBeatGridStyle {
            bar_color: style.bar_line,
            beat_color: style.beat_line,
            bar_line_width: style.bar_line_width_px,
            beat_line_width: style.beat_line_width_px,
            // M14 Phase 63m (daw_01 #027): zoom 連動の beat 線間引き (default 4px)。
            ..BarBeatGridStyle::default()
        };
        // M14 Phase 124 (#100): 3 段目 subdivision。 caller が拍間隔を渡したときだけ構築
        // (ズーム退避は bar_beat_grid 内の px_per_interval 判定に委ねる)。 cache 無効化は
        // viewport_key に interval を含めて行う (下記、 cached が viewport_key で short-circuit
        // するため bar_beat_grid 内の input_hash だけでは効かない)。
        let sub_grid_pr: Option<SubGridSpec> = view.sub_grid_interval_beats.and_then(|iv| {
            (iv > 0.0).then_some(SubGridSpec {
                interval_beats: iv,
                color: style.sub_line,
                line_width: style.sub_line_width_px,
            })
        });
        let ruler_style_pr = TimeRulerStyle {
            bg: style.ruler_bg,
            tick_color: style.bar_line,
            label_color: style.ruler_label_color,
            bar_tick_height: 12.0,
            beat_tick_height: 5.0,
            // M14 Phase 63m (daw_01 #027): zoom 連動の label / beat tick 間引き (default 60 / 4 px)。
            ..TimeRulerStyle::default()
        };
        let id_for_inner: u64 = hash_inputs(&id);

        let visible_owned: Vec<Note> = visible.to_vec();
        let style_copy = *style;
        let view_copy = view;
        // selected は heavy 内 borrow 不可なので Vec を所有権渡しで closure に取り込む
        let selected_set: HashSet<NoteId> = selected.iter().copied().collect();
        let drag_overlay_clone = drag_overlay.clone();
        let lyric_editing_for_draw = lyric_editing;
        // M9 Phase 45f: drag overlay の Resize min_len は snap unit に合わせる
        // (snap_unit < 0.05 なら 0.05)。 release 側 min_len と同じ計算で一貫性確保。 alt 真値は
        // drag session の `last_alt` (overlay と release commit が必ず同一 unit で確定する)。
        // overlay 不在時 (drag していない) は min_len 自体使われないので alt = false で適当に初期化。
        let drag_overlay_alt = drag_overlay.as_ref().is_some_and(|(nd, _, _)| nd.last_alt);
        let drag_overlay_min_len: f64 = if view.snap.is_active(drag_overlay_alt) {
            view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
        } else {
            0.05
        };
        // (M14 Phase 64 / daw_01 #018) velocity drag preview: drag 中なら target_ids の bar を
        // current pointer.y → 絶対 velocity の値で描画 override。 None のときは note.velocity 通常描画。
        // velocity_drag は press 時に vel_area.h > 0 を gate してあるため vel_area.h > 0 が前提。
        let velocity_drag_overlay: Option<(Vec<NoteId>, u8)> =
            velocity_drag_session.as_ref().map(|vd| {
                let new_vel = velocity_from_y(vd.last_mouse.1, vel_area);
                (vd.target_ids.clone(), new_vel)
            });

        // (M14 Phase 69 / daw_01 #041) loop drag overlay の preview range も snap 適用済
        // (commit と同一値で確定、 release 時の「カクッ」 ずれを回避)。 alt は session の `last_alt`
        // を真値とし、 `pointer.modifiers.alt` を直接見ない (clip_drag / loop_drag in arrangement と
        // 同 pattern)。
        let loop_drag_preview_range: Option<(f64, f64)> = loop_drag_session.map(|ld| {
            let cur_beat =
                view.start_beat + f64::from(ld.last_mouse_x - ruler.x) * beat_per_px;
            compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat)
        });
        let loop_band_color = style.loop_band;
        let loop_handle_color = style.loop_handle;
        let loop_handle_w = style.loop_handle_w;

        // note_create preview: 作成中 note の rect (drag preview と同じ helper で
        // 長さ確定値を計算 → grid clamp)。session 不在なら None。色は resize ghost (selected) と同じ。
        let note_create_preview: Option<Rect> = note_create_session.map(|nc| {
            let (start_beat, len_beats, pitch) =
                note_create_geometry(&nc, view, beat_per_px, zoom_x_px_per_beat);
            note_geometry_to_rect(start_beat, len_beats, pitch, view, grid)
        });

        self.heavy(("piano_roll_inner", &id), move |hctx| {
            // === cached(): viewport_key 一致時に skip される背景レイヤ ===
            hctx.cached(viewport_key, |hctx| {
                draw_grid_background(hctx, grid, kbd, view_copy, &style_copy);
                hctx.bar_beat_grid(
                    ("pr_grid", id_for_inner),
                    grid,
                    mapping,
                    sample_viewport,
                    grid_style_pr,
                    sub_grid_pr,
                );
                if ruler_h > 0.0 {
                    hctx.time_ruler(
                        ("pr_ruler", id_for_inner),
                        ruler,
                        mapping,
                        sample_viewport,
                        ruler_style_pr,
                    );
                }
                draw_notes(
                    hctx,
                    &visible_owned,
                    view_copy,
                    grid,
                    style_copy.note_fill_fn,
                    style_copy.bg,
                    style_copy.note_border_radius_px,
                    style_copy.note_muted_hatch_color,
                    style_copy.note_muted_hatch_spacing_px,
                    style_copy.note_muted_hatch_width_px,
                );
            });

            // === cached の外: 動的 overlay (selection / velocity lane / drag preview / lyric / cursor / playhead) ===
            // (M14 Phase 64 / daw_01 #018) velocity lane は cached の外に移動。 drag preview の
            // override velocity を毎 frame 反映するため (drag 中はバー高さが pointer.y で変わる)。
            // 静的時は visible 数 ≤ ~100 なので毎 frame 描画でも負荷は軽微 (rect command ~100 個)、
            // model 更新時の cache 無効化を待たずに即時反映するメリットが上回る。
            if vel_h > 0.0 {
                draw_velocity_lane(
                    hctx,
                    &visible_owned,
                    view_copy,
                    vel_area,
                    &style_copy,
                    velocity_drag_overlay.as_ref().map(|(ids, v)| (ids.as_slice(), *v)),
                );
            }
            // selection overlay (note の上、lyric の下)
            if !selected_set.is_empty() {
                draw_selection_overlay(
                    hctx,
                    &visible_owned,
                    &selected_set,
                    view_copy,
                    grid,
                    &style_copy,
                );
            }
            // drag preview (drag 中の shifted rect)
            if let Some((nd, bd, pd)) = drag_overlay_clone {
                draw_drag_preview(
                    hctx,
                    &nd,
                    view_copy,
                    grid,
                    &style_copy,
                    bd,
                    pd,
                    drag_overlay_min_len,
                );
            }
            // note_create preview (作成中の note を selection ghost 色で描画)。
            if let Some(r) = note_create_preview {
                let x_left = r.x.max(grid.x);
                let x_right = (r.x + r.w).min(grid.x + grid.w);
                let y_top = r.y.max(grid.y);
                let y_bot = (r.y + r.h).min(grid.y + grid.h);
                if x_right > x_left && y_bot > y_top {
                    hctx.push_rect(RectCommand {
                        rect: Rect {
                            x: x_left,
                            y: y_top,
                            w: x_right - x_left,
                            h: y_bot - y_top,
                        },
                        fill: style_copy.note_selected_fill,
                        border: style_copy.note_selected_border,
                        border_width: style_copy.note_selected_border_w,
                        radius: [style_copy.note_border_radius_px; 4],
                        clip_rect: None,
                    });
                }
            }
            // M14 Phase 59: lyric 描画 (selection overlay より後 = 黄色 fill に隠れない、
            // 編集中 note は text_input overlay に譲る)。 font_size は note 高さスケール。
            draw_lyrics(
                hctx,
                &visible_owned,
                view_copy,
                grid,
                style_copy.lyric_color,
                style_copy.lyric_font_px,
                lyric_editing_for_draw,
            );
            // (M14 Phase 69 / daw_01 #041) loop band overlay (ruler 上、 cached の外で毎 frame 描画)。
            // drag preview range があれば preview を、 無ければ `view.loop_range` を描画。 ruler_h <= 0
            // のときは `ruler.h = 0` なので描画 helper 内で band_w = 0 となり no-op (= 旧 API 互換)。
            // arrangement と完全同 helper (`crate::widgets::ruler_ops::draw_loop_band`)、 daw_01 が
            // ruler_h > 0 + loop_range Some を渡したときのみ表示される。
            if ruler_h > 0.0
                && let Some(range) = loop_drag_preview_range.or(view_copy.loop_range)
            {
                crate::widgets::ruler_ops::draw_loop_band(
                    hctx,
                    range,
                    view_copy.start_beat,
                    view_copy.len_beats,
                    ruler,
                    loop_band_color,
                    loop_handle_color,
                    loop_handle_w,
                );
            }
            // M9 Phase 45c: playhead 線 (time で動くので cache 対象外、毎フレーム描画)。
            // 範囲外なら描画スキップ。grid と vel_area を縦断する 1 本。
            if let Some(b) = view_copy.playhead_beat
                && b >= view_copy.start_beat
                && b <= view_copy.start_beat + view_copy.len_beats
            {
                let beat_to_px = f64::from(grid.w) / view_copy.len_beats.max(1e-6);
                let x = grid.x + ((b - view_copy.start_beat) * beat_to_px) as f32;
                let y_top = grid.y;
                let y_bottom = vel_area.y + vel_area.h;
                draw_playhead_line(
                    hctx,
                    x,
                    y_top,
                    y_bottom,
                    style_copy.playhead_color,
                    style_copy.playhead_width_px,
                );
            }
        });

        // ----- shortcut: Insert (note 追加) / Delete (selected 削除) -----
        // M14 Phase 59: editing_mode 中は global shortcut が typing_focus で抑制される
        // ため take_shortcut は false を返すはずだが、defensive で明示 guard。
        if !editing_mode
            && self.take_shortcut("add_note")
            && let Some((cx, cy)) = pointer.pos
            && grid.contains(cx, cy)
        {
            let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
            let raw_start = (view.start_beat + f64::from(cx - grid.x) / beat_to_px).max(0.0);
            // M9 Phase 45f: Insert は widget 内発火、grid 吸着が UX 自然 (#010 [Replied])。
            // single frame の click なので drag state は関与せず、 直接 `pointer.modifiers.alt` を読む。
            let start_beat = view
                .snap
                .snap_beat(raw_start, pointer.modifiers.alt, zoom_x_px_per_beat)
                .max(0.0);
            // M14 Phase 70 / daw_01 #042: RowGeometry 経由で Fold mode も対応 (Fold では
            // y_to_pitch_f が在 row index → in-scale pitch を返すので、 ceil で確実に in-scale)。
            let geom = RowGeometry::compute(view, grid);
            let pitch_f = geom.y_to_pitch_f(cy);
            // M14 Phase 61d (#012): 描画式 `y = grid.y + (pitch_top - pitch) * pitch_to_px` の
            // 逆関数として ceil() を使う (pitch P の視覚行 y ∈ [(top-P)*pt, (top-P+1)*pt) なので
            // 逆引きは pitch_f ∈ (P-1, P] のとき P を返す = ceil)。 round() だと判定領域が視覚行
            // に対して半行ぶん上にずれて、 行の下半分にカーソルがあると 1 つ下のピッチに化ける。
            // (Fold mode では y_to_pitch_f が既に in-scale pitch を整数で返すので ceil は no-op)。
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let pitch = (pitch_f.ceil() as i32).clamp(0, 127) as u8;
            // note id は user 側で next_note_id を bump して上書き。
            // ここでは placeholder id=0 で渡す (user は make_edit closure 内で bump 済 id を使う)。
            // 長さは caller の既定長 (= last_note_duration_beats) に統一。 旧 0.5 固定だと
            // 直前にドラッグ / resize した長さが Insert に反映されず一貫性が無かった。 下限 1/16。
            let new_note = Note {
                id: 0,
                start_beat,
                len_beats: view.default_note_len_beats.max(0.0625),
                pitch,
                velocity: 96,
                lyric: None,
                muted: false,
                style: NoteStyle::default(),
            };
            self.push_edit(make_edit(PianoRollEditRequest::Add(vec![new_note])));
        }

        if !editing_mode && self.take_shortcut("delete") && !selected.is_empty() {
            let sel_set: HashSet<NoteId> = selected.iter().copied().collect();
            let to_delete: Vec<Note> =
                notes.iter().filter(|n| sel_set.contains(&n.id)).cloned().collect();
            if !to_delete.is_empty() {
                self.push_edit(make_edit(PianoRollEditRequest::Delete(to_delete)));
            }
        }

        // ----- pending click → selection 切替 (Edit 発行のみ、外部 selected は frame 末で apply 後に反映) -----
        if let Some((cx, cy)) = pending_click {
            let prev: Vec<NoteId> = selected.to_vec();
            let new_sel: Vec<NoteId> = if grid.contains(cx, cy) {
                if let Some((hit_id, _)) =
                    note_hit(notes, view, grid, cx, cy, style.resize_handle_px)
                {
                    vec![hit_id]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            if prev != new_sel {
                self.push_edit(make_edit(PianoRollEditRequest::Select {
                    prev,
                    next: new_sel,
                }));
                response.selection_changed = true;
            }
            // grid 内の short click なら beat/pitch も Response に載せる
            if grid.contains(cx, cy) {
                let beat_to_px = f64::from(grid.w) / view.len_beats.max(1e-6);
                let beat = view.start_beat + f64::from(cx - grid.x) / beat_to_px;
                // M14 Phase 70 / daw_01 #042: Fold mode 中も「視覚的な行 → in-scale pitch」 で返す。
                let pitch = RowGeometry::compute(view, grid).y_to_pitch_f(cy);
                response.clicked_at_beat_pitch = Some((beat, pitch));
            }
        }

        // ----- drag release → Move / Resize Edit 発行 -----
        // M9 Phase 60: anchor 0 の delta を `view.snap.snap_beat_delta` で round → 全 anchor に
        // 同 delta 適用。 Resize の min_len は snap unit に合わせる (snap_unit < 0.05 なら 0.05)。
        // **alt は drag 中の最終 `nd.last_alt` を真値とする** — release frame の `pointer.modifiers.alt`
        // は OS event 順序 (ModifiersChanged が MouseInput(Released) より先に届く) によって false に
        // 化けることがあるため信用しない。 `last_alt` は continuation frame で更新され release frame
        // では `allow_update = false` で保持されるので OS event 順序に依存しない。 overlay の snap
        // 判定とも同一値で確定し、 「release で grid に飛ぶ」 不整合が起きない。
        if let Some(nd) = drag_release {
            let release_alt = nd.last_alt;
            // pointer.pos に頼らず `nd.last_mouse` を使う (winit release frame で pointer.pos が
            // press 位置に戻る既存問題、 arrangement と同パターン)。
            let dx = nd.last_mouse.0 - nd.anchor_mouse.0;
            let dy = nd.last_mouse.1 - nd.anchor_mouse.1;
            let raw = f64::from(dx) * beat_per_px;
            // 絶対位置 snap (overlay と一貫)。
            let beat_delta =
                compute_note_drag_beat_delta(&nd, raw, &view.snap, zoom_x_px_per_beat);
            // pitch も overlay と同一 helper で確定する。 Fold mode では 1 行 =
            // 1 scale degree なので、 ここだけ半音換算 (dy × pitch_per_px) にすると
            // ghost で見た位置と別の pitch に commit してしまう。
            let pitch_delta = compute_pitch_drag_delta(view, grid, dy);
            let min_len = if view.snap.is_active(release_alt) {
                view.snap.beat_unit(zoom_x_px_per_beat).map_or(0.05, |u| u.max(0.05))
            } else {
                0.05
            };

            match nd.kind {
                NoteDragKind::Move => {
                    let mut deltas: Vec<MoveDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_start = (a.start_beat + beat_delta).max(0.0);
                        // M14 Phase 70b / daw_01 #042 follow-up: release commit も `nd.last_alt` を
                        // 渡して overlay と完全同 helper 経由。 alt で snap 無効も両者一致。
                        let new_pitch =
                            apply_pitch_drag_delta(a.pitch, pitch_delta, view, release_alt);
                        if (new_start - a.start_beat).abs() > 1e-6 || new_pitch != a.pitch
                        {
                            deltas.push((a.id, a.start_beat, a.pitch, new_start, new_pitch));
                        }
                    }
                    if !deltas.is_empty() {
                        // M14 Phase 83 / daw_01 #054: Ctrl 保持なら複製 (元据え置き)、 そうでなければ
                        // 移動。 `nd.last_ctrl` は overlay と同じ careful-update 値なので、 copy ghost を
                        // 見て release した結果と必ず一致する。
                        let req = if nd.last_ctrl {
                            PianoRollEditRequest::Copy(deltas)
                        } else {
                            PianoRollEditRequest::Move(deltas)
                        };
                        self.push_edit(make_edit(req));
                    }
                }
                NoteDragKind::ResizeRight => {
                    let mut deltas: Vec<ResizeDelta> = Vec::new();
                    for a in &nd.anchors {
                        let new_len = (a.len_beats + beat_delta).max(min_len);
                        if (new_len - a.len_beats).abs() > 1e-6 {
                            deltas.push((
                                a.id,
                                a.start_beat,
                                a.len_beats,
                                a.start_beat,
                                new_len,
                            ));
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(PianoRollEditRequest::Resize(deltas)));
                    }
                }
                NoteDragKind::ResizeLeft => {
                    let mut deltas: Vec<ResizeDelta> = Vec::new();
                    for a in &nd.anchors {
                        let max_start = a.start_beat + a.len_beats - min_len;
                        let new_start = (a.start_beat + beat_delta).clamp(0.0, max_start);
                        let actual_delta = new_start - a.start_beat;
                        let new_len = (a.len_beats - actual_delta).max(min_len);
                        if (new_start - a.start_beat).abs() > 1e-6
                            || (new_len - a.len_beats).abs() > 1e-6
                        {
                            deltas.push((
                                a.id,
                                a.start_beat,
                                a.len_beats,
                                new_start,
                                new_len,
                            ));
                        }
                    }
                    if !deltas.is_empty() {
                        self.push_edit(make_edit(PianoRollEditRequest::Resize(deltas)));
                    }
                }
            }
        }

        // ----- velocity drag release → SetVelocity Edit 発行 (M14 Phase 64 / daw_01 #018) -----
        // drag<3px は no-op (誤操作防止)。 release frame では last_mouse を真値とする
        // (note_drag と同パターン: winit が release frame で pointer.pos を press 位置に巻き戻す対策)。
        // 絶対値 mode: pointer.y から `velocity_from_y` で 0..=127 計算 → 全 target に同じ値を set。
        // anchor velocity と一致する note は updates から除外 (no-op Edit を avoid)。
        if let Some(vd) = velocity_drag_release {
            let dx = vd.last_mouse.0 - vd.anchor_mouse.0;
            let dy = vd.last_mouse.1 - vd.anchor_mouse.1;
            let dist = dx.abs() + dy.abs();
            if dist >= 3.0 {
                let new_vel = velocity_from_y(vd.last_mouse.1, vel_area);
                let mut updates: Vec<VelocityUpdate> = Vec::new();
                for (id, anchor_vel) in &vd.anchor_velocities {
                    if *anchor_vel != new_vel {
                        updates.push((*id, new_vel));
                    }
                }
                if !updates.is_empty() {
                    self.push_edit(make_edit(PianoRollEditRequest::SetVelocity(updates)));
                }
            }
        }

        // ----- note_create release → Add 発行 (作成 + 長さ確定を 1 undo step に) -----
        // overlay と同じ `note_create_geometry` で長さを確定 (描画と commit の一致)。 ドラッグせず
        // 即放しなら既定長、 ドラッグしていれば pointer 追従長。 id=0 placeholder (caller が採番)。
        // daw_01 は `n.len_beats` を尊重して AddNote { duration } に変換する (旧: last_note_duration_beats
        // 固定だったのを #82 で n.len_beats へ)。 pitch は press 時に確定済み。
        if let Some(nc) = note_create_release {
            let (start_beat, len_beats, pitch) =
                note_create_geometry(&nc, view, beat_per_px, zoom_x_px_per_beat);
            let new_note = Note {
                id: 0,
                start_beat,
                len_beats,
                pitch,
                velocity: 100,
                lyric: None,
                muted: false,
                style: NoteStyle::default(),
            };
            self.push_edit(make_edit(PianoRollEditRequest::Add(vec![new_note])));
            // 入力完了後、 press 時に既定長ノートの右端へ warp した
            // カーソルを元のクリック位置へ戻す (warp しっぱなしだと「ノートの右端のまま」
            // 残り、 次操作の起点が分かりにくいという要望)。 warp は y を変えない
            // (press 時 `warp_cursor(warp_x, py)`) ので、 復帰先 y は anchor_mouse.1
            // (= press y) をそのまま再利用する (press_y を別フィールドで複製しない = SSoT)。
            self.warp_cursor(nc.press_x, nc.anchor_mouse.1);
        }

        // ----- loop drag release → SetLoopRange (M14 Phase 69 / daw_01 #041) -----
        // snap 適用済 endpoints を overlay と共通の helper で計算 (release frame で grid に飛ぶ
        // 不整合を構造的に回避、 arrangement #024 と同 idiom)。 alt は `ld.last_alt` を真値とし、
        // release frame の `pointer.modifiers.alt` を直接見ない (ModifiersChanged 先行 race 回避)。
        if let Some(ld) = loop_drag_release {
            let cur_beat =
                view.start_beat + f64::from(ld.last_mouse_x - ruler.x) * beat_per_px;
            let (start, end) =
                compute_loop_drag_endpoints(&ld, cur_beat, &view.snap, zoom_x_px_per_beat);
            self.push_edit(make_edit(PianoRollEditRequest::SetLoopRange { start, end }));
        }

        // ----- M14 Phase 125 (#102): marquee commit (plain=REPLACE / Shift=UNION / Ctrl=XOR) -----
        // gate `marquee_active` / `drag_rect_wid` は pending_click 計算の前で算出済 (空き grid press のみ)。
        // `take_drag_rect_in_rect` は呼ぶだけで cyan 半透明 overlay を自動描画し、 press 時 modifier を
        // `DragRect.modifiers` に snapshot する。 release frame (`drag.finished`) に inside を集めて修飾で
        // next を分岐 (`sort_unstable` 後に `prev != next` で no-op 抑制)。 REPLACE は inside そのまま
        // (zero-rect → 空 = 選択 clear)。 editing_mode 中は marquee_press が false なので走らない。
        if !editing_mode
            && marquee_active
            && let Some(drag) = self.take_drag_rect_in_rect(drag_rect_wid, grid)
        {
            response.rect_select_active = true;
            if drag.finished {
                let drag_rect = drag.rect();
                let mut inside: Vec<NoteId> = Vec::new();
                for n in visible {
                    // lock クリップの note は marquee 矩形選択からも除外。
                    if n.style.locked {
                        continue;
                    }
                    let r = note_to_rect(n, view, grid);
                    if rects_intersect(r, drag_rect) {
                        inside.push(n.id);
                    }
                }
                let prev: Vec<NoteId> = selected.to_vec();
                let mut next: Vec<NoteId> = if drag.modifiers.shift {
                    // UNION: prev に inside の新規だけ append。
                    let mut out = prev.clone();
                    for id in &inside {
                        if !out.contains(id) {
                            out.push(*id);
                        }
                    }
                    out
                } else if drag.modifiers.ctrl {
                    // XOR: prev に在って inside にも在る id を除き、 inside の新規を追加。
                    let mut out: Vec<NoteId> =
                        prev.iter().copied().filter(|id| !inside.contains(id)).collect();
                    for id in &inside {
                        if !prev.contains(id) {
                            out.push(*id);
                        }
                    }
                    out
                } else {
                    inside // REPLACE (zero-rect → 空 = clear)
                };
                next.sort_unstable();
                let mut prev_sorted = prev.clone();
                prev_sorted.sort_unstable();
                if prev_sorted != next {
                    self.push_edit(make_edit(PianoRollEditRequest::Select { prev, next }));
                    response.selection_changed = true;
                }
            }
        }

        // ===== M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 overlay (text_input + commit dispatch) =====
        // lyric_editing が Some なら、編集対象 note の rect 内に text_input を重ね描きし、
        // Enter / NumpadEnter で commit text を `split_into_morae` で分割 → 後続 note へ
        // 1 SetLyrics Edit (1 undo) で分配。Esc は text_input が focus clear → 次 frame で
        // resp.focused == false 検出 → lyric_editing = None (2 frame で UX 完了)。
        if let Some(edit_id) = lyric_editing {
            // borrow conflict 回避: 必要なデータを先にコピーしてから self.text_input を呼ぶ。
            let edit_data = notes.iter().find(|n| n.id == edit_id).map(|n| {
                let raw_rect = note_to_rect(n, view, grid);
                let prefill = n.lyric.as_deref().unwrap_or("").to_string();
                (raw_rect, prefill)
            });
            if let Some((raw_rect, prefill)) = edit_data {
                // grid 内に clip (note rect が grid 外にはみ出している場合)
                let clipped_x = raw_rect.x.max(grid.x);
                let clipped_y = raw_rect.y.max(grid.y);
                let clipped_w = (raw_rect.x + raw_rect.w).min(grid.x + grid.w) - clipped_x;
                let clipped_h = (raw_rect.y + raw_rect.h).min(grid.y + grid.h) - clipped_y;
                // M14 Phase 59: text_input overlay の最小表示サイズ (8 px)。 旧 `style.lyric_font_px`
                // を threshold にしていたが、 lyric_font_px が MAX cap になったため固定値に変更
                // (text_input は font_size 14 px 既定で 8 px 高あれば最低限読める)。
                if clipped_w < 8.0 || clipped_h < 8.0 {
                    // 表示できないほど小さい (zoom out 過多 etc) → 編集モード解除
                    self.widget_state::<PianoRollState>(wid).lyric_editing = None;
                    lyric_editing = None;
                } else {
                    let clipped = Rect {
                        x: clipped_x,
                        y: clipped_y,
                        w: clipped_w,
                        h: clipped_h,
                    };
                    // text_input_at_focused: id に edit_id を含めることで note 切替時に
                    // widget id が変化 → was_widget_visible_last_frame == false → 自動 focus +
                    // 全選択 (gained_focus 検知経由)。
                    let resp = self.text_input_at_focused(
                        ("piano_roll_lyric", edit_id),
                        clipped,
                        &prefill,
                        // on_change は per-keystroke で呼ばれるが、ここでは何もしない
                        // (commit 検出で 1 度だけ SetLyrics 発行 = 1 undo)。
                        |_new_text| Edit::mutate(|_: &mut M| {}),
                    );

                    // daw_01 #112「テキスト入力は focus loss で確定」: Enter (committed) と
                    // 外 click (blurred) のどちらでも歌詞を確定する。 違いは確定後の遷移で、
                    // Enter は分配先の次 note へ編集を継続、 外 click はその場で編集終了。
                    // Esc (= committed/blurred でない focus loss) のみ破棄。
                    if resp.committed || resp.blurred {
                        let committed_text = resp.committed_text.unwrap_or_default();
                        let morae: Vec<String> = if committed_text.is_empty() {
                            // 空文字 commit → 起点 note の歌詞を None に (= 削除)
                            Vec::new()
                        } else {
                            split_into_morae(&committed_text)
                        };
                        // 起点 note の歌詞 update count: 空入力は 1 (起点を None に)、
                        // それ以外は morae.len() 個分の連続 note を取る。
                        let target_count = morae.len().max(1);
                        let target_ids =
                            collect_next_notes_for_lyric(notes, edit_id, target_count);
                        let mut updates: Vec<(NoteId, Option<String>)> =
                            Vec::with_capacity(target_ids.len());
                        for (i, nid) in target_ids.iter().enumerate() {
                            let lyric = morae.get(i).cloned().filter(|s| !s.is_empty());
                            updates.push((*nid, lyric));
                        }
                        // 余り (overflow) を Response に載せる (note 数 < 入力モーラ数の場合)。
                        response.lyric_overflow_morae =
                            morae.len().saturating_sub(target_ids.len());
                        if !updates.is_empty() {
                            self.push_edit(make_edit(PianoRollEditRequest::SetLyrics(updates)));
                        }
                        if resp.committed {
                            // Enter: 分配し終わった先の note へ移動して編集継続。
                            let all_sorted =
                                collect_next_notes_for_lyric(notes, edit_id, usize::MAX);
                            let next_id = all_sorted.get(target_ids.len()).copied();
                            self.widget_state::<PianoRollState>(wid).lyric_editing = next_id;
                            lyric_editing = next_id;
                            // selection も自動追従 (daw_01 UI が同期、note 強調が次 note へ)
                            if let Some(nid) = next_id
                                && selected != [nid].as_slice()
                            {
                                let prev = selected.to_vec();
                                self.push_edit(make_edit(PianoRollEditRequest::Select {
                                    prev,
                                    next: vec![nid],
                                }));
                                response.selection_changed = true;
                            }
                        } else {
                            // 外 click (blur): 現 note の歌詞を確定して編集終了 (次 note へは進まない)。
                            self.widget_state::<PianoRollState>(wid).lyric_editing = None;
                            lyric_editing = None;
                        }
                    } else if !resp.focused {
                        // Esc 検出: text_input が clear_focus_if_focused →
                        // 次 frame で resp.focused = false かつ committed/blurred でない。破棄。
                        self.widget_state::<PianoRollState>(wid).lyric_editing = None;
                        lyric_editing = None;
                    }
                }
            } else {
                // defensive: notes に edit_id が無い (フレーム頭の sync check で本来 None
                // にしているので通常起こらない)
                self.widget_state::<PianoRollState>(wid).lyric_editing = None;
                lyric_editing = None;
            }
        }
        response.lyric_editing = lyric_editing;

        response
    }
}

// ============================================================
// Internal drawing helpers
// ============================================================

/// M14 Phase 117 (daw_01 #093): 鍵盤オクターブラベルの色を、 その行の **実効背景** から決める。
/// `key_fill` (白鍵 / 黒鍵 fill) の上に `overlay` (root_row_overlay / out overlay、 無ければ `None`) を
/// alpha 合成した「実際に目に入る色」 の WCAG relative luminance で `label_fg_dark` / `label_fg_light` を
/// 選ぶ (arrangement clip 名 #060 と同じ `crate::color` SSoT)。 `label_auto_contrast == false` なら
/// `fallback` (旧固定色) をそのまま返す。
fn keyboard_label_color(
    style: &PianoRollStyle,
    key_fill: Color,
    overlay: Option<Color>,
    fallback: Color,
) -> Color {
    if !style.label_auto_contrast {
        return fallback;
    }
    let bg = match overlay {
        Some(ov) => crate::color::composite_over(ov, key_fill),
        None => key_fill,
    };
    crate::color::pick_contrast(bg, style.label_fg_light, style.label_fg_dark)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::too_many_lines)]
fn draw_grid_background<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    grid: Rect,
    kbd: Rect,
    view: PianoRollView,
    style: &PianoRollStyle,
) {
    // (a) 主領域背景
    hctx.push_rect(RectCommand {
        rect: grid,
        fill: style.bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    let geom = RowGeometry::compute(view, grid);
    let scale = view.scale;
    let root_pc_opt = geom.root_pc();

    // Per-row 背景 (黒鍵 overlay + scale overlay 3rd pass)。 mode で iter 方法が変わる。
    if geom.fold {
        // (b') Fold: in-scale 行のみ、 等高 row_h
        for (idx, &pitch) in geom.fold_rows.iter().enumerate() {
            let y = grid.y + idx as f32 * geom.row_h;
            // 黒鍵 row overlay (in-scale 行のうち黒鍵 = root が黒鍵 / pentatonic 等で発生)
            if is_black_key(pitch) {
                hctx.push_rect(RectCommand {
                    rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                    fill: style.black_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            }
            // root row overlay
            if Some(pitch % 12) == root_pc_opt {
                hctx.push_rect(RectCommand {
                    rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                    fill: style.root_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            }
        }
    } else {
        // (b) Linear (None / Highlight): 12 行 1 octave、 旧 iteration
        let pitch_top_int = view.pitch_top.floor() as i32;
        let pitch_visible_int = view.pitch_visible.ceil() as i32;
        for i in 0..=pitch_visible_int {
            let pitch_i = pitch_top_int - i;
            if !(0..=127).contains(&pitch_i) {
                continue;
            }
            let pitch = pitch_i as u8;
            let y = grid.y + (view.pitch_top - pitch_i as f32) * geom.row_h;
            // (b-1) 黒鍵 row overlay
            if is_black_key(pitch) {
                hctx.push_rect(RectCommand {
                    rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                    fill: style.black_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            }
            // (b-2) M14 Phase 70 / daw_01 #042: scale overlay 3rd pass (Highlight mode)
            if let Some(sc) = scale {
                let pc = pitch % 12;
                if pc == sc.root % 12 {
                    hctx.push_rect(RectCommand {
                        rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                        fill: style.root_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                } else if !sc.is_in_scale(pitch) {
                    hctx.push_rect(RectCommand {
                        rect: Rect { x: grid.x, y, w: grid.w, h: geom.row_h },
                        fill: style.out_of_scale_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                }
            }
        }
    }

    // (c) 拍縦線 (1 拍ごと細線、bar 縦線) — M13 Phase 55 で library `Ui::bar_beat_grid` に統合。
    // この関数の caller (piano_roll cached layer) で `hctx.bar_beat_grid` を呼ぶ。

    // (d) keyboard widget (左端、kbd.w > 0 のみ)
    if kbd.w == 0.0 {
        return;
    }
    // 背景
    hctx.push_rect(RectCommand {
        rect: kbd,
        fill: style.keyboard_bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });

    if geom.fold {
        // Fold: in-scale 行のみ、 全行にラベル
        for (idx, &pitch) in geom.fold_rows.iter().enumerate() {
            let y = grid.y + idx as f32 * geom.row_h;
            let key_rect = Rect {
                x: kbd.x,
                y,
                w: (kbd.w - 1.0).max(0.0),
                h: (geom.row_h - 1.0).max(0.0),
            };
            let fill =
                if is_black_key(pitch) { style.black_key } else { style.white_key };
            hctx.push_rect(RectCommand {
                rect: key_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
            // root row overlay
            if Some(pitch % 12) == root_pc_opt {
                hctx.push_rect(RectCommand {
                    rect: key_rect,
                    fill: style.root_row_overlay,
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
            }
            // Label: 全 in-scale 行に
            if geom.row_h >= 8.0 {
                let pc = pitch % 12;
                let name = pitch_class_name_spelled(pc, scale.is_some_and(|s| s.prefer_flats));
                let is_root = Some(pc) == root_pc_opt;
                let (text, fallback) = if is_root {
                    let octave = (i32::from(pitch) / 12) - 1;
                    (format!("{name}{octave}"), style.root_label_fg)
                } else {
                    (name.to_string(), style.in_scale_label_fg)
                };
                // M14 Phase 117 (daw_01 #093): root 行は root_row_overlay 重畳、 in-scale 行は key fill のみ。
                // 実効背景の輝度で dark/light を選ぶ (白鍵 in-scale 行で明文字が潰れる旧 symptom も解消)。
                let overlay = if is_root { Some(style.root_row_overlay) } else { None };
                let color = keyboard_label_color(style, fill, overlay, fallback);
                hctx.push_text(GlyphArea {
                    text: text.into(),
                    left: kbd.x + 4.0,
                    top: y,
                    font_size: style.c_label_font_px,
                    line_height: style.c_label_font_px * 1.2,
                    color,
                    clip_rect: None,
                    ..GlyphArea::default()
                });
            }
        }
    } else {
        // Linear (None / Highlight)
        let pitch_top_int = view.pitch_top.floor() as i32;
        let pitch_visible_int = view.pitch_visible.ceil() as i32;
        for i in 0..=pitch_visible_int {
            let pitch_i = pitch_top_int - i;
            if !(0..=127).contains(&pitch_i) {
                continue;
            }
            let pitch = pitch_i as u8;
            let y = grid.y + (view.pitch_top - pitch_i as f32) * geom.row_h;
            let key_rect = Rect {
                x: kbd.x,
                y,
                w: (kbd.w - 1.0).max(0.0),
                h: (geom.row_h - 1.0).max(0.0),
            };
            let fill =
                if is_black_key(pitch) { style.black_key } else { style.white_key };
            hctx.push_rect(RectCommand {
                rect: key_rect,
                fill,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
            // scale overlay (Highlight)
            if let Some(sc) = scale {
                let pc = pitch % 12;
                if pc == sc.root % 12 {
                    hctx.push_rect(RectCommand {
                        rect: key_rect,
                        fill: style.root_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                } else if !sc.is_in_scale(pitch) {
                    hctx.push_rect(RectCommand {
                        rect: key_rect,
                        fill: style.out_of_scale_row_overlay,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                }
            }
            // Label
            if geom.row_h >= 8.0 {
                if let Some(root_pc) = root_pc_opt {
                    // Highlight mode: root pitch class のオクターブだけ label
                    if pitch % 12 == root_pc {
                        let octave = (pitch_i / 12) - 1;
                        let name =
                            pitch_class_name_spelled(root_pc, scale.is_some_and(|s| s.prefer_flats));
                        // M14 Phase 117 (daw_01 #093): Highlight mode の root 行は常に root_row_overlay
                        // 重畳 (warm cream)。 その実効背景で auto-contrast → warm-on-warm 潰れを解消。
                        let color = keyboard_label_color(
                            style,
                            fill,
                            Some(style.root_row_overlay),
                            style.root_label_fg,
                        );
                        hctx.push_text(GlyphArea {
                            text: format!("{name}{octave}").into(),
                            left: kbd.x + 4.0,
                            top: y,
                            font_size: style.c_label_font_px,
                            line_height: style.c_label_font_px * 1.2,
                            color,
                            clip_rect: None,
                            ..GlyphArea::default()
                        });
                    }
                } else if pitch.is_multiple_of(12) {
                    // 旧挙動: C オクターブだけ
                    let octave = (pitch_i / 12) - 1;
                    // M14 Phase 117 (daw_01 #093): scale=None の C 行は overlay 無し (key fill のみ)。
                    // C は白鍵なので auto-contrast で暗文字が選ばれ、 旧 `c_label_color` (dark) と整合。
                    let color = keyboard_label_color(style, fill, None, style.c_label_color);
                    hctx.push_text(GlyphArea {
                        text: format!("C{octave}").into(),
                        left: kbd.x + 4.0,
                        top: y,
                        font_size: style.c_label_font_px,
                        line_height: style.c_label_font_px * 1.2,
                        color,
                        clip_rect: None,
                        ..GlyphArea::default()
                    });
                }
            }
        }
    }
}

/// visible note を grid 内に clip して描画。M9 Phase 44c 以降 `lyric: Some(...)` が
/// あれば note rect の左端に重ねて描画する (note 高さが lyric font 1 行分以上あるときのみ)。
#[allow(clippy::too_many_arguments)]
fn draw_notes<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    note_fill_fn: NoteFillFn,
    bg: Color,
    radius_px: f32,
    muted_hatch_color: Color,
    muted_hatch_spacing_px: f32,
    muted_hatch_width_px: f32,
) {
    for note in visible {
        let r = note_to_rect(note, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        let clipped = Rect {
            x: x_left,
            y: y_top,
            w: x_right - x_left,
            h: y_bot - y_top,
        };
        // クリップ色 (色 None なら velocity 色) → dim/lock 沈め → mute 沈め。
        let fill = note_fill_color(note, note_fill_fn, bg);
        hctx.push_rect(note_rect_command(clipped, fill, radius_px));
        if note.muted {
            crate::widgets::push_muted_hatch(
                hctx,
                clipped,
                clipped,
                muted_hatch_color,
                muted_hatch_spacing_px,
                muted_hatch_width_px,
            );
        }
    }
}

/// (M14 Phase 59) note 上に歌詞を描画する独立 pass。 selection overlay / drag preview の
/// **後** に呼んで lyric を最前面に置く (旧設計では cached 内 draw_notes 内で描いていたため
/// selection の黄色 fill に覆われていた、 daw_01 #017 動作確認で発覚)。
///
/// font_size は note rect の高さに連動 (`note_h * 0.7` を `lyric_font_px_max` で cap)。
/// 縦方向中央寄せで note 内に収める。 lyric_editing 中の note は text_input overlay が
/// 出ているので skip (= 編集中歌詞は text_input 内で表示される)。
fn draw_lyrics<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    grid: Rect,
    lyric_color: Color,
    lyric_font_px_max: f32,
    skip_note_id: Option<NoteId>,
) {
    for note in visible {
        if Some(note.id) == skip_note_id {
            continue;
        }
        let Some(lyric) = note.lyric.as_ref() else {
            continue;
        };
        let r = note_to_rect(note, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        let clipped = Rect {
            x: x_left,
            y: y_top,
            w: x_right - x_left,
            h: y_bot - y_top,
        };
        // note 高さに比例した font (cap = lyric_font_px_max)、 最低 7px 以上で描画。
        // caller が lyric_font_px < 7.0 を style に設定した場合、 floor を cap まで下げて
        // `f32::clamp(min > max)` の panic を防ぐ (cap が勝つ動作)。
        let floor = 7.0_f32.min(lyric_font_px_max);
        let font_size = (clipped.h * 0.75).clamp(floor, lyric_font_px_max);
        if clipped.h < font_size + 1.0 || clipped.w < font_size {
            continue;
        }
        // 縦方向中央寄せ: top = clipped.y + (clipped.h - font_size) / 2
        let top = clipped.y + ((clipped.h - font_size) * 0.5).max(0.0);
        hctx.push_text(GlyphArea {
            text: lyric.clone(),
            left: clipped.x + 2.0,
            top,
            font_size,
            line_height: font_size * 1.1,
            color: lyric_color,
            clip_rect: Some(clipped),
            ..GlyphArea::default()
        });
    }
}

/// selected note に黄色ハイライト + 白枠 overlay を描画 (cached の外、毎フレーム)。
fn draw_selection_overlay<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    selected_set: &HashSet<NoteId>,
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
) {
    for note in visible {
        if !selected_set.contains(&note.id) {
            continue;
        }
        let r = note_to_rect(note, view, grid);
        let pad = style.note_selected_pad_px;
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: r.x - pad,
                y: r.y - pad,
                w: r.w + pad * 2.0,
                h: r.h + pad * 2.0,
            },
            fill: style.note_selected_fill,
            border: style.note_selected_border,
            border_width: style.note_selected_border_w,
            radius: [3.0; 4],
            // grid で clip する — 他 pass (draw_notes / drag preview / lyrics) は
            // 全て clamp/clip 済みで、 ここだけ無 clip だと視界端の選択 note の
            // ハイライトが keyboard / ruler / velocity lane にはみ出す (review)。
            clip_rect: Some(grid),
        });
    }
}

/// drag 中の shifted note rect (drag preview) を描画。
#[allow(clippy::too_many_arguments)]
fn draw_drag_preview<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    nd: &NoteDragSession,
    view: PianoRollView,
    grid: Rect,
    style: &PianoRollStyle,
    beat_delta: f64,
    pitch_delta: i32,
    min_len: f64,
) {
    // M14 Phase 83 / daw_01 #054: Ctrl 保持の copy drag は ghost を clone 色 (緑系) で描き、
    // move drag (黄) と視覚区別する。`nd.last_ctrl` は release commit と同じ careful-update 値。
    let (ghost_fill, ghost_border) = if nd.last_ctrl {
        (style.note_clone_ghost_fill, style.note_clone_ghost_border)
    } else {
        (style.note_selected_fill, style.note_selected_border)
    };
    for a in &nd.anchors {
        let (start_beat, len_beats, pitch) =
            drag_preview_geometry(*a, nd.kind, beat_delta, pitch_delta, min_len, view, nd.last_alt);
        let r = note_geometry_to_rect(start_beat, len_beats, pitch, view, grid);
        let x_left = r.x.max(grid.x);
        let x_right = (r.x + r.w).min(grid.x + grid.w);
        let y_top = r.y.max(grid.y);
        let y_bot = (r.y + r.h).min(grid.y + grid.h);
        if x_right <= x_left || y_bot <= y_top {
            continue;
        }
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: x_left,
                y: y_top,
                w: x_right - x_left,
                h: y_bot - y_top,
            },
            fill: ghost_fill,
            border: ghost_border,
            border_width: style.note_selected_border_w,
            radius: [style.note_border_radius_px; 4],
            clip_rect: None,
        });
    }
}

/// (M9 Phase 45c / M14 Phase 64) velocity lane の描画。`vel_area` は keyboard を除いた grid と同じ x 範囲。
/// 各 visible note の start_beat 位置に幅 `style.velocity_bar_width_px` の縦 bar を、
/// `velocity / 127` の比率で高さを決めて bottom-aligned で描画する。
///
/// (M14 Phase 64 / daw_01 #018) `velocity_override` が `Some((ids, new_vel))` のとき、
/// 含まれる id の note は `n.velocity` の代わりに `new_vel` で bar を描画する (drag preview)。
/// drag 中はこの override が active になり、release で None に戻る (= cache 経由で実値が反映)。
fn draw_velocity_lane<M: ?Sized + 'static>(
    hctx: &mut crate::widgets::heavy::HeavyCtx<'_, '_, M>,
    visible: &[Note],
    view: PianoRollView,
    vel_area: Rect,
    style: &PianoRollStyle,
    velocity_override: Option<(&[NoteId], u8)>,
) {
    // 背景塗り
    hctx.push_rect(RectCommand {
        rect: vel_area,
        fill: style.velocity_lane_bg,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    let beat_to_px = f64::from(vel_area.w) / view.len_beats.max(1e-6);
    let half_w = style.velocity_bar_width_px * 0.5;
    for n in visible {
        let vel = match velocity_override {
            Some((ids, ov)) if ids.contains(&n.id) => ov,
            _ => n.velocity,
        };
        let bar_h = vel_area.h * (f32::from(vel) / 127.0);
        if bar_h <= 0.0 {
            continue;
        }
        let cx = vel_area.x + ((n.start_beat - view.start_beat) * beat_to_px) as f32;
        // grid 範囲外は skip (visible は端に半分はみ出る note も含み得る)
        if cx + half_w < vel_area.x || cx - half_w > vel_area.x + vel_area.w {
            continue;
        }
        // bar はそのクリップの色 (色 None なら従来の velocity_bar_color)。
        // バー高さが既に velocity を表すので velocity 陰影は掛けず、dim/lock のみ反映。
        let bar_fill = match n.style.color {
            Some(c) if n.style.locked => dim_toward(c, style.velocity_lane_bg, 0.72),
            Some(c) if n.style.dimmed => dim_toward(c, style.velocity_lane_bg, 0.48),
            Some(c) => c,
            None => style.velocity_bar_color,
        };
        hctx.push_rect(RectCommand {
            rect: Rect {
                x: cx - half_w,
                y: vel_area.y + vel_area.h - bar_h,
                w: style.velocity_bar_width_px,
                h: bar_h,
            },
            fill: bar_fill,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }
}

// (M9 Phase 45e) `draw_playhead_line` は `crate::widgets::playhead` に切り出し済み。
// piano_roll / arrangement 両方が `pub(crate) fn` を呼ぶ形にリファクタ。

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;
    use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey, PhysicalSize};
    use daw_ui_renderer::Scene;

    fn note(id: NoteId, start: f64, len: f64, pitch: u8) -> Note {
        Note { id, start_beat: start, len_beats: len, pitch, velocity: 96, lyric: None, muted: false, style: NoteStyle::default() }
    }

    fn test_view() -> PianoRollView {
        PianoRollView {
            start_beat: 0.0,
            min_start_beat: 0.0,
            len_beats: 4.0,
            pitch_top: 72.0,
            pitch_visible: 24.0,
            keyboard_w: 0.0,
            notes_generation: 0,
            velocity_lane_h: 0.0,
            playhead_beat: None,
            ruler_h: 0.0,
            bpm: 120.0,
            time_sig: (4, 4),
            // 数値検証 test は raw beat 値を期待するので明示 OFF。
            snap: SnapConfig::OFF,
            loop_range: None,
            scale: None,
            snap_pitch_during_drag: false,
            sub_grid_interval_beats: None,
            // 既定長 1 拍 (test の数値検証で扱いやすい値)。 個別 test で上書き可。
            default_note_len_beats: 1.0,
        }
    }

    // -------- Style invariants (M14 Phase 63d / daw_01 #017) --------

    /// `PianoRollStyle::default()` の `bg` (= 白鍵レーン) と `black_row_overlay` を src-over
    /// 合成した結果 (= 黒鍵レーン) は **bg より暗くなる** こと。鍵盤側の white_key > black_key
    /// と濃淡関係を一致させる業界標準動作。 Ableton Live / Cubase / Reaper / FL Studio 慣習。
    #[test]
    fn default_black_row_is_darker_than_white_row() {
        let style = PianoRollStyle::default();
        let bg = style.bg;
        let ov = style.black_row_overlay;
        // src-over: out = src.rgb * src.a + dst.rgb * (1 - src.a)
        let bk_r = ov.r * ov.a + bg.r * (1.0 - ov.a);
        let bk_g = ov.g * ov.a + bg.g * (1.0 - ov.a);
        let bk_b = ov.b * ov.a + bg.b * (1.0 - ov.a);
        assert!(
            bk_r < bg.r && bk_g < bg.g && bk_b < bg.b,
            "黒鍵 row ({bk_r}, {bk_g}, {bk_b}) は bg ({}, {}, {}) より暗いべき (鍵盤と整合)",
            bg.r, bg.g, bg.b
        );
        // 鍵盤側の濃淡関係も同方向であることを念のため確認 (regression 防止)。
        assert!(
            style.white_key.r > style.black_key.r,
            "鍵盤 white_key.r > black_key.r 不変条件"
        );
    }

    // -------- Pure function tests (note_hit / note_hover_cursor / rects_intersect) --------

    #[test]
    fn rects_intersect_overlapping_returns_true() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 5.0, y: 5.0, w: 10.0, h: 10.0 };
        assert!(rects_intersect(a, b));
    }

    #[test]
    fn rects_intersect_disjoint_returns_false() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 20.0, y: 20.0, w: 10.0, h: 10.0 };
        assert!(!rects_intersect(a, b));
    }

    #[test]
    fn rects_intersect_touching_returns_false() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 10.0, y: 0.0, w: 10.0, h: 10.0 };
        assert!(!rects_intersect(a, b));
    }

    /// note (id 0) start=1, len=1, pitch=60 → grid (50,0)-(450,200) で x∈[150,250]、y≈100。
    fn make_test_setup() -> (Vec<Note>, PianoRollView, Rect) {
        let notes = vec![note(0, 1.0, 1.0, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        (notes, view, grid)
    }

    #[test]
    fn note_hit_returns_move_in_center() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 200.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::Move)));
    }

    #[test]
    fn note_hit_excludes_locked_note() {
        // lock されたクリップの note は描画されても hit-test から除外され、
        // 中央 (通常なら Move hit) でも掴めない (参照専用ゴースト)。
        let (mut notes, view, grid) = make_test_setup();
        notes[0].style.locked = true;
        let hit = note_hit(&notes, view, grid, 200.0, 102.0, 4.0);
        assert_eq!(hit, None, "lock note は中央でも hit しない");
    }

    #[test]
    fn note_hit_returns_resize_left_at_left_edge() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 151.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeLeft)));
    }

    #[test]
    fn note_hit_returns_resize_right_at_right_edge() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 249.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeRight)));
    }

    #[test]
    fn note_hit_returns_none_outside_grid() {
        let (notes, view, grid) = make_test_setup();
        let hit = note_hit(&notes, view, grid, 10.0, 10.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn note_hit_returns_none_on_empty_grid_area() {
        let (notes, view, grid) = make_test_setup();
        // grid 内だが note の上ではない
        let hit = note_hit(&notes, view, grid, 200.0, 10.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn note_hover_cursor_returns_move_in_center() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 200.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::Move));
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_left_edge() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 151.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_right_edge() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 249.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
    }

    #[test]
    fn note_hover_cursor_returns_none_outside_grid() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 10.0, 10.0, 4.0);
        assert_eq!(cursor, None);
    }

    #[test]
    fn note_hover_cursor_returns_none_on_empty_area() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 200.0, 10.0, 4.0);
        assert_eq!(cursor, None);
    }

    // -------- Hit-test extension tests (rect 外側 ±resize_handle_px) --------
    // note (id 0) start=1, len=1 → rect x∈[150,250]、edge=4 で拡張範囲 x∈[146,254)。

    #[test]
    fn note_hit_returns_resize_left_at_outer_left_handle() {
        let (notes, view, grid) = make_test_setup();
        // x=147 = r.x(150) - 3 → 拡張範囲内、左端 handle
        let hit = note_hit(&notes, view, grid, 147.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeLeft)));
    }

    #[test]
    fn note_hit_returns_resize_right_at_outer_right_handle() {
        let (notes, view, grid) = make_test_setup();
        // x=252 = r.x+r.w(250) + 2 → 拡張範囲内、右端 handle
        let hit = note_hit(&notes, view, grid, 252.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeRight)));
    }

    #[test]
    fn note_hit_returns_none_just_past_outer_handle() {
        let (notes, view, grid) = make_test_setup();
        // x=145 = r.x(150) - 5 → 拡張範囲 [146, 254) の外
        let hit = note_hit(&notes, view, grid, 145.0, 102.0, 4.0);
        assert_eq!(hit, None);
    }

    #[test]
    fn note_hit_short_note_inside_returns_move() {
        // 短 note (len=0.05 → w=5px、< edge*2=8) の rect 内中央は Move 強制
        let notes = vec![note(0, 1.0, 0.05, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        // r.x = 150, r.w = 5。中央 x=152 で内側
        let hit = note_hit(&notes, view, grid, 152.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::Move)));
    }

    #[test]
    fn note_hit_short_note_outer_left_returns_resize_left() {
        // 短 note でも rect 外側左は ResizeLeft (内外あいまい性なし)
        let notes = vec![note(0, 1.0, 0.05, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        // r.x = 150。x=148 = r.x - 2 → 外側左
        let hit = note_hit(&notes, view, grid, 148.0, 102.0, 4.0);
        assert_eq!(hit, Some((0, NoteDragKind::ResizeLeft)));
    }

    #[test]
    fn note_hit_adjacent_notes_inside_note_owns_shared_handle() {
        // note A (id 0, start=1, len=1) → rect x∈[150,250]、右端拡張 [246,254)
        // note B (id 1, start=2, len=1) → rect x∈[250,350]、左端拡張 [246,254)
        // 共有境界 boundary=250。各 note は自分の rect 内側のハンドル px を所有する
        // (in-rect は outer-extension に無条件で勝つ / daw_01 #053)。
        let notes = vec![note(0, 1.0, 1.0, 60), note(1, 2.0, 1.0, 60)];
        let view = test_view();
        let grid = Rect { x: 50.0, y: 0.0, w: 400.0, h: 200.0 };
        // x=249: A の rect 内側 (in-rect ResizeRight) が B の外側ハンドル (outer ResizeLeft) に勝つ
        assert_eq!(
            note_hit(&notes, view, grid, 249.0, 102.0, 4.0),
            Some((0, NoteDragKind::ResizeRight))
        );
        // x=251: B の rect 内側 (in-rect ResizeLeft) が A の外側ハンドル (outer) に勝つ
        assert_eq!(
            note_hit(&notes, view, grid, 251.0, 102.0, 4.0),
            Some((1, NoteDragKind::ResizeLeft))
        );
        // x=250: 共有境界。半開区間で B の rect 内側 → B の左端 resize
        assert_eq!(
            note_hit(&notes, view, grid, 250.0, 102.0, 4.0),
            Some((1, NoteDragKind::ResizeLeft))
        );
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_outer_left_handle() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 147.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
    }

    #[test]
    fn note_hover_cursor_returns_ewresize_at_outer_right_handle() {
        let (notes, view, grid) = make_test_setup();
        let cursor = note_hover_cursor(&notes, view, grid, 252.0, 102.0, 4.0);
        assert_eq!(cursor, Some(CursorIcon::EwResize));
    }

    #[test]
    fn note_to_rect_basic_position() {
        let (_, view, grid) = make_test_setup();
        let r = note_to_rect(&note(0, 1.0, 1.0, 60), view, grid);
        // beat_to_px = 400/4 = 100, pitch_to_px = 200/24 ≈ 8.33
        // x = 50 + 1*100 = 150, w = 1*100 = 100
        assert!((r.x - 150.0).abs() < 1e-3);
        assert!((r.w - 100.0).abs() < 1e-3);
    }

    #[test]
    fn is_black_key_basic() {
        assert!(is_black_key(1)); // C#
        assert!(is_black_key(3)); // D#
        assert!(!is_black_key(0)); // C
        assert!(!is_black_key(4)); // E
    }

    /// M14 Phase 117 (daw_01 #093): 鍵盤ラベル色が行の実効背景 (key fill + overlay 合成) の輝度で
    /// dark / light を選び、 opt-out 時は fallback を返す。
    #[test]
    fn keyboard_label_color_auto_contrast_picks_by_row_bg() {
        let style = PianoRollStyle::default();
        // 白鍵 + root_row_overlay (warm cream) → 明るい背景 → 暗ラベル (旧 warm-on-warm 潰れを解消)。
        let on_root =
            keyboard_label_color(&style, style.white_key, Some(style.root_row_overlay), style.root_label_fg);
        assert_eq!(on_root, style.label_fg_dark, "warm cream root 行 → 暗ラベル");
        // 黒鍵 (overlay 無し) → 暗い背景 → 明ラベル。
        let on_black = keyboard_label_color(&style, style.black_key, None, style.in_scale_label_fg);
        assert_eq!(on_black, style.label_fg_light, "黒鍵行 → 明ラベル");
        // 黒鍵 root + root_row_overlay (warm cream overlay を暗い黒鍵に重ねても実効輝度は閾値下) → 明ラベル。
        // root が黒鍵 (F# pentatonic 等) の Highlight mode で warm-on-dark にならないことの確認。
        let on_black_root =
            keyboard_label_color(&style, style.black_key, Some(style.root_row_overlay), style.root_label_fg);
        assert_eq!(on_black_root, style.label_fg_light, "黒鍵 root + overlay 行 → 明ラベル");
        // 白鍵 in-scale (overlay 無し) → 明るい → 暗ラベル (旧 in_scale_label_fg の明文字が潰れる症状も解消)。
        let on_white = keyboard_label_color(&style, style.white_key, None, style.in_scale_label_fg);
        assert_eq!(on_white, style.label_fg_dark, "白鍵 in-scale 行 → 暗ラベル");
        // opt-out: fallback 固定色をそのまま返す。
        let off = PianoRollStyle { label_auto_contrast: false, ..PianoRollStyle::default() };
        assert_eq!(
            keyboard_label_color(&off, off.white_key, Some(off.root_row_overlay), off.root_label_fg),
            off.root_label_fg,
            "auto_contrast=false は fallback 固定色"
        );
    }

    // -------- M14 Phase 59 / daw_01 #017: split_into_morae unit tests --------

    #[test]
    fn split_into_morae_single_char() {
        assert_eq!(split_into_morae("あ"), vec!["あ"]);
    }

    #[test]
    fn split_into_morae_basic_distribution() {
        assert_eq!(split_into_morae("あいうえ"), vec!["あ", "い", "う", "え"]);
    }

    #[test]
    fn split_into_morae_combines_yo() {
        assert_eq!(split_into_morae("きゃ"), vec!["きゃ"]);
    }

    #[test]
    fn split_into_morae_combines_tsu() {
        // "ぱっと" = ぱ + っ (結合) + と → 2 モーラ
        assert_eq!(split_into_morae("ぱっと"), vec!["ぱっ", "と"]);
    }

    #[test]
    fn split_into_morae_consecutive_small_kana() {
        // "きゃっ" = き + ゃ (結合) + っ (続けて結合) → 1 モーラ
        assert_eq!(split_into_morae("きゃっ"), vec!["きゃっ"]);
    }

    #[test]
    fn split_into_morae_leading_small_kana_defensive() {
        // 先頭小書きは defensive で単独 1 モーラ (通常入力では発生しない)
        assert_eq!(split_into_morae("ぁい"), vec!["ぁ", "い"]);
    }

    #[test]
    fn split_into_morae_empty() {
        assert_eq!(split_into_morae(""), Vec::<String>::new());
    }

    #[test]
    fn split_into_morae_long_kana() {
        // "しゅんかんいどう" = しゅ / ん / か / ん / い / ど / う → 7 モーラ
        assert_eq!(
            split_into_morae("しゅんかんいどう"),
            vec!["しゅ", "ん", "か", "ん", "い", "ど", "う"]
        );
    }

    #[test]
    fn split_into_morae_ascii_one_per_char() {
        assert_eq!(split_into_morae("abc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_into_morae_katakana_yo() {
        // カタカナの拗音も同様に結合
        assert_eq!(split_into_morae("シュン"), vec!["シュ", "ン"]);
    }

    // -------- M14 Phase 59 / daw_01 #017: collect_next_notes_for_lyric unit tests --------

    #[test]
    fn collect_next_notes_returns_self_first() {
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60), note(30, 2.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 2);
        assert_eq!(result, vec![10, 20]);
    }

    #[test]
    fn collect_next_notes_sorted_by_start_beat() {
        // notes が start_beat ソート前提だが、関数内で sort を保証
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60), note(30, 2.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 3);
        assert_eq!(result, vec![10, 20, 30]);
    }

    #[test]
    fn collect_next_notes_same_beat_pitch_desc() {
        // 同 start_beat なら高 pitch が先 (歌詞編集モードで「高音先取り」が歌唱メロディと整合)
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 0.0, 1.0, 72), note(30, 1.0, 1.0, 60)];
        // 起点 = id 20 (pitch 72、start_beat 0.0)
        // sorted: 20 (0.0, 72) → 10 (0.0, 60) → 30 (1.0, 60)
        let result = collect_next_notes_for_lyric(&notes, 20, 3);
        assert_eq!(result, vec![20, 10, 30]);
    }

    #[test]
    fn collect_next_notes_truncates_when_count_exceeds() {
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 5);
        assert_eq!(result, vec![10, 20], "残数が count より少ないとき truncate");
    }

    #[test]
    fn collect_next_notes_empty_when_id_not_found() {
        let notes = vec![note(10, 0.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 999, 3);
        assert_eq!(result, Vec::<NoteId>::new());
    }

    #[test]
    fn collect_next_notes_zero_count() {
        let notes = vec![note(10, 0.0, 1.0, 60), note(20, 1.0, 1.0, 60)];
        let result = collect_next_notes_for_lyric(&notes, 10, 0);
        assert_eq!(result, Vec::<NoteId>::new());
    }

    // -------- Widget integration tests --------

    /// 簡易テスト Model (no-Clone 不変条件: Clone/Default/Hash 不要)。
    struct TestModel {
        notes: Vec<Note>,
        selected: Vec<NoteId>,
        last_request: Option<RequestKind>,
        last_select_prev: Option<Vec<NoteId>>,
        last_select_next: Option<Vec<NoteId>>,
        /// (M14 Phase 59) 最後に発行された `SetLyrics` の内容。
        last_set_lyrics: Option<Vec<(NoteId, Option<String>)>>,
        /// (M14 Phase 61d / daw_01 #012) 最後に Add request で渡された note の pitch。
        last_added_pitch: Option<u8>,
        /// 最後に Add request で渡された note の `(start_beat, len_beats)`。
        last_added: Option<(f64, f64)>,
        /// (M14 Phase 64 / daw_01 #018) 最後に発行された `SetVelocity` の内容。
        last_set_velocity: Option<Vec<VelocityUpdate>>,
        /// (M14 Phase 69 / daw_01 #041) ruler 上 click / drag で発行された全 `SetPlayheadBeat` の
        /// beat 列 (press 即発行 + continuation の連続発火を検証するため log 化)。
        playhead_beats: Vec<f64>,
        /// (M14 Phase 69 / daw_01 #041) 最後に発行された `SetLoopRange { start, end }`。
        last_set_loop_range: Option<(f64, f64)>,
        /// edge auto-scroll で発行された全 `ScrollByBeats` の delta 列。
        scroll_by_beats: Vec<f64>,
        /// edge auto-scroll で発行された全 `SetTopPitch` の値列。
        top_pitch_sets: Vec<u8>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RequestKind {
        Add,
        Delete,
        Move,
        Copy,
        Resize,
        Select,
        SetLyrics,
        SetVelocity,
        SetPlayheadBeat,
        SetLoopRange,
        ScrollByBeats,
        SetTopPitch,
    }

    impl TestModel {
        fn new(notes: Vec<Note>) -> Self {
            Self {
                notes,
                selected: Vec::new(),
                last_request: None,
                last_select_prev: None,
                last_select_next: None,
                last_set_lyrics: None,
                last_added_pitch: None,
                last_added: None,
                last_set_velocity: None,
                playhead_beats: Vec::new(),
                last_set_loop_range: None,
                scroll_by_beats: Vec::new(),
                top_pitch_sets: Vec::new(),
            }
        }
    }

    fn make_dispatch(
    ) -> impl Fn(PianoRollEditRequest) -> Edit<TestModel> + Send + Sync + 'static + Clone {
        |req: PianoRollEditRequest| -> Edit<TestModel> {
            match req {
                PianoRollEditRequest::Add(notes) => {
                    let pitch = notes.first().map(|n| n.pitch);
                    let geom = notes.first().map(|n| (n.start_beat, n.len_beats));
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::Add);
                        m.last_added_pitch = pitch;
                        m.last_added = geom;
                    })
                }
                PianoRollEditRequest::Delete(notes) => Edit::mutate(move |m: &mut TestModel| {
                    m.last_request = Some(RequestKind::Delete);
                    let ids: HashSet<NoteId> = notes.iter().map(|n| n.id).collect();
                    m.notes.retain(|x| !ids.contains(&x.id));
                }),
                PianoRollEditRequest::Move(_) => {
                    Edit::mutate(|m: &mut TestModel| m.last_request = Some(RequestKind::Move))
                }
                PianoRollEditRequest::Copy(_) => {
                    Edit::mutate(|m: &mut TestModel| m.last_request = Some(RequestKind::Copy))
                }
                PianoRollEditRequest::Resize(_) => {
                    Edit::mutate(|m: &mut TestModel| m.last_request = Some(RequestKind::Resize))
                }
                PianoRollEditRequest::Select { prev, next } => {
                    let prev_clone = prev.clone();
                    let next_clone = next.clone();
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::Select);
                        m.last_select_prev = Some(prev_clone.clone());
                        m.last_select_next = Some(next_clone.clone());
                        m.selected.clone_from(&next_clone);
                    })
                }
                PianoRollEditRequest::SetLyrics(updates) => {
                    let updates_clone = updates.clone();
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::SetLyrics);
                        // 実際の Model 反映 (歌詞を Note.lyric に書き戻す)
                        for (id, lyric) in &updates_clone {
                            if let Some(n) = m.notes.iter_mut().find(|n| n.id == *id) {
                                n.lyric = lyric.as_deref().map(Arc::from);
                            }
                        }
                        m.last_set_lyrics = Some(updates_clone.clone());
                    })
                }
                PianoRollEditRequest::SetVelocity(updates) => {
                    let updates_clone = updates.clone();
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::SetVelocity);
                        // 実際の Model 反映 (velocity を Note.velocity に書き戻す)
                        for (id, vel) in &updates_clone {
                            if let Some(n) = m.notes.iter_mut().find(|n| n.id == *id) {
                                n.velocity = *vel;
                            }
                        }
                        m.last_set_velocity = Some(updates_clone.clone());
                    })
                }
                PianoRollEditRequest::SetPlayheadBeat(beat) => {
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::SetPlayheadBeat);
                        m.playhead_beats.push(beat);
                    })
                }
                PianoRollEditRequest::SetLoopRange { start, end } => {
                    Edit::mutate(move |m: &mut TestModel| {
                        m.last_request = Some(RequestKind::SetLoopRange);
                        m.last_set_loop_range = Some((start, end));
                    })
                }
                PianoRollEditRequest::ScrollByBeats(by) => Edit::mutate(move |m: &mut TestModel| {
                    m.last_request = Some(RequestKind::ScrollByBeats);
                    m.scroll_by_beats.push(by);
                }),
                PianoRollEditRequest::SetTopPitch(p) => Edit::mutate(move |m: &mut TestModel| {
                    m.last_request = Some(RequestKind::SetTopPitch);
                    m.top_pitch_sets.push(p);
                }),
            }
        }
    }

    /// rect (0,0)-(800,400) の grid 内 (kbd_w=0, view 4 拍 × 24 pitch)。
    fn run_frame<F: FnOnce(&mut Ui<'_, TestModel>)>(
        host: &mut UiHost<TestModel>,
        model: &mut TestModel,
        input: FrameInput,
        f: F,
    ) {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        let edits = host.frame_to_edits(model, &mut scene, screen, input, |_, ui| {
            f(ui);
        });
        for e in edits {
            e.apply(model);
        }
    }

    /// (M14 Phase 61d / daw_01 #012) Insert shortcut で視覚行の **下半分** にカーソルがあっても
    /// その行の pitch (= ceil) で Add される。 旧 `pitch_f.round()` は半行ぶん上にずれて 1 pitch
    /// 下のノートが追加される bug があった。 test_view: pitch_top=72, pitch_visible=24, grid h=400
    /// → pitch_to_px = 16.667。 cy=215 は pitch 60 の行 (y ∈ [200, 216.667)) の下半分、
    /// pitch_f = 72 - 215/16.667 = 59.1 → ceil = 60 (正)、 round = 59 (旧 bug)。
    #[test]
    fn piano_roll_insert_shortcut_uses_ceil_for_pitch() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.shortcut_map_mut().bind("add_note", "Insert");
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let key = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Insert,
        };
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((100.0, 215.0)), // pitch 60 の visual 行の下半分
                ..PointerFrame::default()
            },
            keyboard: vec![key],
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input, |ui| {
            let sel: Vec<NoteId> = vec![];
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Add));
        assert_eq!(
            model.last_added_pitch,
            Some(60),
            "cy=215 (pitch 60 行の下半分) は ceil で pitch=60、 round だと 59 にずれる (#012)"
        );
    }

    /// Insert shortcut で Add request が発行される (id=0 placeholder)。
    #[test]
    fn piano_roll_pushes_add_edit_on_insert_shortcut() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.shortcut_map_mut().bind("add_note", "Insert");
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let key = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Insert,
        };
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((100.0, 100.0)),
                ..PointerFrame::default()
            },
            keyboard: vec![key],
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input, |ui| {
            let sel: Vec<NoteId> = vec![];
            let dispatch = make_dispatch();
            let _resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Add));
    }

    /// Delete shortcut + selected あり → Delete request 発行。
    #[test]
    fn piano_roll_pushes_delete_edit_on_delete_shortcut() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 0.0, 0.5, 60), note(2, 1.0, 0.5, 64)]);
        model.selected = vec![1, 2];
        let view = test_view();
        let style = PianoRollStyle::default();
        let key = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Delete,
        };
        let input = FrameInput {
            keyboard: vec![key],
            ..Default::default()
        };
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Delete));
        // dispatch が Delete を実行すると notes が retain される
        assert!(model.notes.is_empty(), "id 1, 2 が削除された");
    }

    /// short click (drag<4px) on note → Select request、Response.selection_changed = true。
    #[test]
    fn piano_roll_response_emits_selection_changed_on_note_click() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(7, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();

        // press → release を 1 frame で同時に流す (= short click)
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((300.0, 200.0)),
                primary_just_pressed: true,
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel: Vec<NoteId> = vec![];
        let resp_changed = std::cell::Cell::new(false);
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
            resp_changed.set(resp.selection_changed);
        });
        assert_eq!(model.last_request, Some(RequestKind::Select));
        // grid の幅 800、4 拍なので beat_to_px = 200。x=300 = beat 1.5 → note (id 7, start=1, len=1) 上
        assert_eq!(model.last_select_next, Some(vec![7]));
        assert!(resp_changed.get(), "selection_changed = true");
    }

    /// short click on empty area → Select { next: vec![] } (selection clear)。
    #[test]
    fn piano_roll_response_clears_selection_on_empty_click() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 1.0, 1.0, 60)]);
        model.selected = vec![1];
        let view = test_view();
        let style = PianoRollStyle::default();
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)), // 空白の上 (y=50 は note y≈100 から離れている)
                primary_just_pressed: true,
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_select_next, Some(vec![]));
    }

    /// 同 frame に 2 個の piano_roll を `(label, idx)` で並べ、片方の selected が他方に漏れない。
    #[test]
    fn piano_roll_two_instances_independent_state() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let sel0: Vec<NoteId> = vec![1];
        let sel1: Vec<NoteId> = vec![2];

        run_frame(&mut host, &mut model, FrameInput::default(), |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                ("pr", 0),
                Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 },
                &[],
                view,
                &sel0,
                &style,
                dispatch.clone(),
            );
            let _ = ui.piano_roll(
                ("pr", 1),
                Rect { x: 400.0, y: 0.0, w: 400.0, h: 400.0 },
                &[],
                view,
                &sel1,
                &style,
                dispatch,
            );
        });
        // 何も操作しないので selected はそのまま (片方が他方に漏れない)
        assert_eq!(sel0, vec![1]);
        assert_eq!(sel1, vec![2]);
    }

    /// hover で Response.hovered_zone が ResizeLeft (note 左端 1px 内側に pointer)。
    #[test]
    fn piano_roll_response_hovered_zone_resize_left_at_edge() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        // note (id 0) start=1, len=1, pitch=60 → grid (0,0)-(800,400) で x∈[200,400]、y≈200
        // x=201 が左端 4px 内 (= ResizeLeft)
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((201.0, 200.0)),
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let zone = std::cell::Cell::new(None);
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
            zone.set(resp.hovered_zone);
        });
        assert_eq!(zone.get(), Some(NoteDragKind::ResizeLeft));
    }

    /// drag 中 (press → continue) で Response.dragging が Move、まだ Edit は発行されない。
    #[test]
    fn piano_roll_no_edit_during_drag_only_at_release() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: press at note 中央
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((300.0, 200.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let dragging1 = std::cell::Cell::new(None);
        let notes_clone = model.notes.clone();
        let view_owned = view;
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view_owned,
                &sel,
                &style,
                dispatch,
            );
            dragging1.set(resp.dragging);
        });
        assert_eq!(dragging1.get(), Some(NoteDragKind::Move));
        assert_eq!(model.last_request, None, "drag 中は Edit 発行せず");

        // Frame 2: release at 移動先
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 200.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view_owned,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Move), "release で Move 発行");
    }

    // -------- ドラッグ端オートスクロール --------

    /// note Move drag 中、ポインタが grid 右端 hot-zone に入ると `ScrollByBeats(>0)` が発火し、
    /// 中央では発火しない。test_view: 4 拍 × 800px = 200px/拍、zone=28px (default)。
    #[test]
    fn piano_roll_edge_autoscroll_horizontal_on_note_drag() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let run = |host: &mut UiHost<TestModel>, model: &mut TestModel, pos, pressed, just_pressed| {
            let input = FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_pressed: just_pressed,
                    primary_pressed: pressed,
                    ..PointerFrame::default()
                },
                ..Default::default()
            };
            run_frame(host, model, input, |ui| {
                let sel: Vec<NoteId> = vec![];
                let _ = ui.piano_roll("pr", rect, &notes_clone, view, &sel, &style, make_dispatch());
            });
        };
        // press at note 中央 → Move 開始 (中央なので scroll 不発)。
        run(&mut host, &mut model, (300.0, 200.0), true, true);
        assert!(model.scroll_by_beats.is_empty(), "press frame は中央なので scroll 不発");
        // 中央で continuation → scroll 不発。
        run(&mut host, &mut model, (400.0, 200.0), true, false);
        assert!(model.scroll_by_beats.is_empty(), "中央 drag は scroll 不発");
        // 右端 hot-zone で continuation → 前方 (拍増) へ ScrollByBeats。
        run(&mut host, &mut model, (795.0, 200.0), true, false);
        assert_eq!(model.scroll_by_beats.len(), 1, "右端 drag で ScrollByBeats 1 件");
        assert!(
            model.scroll_by_beats[0] > 0.0,
            "右端 = 前方 (拍増) へスクロール: got {}",
            model.scroll_by_beats[0]
        );
    }

    /// note Move drag 中、ポインタが grid 下端 hot-zone に入ると `SetTopPitch` が発火し、より低い
    /// pitch (top_pitch 減) を露出する。test_view: pitch_visible=24 / grid h=400 = 0.06 semitone/px。
    #[test]
    fn piano_roll_edge_autoscroll_vertical_on_note_drag() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view(); // pitch_top = 72
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let run = |host: &mut UiHost<TestModel>, model: &mut TestModel, pos, just_pressed| {
            let input = FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_pressed: just_pressed,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            };
            run_frame(host, model, input, |ui| {
                let sel: Vec<NoteId> = vec![];
                let _ = ui.piano_roll("pr", rect, &notes_clone, view, &sel, &style, make_dispatch());
            });
        };
        // press at note 中央 → Move 開始。
        run(&mut host, &mut model, (300.0, 200.0), true);
        assert!(model.top_pitch_sets.is_empty(), "press frame は scroll 不発");
        // 下端 hot-zone で continuation → 1 semitone ぶん下へ (top_pitch 減)。
        run(&mut host, &mut model, (300.0, 399.0), false);
        assert_eq!(model.top_pitch_sets.len(), 1, "下端 drag で SetTopPitch 1 件");
        assert!(
            model.top_pitch_sets[0] < 72,
            "下端 = より低い pitch を露出 (top_pitch 減): got {}",
            model.top_pitch_sets[0]
        );
    }

    /// 端近くの note を click-and-hold (移動 < ACTIVATE_PX) しても端スクロールしない。実ドラッグ
    /// (ACTIVATE_PX 以上の移動) で初めて発火する (端の clip / note クリックで view が飛ぶのを防ぐ)。
    /// note を start 3.5 拍 (x≈700) に置き、右端 hot-zone (x≈775) でクリック保持 → 動くまで不発。
    #[test]
    fn piano_roll_edge_autoscroll_gated_by_movement() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 3.5, 0.4, 60)]); // x ≈ 700..780 (右端寄り)
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let run = |host: &mut UiHost<TestModel>, model: &mut TestModel, pos, just_pressed| {
            let input = FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_pressed: just_pressed,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            };
            run_frame(host, model, input, |ui| {
                let sel: Vec<NoteId> = vec![];
                let _ = ui.piano_roll("pr", rect, &notes_clone, view, &sel, &style, make_dispatch());
            });
        };
        // press on the note 内、右端 hot-zone (x=775)。
        run(&mut host, &mut model, (775.0, 200.0), true);
        // 同位置で保持 (移動 0px) → ゲートで不発。
        run(&mut host, &mut model, (775.0, 200.0), false);
        assert!(
            model.scroll_by_beats.is_empty(),
            "click-and-hold (未移動) では端スクロール不発: got {:?}",
            model.scroll_by_beats
        );
        // ACTIVATE_PX 以上 (15px) 動かす → 実ドラッグ判定 → 端スクロール発火。
        run(&mut host, &mut model, (790.0, 200.0), false);
        assert!(
            !model.scroll_by_beats.is_empty(),
            "ACTIVATE_PX 以上動かしたら端スクロール発火"
        );
    }

    /// 既に左端 (start_beat == min_start_beat == 0) のとき、左端 hot-zone へドラッグしても scroll は
    /// floor で clamp され 0 件 = 発火しない。これが効かないと anchor が要求 px 分だけ過剰 shift し続け、
    /// 掴んだ note が画面外へ飛ぶ runaway になる (review CRITICAL)。
    #[test]
    fn piano_roll_edge_autoscroll_clamps_at_left_floor() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        // note を左端寄り (start 0.0, x≈0..100) に置く。test_view は start_beat=0=min_start_beat。
        let mut model = TestModel::new(vec![note(0, 0.0, 0.5, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let run = |host: &mut UiHost<TestModel>, model: &mut TestModel, pos, just_pressed| {
            let input = FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_pressed: just_pressed,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            };
            run_frame(host, model, input, |ui| {
                let sel: Vec<NoteId> = vec![];
                let _ = ui.piano_roll("pr", rect, &notes_clone, view, &sel, &style, make_dispatch());
            });
        };
        // press on the note body (x=40)、release せず左端 zone (x=3) までドラッグ。
        run(&mut host, &mut model, (40.0, 200.0), true);
        run(&mut host, &mut model, (3.0, 200.0), false);
        assert!(
            model.scroll_by_beats.is_empty(),
            "左端 floor では左スクロール不発 (clamp): got {:?}",
            model.scroll_by_beats
        );
    }

    // -------- 空白ダブルクリック作成 (放さずドラッグで長さ決定、Bitwig 流) --------

    /// ダブルクリックの 2 度目の press でカーソルが既定長ノートの右端へ warp し、 そのまま右へ
    /// ドラッグ → 右端が cursor に追従し、 release で `Add` を発行する (cursor＝右端モデル)。
    /// test_view: snap OFF / 4 拍 / grid 800px → beat_per_px = 0.005、default = 1.0。
    /// press x=200 (beat 1.0 = start) → warp 先 = 右端 beat 2.0 = x400 (anchor)。 cursor を x600
    /// (beat 3.0) へドラッグ → 右端 beat 3.0 → len = 3.0−1.0 = 2.0。 warp は test では no-op なので
    /// warp 着地フレーム (cursor=anchor) を明示的に挟む。
    #[test]
    fn note_create_double_click_drag_emits_add_with_dragged_length() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

        // Frame 1: 1 度目の click (release) → UiHost が last_click を記録。
        let f1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f1, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });
        assert_eq!(model.last_request, None, "1 度目 click だけでは作成しない");

        // Frame 2: 2 度目の press (放さない) → 作成 session 開始 (anchor = warp 先 x400)。creating = true。
        let f2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let creating2 = std::cell::Cell::new(false);
        run_frame(&mut host, &mut model, f2, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
            creating2.set(resp.creating);
        });
        assert!(creating2.get(), "2 度目 press で作成 session が active (creating=true)");
        assert_eq!(model.last_request, None, "press だけでは Add しない (release で確定)");

        // Frame 3: warp 着地 (cursor が右端 anchor x400 へ。test では明示的に与える)。
        let f3 = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 100.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f3, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 4: 右端をさらに右 (cursor x600 = beat 3.0) へドラッグ (held)。
        let f4 = FrameInput {
            pointer: PointerFrame {
                pos: Some((600.0, 100.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f4, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 5: release → cursor 位置 (beat 3.0) を右端とする長さで Add。
        let f5 = FrameInput {
            pointer: PointerFrame {
                pos: Some((600.0, 100.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f5, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });
        assert_eq!(model.last_request, Some(RequestKind::Add), "release で Add 発行");
        let (start, len) = model.last_added.expect("Add の geometry");
        assert!((start - 1.0).abs() < 1e-9, "start_beat=1.0 (press x=200)、got {start}");
        assert!(
            (len - 2.0).abs() < 1e-9,
            "len=2.0 (右端が cursor beat 3.0、start 1.0)、got {len}"
        );
    }

    /// ノート作成 (ダブルクリック → ドラッグ → release) を完了したら、 press 時に
    /// 右端へ warp したカーソルを **元のクリック位置へ戻す**。 「いまはノートの右端のままになって
    /// いる」 という要望の修正。 warp の OS flush は `frame()` でのみ起きる (run_frame =
    /// frame_to_edits は副作用を発火しない) ので `set_cursor_pos_request` を capture し、
    /// press 位置 (200,100) への復帰が release frame で起きることを検証する。
    /// test_view: snap OFF / 4 拍 / grid 800px → 1 beat = 200px、 default = 1.0。
    /// press x=200 (beat 1.0) → warp 先 = 右端 beat 2.0 = x400。 release で (200,100) へ復帰。
    #[test]
    fn note_create_release_warps_cursor_back_to_press_position() {
        let warps: Arc<std::sync::Mutex<Vec<(f32, f32)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let warps_clone = Arc::clone(&warps);
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.set_cursor_pos_request = Some(Box::new(move |x, y| {
            warps_clone.lock().unwrap().push((x, y));
        }));
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        // warp flush を走らせるため frame_to_edits ではなく frame() を回す local helper。
        let run = |host: &mut UiHost<TestModel>, model: &mut TestModel, input: FrameInput| {
            let mut scene = Scene::new();
            let screen = PhysicalSize { width: 800, height: 400 };
            host.frame(model, &mut scene, screen, input, |_, ui| {
                let d = make_dispatch();
                let sel: Vec<NoteId> = vec![];
                let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
            });
        };

        // Frame 1: 1 度目 click (release) → last_click 記録。
        run(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((200.0, 100.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
        );
        // Frame 2: 2 度目 press → 作成 session 開始 + press 時 warp で右端 (400,100) へ。
        run(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((200.0, 100.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
        );
        assert_eq!(
            warps.lock().unwrap().as_slice(),
            &[(400.0, 100.0)],
            "press で既定長ノートの右端 (400,100) へ warp"
        );
        // Frame 3: warp 着地 (cursor が右端 anchor x400 へ)。
        run(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((400.0, 100.0)),
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
        );
        // Frame 4: 右へドラッグ (cursor x600 = beat 3.0、held)。
        run(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((600.0, 100.0)),
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
        );
        // Frame 5: release → Add 発行 + カーソルを元のクリック位置 (200,100) へ戻す。
        run(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((600.0, 100.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
        );
        assert_eq!(model.last_request, Some(RequestKind::Add), "release で Add 発行");
        assert_eq!(
            warps.lock().unwrap().as_slice(),
            &[(400.0, 100.0), (200.0, 100.0)],
            "press で右端へ warp → release で元のクリック位置 (200,100) へ復帰"
        );
    }

    /// warp 着地後、右へ振らずそのまま左へドラッグするだけで既定長より短いノートを作れる
    /// (= cursor＝右端なので右端を左へ動かす = 短縮、 一度右に振る必要がない)。
    /// press x=400 (beat 2.0 = start、default 1.0 → warp 先 = 右端 beat 3.0 = x600 = anchor)。
    /// warp 着地後 cursor を x500 (beat 2.5) へ左ドラッグ → 右端 beat 2.5 → len = 2.5−2.0 = 0.5
    /// (既定 1.0 より短い)。
    #[test]
    fn note_create_drag_left_shortens_without_prior_right_drag() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = test_view(); // default_note_len_beats = 1.0、snap OFF
        let style = PianoRollStyle::default();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

        // Frame 1: 1 度目 click。
        let f1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 100.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f1, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 2: 2 度目 press (start beat 2.0、anchor = warp 先 x600)。
        let f2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 100.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f2, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 3: warp 着地 (cursor が右端 anchor x600 へ)。
        let f3 = FrameInput {
            pointer: PointerFrame {
                pos: Some((600.0, 100.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f3, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 4: 右へ振らず **左へ** ドラッグ (cursor x500 = beat 2.5、held)。
        let f4 = FrameInput {
            pointer: PointerFrame {
                pos: Some((500.0, 100.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f4, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 5: release → 既定長 1.0 より短い 0.5 で Add (右端から滑らかに短縮)。
        let f5 = FrameInput {
            pointer: PointerFrame {
                pos: Some((500.0, 100.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f5, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });
        assert_eq!(model.last_request, Some(RequestKind::Add));
        let (start, len) = model.last_added.expect("Add の geometry");
        assert!((start - 2.0).abs() < 1e-9, "start_beat=2.0、got {start}");
        assert!(
            len < 1.0 && (len - 0.5).abs() < 1e-9,
            "左ドラッグだけで既定長 1.0 より短い 0.5 になる (min ではなく右端から相対短縮)、got {len}"
        );
    }

    /// ダブルクリックしてドラッグせず即座に放す → **既定長** (`view.default_note_len_beats`) で
    /// `Add`。test_view の default_note_len_beats = 1.0。start=1.0 (press x=200)。
    #[test]
    fn note_create_double_click_without_drag_uses_default_length() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

        // Frame 1: 1 度目 click (release)。
        let f1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f1, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 2: 2 度目 press。
        let f2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f2, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });

        // Frame 3: 動かさず release。
        let f3 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, f3, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });
        assert_eq!(model.last_request, Some(RequestKind::Add), "即放しでも Add 発行");
        let (start, len) = model.last_added.expect("Add の geometry");
        assert!((start - 1.0).abs() < 1e-9, "start_beat=1.0、got {start}");
        assert!(
            (len - 1.0).abs() < 1e-9,
            "ドラッグなしは既定長 default_note_len_beats=1.0、got {len}"
        );
    }

    /// 単発 click (ダブルクリックでない) は作成しない (= release-based 検出が press に化けない)。
    /// 1 度の press → release だけでは `note_create` session は始まらず Add も出ない。
    #[test]
    fn note_create_single_click_does_not_create() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

        // press → release を別フレームで (= 普通の単発クリック、直前 click 無し)。
        let press = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let creating = std::cell::Cell::new(false);
        run_frame(&mut host, &mut model, press, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
            creating.set(resp.creating);
        });
        assert!(!creating.get(), "単発 press では作成 session は始まらない");

        let release = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 100.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, release, |ui| {
            let d = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll("pr", rect, &[], view, &sel, &style, d);
        });
        assert_eq!(model.last_request, None, "単発クリックでは Add しない");
    }

    /// (M14 Phase 83 / daw_01 #054) Ctrl+drag は release で `Move` ではなく `Copy` を発行する。
    /// Ctrl なしの drag が `Move` を出すことは `piano_roll_no_edit_during_drag_only_at_release`
    /// が固定済 (= 修飾の有無だけで分岐することの回帰防止ペア)。
    #[test]
    fn piano_roll_ctrl_drag_emits_copy_on_release() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let view_owned = view;

        // Frame 1: Ctrl+press at note 中央 → drag (ctrl 保持)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((300.0, 200.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { ctrl: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view_owned,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, None, "drag 中は Edit 発行せず");

        // Frame 2: Ctrl 保持で release at 移動先 (100px 移動 = demote 閾値 4px 超)
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 200.0)),
                primary_just_released: true,
                modifiers: Modifiers { ctrl: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view_owned,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(
            model.last_request,
            Some(RequestKind::Copy),
            "Ctrl+drag release で Copy 発行 (Move ではない)"
        );
    }

    /// (M14 Phase 84 / daw_01 #055) 鍵盤レーン press でカーソル位置の pitch を held-value で返す。
    /// 押下中の上下 drag で別キーへ追従 (glissando)、release で None (note-off)。
    #[test]
    fn piano_roll_keyboard_press_returns_active_pitch_with_glissando() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let mut view = test_view();
        view.keyboard_w = 60.0; // 鍵盤レーン有効 (kbd rect x∈[0,60))
        let style = PianoRollStyle::default();
        let view_owned = view;
        let no_notes: Vec<Note> = vec![];
        let rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        // pitch 計算: pitch_top=72 / pitch_visible=24 / main_h=400 → row_h=16.67。
        //   py=208 → y_to_pitch_f=59.52 → ceil=60、py=175 → 61.5 → ceil=62。

        // Frame 1: 鍵盤レーン (px=30 < 60) を press、py=208 → pitch 60
        let p1 = std::cell::Cell::new(None);
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((30.0, 208.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll("pr", rect, &no_notes, view_owned, &sel, &style, dispatch);
            p1.set(resp.keyboard_active_pitch);
        });
        assert_eq!(p1.get(), Some(60), "鍵盤 press でカーソル位置の pitch");

        // Frame 2: 押下したまま上のキーへ drag、py=175 → pitch 62 (glissando 追従)
        let p2 = std::cell::Cell::new(None);
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((30.0, 175.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll("pr", rect, &no_notes, view_owned, &sel, &style, dispatch);
            p2.set(resp.keyboard_active_pitch);
        });
        assert_eq!(p2.get(), Some(62), "押下中 drag で別キーへ追従 (glissando)");

        // Frame 3: release → None (note-off)
        let p3 = std::cell::Cell::new(Some(0));
        let input3 = FrameInput {
            pointer: PointerFrame {
                pos: Some((30.0, 175.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input3, |ui| {
            let dispatch = make_dispatch();
            let sel: Vec<NoteId> = vec![];
            let resp = ui.piano_roll("pr", rect, &no_notes, view_owned, &sel, &style, dispatch);
            p3.set(resp.keyboard_active_pitch);
        });
        assert_eq!(p3.get(), None, "release で None (note-off)");
    }

    /// Modifiers が default (= 修飾なし) で、modifier の Default 実装がエラーで失敗しないことの確認。
    #[test]
    fn modifiers_default_is_no_alt() {
        let m = Modifiers::default();
        assert!(!m.alt);
        assert!(!m.shift);
    }

    /// Shift+drag rect select が **加算**: 既選択 [3] + rect 内 {1, 2} → next = [1, 2, 3] (sorted)。
    /// daw_01 旧自前実装の慣習に合致。
    #[test]
    fn piano_roll_shift_drag_is_additive() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        // note 1: x ∈ [100, 200], y ≈ [116.67, 133.33]
        // note 2: x ∈ [200, 300], y ≈ [133.33, 150.00]
        // note 3: x ∈ [600, 700], y ≈ [200.00, 216.67] (rect 外、prev に保持)
        let mut model = TestModel::new(vec![
            note(1, 0.5, 0.5, 65),
            note(2, 1.0, 0.5, 64),
            note(3, 3.0, 0.5, 60),
        ]);
        model.selected = vec![3];
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();

        // Frame 1: Shift+press at (50, 50) — drag 開始 (空白なので note hit せず、shift 経路へ)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { shift: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });
        // drag 中はまだ Edit 発行せず
        assert_eq!(model.last_request, None, "drag 中は Select 発行せず");

        // Frame 2: release at (350, 200) — finished で rect (50,50)-(350,200) が確定
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((350.0, 200.0)),
                primary_just_released: true,
                modifiers: Modifiers { shift: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::Select), "release で Select 発行");
        assert_eq!(
            model.last_select_next,
            Some(vec![1, 2, 3]),
            "加算: prev [3] + rect 内 {{1, 2}} = sorted [1, 2, 3]"
        );
        assert_eq!(model.last_select_prev, Some(vec![3]));
    }

    /// #102: 空き grid の **無修飾** drag = marquee REPLACE。 prev [3] を捨てて rect 内 {1,2} に置換。
    #[test]
    fn piano_roll_plain_drag_empty_is_replace() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![
            note(1, 0.5, 0.5, 65),
            note(2, 1.0, 0.5, 64),
            note(3, 3.0, 0.5, 60),
        ]);
        model.selected = vec![3];
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

        // Frame 1: plain press at (50,50) (空白) — marquee 開始。
        let sel1 = model.selected.clone();
        run_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((50.0, 50.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |ui| {
                let _ = ui.piano_roll("pr", area, &notes_clone, view, &sel1, &style, make_dispatch());
            },
        );
        assert_eq!(model.last_request, None, "drag 中は Select 発行せず");

        // Frame 2: plain release at (350,200) — rect (50,50)-(350,200) 内 {1,2} で REPLACE。
        let sel2 = model.selected.clone();
        run_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((350.0, 200.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |ui| {
                let _ = ui.piano_roll("pr", area, &notes_clone, view, &sel2, &style, make_dispatch());
            },
        );
        assert_eq!(model.last_request, Some(RequestKind::Select));
        assert_eq!(model.last_select_next, Some(vec![1, 2]), "REPLACE: prev [3] 破棄、 rect 内 [1,2]");
        assert_eq!(model.last_select_prev, Some(vec![3]));
    }

    /// #102: 空き grid の **Ctrl** drag = marquee XOR (toggle)。 prev [1,3] と rect 内 {1,2} の
    /// 対称差 = {3,2} → sorted [2,3] (1 は両方に在り除外、 2 は追加、 3 は保持)。
    #[test]
    fn piano_roll_ctrl_drag_empty_is_xor() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![
            note(1, 0.5, 0.5, 65),
            note(2, 1.0, 0.5, 64),
            note(3, 3.0, 0.5, 60),
        ]);
        model.selected = vec![1, 3];
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };

        let sel1 = model.selected.clone();
        run_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((50.0, 50.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    modifiers: ctrl,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |ui| {
                let _ = ui.piano_roll("pr", area, &notes_clone, view, &sel1, &style, make_dispatch());
            },
        );
        let sel2 = model.selected.clone();
        run_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((350.0, 200.0)),
                    primary_just_released: true,
                    modifiers: ctrl,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |ui| {
                let _ = ui.piano_roll("pr", area, &notes_clone, view, &sel2, &style, make_dispatch());
            },
        );
        assert_eq!(model.last_select_next, Some(vec![2, 3]), "XOR: [1,3] ^ {{1,2}} = [2,3]");
    }

    /// #102: note の上の **無修飾** drag は marquee ではなく MOVE (Select 発行しない)。
    #[test]
    fn piano_roll_plain_drag_on_note_is_move_not_select() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 0.5, 0.5, 65)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

        // press at (150,125) = note 1 中央 (x[100,200] y[116.67,133.33]) → note drag (Move)。
        let sel1 = model.selected.clone();
        run_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((150.0, 125.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |ui| {
                let _ = ui.piano_roll("pr", area, &notes_clone, view, &sel1, &style, make_dispatch());
            },
        );
        // release at (260,125) = +110px (>4px、 demote されず Move commit)。
        let sel2 = model.selected.clone();
        run_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((260.0, 125.0)),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            |ui| {
                let _ = ui.piano_roll("pr", area, &notes_clone, view, &sel2, &style, make_dispatch());
            },
        );
        assert_eq!(model.last_request, Some(RequestKind::Move), "note 上 plain drag は Move、 marquee 不発");
    }

    /// #102: 空き grid の sub-4px 無修飾 press+release (同フレーム) は marquee zero-rect REPLACE で
    /// **ちょうど 1 回** `Select{next:[]}` を emit する (pending_click との二重 emit ガード固定)。
    #[test]
    fn piano_roll_subpx_empty_press_emits_single_clear() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 0.5, 0.5, 65)]);
        model.selected = vec![1];
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        let area = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let sel = model.selected.clone();

        // press+release 同フレーム、 空白 (50,50)、 無修飾。
        let edits = {
            let mut scene = Scene::new();
            let screen = PhysicalSize { width: 800, height: 400 };
            host.frame_to_edits(
                &model,
                &mut scene,
                screen,
                FrameInput {
                    pointer: PointerFrame {
                        pos: Some((50.0, 50.0)),
                        primary_just_pressed: true,
                        primary_just_released: true,
                        ..PointerFrame::default()
                    },
                    ..Default::default()
                },
                |_, ui| {
                    let _ = ui.piano_roll("pr", area, &notes_clone, view, &sel, &style, make_dispatch());
                },
            )
        };
        assert_eq!(edits.len(), 1, "空き sub-px press は ちょうど 1 個の Edit (= marquee REPLACE clear)");
        for e in edits {
            e.apply(&mut model);
        }
        assert_eq!(model.last_select_next, Some(vec![]), "Select{{next:[]}} で clear");
    }

    /// 旧仕様 (Alt+drag rect select) は廃止された: Alt+drag で press → release しても
    /// Select request は発行されない。
    #[test]
    fn piano_roll_alt_drag_no_longer_selects() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(1, 0.5, 0.5, 65), note(2, 1.0, 0.5, 64)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();

        // Frame 1: Alt+press at (50, 50)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { alt: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });

        // Frame 2: Alt+release at (350, 200) — 旧仕様なら Select 発行、新仕様では発行されない
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((350.0, 200.0)),
                primary_just_released: true,
                modifiers: Modifiers { alt: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, None, "Alt+drag は rect select を起動しない");
    }

    // ============================================================
    // M9 Phase 45c: velocity lane + playhead 内蔵描画のテスト
    // ============================================================

    /// 同じ widget 呼び出しで velocity_lane_h を変えたときの scene.rects 差分を測る helper。
    /// 各テストは別々の `host` で cache 状態を分離する (heavy() の cache key が再構築される)。
    fn count_rects_with_view(view: PianoRollView, notes: &[Note]) -> (usize, usize) {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let model = TestModel::new(notes.to_vec());
        let style = PianoRollStyle::default();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &model.notes,
                view,
                &[],
                &style,
                dispatch,
            );
        });
        (scene.rect_count(), scene.line_count())
    }

    #[test]
    fn velocity_lane_disabled_by_default() {
        let v_off = test_view(); // velocity_lane_h: 0.0
        let mut v_on = test_view();
        v_on.velocity_lane_h = 60.0;

        let notes = vec![note(1, 0.5, 0.5, 60), note(2, 1.5, 0.5, 64)];
        let (rects_off, _) = count_rects_with_view(v_off, &notes);
        let (rects_on, _) = count_rects_with_view(v_on, &notes);

        // velocity_lane_h > 0 で bg 1 + bar 2 = 3 個多い rect が積まれる。
        assert_eq!(rects_on, rects_off + 3, "velocity lane bg + 2 bars が追加される");
    }

    #[test]
    fn velocity_lane_skips_zero_velocity_bars() {
        // velocity 0 のみの notes で vel_h: 0 vs 60 の差分を測る。
        // 期待: bg 1 個のみ追加 (bar は skip)、合計差分 = 1。
        let v_off = test_view();
        let mut v_on = test_view();
        v_on.velocity_lane_h = 60.0;
        let n_zero = vec![Note {
            id: 1,
            start_beat: 0.5,
            len_beats: 0.5,
            pitch: 60,
            velocity: 0,
            lyric: None,
            muted: false,
            style: NoteStyle::default(),
        }];
        let (rects_off, _) = count_rects_with_view(v_off, &n_zero);
        let (rects_on, _) = count_rects_with_view(v_on, &n_zero);

        assert_eq!(rects_on, rects_off + 1, "velocity 0 のみなら bar は出ず bg のみ追加");
    }

    #[test]
    fn playhead_renders_line_batch_when_in_range() {
        let v_off = test_view(); // playhead_beat: None
        let mut v_on = test_view();
        v_on.playhead_beat = Some(2.0); // len_beats=4.0、範囲内

        let notes = vec![note(1, 0.5, 0.5, 60)];
        let (_, lines_off) = count_rects_with_view(v_off, &notes);
        let (_, lines_on) = count_rects_with_view(v_on, &notes);

        assert_eq!(lines_on, lines_off + 1, "playhead 1 LineBatch が追加される");
    }

    #[test]
    fn playhead_skipped_when_out_of_range() {
        let v_off = test_view();
        let mut v_out = test_view();
        v_out.playhead_beat = Some(100.0); // start=0, len=4 の範囲外

        let notes = vec![note(1, 0.5, 0.5, 60)];
        let (_, lines_off) = count_rects_with_view(v_off, &notes);
        let (_, lines_out) = count_rects_with_view(v_out, &notes);

        assert_eq!(lines_off, lines_out, "範囲外 playhead は描画されない");
    }

    #[test]
    fn playhead_and_velocity_lane_combine() {
        let v_off = test_view();
        let mut v_both = test_view();
        v_both.velocity_lane_h = 60.0;
        v_both.playhead_beat = Some(2.0);

        let notes = vec![note(1, 0.5, 0.5, 60), note(2, 1.5, 0.5, 64)];
        let (rects_off, lines_off) = count_rects_with_view(v_off, &notes);
        let (rects_both, lines_both) = count_rects_with_view(v_both, &notes);

        // velocity lane: bg 1 + 2 bars = 3 個 rect 増 / playhead: 1 LineBatch 増
        assert_eq!(rects_both, rects_off + 3);
        assert_eq!(lines_both, lines_off + 1);
    }

    // ============================================================
    // M14 Phase 64 / daw_01 #018: velocity lane drag 編集のテスト
    // ============================================================
    //
    // test_view + Rect (0,0)-(800,400) + velocity_lane_h: 60.0 の geometry:
    //   ruler_h=0, kbd_w=0, vel_h=60, main_h=340
    //   vel_area = Rect { x: 0, y: 340, w: 800, h: 60 }
    // velocity_from_y:
    //   y=340 (lane top)    → 127
    //   y=400 (lane bottom) → 0
    //   y=370 (lane center) → 64
    // bar x for note start_beat=B (view.start_beat=0, len_beats=4):
    //   bx = 0 + (B - 0) * (800 / 4) = B * 200

    /// `velocity_from_y` の境界 + clamp。
    #[test]
    fn velocity_from_y_at_lane_top_returns_127() {
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        assert_eq!(velocity_from_y(340.0, area), 127);
    }

    #[test]
    fn velocity_from_y_at_lane_bottom_returns_0() {
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        assert_eq!(velocity_from_y(400.0, area), 0);
    }

    #[test]
    fn velocity_from_y_clamps_above_lane_to_127() {
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        assert_eq!(velocity_from_y(100.0, area), 127, "lane の上を超えても 127 で clamp");
    }

    #[test]
    fn velocity_from_y_clamps_below_lane_to_0() {
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        assert_eq!(velocity_from_y(500.0, area), 0, "lane の下を超えても 0 で clamp");
    }

    #[test]
    fn velocity_from_y_zero_height_is_defensive_zero() {
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 0.0 };
        assert_eq!(velocity_from_y(340.0, area), 0);
    }

    /// `velocity_bar_hit` の hit / miss / tolerance。
    #[test]
    fn velocity_bar_hit_finds_note_at_start_beat() {
        let view = test_view(); // start_beat=0, len_beats=4 → bar x = beat * 200
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        let notes = vec![note(7, 1.0, 0.5, 60)]; // bar x = 200
        // 中央は hit
        assert_eq!(velocity_bar_hit(&notes, view, area, 200.0, 3.0, 4.0), Some(7));
        // 中央 ±5.5px (= bar_width/2 + tolerance) は hit
        assert_eq!(velocity_bar_hit(&notes, view, area, 195.0, 3.0, 4.0), Some(7));
        assert_eq!(velocity_bar_hit(&notes, view, area, 205.0, 3.0, 4.0), Some(7));
    }

    #[test]
    fn velocity_bar_hit_misses_outside_tolerance() {
        let view = test_view();
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        let notes = vec![note(7, 1.0, 0.5, 60)]; // bar x = 200
        // hit zone は ±5.5px。 7 px 離れていれば miss。
        assert_eq!(velocity_bar_hit(&notes, view, area, 207.0, 3.0, 4.0), None);
        assert_eq!(velocity_bar_hit(&notes, view, area, 193.0, 3.0, 4.0), None);
    }

    #[test]
    fn velocity_bar_hit_overlapping_returns_last() {
        // 2 つの note が同 start_beat にあるとき、 後勝ち (visible 順 = note_hit と同 semantics)。
        let view = test_view();
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        let notes = vec![note(1, 1.0, 0.5, 60), note(2, 1.0, 0.5, 67)];
        assert_eq!(velocity_bar_hit(&notes, view, area, 200.0, 3.0, 4.0), Some(2));
    }

    /// vel_area での press → drag → release で `SetVelocity` 発行。
    #[test]
    fn velocity_drag_emits_set_velocity_on_release() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(7, 1.0, 0.5, 60)]); // velocity 96
        let mut view = test_view();
        view.velocity_lane_h = 60.0;
        let style = PianoRollStyle::default();

        // Frame 1: press at bar x=200 (note 7 の start_beat=1.0), y=370 (lane center, vel ~64)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 370.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, None, "drag 中は SetVelocity 発行せず");

        // Frame 2: release at y=350 (vel ~106, drag dist = 20px > 3px → commit)
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 350.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        let notes_clone2 = model.notes.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone2,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        assert_eq!(
            model.last_request,
            Some(RequestKind::SetVelocity),
            "release で SetVelocity 発行"
        );
        let updates = model.last_set_velocity.as_ref().expect("SetVelocity payload");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, 7, "note id = 7");
        // velocity_from_y(350.0, vel_area) = (1 - (350-340)/60) * 127 = (1 - 0.1667) * 127 = 105.83 → 106
        assert_eq!(updates[0].1, 106, "y=350 で絶対 velocity ~106");
        assert_eq!(model.notes[0].velocity, 106, "Model 反映");
    }

    /// vel_area で drag<3px (= 単発 click 相当) → SetVelocity は発行されない (誤操作防止)。
    #[test]
    fn velocity_drag_no_op_for_short_drag() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(7, 1.0, 0.5, 60)]);
        let mut view = test_view();
        view.velocity_lane_h = 60.0;
        let style = PianoRollStyle::default();

        // press と release を 1 frame で同時 = drag 0px = 短 click
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 370.0)),
                primary_just_pressed: true,
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, None, "drag<3px は SetVelocity 発行せず");
        assert!(
            model.last_set_velocity.is_none(),
            "SetVelocity payload も発行されない"
        );
        assert_eq!(model.notes[0].velocity, 96, "velocity 不変 (anchor のまま)");
    }

    /// drag 起点 note が selected に含まれるとき、 全 selected が同じ velocity に。
    #[test]
    fn velocity_drag_targets_all_selected_when_hit_in_selection() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model =
            TestModel::new(vec![note(1, 0.5, 0.5, 60), note(2, 1.0, 0.5, 60), note(3, 1.5, 0.5, 60)]);
        model.selected = vec![1, 2, 3];
        let mut view = test_view();
        view.velocity_lane_h = 60.0;
        let style = PianoRollStyle::default();

        // press at note 2 の bar (x = 200)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 370.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });

        // release at y=340 (= velocity 127)
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 340.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        let notes_clone2 = model.notes.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone2,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        let updates = model.last_set_velocity.as_ref().expect("SetVelocity payload");
        // 3 つ全て velocity 127 に
        let mut ids: Vec<NoteId> = updates.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(updates.iter().all(|(_, v)| *v == 127));
    }

    /// drag 起点 note が selected に含まれないとき、 単一 hit のみ更新 (selection 不変)。
    #[test]
    fn velocity_drag_targets_only_hit_when_not_in_selection() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model =
            TestModel::new(vec![note(1, 0.5, 0.5, 60), note(2, 1.0, 0.5, 60), note(3, 1.5, 0.5, 60)]);
        model.selected = vec![1, 3]; // note 2 は含まれない
        let mut view = test_view();
        view.velocity_lane_h = 60.0;
        let style = PianoRollStyle::default();

        // press at note 2 の bar (x = 200)
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 370.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });

        // release at y=340 (= velocity 127)
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 340.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        let notes_clone2 = model.notes.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone2,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        let updates = model.last_set_velocity.as_ref().expect("SetVelocity payload");
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, 2, "primary hit (id 2) のみ更新");
        assert_eq!(updates[0].1, 127);
    }

    /// vel_h = 0 なら velocity drag は起動しない (lane 自体無効)。
    #[test]
    fn velocity_drag_skips_when_lane_disabled() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(7, 1.0, 0.5, 60)]);
        let view = test_view(); // velocity_lane_h: 0.0
        let style = PianoRollStyle::default();

        // press + release at vel_area 相当の y (vel_h=0 だと vel_area が無いので grid 内扱い)。
        // x=200, y=370 だと grid 内 (grid h = 400)、 note 7 は pitch 60 (y ~ 200) なので空白 click。
        // SetVelocity は発行されないことを確認 (note の velocity は note() helper の default 96 のまま)。
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 370.0)),
                primary_just_pressed: true,
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert!(
            model.last_set_velocity.is_none(),
            "vel_h=0 では velocity drag は起動しない"
        );
        assert_eq!(model.notes[0].velocity, 96, "velocity 不変 (note() helper の default)");
    }

    /// vel_area で bar が無い x position に press → velocity_drag は起動せず selection も変わらない。
    #[test]
    fn velocity_drag_misses_empty_lane_area_no_selection_change() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(7, 1.0, 0.5, 60)]);
        model.selected = vec![7]; // 既存 selection あり
        let mut view = test_view();
        view.velocity_lane_h = 60.0;
        let style = PianoRollStyle::default();

        // x=400 は note 7 (bar x=200) から 200 px 離れていて hit zone 外 → bar 無し空白 click
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 370.0)),
                primary_just_pressed: true,
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert!(
            model.last_set_velocity.is_none(),
            "bar 無しの空白 click では SetVelocity 発行されない"
        );
        // selection も維持される (vel_area click は pending_click 経由の selection clear に流れない)。
        assert_eq!(model.selected, vec![7], "selection 不変");
    }

    /// velocity drag 中は `PianoRollResponse::velocity_dragging == true`。
    #[test]
    fn velocity_drag_response_dragging_flag() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![note(7, 1.0, 0.5, 60)]);
        let mut view = test_view();
        view.velocity_lane_h = 60.0;
        let style = PianoRollStyle::default();

        // Frame 1: press → velocity_drag 開始
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 370.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });

        // Frame 2: continuation (drag 中) → response.velocity_dragging = true
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, 350.0)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        let notes_clone2 = model.notes.clone();
        let dragging_flag = std::cell::Cell::new(false);
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone2,
                view,
                &sel2,
                &style,
                dispatch,
            );
            dragging_flag.set(resp.velocity_dragging);
        });
        assert!(dragging_flag.get(), "drag 中は velocity_dragging = true");
    }

    /// SetVelocity payload は anchor velocity と同じ note を除外する (no-op Edit avoid)。
    #[test]
    fn velocity_drag_excludes_unchanged_velocities() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        // note 1: velocity 64, note 2: velocity 127
        let mut model = TestModel::new(vec![
            Note { id: 1, start_beat: 0.5, len_beats: 0.5, pitch: 60, velocity: 64, lyric: None, muted: false, style: NoteStyle::default() },
            Note { id: 2, start_beat: 1.0, len_beats: 0.5, pitch: 60, velocity: 127, lyric: None, muted: false, style: NoteStyle::default() },
        ]);
        model.selected = vec![1, 2];
        let mut view = test_view();
        view.velocity_lane_h = 60.0;
        let style = PianoRollStyle::default();

        // press at bar 1 (x=100, beat=0.5) → multi-select 全部 target
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((100.0, 370.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel1 = model.selected.clone();
        let notes_clone = model.notes.clone();
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel1,
                &style,
                dispatch,
            );
        });

        // release at y=340 (= velocity 127) → note 2 は anchor 127 と一致するので除外、note 1 のみ更新
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((100.0, 340.0)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel2 = model.selected.clone();
        let notes_clone2 = model.notes.clone();
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone2,
                view,
                &sel2,
                &style,
                dispatch,
            );
        });
        let updates = model.last_set_velocity.as_ref().expect("SetVelocity payload");
        assert_eq!(updates.len(), 1, "note 2 は anchor 127 と new 127 一致で除外");
        assert_eq!(updates[0], (1, 127));
    }

    // -------- M13 Phase 55: ruler / time_sig 対応 grid の確認 --------

    /// 1 frame 描画して `Scene` を返す helper。
    fn render_piano_roll_once(view: PianoRollView, notes: &[Note]) -> daw_ui_renderer::Scene {
        use daw_ui_platform::PhysicalSize;
        use daw_ui_renderer::Scene;

        use crate::input::FrameInput;
        use crate::ui::UiHost;

        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let style = PianoRollStyle::default();
            let _ = ui.piano_roll(
                "pr_test",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                notes,
                view,
                &[],
                &style,
                |_| Edit::mutate(|()| {}),
            );
        });
        scene
    }

    /// `ruler_h: 0.0` で bar 番号 label が出ない (旧 piano_roll 互換)。
    #[test]
    fn ruler_h_zero_disables_bar_labels() {
        let mut view = test_view();
        view.ruler_h = 0.0;
        view.bpm = 120.0;
        view.time_sig = (4, 4);
        let scene = render_piano_roll_once(view, &[]);
        let labels: Vec<String> =
            scene.iter_glyphs().map(|g| g.text.as_ref().to_string()).collect();
        for blocked in ["1", "2", "3"] {
            assert!(
                !labels.iter().any(|s| s == blocked),
                "ruler_h=0.0 で bar label {blocked:?} は出ない: labels={labels:?}",
            );
        }
    }

    /// `ruler_h: 20.0` で bar label "1", "2" が出る (small bars only)。
    #[test]
    fn ruler_h_positive_emits_bar_labels() {
        let mut view = test_view();
        view.start_beat = 0.0;
        view.len_beats = 8.0; // 2 小節分
        view.ruler_h = 20.0;
        view.bpm = 120.0;
        view.time_sig = (4, 4);
        let scene = render_piano_roll_once(view, &[]);
        let labels: Vec<String> =
            scene.iter_glyphs().map(|g| g.text.as_ref().to_string()).collect();
        for expected in ["1", "2"] {
            assert!(
                labels.iter().any(|s| s == expected),
                "ruler_h=20.0 で bar label {expected:?} が出る: labels={expected:?}, found={labels:?}",
            );
        }
    }

    /// `ruler_h: 20.0` のとき grid 内 bar 線の y が `rect.y + 20.0` 以降から始まる
    /// (= grid 領域が ruler 分シフトしている)。
    /// ruler 内の tick 線 (短い) は除外し、grid 全幅の bar 線 (長い) のみ判定する。
    #[test]
    fn grid_y_offset_with_ruler() {
        let style = PianoRollStyle::default();
        let mut view = test_view();
        view.start_beat = 0.0;
        view.len_beats = 8.0;
        view.ruler_h = 20.0;
        view.bpm = 120.0;
        view.time_sig = (4, 4);
        let scene = render_piano_roll_once(view, &[]);

        // bar_color で seg が long (高さ > ruler_h) のものを grid 内 bar 線として抽出。
        let grid_bar_y_tops: Vec<f32> = scene
            .iter_lines()
            .flat_map(|b| b.segments.iter().copied())
            .filter(|seg| seg.color == style.bar_line)
            .filter(|seg| (seg.b[1] - seg.a[1]).abs() > 20.0)
            .map(|seg| seg.a[1].min(seg.b[1]))
            .collect();
        assert!(
            !grid_bar_y_tops.is_empty(),
            "grid 内 bar 線が出ている (sanity)",
        );
        for y in &grid_bar_y_tops {
            assert!(
                *y >= 20.0 - 0.1,
                "ruler_h=20 で grid bar 線の y_top は >=20.0: actual={y}",
            );
        }
    }

    // -------- Stale cursor reset tests --------
    // piano_roll widget が pointer 位置に応じて cursor を明示的に reset するか検証。
    // winit は state-full なので、note 外で set_cursor を呼ばないと前フレームの形状が残る
    // (ui.rs:999 で明記)。修正後は pointer が widget rect 内かつ note hover 圏外で
    // CursorIcon::Default を明示 set する。

    fn run_frame_with_cursor_capture<F: FnOnce(&mut Ui<'_, TestModel>)>(
        captured: Arc<std::sync::Mutex<Vec<CursorIcon>>>,
        model: &mut TestModel,
        input: FrameInput,
        f: F,
    ) {
        let captured_clone = Arc::clone(&captured);
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.set_cursor_request = Some(Box::new(move |c| {
            captured_clone.lock().unwrap().push(c);
        }));
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        host.frame(model, &mut scene, screen, input, |_, ui| {
            f(ui);
        });
    }

    #[test]
    fn piano_roll_resets_cursor_to_default_when_pointer_in_widget_but_not_on_note() {
        let captured: Arc<std::sync::Mutex<Vec<CursorIcon>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        // grid 内だが note 外 (note rect は x∈[200,400] y≈[200,215]、(50,50) は note 外)
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((50.0, 50.0)),
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        run_frame_with_cursor_capture(Arc::clone(&captured), &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(captured.lock().unwrap().as_slice(), &[CursorIcon::Default]);
    }

    #[test]
    fn piano_roll_does_not_set_cursor_when_pointer_outside_widget() {
        let captured: Arc<std::sync::Mutex<Vec<CursorIcon>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        let view = test_view();
        let style = PianoRollStyle::default();
        let notes_clone = model.notes.clone();
        // pointer が widget rect (0,0)-(800,400) の外 → 何もしない (他 widget の責務)
        // screen は (1000,600) で widget 外も合法
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((900.0, 500.0)),
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel = model.selected.clone();
        let captured_clone = Arc::clone(&captured);
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.set_cursor_request = Some(Box::new(move |c| {
            captured_clone.lock().unwrap().push(c);
        }));
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 1000, height: 600 };
        host.frame(&mut model, &mut scene, screen, input, |_, ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert!(
            captured.lock().unwrap().is_empty(),
            "widget 外では set_cursor を呼ばない: actual={:?}",
            *captured.lock().unwrap()
        );
    }

    // ============================================================
    // M14 Phase 59 / daw_01 #017: 歌詞 inline 編集 widget integration tests
    // ============================================================

    fn key_l() -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Char('L'),
        }
    }

    /// 文字 `c` (ASCII alphabet) と挿入 text を持つ KeyEvent を作る。
    /// `text` は IME 経由でも user 直接 type でも text_input.rs が `ev.text` を見るので
    /// 多バイト文字 (例: "あ") も渡せる。
    fn key_typing(c: char, text: &str) -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: Some(text.to_string()),
            physical_key: PhysicalKey::Char(c.to_ascii_uppercase()),
        }
    }

    fn key_enter() -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Enter,
        }
    }

    fn key_esc() -> KeyEvent {
        KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape,
        }
    }

    fn setup_lyric_test_host() -> UiHost<TestModel> {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        host.shortcut_map_mut().bind("piano_roll.edit_lyric", "L");
        host
    }

    /// 共通の piano_roll 呼び出し (rect 800x400 / kbd_w=0)。
    fn run_lyric_frame(
        host: &mut UiHost<TestModel>,
        model: &mut TestModel,
        input: FrameInput,
        view: PianoRollView,
        style: &PianoRollStyle,
        on_resp: impl FnOnce(&PianoRollResponse),
    ) {
        let sel = model.selected.clone();
        let notes_clone = model.notes.clone();
        let resp_cell: std::cell::RefCell<Option<PianoRollResponse>> =
            std::cell::RefCell::new(None);
        run_frame(host, model, input, |ui| {
            let dispatch = make_dispatch();
            let resp = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &notes_clone,
                view,
                &sel,
                style,
                dispatch,
            );
            *resp_cell.borrow_mut() = Some(resp);
        });
        on_resp(resp_cell.borrow().as_ref().expect("resp captured"));
    }

    /// L キー + selected.len() == 1 → lyric_editing = Some(selected[0])、Edit 発行なし。
    #[test]
    fn lyric_edit_l_key_enters_mode_when_single_selected() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = None;
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, Some(0));
        assert_eq!(model.last_request, None, "L で mode 入っただけ、Edit 発行なし");
    }

    #[test]
    fn lyric_edit_l_key_noop_when_zero_selected() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![]; // 選択なし
        let view = test_view();
        let style = PianoRollStyle::default();

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, None);
    }

    #[test]
    fn lyric_edit_l_key_noop_when_multi_selected() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(0, 1.0, 1.0, 60),
            note(1, 2.0, 1.0, 60),
        ]);
        model.selected = vec![0, 1]; // 複数選択
        let view = test_view();
        let style = PianoRollStyle::default();

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, None);
    }

    #[test]
    fn lyric_edit_disabled_via_style_none() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle {
            lyric_edit_shortcut: None, // 無効化
            ..PianoRollStyle::default()
        };

        let input = FrameInput { keyboard: vec![key_l()], ..Default::default() };
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(&mut host, &mut model, input, view, &style, |resp| {
            got_lyric_editing = resp.lyric_editing;
        });
        assert_eq!(got_lyric_editing, None, "style.lyric_edit_shortcut=None なら L で起動しない");
    }

    /// 1 note のみ → "a" + Enter → SetLyrics 発行 + lyric_editing = None。
    /// Frame 1: L → enter mode
    /// Frame 2: type "a" + Enter → commit + clear (no next note)
    #[test]
    fn lyric_edit_enter_commits_single_note_and_clears() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: 'a' + Enter
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(model.last_request, Some(RequestKind::SetLyrics));
        assert_eq!(model.last_set_lyrics, Some(vec![(0u32, Some("a".to_string()))]));
        assert_eq!(got_lyric_editing, None, "次 note 無し → mode 解除");
    }

    /// 4 notes 同 pitch → "abcd" + Enter → 各 note に 1 char ずつ分配 (start_beat 順)。
    #[test]
    fn lyric_edit_enter_distributes_morae_to_next_notes() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
            note(30, 1.0, 0.5, 60),
            note(40, 1.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: "abcd" + Enter (4 chars + commit)
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![
                    key_typing('a', "a"),
                    key_typing('b', "b"),
                    key_typing('c', "c"),
                    key_typing('d', "d"),
                    key_enter(),
                ],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![
                (10u32, Some("a".to_string())),
                (20u32, Some("b".to_string())),
                (30u32, Some("c".to_string())),
                (40u32, Some("d".to_string())),
            ])
        );
    }

    /// 4 notes、入力 2 mora → 2 note に分配 + lyric_editing = Some(notes[2].id) (= 次へ移動)。
    #[test]
    fn lyric_edit_enter_advances_to_next_when_more_notes_remain() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
            note(30, 1.0, 0.5, 60),
            note(40, 1.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        let mut got_lyric_editing = None;
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_typing('b', "b"), key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(10u32, Some("a".to_string())), (20u32, Some("b".to_string()))])
        );
        assert_eq!(got_lyric_editing, Some(30), "次 note (id 30) へ移動");
        // selection も追従していること (同じ frame で Select 発行された)
        assert_eq!(model.selected, vec![30u32]);
    }

    /// 拗音結合: "しゅんかん" → 4 mora ([しゅ] [ん] [か] [ん])。
    #[test]
    fn lyric_edit_enter_combines_kana_correctly() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
            note(30, 1.0, 0.5, 60),
            note(40, 1.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // "しゅんかん" を IME 経由ではなく KeyEvent.text 直接挿入で simulate
        // (実際の IME 入力は manual test: cargo run --bin piano_roll で確認)
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![
                    key_typing('a', "し"),
                    key_typing('b', "ゅ"),
                    key_typing('c', "ん"),
                    key_typing('d', "か"),
                    key_typing('e', "ん"),
                    key_enter(),
                ],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![
                (10u32, Some("しゅ".to_string())),
                (20u32, Some("ん".to_string())),
                (30u32, Some("か".to_string())),
                (40u32, Some("ん".to_string())),
            ])
        );
    }

    /// 2 notes に 3 mora 入力 → SetLyrics 2 件 + lyric_overflow_morae == 1。
    #[test]
    fn lyric_edit_overflow_morae_count_in_response() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(10, 0.0, 0.5, 60),
            note(20, 0.5, 0.5, 60),
        ]);
        model.selected = vec![10];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        let mut overflow = 0;
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![
                    key_typing('a', "a"),
                    key_typing('b', "b"),
                    key_typing('c', "c"),
                    key_enter(),
                ],
                ..Default::default()
            },
            view,
            &style,
            |resp| overflow = resp.lyric_overflow_morae,
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(10u32, Some("a".to_string())), (20u32, Some("b".to_string()))])
        );
        assert_eq!(overflow, 1, "余りモーラ 1 個分 (= 'c') が捨てられた");
    }

    /// Frame 1: L → mode 入る。Frame 2: "a" + Esc → SetLyrics 発行されず + lyric_editing = None。
    #[test]
    fn lyric_edit_esc_cancels_without_setlyrics() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        let mut got_lyric_editing = Some(0);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_esc()],
                ..Default::default()
            },
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(model.last_request, None, "Esc で cancel → SetLyrics 発行なし");
        assert_eq!(got_lyric_editing, None, "Esc 1 frame で完全 cancel");
    }

    /// 編集中 (frame 2) の primary press on note → drag が始まらない。
    #[test]
    fn lyric_edit_short_circuits_drag() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L → enter mode
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: primary press on note (lyric_editing = Some(0))
        let mut dragging = Some(NoteDragKind::Move);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some((300.0, 200.0)),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..Default::default()
            },
            view,
            &style,
            |resp| dragging = resp.dragging,
        );
        assert_eq!(dragging, None, "編集中は drag 開始しない");
    }

    /// 既存 lyric "x" を持つ note → 全選択 (auto on focus) + Backspace 相当の入力なしで Enter
    /// → 「変更なしで Enter」となる。 ※ 「全選択を replace せずに Enter」と
    /// 「全選択 → Backspace → Enter」は別。後者は次 test。
    /// 当 test は「prefill が text_input に入っている → 何もしないで Enter → committed_text =
    /// 既存の "x" → SetLyrics(0, Some("x"))」 (no-op だが Edit は発行される) を確認。
    #[test]
    fn lyric_edit_prefill_then_enter_commits_existing_lyric() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![Note {
            id: 0,
            start_beat: 1.0,
            len_beats: 1.0,
            pitch: 60,
            velocity: 96,
            lyric: Some(Arc::from("x")),
            muted: false,
            style: NoteStyle::default(),
        }]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_enter()], ..Default::default() },
            view,
            &style,
            |_| {},
        );
        // prefill "x" がそのまま commit text として渡る → SetLyrics(0, Some("x"))
        assert_eq!(model.last_set_lyrics, Some(vec![(0u32, Some("x".to_string()))]));
    }

    /// `"x"` lyric を持つ note → 全選択 → typing で空 (※ 全選択中の typing は replace)
    /// → Enter → SetLyrics(0, None) (空文字列正規化)。
    /// 簡易化: 直接 select_all (Ctrl+A) → Backspace → Enter の代わりに
    /// "全選択中に何かを type して Enter" だと replace になる。 ここでは
    /// **既存 lyric を Backspace で削除 → Enter** で空文字列 commit を再現する。
    #[test]
    fn lyric_edit_empty_string_normalized_to_none() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![Note {
            id: 0,
            start_beat: 1.0,
            len_beats: 1.0,
            pitch: 60,
            velocity: 96,
            lyric: Some(Arc::from("x")),
            muted: false,
            style: NoteStyle::default(),
        }]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L → enter mode (prefill = "x"、全選択済)
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // Frame 2: Backspace で全選択 "x" を削除 (text_input は空に) → Enter
        let key_backspace = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Backspace,
        };
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_backspace, key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(0u32, None)]),
            "空文字 commit は None に正規化"
        );
    }

    /// lyric_editing = Some(id) 状態で notes から id を消す → 次 frame で
    /// frame 頭 sync check が lyric_editing = None に reset。
    #[test]
    fn lyric_edit_auto_clears_when_target_note_deleted() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![note(0, 1.0, 1.0, 60)]);
        model.selected = vec![0];
        let view = test_view();
        let style = PianoRollStyle::default();

        // Frame 1: L → enter mode
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        // notes から id 0 を消す (= 外部要因で note 削除)
        model.notes.clear();

        // Frame 2: lyric_editing が Some(0) のまま render → 頭 sync で None に reset
        let mut got_lyric_editing = Some(0);
        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput::default(),
            view,
            &style,
            |resp| got_lyric_editing = resp.lyric_editing,
        );
        assert_eq!(got_lyric_editing, None, "編集対象 note 消失で auto-clear");
    }

    /// 同 start_beat、pitch 60/72 の 2 note → pitch 72 起点で "ab" + Enter →
    /// SetLyrics(72→"a", 60→"b") (高 pitch 先取り順序)。
    #[test]
    fn lyric_edit_same_beat_pitch_desc_order() {
        let mut host = setup_lyric_test_host();
        let mut model = TestModel::new(vec![
            note(60, 0.0, 0.5, 60),  // pitch 60、id 60 (id=pitch でわかりやすく)
            note(72, 0.0, 0.5, 72),  // pitch 72、id 72 (高 pitch)
        ]);
        model.selected = vec![72]; // 高 pitch を起点に
        let view = test_view();
        let style = PianoRollStyle::default();

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput { keyboard: vec![key_l()], ..Default::default() },
            view,
            &style,
            |_| {},
        );

        run_lyric_frame(
            &mut host,
            &mut model,
            FrameInput {
                keyboard: vec![key_typing('a', "a"), key_typing('b', "b"), key_enter()],
                ..Default::default()
            },
            view,
            &style,
            |_| {},
        );
        assert_eq!(
            model.last_set_lyrics,
            Some(vec![(72u32, Some("a".to_string())), (60u32, Some("b".to_string()))]),
            "同 beat なら高 pitch (72) が先、低 pitch (60) が次"
        );
    }

    // M14 Phase 61b (#011): fold_piano_roll_note_hash の cache invalidation 性質を verify。
    // (1) 同一データ stable、 (2-5) start_beat / len_beats / pitch / velocity 各変化で hash 変
    // 化、 (6) lyric Arc identity の振る舞い (同 Arc clone は同 hash、 別 Arc::from は別 hash)。

    fn note_with_velocity(id: NoteId, start: f64, len: f64, pitch: u8, vel: u8) -> Note {
        Note { id, start_beat: start, len_beats: len, pitch, velocity: vel, lyric: None, muted: false, style: NoteStyle::default() }
    }

    #[test]
    fn fold_piano_roll_note_hash_stable() {
        let notes = vec![note(0, 0.0, 1.0, 60), note(1, 1.0, 0.5, 64)];
        assert_eq!(
            fold_piano_roll_note_hash(&notes),
            fold_piano_roll_note_hash(&notes),
            "同じ notes の fold は冪等"
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_move() {
        let before = vec![note(0, 0.0, 1.0, 60)];
        let after = vec![note(0, 0.5, 1.0, 60)]; // start 0 → 0.5
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_resize() {
        let before = vec![note(0, 0.0, 1.0, 60)];
        let after = vec![note(0, 0.0, 2.0, 60)]; // len 1 → 2
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_pitch() {
        let before = vec![note(0, 0.0, 1.0, 60)];
        let after = vec![note(0, 0.0, 1.0, 64)]; // pitch 60 → 64
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    #[test]
    fn fold_piano_roll_note_hash_changes_on_velocity() {
        let before = vec![note_with_velocity(0, 0.0, 1.0, 60, 96)];
        let after = vec![note_with_velocity(0, 0.0, 1.0, 60, 127)]; // vel 96 → 127
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(&after),
        );
    }

    /// mute トグルで note cache (dim + 斜線ハッチ) が再描画されるよう、
    /// `muted` が hash に効くことを保証する (cache 無効化の回帰防止)。
    #[test]
    fn fold_piano_roll_note_hash_changes_on_muted() {
        let before = vec![note(0, 0.0, 1.0, 60)];
        let mut muted = note(0, 0.0, 1.0, 60);
        muted.muted = true;
        assert_ne!(
            fold_piano_roll_note_hash(&before),
            fold_piano_roll_note_hash(std::slice::from_ref(&muted)),
        );
    }

    /// (review) lyric は cached 層 (`draw_notes`) で描かない (`draw_lyrics` は
    /// cache 外) ため fold hash に **含めない**。 旧実装の Arc ptr hash は caller
    /// (daw_gui) が毎フレーム新規 `Arc::from` を作るせいで恒常 cache miss を
    /// 起こしていた。 別 Arc / 別内容でも hash 不変 = cache が保たれることを固定する。
    #[test]
    fn fold_piano_roll_note_hash_ignores_lyric() {
        let n1 = Note {
            id: 0,
            start_beat: 0.0,
            len_beats: 1.0,
            pitch: 60,
            velocity: 96,
            lyric: Some(Arc::<str>::from("a")),
            muted: false,
            style: NoteStyle::default(),
        };
        // 別 Arc (毎フレーム再生成相当) でも同 hash。
        let n2 = Note { lyric: Some(Arc::<str>::from("a")), ..n1.clone() };
        assert_eq!(
            fold_piano_roll_note_hash(std::slice::from_ref(&n1)),
            fold_piano_roll_note_hash(std::slice::from_ref(&n2)),
        );
        // 内容が変わっても cached 層は影響を受けないので同 hash (歌詞の描画は
        // cache 外 pass が毎フレーム反映する)。
        let n3 = Note { lyric: Some(Arc::<str>::from("b")), ..n1.clone() };
        assert_eq!(
            fold_piano_roll_note_hash(std::slice::from_ref(&n1)),
            fold_piano_roll_note_hash(std::slice::from_ref(&n3)),
        );
    }

    /// style (dimmed / locked / color) は cached 層の `note_fill_color` に効くので
    /// hash に含まれる (対象トラック切替 / L ロック / トラック色変更の stale 防止)。
    #[test]
    fn fold_piano_roll_note_hash_changes_on_style() {
        let base = vec![note(0, 0.0, 1.0, 60)];
        let mut dimmed = note(0, 0.0, 1.0, 60);
        dimmed.style.dimmed = true;
        assert_ne!(
            fold_piano_roll_note_hash(&base),
            fold_piano_roll_note_hash(std::slice::from_ref(&dimmed)),
        );
        let mut locked = note(0, 0.0, 1.0, 60);
        locked.style.locked = true;
        assert_ne!(
            fold_piano_roll_note_hash(&base),
            fold_piano_roll_note_hash(std::slice::from_ref(&locked)),
        );
        let mut colored = note(0, 0.0, 1.0, 60);
        colored.style.color = Some(daw_ui_renderer::Color::rgb(0.9, 0.2, 0.1));
        assert_ne!(
            fold_piano_roll_note_hash(&base),
            fold_piano_roll_note_hash(std::slice::from_ref(&colored)),
        );
    }

    /// `PianoRollStyle::lyric_font_px` を 7.0 未満に設定しても `f32::clamp(min > max)` で
    /// panic しないことを保証する regression test。 caller が極小 cap を設定したら、 cap が
    /// 勝つ動作 (note 高さ比例に関わらず cap を上限) に正規化される。
    #[test]
    fn piano_roll_lyric_draw_does_not_panic_with_tiny_lyric_font_px_cap() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        // 歌詞付き note を 1 つ持つ model + lyric_font_px = 6.0 (= floor 7.0 未満) の style。
        let mut n = note(1, 0.0, 1.0, 60);
        n.lyric = Some(Arc::<str>::from("a"));
        let model = TestModel::new(vec![n]);
        let view = test_view();
        let style = PianoRollStyle { lyric_font_px: 6.0, ..PianoRollStyle::default() };
        let mut scene = daw_ui_renderer::Scene::new();
        let screen = PhysicalSize { width: 800, height: 400 };
        let sel: Vec<NoteId> = Vec::new();
        // panic しなければ pass (戻り値は使わない)。
        host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput::default(),
            |m, ui| {
                let _ = ui.piano_roll(
                    "test_lyric_cap",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.notes,
                    view,
                    &sel,
                    &style,
                    |_| Edit::mutate(|_: &mut TestModel| {}),
                );
            },
        );
    }

    // ============================================================
    // M14 Phase 69 / daw_01 #041: ruler 上 click/drag による playhead seek +
    // Shift+drag による loop range edit
    // ============================================================

    /// ruler_h=20 で snap OFF の test view (raw beat 値を期待値で検証する)。
    /// `test_view()` の `ruler_h=0` 既存挙動と分離。
    fn ruler_view() -> PianoRollView {
        PianoRollView { ruler_h: 20.0, ..test_view() }
    }

    /// rect (0,0)-(800,400)、 ruler_h=20 → grid 1 beat = 200 px (len_beats=4)。
    /// y=10 は ruler 内、 y >= 20 は grid。
    const PR_RULER_Y: f32 = 10.0;

    /// plain (Shift 非保持) ruler click → press frame で `SetPlayheadBeat` を 1 度発火。
    /// snap OFF なので raw beat 値が直接送られる、 0.0 以上 clamp の確認も兼ねる。
    #[test]
    fn ruler_click_emits_set_playhead_beat() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = ruler_view();
        let style = PianoRollStyle::default();
        // press at x=400, y=10 → ruler 内、 beat = 400/200 = 2.0
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, PR_RULER_Y)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel: Vec<NoteId> = vec![];
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.last_request, Some(RequestKind::SetPlayheadBeat));
        assert_eq!(
            model.playhead_beats.len(),
            1,
            "press frame で 1 度だけ emit: {:?}",
            model.playhead_beats
        );
        assert!(
            (model.playhead_beats[0] - 2.0).abs() < 1e-6,
            "x=400 → beat 2.0: {:?}",
            model.playhead_beats[0]
        );
    }

    /// `view.ruler_h == 0.0` のとき ruler 内 press は一切 SetPlayheadBeat を emit しない
    /// (旧 piano_roll API 完全互換、 ruler.h=0 で `ruler.contains` が false になる)。
    #[test]
    fn ruler_h_zero_does_not_emit_set_playhead_beat() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = test_view(); // ruler_h: 0.0
        let style = PianoRollStyle::default();
        // y=10 は ruler_h=0 のときは grid 領域 (= 通常の click として selection clear 等が走るが
        // SetPlayheadBeat は発火しない)。
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, 10.0)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        let sel: Vec<NoteId> = vec![];
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert!(
            model.playhead_beats.is_empty(),
            "ruler_h=0 では SetPlayheadBeat は発火しない: {:?}",
            model.playhead_beats
        );
    }

    /// plain ruler drag は press + continuation で連続発火、 release frame では emit しない。
    /// 同値発火抑制で連続同 beat は 1 度のみ。
    #[test]
    fn ruler_drag_emits_continuation_beats_until_release() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = ruler_view();
        let style = PianoRollStyle::default();
        let sel: Vec<NoteId> = vec![];

        // Frame 1: press at x=400 (beat 2.0) → emit
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((400.0, PR_RULER_Y)),
                primary_just_pressed: true,
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });

        // Frame 2: continuation at x=600 (beat 3.0) → emit
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((600.0, PR_RULER_Y)),
                primary_pressed: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });

        // Frame 3: release at x=600 (beat 3.0) → release frame は emit しない
        let input3 = FrameInput {
            pointer: PointerFrame {
                pos: Some((600.0, PR_RULER_Y)),
                primary_just_released: true,
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input3, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });

        // press (beat 2.0) + continuation (beat 3.0) で 2 emit、 release は emit せず。
        assert_eq!(
            model.playhead_beats.len(),
            2,
            "press + 1 continuation で 2 emit (release は emit せず): {:?}",
            model.playhead_beats
        );
        assert!((model.playhead_beats[0] - 2.0).abs() < 1e-6);
        assert!((model.playhead_beats[1] - 3.0).abs() < 1e-6);
    }

    /// Shift + ruler drag → release frame で `SetLoopRange` を 1 度だけ発火 (snap 適用済 endpoints)。
    /// drag 中の continuation で SetPlayheadBeat は発火しない (Shift で loop edit に振り分け済)。
    #[test]
    fn shift_ruler_drag_emits_set_loop_range_on_release() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = ruler_view();
        let style = PianoRollStyle::default();
        let sel: Vec<NoteId> = vec![];

        // Frame 1: Shift + press at x=200 (beat 1.0) → NewRange anchor 1.0
        let input1 = FrameInput {
            pointer: PointerFrame {
                pos: Some((200.0, PR_RULER_Y)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { shift: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input1, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });

        // Frame 2: drag at x=600 (beat 3.0、 continuation)
        let input2 = FrameInput {
            pointer: PointerFrame {
                pos: Some((600.0, PR_RULER_Y)),
                primary_pressed: true,
                modifiers: Modifiers { shift: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input2, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });

        // Frame 3: release at x=600 → SetLoopRange (start=1.0, end=3.0) 発火
        let input3 = FrameInput {
            pointer: PointerFrame {
                pos: Some((600.0, PR_RULER_Y)),
                primary_just_released: true,
                modifiers: Modifiers { shift: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input3, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });

        assert!(
            model.playhead_beats.is_empty(),
            "Shift+drag は loop edit に振り分け、 SetPlayheadBeat は発火しない: {:?}",
            model.playhead_beats
        );
        assert_eq!(model.last_request, Some(RequestKind::SetLoopRange));
        let (s, e) = model.last_set_loop_range.expect("SetLoopRange 発火");
        assert!((s - 1.0).abs() < 1e-6, "loop start = beat 1.0: {s}");
        assert!((e - 3.0).abs() < 1e-6, "loop end = beat 3.0: {e}");
    }

    /// Alt + ruler click → snap 一時無効、 raw beat 値が pass-through される
    /// (snap ON の view で alt 無しと有りの比較)。
    #[test]
    fn alt_ruler_click_disables_snap() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = PianoRollView {
            ruler_h: 20.0,
            // 1 beat snap (SnapMode::Straight { div: 4 })
            snap: SnapConfig {
                mode: crate::snap::SnapMode::Straight { div: 4 },
                enabled: true,
                min_beat_unit: 1.0 / 128.0,
                time_sig: (4, 4),
            },
            ..test_view()
        };
        let style = PianoRollStyle::default();
        let sel: Vec<NoteId> = vec![];

        // x=350 → raw beat 1.75、 snap (1 beat unit) → 2.0、 alt で snap 一時無効 → 1.75 (raw)
        let input = FrameInput {
            pointer: PointerFrame {
                pos: Some((350.0, PR_RULER_Y)),
                primary_just_pressed: true,
                primary_pressed: true,
                modifiers: Modifiers { alt: true, ..Modifiers::empty() },
                ..PointerFrame::default()
            },
            ..Default::default()
        };
        run_frame(&mut host, &mut model, input, |ui| {
            let dispatch = make_dispatch();
            let _ = ui.piano_roll(
                "pr",
                Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                &[],
                view,
                &sel,
                &style,
                dispatch,
            );
        });
        assert_eq!(model.playhead_beats.len(), 1);
        assert!(
            (model.playhead_beats[0] - 1.75).abs() < 1e-6,
            "Alt 押下で snap 一時無効、 raw 1.75 が pass-through: got {:?}",
            model.playhead_beats[0]
        );
    }

    // ============================================================
    // M14 Phase 70 / daw_01 #042: Scale Highlight / Fold tests
    // ============================================================

    /// C Major scale mask (bit 0,2,4,5,7,9,11)。
    const MAJOR_MASK: u16 = 0b0000_1010_1011_0101;

    fn scale_c_major(mode: PianoRollScaleMode) -> PianoRollScale {
        PianoRollScale { root: 0, in_scale_mask: MAJOR_MASK, mode, prefer_flats: false }
    }

    fn scale_d_major(mode: PianoRollScaleMode) -> PianoRollScale {
        // D Major: 同 mask、 root = D (pc 2)。 in-scale = D, E, F#, G, A, B, C# (pitch class 2,4,6,7,9,11,1)。
        PianoRollScale { root: 2, in_scale_mask: MAJOR_MASK, mode, prefer_flats: false }
    }

    #[test]
    fn is_in_scale_c_major_root_0() {
        let sc = scale_c_major(PianoRollScaleMode::Highlight);
        // C major = pc 0,2,4,5,7,9,11 (C D E F G A B)
        for (pc, expect) in (0_u8..12).zip([
            true, false, true, false, true, true, false, true, false, true, false, true,
        ]) {
            // pitch 60 + pc (= middle C 系)
            let pitch = 60 + pc;
            assert_eq!(
                sc.is_in_scale(pitch),
                expect,
                "pc={pc} pitch={pitch} expected in_scale={expect}"
            );
        }
    }

    #[test]
    fn is_in_scale_d_major_root_2() {
        let sc = scale_d_major(PianoRollScaleMode::Highlight);
        // D major = pc 2,4,6,7,9,11,1 (D E F# G A B C#)
        let in_pcs = [1_u8, 2, 4, 6, 7, 9, 11];
        for pc in 0_u8..12 {
            let expected = in_pcs.contains(&pc);
            assert_eq!(
                sc.is_in_scale(60 + pc),
                expected,
                "pc={pc} expected in_scale={expected}"
            );
        }
    }

    #[test]
    fn scale_degree_round_trip_c_major() {
        let sc = scale_c_major(PianoRollScaleMode::Highlight);
        // 全 in-scale pitch で round-trip 厳密一致
        for p in 0_u8..=127 {
            if sc.is_in_scale(p) {
                let d = sc.pitch_to_scale_degree(p);
                assert_eq!(
                    sc.scale_degree_to_pitch(d),
                    p,
                    "round-trip mismatch at pitch {p}, degree {d}"
                );
            }
        }
    }

    #[test]
    fn scale_degree_out_of_scale_returns_below_in_scale_degree() {
        let sc = scale_c_major(PianoRollScaleMode::Highlight);
        // C# (pitch 61) は out、 直下 C (pitch 60) の degree と同じ
        let d_cs = sc.pitch_to_scale_degree(61);
        let d_c = sc.pitch_to_scale_degree(60);
        assert_eq!(d_cs, d_c, "out-of-scale pitch 61 should share degree with pitch 60");
        // D (pitch 62) の degree は C より +1
        let d_d = sc.pitch_to_scale_degree(62);
        assert_eq!(d_d, d_c + 1, "D should be C degree + 1");
    }

    #[test]
    fn pitch_class_name_table() {
        let names: Vec<&str> = (0_u8..12).map(pitch_class_name).collect();
        assert_eq!(
            names,
            vec!["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]
        );
    }

    #[test]
    fn row_geometry_linear_when_scale_none() {
        let v = test_view();
        let g = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let geom = RowGeometry::compute(v, g);
        assert!(!geom.fold);
        // row_h = h / pitch_visible = 400 / 24 ≈ 16.67
        let expected = 400.0 / 24.0;
        assert!((geom.row_h - expected).abs() < 1e-3, "row_h: {} vs {}", geom.row_h, expected);
    }

    #[test]
    fn row_geometry_fold_visible_in_scale_pitches() {
        let v = PianoRollView {
            // pitch_top=72 (C5), pitch_visible=12 → range [60..72] = C4..C5
            pitch_top: 72.0,
            pitch_visible: 12.0,
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            ..test_view()
        };
        let g = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let geom = RowGeometry::compute(v, g);
        assert!(geom.fold);
        // 期待: [72, 71, 69, 67, 65, 64, 62, 60] (C, B, A, G, F, E, D, C) top→bottom
        // C5(72) は in (pc 0), B4(71) in (pc 11), Bb4(70) out, A4(69) in, ...
        assert_eq!(
            geom.fold_rows,
            vec![72, 71, 69, 67, 65, 64, 62, 60],
            "C major fold visible pitches"
        );
        // row_h = h / 8 = 50
        assert!((geom.row_h - 50.0).abs() < 1e-3);
    }

    #[test]
    fn row_geometry_pitch_to_y_fold_in_scale() {
        let v = PianoRollView {
            pitch_top: 72.0,
            pitch_visible: 12.0,
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            ..test_view()
        };
        let g = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let geom = RowGeometry::compute(v, g);
        // C5 (in-scale, row 0): y = 0
        let (y0, _) = geom.pitch_to_y_and_h(72);
        assert!((y0 - 0.0).abs() < 1e-3, "C5 should be at top: y={y0}");
        // D4 (pitch 62, row 6): y = 6 * 50 = 300
        let (y6, _) = geom.pitch_to_y_and_h(62);
        assert!((y6 - 300.0).abs() < 1e-3, "D4 at row 6: y={y6}");
    }

    #[test]
    fn row_geometry_pitch_to_y_fold_out_of_scale_midpoint() {
        let v = PianoRollView {
            pitch_top: 72.0,
            pitch_visible: 12.0,
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            ..test_view()
        };
        let g = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let geom = RowGeometry::compute(v, g);
        // C#5 (pitch 73): out, 視界外 (pitch_top=72)、 上 nothing → row 0 の上に描画
        // Bb4 (pitch 70): out (D# のフラット側、 A♭4 でなく Bb4 = B♭ = pitch 70)
        // 直上 B4 (row 1), 直下 A4 (row 2) → y_mid = (50 + 100) / 2 = 75
        // 中間描画 (高さ 0.5 row) → y = 75 + 12.5 = 87.5 ?? Let me check.
        // pitch_to_y_and_h for out: y_mid + row_h * 0.25 = 75 + 50*0.25 = 87.5、 h = 50*0.5 - 1 = 24
        let (y_bb, h_bb) = geom.pitch_to_y_and_h(70);
        assert!((y_bb - 87.5).abs() < 1e-3, "Bb4 midpoint y: {y_bb}");
        assert!((h_bb - 24.0).abs() < 1e-3, "Bb4 half-row h: {h_bb}");
    }

    #[test]
    fn y_to_pitch_fold_snaps_to_in_scale() {
        let v = PianoRollView {
            pitch_top: 72.0,
            pitch_visible: 12.0,
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            ..test_view()
        };
        let g = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        let geom = RowGeometry::compute(v, g);
        // row_h = 50、 fold_rows = [72, 71, 69, 67, 65, 64, 62, 60]。
        // y_to_pitch_f は in-scale pitch を整数で返す (= row 写像) ので == 比較で安全。
        #[allow(clippy::float_cmp)]
        {
            // y=0 → row 0 → pitch 72 (C5)
            assert_eq!(geom.y_to_pitch_f(0.0), 72.0);
            // y=300 → row 6 → pitch 62 (D4)
            assert_eq!(geom.y_to_pitch_f(300.0), 62.0);
            // y=399 (下端) → row 7 → pitch 60 (C4)
            assert_eq!(geom.y_to_pitch_f(399.0), 60.0);
        }
    }

    #[test]
    fn compute_pitch_drag_delta_linear() {
        let v = PianoRollView { pitch_top: 72.0, pitch_visible: 24.0, ..test_view() };
        let g = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        // pitch_per_px = 24/400 = 0.06、 dy = 17 (= 約 1 半音下) → -1
        let d = compute_pitch_drag_delta(v, g, 17.0);
        assert_eq!(d, -1, "linear: 17px ≈ 1 semitone");
    }

    #[test]
    fn compute_pitch_drag_delta_fold() {
        let v = PianoRollView {
            pitch_top: 72.0,
            pitch_visible: 12.0,
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            ..test_view()
        };
        let g = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };
        // row_h = 50、 dy = 50 (= 1 row 下) → -1 (= 1 scale degree 下)
        let d = compute_pitch_drag_delta(v, g, 50.0);
        assert_eq!(d, -1, "fold: 1 row = -1 scale degree");
        // dy = 150 (= 3 rows) → -3
        let d3 = compute_pitch_drag_delta(v, g, 150.0);
        assert_eq!(d3, -3);
    }

    #[test]
    fn apply_pitch_drag_delta_fold_always_in_scale() {
        let v = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            ..test_view()
        };
        // C5 (in-scale) を -1 scale degree → B4 (in-scale)。 last_alt=false (Fold mode は無関係)
        assert_eq!(apply_pitch_drag_delta(72, -1, v, false), 71);
        // E4 (in-scale, pitch 64) を -2 → 必ず in-scale
        let result = apply_pitch_drag_delta(64, -2, v, false);
        let sc = scale_c_major(PianoRollScaleMode::Fold);
        assert!(sc.is_in_scale(result), "fold delta result {result} should be in-scale");
    }

    #[test]
    fn apply_pitch_drag_delta_linear_semitone() {
        let v = test_view(); // scale = None
        // pitch 60 + (-1) = 59 (B、 = 半音下)、 last_alt=false (scale None なので影響なし)
        assert_eq!(apply_pitch_drag_delta(60, -1, v, false), 59);
        // pitch 60 + (+5) = 65 (F)
        assert_eq!(apply_pitch_drag_delta(60, 5, v, false), 65);
        // clamp test: 0 - 1 = 0
        assert_eq!(apply_pitch_drag_delta(0, -1, v, false), 0);
    }

    // -------- Phase 70b / daw_01 #042 follow-up: snap_pitch_during_drag tests --------

    #[test]
    fn snap_to_nearest_in_scale_returns_input_when_already_in_scale() {
        let sc = scale_c_major(PianoRollScaleMode::Highlight);
        // C, D, E, F, G, A, B (= in-scale) は変化なし
        for &p in &[60_u8, 62, 64, 65, 67, 69, 71] {
            assert_eq!(snap_to_nearest_in_scale(p, sc), p);
        }
    }

    #[test]
    fn snap_to_nearest_in_scale_picks_nearest() {
        let sc = scale_c_major(PianoRollScaleMode::Highlight);
        // C# (61) は 上 D(62) / 下 C(60) で同距離 → 上優先 = 62
        assert_eq!(snap_to_nearest_in_scale(61, sc), 62);
        // D# (63) も 上 E(64) / 下 D(62) で同距離 → 上優先 = 64
        assert_eq!(snap_to_nearest_in_scale(63, sc), 64);
        // F# (66) は 上 G(67) / 下 F(65) → 上優先 = 67
        assert_eq!(snap_to_nearest_in_scale(66, sc), 67);
    }

    #[test]
    fn apply_pitch_drag_delta_highlight_snap_when_flag_on() {
        let v = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Highlight)),
            snap_pitch_during_drag: true,
            ..test_view()
        };
        // pitch 60 (C) + (-1) = 59 (B、 in-scale) → snap 後 59 (= そのまま)
        assert_eq!(apply_pitch_drag_delta(60, -1, v, false), 59);
        // pitch 60 (C) + (-2) = 58 (Bb、 out-of-scale) → 上 B(59) と下 A(57) は同距離 → 上 = 59
        assert_eq!(apply_pitch_drag_delta(60, -2, v, false), 59);
        // pitch 60 + (+1) = 61 (C#、 out) → 上 D(62) / 下 C(60) 同距離 → 上 = 62
        assert_eq!(apply_pitch_drag_delta(60, 1, v, false), 62);
        // pitch 60 + (+3) = 63 (D#、 out) → 上 E(64) / 下 D(62) 同距離 → 上 = 64
        assert_eq!(apply_pitch_drag_delta(60, 3, v, false), 64);
    }

    #[test]
    fn apply_pitch_drag_delta_highlight_snap_disabled_when_alt() {
        let v = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Highlight)),
            snap_pitch_during_drag: true,
            ..test_view()
        };
        // alt=true で snap 無効、 raw clamp が走る
        // pitch 60 + (-2) = 58 (raw、 Bb)、 alt で snap 無効なので 58 のまま
        assert_eq!(apply_pitch_drag_delta(60, -2, v, true), 58);
        assert_eq!(apply_pitch_drag_delta(60, 1, v, true), 61);
    }

    #[test]
    fn apply_pitch_drag_delta_highlight_no_snap_when_flag_off() {
        let v = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Highlight)),
            snap_pitch_during_drag: false, // 旧挙動
            ..test_view()
        };
        // flag off で旧挙動 (raw clamp)
        assert_eq!(apply_pitch_drag_delta(60, -2, v, false), 58);
        assert_eq!(apply_pitch_drag_delta(60, 1, v, false), 61);
    }

    #[test]
    fn apply_pitch_drag_delta_fold_ignores_snap_flag() {
        // Fold は元々 scale degree 単位、 flag 関係なく in-scale 出力
        let v = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            snap_pitch_during_drag: false,
            ..test_view()
        };
        let sc = scale_c_major(PianoRollScaleMode::Fold);
        for delta in [-3_i32, -1, 0, 1, 2, 5] {
            let r = apply_pitch_drag_delta(60, delta, v, false);
            assert!(sc.is_in_scale(r), "Fold flag=off: delta={delta} → {r} must be in-scale");
        }
        let v_on = PianoRollView { snap_pitch_during_drag: true, ..v };
        for delta in [-3_i32, -1, 0, 1, 2, 5] {
            let r = apply_pitch_drag_delta(60, delta, v_on, false);
            assert!(sc.is_in_scale(r), "Fold flag=on: delta={delta} → {r} must be in-scale");
        }
    }

    #[test]
    fn apply_pitch_drag_delta_highlight_snap_multi_anchor_relative_preserved() {
        // multi-select drag で全 anchor が同じ delta を適用される。 anchor 間の相対位置が
        // 「同 scale degree 差」 を保つことを確認 (= Bitwig 流 multi-drag)。
        let v = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Highlight)),
            snap_pitch_during_drag: true,
            ..test_view()
        };
        // anchor 1: C (60), anchor 2: E (64) (= 3 semitones above C)
        // delta = +2 半音 (raw)、 snap 後:
        //   C(60) + 2 = D(62) (in-scale そのまま)
        //   E(64) + 2 = F#(66) → snap → 上G(67) / 下F(65) 同距離 → 上 = 67
        // 結果: D(62) と G(67) で「scale degree 差 3」 維持 (= 元の C/E 差と等価ではないが
        // raw 半音差 2 を delta としたときの自然な結果)。
        assert_eq!(apply_pitch_drag_delta(60, 2, v, false), 62);
        assert_eq!(apply_pitch_drag_delta(64, 2, v, false), 67);
    }

    #[test]
    fn scale_none_view_compiles_and_renders() {
        // scale = None で旧 API 完全互換 (regression check)
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![Note {
            id: 0,
            start_beat: 0.5,
            len_beats: 0.5,
            pitch: 60,
            velocity: 96,
            lyric: None,
            muted: false,
            style: NoteStyle::default(),
        }]);
        let view = test_view(); // scale = None
        let style = PianoRollStyle::default();
        let sel: Vec<NoteId> = vec![];
        host.frame(
            &mut model,
            &mut Scene::new(),
            PhysicalSize { width: 800, height: 400 },
            FrameInput::default(),
            |m, ui| {
                let _ = ui.piano_roll(
                    "pr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.notes,
                    view,
                    &sel,
                    &style,
                    |_| Edit::mutate(|_m: &mut TestModel| {}),
                );
            },
        );
        // frame が panic しないこと
    }

    #[test]
    fn highlight_mode_view_compiles_and_renders() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![]);
        let view = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Highlight)),
            ..test_view()
        };
        let style = PianoRollStyle::default();
        let sel: Vec<NoteId> = vec![];
        host.frame(
            &mut model,
            &mut Scene::new(),
            PhysicalSize { width: 800, height: 400 },
            FrameInput::default(),
            |m, ui| {
                let _ = ui.piano_roll(
                    "pr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.notes,
                    view,
                    &sel,
                    &style,
                    |_| Edit::mutate(|_m: &mut TestModel| {}),
                );
            },
        );
    }

    #[test]
    fn fold_mode_view_compiles_and_renders() {
        let mut host: UiHost<TestModel> = UiHost::no_redraw();
        let mut model = TestModel::new(vec![Note {
            // pitch 61 (C#、 out-of-scale) を fold mode で描画して中間描画 path を hit させる
            id: 0,
            start_beat: 1.0,
            len_beats: 0.5,
            pitch: 61,
            velocity: 96,
            lyric: None,
            muted: false,
            style: NoteStyle::default(),
        }]);
        let view = PianoRollView {
            scale: Some(scale_c_major(PianoRollScaleMode::Fold)),
            ..test_view()
        };
        let style = PianoRollStyle::default();
        let sel: Vec<NoteId> = vec![];
        host.frame(
            &mut model,
            &mut Scene::new(),
            PhysicalSize { width: 800, height: 400 },
            FrameInput::default(),
            |m, ui| {
                let _ = ui.piano_roll(
                    "pr",
                    Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 },
                    &m.notes,
                    view,
                    &sel,
                    &style,
                    |_| Edit::mutate(|_m: &mut TestModel| {}),
                );
            },
        );
    }
}
