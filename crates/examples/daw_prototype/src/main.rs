//! examples/daw_prototype — M7 visual prototype demo。
//!
//! M7 で実装した全 widget (scroll_area / popup / menu_bar / context_menu / dropdown /
//! tab_view / split_view / time_ruler / bar_beat_grid / level_meter) を 1 window に
//! 統合した「見た目 DAW」サンプル。操作の整合性は問わない (M8 で undo / shortcut /
//! drag&drop が来てから本格化)。
//!
//! UI 構成:
//! - 上端: menu_bar (File / Edit / View / Help)
//! - 左 sidebar (split_view 左側): プリセット dropdown + scroll 可能なファイルリスト
//! - 右メイン (split_view 右側): tab_view (Mixer / Arrangement / Piano Roll / Sample)
//!   - Mixer タブ: 8ch fader + level_meter
//!   - Arrangement タブ: time_ruler + bar_beat_grid + 仮想クリップ (右クリックで context_menu)
//!   - Piano Roll タブ: time_ruler + bar_beat_grid + 仮 note 配置
//!   - Sample タブ: 簡易プレースホルダー (sample_editor は別 example)

use std::sync::Arc;

use daw_ui_core::{
    BarBeatGridStyle, Edit, FaderResponse, InputAccumulator, LevelMeterStyle, MeterBallistic,
    Orientation, TimeMapping, TimeRulerStyle, UiHost, ViewportState1D,
};
use daw_ui_platform::{AppEvent, AppHost, WindowBackend, winit_backend};
use daw_ui_renderer::{Color, Rect, RectCommand, Renderer, Scene};
use winit::window::WindowAttributes;

const N_CH: usize = 8;
const N_TRACKS: usize = 12;
const N_BROWSER_ITEMS: usize = 40;

struct DawModel {
    /// mixer faders / pans / mutes
    faders: [f32; N_CH],
    mutes: [bool; N_CH],
    /// preset dropdown 選択中 index
    preset_idx: usize,
    /// arrangement / piano_roll の viewport (X 軸)
    arr_viewport: ViewportState1D,
    /// simulated peak per channel (sin で時間変化)
    sim_phase: f32,
    last_action: String,
}

impl DawModel {
    fn new() -> Self {
        Self {
            faders: [0.55, 0.70, 0.30, 0.60, 0.40, 0.80, 0.20, 0.55],
            mutes: [false; N_CH],
            preset_idx: 0,
            arr_viewport: ViewportState1D::new(0.0, 48_000.0 * 30.0), // 30 sec @ 48k
            sim_phase: 0.0,
            last_action: "起動 — メニュー / タブ / dropdown / 右クリック を試して下さい".to_string(),
        }
    }

    fn sim_peak(&self, ch: usize) -> f32 {
        let f = (self.sim_phase * 0.4 + ch as f32 * 0.7).sin().abs();
        // mute はゼロ、fader で attenuation、peak は ±1 にクリップ可能
        if self.mutes[ch] { 0.0 } else { (f * self.faders[ch] * 1.2).clamp(0.0, 1.5) }
    }
}

fn arr_total_samples() -> f64 {
    48_000.0 * 60.0 // 60 sec @ 48k
}

