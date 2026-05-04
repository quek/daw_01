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
    ArrangementClip, ArrangementEditRequest, ArrangementStyle, ArrangementTrack, ArrangementView,
    BarBeatGridStyle, ClipKey, DialogResult, Edit, FaderResponse, FileDialogFilter,
    InputAccumulator, LevelMeterStyle, ListViewStyle, MenuItemSpec, MeterBallistic, ModalStyle,
    Orientation, TimeMapping, TimeRulerStyle, UiHost, ViewportState1D, WidgetId,
};
use daw_ui_platform::{AppEvent, AppHost, WindowBackend, winit_backend};
use daw_ui_renderer::{Color, Rect, RectCommand, Renderer, Scene};
use winit::window::WindowAttributes;

const N_CH: usize = 8;
const N_TRACKS: usize = 12;
const N_BROWSER_ITEMS: usize = 40;

struct DawClip {
    id: u32,
    start_beat: f64,
    len_beats: f64,
    name: Arc<str>,
    color: Option<Color>,
}

struct DawTrack {
    id: u32,
    name: Arc<str>,
    muted: bool,
    solo: bool,
    next_clip_id: u32,
    clips: Vec<DawClip>,
}

struct DawModel {
    /// mixer faders / pans / mutes
    faders: [f32; N_CH],
    mutes: [bool; N_CH],
    /// preset dropdown 選択中 index
    preset_idx: usize,
    /// arrangement / piano_roll の viewport (X 軸)
    arr_viewport: ViewportState1D,
    /// M9 P0-2: tab_view_with_state で外部制御中のタブ index
    /// (0=Mixer / 1=Arrangement / 2=Piano Roll / 3=Sample)
    current_tab: usize,
    /// simulated peak per channel (sin で時間変化)
    sim_phase: f32,
    last_action: String,
    /// (M9 Phase 45d) `Demo Dialog` ボタン押下から次フレーム frame 開始時の `ui.open_modal`
    /// 発火までを繋ぐ 1 frame レイテンシ用フラグ。`button_at` の click closure からは
    /// `&mut Ui` にアクセスできないので、Edit 経由で立てて次フレームに `ui.open_modal` する。
    open_demo_request: bool,
    // (M9 Phase 45e) arrangement widget 用 state
    arr_tracks: Vec<DawTrack>,
    arr_view: ArrangementView,
    arr_selected_clips: Vec<ClipKey>,
    arr_selected_track: Option<u32>,
    /// `BeginRenameTrack(id)` 受信時にセット。`Some(id)` 中は該当 track header 上に
    /// `text_input_at` を重ね描画。Enter / blur / ESC で `None` に戻す。
    arr_rename_target: Option<u32>,
    /// rename 開始した最初のフレームを示すフラグ。`true` のフレームで `ui.set_focus()` を呼んで
    /// text_input にプログラム的にフォーカスを与え、その後 `false` にリセットする。
    arr_rename_just_started: bool,
}

impl DawModel {
    fn new() -> Self {
        let mut tracks: Vec<DawTrack> = Vec::with_capacity(N_TRACKS);
        for ti in 0..N_TRACKS {
            let mut clips: Vec<DawClip> = Vec::with_capacity(2);
            for ci in 0..2 {
                clips.push(DawClip {
                    id: ci as u32,
                    start_beat: ti as f64 * 1.5 + f64::from(ci) * 6.0,
                    len_beats: 4.0,
                    name: Arc::from(format!("clip{}", ci + 1)),
                    color: None,
                });
            }
            tracks.push(DawTrack {
                id: ti as u32,
                name: Arc::from(format!("Track {}", ti + 1)),
                muted: false,
                solo: false,
                next_clip_id: 2,
                clips,
            });
        }
        let arr_view = ArrangementView {
            start_beat: 0.0,
            len_beats: 24.0,
            track_top: 0.0,
            tracks_visible: 8.0,
            track_row_h: 32.0,
            header_w: 180.0,
            ruler_h: 24.0,
            playhead_beat: Some(2.0),
            loop_range: Some((4.0, 12.0)),
            data_generation: 0,
        };
        Self {
            faders: [0.55, 0.70, 0.30, 0.60, 0.40, 0.80, 0.20, 0.55],
            mutes: [false; N_CH],
            preset_idx: 0,
            arr_viewport: ViewportState1D::new(0.0, 48_000.0 * 30.0), // 30 sec @ 48k
            current_tab: 0,
            sim_phase: 0.0,
            last_action: "起動 — メニュー / タブ / dropdown / 右クリック を試して下さい".to_string(),
            open_demo_request: false,
            arr_tracks: tracks,
            arr_view,
            arr_selected_clips: Vec::new(),
            arr_selected_track: None,
            arr_rename_target: None,
            arr_rename_just_started: false,
        }
    }

