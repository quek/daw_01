//! `button` ウィジェット — クリックされると `Edit<M>` を発行する。
//!
//! クリック判定: **press 開始位置を記憶**するモデル。
//! - press inside → `press_started_inside = true` を記憶
//! - release inside かつ `press_started_inside` → click 発火
//! - press outside で始まったクリックは release が内側でも発火しない
//! - press 中に外れて戻ってきても、release が内側なら click 発火 (Windows 標準挙動)
//!
//! これで「press inside → 少しドリフト → release inside」を取りこぼさない。

use daw_ui_renderer::{theme, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::Ui;

/// rect 内のテキスト水平揃え (`button_at_clicked_sized_aligned` 用)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ButtonTextAlign {
    /// 中央寄せ。 汎用 `button` / menu / dialog の既定。
    Center,
    /// 左寄せ (`tx = rect.x`)。 track 名など先頭が識別に最重要なラベル用
    /// (Reaper / Cubase / Live のトラック名と同じ)。 省略時の左寄せとも一致する。
    Left,
}

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

    /// `button_at` の font_size 可変版。 click 判定・外観は `button_at` と完全同一で、
    /// テキストの font_size だけを呼び出し側が指定する。 16px 固定の `button_at` では
    /// 大きすぎる狭い領域の小ボタン (mixer send slot の × 等) を、 ボタン外で
    /// `button_at_clicked_sized` + `push_edit` (pub(crate) で view から呼べない) を
    /// 手書きする boilerplate なしに置けるようにする。
    pub fn button_at_sized(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        rect: Rect,
        font_size: f32,
        on_click: impl FnOnce() -> Edit<M>,
    ) {
        if self.button_at_clicked_sized(id, text, rect, font_size) {
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
        // 汎用ボタンの font_size は 16px 固定 (menu / dialog 等が依存)。
        self.button_at_clicked_sized(id, text, rect, 16.0)
    }

    /// `button_at_clicked` の font_size 可変版。click 判定・外観 (fill / border / 角丸) は
    /// `button_at_clicked` と完全同一で、**テキストの font_size だけ**を呼び出し側が指定する。
    ///
    /// 汎用 `button_at_clicked` は 16px 固定 (menu / dialog 等が依存) なので変えられないが、
    /// arrangement の track 名のように `style.track_text_size` へ追従させたい widget が
    /// boilerplate な再実装なしにサイズだけ差し替えられる (daw_01 #076)。
    #[must_use]
    pub fn button_at_clicked_sized(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        rect: Rect,
        font_size: f32,
    ) -> bool {
        self.button_at_clicked_sized_aligned(id, text, rect, font_size, ButtonTextAlign::Center)
    }

    /// `button_at_clicked_sized` のテキスト水平揃え可変版。 click 判定・外観は同一で、
    /// 収まるテキストの揃えだけを指定する。 `Center` は従来どおり (byte 互換)、 `Left` は
    /// `tx = rect.x` で左寄せ (track 名のように先頭が識別に最重要なラベル用、 daw_01 #079)。
    ///
    /// 省略 (ellipsis) が発生したテキストは align に関係なく**常に左寄せ + clip** になる
    /// (先頭を残すのが ellipsis の意味。 `Left` は「収まる時も左」 を足すだけ)。
    #[must_use]
    pub fn button_at_clicked_sized_aligned(
        &mut self,
        id: impl std::hash::Hash,
        text: &str,
        rect: Rect,
        font_size: f32,
        align: ButtonTextAlign,
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
        // input_hash の入力は visual に影響する: rect / text / inside / visual_pressed / font_size。
        // font_size は text 幅・縦中央位置を変えるので、runtime で size が変わる呼び出し側
        // (track 名 = style.track_text_size) でも cache が stale にならないよう hash に含める。
        let input_hash = hash_inputs((
            b"button",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            text,
            inside,
            visual_pressed,
            font_size.to_bits(),
            align as u8,
        ));
        self.with_widget_node(wid, input_hash, |ui| {
            let base = theme::CONTROL;
            let hover = theme::CONTROL_HOVER;
            let press = theme::ACCENT;

            let fill = if visual_pressed {
                press
            } else if inside {
                base.lerp(hover, 0.85)
            } else {
                base
            };

            ui.push_rect(RectCommand {
                rect,
                fill,
                border: theme::BORDER,
                border_width: 1.0,
                radius: [6.0; 4],
                clip_rect: None,
            });

            // テキストを矩形中央付近に (cosmic-text 経由の実 advance ベース)。
            // Nerd Font の wide glyph (⟳ ▶ ⏱ ♩ 等) は固定 9px / 文字 approx より広く、
            // approx で centering すると右ずれする (daw_01 #050)。
            // rect 幅を超えるラベル (例: 長い track 名) は ellipsis 省略 + 左寄せ + clip で
            // rect 外へはみ出させない (daw_01 #079)。 収まるテキストは Cow::Borrowed で
            // align に従う (Center は byte 完全互換、 Left は track 名用に左寄せ)。
            // Left は左マージン 4px を空ける (clip 名の left inset と同じ、 文字が rect 左端に
            // 張り付かない)。 省略幅も pad 分を差し引くので末尾 … も右端で余白を持つ。
            let line_h = font_size * 1.2;
            let left_pad = if align == ButtonTextAlign::Left { 4.0 } else { 0.0 };
            let avail_w = (rect.w - left_pad).max(1.0);
            let (display, text_w) = ui.fit_text_ellipsized(text, font_size, avail_w);
            let truncated = matches!(display, std::borrow::Cow::Owned(_));
            // 左寄せ位置: Left は rect.x + pad、 Center は省略時のみ flush 左寄せ (従来どおり)、
            // 収まる時は中央寄せ。
            let tx = match align {
                ButtonTextAlign::Left => rect.x + left_pad,
                ButtonTextAlign::Center if truncated => rect.x,
                ButtonTextAlign::Center => rect.x + (rect.w - text_w).max(0.0) * 0.5,
            };
            let ty = rect.y + (rect.h - line_h).max(0.0) * 0.5;
            ui.push_text(GlyphArea {
                text: display.as_ref().into(),
                left: tx,
                top: ty,
                font_size,
                line_height: line_h,
                color: theme::TEXT,
                clip_rect: truncated.then_some(rect),
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
    fn button_text_left_uses_measured_advance_not_approx() {
        // daw_01 #050 regression: `chars * 9.0` 固定 approx 廃止 → measure_text 化。
        // push_text の left が `(rect.w - measure_text) / 2` に一致することを確認。
        let font_size = 16.0_f32; // button.rs 内 hardcode と同期
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 20.0, y: 30.0, w: 100.0, h: 30.0 };

        let mut measured_w = 0.0;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            measured_w = ui.measure_text("M", font_size);
            let _ = ui.button_at_clicked("btn", "M", rect);
        });
        assert!(measured_w > 0.0, "measure_text(\"M\", 16) > 0");

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        let expected_left = rect.x + (rect.w - measured_w) * 0.5;
        assert!(
            (glyph.left - expected_left).abs() < 1e-3,
            "text left should match measured center: expected {expected_left}, got {}",
            glyph.left
        );

        // 旧 approx (1 * 9.0 = 9px) と measure (≥10px for "M") は明確に異なる。
        let approx_w = 1.0 * 9.0;
        let approx_left = rect.x + (rect.w - approx_w) * 0.5;
        assert!(
            (glyph.left - approx_left).abs() > 0.1,
            "text left must differ from approx-based centering (approx={approx_left}, got {})",
            glyph.left
        );
    }

    #[test]
    fn button_at_clicked_sized_renders_given_font_size_and_centers_by_measure() {
        // daw_01 #076: track 名 font を style.track_text_size に追従させる sized 版。
        // 指定 font_size で push され、 left は measure_text(font_size) で中央寄せされる。
        let font_size = 11.0_f32;
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 20.0, y: 30.0, w: 100.0, h: 30.0 };

        let mut measured_w = 0.0;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            measured_w = ui.measure_text("M", font_size);
            let _ = ui.button_at_clicked_sized("btn", "M", rect, font_size);
        });

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        assert!(
            (glyph.font_size - font_size).abs() < 1e-3,
            "sized button should render at given font_size: expected {font_size}, got {}",
            glyph.font_size
        );
        let expected_left = rect.x + (rect.w - measured_w) * 0.5;
        assert!(
            (glyph.left - expected_left).abs() < 1e-3,
            "text left should match measured center at given size: expected {expected_left}, got {}",
            glyph.left
        );
    }

    #[test]
    fn button_at_clicked_default_stays_16px() {
        // 汎用 button は font_size 16px 固定のまま (menu / dialog 等が依存、byte-compat)。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 20.0, y: 30.0, w: 100.0, h: 30.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.button_at_clicked("btn", "M", rect);
        });

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        assert!(
            (glyph.font_size - 16.0).abs() < 1e-3,
            "default button must stay 16px: got {}",
            glyph.font_size
        );
    }

    #[test]
    fn fit_text_ellipsized_borrows_when_fits_and_truncates_when_overflow() {
        // daw_01 #079: 収まる text は Cow::Borrowed + 実幅、 超える text は Cow::Owned +
        // max_w 以下の幅で末尾 ellipsis。
        use std::borrow::Cow;
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let font = 12.0_f32;

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            // (a) 収まる: Borrowed + 幅 = measure(full)。
            let full_w = ui.measure_text("Hi", font);
            let (d, w) = ui.fit_text_ellipsized("Hi", font, 200.0);
            assert!(matches!(d, Cow::Borrowed(_)), "収まる text は Borrowed");
            assert_eq!(d.as_ref(), "Hi");
            assert!((w - full_w).abs() < 1e-3, "Borrowed の幅は measure(full) と一致");

            // (b) 超える: Owned + max_w 以下 + ellipsis 終端 + full より短い幅。
            let long = "VeryLongTrackNameThatOverflows";
            let long_full = ui.measure_text(long, font);
            let max_w = 60.0_f32;
            assert!(long_full > max_w, "前提: full は max_w を超える");
            let (d2, w2) = ui.fit_text_ellipsized(long, font, max_w);
            assert!(matches!(d2, Cow::Owned(_)), "超える text は Owned");
            assert!(w2 <= max_w + 0.5, "省略幅 {w2} は max_w {max_w} 以下");
            assert!(w2 < long_full, "省略幅は full より狭い");
            assert!(
                d2.ends_with('…') || d2.ends_with("..."),
                "末尾は ellipsis: got {d2:?}"
            );
            assert!(d2.as_ref() != long, "文字列が短縮されている");

            // (c) ellipsis すら入らない極小 max_w: ellipsis のみ。
            let (d3, _) = ui.fit_text_ellipsized(long, font, 1.0);
            assert!(matches!(d3, Cow::Owned(_)));
            assert!(d3.as_ref() == "…" || d3.as_ref() == "...", "極小幅は ellipsis のみ: {d3:?}");
        });
    }

    #[test]
    fn button_long_text_truncates_left_aligned_and_clipped() {
        // daw_01 #079: rect に収まらない長い track 名は省略 + 左寄せ + clip_rect で
        // M/S/R へはみ出させない。
        let font = 12.0_f32;
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 20.0, y: 30.0, w: 50.0, h: 18.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.button_at_clicked_sized(
                "btn",
                "VeryLongTrackNameThatOverflowsTheNameArea",
                rect,
                font,
            );
        });

        // 次フレームで scene を再借用するので、 必要な値を先に owned で退避。
        let (glyph_left, glyph_clip, glyph_text) = {
            let glyph = scene.iter_glyphs().next().expect("text should be pushed");
            (glyph.left, glyph.clip_rect, glyph.text.to_string())
        };
        assert!(
            (glyph_left - rect.x).abs() < 1e-3,
            "省略時は左寄せ (left == rect.x): got {glyph_left}"
        );
        assert_eq!(glyph_clip, Some(rect), "省略時は clip_rect = Some(rect)");
        assert!(
            glyph_text.ends_with('…') || glyph_text.ends_with("..."),
            "省略名は ellipsis 終端: {glyph_text:?}"
        );
        // 描画文字列の実幅は rect.w を超えない (clip 前提の overshoot を最小化)。
        let mut w = 0.0;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            w = ui.measure_text(&glyph_text, font);
        });
        assert!(w <= rect.w + 0.5, "省略名の幅 {w} は rect.w {} 以下", rect.w);
    }

    #[test]
    fn button_short_text_stays_centered_without_clip() {
        // byte 互換: 収まる短ラベルは従来どおり中央寄せ + clip_rect None。
        let font = 12.0_f32;
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 20.0, y: 30.0, w: 100.0, h: 24.0 };

        let mut measured = 0.0;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            measured = ui.measure_text("Drums", font);
            let _ = ui.button_at_clicked_sized("btn", "Drums", rect, font);
        });

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        let expected_left = rect.x + (rect.w - measured) * 0.5;
        assert!(
            (glyph.left - expected_left).abs() < 1e-3,
            "収まる text は中央寄せ: expected {expected_left}, got {}",
            glyph.left
        );
        assert_eq!(glyph.clip_rect, None, "収まる text は clip_rect None (byte 互換)");
    }

    #[test]
    fn button_left_align_keeps_fitting_text_at_left_margin_without_clip() {
        // daw_01 #079: ButtonTextAlign::Left は収まるテキストも左寄せ (track 名用)。
        // 左マージン 4px を空ける (rect 左端に張り付かない)。 省略は起きないので clip 無し。
        use crate::widgets::button::ButtonTextAlign;
        let font = 12.0_f32;
        let left_pad = 4.0_f32;
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 25.0, y: 30.0, w: 120.0, h: 20.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.button_at_clicked_sized_aligned(
                "btn",
                "Drums",
                rect,
                font,
                ButtonTextAlign::Left,
            );
        });

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        assert!(
            (glyph.left - (rect.x + left_pad)).abs() < 1e-3,
            "Left align は rect.x + 左マージン: expected {}, got {}",
            rect.x + left_pad,
            glyph.left
        );
        assert_eq!(glyph.clip_rect, None, "収まる時は clip None");
    }

    #[test]
    fn button_left_align_long_text_truncates_within_margin() {
        // Left + 長名: rect.x + pad 起点で省略 + clip、 描画幅は rect.w - pad 以下
        // (= 右端にも pad 相当の余白が残り、 文字が rect 両端に張り付かない)。
        use crate::widgets::button::ButtonTextAlign;
        let font = 12.0_f32;
        let left_pad = 4.0_f32;
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 800, height: 600 };
        let rect = Rect { x: 25.0, y: 30.0, w: 60.0, h: 18.0 };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            let _ = ui.button_at_clicked_sized_aligned(
                "btn",
                "VeryLongTrackNameThatOverflows",
                rect,
                font,
                ButtonTextAlign::Left,
            );
        });

        let (gleft, gclip, gtext) = {
            let g = scene.iter_glyphs().next().expect("text pushed");
            (g.left, g.clip_rect, g.text.to_string())
        };
        assert!((gleft - (rect.x + left_pad)).abs() < 1e-3, "Left 省略も rect.x + pad 起点");
        assert_eq!(gclip, Some(rect), "省略時は clip Some(rect)");
        assert!(gtext.ends_with('…') || gtext.ends_with("..."), "ellipsis 終端: {gtext:?}");
        let mut w = 0.0;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            w = ui.measure_text(&gtext, font);
        });
        assert!(w <= rect.w - left_pad + 0.5, "省略幅 {w} は rect.w - pad 以下");
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
