//! examples/arrangement — M5 Phase 17 動作確認サンプル (M5 最終 phase)。
//!
//! Phase 13 で導入した heavy() の **多数 widget スケール** 用法を実証する:
//! 10 tracks × 50 clips = 500 個の `Ui::waveform` widget を 1 つの heavy ブロックで
//! 束ね、`HeavyCtx::cached(viewport_key, ...)` で外側粗粒度キャッシュを効かせる。
//!
//! 二段キャッシュ:
//! - 外側 cached() hit (viewport_key 一致) → 内側 500 widgets の `with_widget_node`
//!   も全て skip、scene への描画コマンドは前フレームの extend_from_slice で復帰
//! - 外側 cached() miss → 内側 500 widgets が個別に input_hash 判定
//!
//! 操作:
//! - 左ドラッグ: 横スクロール (X pan、全 clip 同期)
//! - マウスホイール: X zoom (cur_mouse 位置を anchor)
//! - Ctrl+Wheel: Y zoom (レーン高さ y_zoom + y_offset anchor 維持型)
//! - キー [1] PeakLines / [2] SamplePolyline / [3] RmsBars / [a] Auto

use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    ChannelLayout, InputAccumulator, SampleSlices, UiHost, WaveformRenderMode, WaveformSource,
    WaveformStyle, WaveformView, hash_inputs,
};
use daw_ui_platform::{
    AppEvent, AppHost, ElementState, Modifiers, PhysicalSize, ScrollDelta, WindowBackend,
    winit_backend,
};
use daw_ui_renderer::{Color, Rect, Renderer, Scene};
use winit::window::WindowAttributes;

const SAMPLE_RATE: u32 = 48_000;
const SECONDS: f32 = 60.0;

/// 10 tracks × 50 clips = 500 widgets。Phase 17 の M5 元仕様。
const TRACKS: usize = 10;
const CLIPS_PER_TRACK: usize = 50;
const N_WIDGETS: usize = TRACKS * CLIPS_PER_TRACK;

// ----- Model -----

struct ArrangementModel {
    samples: Vec<Vec<f32>>, // Planar 1ch、全 clip で共有
    valid_len: usize,
    generation: u64,

    view_start: u64,
    view_len: u64,
    vertical_gain: f32,

    y_zoom: f32,
    y_offset: f32,

    forced_mode: Option<WaveformRenderMode>,

    last_frame_ms: f32,
    last_action: String,
}

impl ArrangementModel {
    fn new() -> Self {
        let samples = generate_test_samples(SECONDS, SAMPLE_RATE);
        let total = samples.first().map_or(0, Vec::len);
        Self {
            samples,
            valid_len: total,
            generation: 0,
            view_start: 0,
            view_len: total as u64,
            vertical_gain: 1.0,
            y_zoom: 1.0,
            y_offset: 0.0,
            forced_mode: None,
            last_frame_ms: 0.0,
            last_action: format!("起動 ({N_WIDGETS} widgets を heavy 化)"),
        }
    }

    fn total_frames(&self) -> u64 {
        self.samples.first().map_or(0, |p| p.len() as u64)
    }

    fn pan_pixels(&mut self, dx: f32, widget_w: f32) {
        if widget_w <= 0.0 || self.view_len == 0 {
            return;
        }
        let spp = self.view_len as f64 / f64::from(widget_w);
        let delta_samples = (f64::from(dx) * spp) as i64;
        let total = self.total_frames();
        let max_start = total.saturating_sub(self.view_len);
        self.view_start = if delta_samples >= 0 {
            self.view_start.saturating_sub(delta_samples.unsigned_abs())
        } else {
            self.view_start
                .saturating_add(delta_samples.unsigned_abs())
                .min(max_start)
        };
    }