const PRESETS: &[&str] = &["Default", "Mixer-only", "Arrangement", "Piano-roll"];
const HELP_TEXT: &str = "M7 prototype: タブ / split / メニュー / dropdown / 右クリック / scroll";

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<DawModel>,
    model: DawModel,
    scene: Scene,
    input: InputAccumulator,
    last_tick: std::time::Instant,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        let ui = UiHost::<DawModel>::with_window(window.clone());
        let model = DawModel::new();
        let scene = Scene::new();
        let input = InputAccumulator::new();
        window.set_title("daw-ui daw_prototype (M7 visual prototype)");
        Self {
            window,
            renderer,
            ui,
            model,
            scene,
            input,
            last_tick: std::time::Instant::now(),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let input = self.input.take_input();
        // sim_phase を時間で進める (level_meter のアニメーション用)
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        self.last_tick = now;
        self.model.sim_phase += dt * 4.0;

        let menu_h = 28.0;
        let footer_h = 24.0;
        let menu_rect = Rect { x: 0.0, y: 0.0, w: screen.width as f32, h: menu_h };
        let footer_rect = Rect {
            x: 0.0,
            y: screen.height as f32 - footer_h,
            w: screen.width as f32,
            h: footer_h,
        };
        let body_rect = Rect {
            x: 0.0,
            y: menu_h,
            w: screen.width as f32,
            h: (screen.height as f32 - menu_h - footer_h).max(100.0),
        };

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // ---- 1. menu_bar ----
                ui.menu_bar(menu_rect, |menu| {
                    menu.menu("File", |sub| {
                        sub.item("New", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → New".to_string();
                            })
                        });
                        sub.item("Open...", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Open".to_string();
                            })
                        });
                        sub.item("Save", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Save".to_string();
                            })
                        });
                        sub.item("Quit", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Quit (no-op in demo)".to_string();
                            })
                        });
                    });
                    menu.menu("Edit", |sub| {
                        sub.item("Undo", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "Edit → Undo (M8 で本実装)".to_string();
                            })
                        });
                        sub.item("Redo", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "Edit → Redo (M8 で本実装)".to_string();
                            })
                        });
                    });
                    menu.menu("View", |sub| {
                        sub.item("Reset zoom", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.arr_viewport = ViewportState1D::new(0.0, 48_000.0 * 30.0);
                                m.last_action = "View → Reset zoom".to_string();
                            })
                        });
                    });
                    menu.menu("Help", |sub| {
                        sub.item("About", || {
                            Edit::mutate(|m: &mut DawModel| {
                                m.last_action = HELP_TEXT.to_string();
                            })
                        });
                    });
                });

                // ---- 2. split_view (sidebar | main) ----
                ui.split_view("root_split", body_rect, Orientation::Horizontal, 0.22, |ui, sidebar, main| {
                    // ---- sidebar: preset dropdown + scroll list ----
                    let sidebar_pad = 8.0;
                    let dd_rect = Rect {
                        x: sidebar.x + sidebar_pad,
                        y: sidebar.y + sidebar_pad,
                        w: sidebar.w - sidebar_pad * 2.0,
                        h: 26.0,
                    };
                    let preset_idx = m.preset_idx;
                    if let Some(idx) = ui.dropdown("preset", dd_rect, PRESETS, preset_idx) {
                        ui.push_edit(Edit::mutate(move |m: &mut DawModel| {
                            m.preset_idx = idx;
                            m.last_action = format!("preset → {}", PRESETS[idx]);
                        }));
                    }

                    // browser scroll list (40 items)
                    let list_rect = Rect {
                        x: sidebar.x + sidebar_pad,
                        y: sidebar.y + sidebar_pad + dd_rect.h + sidebar_pad,
                        w: sidebar.w - sidebar_pad * 2.0,
                        h: (sidebar.h - sidebar_pad * 3.0 - dd_rect.h).max(50.0),
                    };
                    let item_h = 24.0;
                    let total_h = item_h * N_BROWSER_ITEMS as f32;
                    ui.scroll_area("browser", list_rect, (list_rect.w, total_h), |ui, offset| {
                        for i in 0..N_BROWSER_ITEMS {
                            let y = list_rect.y - offset.1 + i as f32 * item_h;
                            let r = Rect { x: list_rect.x, y, w: list_rect.w - 12.0, h: item_h };
                            // 簡易: ラベル付き矩形 (button だと clip 越しに hit が変かも)
                            ui.push_rect(RectCommand::uniform_radius(
                                r,
                                Color::rgb(0.13, 0.14, 0.18),
                                2.0,
                            ));
                            ui.label_at(
                                ("browser_lbl", i),
                                &format!("Sample {i:02}.wav"),
                                r.x + 8.0,
                                r.y + 6.0,
                                12.0,
                                Color::rgb(0.85, 0.88, 0.92),
                            );
                        }
                    });

                    // ---- main: tab_view ----
                    ui.tab_view("main_tabs", main, |tabs| {
                        tabs.tab("Mixer", |ui, pane| {
                            drawmixer_tab(ui, m, pane);
                        });
                        tabs.tab("Arrangement", |ui, pane| {
                            draw_arrangement_tab(ui, m, pane);
                        });
                        tabs.tab("Piano Roll", |ui, pane| {
                            draw_piano_roll_tab(ui, m, pane);
                        });
                        tabs.tab("Sample", |ui, pane| {
                            ui.label_at(
                                "sample_placeholder",
                                "Sample editor は別 example: cargo run --bin sample_editor",
                                pane.x + 16.0,
                                pane.y + 16.0,
                                14.0,
                                Color::rgb(0.85, 0.88, 0.92),
                            );
                        });
                    });
                });

                // ---- 3. footer (last_action) ----
                ui.push_rect(RectCommand {
                    rect: footer_rect,
                    fill: Color::rgb(0.08, 0.09, 0.11),
                    border: Color::TRANSPARENT,
                    border_width: 0.0,
                    radius: [0.0; 4],
                    clip_rect: None,
                });
                ui.label_at(
                    "footer",
                    &m.last_action,
                    8.0,
                    footer_rect.y + 5.0,
                    12.0,
                    Color::rgb(0.85, 0.88, 0.92),
                );
            },
        );
    }
}

