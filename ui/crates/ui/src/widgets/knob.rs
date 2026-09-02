//! `knob` ウィジェット — 回転ノブ。ドラッグで値編集 (上下ドラッグ、上 = 増)。
//!
//! - 値範囲: `0.0..=1.0`
//! - 視覚: 7 時の位置から 5 時の位置まで 300° のスイープ (DAW 標準)
//! - **値弧の起点は [`KnobStyle::arc_origin`]** で決まる (DAW 標準): unipolar param
//!   (send level / dry-wet) は 7 時起点、bipolar param (pan / balance) は 12 時起点で
//!   中央から左右へ伸びる。起点が可動範囲の内側にあるときは起点に目印 (notch) を描く
//! - drag 感度: **ノブの大きさに依らない定数** ([`KNOB_UNITS_PER_PX`]) = 250px のドラッグで 0 → 1
//! - hit area: rect 全体 (つまみが小さいので円外部でもドラッグ可とする)
//! - **DAW 標準挙動** (fader と同じ):
//!   - ダブルクリックで `default_value` に戻る (~300ms × 5px 以内の 2 回目 press)
//!   - Ctrl + ドラッグで感度 1/10 (高精度)。Mid-drag toggle で値が jump しないよう再 anchor

use std::f32::consts::PI;
use std::hash::Hash;
use std::time::Instant;

use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, hovered};
use crate::widgets::scrubable_number::{ModEntry, Modulation};

/// ダブルクリック判定の時間しきい値 (ms)。
const DOUBLE_CLICK_MS: u128 = 300;
/// ダブルクリック判定の位置しきい値 (px)。
const DOUBLE_CLICK_PX: f32 = 5.0;
/// Ctrl + ドラッグ時の感度倍率 (1/10)。
const FINE_DRAG_SCALE: f32 = 0.1;
/// depth-edit gesture を「実 drag」 と見なす最小移動量 (px、 縦距離)。 これ未満の press→release は
/// micro-jitter として depth Edit を発火させず `mod_dragging` も立てない (scrubable_number #107 と同義)。
const DRAG_THRESHOLD_PX: f32 = 4.0;
/// 可動範囲のスイープ角 (rad) = 300° (7 時 → 5 時)。 残る 60° (5 時 → 6 時 → 7 時) は範囲外で
/// 弧が届かない (= "切れて見える") DAW 標準の見え方。
const SWEEP: f32 = 5.0 * PI / 3.0;
/// 値弧 / track 弧の線幅 (px)。**基準径 [`STROKE_REFERENCE_SIZE_PX`] での値**で、
/// 実際の線幅は [`stroke_scale`] を掛けて求める。
const ARC_WIDTH_PX: f32 = 4.0;
/// 指針 (中心から外周へ伸びる太線) の線幅 (px)。基準径での値。
const INDICATOR_WIDTH_PX: f32 = 4.0;
/// 線幅を調律した基準のノブ径 (px)。mixer の pan ノブがこの径。
const STROKE_REFERENCE_SIZE_PX: f32 = 32.0;
/// 起点 notch の線幅 (px)。 値弧より細く、 指針より細い「パネル刻印」の太さ。
const NOTCH_WIDTH_PX: f32 = 1.5;
/// 弧を折れ線近似するときの 1 segment の目標弦長 (px)。 **1px を下回らせない** ことが要点
/// (line pipeline の quad が sub-pixel になると rasterizer が拾い落として弧に穴が空く)。
/// 均等割りで実際の弦長はこの値の 1/2 〜 1 倍に収まるので、 3px なら最悪でも 1.5px。
const ARC_CHORD_PX: f32 = 3.0;
/// 弧の角度刻みの下限 (大半径で刻みが細かくなり過ぎて instance 数が膨らむのを防ぐ)。
const ARC_STEP_MIN: f32 = 2.0 * PI / 180.0;
/// 弧の角度刻みの上限 (小半径で粗くなり過ぎないように)。 15° 刻みでも半径 7px の
/// 弦の反り (sagitta) は 0.06px で、 目に見える多角形化は起きない。
const ARC_STEP_MAX: f32 = 15.0 * PI / 180.0;
/// 起点 notch がリング内側へ食い込む長さ (px、 弧の内縁からさらに内側へ)。
const NOTCH_INNER_PX: f32 = 3.0;
/// 起点と現在値の差がこれ未満なら値弧を描かない (正規化値の dead band)。 丸め誤差で pan が
/// センタから 1e-5 ずれただけで「センタなのに片側に 1px の欠片が光る」 のを防ぐ。 DAW 実装の
/// 慣習値: nih-plug `ParamSlider` は 1e-3、 iced_audio の bipolar 判定は ±0.001。
const ARC_DEAD_BAND: f32 = 1e-3;
/// 線幅倍率の下限 / 上限。細くしすぎると line pipeline の quad が sub-pixel になって
/// 弧が途切れ、太くしすぎると大径ノブで円面が塗り潰される。
const STROKE_SCALE_MIN: f32 = 0.55;
/// 線幅倍率の上限 ([`STROKE_SCALE_MIN`] 参照)。
const STROKE_SCALE_MAX: f32 = 1.25;

/// ノブ径に応じた線幅の倍率。
///
/// 弧も指針も刻印も **径に比例** させる。基準 (32px) で調律した太さをそのまま
/// 20px のノブに使うと、線が太すぎて円面が潰れ「小さいノブほど塗り絵に見える」。
/// caller に線幅を持たせないのは、同じ見た目の調律を使う側全員に写させないため。
fn stroke_scale(size: f32) -> f32 {
    (size / STROKE_REFERENCE_SIZE_PX).clamp(STROKE_SCALE_MIN, STROKE_SCALE_MAX)
}
/// knob の drag 感度 (値/px)。 **ノブの大きさに依らない定数** = 可動域全体を 250px で走る。
///
/// 一次情報: x42 robtk `robtk_dial.h` の `d->base_mult *= 0.004; // 250px` (= 25〜40px の dial を
/// 駆動する実装で、 当 knob の 32px / 18px と同じサイズ帯)。 参考値: Ardour `scale = 0.0025`
/// (400px)、 VCV Rack `0.001` (1000px)。 **3 実装とも knob の大きさに依らない定数** で駆動する。
///
/// 旧実装の `1/rect.h` (= 32px で端から端) は fader からの写しだったが、 fader の式は「thumb が
/// pointer に追従する」 幾何的必然から来るもので、 drag 軸上に追従対象が無い knob には根拠が無い
/// (1px = 3.1% 変化では微調整不能で、 px ドメインの detent も成立しない)。
pub const KNOB_UNITS_PER_PX: f32 = 0.004;
/// detent (零点吸着) の総 dead travel (px)。 平坦域は target の左右に半分ずつ (片側 16px)。
///
/// 一次情報: Ardour `px_deadzone = 42.f * ui_scale` (`ardour_ctrl_base.cc:171`)、 x42 robtk
/// `px_deadzone = 34.f - n_detents` (= 33px)。 [`KNOB_UNITS_PER_PX`] との比で 32px = 可動域の
/// 12.8% (robtk 13.2% / Ardour 10.5% と同水準)。 **Ctrl (fine) でも px は同じ** (両実装とも
/// modifier で deadzone を縮めない) なので、 値ドメインでは自動的に 1/10 になる。
const DETENT_TOTAL_PX: f64 = 32.0;

/// knob の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct KnobState {
    drag_anchor: Option<DragAnchor>,
    /// 直近のクリック (ダブルクリック判定用)。
    last_click: Option<ClickRecord>,
    /// M8 Phase 29: drag 開始時の値 (release frame で undoable Edit の inverse に使う)。
    drag_initial_value: Option<f32>,
    /// #109: press からの最大縦移動量 (px)。 depth gesture の `mod_dragging` / release 確定発火を
    /// `>= DRAG_THRESHOLD_PX` で gate して micro-jitter の depth Edit を防ぐ (scrubable と同 idiom、
    /// knob は縦専用なので合成 hypot でなく `|py - anchor_y|`)。 base scrub には影響しない (後方互換)。
    drag_distance: f32,
}

#[derive(Debug, Clone, Copy)]
struct DragAnchor {
    /// 押下/再 anchor 時のマウス y。
    pointer_y: f32,
    /// 押下/再 anchor 時の基準値。 base gesture は press 時の value、 depth-edit gesture は press 時の
    /// depth (= `ModEdit::current_depth`)。 どちらも knob と同じ 0..=1 正規化ドメイン (depth は符号付き)。
    value: f64,
    /// 押下/再 anchor 時の Ctrl 状態。mid-drag toggle で再 anchor する判定用。
    ctrl: bool,
    /// この gesture が depth-edit (= `Modulation::edit` Some) で始まったか。 true なら drag は base
    /// でなく depth を変化させ base scrub を抑止する (= 非破壊)。 gesture 途中で固定 (arm 変化に不追従)。
    depth_drag: bool,
}

#[derive(Debug, Clone, Copy)]
struct ClickRecord {
    when: Instant,
    pos: (f32, f32),
}

