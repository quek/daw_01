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
    ChannelLayout, InputAccumulator, SampleSlices, UiHost, ViewportState1D, WaveformRenderMode,
    WaveformSource, WaveformStyle, WaveformView, hash_inputs,
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

    /// X 軸 view (M7+: ViewportState1D に集約、sample 単位)。
    viewport: ViewportState1D,
    vertical_gain: f32,

    /// レーン高さ倍率 (Ctrl+Wheel で操作)。Y scroll は scroll_area widget が管理。
    y_zoom: f32,

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
            viewport: ViewportState1D::new(0.0, total as f64),
            vertical_gain: 1.0,
            y_zoom: 1.0,
            forced_mode: None,
            last_frame_ms: 0.0,
            last_action: format!("起動 ({N_WIDGETS} widgets を heavy 化, scroll_area 適用)"),
        }
    }

    fn total_frames(&self) -> u64 {
        self.samples.first().map_or(0, |p| p.len() as u64)
    }

    fn pan_pixels(&mut self, dx: f32, widget_w: f32) {
        let total = self.total_frames() as f64;
        self.viewport.pan_pixels(dx, widget_w);
        self.viewport.clamp_to(total);
    }

    fn zoom_at(&mut self, factor: f32, anchor_frac: f32) {
        let total = self.total_frames() as f64;
        self.viewport.zoom_at(factor, anchor_frac, 64.0);
        self.viewport.clamp_to(total);
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

/// 各 clip の時間軸上の配置 (DAW タイムライン式、X zoom が clip 幅に連動)。
/// `viewport` で X 軸を unit (sample) → px に変換。
/// `total_frames` は全 sample 数 (clip 配置の基準)。
/// `scroll_offset_y` は scroll_area から得られる縦 scroll 量 (px)。
fn clip_rect(
    area: Rect,
    i: usize,
    y_zoom: f32,
    scroll_offset_y: f32,
    viewport: &ViewportState1D,
    total_frames: f64,
) -> Rect {
    let col = i % CLIPS_PER_TRACK;
    let row = i / CLIPS_PER_TRACK;
    // 各 clip は CLIPS_PER_TRACK 等分で時間軸に並ぶ。clip 内は 95% を占めて 5% 間隙。
    let clip_spacing = total_frames / CLIPS_PER_TRACK as f64;
    let clip_start = col as f64 * clip_spacing;
    let clip_end = clip_start + clip_spacing * 0.95;
    let x_start = viewport.unit_to_px(clip_start, area.w);
    let x_end = viewport.unit_to_px(clip_end, area.w);
    let cell_h = (area.h / TRACKS as f32) * y_zoom;
    Rect {
        x: area.x + x_start,
        y: area.y + row as f32 * cell_h - scroll_offset_y,
        w: (x_end - x_start).max(1.0),
        h: (cell_h - 1.0).max(1.0),
    }
}

/// 各 clip の時間軸上の長さ (sample 数)。clip_rect の cell_spacing × 0.95 と一致。
fn clip_length_samples(total_frames: f64) -> u64 {
    (total_frames / CLIPS_PER_TRACK as f64 * 0.95) as u64
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
    /// drag 開始時の (mouse_x, viewport.view_start [unit = sample, f64])
    drag_anchor: Option<(f32, f64)>,
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
        // M7+: viewport.view_start (f64) を anchor として保存
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && area.contains(px, py)
            && !self.cur_modifiers.shift
        {
            self.drag_anchor = Some((px, self.model.viewport.view_start));
        }
        if pointer.primary_just_released {
            self.drag_anchor = None;
        }
        if let (Some((anchor_x, anchor_view_start)), Some((px, _))) =
            (self.drag_anchor, pointer.pos)
        {
            self.model.viewport.view_start = anchor_view_start;
            // pan 感度: area 全幅基準 (= 画面 1 横分 drag で view_len 全部動く、DAW 標準)。
            // 旧仕様 (cell_w 基準) は clip が固定配置だった頃の名残で、新仕様では area.w が正しい。
            self.model.pan_pixels(px - anchor_x, area.w);
        }

        // 2. wheel zoom — 新仕様 (M7+):
        //   - 無修飾 Wheel: scroll_area 経由で Y scroll (本関数では何もしない)
        //   - Shift+Wheel: X zoom (anchor = area 内の mouse_x 比率、clip 全体が時間軸上で zoom)
        //   - Ctrl+Wheel: Y zoom (lane height) + scroll_area offset を anchor 維持で同期
        // pending_zoom_dy は on_event で「修飾あり wheel のみ」蓄積される (下記 on_event 参照)。
        // pending_y_zoom_update: (new_y_zoom, scale, local_y) を frame 内で scroll_offset と組み合わせる
        let mut pending_y_zoom_update: Option<(f32, f32, f32)> = None;
        if self.pending_zoom_dy.abs() > 0.0 {
            let factor = (-self.pending_zoom_dy * 0.15).exp();
            if self.cur_modifiers.ctrl {
                let y_factor = 1.0 / factor;
                let cell_h_base = area.h / TRACKS as f32;
                let old_zoom = self.model.y_zoom;
                let new_zoom = (old_zoom * y_factor).clamp(0.1, 16.0);
                let mouse_y = self.cur_mouse.map_or(area.y + area.h * 0.5, |(_, my)| my);
                let local_y = (mouse_y - area.y).clamp(0.0, area.h);
                let old_cell_h = (cell_h_base * old_zoom).max(0.001);
                let new_cell_h = cell_h_base * new_zoom;
                let scale = new_cell_h / old_cell_h;
                // anchor 維持: new_offset = (local_y + old_offset) * scale - local_y
                // (frame closure 内で old_offset = ui.scroll_offset を取得して計算)
                pending_y_zoom_update = Some((new_zoom, scale, local_y));
            } else if self.cur_modifiers.shift {
                // X zoom: anchor は mouse_x の area 内比率 (DAW 標準: ポインタ位置を中心に zoom)
                let anchor_frac = if let Some((mx, _)) = self.cur_mouse {
                    ((mx - area.x) / area.w).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                self.model.zoom_at(factor, anchor_frac);
            }
            self.pending_zoom_dy = 0.0;
        }

        // scroll_area の content_size。y_zoom × TRACKS 分の高さ。
        let cell_h = (area.h / TRACKS as f32) * self.model.y_zoom;
        let content_h = TRACKS as f32 * cell_h;
        let content_size = (area.w, content_h);

        // 3. viewport_key + cache HIT/MISS 推定 (scroll_offset を frame 内で取得して再計算)
        // 一旦、簡易的に y_zoom と viewport だけで viewport_key を作る。
        let viewport_key_seed = (
            b"arrangement_v2" as &[u8],
            self.model.viewport.view_start.to_bits(),
            self.model.viewport.view_len.to_bits(),
            self.model.y_zoom.to_bits(),
            self.model.vertical_gain.to_bits(),
            area.w.to_bits(),
            area.h.to_bits(),
            self.model.generation,
            forced_mode_tag(self.model.forced_mode),
        );

        let mode_str = match self.model.forced_mode {
            Some(WaveformRenderMode::PeakLines) => "PeakLines (forced)",
            Some(WaveformRenderMode::SamplePolyline) => "SamplePolyline (forced)",
            Some(WaveformRenderMode::RmsBars) => "RmsBars (forced)",
            None | Some(WaveformRenderMode::Auto) => "Auto",
        };
        let view_start_u = self.model.viewport.view_start as u64;
        let view_len_u = self.model.viewport.view_len as u64;
        let hud = format!(
            "frame {:>5.2}ms │ view [{:>7}..{:>7}) │ spp {:>6.1} │ {} widgets │ y_zoom {:.2} │ mode {} │ cache {}",
            self.model.last_frame_ms,
            view_start_u,
            view_start_u + view_len_u,
            self.model.viewport.view_len / f64::from(area.w),
            N_WIDGETS,
            self.model.y_zoom,
            mode_str,
            if self.last_cache_hit { "HIT " } else { "MISS" },
        );

        let last_cache_hit = self.last_cache_hit;
        let last_viewport_hash = &mut self.last_viewport_hash;
        let mut new_viewport_hash: u64 = 0;

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // Ctrl+Wheel zoom の anchor 維持: scroll_area の offset を更新
                // anchor 維持式: new_offset = (local_y + old_offset) * scale - local_y
                // → mouse 位置 (local_y) が指す track unit が zoom 後も同じ screen 位置に残る
                if let Some((new_zoom, scale, local_y)) = pending_y_zoom_update {
                    let old_offset = ui.scroll_offset("arr_scroll").1;
                    let new_offset = ((local_y + old_offset) * scale - local_y).max(0.0);
                    ui.set_scroll_offset("arr_scroll", (0.0, new_offset));
                    ui.push_edit(daw_ui_core::Edit::mutate(move |mm: &mut ArrangementModel| {
                        mm.y_zoom = new_zoom;
                    }));
                }

                // --- HUD ---
                ui.label_at(
                    "title",
                    "daw-ui arrangement — M5 Phase 17 + M7 scroll_area (10 tracks × 50 clips = 500 widgets)",
                    16.0, 16.0, 18.0,
                    Color::rgb(0.95, 0.95, 0.97),
                );
                ui.label_at(
                    "hud",
                    &hud,
                    16.0, 44.0, 13.0,
                    if last_cache_hit {
                        Color::rgb(0.55, 0.85, 0.65)
                    } else {
                        Color::rgb(0.95, 0.78, 0.55)
                    },
                );

                // --- 500 widgets を scroll_area + heavy() + cached() で描画 ---
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

                let view_len_unit = m.viewport.view_len;
                let view_start_unit = m.viewport.view_start;
                let total_frames = m.total_frames() as f64;

                ui.scroll_area("arr_scroll", area, content_size, |ui, offset| {
                    // viewport_key に scroll_offset_y も含める (scroll で見える行が変わるため)
                    let viewport_key = (viewport_key_seed, offset.1.to_bits());
                    new_viewport_hash = hash_inputs(viewport_key);

                    ui.heavy("arrangement", |hctx| {
                        hctx.cached(viewport_key, |hctx| {
                            let clip_len = clip_length_samples(total_frames);
                            for i in 0..N_WIDGETS {
                                let rect = clip_rect(area, i, m.y_zoom, offset.1, &m.viewport, total_frames);
                                // viewport / scroll 範囲外なら描画 skip
                                if rect.y + rect.h < area.y || rect.y > area.y + area.h {
                                    continue;
                                }
                                if rect.x + rect.w < area.x || rect.x > area.x + area.w {
                                    continue;
                                }
                                // 各 clip は波形の 0..clip_len を全幅表示 (DAW の clip 標準)。
                                // X zoom で clip 幅 (rect.w) が変わるため、内部波形も同じ比率で
                                // 拡大表示される。
                                let view = WaveformView {
                                    start_sample: 0,
                                    len_samples: clip_len,
                                    vertical_gain: m.vertical_gain,
                                };
                                let _ = hctx.waveform(("clip", i), rect, source, view, style);
                            }
                            let _ = view_start_unit;
                            let _ = view_len_unit;
                        });
                    });
                });

                // --- footer ---
                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "Drag = X pan │ Wheel = Y scroll │ Shift+Wheel = X zoom │ Ctrl+Wheel = Y zoom (lane height)",
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

        self.last_cache_hit = Some(new_viewport_hash) == *last_viewport_hash;
        *last_viewport_hash = Some(new_viewport_hash);
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
        // 修飾あり wheel (Ctrl = Y zoom / Shift = X zoom) のみ pending_zoom_dy に蓄積。
        // 無修飾 wheel は InputAccumulator 経由で scroll_area が Y scroll として消費。
        let is_modifier_scroll = matches!(ev, AppEvent::Scroll(_))
            && (self.cur_modifiers.ctrl || self.cur_modifiers.shift);
        if let AppEvent::Scroll(delta) = &ev
            && (self.cur_modifiers.ctrl || self.cur_modifiers.shift)
        {
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
        // Ctrl/Shift+wheel は zoom 用に独自処理 (上の pending_zoom_dy)。
        // InputAccumulator に流すと scroll_area の take_scroll_in_rect が消費して
        // anchor 維持式の new_offset から wheel 分が再度引かれて anchor がズレる。
        if !is_modifier_scroll {
            self.input.ingest(&ev);
        }
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
