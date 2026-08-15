//! examples/automation — M5.5 動作確認サンプル。
//!
//! `Ui::automation_curve` で 1 本のオートメーションカーブ (cubic Bezier flatten +
//! Catmull-Rom 自動 tangent) を表示し、各点を drag で編集する。
//!
//! 操作:
//! - ノードを drag で移動 (rect 内 [0, 1] 比率に clamp)

use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    AutomationCurveStyle, Edit, InputAccumulator, UiHost,
};
use daw_ui_platform::{AppEvent, AppHost, PhysicalSize, WindowBackend, winit_backend};
use daw_ui_renderer::{Color, Rect, Renderer, Scene};
use winit::window::WindowAttributes;

const HEADER_H: f32 = 56.0;
const FOOTER_H: f32 = 56.0;

// ----- Model -----

struct AutomationModel {
    points: Vec<(f32, f32)>,
    last_frame_ms: f32,
    last_action: String,
}

impl AutomationModel {
    fn new() -> Self {
        // 初期 6 点で sin curve を粗くサンプリング (x = 0..1、y = 0.5 + 0.4*sin(2πx))
        let n = 6;
        let points: Vec<(f32, f32)> = (0..n)
            .map(|i| {
                let x = i as f32 / (n - 1) as f32;
                let y = 0.5 + 0.4 * (x * std::f32::consts::TAU).sin();
                (x, y.clamp(0.0, 1.0))
            })
            .collect();
        Self {
            points,
            last_frame_ms: 0.0,
            last_action: "起動 (ノードを drag で移動)".to_string(),
        }
    }
}

fn curve_area(screen: PhysicalSize) -> Rect {
    let pad_x = 16.0;
    let w = (screen.width as f32 - pad_x * 2.0).max(100.0);
    let h = (screen.height as f32 - HEADER_H - FOOTER_H).max(100.0);
    Rect { x: pad_x, y: HEADER_H, w, h }
}

// ----- App -----

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<AutomationModel>,
    model: AutomationModel,
    scene: Scene,
    input: InputAccumulator,
    last_frame_start: Option<Instant>,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        window.set_title("daw-ui automation (M5.5)");
        Self {
            ui: UiHost::with_window(window.clone()),

            window,
            renderer,
            model: AutomationModel::new(),
            scene: Scene::new(),
            input: InputAccumulator::new(),
            last_frame_start: None,
        }
    }

    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let area = curve_area(screen);
        let input = self.input.take_input();

        let hud = format!(
            "frame {:>5.2}ms │ points {} │ last: {}",
            self.model.last_frame_ms,
            self.model.points.len(),
            self.model.last_action,
        );

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                ui.label_at(
                    "title",
                    "daw-ui automation_curve — M5.5 (cubic Bezier flatten + Catmull-Rom)",
                    16.0, 16.0, 18.0,
                    Color::rgb(0.95, 0.95, 0.97),
                );
                ui.label_at("hud", &hud, 16.0, 40.0, 13.0, Color::rgb(0.75, 0.78, 0.82));

                let resp = ui.automation_curve(
                    "main",
                    area,
                    &m.points,
                    AutomationCurveStyle::from_palette(ui.palette()),
                    |idx, pos| {
                        Edit::mutate(move |m: &mut AutomationModel| {
                            if idx < m.points.len() {
                                m.points[idx] = pos;
                                m.last_action = format!(
                                    "node {idx} → ({:.3}, {:.3})",
                                    pos.0, pos.1,
                                );
                            }
                        })
                    },
                );
                let _ = resp;

                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "ノードを drag で移動 (rect 内 [0..1] 比率で clamp)",
                    16.0, footer_y, 13.0,
                    Color::rgb(0.65, 0.68, 0.72),
                );
                ui.label_at(
                    "footer2",
                    &m.last_action,
                    16.0, footer_y + 18.0, 13.0,
                    Color::rgb(0.50, 0.55, 0.62),
                );
            },
        );

    }
}

impl AppHost for App {
    fn on_event(&mut self, ev: AppEvent) {
        self.input.ingest(&ev);
        match ev {
            AppEvent::Resized(size) => {
                self.renderer.resize(size);
                self.window.request_redraw();
            }
            AppEvent::PointerMoved(_)
            | AppEvent::PointerInput { .. }
            | AppEvent::Scroll(_)
            | AppEvent::Keyboard(_)
            | AppEvent::ModifiersChanged(_) => {
                self.window.request_redraw();
            }
            _ => {}
        }
    }

    fn on_render(&mut self) -> bool {
        let now = Instant::now();
        self.last_frame_start = Some(now);
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        if let Some(t) = self.last_frame_start.take() {
            self.model.last_frame_ms = t.elapsed().as_secs_f32() * 1000.0;
        }
        // edits / focus 変化時の追加描画は UiHost::with_window が自動で request_redraw
        // を呼ぶため、ここでは drag 等の連続再描画判定だけ書けばよい (automation は drag
        // 中も Edit が連続発火するので、library 側の自動 redraw でカバーされる)。
        false
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui automation (M5.5)")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 600.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
