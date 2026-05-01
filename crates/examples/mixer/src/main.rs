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
use daw_ui_core::{CheckboxResponse, FaderResponse, KnobResponse};
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
    /// M3 動作確認用: 3 ch のフェーダ値 (0..1)。
    faders: [f32; 3],
    /// M3 動作確認用: 3 ch の pan ノブ値 (0..1, 0.5 = center)。
    pans: [f32; 3],
    /// M3 動作確認用: 3 ch の mute フラグ。
    mutes: [bool; 3],
}

impl MixerModel {
    fn new() -> Self {
        Self {
            title: "daw-ui ミキサー (M1+M3 動作確認)".to_string(),
            count: 0,
            bench_active: false,
            last_action: "起動しました".to_string(),
            faders: [0.5, 0.7, 0.3],
            pans: [0.5, 0.4, 0.6],
            mutes: [false, false, false],
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

    /// `Ui::frame` で edits が出たかどうかを返す。出たなら呼び出し側で
    /// 直後に redraw を要求して、適用後の Model でもう 1 度ラベル等を
    /// 描画し直す必要がある (immediate-mode + Edit queue の常で、edits は
    /// 描画クロージャ後に apply されるので、この関数の `render` までの間に
    /// scene へ積まれているラベル文字列は 1 フレーム古い値になっている)。
    fn build_ui(&mut self) -> bool {
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

                // 3ch フェーダ (M3 動作確認)。
                let fader_w = 60.0;
                let fader_h = 200.0;
                let fader_top = 240.0;
                let fader_left = 320.0;
                ui.label_at(
                    "fader_label",
                    "M3: フェーダ (上下ドラッグ)",
                    fader_left,
                    fader_top - 28.0,
                    14.0,
                    Color::rgb(0.85, 0.88, 0.92),
                );
                let knob_size = 56.0;
                let knob_top = fader_top + fader_h + 28.0;
                ui.label_at(
                    "knob_label",
                    "M3: pan ノブ (0.5 = center)",
                    fader_left,
                    knob_top - 22.0,
                    14.0,
                    Color::rgb(0.85, 0.88, 0.92),
                );
                for i in 0..3 {
                    let rect = Rect {
                        x: fader_left + i as f32 * (fader_w + 16.0),
                        y: fader_top,
                        w: fader_w,
                        h: fader_h,
                    };
                    let resp: FaderResponse =
                        ui.fader_at(("ch_fader", i), rect, m.faders[i], move |v| {
                            Edit::mutate(move |m: &mut MixerModel| {
                                m.faders[i] = v;
                                m.last_action = format!("ch{} fader = {v:.2}", i + 1);
                            })
                        });
                    let percent = (resp.displayed_value * 100.0).round() as i32;
                    ui.label_at(
                        ("fader_pct", i),
                        &format!("ch{}\n{percent}%", i + 1),
                        rect.x,
                        rect.y + rect.h + 6.0,
                        12.0,
                        if resp.dragging {
                            Color::rgb(0.95, 0.97, 1.0)
                        } else {
                            Color::rgb(0.65, 0.68, 0.72)
                        },
                    );

                    // pan knob (fader と同じ列、下に配置)
                    let knob_rect = Rect {
                        x: rect.x + (fader_w - knob_size) * 0.5,
                        y: knob_top,
                        w: knob_size,
                        h: knob_size,
                    };
                    let kresp: KnobResponse =
                        ui.knob_at(("ch_pan", i), knob_rect, m.pans[i], move |v| {
                            Edit::mutate(move |m: &mut MixerModel| {
                                m.pans[i] = v;
                                let lr = (v - 0.5) * 2.0; // -1..1
                                m.last_action = if lr.abs() < 0.02 {
                                    format!("ch{} pan = C", i + 1)
                                } else if lr < 0.0 {
                                    format!("ch{} pan = L{:.0}", i + 1, lr.abs() * 100.0)
                                } else {
                                    format!("ch{} pan = R{:.0}", i + 1, lr * 100.0)
                                };
                            })
                        });
                    let lr = (kresp.displayed_value - 0.5) * 2.0;
                    let pan_label = if lr.abs() < 0.02 {
                        "C".to_string()
                    } else if lr < 0.0 {
                        format!("L{:.0}", lr.abs() * 100.0)
                    } else {
                        format!("R{:.0}", lr * 100.0)
                    };
                    ui.label_at(
                        ("pan_label", i),
                        &pan_label,
                        knob_rect.x,
                        knob_rect.y + knob_rect.h + 4.0,
                        12.0,
                        if kresp.dragging {
                            Color::rgb(0.95, 0.97, 1.0)
                        } else {
                            Color::rgb(0.65, 0.68, 0.72)
                        },
                    );

                    // mute checkbox (knob のさらに下)
                    let cb_rect = Rect {
                        x: rect.x,
                        y: knob_rect.y + knob_rect.h + 26.0,
                        w: fader_w,
                        h: 24.0,
                    };
                    let _cresp: CheckboxResponse = ui.checkbox_at(
                        ("ch_mute", i),
                        cb_rect,
                        m.mutes[i],
                        "Mute",
                        move |new| {
                            Edit::mutate(move |m: &mut MixerModel| {
                                m.mutes[i] = new;
                                m.last_action = format!(
                                    "ch{} mute = {}",
                                    i + 1,
                                    if new { "ON" } else { "OFF" }
                                );
                            })
                        },
                    );
                }
            },
        );

        let had_edits = !edits.is_empty();
        for e in edits {
            e.apply(&mut self.model);
        }
        had_edits
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
        let had_edits = self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        // edits が出たら次フレームで適用後 Model のラベルを描き直す。
        if had_edits {
            self.window.request_redraw();
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
