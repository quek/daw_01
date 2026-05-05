//! `modal` ウィジェット — 半透明 overlay + 画面中央 panel + ESC / outside click で
//! close できる project 内ダイアログ (M9 Phase 45d)。
//!
//! 設計:
//! - 既存 `popup_layer` / `open_popup` / `close_popup` の上の薄いラッパ
//! - panel rect を毎フレーム `Ui::screen()` から中央配置で計算 → `update_popup_anchor`
//!   で anchor を最新化 (window resize 対応)
//! - ESC: `take_shortcut("escape")` (default binding 済み) で消費 → close
//! - outside click: `popup_layer` の標準挙動で auto-close
//! - `on_close: Option<Box<dyn FnOnce() -> Edit<M>>>` を modal 関数 local に保持し、
//!   close を検出したフレームで `push_edit` する (popup_layer closure には move しない)
//!
//! file picker は `rfd` (OS native) で済ませる方針なので、modal は project 内 dialog
//! (Plugin Picker / Save 確認 / Export 設定 / About) 専用。

use std::hash::Hash;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::edit::Edit;
use crate::ui::Ui;

/// `Ui::modal` の見た目スタイル。
#[derive(Clone, Copy, Debug)]
pub struct ModalStyle {
    /// panel 背後の overlay 色 (通常 alpha 付き黒)。
    pub overlay_color: Color,
    pub panel_bg: Color,
    pub panel_radius: f32,
    /// `true` で panel 外 click で close (= `popup_layer` の標準挙動)。
    /// 現状は `false` でも popup_layer 側が常に auto-close するため意味的フィールドのみ
    /// (将来の拡張点)。
    pub close_on_outside_click: bool,
    /// `true` で `take_shortcut("escape")` 検出時に close。
    pub close_on_escape: bool,
}

