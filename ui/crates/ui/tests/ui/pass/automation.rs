// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! `Ui::automation_curve` (M5.5) が `Clone`/`PartialEq`/`Hash`/`Default` 不要の Model に
//! 対してコンパイルすることを確認する。
//!
//! ライブラリの不変条件:
//! - `points: &[(f32, f32)]` は借用のみ
//! - `on_change(idx, pos) -> Edit<M>` で 1 点だけ更新する Edit を組み立てる (Vec 全体の
//!   copy 不要、no-Clone 不変条件と整合)

use daw_ui_core::{AutomationCurveStyle, Edit, FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

// 意図的に derive マクロを一切付けない non-Clone Model。
struct Model {
    points: Vec<(f32, f32)>,
}

fn main() {
    let mut host: UiHost<Model> = UiHost::no_redraw();
    let mut scene = Scene::new();
    let model = Model {
        points: vec![(0.0, 0.5), (0.25, 0.8), (0.5, 0.3), (0.75, 0.7), (1.0, 0.5)],
    };
    let screen = PhysicalSize { width: 1280, height: 600 };
    let rect = Rect { x: 16.0, y: 56.0, w: 1248.0, h: 488.0 };

    let _edits = host.frame_to_edits(&model, &mut scene, screen, FrameInput::default(), |m, ui| {
        let _ = ui.automation_curve(
            "main",
            rect,
            &m.points,
            AutomationCurveStyle::from_palette(ui.palette()),
            |idx, pos| {
                Edit::mutate(move |m: &mut Model| {
                    if idx < m.points.len() {
                        m.points[idx] = pos;
                    }
                })
            },
        );
    });
}
