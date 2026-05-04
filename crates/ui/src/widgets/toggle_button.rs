//! `toggle_button` ウィジェット — ON/OFF state を持つ矩形 button (M9 Phase 45b)。
//!
//! `checkbox` との違い:
//! - `checkbox`: 16px の □/☑ 枠 + ラベル (boolean property toggle の意味的アフォーダンス)
//! - `toggle_button`: 矩形全面に任意ラベル + ON/OFF 背景色変化 + 任意の hint band
//!   (DAW の M=赤 / S=黄 のように **value=true で rect 下端に色帯** を出す DAW 慣習を style で吸収)
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

/// `toggle_button_at` の見た目スタイル。
///
/// DAW の M=赤 / S=黄 のような「ON のとき下端に色帯」は `hint_band: Some(...)` で表現する。
/// `hint_band: None` なら純粋な ON/OFF トグル button (背景色のみ変化)。
#[derive(Clone, Copy, Debug)]
pub struct ToggleButtonStyle {
    pub off_color: Color,
    pub on_color: Color,
    /// `value=true` のとき rect 下端 `hint_band_h` px に塗る帯の色。
    /// `None` なら hint band なし。
    pub hint_band: Option<Color>,
    pub hint_band_h: f32,
    pub border: Color,
    pub border_width: f32,
    pub radius: f32,
    pub font_size: f32,
    pub text_color: Color,
}

impl Default for ToggleButtonStyle {
    fn default() -> Self {
        Self {
            off_color: Color::rgb(0.18, 0.20, 0.26),
            on_color: Color::rgb(0.32, 0.55, 0.85),
            hint_band: None,
            hint_band_h: 2.0,
            border: Color::rgb(0.35, 0.38, 0.45),
            border_width: 1.0,
            radius: 6.0,
            font_size: 14.0,
            text_color: Color::rgb(0.95, 0.95, 0.97),
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
            style
                .hint_band
                .map(|c| (c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits())),
            [
                style.hint_band_h.to_bits(),
                style.border_width.to_bits(),
                style.radius.to_bits(),
                style.font_size.to_bits(),
            ],
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

    // text 中央配置 (button と同じ簡易配置、ASCII 幅 ≈ font_size*0.55)。
    let line_h = style.font_size * 1.2;
    let approx_w = (text.chars().count() as f32) * (style.font_size * 0.55);
    let tx = rect.x + (rect.w - approx_w).max(0.0) * 0.5;
    let ty = rect.y + (rect.h - line_h).max(0.0) * 0.5;
    ui.push_text(GlyphArea {
        text: text.to_string(),
        left: tx,
        top: ty,
        font_size: style.font_size,
        line_height: line_h,
        color: style.text_color,
        clip_rect: None,
    });

    // hint band: value=true && Some && hint_band_h > 0 のとき rect 下端に塗る。
    if value
        && let Some(hint) = style.hint_band
        && style.hint_band_h > 0.0
    {
        let h = style.hint_band_h.min(rect.h);
        ui.push_rect(RectCommand {
            rect: Rect { x: rect.x, y: rect.y + rect.h - h, w: rect.w, h },
            fill: hint,
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }
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
    fn hint_band_appears_when_value_true_and_some() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle {
            hint_band: Some(Color::rgb(1.0, 0.0, 0.0)),
            ..ToggleButtonStyle::default()
        };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("t", "M", rect, true, &style, |_| Edit::mutate(|(): &mut ()| {}));
        });

        // 1: 本体 / 2: hint band
        assert_eq!(scene.rect_count(), 2);
        let rects = scene.rects_vec();
        let band = &rects[1];
        assert!((band.rect.h - style.hint_band_h).abs() < 1e-6);
        assert!((band.rect.y + band.rect.h - rect.h).abs() < 1e-6);
        assert!((band.fill.r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn hint_band_absent_when_value_false() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle {
            hint_band: Some(Color::rgb(1.0, 0.0, 0.0)),
            ..ToggleButtonStyle::default()
        };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("t", "M", rect, false, &style, |_| Edit::mutate(|(): &mut ()| {}));
        });

        assert_eq!(scene.rect_count(), 1, "value=false なら hint band は出ない");
    }

    #[test]
    fn hint_band_absent_when_style_none() {
        let mut host: UiHost<()> = UiHost::no_redraw();
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: 200, height: 100 };
        let rect = Rect { x: 0.0, y: 0.0, w: 60.0, h: 24.0 };
        let style = ToggleButtonStyle { hint_band: None, ..ToggleButtonStyle::default() };

        host.frame_to_edits(&(), &mut scene, screen, FrameInput::default(), |(), ui| {
            ui.toggle_button_at("t", "M", rect, true, &style, |_| Edit::mutate(|(): &mut ()| {}));
        });

        assert_eq!(scene.rect_count(), 1, "hint_band: None なら value=true でも帯なし");
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
            hint_band: None,
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
