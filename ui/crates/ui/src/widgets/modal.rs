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
use crate::theme::Palette;
use crate::ui::Ui;

/// `Ui::modal` の見た目スタイル。
#[derive(Clone, Copy, Debug)]
pub struct ModalStyle {
    /// panel 背後の overlay 色 (通常 alpha 付き黒)。
    pub overlay_color: Color,
    pub panel_bg: Color,
    pub panel_radius: f32,
    /// `true` (default) で panel 外 click で close (= menu / dropdown と同じ標準挙動)。
    /// `false` にすると外 click で閉じず、 capturing modal では click を consume して無視する
    /// だけ (= Cancel / OK ボタンでしか閉じない blocking modal)。M14 Phase 95 (daw_01 #066) で
    /// `Ui::modal` が毎フレーム `PopupOpenState::dismiss_on_outside_click` へ同期し機能化。
    pub close_on_outside_click: bool,
    /// `true` で `take_shortcut("escape")` 検出時に close。
    pub close_on_escape: bool,
}

impl ModalStyle {
    /// パレットから既定の modal スタイルを組む。overlay は背後を沈める暗幕 (`backdrop`)、
    /// panel は elevation-1 の面 (`panel`)。
    ///
    /// `Default` は持たない (r.md #48): テーマ色を読む `Default::default()` は隠れた
    /// グローバル依存になり、ライトテーマに追従しないため。caller は
    /// `ModalStyle::from_palette(ui.palette())` で組む。
    #[must_use]
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            overlay_color: p.backdrop,
            panel_bg: p.panel,
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
        // M14 Phase 94 (daw_01 #065): dialog は `capture_input = true` (真のモーダル) で開く。
        // = 開いた次フレームから panel 外の全 widget への pointer / keyboard 入力が遮断される。
        // menu / dropdown / context_menu (`open_popup`, capture_input = false) とはここで区別する。
        self.open_popup_inner(
            ("modal", &id),
            Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            true,
            true,
            true,
        );
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

        // M14 Phase 95 (daw_01 #066): outside-click で閉じるか否かを style から popup state へ同期。
        // popup_layer はこの直後に呼ぶので、同フレームの outside-click 判定にラグなく反映される。
        self.set_popup_dismiss_on_outside_click(("modal", &id), style.close_on_outside_click);

        // popup_layer 前後の差分で close 検出 (ESC / outside click / body 内 close_modal)
        let was_open = self.is_modal_open(id);
        let style_copy = *style;