/// knob の視覚 + 吸着スタイル。
///
/// DAW の knob は param が unipolar (最小値が零点) か bipolar (中央が零点) かで **見かけの零点**
/// が変わり、 これは色やサイズと同じ「描画の指定」なので widget 引数ではなく style に置く
/// (scrubable_number / level_meter と同じ作法)。
///
/// `arc_origin` (描画) と `detent` (操作) は **独立** に指定する。 Ardour も `ArcToZero`
/// (弧を default 値起点に) と `Detent` (default 値で粘る) を別フラグにしていて、 例えば unity
/// gain を既定値とする音量 knob は「弧は左端起点 + unity で粘る」 = `arc_origin: 0.0` +
/// `detent: true` になる (`gtk2_ardour/monitor_section.cc` の `gain_control` / `dim_control`)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KnobStyle {
    /// 値弧 (accent) が伸び始める **起点値** (knob と同じ 0..=1 正規化ドメイン)。
    ///
    /// - `0.0` = 7 時起点 = unipolar (send level / dry-wet / 音量)。 [`KnobStyle::UNIPOLAR`]
    /// - `0.5` = 12 時起点 = bipolar (pan / balance / EQ gain)。 [`KnobStyle::BIPOLAR`]
    ///
    /// 起点が可動範囲の **内側** (`0.0 < arc_origin < 1.0`) のときは、 零点が中央にあることが
    /// 一目で分かるよう起点に目印 (notch) を描く (= 物理ノブのパネル刻印、 DAW 標準)。
    pub arc_origin: f32,
    /// `true` で **`default_value` に吸着** する (detent): ドラッグが `default_value` を通ると
    /// そこで一旦張り付き、 [`DETENT_TOTAL_PX`] 分のドラッグを追加で消費するまで
    /// 離れない。 pan をセンタへ正確に戻す / センタから不意にずれるのを防ぐ DAW 標準挙動。
    ///
    /// 吸着先が `arc_origin` ではなく `default_value` なのは Ardour と同じ帰属
    /// (`_normal = c->internal_to_interface(c->normal())` = param の既定値)。 modulation depth
    /// の drag には効かない (depth は base と別ドメインで、 Ardour にも対応概念が無い)。
    pub detent: bool,
    /// この knob が **載っている面** の色。
    ///
    /// 可動範囲 (300°) の外側にあたる下の 60° を この色のリングで塗り、 円本体の縁と枠を
    /// くり抜いて **「リングが下で切れている」** ように見せる。 これが無いと、 その 60° では
    /// 円本体の塗り (`control`) と 1px の枠 (`border`) がそのまま見えるため、 リングが
    /// 途切れず 1 周しているように読めてしまい、 「どこまで回るのか」 が分からない。
    ///
    /// `None` で palette の `panel` (elevation-1 = 主要 panel / strip 本体 =
    /// knob が載る既定の面)。 **面が既定と違う場所** に置く caller だけ `Some` を渡す
    /// (widget は自分が何色の上に描かれているかを知り得ないため、 そこだけは caller の責務)。
    pub surface: Option<Color>,
}

impl KnobStyle {
    /// 片極性: 弧は最小値 (7 時) から伸びる。 吸着なし。 送り量 / dry-wet など。
    pub const UNIPOLAR: Self = Self { arc_origin: 0.0, detent: false, surface: None };
    /// 双極性: 弧は中央 (12 時) から左右へ伸び、 中央に目印が付き、 中央 (= `default_value`)
    /// に吸着する。 pan / balance など。
    pub const BIPOLAR: Self = Self { arc_origin: 0.5, detent: true, surface: None };
}

/// 零点吸着 (detent) の写像。 ドラッグ座標 (raw) と値の間に「`target` で平らな区間」を挟む
/// piecewise-linear 写像で、 `plateau` がその区間の幅 (値単位、 target の左右へ半分ずつ)。
///
/// Ardour は incremental な motion delta から pixel を食う実装 (`_dead_zone_delta` に蓄積) だが、
/// 当 knob は **anchor からの絶対 delta** で値を出すので、 同じ体感を **状態を持たない純関数**
/// で作る (蓄積器が要らず、 mid-drag の再 anchor / release と干渉しない)。 `plateau == 0` で
/// 恒等写像 (= detent 無効) に退化する。
#[derive(Debug, Clone, Copy)]
struct Detent {
    target: f64,
    plateau: f64,
}

impl Detent {
    /// この gesture の detent 記述。 Ctrl (fine drag) 中は、 同じ **pixel 量** で抜けられるよう
    /// plateau を感度と同率で縮める (Ardour が `px_deadzone` を pixel 固定にしているのと同義)。
    fn for_gesture(style: KnobStyle, default_value: f32, ctrl: bool) -> Self {
        let scale = if ctrl { f64::from(FINE_DRAG_SCALE) } else { 1.0 };
        Self {
            target: f64::from(default_value),
            plateau: if style.detent {
                DETENT_TOTAL_PX * f64::from(KNOB_UNITS_PER_PX) * scale
            } else {
                0.0
            },
        }
    }

    /// 値 → raw ドラッグ座標 ([`Self::value_of`] の逆写像)。 `value_of(raw_of(v)) == v` なので
    /// **掴んだ瞬間に値が跳ねない**。 ちょうど target 上の値は plateau の中心に置く (= どちら
    /// 向きにも半幅ぶん粘る = 対称)。
    fn raw_of(self, value: f64) -> f64 {
        let half = self.plateau * 0.5;
        if value < self.target {
            value - half
        } else if value > self.target {
            value + half
        } else {
            self.target
        }
    }

    /// raw ドラッグ座標 → 値。 `target ± plateau/2` の区間は `target` に張り付き、 外側は
    /// 平行移動 (傾き 1 のまま) なので連続。
    fn value_of(self, raw: f64) -> f64 {
        let half = self.plateau * 0.5;
        if raw < self.target - half {
            raw + half
        } else if raw > self.target + half {
            raw - half
        } else {
            self.target
        }
    }
}

