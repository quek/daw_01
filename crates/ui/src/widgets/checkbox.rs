//! `checkbox` ウィジェット — bool toggle。click でチェック状態を反転する `Edit<M>` を発行。
//!
//! クリック判定は `button` と同じ armed-state モデル (`press_started_inside`) を使う。
//! 視覚: 16px の正方形チェック枠 + チェック時の塗りつぶし + ラベル。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, LineBatch, LineSegment, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, lerp_color};

/// checkbox の永続状態 (button と同形式)。
#[derive(Debug, Default)]
pub(crate) struct CheckboxState {
    press_started_inside: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CheckboxResponse {
    /// クリックされたか (Edit<M> 発行と同じフレームで `true`)。
    pub toggled: bool,
    pub hovered: bool,
}

/// チェック枠のサイズ (px)。
const BOX_SIZE: f32 = 16.0;
/// チェック枠とラベルの間隔。
const LABEL_GAP: f32 = 8.0;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で checkbox を描画 + ヒットテスト。
    /// click 時に `on_toggle(!checked)` の返り値を Edit 列に積む。
    pub fn checkbox_at<F>(
        &mut self,
        id: impl Hash,
        rect: Rect,
        checked: bool,
        label: &str,
        on_toggle: F,
    ) -> CheckboxResponse
    where
        F: FnOnce(bool) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"checkbox", &id));
        let pointer = self.pointer;
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));

        // armed-state click 判定 (button と同じモデル)。
        let (visual_pressed, click) = {
            let state: &mut CheckboxState = self.widget_state(wid);
            if pointer.primary_just_pressed {
                state.press_started_inside = inside;
            }
            let started = state.press_started_inside;
            let visual_pressed = started && inside && pointer.primary_pressed;
            let click = pointer.primary_just_released && started && inside;
            if pointer.primary_just_released {
                state.press_started_inside = false;
            }
            (visual_pressed, click)
        };

        // 描画。M4 Phase 11: with_widget_node で input_hash キャッシュ。
        let input_hash = hash_inputs((
            b"checkbox",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            checked,
            label,
            inside,
            visual_pressed,
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            draw_checkbox(ui, rect, checked, label, inside, visual_pressed);
        });

        if click {
            let edit = on_toggle(!checked);
            self.push_edit(edit);
        }

        CheckboxResponse { toggled: click, hovered: inside }
    }

    /// vstack カーソル位置に 1 行 checkbox を追加 (高さ 24px、幅は cursor 幅)。
    pub fn checkbox<F>(
        &mut self,
        id: impl Hash,
        checked: bool,
        label: &str,
        on_toggle: F,
    ) -> CheckboxResponse
    where
        F: FnOnce(bool) -> Edit<M>,
    {
        let pad = 8.0;
        let h = 24.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: self.cursor.w - pad * 2.0,
            h,
        };
        let resp = self.checkbox_at(id, rect, checked, label, on_toggle);
        self.next_y += h + pad;
        resp
    }
}

fn draw_checkbox<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    rect: Rect,
    checked: bool,
    label: &str,
    hovered: bool,
    pressed: bool,
) {
    // チェック枠を rect の左端 y 中央に配置。
    let box_y = rect.y + (rect.h - BOX_SIZE) * 0.5;
    let box_rect = Rect { x: rect.x, y: box_y, w: BOX_SIZE, h: BOX_SIZE };

    let base = Color::rgb(0.10, 0.11, 0.13);
    let hover_c = Color::rgb(0.18, 0.20, 0.24);
    let press_c = Color::rgb(0.32, 0.55, 0.85);
    let checked_c = Color::rgb(0.32, 0.55, 0.85);

    let bg_fill = if checked {
        checked_c
    } else if pressed {
        press_c
    } else if hovered {
        lerp_color(base, hover_c, 0.85)
    } else {
        base
    };
    let border_c = if hovered || checked {
        Color::rgb(0.55, 0.62, 0.74)
    } else {
        Color::rgb(0.35, 0.38, 0.45)
    };

    ui.push_rect(RectCommand {
        rect: box_rect,
        fill: bg_fill,
        border: border_c,
        border_width: 1.5,
        radius: [3.0; 4],
    });

    // チェックマーク (チェック時のみ): 2 本のラインで V を描く。
    if checked {
        let cx = box_rect.x;
        let cy = box_rect.y;
        let s = BOX_SIZE;
        // V 字の 3 点 (左上 → 中央下 → 右上)
        let p1 = [cx + s * 0.22, cy + s * 0.50];
        let p2 = [cx + s * 0.42, cy + s * 0.72];
        let p3 = [cx + s * 0.78, cy + s * 0.30];
        let check_color = Color::rgb(0.95, 0.97, 1.0);
        ui.push_lines(LineBatch {
            segments: vec![
                LineSegment { a: p1, b: p2, color: check_color },
                LineSegment { a: p2, b: p3, color: check_color },
            ],
            line_width_px: 2.0,
            clip_rect: None,
        });
    }

    // ラベル (チェック枠の右側、垂直中央)。
    if !label.is_empty() {
        let font_size = 14.0;
        let line_h = font_size * 1.2;
        let tx = box_rect.x + BOX_SIZE + LABEL_GAP;
        let ty = rect.y + (rect.h - line_h) * 0.5;
        ui.push_text(GlyphArea {
            text: label.to_string(),
            left: tx,
            top: ty,
            font_size,
            line_height: line_h,
            color: Color::rgb(0.92, 0.92, 0.94),
        });
    }
}
