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
        if self.button_at_clicked(id, text, rect) {
            let edit = on_click();
            self.push_edit(edit);
        }
    }

    /// `button_at` の Edit-less 版。click された frame で `true` を返す。
    ///
    /// 用途: button click 内で `Ui` 操作 (modal の `close_modal` / `set_focus` /
    /// 複数 `push_edit` / 動的 popup 開閉) を行いたい場合。menu item と同じく
    /// 「click handler が `&mut Ui` を必要とする」パターンに対応する。
    ///
    /// daw_01 #015 で plugin_picker の ✕ ボタンが「`Edit` だけ返しても popup state は
    /// 閉じない」問題を解消するために導入 (M9 Phase 46)。
    #[must_use]
    pub fn button_at_clicked(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        rect: Rect,
    ) -> bool {
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
                clip_rect: None,
            });

            // テキストを矩形中央付近に
            let font_size = 16.0;
            let line_h = font_size * 1.2;
            // 簡易: ASCII 文字幅を 9px 仮定。日本語は適当に配置 (M1)。
            let approx_w = (text.chars().count() as f32) * 9.0;
            let tx = rect.x + (rect.w - approx_w).max(0.0) * 0.5;
            let ty = rect.y + (rect.h - line_h).max(0.0) * 0.5;
            ui.push_text(GlyphArea {
                text: text.into(),
                left: tx,
                top: ty,
                font_size,
                line_height: line_h,
                color: Color::rgb(0.95, 0.95, 0.97),
                clip_rect: None,
                ..GlyphArea::default()
            });
        });

        click
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Rect, Scene};

    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    #[test]
    fn button_at_clicked_returns_true_on_press_and_release_inside() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 10.0, y: 10.0, w: 100.0, h: 30.0 };
        let pos = (50.0, 25.0);

        // press inside
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            |(), ui| {
                let clicked = ui.button_at_clicked("btn", "OK", rect);
                assert!(!clicked, "press 単独では click 発火しない");
            },
        );

        // release inside → click 発火
        let observed = Cell::new(false);
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some(pos),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            |(), ui| {
                observed.set(ui.button_at_clicked("btn", "OK", rect));
            },
        );
        assert!(observed.get(), "release inside で click=true");
    }

    #[test]
    fn button_at_clicked_returns_false_when_press_started_outside() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 10.0, y: 10.0, w: 100.0, h: 30.0 };
        let outside = (200.0, 200.0);
        let inside = (50.0, 25.0);

        // press outside
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some(outside),
                    primary_just_pressed: true,
                    primary_pressed: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            |(), ui| {
                let _ = ui.button_at_clicked("btn", "OK", rect);
            },
        );

        // release inside → press_started_inside == false なので click 発火しない
        let observed = Cell::new(true);
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput {
                pointer: PointerFrame {
                    pos: Some(inside),
                    primary_just_released: true,
                    ..PointerFrame::default()
                },
                ..FrameInput::default()
            },
            |(), ui| {
                observed.set(ui.button_at_clicked("btn", "OK", rect));
            },
        );
        assert!(!observed.get(), "press outside で始まった click は発火しない");
    }
}
