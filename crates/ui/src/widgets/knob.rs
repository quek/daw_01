//! `knob` ウィジェット — 回転ノブ。ドラッグで値編集 (上下ドラッグ、上 = 増)。
//!
//! - 値範囲: `0.0..=1.0`
//! - 視覚: 7 時の位置から 5 時の位置まで 300° のスイープ (DAW 標準)
//! - drag 感度: rect 高さ分のドラッグで 0 → 1 (fader と同じ感覚)
//! - hit area: rect 全体 (つまみが小さいので円外部でもドラッグ可とする)

use std::f32::consts::PI;
use std::hash::Hash;

use daw_ui_renderer::{Color, LineBatch, LineSegment, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::ui::{Ui, hovered, lerp_color};

/// knob の永続状態 (フレーム間で保持)。
#[derive(Debug, Default)]
pub(crate) struct KnobState {
    /// (押下時のマウス y, 押下時の value)。`None` ならドラッグしていない。
    drag_anchor: Option<(f32, f32)>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KnobResponse {
    pub displayed_value: f32,
    pub hovered: bool,
    pub dragging: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で knob を描画 + ドラッグ。値変化時に `on_change(new_value)` を Edit 列に積む。
    pub fn knob_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        value: f32,
        on_change: F,
    ) -> KnobResponse
    where
        F: FnOnce(f32) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"knob", &id));
        let pointer = self.pointer;
        let value = value.clamp(0.0, 1.0);

        // 1. ドラッグ判定。rect 全体が hit area。
        let drag_anchor = {
            let state: &mut KnobState = self.widget_state(wid);
            if pointer.primary_just_pressed
                && let Some((px, py)) = pointer.pos
                && rect.contains(px, py)
            {
                state.drag_anchor = Some((py, value));
            }
            if pointer.primary_just_released {
                state.drag_anchor = None;
            }
            state.drag_anchor
        };

        // 2. 表示値: ドラッグ中なら anchor からの差分、そうでなければ入力値。
        let displayed_value = if let (Some((anchor_y, anchor_value)), Some((_, py))) =
            (drag_anchor, pointer.pos)
        {
            let h = rect.h.max(1.0);
            let dv = -(py - anchor_y) / h;
            (anchor_value + dv).clamp(0.0, 1.0)
        } else {
            value
        };

        // 3. 描画。
        draw_knob(self, rect, displayed_value, drag_anchor.is_some(), pointer);

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
    pub fn knob<F>(&mut self, id: impl Hash, value: f32, on_change: F) -> KnobResponse
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
        let resp = self.knob_at(id, rect, value, on_change);
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

    let base = Color::rgb(0.18, 0.20, 0.26);
    let hover_c = Color::rgb(0.24, 0.27, 0.34);
    let press_c = Color::rgb(0.32, 0.55, 0.85);
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
        border: Color::rgb(0.35, 0.38, 0.45),
        border_width: 1.5,
        radius: [r; 4],
    });

    // インジケータ: 中心から円周に向けて伸びる線 (現在値の角度)。
    // 角度: value=0 → -150° (7 時)、value=0.5 → 0° (12 時)、value=1 → +150° (5 時)。
    let angle = (value - 0.5) * (5.0 * PI / 3.0);
    let dx = angle.sin();
    let dy = -angle.cos();
    let inner_r = r * 0.30;
    let outer_r = r * 0.85;
    let indicator = LineSegment {
        a: [cx + dx * inner_r, cy + dy * inner_r],
        b: [cx + dx * outer_r, cy + dy * outer_r],
        color: Color::rgb(0.95, 0.97, 1.0),
    };
    ui.push_lines(LineBatch {
        segments: vec![indicator],
        line_width_px: 2.5,
        clip_rect: None,
    });
}
