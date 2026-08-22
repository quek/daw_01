// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! examples/sample_editor — M5 Phase 16 動作確認サンプル。
//!
//! Phase 16 で実装した 3 機能を実用検証する:
//! - 波形 RmsBars (`WaveformRenderMode::RmsBars`、LOD ピラミッドの rms_sum_sq)
//! - サンプル点マーカー (`SamplePolyline` モード時、rect 角丸円)
//! - 1 サンプルクリップ + 選択範囲 + カーソル UI
//!
//! 操作:
//! - 無修飾 Drag: 横スクロール (X pan)
//! - Shift + Drag: 選択範囲設定 (cyan 半透明 overlay)
//! - 短い Click (drag<16px): カーソル位置移動 + 選択解除 (赤い縦線 1px)
//! - Wheel: X zoom
//! - Ctrl+Wheel: Y zoom (vertical_gain、0.05〜64.0)
//! - キー [1] PeakLines / [2] SamplePolyline / [3] RmsBars (強制) / [a] Auto

use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    ChannelLayout, InputAccumulator, SampleSlices, UiHost, ViewportState1D, WaveformRenderMode,
    WaveformSource, WaveformStyle, WaveformView,
};
use daw_ui_platform::{
    AppEvent, AppHost, ElementState, Modifiers, PhysicalSize, ScrollDelta,
    WindowBackend, winit_backend,
};
use daw_ui_renderer::{Color, Rect, RectCommand, Renderer, Scene};
use winit::window::WindowAttributes;

const SAMPLE_RATE: u32 = 48_000;
const SECONDS: f32 = 2.0;

// ----- レイアウト定数 -----

const HEADER_H: f32 = 56.0;
const FOOTER_H: f32 = 56.0;

// ----- Model -----

/// no-Clone 不変条件。`Clone` / `PartialEq` / `Hash` / `Default` は実装しない。
struct SampleEditorModel {
    samples: Vec<Vec<f32>>, // Planar 1ch
    valid_len: usize,
    generation: u64,

    // X 軸 view (M7 Phase 22: ViewportState1D に集約、`view_start as u64` 等で従来 API と互換)
    viewport: ViewportState1D,
    vertical_gain: f32,

    // Phase 16 新機能
    selection: Option<(u64, u64)>, // (start, end) 順序保証 (start <= end)
    cursor_sample: u64,
    /// `None` で Auto。1/2/3 キーで明示上書き、a で None に戻す。
    forced_mode: Option<WaveformRenderMode>,

    // HUD
    last_frame_ms: f32,
    last_action: String,
}

impl SampleEditorModel {
    fn new() -> Self {
        let samples = generate_test_samples(SECONDS, SAMPLE_RATE);
        let total = samples.first().map_or(0, Vec::len);
        Self {
            samples,
            valid_len: total,
            generation: 0,
            viewport: ViewportState1D::new(0.0, total as f64),
            vertical_gain: 1.0,
            selection: None,
            cursor_sample: 0,
            forced_mode: None,
            last_frame_ms: 0.0,
            last_action: "起動 (Drag = pan / Shift+Drag = 選択 / Click = cursor)".to_string(),
        }
    }

    fn total_frames(&self) -> u64 {
        self.samples.first().map_or(0, |p| p.len() as u64)
    }

    /// `dx_pixels` だけ pan する (M7: ViewportState1D 経由)。
    fn pan_pixels(&mut self, dx: f32, widget_w: f32) {
        let total = self.total_frames() as f64;
        self.viewport.pan_pixels(dx, widget_w);
        self.viewport.clamp_to(total);
    }

    /// `anchor_frac` (0..1) を中心に `factor` 倍 X zoom する (M7: ViewportState1D 経由)。
    fn zoom_at(&mut self, factor: f32, anchor_frac: f32) {
        let total = self.total_frames() as f64;
        self.viewport.zoom_at(factor, anchor_frac, 8.0);
        self.viewport.clamp_to(total);
    }

    /// 画面 x px から sample idx に変換。
    fn x_to_sample(&self, x: f32, area: Rect) -> u64 {
        let local_x = (x - area.x).clamp(0.0, area.w);
        self.viewport.px_to_unit(local_x, area.w) as u64
    }