impl Default for KnobStyle {
    fn default() -> Self {
        Self::UNIPOLAR
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KnobResponse {
    /// 描画された値 (drag 中は preview、 idle は入力値、 dblclick reset frame は default_value)。
    pub displayed_value: f32,
    /// rect 上に cursor が乗っているか。
    pub hovered: bool,
    /// base value の drag scrub 中 (depth-edit gesture とは排他)。
    pub dragging: bool,
    /// modulation depth の drag 編集中 (= `Modulation::edit` Some + press 中)。 base `dragging` とは
    /// 排他。 edge 検出で caller が undo bracket (`ParamGestureBegin/End` 相当) を発火する (daw_01 #109)。
    pub mod_dragging: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で knob を描画 + ドラッグ。値変化時に `on_change(new_value)` を Edit 列に積む。
    ///
    /// drag 中は per-frame で forward Mutate、drag 終端で最終値を 1 度発行する。undo/redo は
    /// アプリ層の責務 (S4a で lib undo 撤去)。
    ///
    /// `default_value` は rect のダブルクリック時にリセットされる値 (例: pan の中央 0.5)。
    ///
    /// `style` は値弧の起点 (= 見かけの零点) を決める。 pan / balance のような bipolar param は
    /// [`KnobStyle::BIPOLAR`] を渡すと中央 (12 時) から左右へ弧が伸び、 中央に目印が付く。
    /// send level のような unipolar param は [`KnobStyle::UNIPOLAR`] (= `Default`)。
    /// **`default_value` とは別物** で、 dblclick の戻り先が中央でも弧の起点は左端でありうる
    /// (例: unity gain = 0.5 が既定値の送り量 knob は起点 0.0 のまま)。
    ///
    /// 操作:
    /// - rect 全体をドラッグで値編集 (rect.h 分 = 0→1)
    /// - rect 全体をダブルクリック (~300ms / 5px 以内) で `default_value` に戻る
    /// - Ctrl + ドラッグで感度 1/10
    ///
    /// `modulation`: `Some` で Bitwig 流 modulation を表示・編集する (daw_01 #109、 #107 scrubable_number
    ///   の knob 版)。 `None` で従来描画・従来挙動 (完全回帰)。 値ドメインは **knob と同じ正規化単位**
    ///   で渡す (knob は plain range を持たないため scrubable と違い range 引数不要、 弧 = 0..=1 そのもの):
    ///   絶対値 [`Modulation::live_value`] は 0..=1、 符号付き delta [`ModEntry::depth`] /
    ///   `ModEdit::current_depth` は base からの増減量 (典型 ±1、 実 clamp 域は `ModEdit::depth_range`、
    ///   polarity は caller が解決)。 [`Modulation::entries`] を base 角からの色弧でリング上に重畳、
    ///   [`Modulation::live_value`] を可動の半径マークで描画、 [`Modulation::edit`] が `Some` のとき
    ///   press + 縦 drag は base でなく depth を変化させ `on_mod_change` を発火する (base scrub 抑止)。
    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub fn knob_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        default_value: f32,
        style: &KnobStyle,
        on_change: F,
        modulation: Option<Modulation<'_, M>>,
    ) -> KnobResponse
    where
        F: Fn(f32) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"knob", &id));
        let pointer = self.pointer;
        let value = value.clamp(0.0, 1.0);
        let default_value = default_value.clamp(0.0, 1.0);
        let arc_origin = style.arc_origin.clamp(0.0, 1.0);

        // ---- modulation 記述の展開 (None = 完全回帰、 borrow のみ取り出す、 scrubable_number と同形) ----
        let mod_ref = modulation.as_ref();
        let mod_entries: &[ModEntry] = mod_ref.map_or(&[], |m| m.entries);
        let mod_live = mod_ref.and_then(|m| m.live_value);
        let mod_edit = mod_ref.and_then(|m| m.edit.as_ref());
        let depth_mode = mod_edit.is_some();
        let current_depth = mod_edit.map_or(0.0, |e| e.current_depth);
        let depth_range = mod_edit.and_then(|e| e.depth_range);
        // knob の base drag 感度 = [`KNOB_UNITS_PER_PX`] (= 250px で 0→1、 サイズ非依存)。 depth も
        // ModEdit 指定が無ければ同じ感度を流用 (knob 値と depth が同じ 0..=1 スパンなので自然)。
        let base_units_per_px = KNOB_UNITS_PER_PX;
        let depth_units_per_px = mod_edit
            .and_then(|e| e.depth_sensitivity)
            .unwrap_or(base_units_per_px);

        // 1. 押下処理 + 2. mid-drag ctrl toggle 再 anchor + 3. release 解除 (depth/base で分岐)
        let mut reset_fired = false;
        // depth gesture の release frame で確定する最終 depth (pointer 最終位置から再計算)。
        let mut release_depth: Option<f64> = None;
        let (drag_anchor, release_initial_value, drag_distance) = {
            let state: &mut KnobState = self.widget_state(wid);

            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && rect.contains(px, py)
            {
                let now = Instant::now();
                let is_double = state.last_click.is_some_and(|c| {
                    now.duration_since(c.when).as_millis() < DOUBLE_CLICK_MS
                        && (c.pos.0 - px).hypot(c.pos.1 - py) < DOUBLE_CLICK_PX
                });

                if is_double && !depth_mode {
                    // dblclick → default reset。 depth-edit 中は base を触らない (非破壊) ので reset を
                    // 抑止し、 下の else と同じく通常 press (depth gesture) として扱う。
                    state.last_click = None;
                    state.drag_anchor = None;
                    state.drag_initial_value = None;
                    state.drag_distance = 0.0;
                    reset_fired = true;
                } else {
                    state.last_click = Some(ClickRecord { when: now, pos: (px, py) });
                    // depth-edit 中は anchor の基準値を base でなく現 depth にする。
                    let anchor_value = if depth_mode { current_depth } else { f64::from(value) };
                    state.drag_anchor = Some(DragAnchor {
                        pointer_y: py,
                        value: anchor_value,
                        ctrl: pointer.modifiers.ctrl,
                        depth_drag: depth_mode,
                    });
                    // M8 Phase 29: drag 開始時の base 値を保存 (release の undoable inverse 用)。
                    state.drag_initial_value = Some(value);
                    state.drag_distance = 0.0;
                }
            }

            // mid-drag で Ctrl が toggle されたら anchor を張り直す (詳細は fader.rs 参照)。
            // depth gesture は depth 基準、 base gesture は base 基準で再 anchor (値 jump 回避)。
            if let Some(anchor) = state.drag_anchor
                && let Some((_, py)) = pointer.pos
                && pointer.modifiers.ctrl != anchor.ctrl
            {
                let anchor_value = if anchor.depth_drag { current_depth } else { f64::from(value) };
                state.drag_anchor = Some(DragAnchor {
                    pointer_y: py,
                    value: anchor_value,
                    ctrl: pointer.modifiers.ctrl,
                    depth_drag: anchor.depth_drag,
                });
            }

            // #109: drag 距離 (縦) の最大値を計測 (depth gesture の閾値判定用、 knob は縦専用)。
            if let (Some(anchor), Some((_, py))) = (state.drag_anchor, pointer.pos) {
                let d = (py - anchor.pointer_y).abs();
                if d > state.drag_distance {
                    state.drag_distance = d;
                }
            }

            // M8 Phase 29 + #109: release frame を depth / base で分岐。
            let mut release_initial_value: Option<f32> = None;
            if pointer.primary_just_released {
                let anchor_opt = state.drag_anchor;
                let init = state.drag_initial_value.take();
                let dist = state.drag_distance;
                state.drag_anchor = None;
                state.drag_distance = 0.0;
                if anchor_opt.is_some_and(|a| a.depth_drag) {
                    // depth gesture: per-frame は anchor が None になる release frame で fire しない
                    // ため、 pointer 最終位置から depth を再計算して 1 度確定発火する (daw_01 #109
                    // 「release で最終 depth も確定発火」)。 micro-jitter の click は閾値未満で抑止
                    // (scrubable_number #107 と同義、 base scrub は閾値なしで後方互換)。
                    if dist >= DRAG_THRESHOLD_PX
                        && let (Some(anchor), Some((_, py))) = (anchor_opt, pointer.pos)
                    {
                        let d =
                            knob_drag_delta(anchor.pointer_y, py, depth_units_per_px, anchor.ctrl);
                        release_depth = Some(clamp_opt(anchor.value + d, depth_range));
                    }
                } else {
                    // base scrub のみ release で undoable wrap するため初期値を残す。
                    release_initial_value = init;
                }
            }

            (state.drag_anchor, release_initial_value, state.drag_distance)
        };

        // 2. base 表示値: リセット > base drag (depth gesture 中は抑止 = 非破壊) > 入力値。
        let displayed_value: f32 = if reset_fired {
            default_value
        } else if let (Some(anchor), Some((_, py))) = (drag_anchor, pointer.pos)
            && !anchor.depth_drag
        {
            let d = knob_drag_delta(anchor.pointer_y, py, base_units_per_px, anchor.ctrl);
            // detent (`KnobStyle::detent`): anchor 値を raw ドラッグ座標へ写してから delta を
            // 足し、 値へ戻す。 plateau の中は `default_value` に張り付く (pan のセンタ吸着)。
            // detent 無効なら恒等写像なので旧経路とバイト等価。
            let detent = Detent::for_gesture(*style, default_value, anchor.ctrl);
            let raw = detent.raw_of(anchor.value) + d;
            (detent.value_of(raw) as f32).clamp(0.0, 1.0)
        } else {
            value
        };

        // depth 表示値 (= modulation 弧 + on_mod_change): depth gesture drag 中のみ更新、 他は現 depth。
        let displayed_depth: f64 = if let (Some(anchor), Some((_, py))) = (drag_anchor, pointer.pos)
            && anchor.depth_drag
        {
            let d = knob_drag_delta(anchor.pointer_y, py, depth_units_per_px, anchor.ctrl);
            clamp_opt(anchor.value + d, depth_range)
        } else {
            current_depth
        };

        // 3. 描画。M4 Phase 11: with_widget_node で input_hash キャッシュ。
        // base scrub と depth-edit は排他 (anchor.depth_drag で判定)。 depth gesture 中は base bg を
        // press 色にしない (= 非破壊の視覚化、 強調は overlay の source 色枠が担う)。
        let dragging = drag_anchor.is_some_and(|a| !a.depth_drag);
        // depth gesture は閾値超で初めて mod_dragging (= daw の undo bracket edge、 scrubable と同義)。
        // base dragging は後方互換で閾値なし (press から true)。
        let mod_dragging =
            drag_anchor.is_some_and(|a| a.depth_drag) && drag_distance >= DRAG_THRESHOLD_PX;
        let hovered_rect = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));
        let input_hash = hash_inputs((
            b"knob",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            displayed_value.to_bits(),
            default_value.to_bits(),
            // 弧の起点 (= 見かけの零点)。 hash に入れないと style 切替が cache に映らない。
            arc_origin.to_bits(),
            dragging,
            hovered_rect,
            // 可動範囲外をくり抜く面の色。 palette 由来の色は set_palette が
            // scene cache ごと捨てるので hash 不要だが、 これは **caller が渡す色** なので
            // 畳まないと「同じ knob を別の面へ移した」 変化が cache に映らない。
            style.surface.map(|c| (c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits())),
        ));
        let surface = style.surface;
        self.with_widget_node(wid, input_hash, |ui| {
            draw_knob(ui, rect, displayed_value, arc_origin, surface, dragging, pointer);
        });

        // ---- modulation overlay (= cache node の外、 毎フレーム描画) ----
        // live_value は ~30Hz、 base/depth は drag 追従なので cache に載せず overlay 化。 bg/arc/
        // indicator の cache node は modulation 非依存のまま据え置き (None で完全回帰)。 scrubable_number
        // の draw_modulation_overlay と同 idiom (cache の後に描く)。 cache HIT/MISS いずれでも毎フレーム
        // 描かれるので HIT 経路の取りこぼしは無い (feedback_cache_hit_path_and_multiframe_verify)。
        if mod_ref.is_some() {
            draw_knob_modulation_overlay(
                self,
                rect,
                displayed_value,
                displayed_depth,
                mod_entries,
                mod_live,
                mod_edit.map(|e| e.source_color),
            );
        }

        // 4. M8 Phase 29 + #109: depth (modulation) 発火 → base drag 中 / 終端 / undoable Edit。
        // depth per-frame: depth gesture hold 中のみ (release frame は anchor None で skip)。 daw は
        // mod_dragging の falling edge で undo bracket するため widget 側は base のような Undoable wrap
        // はしない (= base scrub と違う発火経路、 #107 と同契約)。
        if let Some(edit) = mod_edit
            && drag_anchor.is_some_and(|a| a.depth_drag)
            && (displayed_depth - current_depth).abs() > f64::EPSILON
        {
            self.push_edit((edit.on_mod_change)(displayed_depth));
        }
        // depth release-frame の最終確定発火 (上の per-frame は release frame で skip される)。
        if let Some(edit) = mod_edit
            && let Some(final_depth) = release_depth
            && (final_depth - current_depth).abs() > f64::EPSILON
        {
            self.push_edit((edit.on_mod_change)(final_depth));
        }

        // base scrub の per-frame mutate (depth gesture 中は displayed_value == value で自然に抑止、
        // release frame は下の undoable wrap で 1 度のみ)。
        let suppress_mutate_on_release = release_initial_value.is_some();
        if !suppress_mutate_on_release && (displayed_value - value).abs() > f32::EPSILON {
            let edit = on_change(displayed_value);
            self.push_edit(edit);
        }

        // S4a: base drag 終端で最終値を 1 度 commit (旧 Undoable の forward 相当)。undo はアプリ層。
        if let Some(start_value) = release_initial_value
            && (start_value - displayed_value).abs() > f32::EPSILON
        {
            self.push_edit(on_change(displayed_value));
        }

        KnobResponse {
            displayed_value,
            hovered: hovered(rect, pointer),
            dragging,
            mod_dragging,
        }
    }

    /// vstack カーソル位置に固定サイズで knob を追加 (64×64 px)。
    pub fn knob<F>(
        &mut self,
        id: impl Hash,
        value: f32,
        default_value: f32,
        style: &KnobStyle,
        on_change: F,
    ) -> KnobResponse
    where
        F: Fn(f32) -> Edit<M>,
    {
        let pad = 8.0;
        let size = 64.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: size,
            h: size,
        };
        let resp = self.knob_at(id, rect, value, default_value, style, on_change, None);
        self.next_y += size + pad;
        resp
    }
}

/// 正規化値 (0..=1) → 弧の角度 (rad、 12 時起点・時計回り正)。 value=0 → -150° (7 時)、
/// value=0.5 → 0° (12 時)、 value=1 → +150° (5 時)。 非有限値は 7 時に丸めて renderer に
/// NaN 座標を渡さない。 **knob の値 ⇄ 角度写像の SSoT** (本体描画と modulation overlay が共有)。
fn value_angle(v: f32) -> f32 {
    if !v.is_finite() {
        return -0.5 * SWEEP;
    }
    (v.clamp(0.0, 1.0) - 0.5) * SWEEP
}

