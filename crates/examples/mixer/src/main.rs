//! examples/mixer — M1 動作確認サンプル。
//!
//! 確認項目:
//! - winit でウィンドウが開く
//! - wgpu instanced rect で 1 万矩形を 60fps で描画
//! - glyphon で日本語テキストが出る
//! - ボタンを taffy 経由でレイアウトしてヒットテストできる
//! - Edit がアプリ側 Model を変更する (ライブラリは Model を Clone しない)

use std::sync::Arc;

use daw_ui_core::{Edit, FlexDirection, Gap, InputAccumulator, LayoutPass, NodeId, Padding, UiHost};
use daw_ui_platform::{AppEvent, AppHost, PhysicalSize, WindowBackend, winit_backend};
use daw_ui_core::{FaderResponse, KnobResponse, TextInputResponse};
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
    /// M3 動作確認用: 8 ch のフェーダ値 (0..1, ダブルクリックで 0.0)。
    faders: [f32; 8],
    /// M3 動作確認用: 8 ch の pan ノブ値 (0..1, 0.5 = center, ダブルクリックで 0.5)。
    pans: [f32; 8],
    /// M3 動作確認用: 8 ch の mute フラグ。
    mutes: [bool; 8],
}

impl MixerModel {
    fn new() -> Self {
        Self {
            title: "daw-ui ミキサー (M3 8ch)".to_string(),
            count: 0,
            bench_active: false,
            last_action: "起動しました".to_string(),
            faders: [0.50, 0.70, 0.30, 0.60, 0.40, 0.80, 0.20, 0.55],
            pans: [0.50, 0.40, 0.60, 0.55, 0.30, 0.70, 0.50, 0.45],
            mutes: [false; 8],
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
    /// 直近に OS ウィンドウへ反映した title。Model.title と差分を見て set_title を呼ぶ。
    last_window_title: String,
    /// 直前フレームで IME を有効化していたか。`UiHost::ime_request()` の Some/None
    /// 切替で OS への `set_ime_allowed` を最小限に呼ぶための差分管理。
    ime_enabled: bool,
    /// 起動からの経過フレーム数 (デバッグ用)。
    frames: u64,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        let ui = UiHost::<MixerModel>::with_window(window.clone());
        let model = MixerModel::new();
        let scene = Scene::new();
        let input = InputAccumulator::new();
        let last_window_title = model.title.clone();
        // タイトルを反映
        window.set_title(&last_window_title);
        Self {
            window,
            renderer,
            ui,
            model,
            scene,
            input,
            last_window_title,
            ime_enabled: false,
            frames: 0,
        }
    }

    /// `Ui::frame` で edits が出たかどうかを返す。出たなら呼び出し側で
    /// 直後に redraw を要求して、適用後の Model でもう 1 度ラベル等を
    /// 描画し直す必要がある (immediate-mode + Edit queue の常で、edits は
    /// 描画クロージャ後に apply されるので、この関数の `render` までの間に
    /// scene へ積まれているラベル文字列は 1 フレーム古い値になっている)。
    #[allow(clippy::too_many_lines, clippy::needless_range_loop)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let input = self.input.take_input();

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
                        clip_rect: None,
                    });
                }
            }
        }

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // M8 Phase 30: keyboard shortcut。Ctrl+Z で undo、Ctrl+Shift+Z / Ctrl+Y で redo。
                // fader / knob は drag 終端で Undoable Edit を発行するので、ここで request_undo
                // するだけで前回の drag が巻き戻る。
                if ui.take_shortcut("undo") {
                    ui.request_undo();
                }
                if ui.take_shortcut("redo") {
                    ui.request_redo();
                }

                // タイトル編集 (M3 Phase 4b: text_input)。
                ui.label("title_lbl", "タイトル (クリックで編集):");
                let _: TextInputResponse = ui.text_input("title_edit", &m.title, |new| {
                    Edit::mutate(move |m: &mut MixerModel| {
                        m.title = new;
                        m.last_action = "タイトル変更".to_string();
                    })
                });

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

                // 8 ch チャンネルストリップ。LayoutPass で row(8 col) × column(fader/pct/knob/pan/mute)
                // を組む (M3 Phase 5 で拡張した Padding/Gap を実用)。
                //
                // 各 ch column の高さ合計:
                //   fader 200 + gap 6 + pct 26 + gap 6 + knob 56 + gap 6 + pan 16 + gap 6 + mute 24 = 346
                // mixer_root 幅合計:
                //   pad_left 20 + 8×60 + 7×16 + pad_right 20 = 632
                // 左側 (title/count/status/3 buttons) は convenience method の cursor+next_y で
                // full-width に縦積みされる。strip header はそれを下回る位置に置き、
                // 視覚衝突を避ける (左側 3 buttons は y ≈ 154-226、bench ボタン下端 226)。
                let strip_origin_x = 320.0;
                let strip_origin_y = 280.0;
                let strip_avail_w = 632.0_f32;
                let strip_avail_h = 346.0_f32;

                ui.label_at(
                    "strip_label",
                    "M3: 8ch チャンネルストリップ — drag / Ctrl+drag (1/10) / dbl-click でリセット",
                    strip_origin_x,
                    strip_origin_y - 28.0,
                    14.0,
                    Color::rgb(0.85, 0.88, 0.92),
                );

                // LayoutPass で各 ch のサブノードと列ノードを作る。
                let mut layout = LayoutPass::new();
                let mut sub_nodes: Vec<(NodeId, NodeId, NodeId, NodeId, NodeId)> =
                    Vec::with_capacity(8);
                let mut col_nodes: Vec<NodeId> = Vec::with_capacity(8);
                for _ in 0..8 {
                    let fader_n = layout.leaf(60.0, 200.0);
                    let pct_n   = layout.leaf(60.0, 26.0);
                    let knob_n  = layout.leaf(60.0, 56.0);
                    let pan_n   = layout.leaf(60.0, 16.0);
                    let mute_n  = layout.leaf(60.0, 24.0);
                    sub_nodes.push((fader_n, pct_n, knob_n, pan_n, mute_n));
                    col_nodes.push(layout.flex(
                        FlexDirection::Column,
                        Gap::all(6.0),
                        Padding::ZERO,
                        &[fader_n, pct_n, knob_n, pan_n, mute_n],
                    ));
                }
                let root = layout.flex(
                    FlexDirection::Row,
                    Gap::xy(16.0, 0.0),
                    Padding::axis(20.0, 0.0),
                    &col_nodes,
                );

                // 計算 + screen 座標オフセットを 1 度に。各 widget の rect は layout.rect(node)。
                layout.compute_at(
                    root,
                    strip_avail_w,
                    strip_avail_h,
                    (strip_origin_x, strip_origin_y),
                );

                for i in 0..8 {
                    let (fader_n, pct_n, knob_n, pan_n, mute_n) = sub_nodes[i];
                    let fader_rect = layout.rect(fader_n);
                    let pct_rect   = layout.rect(pct_n);
                    let knob_rect  = layout.rect(knob_n);
                    let pan_rect   = layout.rect(pan_n);
                    let mute_rect  = layout.rect(mute_n);

                    let resp: FaderResponse =
                        ui.fader_at(("ch_fader", i), fader_rect, m.faders[i], 0.0, "fader", move |v| {
                            Edit::mutate(move |m: &mut MixerModel| {
                                m.faders[i] = v;
                                m.last_action = format!("ch{} fader = {v:.2}", i + 1);
                            })
                        });
                    let percent = (resp.displayed_value * 100.0).round() as i32;
                    ui.label_at(
                        ("fader_pct", i),
                        &format!("ch{}\n{percent}%", i + 1),
                        pct_rect.x,
                        pct_rect.y,
                        12.0,
                        if resp.dragging {
                            Color::rgb(0.95, 0.97, 1.0)
                        } else {
                            Color::rgb(0.65, 0.68, 0.72)
                        },
                    );

                    let kresp: KnobResponse =
                        ui.knob_at(("ch_pan", i), knob_rect, m.pans[i], 0.5, "pan", move |v| {
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
                        pan_rect.x,
                        pan_rect.y,
                        12.0,
                        if kresp.dragging {
                            Color::rgb(0.95, 0.97, 1.0)
                        } else {
                            Color::rgb(0.65, 0.68, 0.72)
                        },
                    );

                    let _ = ui.checkbox_at(
                        ("ch_mute", i),
                        mute_rect,
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

                // M5 Phase 13: heavy() 動作確認デモ。viewport_key = m.count に
                // することで、count 変化時のみ draw_fn が走り (cache miss)、
                // 他のフレームでは前フレームの描画コマンドを再利用 (cache hit)。
                // 視覚としては左下に「Heavy demo: count = N」の薄いラベル 1 行。
                ui.heavy("heavy_demo", |hctx| {
                    let viewport_key = m.count;
                    hctx.cached(viewport_key, |hctx| {
                        hctx.label_at(
                            "heavy_demo_label",
                            &format!("Heavy demo: count = {} (cached)", m.count),
                            20.0,
                            screen.height as f32 - 28.0,
                            12.0,
                            Color::rgb(0.55, 0.65, 0.78),
                        );
                    });
                });
            },
        );

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
            | AppEvent::Keyboard(_)
            | AppEvent::ImePreedit { .. }
            | AppEvent::ImeCommit(_) => {
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
        // model.title が変わっていれば OS のウィンドウタイトルを追従させる。
        if self.model.title != self.last_window_title {
            self.window.set_title(&self.model.title);
            self.last_window_title = self.model.title.clone();
        }
        // IME 有効/無効と候補ウィンドウ位置を差分で OS に伝える。
        match (self.ime_enabled, self.ui.ime_request()) {
            (false, Some(area)) => {
                self.window.set_ime_allowed(true);
                self.window.set_ime_cursor_area(
                    f64::from(area.x), f64::from(area.y),
                    f64::from(area.w), f64::from(area.h),
                );
                self.ime_enabled = true;
            }
            (true, Some(area)) => {
                // 位置だけ追従。
                self.window.set_ime_cursor_area(
                    f64::from(area.x), f64::from(area.y),
                    f64::from(area.w), f64::from(area.h),
                );
            }
            (true, None) => {
                self.window.set_ime_allowed(false);
                self.ime_enabled = false;
            }
            (false, None) => {}
        }
        // edits や focus 変化が出たら次フレームで適用後 Model / focus 状態を描き直す。
        if self.ui.focus_changed_in_last_frame() {
            self.window.request_redraw();
        }
        // bench 中は連続再描画
        self.model.bench_active
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui mixer (M3 8ch)")
        .with_inner_size(winit::dpi::LogicalSize::new(960.0, 660.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