    fn zoom_at(&mut self, factor: f32, anchor_frac: f32) {
        if self.view_len == 0 {
            return;
        }
        let total = self.total_frames();
        let anchor_sample =
            self.view_start as f64 + f64::from(anchor_frac) * self.view_len as f64;
        let new_len = ((self.view_len as f32) * factor)
            .max(64.0)
            .min(total as f32) as u64;
        let new_anchor_offset = f64::from(anchor_frac) * new_len as f64;
        let new_start = (anchor_sample - new_anchor_offset).max(0.0) as u64;
        let max_start = total.saturating_sub(new_len);
        self.view_start = new_start.min(max_start);
        self.view_len = new_len.max(1);
    }
}

fn generate_test_samples(seconds: f32, sample_rate: u32) -> Vec<Vec<f32>> {
    let frames = (seconds * sample_rate as f32) as usize;
    let mut plane: Vec<f32> = Vec::with_capacity(frames);
    for i in 0..frames {
        let t = i as f32 / sample_rate as f32;
        let env = ((t * 0.5 * std::f32::consts::TAU).sin() * 0.5 + 0.5).powi(2);
        let f = 220.0;
        let phase = (t * f * std::f32::consts::TAU).sin();
        let harm = (t * f * 2.0 * std::f32::consts::TAU).sin() * 0.3;
        let n = (i.wrapping_mul(1664525).wrapping_add(1013904223)) as u32;
        let noise = (n as f32 / u32::MAX as f32 - 0.5) * 0.1;
        plane.push((phase + harm + noise) * env * 0.85);
    }
    vec![plane]
}

fn arrangement_area(screen: PhysicalSize) -> Rect {
    let pad_x = 8.0;
    let header_h = 88.0;
    let footer_h = 56.0;
    let w = (screen.width as f32 - pad_x * 2.0).max(100.0);
    let h = (screen.height as f32 - header_h - footer_h).max(100.0);
    Rect { x: pad_x, y: header_h, w, h }
}

fn clip_rect(area: Rect, i: usize, y_zoom: f32, y_offset: f32) -> Rect {
    let col = i % CLIPS_PER_TRACK;
    let row = i / CLIPS_PER_TRACK;
    let cell_w = area.w / CLIPS_PER_TRACK as f32;
    let cell_h = (area.h / TRACKS as f32) * y_zoom;
    Rect {
        x: area.x + col as f32 * cell_w,
        y: area.y + row as f32 * cell_h - y_offset,
        w: (cell_w - 1.0).max(1.0),
        h: (cell_h - 1.0).max(1.0),
    }
}