#[allow(clippy::too_many_arguments)]
fn draw_knob<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    value: f32,
    arc_origin: f32,
    // この knob が載っている面の色 (`KnobStyle::surface`)。 None で palette の `panel`。
    surface: Option<Color>,
    dragging: bool,
    pointer: crate::input::PointerFrame,
) {
    let p = ui.palette();

    // 円本体: rect の中央に max-radius の正方形を置いて 4 隅 r で円形に。
    let size = rect.w.min(rect.h);
    let cx = rect.x + rect.w * 0.5;
    let cy = rect.y + rect.h * 0.5;
    let r = (size * 0.5 - 2.0).max(2.0); // 2px の周囲余白
    // 線幅はノブ径に比例させる (基準 32px = mixer の pan ノブ)。
    let lw = stroke_scale(size);
    let arc_w = ARC_WIDTH_PX * lw;
    let circle_rect = Rect { x: cx - r, y: cy - r, w: r * 2.0, h: r * 2.0 };

    // Ableton 流: 中立な `control` の円 + 円周上に `accent` の arc。
    // arc は「起点値の角度」から value_angle までを円周 (radius = r) 上に描画。
    // 下の 60° (5時 → 7時 経由 6時) は 300° sweep 範囲外なので、 面の色で塗って
    // リングを物理的に切る (下の「0.」 を参照)。
    let base = p.control;
    let hover_c = p.control_hover;
    let press_c = p.accent;
    let bg_fill = if dragging {
        press_c
    } else if hovered(rect, pointer) {
        base.lerp(hover_c, 0.85)
    } else {
        base
    };

    ui.push_rect(RectCommand {
        rect: circle_rect,
        fill: bg_fill,
        border: p.border,
        border_width: 1.0,
        radius: [r; 4],
        clip_rect: None,
    });

    // 角度: value=0 → -150° (7時)、value=0.5 → 0° (12時)、value=1 → +150° (5時)。
    let val_angle = value_angle(value);
    let origin_angle = value_angle(arc_origin);
    let start_angle = value_angle(0.0);
    let end_angle = value_angle(1.0);
    let arc_radius = r;
    let active_color = p.accent;
    let inactive_color = p.inset_bg;

    // 0. 可動範囲 **外** の 60° (5時 → 6時 → 7時) を「この knob が載っている面」の色で塗り、
    //    円本体の縁 (control) と 1px 枠 (border) をその区間だけくり抜く。
    //
    //    これが無いと、 可動範囲外でも円本体の縁と枠が見えているせいで **リングが途切れず
    //    1 周しているように読め**、 「ノブがどこまで回るのか」 が分からない (ユーザー報告:
    //    「パンのノブがそこまで回転しない場合の弧の色はミキサーの背景色と同じにしてください」)。
    //    面の色は widget からは知り得ないので caller が `KnobStyle::surface` で渡す。
    //    既定 (`None`) は `panel` = knob が載る標準的な面。
    //
    //    角度は end (+150°) → start + 360° (+210°) の **値弧・track 弧が通らない側**。
    //    線幅を ARC_WIDTH_PX に揃えるので、 くり抜きの内外縁が他の 2 本と一致する
    //    (揃っていないと「切り欠き」でなく「別の帯」に見える)。
    let surface_color = surface.unwrap_or(p.panel);
    push_arc(
        ui,
        cx,
        cy,
        arc_radius,
        end_angle,
        start_angle + 2.0 * PI,
        surface_color,
        arc_w,
    );

    // 1-2. 弧は 2 色で可動範囲 300° を **過不足なく 1 周** 分だけ描く:
    //   - 値弧 (accent) = 起点値 → 現在値。 unipolar (起点 0.0) では 7 時から伸び、
    //     bipolar (起点 0.5) では中央 12 時から左右どちらへも伸びる。 起点 == 現在値
    //     (dead band 内) なら 1 本も描かれない (= pan センタで塗りが消える)。
    //   - track (暗グレー) = 残りの可動範囲。 値弧が覆う区間は描かない。
    // 6時付近の 60° (5時 → 7時) は上の「0.」 が面の色で塗り済み = "弧が切れて見える"。
    //
    // Ardour / iced_audio は track を全 span 描いてから値弧を上書きするが、 こちらは弧を
    // polygon 近似 (2° 刻み) するので、 全 span 重ね描きは segment 数が最悪 2 倍になる
    // (mixer の strip 数だけ効く)。 同じ見た目を区間分割で得られるなら分割する。
    let (fill_lo, fill_hi) = if (value - arc_origin).abs() >= ARC_DEAD_BAND {
        let (lo, hi) = (origin_angle.min(val_angle), origin_angle.max(val_angle));
        push_arc(ui, cx, cy, arc_radius, lo, hi, active_color, arc_w);
        (lo, hi)
    } else {
        // 値弧なし = track が全 span (dead band 内は起点角で 0 幅の切れ目を作らない)。
        (origin_angle, origin_angle)
    };
    push_arc(ui, cx, cy, arc_radius, start_angle, fill_lo, inactive_color, arc_w);
    push_arc(ui, cx, cy, arc_radius, fill_hi, end_angle, inactive_color, arc_w);

    // 3. 起点 notch: 起点が可動範囲の内側 (= bipolar) のときだけ、 零点の位置を刻印する。
    //    値弧の**内縁より内側からリング外縁まで**を横切る細線で、 値弧 (2) の上に描く。
    //    「リング上の点」 として描くと、 センタでは指針に、 振り切った側では値弧の始端に
    //    必ず覆われて消えるため、 radial に横切る形が唯一「常に見える」 幾何。
    if arc_origin > 0.0 && arc_origin < 1.0 {
        let dx = origin_angle.sin();
        let dy = -origin_angle.cos();
        let inner = (arc_radius - arc_w * 0.5 - NOTCH_INNER_PX * lw).max(1.0);
        let outer = arc_radius + arc_w * 0.5;
        ui.push_lines(LineBatch {
            segments: vec![LineSegment {
                a: [cx + dx * inner, cy + dy * inner],
                b: [cx + dx * outer, cy + dy * outer],
                color: p.text_dim,
            }]
            .into(),
            line_width_px: NOTCH_WIDTH_PX * lw,
            clip_rect: None,
        });
    }

    // インジケータ: 中心から外円まで伸びる太線。値角度を指す。
    let dx = val_angle.sin();
    let dy = -val_angle.cos();
    let indicator = LineSegment {
        a: [cx, cy],
        b: [cx + dx * r, cy + dy * r],
        // knob の値を指す物理的なつまみ指針。 hover/drag で色を変えない常時最大コントラストの
        // 指針なので、 中立ハンドル 2 段のうち強い方 (`handle_active`) を使う (ダークでは明、
        // ライトでは暗に反転して、 どちらでも `control` の円面から浮く)。
        color: p.handle_active,
    };
    ui.push_lines(LineBatch {
        segments: vec![indicator].into(),
        line_width_px: INDICATOR_WIDTH_PX * lw,
        clip_rect: None,
    });
}

/// anchor からの縦 drag 量を value/depth ドメインの delta に変換する (base / depth 共用)。
/// 上 (py < anchor_y) で増加、 下で減少 (= DAW 慣習)。 `units_per_px` は base なら `1/rect.h`、
/// depth なら `ModEdit::depth_sensitivity` (None で base と同値)。 Ctrl で `FINE_DRAG_SCALE` 倍精細。
fn knob_drag_delta(anchor_y: f32, py: f32, units_per_px: f32, ctrl: bool) -> f64 {
    let scale = if ctrl { FINE_DRAG_SCALE } else { 1.0 };
    f64::from(-(py - anchor_y) * units_per_px * scale)
}

/// `Some(range)` かつ `min <= max` のとき clamp、 それ以外 (`None` / 反転 bound / 非有限 bound) は
/// そのまま素通し。 `f64::clamp` は `min > max` や NaN bound で **panic** するため、 caller の depth_range
/// 取り違えで widget を crash させないよう防御する (#109 review、 scrubable_number の同名 helper より堅牢)。
fn clamp_opt(v: f64, range: Option<(f64, f64)>) -> f64 {
    match range {
        Some((min, max)) if min <= max => v.clamp(min, max),
        _ => v,
    }
}

/// 中心 `(cx, cy)`・半径 `radius` の円弧を `a0`→`a1` (rad、 12 時起点・時計回り正) で polygon 近似して
/// line segment で push する。 角度ステップ 2° (draw_knob の value 弧と同じ近似)。 depth 0 = 弧なし。
#[allow(clippy::too_many_arguments)]
fn push_arc<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    cx: f32,
    cy: f32,
    radius: f32,
    a0: f32,
    a1: f32,
    color: Color,
    line_width_px: f32,
) {
    let (lo, hi) = (a0.min(a1), a0.max(a1));
    if hi - lo < 1e-4 {
        return;
    }
    // 刻みは **半径に反比例** させて 1 segment の弦長を `ARC_CHORD_PX` 前後に保つ。
    //
    // 旧実装は半径によらず固定 2° だった。 半径 14px の knob では 1 segment の弦長が
    // `14 × 2° = 0.49px` = **1 pixel 未満の細切れ quad** になり、 rasterizer が
    // pixel 中心を拾えず **弧に穴が空く**。 値弧 / track 弧では下地が同じ knob 本体なので
    // 斑点として微かに出るだけだったが、 可動範囲外を面の色で塗り潰す用途では
    // その穴から本体の縁と枠が漏れて「切れて見えない」 という実害になった。
    //
    // 刻みは span を **均等割り** する (`span / ceil(span / target)`)。 単純に target を
    // 足し込むと最後に「余り」の短い segment が 1 本出て、 そこだけ sub-pixel になる。
    let span = hi - lo;
    let target = (ARC_CHORD_PX / radius.max(1.0)).clamp(ARC_STEP_MIN, ARC_STEP_MAX);
    let n = (span / target).ceil().max(1.0);
    let step = span / n;
    // line pipeline の quad は cap を持たない butt 継ぎなので、 joint では外側に
    // 微小な楔形の隙間が残る。 各 segment の終端を半 step 伸ばして重ね、 隙間を無くす
    // (不透明色では完全に不可視、 半透明弧でも 1px 未満の重なりに収まる)。
    let overlap = step * 0.5;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let count = n as usize; // span <= 2π / target >= 2° より高々 180
    let mut segs: Vec<LineSegment> = Vec::with_capacity(count);
    for i in 0..count {
        let a = lo + step * i as f32;
        let b = (a + step + overlap).min(hi);
        segs.push(LineSegment {
            a: [cx + a.sin() * radius, cy - a.cos() * radius],
            b: [cx + b.sin() * radius, cy - b.cos() * radius],
            color,
        });
    }
    if !segs.is_empty() {
        ui.push_lines(LineBatch { segments: segs.into(), line_width_px, clip_rect: None });
    }
}

