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

/// ダブルクリック判定の時間しきい値 (ms)。
const DOUBLE_CLICK_MS: u128 = 300;
/// ダブルクリック判定の位置しきい値 (px)。
const DOUBLE_CLICK_PX: f32 = 5.0;
/// Ctrl + ドラッグ時の感度倍率 (1/10)。
const FINE_DRAG_SCALE: f32 = 0.1;

/// knob の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct KnobState {
    drag_anchor: Option<DragAnchor>,
    /// 直近のクリック (ダブルクリック判定用)。
    last_click: Option<ClickRecord>,
}

#[derive(Debug, Clone, Copy)]
struct DragAnchor {
    /// 押下/再 anchor 時のマウス y。
    pointer_y: f32,
    /// 押下/再 anchor 時の value。
    value: f32,
    /// 押下/再 anchor 時の Ctrl 状態。mid-drag toggle で再 anchor する判定用。
    ctrl: bool,
}

#[derive(Debug, Clone, Copy)]
struct ClickRecord {
    when: Instant,
    pos: (f32, f32),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KnobResponse {
    pub displayed_value: f32,
    pub hovered: bool,
    pub dragging: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で knob を描画 + ドラッグ。値変化時に `on_change(new_value)` を Edit 列に積む。
    ///
    /// `default_value` は rect のダブルクリック時にリセットされる値 (例: pan の中央 0.5)。
    ///
    /// 操作:
    /// - rect 全体をドラッグで値編集 (rect.h 分 = 0→1)
    /// - rect 全体をダブルクリック (~300ms / 5px 以内) で `default_value` に戻る
    /// - Ctrl + ドラッグで感度 1/10
    pub fn knob_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        default_value: f32,
        on_change: F,
    ) -> KnobResponse
    where
        F: FnOnce(f32) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"knob", &id));
        let pointer = self.pointer;
        let value = value.clamp(0.0, 1.0);
        let default_value = default_value.clamp(0.0, 1.0);

        // 1. 押下処理 + 2. mid-drag ctrl toggle 再 anchor + 3. release 解除
        let mut reset_fired = false;
        let drag_anchor = {
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

                if is_double {
                    state.last_click = None;
                    state.drag_anchor = None;
                    reset_fired = true;
                } else {
                    state.last_click = Some(ClickRecord { when: now, pos: (px, py) });
                    state.drag_anchor = Some(DragAnchor {
                        pointer_y: py,
                        value,
                        ctrl: pointer.modifiers.ctrl,
                    });
                }
            }

            // mid-drag で Ctrl が toggle されたら anchor を張り直す (詳細は fader.rs 参照)。
            if let Some(anchor) = state.drag_anchor
                && let Some((_, py)) = pointer.pos
                && pointer.modifiers.ctrl != anchor.ctrl
            {
                state.drag_anchor = Some(DragAnchor {
                    pointer_y: py,
                    value,
                    ctrl: pointer.modifiers.ctrl,
                });
            }

            if pointer.primary_just_released {
                state.drag_anchor = None;
            }

            state.drag_anchor
        };

        // 2. 表示値: リセット > drag > 入力値。
        let displayed_value = if reset_fired {
            default_value
        } else if let (Some(anchor), Some((_, py))) = (drag_anchor, pointer.pos) {
            let h = rect.h.max(1.0);
            let scale = if anchor.ctrl { FINE_DRAG_SCALE } else { 1.0 };
            let raw_dv = -(py - anchor.pointer_y) / h;
            (anchor.value + raw_dv * scale).clamp(0.0, 1.0)
        } else {
            value
        };

        // 3. 描画。M4 Phase 11: with_widget_node で input_hash キャッシュ。
        let dragging = drag_anchor.is_some();
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

        // 4. 値が変わっていれば Edit を発行。
        if (displayed_value - value).abs() > f32::EPSILON {
            let edit = on_change(displayed_value);
            self.push_edit(edit);
        }

        KnobResponse {
            displayed_value,
            hovered: hovered(rect, pointer),
            dragging: drag_anchor.is_some(),
        }
    }

    /// vstack カーソル位置に固定サイズで knob を追加 (64×64 px)。
    pub fn knob<F>(
        &mut self,
        id: impl Hash,
        value: f32,
        default_value: f32,
        on_change: F,
    ) -> KnobResponse
    where
        F: FnOnce(f32) -> Edit<M>,
    {
        let pad = 8.0;
        let size = 64.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: size,
            h: size,
        };
        let resp = self.knob_at(id, rect, value, default_value, on_change);
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
            segments: active,
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
            segments: inactive,
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
        segments: vec![indicator],
        line_width_px: 4.0,
        clip_rect: None,
    });
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
                ui.knob_at("test", rect, value, default_value, |v| {
                    Edit::mutate(move |m: &mut PanModel| m.value = v)
                });
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
}
