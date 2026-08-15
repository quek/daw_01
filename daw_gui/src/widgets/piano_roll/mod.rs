//! `piano_roll` widget — DAW piano roll (鍵盤 / ruler / grid / velocity lane / note 編集) を
//! 1 widget で扱う library widget (M9 Phase 41e)。
//!
//! S4c (arch-refactor): 旧 `ui/` 汎用 widget から `daw_gui` へ移設し `common::model` 直読・
//! `Edit<AppData>` 直発行に変更 (mirror 型 + `make_edit` 翻訳層を撤去)。
//!
//! - **Note schema** (id / start_beat / len_beats / pitch / velocity / lyric) は library 公開型。
//!   id は `u32` (packed clip_slot + local index) で move/delete でも不変 (multi-select identity 安定)。
//! - **描画 + drag state machine + hit-test + shortcut + rect select** は widget 内に閉じる。
//!   heavy() ブロック + cached(viewport_key) で背景を粗粒度キャッシュ、selection / drag preview /
//!   playhead / lyric は cached 外で毎フレーム描画。
//! - **Edit 発行はインライン**: 各 interaction site が `ui.push_edit(Edit::mutate(|app| ...))` を
//!   直接発行する (`app.handle_event(AppEvent::X)` / `app.seek_playhead_to(..)` へ流す)。
//! - **commit-by-release**: drag 中は library が overlay 描画、release frame で初めて
//!   `SetNotePositions` / `ResizeNotes` / `SetLoopRange` 等を発行する。
//! - **座標系**: widget は song-absolute 拍で動き、note 書き戻し出口で `clip_start` を減算する
//!   (note は共有 content のため clip-local 保持)。
//!
//! # モジュール構成 (arrangement と平行)
//! - [`view_build`] — `AppData` → `BuiltPianoRoll` (レイアウト SSoT 込み)。
//! - `geometry` — hit-test / 座標変換 / drag geometry。
//! - `draw` — heavy/cached 内の描画 helper (`HeavyCtx<M>` 汎用)。
//! - `run` — `piano_roll()` エントリ + toolbar / legend / wheel / hover mirror。

use std::collections::HashSet;
use std::sync::Arc;

use daw_ui_platform::CursorIcon;
use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};
use crate::theme::Theme;

use daw_ui_core::edit::Edit;
use daw_ui_core::id::WidgetId;
use daw_ui_core::scenegraph::hash_inputs;
use common::snap::SnapConfig;
use common::time::{TimeDisplay, TimeMapping};
use daw_ui_core::ui::Ui;
use daw_ui_core::viewport::ViewportState1D;
use daw_ui_core::widgets::playhead::draw_playhead_line;
use crate::widgets::ruler_ops::{
    LoopBandHit, LoopDragKind, LoopDragSession, PlayheadDragSession,
    compute_loop_drag_endpoints, loop_band_hit_kind,
};
use crate::widgets::time_grid::{BarBeatGridStyle, SubGridSpec, TimeGridExt, TimeRulerStyle};

pub(crate) mod view_build;
mod draw;
use draw::*;
mod geometry;
use geometry::*;
mod run;
pub use run::piano_roll;

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
/// - `velocity: u8` — MIDI 0..127 (色濃度に使う、`PianoRollStyle::velocity_ramp` で Color に変換)
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
    /// (複数クリップをクリップ色で塗り分ける)。`None` = 既存どおり `PianoRollStyle::velocity_ramp`
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
    /// このフレームで note selection Edit (`SetNoteSelection`) を push_edit したか (= 次フレームで
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

/// velocity → note fill を決める色ランプ。両端はテーマトークン
/// (`daw.note_velocity_low` = velocity 0 / `daw.note_velocity_high` = velocity 127) から解決し、
/// 間は線形補間する。
///
/// r.md #48 で旧 `NoteFillFn` (= `fn(velocity: u8) -> Color` の関数ポインタ) を置き換えた。
/// 関数ポインタは runtime パレットを読めないため、テーマを切り替えても note だけ旧色のまま
/// 取り残される。`PianoRollStyle` の他フィールドと同じ「解決済みの色」を持つ `Copy` データに
/// することで、heavy() クロージャへの capture (style ごと move) もそのまま維持できる。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VelocityRamp {
    /// velocity 0 の色 (ランプ下端)。
    pub low: Color,
    /// velocity 127 の色 (ランプ上端)。
    pub high: Color,
}