    fn sim_peak(&self, ch: usize) -> f32 {
        let f = (self.sim_phase * 0.4 + ch as f32 * 0.7).sin().abs();
        // mute はゼロ、fader で attenuation、peak は ±1 にクリップ可能
        if self.mutes[ch] { 0.0 } else { (f * self.faders[ch] * 1.2).clamp(0.0, 1.5) }
    }
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
                // sim_phase アニメ継続のため毎フレーム redraw を要求
                // (実運用では audio thread から peak 取得時に request_redraw を呼ぶ)
                ui.request_redraw();

                // M8 Phase 30: shortcut layer。
                // - Ctrl+Z で undo / Ctrl+Shift+Z / Ctrl+Y で redo
                // - Ctrl+O で audio file open dialog
                if ui.take_shortcut("undo") {
                    ui.request_undo();
                }
                if ui.take_shortcut("redo") {
                    ui.request_redo();
                }
                if ui.take_shortcut("open") {
                    ui.request_open_file_dialog(
                        "open_audio",
                        "Open audio file",
                        &[FileDialogFilter {
                            name: "Audio",
                            extensions: &["wav", "mp3", "flac", "ogg"],
                        }],
                    );
                }
                // 前フレームに完了した dialog 結果を取り出して last_action に表示。
                if let Some(result) = ui.take_dialog_result("open_audio") {
                    let action = match result {
                        DialogResult::OpenFile(p) => format!("open: {}", p.display()),
                        DialogResult::Cancelled => "open: cancelled".to_string(),
                        _ => "open: unexpected".to_string(),
                    };
                    ui.push_edit(Edit::mutate(move |m: &mut DawModel| {
                        m.last_action = action;
                    }));
                }
                // M8 Phase 32: file drop を window 全体で受ける。drop された path を last_action に表示。
                let screen_rect = Rect::new(0.0, 0.0, ui.screen().width as f32, ui.screen().height as f32);
                if let Some(paths) = ui.take_file_drop_in_rect(screen_rect) {
                    let action = format!(
                        "drop: {}",
                        paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
                    );
                    ui.push_edit(Edit::mutate(move |m: &mut DawModel| {
                        m.last_action = action;
                    }));
                }

