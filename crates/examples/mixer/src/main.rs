//! examples/mixer — M1 動作確認サンプル。
//!
//! 確認項目:
//! - winit でウィンドウが開く
//! - wgpu instanced rect で 1 万矩形を 60fps で描画
//! - glyphon で日本語テキストが出る
//! - ボタンを taffy 経由でレイアウトしてヒットテストできる
//! - Edit がアプリ側 Model を変更する (ライブラリは Model を Clone しない)

use std::sync::Arc;

use daw_ui_core::{Edit, InputAccumulator, UiHost};
use daw_ui_platform::{AppEvent, AppHost, PhysicalSize, WindowBackend, winit_backend};
use daw_ui_renderer::{Color, Rect, RectCommand, Renderer, Scene};
use winit::window::WindowAttributes;

/// アプリの「GUI Model」。`Clone` は実装していないことに注意 (本ライブラリの不変条件)。
struct MixerModel {
    title: String,
    count: u32,
    /// 1 万矩形ベンチを ON にするか。
    bench_active: bool,
    /// メッセージ。
    last_action: String,
}

impl MixerModel {
    fn new() -> Self {
        Self {
            title: "daw-ui ミキサー (M1 動作確認)".to_string(),
            count: 0,
            bench_active: false,
            last_action: "起動しました".to_string(),
        }
    }
}

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<MixerModel>,
    model: MixerModel,
    scene: Scene,
    input: InputAccumulator,
    /// 起動からの経過フレーム数 (デバッグ用)。
    frames: u64,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        let ui = UiHost::<MixerModel>::new();
        let model = MixerModel::new();
        let scene = Scene::new();
        let input = InputAccumulator::new();
        // タイトルを反映
        window.set_title(&model.title);
        Self {
            window,
            renderer,
            ui,
            model,
            scene,
            input,
            frames: 0,
        }
    }

    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let pointer = self.input.take_frame();

        // 1 万矩形ベンチ: bench_active なら 100x100 グリッドを背景に積む
        if self.model.bench_active {
            let cols = 100;
            let rows = 100;
            let cell_w = (screen.width as f32) / cols as f32;
            let cell_h = (screen.height as f32) / rows as f32;
            for j in 0..rows {
                for i in 0..cols {
                    let t = ((self.frames as f32 * 0.02).sin() * 0.5 + 0.5)
                        * (((i + j) as f32 * 0.07).cos() * 0.5 + 0.5);
                    self.scene.push_rect(RectCommand {
                        rect: Rect {
                            x: i as f32 * cell_w,
                            y: j as f32 * cell_h,
                            w: cell_w - 1.0,
                            h: cell_h - 1.0,
                        },
                        fill: Color::rgba(0.15 + t * 0.4, 0.20 + t * 0.3, 0.30 + t * 0.4, 1.0),
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [2.0; 4],
                    });
                }
            }
        }

        let edits = self.ui.frame(
            &self.model,
            &mut self.scene,
            screen,
            pointer,
            |m, ui| {
                ui.label("title", &m.title);
                ui.label("count", &format!("クリック回数: {}", m.count));
                ui.label("status", &m.last_action);

                ui.button("inc", "カウント+1", || {
                    Edit::mutate(|m: &mut MixerModel| {
                        m.count += 1;
                        m.last_action = "カウント増加".to_string();
                    })
                });

                ui.button("reset", "リセット", || {
                    Edit::mutate(|m: &mut MixerModel| {
                        m.count = 0;
                        m.last_action = "リセット".to_string();
                    })
                });

                let bench_label = if m.bench_active {
                    "ベンチ停止 (1万矩形)"
                } else {
                    "ベンチ開始 (1万矩形)"
                };
                ui.button("bench", bench_label, || {
                    Edit::mutate(|m: &mut MixerModel| {
                        m.bench_active = !m.bench_active;
                        m.last_action = if m.bench_active {
                            "1万矩形ベンチ開始".to_string()
                        } else {
                            "ベンチ停止".to_string()
                        };
                    })
                });
            },
        );

        for e in edits {
            e.apply(&mut self.model);
        }
    }
}

impl AppHost for App {
    fn on_event(&mut self, ev: AppEvent) {
        self.input.ingest(&ev);
        match ev {
            AppEvent::Resized(PhysicalSize { width, height }) => {
                self.renderer.resize(PhysicalSize { width, height });
                self.window.request_redraw();
            }
            AppEvent::PointerMoved(_)
            | AppEvent::PointerInput { .. }
            | AppEvent::Scroll(_)
            | AppEvent::Keyboard(_) => {
                self.window.request_redraw();
            }
            _ => {}
        }
    }

    fn on_render(&mut self) -> bool {
        self.frames = self.frames.wrapping_add(1);
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        // bench 中は連続再描画
        self.model.bench_active
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui mixer (M1)")
        .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