/// modulation の色弧 (リング上、 base 角 → base+depth 角) + live 半径マーク + depth-edit 枠/弧強調を
/// 描く (daw_01 #109、 scrubable_number の `draw_modulation_overlay` の knob 版)。
///
/// cache node の **後** に毎フレーム呼ばれる overlay (live_value 30Hz / depth drag 追従でも bg/arc/
/// indicator の cache を無効化しない)。 値ドメインは knob と同じ 0..=1 正規化で、 角度は value=0 →
/// -150° (7時)、 value=1 → +150° (5時) の 300° sweep に写す (= 円弧 = 0..=1 そのもの、 scrubable と
/// 違い range 引数不要)。 非有限値は 7 時に丸めて renderer に NaN 座標を渡さない。
#[allow(clippy::too_many_arguments)]
fn draw_knob_modulation_overlay<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    base_value: f32,
    edit_depth: f64,
    entries: &[ModEntry],
    live_value: Option<f64>,
    edit_color: Option<Color>,
) {
    let size = rect.w.min(rect.h);
    let cx = rect.x + rect.w * 0.5;
    let cy = rect.y + rect.h * 0.5;
    let r = (size * 0.5 - 2.0).max(2.0);
    // 本体描画と同じ 300° sweep 写像 (SSoT = `value_angle`)。 f64 の非有限も f32 cast 後に
    // 非有限のまま残るので、 `value_angle` の guard がそのまま効く。
    let angle_of = |v: f64| -> f32 { value_angle(v as f32) };
    let base_angle = angle_of(f64::from(base_value));

    // depth-edit 中の枠強調 (entries / live が無くても出す、 source 色の円周枠)。
    if let Some(c) = edit_color {
        ui.push_rect(RectCommand {
            rect: Rect { x: cx - r, y: cy - r, w: r * 2.0, h: r * 2.0 },
            fill: Color::TRANSPARENT,
            border: c,
            border_width: 1.5,
            radius: [r; 4],
            clip_rect: None,
        });
    }

    // 各 source の色弧 (base 角 → base+depth 角)。 複数は内側へ同心円状に分割 (= リング帯を分割)。
    let arc_lw = (r * 0.12).clamp(2.0, 4.0);
    let arc_gap = 1.5_f32;
    let band_top = (r - arc_lw - 1.0).max(3.0); // main value arc (radius r、 lw 4) の内側
    for (i, e) in entries.iter().enumerate() {
        let ri = band_top - i as f32 * (arc_lw + arc_gap);
        if ri < 3.0 {
            break; // 同心円の radial 余地が尽きた (= 描けない source は省略、 caller が知るべき制約)
        }
        let end_angle = angle_of(f64::from(base_value) + e.depth);
        push_arc(ui, cx, cy, ri, base_angle, end_angle, Color { a: 0.95, ..e.color }, arc_lw);
    }

    // depth-edit 中: 編集中 depth を source 色で band_top に重ね描き (drag の live feedback、 太め)。
    if let Some(c) = edit_color {
        let end_angle = angle_of(f64::from(base_value) + edit_depth);
        push_arc(ui, cx, cy, band_top, base_angle, end_angle, Color { a: 0.9, ..c }, arc_lw + 1.0);
    }

    // live 変調値の可動半径マーク (最前面、 明るい指針)。 base の中立指針と別色 (amber) で区別。
    if let Some(lv) = live_value {
        let la = angle_of(lv);
        let dx = la.sin();
        let dy = -la.cos();
        let r_in = r * 0.45;
        ui.push_lines(LineBatch {
            segments: vec![LineSegment {
                a: [cx + dx * r_in, cy + dy * r_in],
                b: [cx + dx * r, cy + dy * r],
                // modulation の live 出力値マーク (amber、 base の中立指針と区別。
                // r.md #48 で専用トークン化)。線なので alpha は載せない。
                color: ui.palette().modulation_live.with_alpha(1.0),
            }]
            .into(),
            // 弧 (`arc_lw`) と同じく径に追従させる。小径ノブで指針だけ太いと
            // 「値がどこか」より線そのものが目立つ。
            line_width_px: (arc_lw * 0.7).max(1.0),
            clip_rect: None,
        });
    }
}

#[cfg(test)]
mod tests {
    //! knob の双方向挙動テスト (fader.rs と同形式、knob は rect 全体が hit area)。

    use std::thread;
    use std::time::Duration;