                // ---- 1. menu_bar ----
                // M9 P1-5: Undo/Redo の dynamic enable + shortcut hint。menu_bar の前に
                // 取得して closure に move (内側 closure から borrow できるように `move` keyword)。
                let edit_can_undo = ui.can_undo();
                let edit_can_redo = ui.can_redo();
                let undo_hint = ui.shortcut_for("undo");
                let redo_hint = ui.shortcut_for("redo");
                ui.menu_bar(menu_rect, move |menu| {
                    menu.menu("File", |sub| {
                        sub.item("New", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → New".to_string();
                            }));
                        });
                        sub.item("Open...", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Open".to_string();
                            }));
                        });
                        // M7+ sub_menu cascade demo: hover で右に出る
                        sub.sub_menu("Recent", |recent| {
                            recent.item("project1.daw", |ui| {
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "File → Recent → project1.daw".to_string();
                                }));
                            });
                            recent.item("session_2026.daw", |ui| {
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "File → Recent → session_2026.daw".to_string();
                                }));
                            });
                            recent.sub_menu("Older", |older| {
                                older.item("draft_a.daw", |ui| {
                                    ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                        m.last_action = "File → Recent → Older → draft_a".to_string();
                                    }));
                                });
                                older.item("draft_b.daw", |ui| {
                                    ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                        m.last_action = "File → Recent → Older → draft_b".to_string();
                                    }));
                                });
                            });
                        });
                        sub.item("Save", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Save".to_string();
                            }));
                        });
                        sub.item("Quit", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = "File → Quit (no-op in demo)".to_string();
                            }));
                        });
                    });
                    menu.menu("Edit", |sub| {
                        // M9 P1-5 (C 案): on_click closure に &mut Ui を渡せるので、menu の Undo
                        // から ui.request_undo() を直接発火可能。shortcut Ctrl+Z 経路と同等の動作。
                        // enabled は can_undo() / can_redo() に基づいて動的に灰色化、shortcut_hint
                        // で右端に "Ctrl+Z" 等を表示。
                        sub.item_with(MenuItemSpec {
                            label: "Undo",
                            on_click: Box::new(|ui| {
                                ui.request_undo();
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "Edit → Undo (menu)".to_string();
                                }));
                            }),
                            enabled: edit_can_undo,
                            shortcut_hint: undo_hint,
                        });
                        sub.item_with(MenuItemSpec {
                            label: "Redo",
                            on_click: Box::new(|ui| {
                                ui.request_redo();
                                ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                    m.last_action = "Edit → Redo (menu)".to_string();
                                }));
                            }),
                            enabled: edit_can_redo,
                            shortcut_hint: redo_hint,
                        });
                    });
                    menu.menu("View", |sub| {
                        sub.item("Reset zoom", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.arr_viewport = ViewportState1D::new(0.0, 48_000.0 * 30.0);
                                m.last_action = "View → Reset zoom".to_string();
                            }));
                        });
                    });
                    menu.menu("Help", |sub| {
                        sub.item("About", |ui| {
                            ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                                m.last_action = HELP_TEXT.to_string();
                            }));
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

                    // ---- main: tab_view (M9 P0-2: 外部 state 版で footer button から
                    // タブを切替可能に) ----
                    let mut tab_idx = m.current_tab;
                    ui.tab_view_with_state("main_tabs", main, &mut tab_idx, |tabs| {
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
                    // クリックで selected が変化していれば model に書き戻し
                    if tab_idx != m.current_tab {
                        ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                            mm.current_tab = tab_idx;
                        }));
                    }
                });

                // ---- 3. footer (last_action + M9 P0-2: Open Piano Roll button) ----
                // M9 Phase 45a: Ui::panel を背景塗りに使う (heavy+cached+push_rect の 1 行ラッパ)
                ui.panel("footer_bg", footer_rect, Color::rgb(0.08, 0.09, 0.11), 0.0);
                let btn_w = 160.0;
                let btn_pad = 4.0;
                let btn_rect = Rect {
                    x: footer_rect.x + footer_rect.w - btn_w - 8.0,
                    y: footer_rect.y + btn_pad,
                    w: btn_w,
                    h: footer_rect.h - btn_pad * 2.0,
                };
                ui.button_at("open_pr", "Open Piano Roll", btn_rect, || {
                    Edit::mutate(|m: &mut DawModel| {
                        m.current_tab = 2;
                        m.last_action = "footer button → Piano Roll タブへ遷移".to_string();
                    })
                });

                // M9 Phase 45d: Demo Dialog button + Ui::modal + Ui::list_view デモ
                let demo_btn_w = 110.0;
                let demo_btn_rect = Rect {
                    x: btn_rect.x - demo_btn_w - 8.0,
                    y: btn_rect.y,
                    w: demo_btn_w,
                    h: btn_rect.h,
                };
                ui.button_at("open_demo", "Demo Dialog", demo_btn_rect, || {
                    Edit::mutate(|m: &mut DawModel| {
                        m.open_demo_request = true;
                        m.last_action = "Demo Dialog open".to_string();
                    })
                });
                ui.label_at(
                    "footer",
                    &m.last_action,
                    8.0,
                    footer_rect.y + 5.0,
                    12.0,
                    Color::rgb(0.85, 0.88, 0.92),
                );

                // ---- 4. Demo Dialog (M9 Phase 45d): button click → 次フレーム open_modal ----
                if m.open_demo_request {
                    ui.open_modal("demo");
                    ui.push_edit(Edit::mutate(|m: &mut DawModel| {
                        m.open_demo_request = false;
                    }));
                }
                let modal_style = ModalStyle::default();
                let list_style = ListViewStyle::default();
                let demo_items: [&str; 8] = [
                    "Reverb", "Delay", "Compressor", "EQ",
                    "Limiter", "Chorus", "Distortion", "Gate",
                ];
                ui.modal(
                    "demo",
                    (380.0, 360.0),
                    &modal_style,
                    Some(Box::new(|| {
                        Edit::mutate(|m: &mut DawModel| {
                            m.last_action = "Demo Dialog closed".to_string();
                        })
                    })),
                    |ui, panel| {
                        // タイトル
                        ui.label_at(
                            "demo_title",
                            "Demo Dialog (Phase 45d) — クリックで close",
                            panel.x + 16.0,
                            panel.y + 16.0,
                            14.0,
                            Color::rgb(0.95, 0.95, 0.97),
                        );
                        // list_view
                        let list_rect = Rect {
                            x: panel.x + 16.0,
                            y: panel.y + 48.0,
                            w: panel.w - 32.0,
                            h: panel.h - 64.0,
                        };
                        let resp = ui.list_view(
                            "demo_list",
                            list_rect,
                            &demo_items,
                            None,
                            &list_style,
                            |ui, name, i, row, _sel| {
                                ui.label_at(
                                    ("demo_label", i),
                                    name,
                                    row.x + 12.0,
                                    row.y + 6.0,
                                    13.0,
                                    Color::rgb(0.92, 0.92, 0.94),
                                );
                            },
                        );
                        // 行 click で close (= ESC / outside click と同じ閉じ方)
                        if resp.clicked.is_some() {
                            ui.close_modal("demo");
                        }
                    },
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
        let _resp: FaderResponse = ui.fader_at(("ch_fader", ch), fader_rect, cur, 0.7, "fader", move |v| {
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

/// `DawTrack` 列を `ArrangementTrack` に変換 (Arc<str> のみ clone)。
fn arr_track_views(m: &DawModel) -> Vec<ArrangementTrack> {
    m.arr_tracks
        .iter()
        .map(|t| ArrangementTrack {
            id: t.id,
            name: Arc::clone(&t.name),
            muted: t.muted,
            solo: t.solo,
            clips: t
                .clips
                .iter()
                .map(|c| ArrangementClip {
                    id: c.id,
                    start_beat: c.start_beat,
                    len_beats: c.len_beats,
                    name: Arc::clone(&c.name),
                    color: c.color,
                })
                .collect(),
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn draw_arrangement_tab(ui: &mut daw_ui_core::Ui<'_, DawModel>, m: &DawModel, pane: Rect) {
    let arr_tracks = arr_track_views(m);
    let style = ArrangementStyle::default();
    let resp = ui.arrangement(
        "arr",
        pane,
        &arr_tracks,
        m.arr_view,
        &m.arr_selected_clips,
        m.arr_selected_track,
        &style,
        |req| match req {
            ArrangementEditRequest::SelectClips { next, .. } => {
                let next_v = next;
                Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_selected_clips = next_v;
                    mm.last_action = "arr: SelectClips".to_string();
                })
            }
            ArrangementEditRequest::SelectTrack { next, .. } => Edit::mutate(move |mm: &mut DawModel| {
                mm.arr_selected_track = next;
                mm.last_action = format!("arr: SelectTrack {next:?}");
            }),
            ArrangementEditRequest::MoveClips(deltas) => Edit::mutate(move |mm: &mut DawModel| {
                let n = deltas.len();
                for d in deltas {
                    // remove from source track
                    let removed = mm
                        .arr_tracks
                        .iter_mut()
                        .find(|t| t.id == d.from.track)
                        .and_then(|t| {
                            let pos = t.clips.iter().position(|c| c.id == d.from.clip)?;
                            Some(t.clips.remove(pos))
                        });
                    if let Some(mut clip) = removed {
                        clip.start_beat = d.next_start_beat;
                        if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == d.to_track) {
                            // start_beat 順に挿入
                            let pos = t
                                .clips
                                .iter()
                                .position(|c| c.start_beat > clip.start_beat)
                                .unwrap_or(t.clips.len());
                            t.clips.insert(pos, clip);
                        }
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: MoveClips ({n})");
            }),
            ArrangementEditRequest::ResizeClips(deltas) => Edit::mutate(move |mm: &mut DawModel| {
                let n = deltas.len();
                for d in deltas {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == d.key.track)
                        && let Some(c) = t.clips.iter_mut().find(|c| c.id == d.key.clip)
                    {
                        c.start_beat = d.next_start;
                        c.len_beats = d.next_len;
                    }
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ResizeClips ({n})");
            }),
            ArrangementEditRequest::DeleteClips(keys) => Edit::mutate(move |mm: &mut DawModel| {
                let n = keys.len();
                for k in keys {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == k.track) {
                        t.clips.retain(|c| c.id != k.clip);
                    }
                }
                mm.arr_selected_clips.clear();
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: DeleteClips ({n})");
            }),
            ArrangementEditRequest::DoubleClickClip(key) => Edit::mutate(move |mm: &mut DawModel| {
                mm.current_tab = 2;
                mm.last_action =
                    format!("arr: dbl-click clip → Piano Roll (track {} clip {})", key.track, key.clip);
            }),
            ArrangementEditRequest::DoubleClickEmpty { track, beat } => {
                Edit::mutate(move |mm: &mut DawModel| {
                    if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == track) {
                        let new_id = t.next_clip_id;
                        t.next_clip_id += 1;
                        let clip = DawClip {
                            id: new_id,
                            start_beat: beat.max(0.0),
                            len_beats: 2.0,
                            name: Arc::from(format!("new{new_id}")),
                            color: None,
                        };
                        let pos = t
                            .clips
                            .iter()
                            .position(|c| c.start_beat > clip.start_beat)
                            .unwrap_or(t.clips.len());
                        t.clips.insert(pos, clip);
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: CreateClip @ track {track} beat {beat:.2}");
                })
            }
            ArrangementEditRequest::BeginRenameTrack(id) => Edit::mutate(move |mm: &mut DawModel| {
                mm.arr_rename_target = Some(id);
                mm.arr_rename_just_started = true;
                mm.last_action = format!("arr: BeginRenameTrack {id}");
            }),
            ArrangementEditRequest::DeleteTrack(id) => Edit::mutate(move |mm: &mut DawModel| {
                mm.arr_tracks.retain(|t| t.id != id);
                if mm.arr_selected_track == Some(id) {
                    mm.arr_selected_track = None;
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: DeleteTrack {id}");
            }),
            ArrangementEditRequest::MoveTrackUp(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(idx) = mm.arr_tracks.iter().position(|t| t.id == id)
                    && idx > 0
                {
                    mm.arr_tracks.swap(idx, idx - 1);
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: MoveTrackUp {id}");
            }),
            ArrangementEditRequest::MoveTrackDown(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(idx) = mm.arr_tracks.iter().position(|t| t.id == id)
                    && idx + 1 < mm.arr_tracks.len()
                {
                    mm.arr_tracks.swap(idx, idx + 1);
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: MoveTrackDown {id}");
            }),
            ArrangementEditRequest::ReorderTracks(order) => Edit::mutate(move |mm: &mut DawModel| {
                let n = order.len();
                // id → DawTrack の lookup table を作って、order 順で並べ直す。
                // Vec::swap_remove で順次取り出すと O(n^2) になるが N_TRACKS=12 なので問題なし。
                let mut new_tracks: Vec<DawTrack> = Vec::with_capacity(n);
                for id in &order {
                    if let Some(pos) = mm.arr_tracks.iter().position(|t| t.id == *id) {
                        new_tracks.push(mm.arr_tracks.remove(pos));
                    }
                }
                // order に含まれなかった track は末尾に keep (gui_01 widget が一部 id だけ送る semantics は
                // 無いが、防御的に)。
                new_tracks.append(&mut mm.arr_tracks);
                mm.arr_tracks = new_tracks;
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ReorderTracks ({n})");
            }),
            ArrangementEditRequest::ToggleTrackMute(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == id) {
                    t.muted = !t.muted;
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ToggleMute {id}");
            }),
            ArrangementEditRequest::ToggleTrackSolo(id) => Edit::mutate(move |mm: &mut DawModel| {
                if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == id) {
                    t.solo = !t.solo;
                }
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: ToggleSolo {id}");
            }),
            ArrangementEditRequest::SetLoopRange { start, end } => Edit::mutate(move |mm: &mut DawModel| {
                let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
                mm.arr_view.loop_range = Some((lo, hi));
                mm.last_action = format!("arr: SetLoopRange [{lo:.2}, {hi:.2}]");
            }),
            ArrangementEditRequest::SetZoomX(factor) => Edit::mutate(move |mm: &mut DawModel| {
                let new_len = (mm.arr_view.len_beats * f64::from(factor)).clamp(1.0, 256.0);
                mm.arr_view.len_beats = new_len;
                mm.last_action = format!("arr: SetZoomX → len={new_len:.2}");
            }),
            ArrangementEditRequest::SetScrollX(start) => Edit::mutate(move |mm: &mut DawModel| {
                mm.arr_view.start_beat = start.max(0.0);
            }),
            ArrangementEditRequest::SetTrackTop(top) => Edit::mutate(move |mm: &mut DawModel| {
                let max_top = (mm.arr_tracks.len() as f32 - mm.arr_view.tracks_visible)
                    .max(0.0)
                    * mm.arr_view.track_row_h;
                mm.arr_view.track_top = top.clamp(0.0, max_top);
            }),
            ArrangementEditRequest::SetTrackRowH(h) => Edit::mutate(move |mm: &mut DawModel| {
                let new_h = h.clamp(16.0, 96.0);
                mm.arr_view.track_row_h = new_h;
                // row_h 変化に伴う track_top の上限再計算 (拡大時に下端が空かないように)。
                let max_top = (mm.arr_tracks.len() as f32 - mm.arr_view.tracks_visible)
                    .max(0.0)
                    * new_h;
                mm.arr_view.track_top = mm.arr_view.track_top.clamp(0.0, max_top);
                mm.arr_view.data_generation += 1;
                mm.last_action = format!("arr: SetTrackRowH → {new_h:.1}");
            }),
        },
    );

    // ---- track header 右クリック context_menu (Rename / Delete) ----
    for (track_id, header_rect) in &resp.track_header_rects {
        let tid = *track_id;
        ui.context_menu_for(*header_rect, &["Rename", "Delete"], move |idx, ui| {
            match idx {
                0 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_rename_target = Some(tid);
                    mm.arr_rename_just_started = true;
                    mm.last_action = format!("arr: Rename {tid} (context)");
                })),
                1 => ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_tracks.retain(|t| t.id != tid);
                    if mm.arr_selected_track == Some(tid) {
                        mm.arr_selected_track = None;
                    }
                    mm.arr_view.data_generation += 1;
                    mm.last_action = format!("arr: Delete {tid} (context)");
                })),
                _ => {}
            }
        });
    }

    // ---- Rename UI overlay (text_input_at を該当 header rect 上に重ねる) ----
    if let Some(rid) = m.arr_rename_target {
        // 該当 track の現名 + header rect を引く
        let cur_name = m
            .arr_tracks
            .iter()
            .find(|t| t.id == rid)
            .map(|t| t.name.to_string())
            .unwrap_or_default();
        let header_rect = resp
            .track_header_rects
            .iter()
            .find(|(id, _)| *id == rid)
            .map(|(_, r)| *r);
        if let Some(rect) = header_rect {
            let pad = 4.0_f32;
            let text_rect = Rect {
                x: rect.x + pad,
                y: rect.y + pad,
                w: (rect.w - pad * 2.0).max(20.0),
                h: (rect.h - pad * 2.0).max(20.0),
            };
            // 初回フレームで text_input にプログラム的に focus を与える (内部 wid は
            // `WidgetId::ROOT.child((b"text_input", &id))` 規約)。
            if m.arr_rename_just_started {
                let text_input_wid =
                    WidgetId::ROOT.child((b"text_input", &("arr_rename", rid)));
                ui.set_focus(text_input_wid);
                ui.push_edit(Edit::mutate(|mm: &mut DawModel| {
                    mm.arr_rename_just_started = false;
                }));
            }
            // text_input は背景塗りを持たないため、後ろの track header text が透けて見える。
            // overlay 用に不透明 panel を先に置く (text_input より一段下、glyph より上に来る)。
            ui.panel(
                ("arr_rename_bg", rid),
                text_rect,
                Color::rgb(0.18, 0.20, 0.24),
                3.0,
            );
            // on_change では track 名だけ更新 (`arr_rename_target` は触らない、
            // overlay 消去は Enter (resp.committed) / ESC (take_shortcut) で行う)。
            let resp_text =
                ui.text_input_at(("arr_rename", rid), text_rect, &cur_name, move |new| {
                    Edit::mutate(move |mm: &mut DawModel| {
                        if let Some(t) = mm.arr_tracks.iter_mut().find(|t| t.id == rid) {
                            t.name = Arc::from(new.as_str());
                        }
                        mm.arr_view.data_generation += 1;
                        mm.last_action = format!("arr: rename → {new}");
                    })
                });
            // Enter で確定 → overlay 消去
            if resp_text.committed {
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_rename_target = None;
                    mm.last_action = format!("arr: Rename {rid} committed");
                }));
            }
            // ESC でキャンセル
            if ui.take_shortcut("escape") {
                ui.push_edit(Edit::mutate(move |mm: &mut DawModel| {
                    mm.arr_rename_target = None;
                    mm.last_action = "arr: Rename cancelled".to_string();
                }));
            }
        } else {
            // header_rect が引けない (track 削除済) → クリア
            ui.push_edit(Edit::mutate(|mm: &mut DawModel| {
                mm.arr_rename_target = None;
                mm.arr_rename_just_started = false;
            }));
        }
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
        match ev {
            AppEvent::Resized(s) => {
                self.renderer.resize(s);
                self.window.request_redraw();
            }
            // winit ControlFlow::Wait では入力イベントだけでは再描画されないため、
            // 入力が来たら明示的に request_redraw して build_ui を走らせる (mixer 同パターン)。
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
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e:?}");
        }
        // IME (text_input がない demo だが、念のため empty で disable)
        self.window.set_ime_allowed(false);
        // false: 連続再描画は library 側 (level_meter / tab_view / scroll_area /
        // split_view 等が `Ui::request_redraw()` を呼ぶ) と Edit / focus 変化の
        // auto-redraw に任せる。アイドル時 (sim_phase 動かない / tab 切替なし) は
        // 0fps で電力節約。
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