impl Default for ModalStyle {
    fn default() -> Self {
        Self {
            overlay_color: Color::rgba(0.0, 0.0, 0.0, 0.6),
            panel_bg: Color::rgb(0.14, 0.15, 0.18),
            panel_radius: 6.0,
            close_on_outside_click: true,
            close_on_escape: true,
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// modal を開く。次以降のフレームで `Ui::modal(id, ...)` の body が実行される。
    pub fn open_modal(&mut self, id: impl Hash) {
        // anchor は modal 関数で毎フレーム update_popup_anchor で上書きするので、
        // ここでは仮値 (0,0,0,0) で OK。
        self.open_popup(("modal", &id), Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, true);
    }

    /// modal を閉じる。`on_close` Edit は呼び出し側責任 (modal 関数経由で発火させる場合は
    /// 呼ばない、明示的に閉じる場合は別途 push_edit する)。
    pub fn close_modal(&mut self, id: impl Hash) {
        self.close_popup(("modal", &id));
    }

    /// modal が現在開いているか。
    #[must_use]
    pub fn is_modal_open(&self, id: impl Hash) -> bool {
        self.is_popup_open(("modal", &id))
    }

    /// modal 本体を描画する。modal が開いていなければ body は呼ばれない。
    ///
    /// - `panel_size`: panel の (幅, 高さ)。画面中央に配置される。
    /// - `on_close`: ESC / outside click / body 内 `close_modal` で閉じたフレームに発火する
    ///   `Edit<M>`。`None` なら何もしない (caller が `is_modal_open` で同期する想定)。
    /// - `body`: panel 内側の rect を渡された描画 closure。caller が title / list / buttons
    ///   などを自由配置する。
    pub fn modal<F>(
        &mut self,
        id: impl Hash + Copy,
        panel_size: (f32, f32),
        style: &ModalStyle,
        on_close: Option<Box<dyn FnOnce() -> Edit<M>>>,
        body: F,
    ) where
        F: FnOnce(&mut Self, Rect),
    {
        if !self.is_modal_open(id) {
            return;
        }

        // 中央 panel rect 計算
        let screen = self.screen();
        let screen_w = screen.width as f32;
        let screen_h = screen.height as f32;
        let panel_w = panel_size.0.min(screen_w);
        let panel_h = panel_size.1.min(screen_h);
        let panel_rect = Rect {
            x: ((screen_w - panel_w) * 0.5).max(0.0),
            y: ((screen_h - panel_h) * 0.5).max(0.0),
            w: panel_w,
            h: panel_h,
        };
        let screen_rect = Rect { x: 0.0, y: 0.0, w: screen_w, h: screen_h };

        // anchor を最新の panel_rect に更新 (window resize 対応)
        self.update_popup_anchor(("modal", &id), panel_rect);

        // ESC ハンドラ
        if style.close_on_escape && self.take_shortcut("escape") {
            self.close_modal(id);
            if let Some(f) = on_close {
                let edit = f();
                self.push_edit(edit);
            }
            return;
        }

        // popup_layer 前後の差分で close 検出 (outside click / body 内 close_modal)
        let was_open = self.is_modal_open(id);
        let style_copy = *style;

        self.popup_layer(("modal", &id), |ui| {
            // 1. 画面全体の overlay
            ui.push_rect(RectCommand {
                rect: screen_rect,
                fill: style_copy.overlay_color,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
            // 2. 中央 panel
            ui.push_rect(RectCommand {
                rect: panel_rect,
                fill: style_copy.panel_bg,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [style_copy.panel_radius; 4],
                clip_rect: None,
            });
            // 3. body (panel_inner_rect = panel_rect そのまま、border 無いので padding 不要)
            body(ui, panel_rect);
        });

        // outside click / body 内 close_modal で閉じた場合 → on_close 発火
        let now_open = self.is_modal_open(id);
        if was_open
            && !now_open
            && let Some(f) = on_close
        {
            let edit = f();
            self.push_edit(edit);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey, PhysicalSize};
    use daw_ui_renderer::{Rect, Scene};

    use super::ModalStyle;
    use crate::edit::Edit;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    #[test]
    fn modal_body_runs_only_when_open() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::default();
        let body_called = Cell::new(0u32);

        // 開いていない → body は呼ばれない
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {
                body_called.set(body_called.get() + 1);
            });
        });
        assert_eq!(body_called.get(), 0);

        // open_modal → 次フレームで body が呼ばれる
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
            ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {
                body_called.set(body_called.get() + 1);
            });
        });
        assert_eq!(body_called.get(), 1);
    }

    #[test]
    fn escape_closes_modal_and_fires_on_close() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::default();
        let on_close_fired = std::rc::Rc::new(Cell::new(false));

        // open
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
        });
        let opened = Cell::new(false);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            opened.set(ui.is_modal_open("dlg"));
        });
        assert!(opened.get());

        // ESC キーを送って close
        let esc = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape,
        };
        let on_close_fired_clone = on_close_fired.clone();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { keyboard: vec![esc], ..Default::default() },
            |(), ui| {
                let on_close: Option<Box<dyn FnOnce() -> Edit<()>>> = Some(Box::new(move || {
                    on_close_fired_clone.set(true);
                    Edit::mutate(|(): &mut ()| {})
                }));
                ui.modal("dlg", (200.0, 100.0), &style, on_close, |_ui, _r| {});
            },
        );

        assert!(on_close_fired.get(), "ESC で on_close が呼ばれた");
        let still_open = Cell::new(false);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            still_open.set(ui.is_modal_open("dlg"));
        });
        assert!(!still_open.get());
    }

    #[test]
    fn outside_click_closes_modal_and_fires_on_close() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::default();
        let on_close_fired = std::rc::Rc::new(Cell::new(false));

        // open + 1 frame で anchor 更新
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
            ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // panel rect = (300, 250, 200, 100)、その外を click → close
        let click = PointerFrame {
            pos: Some((10.0, 10.0)),
            primary_just_pressed: true,
            ..PointerFrame::default()
        };
        let on_close_fired_clone = on_close_fired.clone();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..Default::default() },
            |(), ui| {
                let on_close: Option<Box<dyn FnOnce() -> Edit<()>>> = Some(Box::new(move || {
                    on_close_fired_clone.set(true);
                    Edit::mutate(|(): &mut ()| {})
                }));
                ui.modal("dlg", (200.0, 100.0), &style, on_close, |_ui, _r| {});
            },
        );

        assert!(on_close_fired.get(), "outside click で on_close が呼ばれた");
        let still_open = Cell::new(false);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            still_open.set(ui.is_modal_open("dlg"));
        });
        assert!(!still_open.get());
    }

    #[test]
    fn modal_renders_overlay_and_panel_rects() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::default();

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
            ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // overlay (画面サイズ) + panel (panel_rect) の 2 個 push_rect される。
        // popup_layer 内 (`drawing_in_popup = true`) の primitive は `scene.popup_rects` に
        // 移される (Phase 44a で popup pass を独立 pipeline 化した結果)。
        assert!(
            scene.popup_rect_count() >= 2,
            "overlay + panel が popup_primitives に積まれた ({})",
            scene.popup_rect_count()
        );
    }

    #[test]
    fn body_can_close_modal_explicitly() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::default();
        let on_close_fired = std::rc::Rc::new(Cell::new(false));

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
        });

        // body 内で close_modal を呼ぶ
        let on_close_fired_clone = on_close_fired.clone();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let on_close: Option<Box<dyn FnOnce() -> Edit<()>>> = Some(Box::new(move || {
                on_close_fired_clone.set(true);
                Edit::mutate(|(): &mut ()| {})
            }));
            ui.modal("dlg", (200.0, 100.0), &style, on_close, |ui, _r| {
                ui.close_modal("dlg");
            });
        });

        assert!(on_close_fired.get(), "body 内 close で on_close が呼ばれた");
    }

    /// daw_01 #015: modal body 内の ✕ ボタンを `button_at_clicked` で実装し、
    /// click された frame で `close_modal` を呼ぶと on_close が発火し、次フレームで
    /// `is_modal_open == false` になる回帰テスト。
    #[test]
    fn close_button_inside_modal_closes_via_button_at_clicked() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::default();
        let on_close_fired = std::rc::Rc::new(Cell::new(false));

        // open + 1 frame で anchor を panel_rect に確定 (panel 200x100 を screen 800x600 中央)。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
            ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // panel_rect = (300, 250, 200, 100)、close button rect は panel 右上 (470, 252, 24, 24)。
        let close_rect = Rect { x: 470.0, y: 252.0, w: 24.0, h: 24.0 };
        let pos = (482.0, 264.0); // close_rect 中央

        // press
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
                ui.modal("dlg", (200.0, 100.0), &style, None, |ui, _panel| {
                    let _ = ui.button_at_clicked("close_x", "x", close_rect);
                });
            },
        );

        // release → button_at_clicked が true → close_modal → on_close 発火
        let on_close_fired_clone = on_close_fired.clone();
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
                let on_close: Option<Box<dyn FnOnce() -> Edit<()>>> = Some(Box::new(move || {
                    on_close_fired_clone.set(true);
                    Edit::mutate(|(): &mut ()| {})
                }));
                ui.modal("dlg", (200.0, 100.0), &style, on_close, |ui, _panel| {
                    if ui.button_at_clicked("close_x", "x", close_rect) {
                        ui.close_modal("dlg");
                    }
                });
            },
        );

        assert!(on_close_fired.get(), "close button click で on_close が発火する");

        let still_open = Cell::new(true);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            still_open.set(ui.is_modal_open("dlg"));
        });
        assert!(!still_open.get(), "次フレームで modal が閉じている");
    }
}
