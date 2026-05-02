//! ルート view: 画面全体を Transport / Inspector / Arrangement / BottomPanel /
//! StatusBar に分割し、各 sub view を呼ぶ。Plugin picker / help は modal overlay。

use daw_ui_core::Ui;
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Color, Rect, RectCommand};

use crate::app::AppData;
use crate::view::{
    arrangement_view, bottom_panel, plugin_picker, status_bar, track_inspector, transport,
};

pub const TRANSPORT_H: f32 = 44.0;
pub const STATUS_H: f32 = 24.0;
pub const BOTTOM_H: f32 = 240.0;
pub const INSPECTOR_W: f32 = 280.0;

pub fn build_root(app: &AppData, ui: &mut Ui<'_, AppData>, screen: PhysicalSize) {
    let sw = screen.width as f32;
    let sh = screen.height as f32;

    // 全画面背景。
    ui.heavy("root_bg", |hctx| {
        hctx.cached((screen.width, screen.height), |hctx| {
            hctx.push_rect(RectCommand {
                rect: Rect { x: 0.0, y: 0.0, w: sw, h: sh },
                fill: Color::rgb(0.10, 0.10, 0.12),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        });
    });

    // ----- レイアウト計算 -----
    let transport_rect = Rect { x: 0.0, y: 0.0, w: sw, h: TRANSPORT_H };
    let bottom_rect_h = BOTTOM_H.min((sh - TRANSPORT_H - STATUS_H - 100.0).max(120.0));
    let center_h = (sh - TRANSPORT_H - bottom_rect_h - STATUS_H).max(0.0);
    let inspector_rect = Rect {
        x: 0.0,
        y: TRANSPORT_H,
        w: INSPECTOR_W,
        h: center_h,
    };
    let arrangement_rect = Rect {
        x: INSPECTOR_W,
        y: TRANSPORT_H,
        w: (sw - INSPECTOR_W).max(0.0),
        h: center_h,
    };
    let bottom_rect = Rect {
        x: 0.0,
        y: TRANSPORT_H + center_h,
        w: sw,
        h: bottom_rect_h,
    };
    let status_rect = Rect {
        x: 0.0,
        y: sh - STATUS_H,
        w: sw,
        h: STATUS_H,
    };

    transport::draw(app, ui, transport_rect);
    track_inspector::draw(app, ui, inspector_rect);
    arrangement_view::draw(app, ui, arrangement_rect);
    bottom_panel::draw(app, ui, bottom_rect);
    status_bar::draw(app, ui, status_rect);

    // Modal: plugin picker。最後に描く (上に乗せる)。
    if app.is_plugin_picker_open {
        plugin_picker::draw(app, ui, screen);
    }
}
