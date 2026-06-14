//! `panel` ウィジェット — 背景塗り 1 行 helper (M9 Phase 45a)。
//!
//! 多くの view で同じ boilerplate (`heavy + cached + push_rect`) を書いていたのを
//! library に吸収する。CLAUDE.md「ユーザに同じ workaround を書かせる API は設計欠陥」適用。
//!
//! ```ignore
//! // before:
//! ui.heavy("bg", |hctx| {
//!     hctx.cached((rect.w.to_bits(), rect.h.to_bits()), |hctx| {
//!         hctx.push_rect(RectCommand {
//!             rect, fill: COLOR_BG,
//!             border: Color::TRANSPARENT, border_width: 0.0,
//!             radius: [0.0; 4], clip_rect: None,
//!         });
//!     });
//! });
//!
//! // after:
//! ui.panel("bg", rect, COLOR_BG, 0.0);
//! ```
//!
//! cache key は rect / fill / border / radius の全要素を含めるので、ピクセル単位で
//! 同じ panel が出続けるかぎり cache hit。view のリサイズ・色変更で自動 invalidate。

use std::hash::Hash;

use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::ui::Ui;

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 背景塗り (border なし、`radius == 0.0` で角丸なし)。
    ///
    /// 内部で `heavy + cached + push_rect` を実行する。`id` は他の widget や panel と
    /// 衝突しない identifier (位置やプロパティで決め打ち、frame 間で一貫させる)。
    pub fn panel(&mut self, id: impl Hash, rect: Rect, fill: Color, radius: f32) {
        self.panel_with_border(id, rect, fill, Color::TRANSPARENT, 0.0, radius);
    }

    /// border 付き背景塗り (file drop hover の hint frame 等)。
    pub fn panel_with_border(
        &mut self,
        id: impl Hash,
        rect: Rect,
        fill: Color,
        border: Color,
        border_width: f32,
        radius: f32,
    ) {
        self.heavy((b"panel", &id), |hctx| {
            hctx.cached(
                [
                    rect.x.to_bits(),
                    rect.y.to_bits(),
                    rect.w.to_bits(),
                    rect.h.to_bits(),
                    fill.r.to_bits(),
                    fill.g.to_bits(),
                    fill.b.to_bits(),
                    fill.a.to_bits(),
                    border.r.to_bits(),
                    border.g.to_bits(),
                    border.b.to_bits(),
                    border.a.to_bits(),
                    border_width.to_bits(),
                    radius.to_bits(),
                ],
                |hctx| {
                    hctx.push_rect(RectCommand {
                        rect,
                        fill,
                        border,
                        border_width,
                        radius: [radius; 4],
                        clip_rect: None,
                    });
                },
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Color, Rect, Scene};

    use crate::input::FrameInput;
    use crate::ui::UiHost;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn panel_pushes_rect_with_given_fill_and_no_border() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 10.0, y: 20.0, w: 30.0, h: 40.0 };
        let fill = Color::rgb(0.1, 0.2, 0.3);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("bg", rect, fill, 0.0);
        });

        assert_eq!(scene.rect_count(), 1);
        assert_eq!(scene.rects_vec()[0].rect, rect);
        assert!(approx(scene.rects_vec()[0].fill.r, fill.r));
        assert!(approx(scene.rects_vec()[0].fill.g, fill.g));
        assert!(approx(scene.rects_vec()[0].fill.b, fill.b));
        assert!(approx(scene.rects_vec()[0].border_width, 0.0));
        assert!(scene.rects_vec()[0].radius.iter().all(|r| approx(*r, 0.0)));
    }

    #[test]
    fn panel_with_border_pushes_rect_with_border_and_radius() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 50.0, h: 50.0 };
        let fill = Color::rgb(0.5, 0.5, 0.5);
        let border = Color::rgb(1.0, 0.0, 0.0);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel_with_border("bg2", rect, fill, border, 2.0, 6.0);
        });

        assert_eq!(scene.rect_count(), 1);
        assert!(approx(scene.rects_vec()[0].border_width, 2.0));
        assert!(scene.rects_vec()[0].radius.iter().all(|r| approx(*r, 6.0)));
        assert!(approx(scene.rects_vec()[0].border.r, 1.0));
    }

    #[test]
    fn panel_caches_when_unchanged_and_reuses_command() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let fill = Color::rgb(0.1, 0.2, 0.3);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("bg", rect, fill, 0.0);
        });
        assert_eq!(scene.rect_count(), 1);

        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("bg", rect, fill, 0.0);
        });
        assert_eq!(scene.rect_count(), 1, "2 回目も rect が cached 経由で積まれる");
        assert_eq!(scene.rects_vec()[0].rect, rect);
    }

    #[test]
    fn panel_invalidates_on_size_change() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let fill = Color::rgb(0.1, 0.2, 0.3);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("bg", Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 }, fill, 0.0);
        });
        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("bg", Rect { x: 0.0, y: 0.0, w: 200.0, h: 100.0 }, fill, 0.0);
        });
        assert_eq!(scene.rect_count(), 1);
        assert!(approx(scene.rects_vec()[0].rect.w, 200.0));
    }

    #[test]
    fn panel_invalidates_on_color_change() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("bg", rect, Color::rgb(0.1, 0.2, 0.3), 0.0);
        });
        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("bg", rect, Color::rgb(0.9, 0.1, 0.1), 0.0);
        });
        assert_eq!(scene.rect_count(), 1);
        assert!((scene.rects_vec()[0].fill.r - 0.9).abs() < 1e-6);
    }

    #[test]
    fn panels_with_different_ids_do_not_collide() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 100 };
        let fill = Color::rgb(0.2, 0.2, 0.2);

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.panel("a", Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 }, fill, 0.0);
            ui.panel("b", Rect { x: 100.0, y: 0.0, w: 100.0, h: 100.0 }, fill, 0.0);
        });
        assert_eq!(scene.rect_count(), 2);
        assert!(approx(scene.rects_vec()[0].rect.x, 0.0));
        assert!(approx(scene.rects_vec()[1].rect.x, 100.0));
    }
}