fn drawmixer_tab(ui: &mut daw_ui_core::Ui<'_, DawModel>, m: &DawModel, pane: Rect) {
    // 8 ch を横並び。各 ch = fader + level_meter + label
    let ch_w = pane.w / N_CH as f32;
    let pad_y = 12.0;
    let label_h = 18.0;
    let meter_w = 16.0;
    let fader_w = ch_w - meter_w - 24.0;
    let body_top = pane.y + pad_y + label_h;
    let body_h = (pane.h - pad_y * 2.0 - label_h).max(60.0);

    for ch in 0..N_CH {
        let cx = pane.x + ch_w * ch as f32 + 8.0;
        // ch label
        ui.label_at(
            ("ch_lbl", ch),
            &format!("CH {}", ch + 1),
            cx,
            pane.y + 6.0,
            12.0,
            Color::rgb(0.85, 0.88, 0.92),
        );

        // fader
        let fader_rect = Rect { x: cx, y: body_top, w: fader_w, h: body_h };
        let cur = m.faders[ch];
        let _resp: FaderResponse = ui.fader_at(("ch_fader", ch), fader_rect, cur, 0.7, move |v| {
            Edit::mutate(move |m: &mut DawModel| m.faders[ch] = v)
        });

        // level_meter
        let meter_rect = Rect {
            x: cx + fader_w + 4.0,
            y: body_top,
            w: meter_w,
            h: body_h,
        };
        ui.level_meter(
            ("chmeter", ch),
            meter_rect,
            m.sim_peak(ch),
            MeterBallistic::Peak,
            LevelMeterStyle::default(),
        );

        // mute checkbox
        let mute_rect = Rect {
            x: cx,
            y: body_top + body_h + 4.0,
            w: ch_w - 16.0,
            h: 20.0,
        };
        let _ = mute_rect;
    }
}