    /// sample idx から画面 x px に変換。
    fn sample_to_x(&self, s: u64, area: Rect) -> f32 {
        area.x + self.viewport.unit_to_px(s as f64, area.w)
    }
}

/// 決定論的 sin + 倍音 + 軽量ノイズで `seconds` 秒分のモノラル波形を生成。
fn generate_test_samples(seconds: f32, sample_rate: u32) -> Vec<Vec<f32>> {
    let frames = (seconds * sample_rate as f32) as usize;
    let mut plane: Vec<f32> = Vec::with_capacity(frames);
    for i in 0..frames {
        let t = i as f32 / sample_rate as f32;
        // envelope は中央で最大、両端で 0
        let env = (t * std::f32::consts::PI / seconds).sin().max(0.0);
        let f1 = (t * 220.0 * std::f32::consts::TAU).sin();
        let f2 = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        let f3 = (t * 880.0 * std::f32::consts::TAU).sin() * 0.25;
        // 決定論的 LCG ノイズ
        let n = (i.wrapping_mul(1664525).wrapping_add(1013904223)) as u32;
        let noise = (n as f32 / u32::MAX as f32 - 0.5) * 0.05;
        plane.push(((f1 + f2 + f3) * env + noise) * 0.85);
    }
    vec![plane]
}

fn waveform_area(screen: PhysicalSize) -> Rect {
    let pad_x = 16.0;
    let w = (screen.width as f32 - pad_x * 2.0).max(100.0);
    let h = (screen.height as f32 - HEADER_H - FOOTER_H).max(100.0);
    Rect { x: pad_x, y: HEADER_H, w, h }
}

