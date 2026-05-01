//! `button` ウィジェット — クリックされると `Edit<M>` を発行する。
//!
//! クリック判定: **press 開始位置を記憶**するモデル。
//! - press inside → `press_started_inside = true` を記憶
//! - release inside かつ `press_started_inside` → click 発火
//! - press outside で始まったクリックは release が内側でも発火しない
//! - press 中に外れて戻ってきても、release が内側なら click 発火 (Windows 標準挙動)
//!
//! これで「press inside → 少しドリフト → release inside」を取りこぼさない。

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, lerp_color};

/// button の永続状態。
#[derive(Debug, Default)]
pub(crate) struct ButtonState {
    /// 直近の primary press がこのボタン内から始まったか。
    /// release 時の click 判定に使う。release で false にリセット。
    press_started_inside: bool,
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定でボタンを描画+ヒットテスト。click 時 `on_click()` を Edit 列に積む。
    pub fn button_at(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        rect: Rect,
        on_click: impl FnOnce() -> Edit<M>,
    ) {
        let wid = WidgetId::ROOT.child((b"button", &id));
        let pointer = self.pointer;
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));

        // press 開始位置の記録と click 判定。
        let (visual_pressed, click) = {
            let state: &mut ButtonState = self.widget_state(wid);
            if pointer.primary_just_pressed {
                state.press_started_inside = inside;
            }
            let started = state.press_started_inside;
            // 視覚: 「このボタンで押下が始まり、今もボタン内にホールド中」のときだけ pressed 表示。
            let visual_pressed = started && inside && pointer.primary_pressed;
            // click: release inside かつこのボタンで press が始まっていた。
            let click = pointer.primary_just_released && started && inside;
            if pointer.primary_just_released {
                state.press_started_inside = false;
            }
            (visual_pressed, click)
        };

        // M4 Phase 11: 描画を with_widget_node で input_hash キャッシュ。
        // input_hash の入力は visual に影響する: rect / text / inside / visual_pressed。
        let input_hash = hash_inputs((
            b"button",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            text,
            inside,
            visual_pressed,
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            let base = Color::rgb(0.18, 0.20, 0.26);
            let hover = Color::rgb(0.24, 0.27, 0.34);
            let press = Color::rgb(0.32, 0.55, 0.85);

            let fill = if visual_pressed {
                press
            } else if inside {
                lerp_color(base, hover, 0.85)
            } else {
                base
            };

            ui.push_rect(RectCommand {
                rect,
                fill,
                border: Color::rgb(0.35, 0.38, 0.45),
                border_width: 1.0,
                radius: [6.0; 4],
            });

            // テキストを矩形中央付近に
            let font_size = 16.0;
            let line_h = font_size * 1.2;
            // 簡易: ASCII 文字幅を 9px 仮定。日本語は適当に配置 (M1)。
            let approx_w = (text.chars().count() as f32) * 9.0;
            let tx = rect.x + (rect.w - approx_w).max(0.0) * 0.5;
            let ty = rect.y + (rect.h - line_h).max(0.0) * 0.5;
            ui.push_text(GlyphArea {
                text: text.to_string(),
                left: tx,
                top: ty,
                font_size,
                line_height: line_h,
                color: Color::rgb(0.95, 0.95, 0.97),
            });
        });

        if click {
            let edit = on_click();
            self.push_edit(edit);
        }
    }

    /// vstack カーソル位置に 1 行ボタンを追加 (幅は cursor 幅 - padding)。
    pub fn button(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        on_click: impl FnOnce() -> Edit<M>,
    ) {
        let pad = 8.0;
        let h = 32.0;
        let rect = Rect {
            x: self.cursor.x + pad,
            y: self.cursor.y + self.next_y,
            w: self.cursor.w - pad * 2.0,
            h,
        };
        self.button_at(id, text, rect, on_click);
        self.next_y += h + pad;
    }
}