impl VelocityRamp {
    /// `velocity` (0..=127) に対応する fill。`velocity = 0` で [`low`](Self::low)、
    /// `127` で [`high`](Self::high)。
    #[must_use]
    pub fn fill(self, velocity: u8) -> Color {
        self.low.lerp(self.high, f32::from(velocity) / 127.0)
    }
}

/// piano roll の見た目スタイル。[`PianoRollStyle::from_theme`] で現在のテーマから解決する。
///
/// 色は全て **解決済みの値** で持つ (テーマトークンへの参照は持たない)。widget 本体は
/// `ui.heavy()` のクロージャへ style ごと move するので、`Copy` であることが load-bearing。
#[derive(Clone, Copy, Debug)]
pub struct PianoRollStyle {
    /// grid (note 領域) の背景色 = **白鍵レーン**。
    ///
    /// **不変条件 (M14 Phase 63d / daw_01 #017)**: `black_row_overlay` を `bg` に src-over 合成した
    /// 結果が `bg` より **暗く** なるよう値を選ぶこと (= 鍵盤側 `white_key` > `black_key` の
    /// 濃淡関係を grid 側でも保つ)。Ableton Live / Cubase / Reaper / FL Studio 等の主流 DAW 慣習。
    /// `default_black_row_is_darker_than_white_row` /
    /// `row_shading_and_label_polarity_hold_in_every_builtin_theme` test で全テーマ分固定。
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
    /// `NoteStyle::color == None` の note を velocity で塗るランプ (両端はテーマトークン)。
    pub velocity_ramp: VelocityRamp,
    pub note_border_radius_px: f32,
    /// muted note に重ねる斜線ハッチの色 (半透明)。極性固定の `core.hatch_ink` を
    /// note 用の濃さ (alpha 0.40) にしたもの。`Note.muted == true` のときのみ描画。
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
    /// root/out overlay の alpha 合成色) の輝度で明暗反転するか。 default `true`。 `false` で
    /// 旧挙動 (root=`root_label_fg` / in-scale=`in_scale_label_fg` / C=`c_label_color` の固定色)。
    /// arrangement の clip 名 auto-contrast (#060) と同じ「widget が実際に塗った fill から文字色を
    /// 導出する」 SSoT を鍵盤ラベルに適用したもの。 warm root 行 (root_row_overlay 重畳の cream) /
    /// 白鍵で暗文字、 黒鍵 / dim 行で明文字を選ぶ。
    ///
    /// **選ばれる 2 色は style に持たない** (r.md #48): 鍵盤 fill は可変背景なので極性固定インク
    /// (`Palette::ink_for` = `ink_on_bright` / `ink_on_dark`) が唯一の出どころ。
    pub label_auto_contrast: bool,
}

/// クリップ基底色 `base` を velocity で陰影付けする (hue は保ち明度のみ変える)。
/// 低 velocity ほど暗く (係数 0.55..1.0)。`NoteStyle::color = Some` の note に使う。
/// alpha は `base` を維持。[`VelocityRamp`] (velocity → テーマの青の濃淡) のクリップ色版に相当。
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