// ----- App -----

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<SampleEditorModel>,
    model: SampleEditorModel,
    scene: Scene,
    input: InputAccumulator,

    /// drag 状態: (anchor_x, anchor_view_start (f64 unit = sample), anchor_sample, accum_dx, kind)
    /// kind: false = pan、true = selection
    drag_anchor: Option<(f32, f64, u64, f32, bool)>,
    cur_mouse: Option<(f32, f32)>,
    cur_modifiers: Modifiers,
    pending_zoom_dy: f32,
    pending_click: Option<f32>,

    last_frame_start: Option<Instant>,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        window.set_title("daw-ui sample_editor (M5 Phase 16)");
        Self {
            ui: UiHost::with_window(window.clone()),

            window,
            renderer,
            model: SampleEditorModel::new(),
            scene: Scene::new(),
            input: InputAccumulator::new(),
            drag_anchor: None,
            cur_mouse: None,
            cur_modifiers: Modifiers::default(),
            pending_zoom_dy: 0.0,
            pending_click: None,
            last_frame_start: None,
        }
    }

    fn apply_pending_input(&mut self, screen: PhysicalSize) {
        let area = waveform_area(screen);
        let pointer = self.input.take_frame();

        // --- drag 開始 ---
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && area.contains(px, py)
        {
            let anchor_sample = self.model.x_to_sample(px, area);
            let kind_selection = self.cur_modifiers.shift;
            self.drag_anchor =
                Some((px, self.model.viewport.view_start, anchor_sample, 0.0, kind_selection));
            self.pending_click = None;
            // Shift+drag 開始時点で selection の anchor を確定
            if kind_selection {
                self.model.selection = Some((anchor_sample, anchor_sample));
            }
        }

        // --- drag 中 ---
        if let (Some((ax, ave_start, anchor_sample, _accum, kind)), Some((px, _))) =
            (self.drag_anchor, pointer.pos)
        {
            let dx = px - ax;
            if kind {
                // Selection: anchor_sample から現在 sample idx までを範囲に
                let cur_sample = self.model.x_to_sample(px, area);
                let (s, e) = if cur_sample >= anchor_sample {
                    (anchor_sample, cur_sample)
                } else {
                    (cur_sample, anchor_sample)
                };
                self.model.selection = Some((s, e));
            } else {
                // Pan: view_start を anchor から復元 + dx 分だけ pan
                self.model.viewport.view_start = ave_start;
                self.model.pan_pixels(dx, area.w);
            }
            if let Some(anchor) = self.drag_anchor.as_mut() {
                anchor.3 = dx.abs();
            }
        }

        // --- drag 終了 ---
        if pointer.primary_just_released
            && let Some((_, _, _, accum_dx, kind)) = self.drag_anchor.take()
            && !kind
            && accum_dx < 16.0
            && let Some((px, _)) = pointer.pos
        {
            self.pending_click = Some(px);
        }

        // --- wheel zoom ---
        if self.pending_zoom_dy.abs() > 0.0 {
            let factor = (-self.pending_zoom_dy * 0.15).exp();
            if self.cur_modifiers.ctrl {
                let new_gain = (self.model.vertical_gain * factor).clamp(0.05, 64.0);
                self.model.vertical_gain = new_gain;
            } else {
                let anchor_frac = if let Some((mx, _)) = self.cur_mouse {
                    ((mx - area.x) / area.w).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                self.model.zoom_at(factor, anchor_frac);
            }
            self.pending_zoom_dy = 0.0;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let area = waveform_area(screen);
        let input = self.input.take_input();

        // pending_click を消費して cursor 移動 + selection 解除
        if let Some(click_px) = self.pending_click.take() {
            let s = self.model.x_to_sample(click_px, area);
            self.model.cursor_sample = s;
            self.model.selection = None;
            self.model.last_action = format!("cursor → sample {s}");
        }

        let mode_str = match self.model.forced_mode {
            Some(WaveformRenderMode::PeakLines) => "PeakLines (forced)",
            Some(WaveformRenderMode::SamplePolyline) => "SamplePolyline (forced)",
            Some(WaveformRenderMode::RmsBars) => "RmsBars (forced)",
            None | Some(WaveformRenderMode::Auto) => "Auto",
        };
        let hud = format!(
            "frame {:>5.2}ms │ view [{:>7}..{:>7}) │ spp {:>6.1} │ mode {} │ cursor {} │ sel {} │ gain {:.2}x",
            self.model.last_frame_ms,
            self.model.viewport.view_start,
            self.model.viewport.view_start + self.model.viewport.view_len,
            self.model.viewport.view_len / f64::from(area.w),
            mode_str,
            self.model.cursor_sample,
            self.model.selection.map_or("-".to_string(), |(s, e)| format!("[{s}..{e}) {} samples", e - s)),
            self.model.vertical_gain,
        );

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // --- HUD ---
                ui.label_at(
                    "title",
                    "daw-ui sample_editor — M5 Phase 16 (RmsBars + sample マーカー + 選択範囲)",
                    16.0, 16.0, 16.0,
                    Color::rgb(0.92, 0.95, 0.98),
                );
                ui.label_at("hud", &hud, 16.0, 36.0, 12.0, Color::rgb(0.75, 0.78, 0.82));

                // --- waveform 本体 ---
                let plane: &[f32] = m.samples.first().map_or(&[][..], Vec::as_slice);
                let planes: [&[f32]; 1] = [plane];
                let source = WaveformSource {
                    samples: SampleSlices::Planar(&planes),
                    valid_len: m.valid_len,
                    generation: m.generation,
                    sample_rate: SAMPLE_RATE,
                };
                let view = WaveformView {
                    start_sample: m.viewport.view_start as u64,
                    len_samples: m.viewport.view_len as u64,
                    vertical_gain: m.vertical_gain,
                    reversed: false,
                };
                let render_mode = m.forced_mode.unwrap_or(WaveformRenderMode::Auto);
                let style = WaveformStyle {
                    fg: Color::rgb(0.55, 0.78, 0.95),
                    fg_clipped: Color::rgb(0.95, 0.45, 0.40),
                    fill: None,
                    baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.10)),
                    channel_layout: ChannelLayout::Overlay,
                    render_mode,
                    line_width_px: 1.0,
                };
                let _ = ui.waveform("main", area, source, view, style);

                // --- selection overlay + cursor (heavy() 経由で push_rect、毎フレーム) ---
                // Ui::push_rect は pub(crate) のため、example からは HeavyCtx 経由 (pub) で push する。
                // cached() は使わない (毎フレーム描画でよい、cache 効果なし)。
                ui.heavy("overlay", |hctx| {
                    if let Some((s, e)) = m.selection {
                        let x_s = m.sample_to_x(s, area);
                        let x_e = m.sample_to_x(e, area);
                        let rect = Rect {
                            x: x_s.min(x_e),
                            y: area.y,
                            w: (x_e - x_s).abs().max(1.0),
                            h: area.h,
                        };
                        hctx.push_rect(RectCommand {
                            rect,
                            fill: Color::rgba(0.0, 0.85, 1.0, 0.20),
                            border: Color::TRANSPARENT,
                            border_width: 0.0,
                            radius: [0.0; 4],
                            clip_rect: None,
                        });
                    }
                    let x_c = m.sample_to_x(m.cursor_sample, area);
                    hctx.push_rect(RectCommand {
                        rect: Rect { x: x_c - 0.5, y: area.y, w: 1.0, h: area.h },
                        fill: Color::rgb(1.0, 0.30, 0.30),
                        border: Color::TRANSPARENT,
                        border_width: 0.0,
                        radius: [0.0; 4],
                        clip_rect: None,
                    });
                });

                // --- footer ---
                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "Drag = pan / Shift+Drag = 選択 / Click = cursor / Wheel = X zoom / Ctrl+Wheel = Y gain",
                    16.0, footer_y, 12.0,
                    Color::rgb(0.65, 0.68, 0.72),
                );
                ui.label_at(
                    "footer2",
                    "[1] PeakLines / [2] SamplePolyline / [3] RmsBars / [a] Auto │ ",
                    16.0, footer_y + 18.0, 12.0,
                    Color::rgb(0.50, 0.55, 0.62),
                );
                ui.label_at(
                    "footer3",
                    &m.last_action,
                    560.0, footer_y + 18.0, 12.0,
                    Color::rgb(0.50, 0.55, 0.62),
                );
            },
        );

    }
}