    use daw_ui_platform::{Modifiers, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::*;
    use crate::FrameInput;
    use crate::input::PointerFrame;
    use crate::ui::UiHost;

    struct PanModel {
        value: f32,
    }

    fn knob_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, w: 64.0, h: 64.0 }
    }

    /// rect 内の任意の中心点 (knob は full rect が hit area)。
    fn knob_center() -> (f32, f32) {
        (32.0, 32.0)
    }

    fn run_frame(
        host: &mut UiHost<PanModel>,
        model: &PanModel,
        rect: Rect,
        value: f32,
        default_value: f32,
        pointer: PointerFrame,
    ) -> Vec<Edit<PanModel>> {
        run_frame_styled(host, model, rect, value, default_value, &KnobStyle::default(), pointer)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_frame_styled(
        host: &mut UiHost<PanModel>,
        model: &PanModel,
        rect: Rect,
        value: f32,
        default_value: f32,
        style: &KnobStyle,
        pointer: PointerFrame,
    ) -> Vec<Edit<PanModel>> {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        host.frame_to_edits(
            model,
            &mut scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.knob_at("test", rect, value, default_value, style, |v| {
                    Edit::mutate(move |m: &mut PanModel| m.value = v)
                }, None);
            },
        )
    }

    fn press_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_pressed: true,
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn hold_at(pos: (f32, f32), ctrl: bool) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_pressed: true,
            modifiers: Modifiers { ctrl, ..Modifiers::default() },
            ..PointerFrame::default()
        }
    }

    fn release_at(pos: (f32, f32)) -> PointerFrame {
        PointerFrame {
            pos: Some(pos),
            primary_just_released: true,
            ..PointerFrame::default()
        }
    }

    /// 指定 size の knob を 1 フレーム描き、 描かれた線分をすべて集める。
    fn draw_to_scene(size: f32, value: f32, style: &KnobStyle) -> (Scene, f32, f32, f32) {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let model = PanModel { value };
        let mut scene = Scene::new();
        let rect = Rect { x: 0.0, y: 0.0, w: size, h: size };
        host.frame_to_edits(
            &model,
            &mut scene,
            PhysicalSize { width: 200, height: 200 },
            FrameInput::default(),
            |_, ui| {
                ui.knob_at("test", rect, value, 0.5, style, |v| {
                    Edit::mutate(move |m: &mut PanModel| m.value = v)
                }, None);
            },
        );
        let r = (size * 0.5 - 2.0).max(2.0);
        (scene, size * 0.5, size * 0.5, r)
    }

    /// 弧の折れ線近似は **1 segment を 1px 未満にしない**。
    ///
    /// 旧実装は半径によらず固定 2° 刻みで、 半径 7px (= mixer の send ミニ knob 18px) では
    /// 1 segment の弦長が 0.24px しかなかった。 line pipeline の quad は cap を持たない
    /// 素の矩形なので、 sub-pixel の quad は rasterizer が pixel 中心を拾えず **弧に穴が空く**
    /// (可動範囲外を面の色で塗る用途では、 その穴から本体の縁と枠が漏れる)。
    #[test]
    fn arc_segments_never_go_below_one_pixel() {
        // 18px = mixer の send ミニ knob (半径 7px)、 32px = pan knob (半径 14px)。
        for size in [18.0_f32, 32.0] {
            let (scene, ..) = draw_to_scene(size, 0.8, &KnobStyle::BIPOLAR);
            let mut shortest = f32::INFINITY;
            for batch in scene.iter_lines() {
                for seg in batch.segments.iter() {
                    let len = (seg.b[0] - seg.a[0]).hypot(seg.b[1] - seg.a[1]);
                    shortest = shortest.min(len);
                }
            }
            assert!(
                shortest >= 1.0,
                "size {size}px: 最短の線分が {shortest}px (1px 未満の quad は rasterize で落ちる)"
            );
        }
    }

    /// 可動範囲 (300°) の **外側** 60° が `KnobStyle::surface` の色で塗られ、 その色が
    /// 可動範囲の内側には一切乗らないこと。 これが無いと下の 60° に円本体の縁と枠が
    /// 見えたままで、 リングが 1 周しているように読める (daw_01: pan ノブの可動範囲が
    /// 分からない、 という報告)。
    #[test]
    fn out_of_range_span_is_filled_with_the_surface_color() {
        // palette に無い識別色。 これで塗られた線分 = くり抜きの弧。
        let surface = Color::rgb(0.123, 0.456, 0.789);
        let style = KnobStyle { surface: Some(surface), ..KnobStyle::BIPOLAR };
        let (scene, cx, cy, _r) = draw_to_scene(32.0, 0.8, &style);

        let mut found = 0;
        for batch in scene.iter_lines() {
            for seg in batch.segments.iter().filter(|s| s.color == surface) {
                found += 1;
                for pt in [seg.a, seg.b] {
                    // widget の角度写像 (x = sin, y = -cos) の逆。
                    let theta = (pt[0] - cx).atan2(-(pt[1] - cy));
                    assert!(
                        theta.abs() >= 0.5 * SWEEP - 1e-3,
                        "くり抜きの弧が可動範囲の内側 ({}°) に入っている",
                        theta.to_degrees()
                    );
                }
            }
        }
        assert!(found > 0, "surface 色の弧が 1 本も描かれていない");
    }

    #[test]
    fn double_click_within_threshold_resets_to_default() {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let mut model = PanModel { value: 0.8 };
        let rect = knob_rect();
        let c = knob_center();

        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, release_at(c));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.5).abs() < 1e-5,
            "ダブルクリックで default_value=0.5 (pan center) にリセットされるべき (got {})",
            model.value
        );
    }

    #[test]
    fn click_after_threshold_does_not_reset() {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let mut model = PanModel { value: 0.8 };
        let rect = knob_rect();
        let c = knob_center();

        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, release_at(c));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(350));

        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.8).abs() < 1e-5,
            "閾値超過の 2 回目 press はリセットを起こさない (got {})",
            model.value
        );
    }

    #[test]
    fn click_far_position_does_not_reset() {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let mut model = PanModel { value: 0.8 };
        let rect = knob_rect();
        let c = knob_center();

        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, release_at(c));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 10px 離れた rect 内座標
        let far = (c.0 + 10.0, c.1);
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(far, false));
        for e in edits { e.apply(&mut model); }

        assert!(
            (model.value - 0.8).abs() < 1e-5,
            "10px 離れた 2 回目 press はリセットを起こさない (got {})",
            model.value
        );
    }

    #[test]
    fn ctrl_drag_uses_one_tenth_sensitivity() {
        let mut host_n: UiHost<PanModel> = UiHost::no_redraw();
        let mut model_n = PanModel { value: 0.5 };
        let rect = knob_rect();
        let c = knob_center();

        let edits = run_frame(&mut host_n, &model_n, rect, model_n.value, 0.0, press_at(c, false));
        for e in edits { e.apply(&mut model_n); }
        let edits = run_frame(&mut host_n, &model_n, rect, model_n.value, 0.0,
            hold_at((c.0, c.1 - 20.0), false));
        for e in edits { e.apply(&mut model_n); }
        let normal_delta = model_n.value - 0.5;

        let mut host_c: UiHost<PanModel> = UiHost::no_redraw();
        let mut model_c = PanModel { value: 0.5 };
        let edits = run_frame(&mut host_c, &model_c, rect, model_c.value, 0.0, press_at(c, true));
        for e in edits { e.apply(&mut model_c); }
        let edits = run_frame(&mut host_c, &model_c, rect, model_c.value, 0.0,
            hold_at((c.0, c.1 - 20.0), true));
        for e in edits { e.apply(&mut model_c); }
        let fine_delta = model_c.value - 0.5;

        assert!(normal_delta > 0.0);
        assert!(fine_delta > 0.0);
        let ratio = fine_delta / normal_delta;
        assert!(
            (ratio - 0.1).abs() < 1e-3,
            "Ctrl+drag は 1/10 感度 (ratio={ratio}, normal={normal_delta}, fine={fine_delta})",
        );
    }

    #[test]
    fn mid_drag_ctrl_toggle_does_not_jump() {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let mut model = PanModel { value: 0.5 };
        let rect = knob_rect();
        let c = knob_center();

        let edits = run_frame(&mut host, &model, rect, model.value, 0.0, press_at(c, false));
        for e in edits { e.apply(&mut model); }

        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((c.0, c.1 - 20.0), false));
        for e in edits { e.apply(&mut model); }
        let after_normal = model.value;

        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((c.0, c.1 - 20.0), true));
        for e in edits { e.apply(&mut model); }
        assert!(
            (model.value - after_normal).abs() < 1e-5,
            "Ctrl 押下のみで値が変わらない (before={}, after={})",
            after_normal, model.value,
        );

        let edits = run_frame(&mut host, &model, rect, model.value, 0.0,
            hold_at((c.0, c.1 - 40.0), true));
        for e in edits { e.apply(&mut model); }
        let after_fine = model.value;

        let expected = after_normal + (after_normal - 0.5) * 0.1;
        assert!(
            (after_fine - expected).abs() < 1e-4,
            "再 anchor + 1/10 感度: expected={expected}, got={after_fine}",
        );
    }

    #[test]
    fn triple_click_does_not_reset_again() {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let mut model = PanModel { value: 0.8 };
        let rect = knob_rect();
        let c = knob_center();

        // 1 回目
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, release_at(c));
        for e in edits { e.apply(&mut model); }

        thread::sleep(Duration::from_millis(50));

        // 2 回目: リセット → 0.5
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, release_at(c));
        for e in edits { e.apply(&mut model); }
        assert!((model.value - 0.5).abs() < 1e-5);

        thread::sleep(Duration::from_millis(50));

        // 3 回目: rect 全体が hit area なので thumb が動かない knob でも同じ位置で OK。
        // last_click は 2 回目で None になっているので drag 開始扱い。
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5, press_at(c, false));
        for e in edits { e.apply(&mut model); }

        // hold-move で値が動くなら drag が active。
        let edits = run_frame(&mut host, &model, rect, model.value, 0.5,
            hold_at((c.0, c.1 - 20.0), false));
        for e in edits { e.apply(&mut model); }

        assert!(
            model.value > 0.5 + 1e-3,
            "3 回目 click は drag を開始する (move で値が増えるはず): value={}",
            model.value
        );
    }

    // ---- r.md #47: センタ吸着 (KnobStyle::detent) ----
    //
    // base 感度 = KNOB_UNITS_PER_PX (0.004/px = 250px で端から端)。 plateau =
    // DETENT_TOTAL_PX (32px) × 0.004 = 0.128 値単位、 片側 0.064 (16px)。 吸着先 = default_value = 0.5。

    /// press → 下に `down_px` ドラッグしたときの値 (BIPOLAR = detent 有効)。
    fn drag_from(start: f32, down_px: f32, style: &KnobStyle) -> f32 {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let mut model = PanModel { value: start };
        let rect = knob_rect();
        let c = knob_center();
        for e in run_frame_styled(&mut host, &model, rect, model.value, 0.5, style, press_at(c, false)) {
            e.apply(&mut model);
        }
        let edits = run_frame_styled(
            &mut host,
            &model,
            rect,
            model.value,
            0.5,
            style,
            hold_at((c.0, c.1 + down_px), false),
        );
        for e in edits {
            e.apply(&mut model);
        }
        model.value
    }

    /// plateau の外 (掴んだ側) では detent が値を歪めない = 素の drag と同値。
    #[test]
    fn detent_does_not_distort_outside_plateau() {
        // 0.8 から 25px 下 = -0.1 → 0.7 (plateau [0.436, 0.564] の外)。
        let v = drag_from(0.8, 25.0, &KnobStyle::BIPOLAR);
        assert!((v - 0.7).abs() < 1e-5, "plateau 外は素の drag と同値 (got {v})");
    }

    /// default_value を通り越すドラッグは、 そこで **ぴったり張り付く** (素の drag なら 0.45)。
    #[test]
    fn detent_holds_exactly_at_default_when_crossing() {
        // 91px 下げ = 素なら 0.436。 detent では plateau 内なのでちょうど 0.5。
        let v = drag_from(0.8, 91.0, &KnobStyle::BIPOLAR);
        assert!((v - 0.5).abs() < 1e-6, "センタに正確に吸着する (got {v})");
    }

    /// plateau を食い切ると離脱し、 写像は連続 (= 素の値 + plateau)。
    #[test]
    fn detent_escapes_after_consuming_plateau() {
        // 0.8 から 125px 下 = -0.5 → 素なら 0.3、 detent は plateau 0.128 を食った分ずれて 0.428。
        let v = drag_from(0.8, 125.0, &KnobStyle::BIPOLAR);
        assert!(v < 0.5 - 1e-6, "plateau を超えたら離脱する (got {v})");
        assert!((v - 0.428).abs() < 1e-5, "離脱後は連続 (素 0.3 + plateau 0.128) (got {v})");
    }

    /// センタを掴んだ場合、 上下どちらへも半幅ぶん粘る (対称) — 1px では動かない。
    #[test]
    fn detent_from_center_is_symmetric() {
        for px in [1.0_f32, -1.0] {
            let v = drag_from(0.5, px, &KnobStyle::BIPOLAR);
            assert!((v - 0.5).abs() < 1e-6, "{px}px では動かない (got {v})");
        }
        // 半幅 (16px) を超えれば両方向へ離脱する。
        assert!(drag_from(0.5, 20.0, &KnobStyle::BIPOLAR) < 0.5, "下へ離脱");
        assert!(drag_from(0.5, -20.0, &KnobStyle::BIPOLAR) > 0.5, "上へ離脱");
    }

    /// `detent: false` (unipolar) は default_value 上でも一切粘らない (回帰保証)。
    #[test]
    fn no_detent_when_disabled() {
        let v = drag_from(0.8, 91.0, &KnobStyle::UNIPOLAR);
        assert!((v - 0.436).abs() < 1e-5, "detent 無効なら素の drag (got {v})");
    }

    // ---- r.md #47: 値弧の起点 (KnobStyle::arc_origin) の幾何 ----
    //
    // knob rect は 64×64 @ (0,0) なので中心 (32, 32)、 弧半径 r = 64/2 - 2 = 30。
    // 12 時の点 = (32, 2)、 7 時 (value 0) の点 = (32 + sin(-150°)·30, 32 - cos(-150°)·30)。

    const CX: f32 = 32.0;
    const CY: f32 = 32.0;
    const R: f32 = 30.0;

    /// 指定 value / style で 1 frame 描画して scene を返す (入力なし・pointer 無し)。
    fn render_knob(value: f32, style: &KnobStyle) -> Scene {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let model = PanModel { value };
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
            ui.knob_at(
                "test",
                knob_rect(),
                value,
                0.5,
                style,
                |v| Edit::mutate(move |m: &mut PanModel| m.value = v),
                None,
            );
        });
        scene
    }

    /// scene 中の指定色の line segment を全部集める (値弧 = ACCENT、 notch = TEXT_DIM で識別)。
    fn segments_colored(scene: &Scene, color: Color) -> Vec<LineSegment> {
        scene
            .iter_lines()
            .flat_map(|b| b.segments.iter().copied())
            .filter(|s| {
                (s.color.r - color.r).abs() < 1e-3
                    && (s.color.g - color.g).abs() < 1e-3
                    && (s.color.b - color.b).abs() < 1e-3
            })
            .collect()
    }

    /// 与えた点を端点に持つ segment があるか (弧の端 = 起点/現在値の確認用)。
    fn touches(segs: &[LineSegment], p: (f32, f32)) -> bool {
        segs.iter().any(|s| {
            ((s.a[0] - p.0).abs() < 0.05 && (s.a[1] - p.1).abs() < 0.05)
                || ((s.b[0] - p.0).abs() < 0.05 && (s.b[1] - p.1).abs() < 0.05)
        })
    }

    /// bipolar (pan) のセンタでは値弧が 1 本も描かれない (= 「フルレフトが零点」 の解消)。
    /// 零点の位置は notch が示す。
    #[test]
    fn bipolar_center_draws_no_value_arc_but_a_notch() {
        let scene = render_knob(0.5, &KnobStyle::BIPOLAR);
        let arc = segments_colored(&scene, crate::theme::Palette::dark().accent);
        assert!(arc.is_empty(), "センタで値弧は 0 本 (got {} 本)", arc.len());
        let notch = segments_colored(&scene, crate::theme::Palette::dark().text_dim);
        assert_eq!(notch.len(), 1, "起点 notch が 1 本描かれる (got {})", notch.len());
        // notch は 12 時の radial 線 (x はセンタ、 y は弧の内側 → 外側)。
        let n = notch[0];
        assert!((n.a[0] - CX).abs() < 0.05 && (n.b[0] - CX).abs() < 0.05, "notch は 12 時 (got {n:?})");
        assert!(n.a[1] > n.b[1], "notch は内側 → 外側 (上向き) に伸びる (got {n:?})");
    }

    /// センタから 1e-4 しかずれていない値でも塗らない (dead band): 丸め誤差の 1px 欠片で
    /// 「センタなのに片側が光る」 のを防ぐ。
    #[test]
    fn bipolar_near_center_stays_unfilled() {
        for v in [0.5_f32 + 1e-4, 0.5 - 1e-4] {
            let scene = render_knob(v, &KnobStyle::BIPOLAR);
            let arc = segments_colored(&scene, crate::theme::Palette::dark().accent);
            assert!(arc.is_empty(), "value={v} (dead band 内) で値弧は 0 本 (got {} 本)", arc.len());
        }
    }

    /// 同一 host / 同一 id で style だけ差し替えた 2 frame 目が **新しい起点** で描かれる
    /// (= `arc_origin` が input_hash に入っている証明。 漏れていると cache HIT で 1 frame 目の
    /// unipolar 弧が描かれ続ける)。
    #[test]
    fn arc_origin_change_invalidates_draw_cache() {
        let mut host: UiHost<PanModel> = UiHost::no_redraw();
        let model = PanModel { value: 0.25 };
        let screen = PhysicalSize { width: 200, height: 200 };
        let draw = |host: &mut UiHost<PanModel>, style: &KnobStyle| -> Scene {
            let mut scene = Scene::new();
            host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |_, ui| {
                ui.knob_at(
                    "test",
                    knob_rect(),
                    0.25,
                    0.5,
                    style,
                    |v| Edit::mutate(move |m: &mut PanModel| m.value = v),
                    None,
                );
            });
            scene
        };
        let f1 = draw(&mut host, &KnobStyle::UNIPOLAR);
        let seven = (
            CX + (-150.0_f32).to_radians().sin() * R,
            CY - (-150.0_f32).to_radians().cos() * R,
        );
        assert!(touches(&segments_colored(&f1, crate::theme::Palette::dark().accent), seven), "1 frame 目は 7 時起点");

        let f2 = draw(&mut host, &KnobStyle::BIPOLAR);
        let arc = segments_colored(&f2, crate::theme::Palette::dark().accent);
        assert!(touches(&arc, (CX, CY - R)), "2 frame 目は 12 時起点で描き直される");
        assert!(!touches(&arc, seven), "cache HIT で 1 frame 目 (7 時起点) の弧が残らない");
    }

    /// bipolar で L 側 (value < 0.5) は **センタから左へ** 弧が伸びる (左端からではない)。
    #[test]
    fn bipolar_arc_grows_from_center_to_left() {
        let scene = render_knob(0.25, &KnobStyle::BIPOLAR);
        let arc = segments_colored(&scene, crate::theme::Palette::dark().accent);
        assert!(!arc.is_empty(), "L 側で値弧が描かれる");
        assert!(
            touches(&arc, (CX, CY - R)),
            "弧の起点は 12 時 (32, 2) にある (got {:?})",
            arc.iter().map(|s| (s.a, s.b)).collect::<Vec<_>>(),
        );
        for s in &arc {
            assert!(
                s.a[0] <= CX + 0.05 && s.b[0] <= CX + 0.05,
                "L 側の弧はセンタより左だけ (got {s:?})",
            );
        }
    }

    /// bipolar で R 側 (value > 0.5) は **センタから右へ** 弧が伸びる。
    #[test]
    fn bipolar_arc_grows_from_center_to_right() {
        let scene = render_knob(0.75, &KnobStyle::BIPOLAR);
        let arc = segments_colored(&scene, crate::theme::Palette::dark().accent);
        assert!(!arc.is_empty(), "R 側で値弧が描かれる");
        assert!(touches(&arc, (CX, CY - R)), "弧の起点は 12 時 (32, 2)");
        for s in &arc {
            assert!(
                s.a[0] >= CX - 0.05 && s.b[0] >= CX - 0.05,
                "R 側の弧はセンタより右だけ (got {s:?})",
            );
        }
    }

    /// unipolar (送り量 / 音量) は従来どおり **最小値 (7 時) 起点**、 notch 無し (回帰保証)。
    #[test]
    fn unipolar_arc_grows_from_minimum_without_notch() {
        let scene = render_knob(0.5, &KnobStyle::UNIPOLAR);
        let arc = segments_colored(&scene, crate::theme::Palette::dark().accent);
        let seven = (CX + (-150.0_f32).to_radians().sin() * R, CY - (-150.0_f32).to_radians().cos() * R);
        assert!(touches(&arc, seven), "弧の起点は 7 時 {seven:?}");
        assert!(touches(&arc, (CX, CY - R)), "value 0.5 まで = 12 時 (32, 2) まで届く");
        assert!(
            segments_colored(&scene, crate::theme::Palette::dark().text_dim).is_empty(),
            "unipolar は起点 notch を描かない (起点 = 可動範囲の端)",
        );
    }

    // ---- daw_01 #109: Bitwig 流 modulation (knob 版、 値も depth も 0..=1 正規化ドメイン) ----

    use crate::widgets::scrubable_number::ModEdit;

    /// base value (f32) と depth (f64) を別々に持つ test model。
    struct ModModel {
        value: f32,
        depth: f64,
    }

    /// modulation 付き 1 frame を描画 + 処理し、 edits と response を返す (rect = 64×64 knob、
    /// base 感度 = KNOB_UNITS_PER_PX = 0.004 units/px)。
    #[allow(clippy::too_many_arguments)]
    fn run_mod_frame(
        host: &mut UiHost<ModModel>,
        model: &ModModel,
        rect: Rect,
        pointer: PointerFrame,
        edit_mode: bool,
        entries: &[ModEntry],
        live_value: Option<f64>,
        scene: &mut Scene,
    ) -> (Vec<Edit<ModModel>>, KnobResponse) {
        let screen = PhysicalSize { width: 200, height: 200 };
        let base = model.value;
        let cur_depth = model.depth;
        let resp_cell: std::cell::RefCell<KnobResponse> =
            std::cell::RefCell::new(KnobResponse::default());
        let edits = host.frame_to_edits(
            model,
            scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let edit_desc = edit_mode.then_some(ModEdit {
                    source_color: Color::rgb(0.2, 0.9, 0.4),
                    current_depth: cur_depth,
                    depth_range: Some((-1.0, 1.0)),
                    depth_sensitivity: None,
                    on_mod_change: &on_mod,
                });
                let modulation = Modulation { entries, live_value, edit: edit_desc };
                let r = ui.knob_at(
                    "mtest",
                    rect,
                    base,
                    0.5,
                    &KnobStyle::default(),
                    |v| Edit::mutate(move |m: &mut ModModel| m.value = v),
                    Some(modulation),
                );
                *resp_cell.borrow_mut() = r;
            },
        );
        (edits, resp_cell.into_inner())
    }

    /// arm 中 (edit_mode) の press + 縦 drag は **depth** を変化させ、 base value は触らない (非破壊)。
    #[test]
    fn mod_edit_drag_changes_depth_not_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let c = knob_center();

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(c, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // drag up 32px → depth = 0 + 32×0.004 = 0.128。 value は不変。
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, hold_at((c.0, c.1 - 32.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.depth - 0.128).abs() < 1e-5, "depth scrub +0.128 (got {})", model.depth);
        assert!((model.value - 0.5).abs() < 1e-5, "base value は depth-edit 中 不変 (got {})", model.value);
        assert!(resp.mod_dragging, "depth drag 中は mod_dragging=true");
        assert!(!resp.dragging, "depth drag 中は base dragging=false (排他)");
    }

    /// 非 arm (edit_mode=false) の drag は従来どおり base value を scrub し、 depth は触らない。
    #[test]
    fn non_arm_drag_scrubs_base_only() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.25 };
        let rect = knob_rect();
        let c = knob_center();

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(c, false), false, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // drag up 32px → value 0.5 + 32×0.004 = 0.628
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, hold_at((c.0, c.1 - 32.0), false), false, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.value - 0.628).abs() < 1e-5, "base scrub +0.128 → 0.628 (got {})", model.value);
        assert!((model.depth - 0.25).abs() < 1e-5, "非 arm では depth 不変 (got {})", model.depth);
        assert!(resp.dragging, "非 arm は base dragging=true");
        assert!(!resp.mod_dragging, "非 arm は mod_dragging=false");
    }

    /// arm 中 dblclick は base value の default reset を発火しない (非破壊)。
    #[test]
    fn mod_edit_dblclick_does_not_reset_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.8, depth: 0.0 };
        let rect = knob_rect();
        let c = knob_center();

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(c, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, release_at(c), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        thread::sleep(Duration::from_millis(50));
        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(c, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }

        assert!((model.value - 0.8).abs() < 1e-5, "arm 中 dblclick で base は reset されない (got {})", model.value);
    }

    /// `entries` を渡すと色弧 (line batch) が overlay として追加され、 entry 色で描かれる。 None で出ない。
    #[test]
    fn entries_draw_arcs() {
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let screen = PhysicalSize { width: 200, height: 200 };

        let mut host_n: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_none = Scene::new();
        host_n.frame_to_edits(&model, &mut scene_none, screen, FrameInput::default(), |_, ui| {
            ui.knob_at("mtest", rect, 0.5, 0.5, &KnobStyle::default(),
                |v| Edit::mutate(move |m: &mut ModModel| m.value = v), None);
        });

        let cyan = Color::rgb(0.2, 0.8, 1.0);
        let entries = [ModEntry { color: cyan, depth: 0.30 }];
        let mut host_s: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_some = Scene::new();
        run_mod_frame(&mut host_s, &model, rect, PointerFrame::default(), false, &entries, None, &mut scene_some);

        assert!(
            scene_some.line_count() > scene_none.line_count(),
            "entries で arc line batch が増える (none={}, some={})",
            scene_none.line_count(), scene_some.line_count(),
        );
        assert!(
            scene_some.iter_lines().any(|b| b.segments.iter().any(|s| {
                (s.color.r - cyan.r).abs() < 1e-3
                    && (s.color.g - cyan.g).abs() < 1e-3
                    && (s.color.b - cyan.b).abs() < 1e-3
            })),
            "entry 色の弧 segment が描かれる",
        );
    }

    /// `live_value` を渡すと可動半径マーク (line batch) が 1 本追加される。
    #[test]
    fn live_value_draws_mark() {
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();

        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_no = Scene::new();
        run_mod_frame(&mut host, &model, rect, PointerFrame::default(), false, &[], None, &mut scene_no);

        let mut host2: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_live = Scene::new();
        run_mod_frame(&mut host2, &model, rect, PointerFrame::default(), false, &[], Some(0.7), &mut scene_live);

        assert!(
            scene_live.line_count() > scene_no.line_count(),
            "live_value で mark line batch が増える (no={}, live={})",
            scene_no.line_count(), scene_live.line_count(),
        );
    }

    /// depth gesture の release frame で pointer が動いた最終位置の depth が確定発火する。
    #[test]
    fn mod_edit_release_commits_final_depth() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let c = knob_center();

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(c, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // hold up 16px → depth 16×0.004 = 0.064
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, hold_at((c.0, c.1 - 16.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 0.064).abs() < 1e-5, "hold で depth 0.064 (got {})", model.depth);

        // release は更に上 (-32px) で離す → 最終 depth 0.128 が release frame で確定。
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, release_at((c.0, c.1 - 32.0)), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 0.128).abs() < 1e-5, "release frame で最終 depth 0.128 確定 (got {})", model.depth);
    }

    /// `depth_sensitivity: Some` は depth drag で knob の base 感度 (1/rect.h) を上書きする。
    #[test]
    fn depth_sensitivity_overrides_base() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let c = knob_center();
        let screen = PhysicalSize { width: 200, height: 200 };

        let run = |host: &mut UiHost<ModModel>, model: &ModModel, pointer: PointerFrame| -> Vec<Edit<ModModel>> {
            let cur = model.depth;
            host.frame_to_edits(model, &mut Scene::new(), screen, FrameInput { pointer, ..Default::default() }, |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let m = Modulation {
                    entries: &[],
                    live_value: None,
                    edit: Some(ModEdit {
                        source_color: Color::WHITE,
                        current_depth: cur,
                        depth_range: Some((-2.0, 2.0)),
                        depth_sensitivity: Some(0.1), // 0.1 units/px (base 0.004 を上書き)
                        on_mod_change: &on_mod,
                    }),
                };
                ui.knob_at("mtest", rect, model.value, 0.5, &KnobStyle::default(),
                    |v| Edit::mutate(move |m: &mut ModModel| m.value = v), Some(m));
            })
        };

        for e in run(&mut host, &model, press_at(c, false)) { e.apply(&mut model); }
        // drag up 10px × depth_sensitivity 0.1 = 1.0 (base 感度 0.004 なら 0.04)。
        for e in run(&mut host, &model, hold_at((c.0, c.1 - 10.0), false)) { e.apply(&mut model); }
        assert!((model.depth - 1.0).abs() < 1e-5, "depth_sensitivity 0.1 で +1.0 (got {})", model.depth);
    }

    /// `Some` でも entries 空 + live None + edit None なら overlay 描画差分なし (None と同 primitive 数)。
    #[test]
    fn empty_modulation_draws_no_overlay() {
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let screen = PhysicalSize { width: 200, height: 200 };

        let mut host_n: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_none = Scene::new();
        host_n.frame_to_edits(&model, &mut scene_none, screen, FrameInput::default(), |_, ui| {
            ui.knob_at("mtest", rect, 0.5, 0.5, &KnobStyle::default(),
                |v| Edit::mutate(move |m: &mut ModModel| m.value = v), None);
        });

        let mut host_e: UiHost<ModModel> = UiHost::no_redraw();
        let mut scene_empty = Scene::new();
        run_mod_frame(&mut host_e, &model, rect, PointerFrame::default(), false, &[], None, &mut scene_empty);

        assert_eq!(
            scene_empty.line_count(), scene_none.line_count(),
            "empty Some は None と同じ line batch 数 (overlay 描画差分なし)",
        );
        assert_eq!(
            scene_empty.rect_count(), scene_none.rect_count(),
            "empty Some は None と同じ rect 数",
        );
    }

    /// 非有限 (NaN/Inf) な depth / live_value を渡しても scene 座標に NaN/Inf を出さない。
    #[test]
    fn nonfinite_values_produce_no_nan() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let entries = [ModEntry { color: Color::WHITE, depth: f64::NAN }];
        let mut scene = Scene::new();
        run_mod_frame(
            &mut host, &model, rect, PointerFrame::default(), false, &entries, Some(f64::INFINITY), &mut scene,
        );
        for r in scene.iter_rects() {
            assert!(
                r.rect.x.is_finite() && r.rect.y.is_finite() && r.rect.w.is_finite() && r.rect.h.is_finite(),
                "rect 座標に NaN/Inf が出ない (got {:?})", r.rect,
            );
        }
        for batch in scene.iter_lines() {
            for s in batch.segments.iter() {
                assert!(
                    s.a[0].is_finite() && s.a[1].is_finite() && s.b[0].is_finite() && s.b[1].is_finite(),
                    "line 座標に NaN/Inf が出ない (got {:?} -> {:?})", s.a, s.b,
                );
            }
        }
    }

    /// knob の depth drag は **縦専用**: 横移動のみ (dx) では depth は変わらない (#109、 scrubable #108 と
    /// 違い knob は横ドラッグ非対応)。
    #[test]
    fn mod_edit_horizontal_drag_has_no_effect() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let c = knob_center();

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(c, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // 横右 40px (dy=0) → 縦移動ゼロなので depth 不変、 mod_dragging も立たない (縦距離 0)。
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, hold_at((c.0 + 40.0, c.1), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 0.0).abs() < 1e-9, "横移動のみで depth は変わらない (got {})", model.depth);
        assert!(!resp.mod_dragging, "縦移動ゼロでは mod_dragging は立たない");
    }

    /// 閾値未満 (< DRAG_THRESHOLD_PX) の press→release は depth Edit を発火せず mod_dragging も立てない
    /// (micro-jitter click 抑止、 #109 review で scrubable と同義に統一)。
    #[test]
    fn mod_edit_subthreshold_click_fires_no_depth() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let c = knob_center();

        let (edits, _) =
            run_mod_frame(&mut host, &model, rect, press_at(c, false), true, &[], None, &mut Scene::new());
        for e in edits { e.apply(&mut model); }
        // 2px だけ上に動いて release (閾値 4px 未満) → hold frame 無しの直接 release。
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, release_at((c.0, c.1 - 2.0)), true, &[], None, &mut Scene::new(),
        );
        let n = edits.len();
        for e in edits { e.apply(&mut model); }
        assert_eq!(n, 0, "閾値未満 click は depth Edit を発火しない (got {n} edits)");
        assert!((model.depth - 0.0).abs() < 1e-9, "depth は変わらない (got {})", model.depth);
        assert!(!resp.mod_dragging, "閾値未満では mod_dragging は立たない");
    }

    /// entries がリングの radial 余地を超えても panic せず graceful に skip する (内側から詰める)。
    #[test]
    fn many_entries_skip_gracefully() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let entries: Vec<ModEntry> = (0..12)
            .map(|i| ModEntry { color: Color::rgb(0.2, 0.8, 1.0), depth: 0.1 * f64::from(i + 1) })
            .collect();
        let mut scene = Scene::new();
        // panic しなければ OK (radial 余地超過分は break で skip)。
        run_mod_frame(&mut host, &model, rect, PointerFrame::default(), false, &entries, None, &mut scene);
        for batch in scene.iter_lines() {
            for s in batch.segments.iter() {
                assert!(
                    s.a[0].is_finite() && s.b[0].is_finite(),
                    "skip 後も座標は有限",
                );
            }
        }
    }

    /// 反転した depth_range (min > max) を渡しても `clamp_opt` が panic しない (#109 review、 防御的素通し)。
    #[test]
    fn inverted_depth_range_does_not_panic() {
        let mut host: UiHost<ModModel> = UiHost::no_redraw();
        let mut model = ModModel { value: 0.5, depth: 0.0 };
        let rect = knob_rect();
        let c = knob_center();
        let screen = PhysicalSize { width: 200, height: 200 };

        let run = |host: &mut UiHost<ModModel>, model: &ModModel, pointer: PointerFrame| -> Vec<Edit<ModModel>> {
            let cur = model.depth;
            host.frame_to_edits(model, &mut Scene::new(), screen, FrameInput { pointer, ..Default::default() }, |_, ui| {
                let on_mod = |d: f64| Edit::mutate(move |m: &mut ModModel| m.depth = d);
                let m = Modulation {
                    entries: &[],
                    live_value: None,
                    edit: Some(ModEdit {
                        source_color: Color::WHITE,
                        current_depth: cur,
                        depth_range: Some((1.0, -1.0)), // 反転 (caller bug) — f64::clamp なら panic
                        depth_sensitivity: None,
                        on_mod_change: &on_mod,
                    }),
                };
                ui.knob_at("mtest", rect, model.value, 0.5, &KnobStyle::default(),
                    |v| Edit::mutate(move |m: &mut ModModel| m.value = v), Some(m));
            })
        };

        // press → hold で drag (panic しないことが確認できれば OK)。
        for e in run(&mut host, &model, press_at(c, false)) { e.apply(&mut model); }
        for e in run(&mut host, &model, hold_at((c.0, c.1 - 32.0), false)) { e.apply(&mut model); }
        // 反転 range は素通し (clamp なし)、 panic 無く depth が更新される。
        assert!(model.depth.is_finite(), "panic せず depth は有限 (got {})", model.depth);
    }
}