impl PianoRollStyle {
    /// いま有効なテーマから piano roll の全色を解決する (r.md #48)。
    ///
    /// 呼び出しは 1 frame 1 回 (`view_build`) を想定。`Copy` なので heavy() クロージャへは
    /// 値ごと move する。
    #[must_use]
    pub fn from_theme(theme: &Theme) -> Self {
        let p = &theme.core;
        let d = &theme.daw;
        Self {
            // M14 Phase 63d / daw_01 #017: 鍵盤側 `key_white > key_black` の濃淡に揃え、
            // grid の **白鍵レーン (= bg) を黒鍵レーン (= bg + overlay) より明るく** する。
            // 旧値 `bg=(0.12)` + `overlay=rgba(1,1,1,0.04)` は黒鍵 row が約 (0.155) で bg より
            // 明るくなる (鍵盤と逆) symptom があった。Live / Cubase / Reaper 慣習に合わせる。
            // 階層 (elevation): ruler_bg(header) < velocity_lane_bg(panel) < bg / keyboard_bg
            // (panel_raised) → grid (note 配置領域) が最も浮き、 周辺 panel が段階的に沈む。
            bg: p.panel_raised,
            keyboard_bg: p.panel_raised,
            // 物理ピアノ鍵盤のメタファなので、 面 (elevation) ではなく専用トークン。
            white_key: d.key_white,
            black_key: d.key_black,
            // src-over 合成で黒鍵 row を bg より暗くする極性固定インク
            // (ライトテーマでも「黒鍵 row は沈む」 は反転してはいけない)。
            black_row_overlay: p.row_dim_ink,
            bar_line: p.grid_line_strong,
            beat_line: p.grid_line,
            bar_line_width_px: 1.5,
            beat_line_width_px: 1.0,
            // M14 Phase 124 / daw_01 #100: subdivision 線は beat_line より淡い 3 段目。
            sub_line: p.grid_line_faint,
            sub_line_width_px: 1.0,
            velocity_ramp: VelocityRamp { low: d.note_velocity_low, high: d.note_velocity_high },
            note_border_radius_px: 1.5,
            // note は clip より小さいので clip ハッチ (alpha 0.34) より一段濃く。
            note_muted_hatch_color: p.hatch_ink.with_alpha(0.40),
            note_muted_hatch_spacing_px: 5.0,
            note_muted_hatch_width_px: 1.0,
            note_selected_fill: p.selection_warm,
            // 選択リングは velocity / クリップ色で着色された note の上に乗るので極性固定 (常に明)。
            note_selected_border: p.selection_ring_outer,
            note_selected_border_w: 2.0,
            note_selected_pad_px: 2.0,
            // M14 Phase 83 / daw_01 #054: copy ghost は move ghost (黄) と区別する緑系
            // (arrangement の clone linked ghost と同系統)。
            note_clone_ghost_fill: d.ghost_linked.with_alpha(0.85),
            note_clone_ghost_border: d.ghost_linked,
            resize_handle_px: 4.0,
            c_label_color: p.text_faint,
            c_label_font_px: 11.0,
            // M9 Phase 45c: playhead / velocity lane
            playhead_color: d.playhead,
            playhead_width_px: 2.5,
            velocity_lane_bg: p.panel,
            velocity_bar_color: p.accent,
            velocity_bar_width_px: 3.0,
            // 歌詞は velocity / クリップ色で塗られた note の上に乗る = 可変背景。 note fill は
            // 明るい側なので極性固定の暗インク (テーマで反転させるとライトで消える)。
            lyric_color: p.ink_on_bright,
            // M14 Phase 59: MAX cap (実 font_size = note_h * 0.75 で note 高さスケール)。
            // 旧 9.0 固定 → 24.0 max にして zoom in 時の readable 化。
            lyric_font_px: 24.0,
            // M14 Phase 59 / daw_01 #017: 歌詞編集 (L キー) shortcut。caller が `bind("L")` する想定。
            lyric_edit_shortcut: Some("piano_roll.edit_lyric"),
            // M13 Phase 55: ruler 領域 (`view.ruler_h > 0` のときのみ描画)
            ruler_bg: p.header,
            ruler_label_color: p.text_dim,
            // M14 Phase 69 / daw_01 #041: arrangement と同値 (loop_band ~0.20 alpha 帯 + 不透明 handle)。
            loop_band: p.loop_band.with_alpha(0.20),
            loop_handle: p.loop_band,
            loop_handle_w: 2.0,
            // M14 Phase 70 / daw_01 #042 + 70a (follow-up): warm tint (root 行) + dim (out 行)。
            // alpha は daw_01 実機 smoke test (#042 follow-up) で「白鍵 row 上の root tint が
            // 見えない / 黒鍵 row との dim 差が 0.015 で out 認識が立たない」 指摘を受けて
            // 引き上げ済 (0.18 / 0.32 では不可視レベルだった)。
            root_row_overlay: p.selection_warm.with_alpha(0.32),
            // out 行は黒鍵 row との差が認識できるまで `row_dim_ink` を濃くした段 (alpha 0.50)。
            out_of_scale_row_overlay: p.row_dim_ink.with_alpha(0.50),
            // 鍵盤レーンラベルの fallback 色 (`label_auto_contrast == false` のときだけ使う)。
            // root は選択暖色を強調、 in-scale は Fold mode で全行に label が出るので二次テキスト、
            // out-of-scale は最弱層 (Highlight mode で root 行以外の label 描画は v0 では出ないが、
            // 将来「全 12 行 label」 拡張用に予約)。
            root_label_fg: p.selection_warm,
            in_scale_label_fg: p.text_dim,
            out_of_scale_label_fg: p.text_faint,
            // M14 Phase 117 / daw_01 #093: 鍵盤ラベルの auto-contrast (既定 on)。 選ぶ 2 色は
            // `Palette::ink_for` が持つ (行の実効背景が白鍵 / 黒鍵 / warm cream root 行と **可変**
            // なので極性固定インク。 `p.text` にするとライトテーマで白鍵行のラベルが消える)。
            label_auto_contrast: true,
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
    /// (r.md #35) drag 中の最終 shift 状態。 `last_ctrl` と完全同型の careful-update で保持し、
    /// 短 click 格下げ時の選択 modifier (Shift = 範囲選択) を release frame の生読みに
    /// 依存せず確定させる (arrangement の `ClipDragSession.last_shift` と同 idiom)。
    last_shift: bool,
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
/// press → drag → release で完結し、 release frame で **1 個の `AddNote` Edit** を
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
// Tests (pure functions のみ。 widget interaction は tests/pr_widget.rs へ)
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

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

    fn theme_of(theme_id: &str) -> Theme {
        Theme::builtin(theme_id).expect("組込みテーマ")
    }

    fn style_of(theme_id: &str) -> PianoRollStyle {
        PianoRollStyle::from_theme(&theme_of(theme_id))
    }

    /// 黒鍵レーン (= `bg` に `black_row_overlay` を src-over 合成した色) が `bg` より暗いか。
    fn black_row_is_darker(style: &PianoRollStyle) -> bool {
        let (bg, ov) = (style.bg, style.black_row_overlay);
        // src-over: out = src.rgb * src.a + dst.rgb * (1 - src.a)
        let bk = |s: f32, d: f32| s * ov.a + d * (1.0 - ov.a);
        bk(ov.r, bg.r) < bg.r && bk(ov.g, bg.g) < bg.g && bk(ov.b, bg.b) < bg.b
    }

    /// ダークテーマの `bg` (= 白鍵レーン) と `black_row_overlay` を src-over 合成した結果
    /// (= 黒鍵レーン) は **bg より暗くなる** こと。鍵盤側の white_key > black_key と濃淡関係を
    /// 一致させる業界標準動作。 Ableton Live / Cubase / Reaper / FL Studio 慣習。
    #[test]
    fn default_black_row_is_darker_than_white_row() {
        let style = style_of("dark");
        assert!(
            black_row_is_darker(&style),
            "黒鍵 row は bg ({:?}) より暗いべき (鍵盤と整合)",
            style.bg
        );
        // 鍵盤側の濃淡関係も同方向であることを念のため確認 (regression 防止)。
        assert!(
            style.white_key.r > style.black_key.r,
            "鍵盤 white_key.r > black_key.r 不変条件"
        );
    }

    /// r.md #48: 上の 2 つの濃淡不変条件と鍵盤ラベルの極性は、**どのテーマでも** 成り立つ。
    /// `black_row_overlay` やラベル色を面トークン (= ライトでは明るい側) に倒すと、黒鍵 row が
    /// 白鍵 row より明るくなったり、白鍵行のラベルが背景に溶けて消えたりする。
    #[test]
    fn row_shading_and_label_polarity_hold_in_every_builtin_theme() {
        for id in ["dark", "light"] {
            let theme = theme_of(id);
            let p = &theme.core;
            let style = PianoRollStyle::from_theme(&theme);
            assert!(black_row_is_darker(&style), "{id}: 黒鍵 row は白鍵 row より暗い");
            assert!(style.white_key.r > style.black_key.r, "{id}: 白鍵 > 黒鍵");
            // 鍵盤ラベルは行の実効背景で極性が決まる (明るい行 → 暗インク / 暗い行 → 明インク)。
            assert_eq!(
                keyboard_label_color(p, &style, style.white_key, None, style.in_scale_label_fg),
                p.ink_on_bright,
                "{id}: 白鍵行 → 暗ラベル"
            );
            assert_eq!(
                keyboard_label_color(p, &style, style.black_key, None, style.in_scale_label_fg),
                p.ink_on_dark,
                "{id}: 黒鍵行 → 明ラベル"
            );
            assert_eq!(
                keyboard_label_color(
                    p,
                    &style,
                    style.white_key,
                    Some(style.root_row_overlay),
                    style.root_label_fg
                ),
                p.ink_on_bright,
                "{id}: warm root tint を重ねた白鍵行 → 暗ラベル"
            );
        }
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
        let theme = theme_of("dark");
        let p = &theme.core;
        let style = PianoRollStyle::from_theme(&theme);
        // 白鍵 + root_row_overlay (warm cream) → 明るい背景 → 暗ラベル (旧 warm-on-warm 潰れを解消)。
        let on_root = keyboard_label_color(
            p,
            &style,
            style.white_key,
            Some(style.root_row_overlay),
            style.root_label_fg,
        );
        assert_eq!(on_root, p.ink_on_bright, "warm cream root 行 → 暗ラベル");
        // 黒鍵 (overlay 無し) → 暗い背景 → 明ラベル。
        let on_black =
            keyboard_label_color(p, &style, style.black_key, None, style.in_scale_label_fg);
        assert_eq!(on_black, p.ink_on_dark, "黒鍵行 → 明ラベル");
        // 黒鍵 root + root_row_overlay: warm cream を 32% 重ねると実効色は画面上
        // sRGB (166,149,110) の中間トーンになる (linear 0.388/0.305/0.165、輝度 0.313)。
        // 明るい側なので暗ラベル。 2026-08-15 まで relative_luminance の二重デコードで
        // 「暗い」と誤判定され明ラベルが選ばれていたが、実効 2.6:1 で読みづらかった。
        let on_black_root = keyboard_label_color(
            p,
            &style,
            style.black_key,
            Some(style.root_row_overlay),
            style.root_label_fg,
        );
        assert_eq!(on_black_root, p.ink_on_bright, "黒鍵 root + warm overlay 行 → 暗ラベル");
        // 白鍵 in-scale (overlay 無し) → 明るい → 暗ラベル (旧 in_scale_label_fg の明文字が潰れる症状も解消)。
        let on_white =
            keyboard_label_color(p, &style, style.white_key, None, style.in_scale_label_fg);
        assert_eq!(on_white, p.ink_on_bright, "白鍵 in-scale 行 → 暗ラベル");
        // opt-out: fallback 固定色をそのまま返す。
        let off = PianoRollStyle { label_auto_contrast: false, ..style };
        assert_eq!(
            keyboard_label_color(
                p,
                &off,
                off.white_key,
                Some(off.root_row_overlay),
                off.root_label_fg
            ),
            off.root_label_fg,
            "auto_contrast=false は fallback 固定色"
        );
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
        assert_eq!(velocity_bar_hit(&notes, view, area, 200.0, 3.0, 4.0, |_| false), Some(7));
        // 中央 ±5.5px (= bar_width/2 + tolerance) は hit
        assert_eq!(velocity_bar_hit(&notes, view, area, 195.0, 3.0, 4.0, |_| false), Some(7));
        assert_eq!(velocity_bar_hit(&notes, view, area, 205.0, 3.0, 4.0, |_| false), Some(7));
    }

    #[test]
    fn velocity_bar_hit_misses_outside_tolerance() {
        let view = test_view();
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        let notes = vec![note(7, 1.0, 0.5, 60)]; // bar x = 200
        // hit zone は ±5.5px。 7 px 離れていれば miss。
        assert_eq!(velocity_bar_hit(&notes, view, area, 207.0, 3.0, 4.0, |_| false), None);
        assert_eq!(velocity_bar_hit(&notes, view, area, 193.0, 3.0, 4.0, |_| false), None);
    }

    #[test]
    fn velocity_bar_hit_overlapping_returns_last() {
        // 2 つの note が同 start_beat にあるとき、 選択が無ければ後勝ち (visible 順 = note_hit と同 semantics)。
        let view = test_view();
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        let notes = vec![note(1, 1.0, 0.5, 60), note(2, 1.0, 0.5, 67)];
        assert_eq!(velocity_bar_hit(&notes, view, area, 200.0, 3.0, 4.0, |_| false), Some(2));
    }

    #[test]
    fn velocity_bar_hit_overlapping_prefers_selected() {
        // daw_01 #33: 同じ x に重なった bar のうち選択中 note を優先 hit する。
        let view = test_view();
        let area = Rect { x: 0.0, y: 340.0, w: 800.0, h: 60.0 };
        let notes = vec![note(1, 1.0, 0.5, 60), note(2, 1.0, 0.5, 67)];
        // note 1 (最前面でない) が選択されていれば、 後勝ちの note 2 でなく note 1 を返す。
        assert_eq!(velocity_bar_hit(&notes, view, area, 200.0, 3.0, 4.0, |id| id == 1), Some(1));
        // note 2 が選択されていれば note 2 (これは後勝ちとも一致)。
        assert_eq!(velocity_bar_hit(&notes, view, area, 200.0, 3.0, 4.0, |id| id == 2), Some(2));
        // 両方選択なら、 選択中の後勝ち = note 2。
        assert_eq!(
            velocity_bar_hit(&notes, view, area, 200.0, 3.0, 4.0, |_| true),
            Some(2)
        );
    }
}