impl AppHost for App {
    fn on_event(&mut self, ev: AppEvent) {
        if let AppEvent::PointerMoved(p) = &ev {
            self.cur_mouse = Some((p.x as f32, p.y as f32));
        }
        if let AppEvent::PointerLeft = &ev {
            self.cur_mouse = None;
        }
        if let AppEvent::Scroll(delta) = &ev {
            let dy = match delta {
                ScrollDelta::Lines { y, .. } => *y,
                ScrollDelta::Pixels { y, .. } => *y as f32 / 30.0,
            };
            self.pending_zoom_dy += dy;
        }
        if let AppEvent::ModifiersChanged(m) = &ev {
            self.cur_modifiers = *m;
        }
        // 1/2/3/a キーで forced_mode 切替。`PhysicalKey::Other` は winit の `KeyCode`
        // discriminant で OS 非依存だが値が予測困難 → `key.text` 経由で判定する
        // (Shift+a でも大文字 "A" として届くので eq_ignore_ascii_case で吸収)。
        if let AppEvent::Keyboard(key) = &ev
            && key.state == ElementState::Pressed
        {
            match key.text.as_deref() {
                Some("1") => {
                    self.model.forced_mode = Some(WaveformRenderMode::PeakLines);
                    self.model.last_action = "forced: PeakLines".to_string();
                }
                Some("2") => {
                    self.model.forced_mode = Some(WaveformRenderMode::SamplePolyline);
                    self.model.last_action = "forced: SamplePolyline".to_string();
                }
                Some("3") => {
                    self.model.forced_mode = Some(WaveformRenderMode::RmsBars);
                    self.model.last_action = "forced: RmsBars".to_string();
                }
                Some(s) if s.eq_ignore_ascii_case("a") => {
                    self.model.forced_mode = None;
                    self.model.last_action = "Auto モードに戻る".to_string();
                }
                _ => {}
            }
        }
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
        let screen = self.renderer.size();
        self.apply_pending_input(screen);
        self.last_frame_start = Some(now);
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        if let Some(t) = self.last_frame_start.take() {
            self.model.last_frame_ms = t.elapsed().as_secs_f32() * 1000.0;
        }
        // drag / wheel 中は連続再描画
        self.drag_anchor.is_some() || self.pending_zoom_dy.abs() > 0.0
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui sample_editor (M5 Phase 16)")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 600.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
