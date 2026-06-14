//! `knob` ウィジェット — 回転ノブ。ドラッグで値編集 (上下ドラッグ、上 = 増)。
//!
//! - 値範囲: `0.0..=1.0`
//! - 視覚: 7 時の位置から 5 時の位置まで 300° のスイープ (DAW 標準)
//! - drag 感度: rect 高さ分のドラッグで 0 → 1 (fader と同じ感覚)
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
use crate::ui::{Ui, hovered, lerp_color};
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
    /// **M8 Phase 29**: drag 中は Mutate Edit (history 非対象)、drag 終端で `label` 付き
    /// Undoable Edit を発行する DAW 標準動作。`on_change` は `Fn + Clone + Send + Sync + 'static`。
    ///
    /// `default_value` は rect のダブルクリック時にリセットされる値 (例: pan の中央 0.5)。
    /// `label` は undoable history パネルでの表示文字列 ("knob" / "pan" 等)。
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
        label: &'static str,
        on_change: F,
        modulation: Option<Modulation<'_, M>>,
    ) -> KnobResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        let wid = WidgetId::ROOT.child((b"knob", &id));
        let pointer = self.pointer;
        let value = value.clamp(0.0, 1.0);
        let default_value = default_value.clamp(0.0, 1.0);

        // ---- modulation 記述の展開 (None = 完全回帰、 borrow のみ取り出す、 scrubable_number と同形) ----
        let mod_ref = modulation.as_ref();
        let mod_entries: &[ModEntry] = mod_ref.map_or(&[], |m| m.entries);
        let mod_live = mod_ref.and_then(|m| m.live_value);
        let mod_edit = mod_ref.and_then(|m| m.edit.as_ref());
        let depth_mode = mod_edit.is_some();
        let current_depth = mod_edit.map_or(0.0, |e| e.current_depth);
        let depth_range = mod_edit.and_then(|e| e.depth_range);
        // knob の base drag 感度 = rect.h 分で 0→1 (= 1/h units_per_pixel)。 depth も ModEdit 指定が
        // 無ければ同じ感度を流用 (knob 値と depth が同じ 0..=1 スパンなので自然)。
        let base_units_per_px = 1.0 / rect.h.max(1.0);
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
            ((anchor.value + d) as f32).clamp(0.0, 1.0)
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
            dragging,
            hovered_rect,
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            draw_knob(ui, rect, displayed_value, dragging, pointer);
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

        if let Some(start_value) = release_initial_value
            && (start_value - displayed_value).abs() > f32::EPSILON
        {
            let on_change_fwd = on_change.clone();
            let on_change_inv = on_change;
            let end = displayed_value;
            let edit = Edit::with_inverse(
                label,
                move |m: &mut M| on_change_fwd(end).apply(m),
                move |m: &mut M| on_change_inv(start_value).apply(m),
            );
            self.push_edit(edit);
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
        label: &'static str,
        on_change: F,
    ) -> KnobResponse
    where
        F: Fn(f32) -> Edit<M> + Clone + Send + Sync + 'static,
    {
        let pad = 8.0;
        let size = 64.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: size,
            h: size,
        };
        let resp = self.knob_at(id, rect, value, default_value, label, on_change, None);
        self.next_y += size + pad;
        resp
    }
}