// ----- App -----

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<ArrangementModel>,
    model: ArrangementModel,
    scene: Scene,
    input: InputAccumulator,

    pending_zoom_dy: f32,
    drag_anchor: Option<(f32, u64)>,
    cur_mouse: Option<(f32, f32)>,
    cur_modifiers: Modifiers,

    /// 直前フレームの viewport_hash (cache HIT/MISS 推定用、approximation)。
    last_viewport_hash: Option<u64>,
    last_cache_hit: bool,

    last_frame_start: Option<Instant>,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        window.set_title("daw-ui arrangement (M5 Phase 17)");
        Self {
            ui: UiHost::with_window(window.clone()),

            window,
            renderer,
            model: ArrangementModel::new(),
            scene: Scene::new(),
            input: InputAccumulator::new(),
            pending_zoom_dy: 0.0,
            drag_anchor: None,
            cur_mouse: None,
            cur_modifiers: Modifiers::default(),
            last_viewport_hash: None,
            last_cache_hit: false,
            last_frame_start: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let area = arrangement_area(screen);
        let input = self.input.take_input();
        let pointer = input.pointer;

        // 1. drag panning (area 全域がドラッグ対象、無修飾 drag のみ)
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && area.contains(px, py)
            && !self.cur_modifiers.shift
        {
            self.drag_anchor = Some((px, self.model.view_start));
        }
        if pointer.primary_just_released {
            self.drag_anchor = None;
        }
        if let (Some((anchor_x, anchor_view_start)), Some((px, _))) =
            (self.drag_anchor, pointer.pos)
        {
            self.model.view_start = anchor_view_start;
            // 1 clip 幅基準で pan の感度を waveform_validation と揃える
            let cell_w = area.w / CLIPS_PER_TRACK as f32;
            self.model.pan_pixels(px - anchor_x, cell_w);
        }

        // 2. wheel zoom (Ctrl で X/Y 切替)
        if self.pending_zoom_dy.abs() > 0.0 {
            let factor = (-self.pending_zoom_dy * 0.15).exp();
            if self.cur_modifiers.ctrl {
                // Y zoom: y_zoom + y_offset anchor 維持 (waveform_validation と同じロジック)
                let y_factor = 1.0 / factor;
                let cell_h_base = area.h / TRACKS as f32;
                let old_zoom = self.model.y_zoom;
                let new_zoom = (old_zoom * y_factor).clamp(0.1, 16.0);
                let mouse_y = self.cur_mouse.map_or(area.y + area.h * 0.5, |(_, my)| my);
                let local_y = mouse_y - area.y;
                let old_cell_h = (cell_h_base * old_zoom).max(0.001);
                let anchor_track_unit = (local_y + self.model.y_offset) / old_cell_h;
                let new_cell_h = cell_h_base * new_zoom;
                let new_y_offset_raw = anchor_track_unit * new_cell_h - local_y;
                let total_h = TRACKS as f32 * new_cell_h;
                let max_offset = (total_h - area.h).max(0.0);
                self.model.y_zoom = new_zoom;
                self.model.y_offset = new_y_offset_raw.clamp(0.0, max_offset);
            } else {
                // X zoom: view_len、anchor は cur_mouse の clip 内 x 比率
                let anchor_frac = if let Some((mx, _)) = self.cur_mouse {
                    let cell_w = area.w / CLIPS_PER_TRACK as f32;
                    let local = (mx - area.x).rem_euclid(cell_w);
                    (local / cell_w).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                self.model.zoom_at(factor, anchor_frac);
            }
            self.pending_zoom_dy = 0.0;
        }

        // 3. viewport_key + cache HIT/MISS 推定 (build_ui の前段で計算)
        let viewport_key = (
            b"arrangement_v1" as &[u8],
            self.model.view_start,
            self.model.view_len,
            self.model.y_zoom.to_bits(),
            self.model.y_offset.to_bits(),
            self.model.vertical_gain.to_bits(),
            area.w.to_bits(),
            area.h.to_bits(),
            self.model.generation,
            forced_mode_tag(self.model.forced_mode),
        );
        let viewport_hash = hash_inputs(viewport_key);
        self.last_cache_hit = Some(viewport_hash) == self.last_viewport_hash;
        self.last_viewport_hash = Some(viewport_hash);

        let mode_str = match self.model.forced_mode {
            Some(WaveformRenderMode::PeakLines) => "PeakLines (forced)",
            Some(WaveformRenderMode::SamplePolyline) => "SamplePolyline (forced)",
            Some(WaveformRenderMode::RmsBars) => "RmsBars (forced)",
            None | Some(WaveformRenderMode::Auto) => "Auto",
        };
        let hud = format!(
            "frame {:>5.2}ms │ view [{:>7}..{:>7}) │ spp {:>6.1} │ {} widgets │ y_zoom {:.2} │ y_offset {:>5.0} │ mode {} │ cache {}",
            self.model.last_frame_ms,
            self.model.view_start,
            self.model.view_start + self.model.view_len,
            self.model.view_len as f64 / f64::from(area.w),
            N_WIDGETS,
            self.model.y_zoom,
            self.model.y_offset,
            mode_str,
            if self.last_cache_hit { "HIT " } else { "MISS" },
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
                    "daw-ui arrangement — M5 Phase 17 (10 tracks × 50 clips = 500 widgets を heavy 化)",
                    16.0, 16.0, 18.0,
                    Color::rgb(0.95, 0.95, 0.97),
                );
                ui.label_at(
                    "hud",
                    &hud,
                    16.0, 44.0, 13.0,
                    if self.last_cache_hit {
                        Color::rgb(0.55, 0.85, 0.65)
                    } else {
                        Color::rgb(0.95, 0.78, 0.55)
                    },
                );

                // --- 500 widgets を heavy() + cached() で描画 ---
                let plane: &[f32] = m.samples.first().map_or(&[][..], Vec::as_slice);
                let planes: [&[f32]; 1] = [plane];
                let source = WaveformSource {
                    samples: SampleSlices::Planar(&planes),
                    valid_len: m.valid_len,
                    generation: m.generation,
                    sample_rate: SAMPLE_RATE,
                };
                let render_mode = m.forced_mode.unwrap_or(WaveformRenderMode::Auto);
                let style = WaveformStyle {
                    fg: Color::rgb(0.55, 0.78, 0.95),
                    fg_clipped: Color::rgb(0.95, 0.45, 0.40),
                    fill: None,
                    baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.08)),
                    channel_layout: ChannelLayout::Overlay,
                    render_mode,
                    line_width_px: 1.0,
                };

                ui.heavy("arrangement", |hctx| {
                    hctx.cached(viewport_key, |hctx| {
                        let shift_per_clip = m.view_len / CLIPS_PER_TRACK as u64;
                        let max_start = m.total_frames().saturating_sub(m.view_len);
                        for i in 0..N_WIDGETS {
                            let rect = clip_rect(area, i, m.y_zoom, m.y_offset);
                            // 画面外の clip は描画 skip (cached miss 時の widget 描画コスト削減)
                            if rect.y + rect.h < area.y || rect.y > area.y + area.h {
                                continue;
                            }
                            let view = WaveformView {
                                start_sample: m
                                    .view_start
                                    .saturating_add(shift_per_clip * (i as u64))
                                    .min(max_start),
                                len_samples: m.view_len,
                                vertical_gain: m.vertical_gain,
                            };
                            let _ = hctx.waveform(("clip", i), rect, source, view, style);
                        }
                    });
                });

                // --- footer ---
                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "Drag = X pan │ Wheel = X zoom │ Ctrl+Wheel = Y zoom (lane height)",
                    16.0, footer_y, 13.0,
                    Color::rgb(0.65, 0.68, 0.72),
                );
                ui.label_at(
                    "footer2",
                    "[1] PeakLines / [2] SamplePolyline / [3] RmsBars / [a] Auto",
                    16.0, footer_y + 18.0, 13.0,
                    Color::rgb(0.50, 0.55, 0.62),
                );
                ui.label_at(
                    "footer3",
                    &m.last_action,
                    520.0, footer_y + 18.0, 13.0,
                    Color::rgb(0.50, 0.55, 0.62),
                );
            },
        );

    }
}

/// `forced_mode` を viewport_key に含めるための u8 タグ化。
fn forced_mode_tag(mode: Option<WaveformRenderMode>) -> u8 {
    match mode {
        None | Some(WaveformRenderMode::Auto) => 0,
        Some(WaveformRenderMode::PeakLines) => 1,
        Some(WaveformRenderMode::SamplePolyline) => 2,
        Some(WaveformRenderMode::RmsBars) => 3,
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
        // 1/2/3/a で forced_mode 切替 (key.text 経由、Phase 16 fix と同パターン)
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
        self.last_frame_start = Some(now);
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        if let Some(t) = self.last_frame_start.take() {
            self.model.last_frame_ms = t.elapsed().as_secs_f32() * 1000.0;
        }
        // drag 中 / wheel pending 中は連続再描画
        self.drag_anchor.is_some() || self.pending_zoom_dy.abs() > 0.0
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui arrangement (M5 Phase 17)")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
