//! Bottom panel: Mixer / Piano Roll を切り替えるタブ + 中身。
//!
//! タブ: 上端に Mixer / Piano Roll の 2 ボタン。背景色で active を表示。
//! 中身: app.bottom_panel に応じて mixer_strips::draw か piano_roll::draw +
//! lyric_panel::draw。

use daw_ui_core::{Edit, Ui};
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::{AppData, AppEvent};
use crate::view::{lyric_panel, mixer_strips, piano_roll_view};

const TAB_H: f32 = 26.0;
const LYRIC_W: f32 = 240.0;

const COLOR_TAB_BG: Color = Color { r: 0.13, g: 0.13, b: 0.16, a: 1.0 };
const COLOR_TAB_INACTIVE: Color = Color { r: 0.18, g: 0.18, b: 0.22, a: 1.0 };
const COLOR_TAB_ACTIVE: Color = Color { r: 0.27, g: 0.40, b: 0.55, a: 1.0 };

pub fn draw(app: &AppData, ui: &mut Ui<'_, AppData>, area: Rect) {
    // タブストリップ背景
    ui.heavy("bp_tab_bg", |hctx| {
        hctx.cached(area.w.to_bits(), |hctx| {
            hctx.push_rect(RectCommand {
                rect: Rect { x: area.x, y: area.y, w: area.w, h: TAB_H },
                fill: COLOR_TAB_BG,
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    // タブボタンの active 背景帯。
    let tab_w = 96.0;
    let panel = app.bottom_panel;
    for (i, label) in [(0u8, "Mixer"), (1u8, "Piano Roll")].iter().enumerate() {
        let bx = area.x + (i as f32) * tab_w;
        let active = panel == label.0;
        if active {
            ui.heavy(("bp_tab_active", label.0 as usize), |hctx| {
                hctx.cached(active, |hctx| {
                    hctx.push_rect(RectCommand {
                        rect: Rect { x: bx, y: area.y, w: tab_w, h: TAB_H },
                        fill: COLOR_TAB_ACTIVE,
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                });
            });
        }
        let target = label.0;
        ui.button_at(
            ("bp_tab", label.0 as usize),
            label.1,
            Rect { x: bx, y: area.y, w: tab_w, h: TAB_H },
            move || {
                Edit::mutate(move |app: &mut AppData| {
                    app.handle_event(AppEvent::SelectBottomPanel(target))
                })
            },
        );
        let _ = COLOR_TAB_INACTIVE;
    }

    // パネル本体
    let body = Rect {
        x: area.x,
        y: area.y + TAB_H,
        w: area.w,
        h: (area.h - TAB_H).max(0.0),
    };
    match panel {
        1 => {
            // Piano roll + lyric panel
            let pr_area = Rect {
                x: body.x,
                y: body.y,
                w: (body.w - LYRIC_W).max(0.0),
                h: body.h,
            };
            let lyr_area = Rect {
                x: body.x + body.w - LYRIC_W,
                y: body.y,
                w: LYRIC_W,
                h: body.h,
            };
            piano_roll_view::draw(app, ui, pr_area);
            lyric_panel::draw(app, ui, lyr_area);
        }
        _ => {
            mixer_strips::draw(app, ui, body);
        }
    }
}