fn draw_arrangement_tab(ui: &mut daw_ui_core::Ui<'_, DawModel>, m: &DawModel, pane: Rect) {
    let mapping = TimeMapping::default_4_4_120();
    let ruler_h = 24.0;
    let ruler_rect = Rect { x: pane.x, y: pane.y, w: pane.w, h: ruler_h };
    let grid_rect = Rect {
        x: pane.x,
        y: pane.y + ruler_h,
        w: pane.w,
        h: (pane.h - ruler_h).max(50.0),
    };

    // wheel zoom
    let scroll = ui.take_scroll_in_rect(grid_rect);
    if scroll.1.abs() > 0.0 {
        let factor = (-scroll.1 * 0.005).exp();
        let anchor_frac = if let Some((px, _)) = ui.pointer().pos {
            ((px - grid_rect.x) / grid_rect.w.max(1.0)).clamp(0.0, 1.0)
        } else {
            0.5
        };
        let total = arr_total_samples();
        ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
            mm.arr_viewport.zoom_at(factor, anchor_frac, 1024.0);
            mm.arr_viewport.clamp_to(total);
        }));
    }

    let viewport = m.arr_viewport;
    ui.time_ruler("arr_ruler", ruler_rect, mapping, viewport, TimeRulerStyle::default());

    // 背景 + grid
    ui.push_rect(RectCommand {
        rect: grid_rect,
        fill: Color::rgb(0.10, 0.11, 0.13),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    ui.bar_beat_grid("arr_grid", grid_rect, mapping, viewport, BarBeatGridStyle::default());

    // 仮 12 トラック の clip 表示
    let track_h = (grid_rect.h / N_TRACKS as f32).max(20.0);
    for t in 0..N_TRACKS {
        let row_y = grid_rect.y + track_h * t as f32;
        let row_rect = Rect { x: grid_rect.x, y: row_y, w: grid_rect.w, h: track_h };
        // クリップ: 各トラックに 2 個ずつ仮配置 (samples 単位)
        for c in 0..2 {
            let start_s = (t as f64 * 96000.0 + f64::from(c) * 144000.0) % arr_total_samples();
            let len_s = 72000.0;
            let x0 = grid_rect.x + viewport.unit_to_px(start_s, grid_rect.w);
            let w = viewport.unit_to_px(len_s, grid_rect.w);
            let clip_rect = Rect { x: x0, y: row_y + 2.0, w, h: track_h - 4.0 };
            // viewport 外なら skip
            if clip_rect.x + clip_rect.w < grid_rect.x || clip_rect.x > grid_rect.x + grid_rect.w {
                continue;
            }
            ui.push_rect(RectCommand {
                rect: clip_rect,
                fill: Color::rgb(0.18, 0.40, 0.65),
                border: Color::rgb(0.30, 0.55, 0.78),
                border_width: 1.0,
                radius: [3.0; 4],
                clip_rect: Some(grid_rect),
            });
            // 右クリックで context_menu (clip 上で)
            ui.context_menu_for(clip_rect, &["Cut", "Copy", "Delete", "Duplicate"], move |idx| {
                let actions = ["Cut", "Copy", "Delete", "Duplicate"];
                let label = actions.get(idx).copied().unwrap_or("?").to_string();
                Edit::mutate(move |mm: &mut DawModel| {
                    mm.last_action = format!("clip ctx → {label} (track {t} clip {c})");
                })
            });
        }
        // track separator
        ui.push_rect(RectCommand {
            rect: Rect { x: row_rect.x, y: row_rect.y + row_rect.h - 1.0, w: row_rect.w, h: 1.0 },
            fill: Color::rgba(0.0, 0.0, 0.0, 0.5),
            border: Color::TRANSPARENT,
            border_width: 0.0,
            radius: [0.0; 4],
            clip_rect: None,
        });
    }
}

fn draw_piano_roll_tab(ui: &mut daw_ui_core::Ui<'_, DawModel>, m: &DawModel, pane: Rect) {
    let mapping = TimeMapping::default_4_4_120();
    let viewport = m.arr_viewport;
    let ruler_h = 24.0;
    let key_w = 36.0;
    let ruler_rect = Rect { x: pane.x + key_w, y: pane.y, w: pane.w - key_w, h: ruler_h };
    let grid_rect = Rect {
        x: pane.x + key_w,
        y: pane.y + ruler_h,
        w: pane.w - key_w,
        h: (pane.h - ruler_h).max(50.0),
    };
    ui.time_ruler("pr_ruler", ruler_rect, mapping, viewport, TimeRulerStyle::default());
    ui.push_rect(RectCommand {
        rect: grid_rect,
        fill: Color::rgb(0.10, 0.11, 0.13),
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    ui.bar_beat_grid("pr_grid", grid_rect, mapping, viewport, BarBeatGridStyle::default());

    // keyboard sidebar
    ui.push_rect(RectCommand {
        rect: Rect { x: pane.x, y: pane.y + ruler_h, w: key_w, h: grid_rect.h },
        fill: Color::rgb(0.85, 0.85, 0.88),
        border: Color::rgb(0.30, 0.32, 0.36),
        border_width: 1.0,
        radius: [0.0; 4],
        clip_rect: None,
    });
    let n_keys = 24; // 2 octaves
    let key_h = (grid_rect.h / n_keys as f32).max(8.0);
    for k in 0..n_keys {
        let pitch = 60 + k; // C4 から
        let is_black = matches!(pitch % 12, 1 | 3 | 6 | 8 | 10);
        if is_black {
            ui.push_rect(RectCommand {
                rect: Rect {
                    x: pane.x,
                    y: grid_rect.y + key_h * k as f32,
                    w: key_w * 0.7,
                    h: key_h - 1.0,
                },
                fill: Color::rgb(0.10, 0.10, 0.12),
                border: Color::TRANSPARENT,
                border_width: 0.0,
                radius: [0.0; 4],
                clip_rect: None,
            });
        }
    }

    // 仮 note (5 個) を等間隔配置
    for i in 0..5 {
        let note_start_s = f64::from(i) * 24_000.0 + 12_000.0; // 0.25 sec stagger
        let note_len_s = 18_000.0;
        let pitch_offset = (i as f32) * key_h;
        let nx = grid_rect.x + viewport.unit_to_px(note_start_s, grid_rect.w);
        let nw = viewport.unit_to_px(note_len_s, grid_rect.w);
        if nx + nw < grid_rect.x || nx > grid_rect.x + grid_rect.w {
            continue;
        }
        ui.push_rect(RectCommand {
            rect: Rect { x: nx, y: grid_rect.y + pitch_offset, w: nw, h: key_h - 2.0 },
            fill: Color::rgb(0.42, 0.85, 0.95),
            border: Color::rgb(0.30, 0.55, 0.78),
            border_width: 1.0,
            radius: [2.0; 4],
            clip_rect: Some(grid_rect),
        });
    }
}

impl AppHost for App {
    fn on_event(&mut self, ev: AppEvent) {
        self.input.ingest(&ev);
        if let AppEvent::Resized(s) = ev {
            self.renderer.resize(s);
        }
    }

    fn on_render(&mut self) -> bool {
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e:?}");
        }
        // IME (text_input がない demo だが、念のため empty で disable)
        self.window.set_ime_allowed(false);
        false
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui daw_prototype")
        .with_inner_size(winit::dpi::LogicalSize::new(1280, 800));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("run_app error: {e:?}");
    }
}
