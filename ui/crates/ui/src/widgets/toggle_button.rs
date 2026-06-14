//! `toggle_button` ウィジェット — ON/OFF state を持つ矩形 button (M9 Phase 45b)。
//!
//! `checkbox` との違い:
//! - `checkbox`: 16px の □/☑ 枠 + ラベル (boolean property toggle の意味的アフォーダンス)
//! - `toggle_button`: 矩形全面に任意ラベル + ON/OFF 背景色変化
//!
//! click 判定は `button` と同じ armed-state モデル (`press_started_inside`)。

use std::hash::Hash;

use daw_ui_renderer::{Color, GlyphArea, Rect, RectCommand};

use crate::edit::Edit;
use crate::id::WidgetId;
use crate::scenegraph::hash_inputs;
use crate::ui::{Ui, lerp_color};

/// `toggle_button_at` の永続状態 (button / checkbox と同形)。
#[derive(Debug, Default)]
pub(crate) struct ToggleButtonState {
    press_started_inside: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ToggleButtonResponse {
    /// このフレームで click を検出 (`Edit<M>` 発行と同じフレームで `true`)。
    pub toggled: bool,
    pub hovered: bool,
}

/// `toggle_button_at` の見た目スタイル。 ON/OFF は `on_color` / `off_color` の背景色のみで表現する。
#[derive(Clone, Copy, Debug)]
pub struct ToggleButtonStyle {
    pub off_color: Color,
    pub on_color: Color,
    pub border: Color,
    pub border_width: f32,
    pub radius: f32,
    pub font_size: f32,
    /// `value=false` (off) の text 色、 および `on_text_color: None` のとき `value=true` の
    /// fallback。
    pub text_color: Color,
    /// `value=true` (on) のときの text 色。 `None` なら `text_color` を使う (back compat)。
    /// daw_01 #051: metronome の Ableton 流「黄背景 + 黒文字」 のような state-dependent text
    /// color が必要な toggle 用 (yellow on_color に white text_color では視認性が低い)。
    pub on_text_color: Option<Color>,
}

impl Default for ToggleButtonStyle {
    fn default() -> Self {
        Self {
            off_color: Color::rgb(0.18, 0.20, 0.26),
            on_color: Color::rgb(0.32, 0.55, 0.85),
            border: Color::rgb(0.35, 0.38, 0.45),
            border_width: 1.0,
            radius: 6.0,
            font_size: 14.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
            on_text_color: None,
        }
    }
}

impl<'a, M: ?Sized + 'static> Ui<'a, M> {
    /// 矩形指定で toggle_button を描画 + ヒットテスト。
    /// click 時に `on_toggle(!value)` の返り値を Edit 列に積む。
    pub fn toggle_button_at<F>(
        &mut self,
        id: impl Hash,
        text: &str,
        rect: Rect,
        value: bool,
        style: &ToggleButtonStyle,
        on_toggle: F,
    ) -> ToggleButtonResponse
    where
        F: FnOnce(bool) -> Edit<M>,
    {
        let wid = WidgetId::ROOT.child((b"toggle_button", &id));
        let pointer = self.pointer;
        let inside = pointer.pos.is_some_and(|(px, py)| rect.contains(px, py));

        // armed-state click 判定 (button / checkbox と同じモデル)。
        let (visual_pressed, click) = {
            let state: &mut ToggleButtonState = self.widget_state(wid);
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

        // style 全体を u64 に sub-hash してから master hash に組み込む
        // (タプル要素数が 12 を超えるため)。
        let style_hash = hash_inputs((
            [
                style.off_color.r.to_bits(),
                style.off_color.g.to_bits(),
                style.off_color.b.to_bits(),
                style.off_color.a.to_bits(),
                style.on_color.r.to_bits(),
                style.on_color.g.to_bits(),
                style.on_color.b.to_bits(),
                style.on_color.a.to_bits(),
                style.border.r.to_bits(),
                style.border.g.to_bits(),
                style.border.b.to_bits(),
                style.border.a.to_bits(),
                style.text_color.r.to_bits(),
                style.text_color.g.to_bits(),
                style.text_color.b.to_bits(),
                style.text_color.a.to_bits(),
            ],
            [
                style.border_width.to_bits(),
                style.radius.to_bits(),
                style.font_size.to_bits(),
            ],
            style
                .on_text_color
                .map(|c| (c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits())),
        ));

        let input_hash = hash_inputs((
            b"toggle_button",
            rect.x.to_bits(),
            rect.y.to_bits(),
            rect.w.to_bits(),
            rect.h.to_bits(),
            text,
            inside,
            visual_pressed,
            value,
            style_hash,
        ));

        let style_copy = *style;
        self.with_widget_node(wid, input_hash, |ui| {
            draw_toggle_button(ui, text, rect, value, inside, visual_pressed, &style_copy);
        });

        if click {
            let edit = on_toggle(!value);
            self.push_edit(edit);
        }

        ToggleButtonResponse { toggled: click, hovered: inside }
    }
}

fn draw_toggle_button<M: ?Sized + 'static>(
    ui: &mut Ui<'_, M>,
    text: &str,
    rect: Rect,
    value: bool,
    hovered: bool,
    pressed: bool,
    style: &ToggleButtonStyle,
) {
    // 背景色: value で off/on を選び、hover で明るく、press で暗く。
    let base = if value { style.on_color } else { style.off_color };
    let hover_c = lerp_color(base, Color::rgb(1.0, 1.0, 1.0), 0.10);
    let press_c = lerp_color(base, Color::rgb(0.0, 0.0, 0.0), 0.20);

    let fill = if pressed {
        press_c
    } else if hovered {
        hover_c
    } else {
        base
    };

    ui.push_rect(RectCommand {
        rect,
        fill,
        border: style.border,
        border_width: style.border_width,
        radius: [style.radius; 4],
        clip_rect: None,
    });

    // text 中央配置 (cosmic-text 経由の実 advance ベース)。 Nerd Font の wide glyph
    // (⟳ ▶ ⏱ ♩ 等) は ASCII proportional の `font_size * 0.55` approx より大幅に広いため、
    // approx で centering すると右ずれする (daw_01 #050)。 `measure_text` は scratch buffer
    // を使い回すので per-frame N 個 button でも cost は無視可能。
    // rect 幅を超えるラベルは ellipsis 省略 + 左寄せ + clip (daw_01 #079)。 M/S/R 等
    // 1 文字ラベルは収まるので Cow::Borrowed = 従来どおり中央寄せ・clip 無しで byte 互換。
    let line_h = style.font_size * 1.2;
    let (display, text_w) = ui.fit_text_ellipsized(text, style.font_size, rect.w);
    let truncated = matches!(display, std::borrow::Cow::Owned(_));
    let tx = if truncated {
        rect.x
    } else {
        rect.x + (rect.w - text_w).max(0.0) * 0.5
    };
    let ty = rect.y + (rect.h - line_h).max(0.0) * 0.5;
    // value=true のとき on_text_color (Some) を優先、 None なら text_color に fallback
    // (daw_01 #051: metronome 黄背景 + 黒文字のような state-dependent text color)。
    let text_color = if value {
        style.on_text_color.unwrap_or(style.text_color)
    } else {
        style.text_color
    };
    ui.push_text(GlyphArea {
        text: display.as_ref().into(),
        left: tx,
        top: ty,
        font_size: style.font_size,
        line_height: line_h,
        color: text_color,
        clip_rect: truncated.then_some(rect),
        ..GlyphArea::default()
    });
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use daw_ui_platform::PhysicalSize;
    use daw_ui_renderer::{Color, Rect, Scene};

    use super::ToggleButtonStyle;
    use crate::edit::Edit;
    use crate::input::{FrameInput, PointerFrame};
    use crate::ui::UiHost;

    struct Toy {
        flag: bool,
    }

    fn click_at(rect: Rect) -> (PointerFrame, PointerFrame) {
        // 1 frame: press inside
        let press = PointerFrame {
            pos: Some((rect.x + rect.w * 0.5, rect.y + rect.h * 0.5)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        };
        // 1 frame: release inside
        let release = PointerFrame {
            pos: Some((rect.x + rect.w * 0.5, rect.y + rect.h * 0.5)),
            primary_just_released: true,
            primary_pressed: false,
            ..PointerFrame::default()
        };
        (press, release)
    }

    #[test]
    fn click_emits_toggle_with_negated_value() {
        let mut host: UiHost<Toy> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let mut model = Toy { flag: false };
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 10.0, y: 10.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle::default();
        let toggled_seen = Cell::new(false);

        let (press, release) = click_at(rect);

        host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput { pointer: press, ..Default::default() },
            |_, ui| {
                let resp = ui.toggle_button_at("t", "M", rect, model.flag, &style, |new| {
                    Edit::mutate(move |m: &mut Toy| {
                        m.flag = new;
                    })
                });
                if resp.toggled {
                    toggled_seen.set(true);
                }
            },
        );
        assert!(!toggled_seen.get(), "press のみでは click 未発火");

        let edits = host.frame_to_edits(
            &model,
            &mut scene,
            screen,
            FrameInput { pointer: release, ..Default::default() },
            |_, ui| {
                let resp = ui.toggle_button_at("t", "M", rect, model.flag, &style, |new| {
                    Edit::mutate(move |m: &mut Toy| {
                        m.flag = new;
                    })
                });
                if resp.toggled {
                    toggled_seen.set(true);
                }
            },
        );
        assert!(toggled_seen.get(), "release で click 発火");
        for e in edits {
            e.apply(&mut model);
        }
        assert!(model.flag, "value=false → on_toggle(true) で flag=true");
    }

    #[test]
    fn text_left_uses_measured_advance_not_approx() {
        // daw_01 #050 regression: `chars * font_size * 0.55` approx 廃止 → measure_text 化。
        // approx で右ずれする wide glyph (ASCII の "M" でも proportional system font なら
        // 0.55 * 16 = 8.8px より広い) で push_text の left が `(rect.w - measure_text) / 2`
        // に一致することを確認する。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 10.0, y: 20.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle::default();

        let mut measured_w = 0.0;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            measured_w = ui.measure_text("M", style.font_size);
            ui.toggle_button_at("t", "M", rect, false, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });
        assert!(measured_w > 0.0, "measure_text(\"M\", 14) > 0");

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        let expected_left = rect.x + (rect.w - measured_w) * 0.5;
        assert!(
            (glyph.left - expected_left).abs() < 1e-3,
            "text left should match measured center: expected {expected_left}, got {}",
            glyph.left
        );

        // approx (= 1 * 14 * 0.55 = 7.7) と measure (≥10px for system M) は明確に異なる。
        // = approx 配置と measure 配置で left が一致しないことを確認 (回帰検出)。
        let approx_w = 1.0 * style.font_size * 0.55;
        let approx_left = rect.x + (rect.w - approx_w) * 0.5;
        assert!(
            (glyph.left - approx_left).abs() > 0.1,
            "text left must differ from approx-based centering (approx={approx_left}, got {})",
            glyph.left
        );
    }

    #[test]
    fn text_color_uses_on_text_color_when_value_true_and_some() {
        // daw_01 #051: value=true で on_text_color=Some なら on_text_color が
        // push_text に使われる。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle {
            text_color: Color::rgb(0.95, 0.95, 0.97), // white (off)
            on_text_color: Some(Color::rgb(0.10, 0.10, 0.12)), // black (on)
            ..ToggleButtonStyle::default()
        };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("t", "M", rect, true, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        // 黒 (= on_text_color) が選ばれる、 白 (= text_color) ではない。
        assert!(glyph.color.r < 0.5, "value=true で on_text_color (0.10) が使われる");
        assert!((glyph.color.r - 0.10).abs() < 1e-6);
    }

    #[test]
    fn text_color_falls_back_to_text_color_when_on_text_color_none() {
        // daw_01 #051 back compat: on_text_color=None なら value=true でも text_color。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle {
            text_color: Color::rgb(0.95, 0.95, 0.97), // white
            on_text_color: None,
            ..ToggleButtonStyle::default()
        };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("t", "M", rect, true, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        assert!((glyph.color.r - 0.95).abs() < 1e-6, "on_text_color=None で text_color を fallback");
    }

    #[test]
    fn text_color_uses_text_color_when_value_false() {
        // daw_01 #051: value=false なら必ず text_color (on_text_color は無視)。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle {
            text_color: Color::rgb(0.95, 0.95, 0.97), // white (off で使われる)
            on_text_color: Some(Color::rgb(0.10, 0.10, 0.12)), // black (off では無視)
            ..ToggleButtonStyle::default()
        };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("t", "M", rect, false, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });

        let glyph = scene.iter_glyphs().next().expect("text should be pushed");
        // off では text_color (= 白 0.95) のはず。
        assert!((glyph.color.r - 0.95).abs() < 1e-6, "value=false で text_color");
    }

    #[test]
    fn long_label_truncates_left_aligned_and_clipped_short_stays_centered() {
        // daw_01 #079: rect 超えラベルは省略 + 左寄せ + clip。 M/S/R 等の 1 文字は
        // 中央寄せ + clip None で byte 互換。
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 400, height: 100 };
        let style = ToggleButtonStyle { font_size: 11.0, ..ToggleButtonStyle::default() };
        let rect = Rect { x: 10.0, y: 10.0, w: 40.0, h: 18.0 };

        // (a) 長いラベル → 省略。
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("t", "RecordArmAllInputs", rect, false, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });
        let glyph = scene.iter_glyphs().next().expect("text pushed");
        assert!((glyph.left - rect.x).abs() < 1e-3, "省略時は左寄せ");
        assert_eq!(glyph.clip_rect, Some(rect), "省略時は clip Some");
        assert!(
            glyph.text.ends_with('…') || glyph.text.ends_with("..."),
            "ellipsis 終端: {:?}",
            glyph.text
        );

        // (b) 1 文字 "R" → 中央寄せ + clip None (byte 互換)。
        scene.clear();
        let mut measured = 0.0;
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            measured = ui.measure_text("R", style.font_size);
            ui.toggle_button_at("t2", "R", rect, false, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });
        let g2 = scene.iter_glyphs().next().expect("text pushed");
        let expected_left = rect.x + (rect.w - measured) * 0.5;
        assert!((g2.left - expected_left).abs() < 1e-3, "短ラベルは中央寄せ");
        assert_eq!(g2.clip_rect, None, "短ラベルは clip None (byte 互換)");
    }

    #[test]
    fn fill_color_swaps_between_on_off() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle {
            off_color: Color::rgb(0.10, 0.10, 0.10),
            on_color: Color::rgb(0.90, 0.10, 0.10),
            ..ToggleButtonStyle::default()
        };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("a", "OFF", rect, false, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });
        let off_r = scene.rects_vec()[0].fill.r;

        scene.clear();
        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("b", "ON", rect, true, &style, |_| {
                Edit::mutate(|(): &mut ()| {})
            });
        });
        let on_r = scene.rects_vec()[0].fill.r;

        assert!(off_r < 0.5);
        assert!(on_r > 0.5);
    }
}