fn draw_knob<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    value: f32,
    dragging: bool,
    pointer: crate::input::PointerFrame,
) {
    // 円本体: rect の中央に max-radius の正方形を置いて 4 隅 r で円形に。
    let size = rect.w.min(rect.h);
    let cx = rect.x + rect.w * 0.5;
    let cy = rect.y + rect.h * 0.5;
    let r = (size * 0.5 - 2.0).max(2.0); // 2px の周囲余白
    let circle_rect = Rect { x: cx - r, y: cy - r, w: r * 2.0, h: r * 2.0 };

    // Ableton 流: dark gray の円 + 円周上に cyan の arc。インジケータ線なし。
    // arc は -150° (7時) から value_angle までを円周 (radius = r) 上に描画。
    // 下の 60° (5時 → 7時 経由 6時) は 300° sweep 範囲外で arc は届かない (= "切れている")。
    let base = Color::rgb(0.22, 0.24, 0.28);
    let hover_c = Color::rgb(0.28, 0.30, 0.34);
    let press_c = Color::rgb(0.32, 0.40, 0.52);
    let bg_fill = if dragging {
        press_c
    } else if hovered(rect, pointer) {
        lerp_color(base, hover_c, 0.85)
    } else {
        base
    };

    ui.push_rect(RectCommand {
        rect: circle_rect,
        fill: bg_fill,
        border: Color::rgb(0.40, 0.43, 0.47),
        border_width: 1.0,
        radius: [r; 4],
        clip_rect: None,
    });

    // 角度: value=0 → -150° (7時)、value=0.5 → 0° (12時)、value=1 → +150° (5時)。
    let value_angle = (value - 0.5) * (5.0 * PI / 3.0);
    let start_angle = -150.0_f32 * PI / 180.0;
    let end_angle = 150.0_f32 * PI / 180.0;

    // 可動範囲の弧を 2 色で描く (300° 全部表示):
    // - 回転側 (start → value): cyan = 既に動いた範囲
    // - 非回転側 (value → end): 暗グレー = 残りの可動範囲
    // 6時付近の 60° (5時 → 7時) は範囲外なので空白 = "弧が切れて見える"。
    // 角度ステップを 2° に下げて polygon 近似のコーナーアーティファクトを目立たなく。
    // 300° / 2° = 150 segments per knob、毎フレーム計算でも軽量。
    let step = 2.0_f32 * PI / 180.0;
    let arc_radius = r;
    let active_color = Color::rgb(0.42, 0.85, 0.95);
    let inactive_color = Color::rgb(0.32, 0.34, 0.38);

    let mut active: Vec<LineSegment> = Vec::new();
    let mut a0 = start_angle;
    while a0 < value_angle {
        let a1 = (a0 + step).min(value_angle);
        active.push(LineSegment {
            a: [cx + a0.sin() * arc_radius, cy - a0.cos() * arc_radius],
            b: [cx + a1.sin() * arc_radius, cy - a1.cos() * arc_radius],
            color: active_color,
        });
        a0 = a1;
    }
    if !active.is_empty() {
        ui.push_lines(LineBatch {
            segments: active.into(),
            line_width_px: 4.0,
            clip_rect: None,
        });
    }

    let mut inactive: Vec<LineSegment> = Vec::new();
    let mut a0 = value_angle;
    while a0 < end_angle {
        let a1 = (a0 + step).min(end_angle);
        inactive.push(LineSegment {
            a: [cx + a0.sin() * arc_radius, cy - a0.cos() * arc_radius],
            b: [cx + a1.sin() * arc_radius, cy - a1.cos() * arc_radius],
            color: inactive_color,
        });
        a0 = a1;
    }
    if !inactive.is_empty() {
        ui.push_lines(LineBatch {
            segments: inactive.into(),
            line_width_px: 4.0,
            clip_rect: None,
        });
    }

    // インジケータ: 中心から外円まで伸びる白い太線。値角度を指す。
    let dx = value_angle.sin();
    let dy = -value_angle.cos();
    let indicator = LineSegment {
        a: [cx, cy],
        b: [cx + dx * r, cy + dy * r],
        color: Color::rgb(0.95, 0.97, 1.00),
    };
    ui.push_lines(LineBatch {
        segments: vec![indicator].into(),
        line_width_px: 4.0,
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
    let step = 2.0_f32 * PI / 180.0;
    let mut segs: Vec<LineSegment> = Vec::new();
    let mut a = lo;
    while a < hi {
        let b = (a + step).min(hi);
        segs.push(LineSegment {
            a: [cx + a.sin() * radius, cy - a.cos() * radius],
            b: [cx + b.sin() * radius, cy - b.cos() * radius],
            color,
        });
        a = b;
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
    let sweep = 5.0_f32 * PI / 3.0; // 300°
    let angle_of = |v: f64| -> f32 {
        if !v.is_finite() {
            return -0.5 * sweep; // 7 時 (= value 0 の角度)
        }
        let t = (v as f32).clamp(0.0, 1.0);
        (t - 0.5) * sweep
    };
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

    // live 変調値の可動半径マーク (最前面、 明るい指針)。 base 白インジケータと別色 (amber) で区別。
    if let Some(lv) = live_value {
        let la = angle_of(lv);
        let dx = la.sin();
        let dy = -la.cos();
        let r_in = r * 0.45;
        ui.push_lines(LineBatch {
            segments: vec![LineSegment {
                a: [cx + dx * r_in, cy + dy * r_in],
                b: [cx + dx * r, cy + dy * r],
                color: Color::rgb(1.0, 0.85, 0.30),
            }]
            .into(),
            line_width_px: 2.5,
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
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 200 };
        host.frame_to_edits(
            model,
            &mut scene,
            screen,
            FrameInput { pointer, ..Default::default() },
            |_, ui| {
                ui.knob_at("test", rect, value, default_value, "knob", |v| {
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

    // ---- daw_01 #109: Bitwig 流 modulation (knob 版、 値も depth も 0..=1 正規化ドメイン) ----

    use crate::widgets::scrubable_number::ModEdit;

    /// base value (f32) と depth (f64) を別々に持つ test model。
    struct ModModel {
        value: f32,
        depth: f64,
    }

    /// modulation 付き 1 frame を描画 + 処理し、 edits と response を返す (rect = 64×64 knob、
    /// base 感度 = 1/64 units/px)。
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
                    "pan",
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
        // drag up 32px → depth = 0 + 32/64 = 0.5。 value は不変。
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, hold_at((c.0, c.1 - 32.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.depth - 0.5).abs() < 1e-5, "depth scrub +0.5 (got {})", model.depth);
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
        // drag up 32px → value 0.5 + 0.5 = 1.0
        let (edits, resp) = run_mod_frame(
            &mut host, &model, rect, hold_at((c.0, c.1 - 32.0), false), false, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }

        assert!((model.value - 1.0).abs() < 1e-5, "base scrub +0.5 → 1.0 (got {})", model.value);
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
            ui.knob_at("mtest", rect, 0.5, 0.5, "pan",
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
        // hold up 16px → depth 16/64 = 0.25
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, hold_at((c.0, c.1 - 16.0), false), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 0.25).abs() < 1e-5, "hold で depth 0.25 (got {})", model.depth);

        // release は更に上 (-32px) で離す → 最終 depth 0.5 が release frame で確定。
        let (edits, _) = run_mod_frame(
            &mut host, &model, rect, release_at((c.0, c.1 - 32.0)), true, &[], None, &mut Scene::new(),
        );
        for e in edits { e.apply(&mut model); }
        assert!((model.depth - 0.5).abs() < 1e-5, "release frame で最終 depth 0.5 確定 (got {})", model.depth);
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
                        depth_sensitivity: Some(0.1), // 0.1 units/px (base 1/64 ≈ 0.0156 を上書き)
                        on_mod_change: &on_mod,
                    }),
                };
                ui.knob_at("mtest", rect, model.value, 0.5, "pan",
                    |v| Edit::mutate(move |m: &mut ModModel| m.value = v), Some(m));
            })
        };

        for e in run(&mut host, &model, press_at(c, false)) { e.apply(&mut model); }
        // drag up 10px × depth_sensitivity 0.1 = 1.0 (base 感度 1/64 なら 0.156)。
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
            ui.knob_at("mtest", rect, 0.5, 0.5, "pan",
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
                ui.knob_at("mtest", rect, model.value, 0.5, "pan",
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