        self.popup_layer(("modal", &id), |ui| {
            // ESC ハンドラ (M14 Phase 94: 真のモーダル中の keyboard guard は drawing_in_popup
            // の外でのみ効くため、popup_layer の body 内 = guard 対象外でここに置く。これで
            // capture_input = true でも ESC が確実に modal を閉じられる)。閉じるフレームは
            // overlay / panel / body を描かず (= 従来の早期 return と同じ見た目)、popup_layer 出口
            // 〜 modal() 末尾の close 検出で on_close が発火する。
            if style_copy.close_on_escape && ui.take_shortcut("escape") {
                ui.close_modal(id);
                return;
            }
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
    use crate::theme::Palette;
    use crate::ui::UiHost;

    #[test]
    fn modal_body_runs_only_when_open() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::from_palette(&Palette::dark());
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
        let style = ModalStyle::from_palette(&Palette::dark());
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
            physical_key: PhysicalKey::Escape, repeat: false
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
        let style = ModalStyle::from_palette(&Palette::dark());
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
    fn overlay_masks_pointer_but_passes_keyboard_shortcuts() {
        // resource monitor (r.md #3): open_overlay は pointer を masking する
        // (panel 上クリックが背後の widget に突き抜けない) が、 keyboard / shortcut は
        // background に通す (Space 等の再生操作が効く)。 open_modal (真のモーダル、
        // keyboard も遮断) との差はこの 1 点。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_overlay("perf");
        });

        // Esc キー + pointer click を background widget へ流す。
        let esc = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape, repeat: false
        };
        let click = PointerFrame {
            pos: Some((10.0, 10.0)),
            primary_just_pressed: true,
            ..PointerFrame::default()
        };
        let shortcut_seen = Cell::new(false);
        let pointer_pos_seen = Cell::new(true);
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { keyboard: vec![esc], pointer: click, ..Default::default() },
            |(), ui| {
                // background (popup_layer の外) が shortcut / pointer を読む。
                shortcut_seen.set(ui.take_shortcut("escape"));
                pointer_pos_seen.set(ui.pointer().pos.is_some());
            },
        );
        assert!(
            shortcut_seen.get(),
            "overlay 中も background の shortcut (Esc) は効く = keyboard pass (Space 再生継続)"
        );
        assert!(
            !pointer_pos_seen.get(),
            "overlay 中は background の pointer が masked = panel 上クリックが突き抜けない"
        );
    }

    #[test]
    fn modal_renders_overlay_and_panel_rects() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::from_palette(&Palette::dark());

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
        let style = ModalStyle::from_palette(&Palette::dark());
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
        let style = ModalStyle::from_palette(&Palette::dark());
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

    // -------- M14 Phase 94 (daw_01 #065): 真のモーダル (panel 外の入力遮断) --------

    /// capture_input = true (default) の modal が開いている間、panel の外 (background) で
    /// pointer を読む widget には pos = None / press = false が見え (= inert)、panel 内
    /// (popup body) の widget には生 pointer が見える。fader 等 `self.pointer` 直読み widget が
    /// 自動的に無反応になることの SSoT 検証。
    #[test]
    fn capturing_modal_masks_background_pointer_but_not_body() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::from_palette(&Palette::dark());

        // 1 frame: open + draw (anchor 確定 + capture_input=true sync)
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("m");
            ui.modal("m", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // 次 frame: panel 内中央 (400,300) を press。背景は masked、body は raw。
        let bg_pos = Cell::new(Some((1.0, 1.0)));
        let bg_press = Cell::new(true);
        let body_pos = Cell::new(None::<(f32, f32)>);
        let body_press = Cell::new(false);
        let click = PointerFrame {
            pos: Some((400.0, 300.0)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..FrameInput::default() },
            |(), ui| {
                // 背景 (modal() より前) で pointer を読む
                bg_pos.set(ui.pointer().pos);
                bg_press.set(ui.pointer().primary_just_pressed);
                ui.modal("m", (200.0, 100.0), &style, None, |ui, _r| {
                    body_pos.set(ui.pointer().pos);
                    body_press.set(ui.pointer().primary_just_pressed);
                });
            },
        );
        assert_eq!(bg_pos.get(), None, "背景 widget は pointer pos が masking される");
        assert!(!bg_press.get(), "背景 widget は press が masking される");
        assert_eq!(body_pos.get(), Some((400.0, 300.0)), "modal body は raw pointer を見る");
        assert!(body_press.get(), "modal body は press を見る");
    }

    /// capture_input = true の modal が開いている間、panel 外で `take_primary_press_in_rect` を
    /// 呼んでも (anchor の外であっても) press は返らない (真のモーダル = 全画面遮断)。
    #[test]
    fn capturing_modal_blocks_background_press_outside_anchor() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::from_palette(&Palette::dark());

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("m");
            ui.modal("m", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // panel_rect = (300,250,200,100)。その外 (50,50) を press。
        let bg_rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let observed = Cell::new(Some((0.0_f32, 0.0_f32)));
        let click = PointerFrame {
            pos: Some((50.0, 50.0)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..FrameInput::default() },
            |(), ui| {
                observed.set(ui.take_primary_press_in_rect(bg_rect));
                ui.modal("m", (200.0, 100.0), &style, None, |_ui, _r| {});
            },
        );
        assert_eq!(observed.get(), None, "真のモーダル中は anchor 外の press も遮断される");
    }

    /// capture_input = true の modal が開いている間、background の `take_shortcut` は遮断される
    /// (consume もされないので、modal body が同じ shortcut を後で拾える)。
    #[test]
    fn capturing_modal_blocks_background_shortcut() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::from_palette(&Palette::dark());

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("m");
            ui.modal("m", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // ESC 送信。modal() を呼ばない frame で background take_shortcut("escape") を確認。
        let esc = KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape, repeat: false
        };
        let got = Cell::new(true);
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { keyboard: vec![esc], ..FrameInput::default() },
            |(), ui| {
                got.set(ui.take_shortcut("escape"));
            },
        );
        assert!(!got.get(), "真のモーダル中は background の shortcut が遮断される");
    }

    /// review fix: capturing modal が開いている間、**同時に開いている background の非 capturing
    /// popup (menu / dropdown / context_menu)** の body も masking される (= popup item が
    /// hover / click に反応しない)。`masked_here` を `state.capture_input` で gate していないと、
    /// 全 top-level popup body が un-mask されて background menu が生きてしまう regression を防ぐ。
    #[test]
    fn capturing_modal_masks_background_popup_body() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::from_palette(&Palette::dark());
        let menu_anchor = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

        // 1 frame: background popup (menu = capture_input false) と capturing modal を両方開く。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_popup("menu", menu_anchor, true);
            ui.open_modal("m");
            ui.modal("m", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // 次 frame: menu anchor 内 (50,50) を press。background menu の body は masked であるべき。
        let menu_body_pos = Cell::new(Some((9.0, 9.0)));
        let menu_body_press = Cell::new(true);
        let click = PointerFrame {
            pos: Some((50.0, 50.0)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..FrameInput::default() },
            |(), ui| {
                // background popup の body (menu_bar dropdown 相当)
                ui.popup_layer("menu", |ui| {
                    menu_body_pos.set(ui.pointer().pos);
                    menu_body_press.set(ui.pointer().primary_just_pressed);
                });
                // capturing modal を描画
                ui.modal("m", (200.0, 100.0), &style, None, |_ui, _r| {});
            },
        );
        assert_eq!(menu_body_pos.get(), None, "capturing modal 中は background popup body も masked");
        assert!(!menu_body_press.get(), "capturing modal 中は background popup body の press も masked");
    }

    /// daw_01 #066: `close_on_outside_click: false` の modal は panel 外 click で閉じず、
    /// その frame でも body が描画される (= 「閉じて再 open」のフラッシュが起きない)。on_close も
    /// 発火しない。
    #[test]
    fn blocking_modal_does_not_close_on_outside_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle {
            close_on_outside_click: false,
            ..ModalStyle::from_palette(&Palette::dark())
        };
        let on_close_fired = std::rc::Rc::new(Cell::new(false));
        let body_called = Cell::new(0u32);

        // open + draw 1 frame (close_on_outside_click=false を popup state へ同期)。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
            ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        // panel_rect = (300,250,200,100)。その外 (10,10) を click。
        let click = PointerFrame {
            pos: Some((10.0, 10.0)),
            primary_just_pressed: true,
            ..PointerFrame::default()
        };
        let on_close_clone = on_close_fired.clone();
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..FrameInput::default() },
            |(), ui| {
                let on_close: Option<Box<dyn FnOnce() -> Edit<()>>> = Some(Box::new(move || {
                    on_close_clone.set(true);
                    Edit::mutate(|(): &mut ()| {})
                }));
                ui.modal("dlg", (200.0, 100.0), &style, on_close, |_ui, _r| {
                    body_called.set(body_called.get() + 1);
                });
            },
        );

        assert!(!on_close_fired.get(), "close_on_outside_click=false では外 click で on_close 発火しない");
        assert_eq!(body_called.get(), 1, "外 click frame でも body が描画される (フラッシュ防止)");

        let still_open = Cell::new(false);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            still_open.set(ui.is_modal_open("dlg"));
        });
        assert!(still_open.get(), "外 click 後も modal は開いたまま");
    }

    /// 既存 default (`close_on_outside_click: true`) は従来どおり外 click で閉じる回帰確認。
    #[test]
    fn default_modal_still_closes_on_outside_click() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let style = ModalStyle::from_palette(&Palette::dark()); // close_on_outside_click = true

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.open_modal("dlg");
            ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {});
        });

        let click = PointerFrame {
            pos: Some((10.0, 10.0)),
            primary_just_pressed: true,
            ..PointerFrame::default()
        };
        host.frame_to_edits(
            &(),
            &mut scene,
            screen,
            FrameInput { pointer: click, ..FrameInput::default() },
            |(), ui| {
                ui.modal("dlg", (200.0, 100.0), &style, None, |_ui, _r| {});
            },
        );

        let still_open = Cell::new(true);
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            still_open.set(ui.is_modal_open("dlg"));
        });
        assert!(!still_open.get(), "default は外 click で閉じる (従来挙動)");
    }
}
