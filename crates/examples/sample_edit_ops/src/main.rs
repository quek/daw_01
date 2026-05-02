//! examples/sample_edit_ops — M6 Phase 21 動作確認サンプル。
//!
//! 波形編集 (trim / linear fade in / linear fade out) の destructive edit サンプル。
//! sample_editor を流用しつつ、ボタン UI で `Edit<M>` を発行し model.samples を直接変更。
//! WaveformPyramid は generation bump で再構築。
//!
//! 操作:
//! - 無修飾 Drag: pan
//! - Shift+Drag: 選択範囲設定 (cyan 半透明 overlay)
//! - 短い Click (drag<16px): カーソル位置移動 + 選択解除
//! - Wheel: X zoom / Ctrl+Wheel: Y zoom (vertical_gain)
//! - "Trim" ボタン: selection 範囲だけ残して切り取り (drain + truncate)
//! - "Fade In" ボタン: selection 範囲に linear gain ramp 0→1 適用
//! - "Fade Out" ボタン: selection 範囲に linear gain ramp 1→0 適用
//!
//! M7 送り (本 example スコープ外):
//! - **split**: `Vec<Clip>` への構造変更が必要 (Model.samples が `Vec<Vec<f32>>` = 複数 clip)。
//! - **curve fade**: automation_curve UX (Catmull-Rom + 点ドラッグ) 流用、UI 拡張が必要。
//! - **undo / redo**: history stack の整備が必要。

use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    ChannelLayout, Edit, InputAccumulator, SampleSlices, UiHost, ViewportState1D,
    WaveformRenderMode, WaveformSource, WaveformStyle, WaveformView,
};
use daw_ui_platform::{
    AppEvent, AppHost, Modifiers, PhysicalSize, ScrollDelta, WindowBackend, winit_backend,
};
use daw_ui_renderer::{Color, Rect, RectCommand, Renderer, Scene};
use winit::window::WindowAttributes;

const SAMPLE_RATE: u32 = 48_000;
const SECONDS: f32 = 2.0;

const HEADER_H: f32 = 56.0;
const TOOLBAR_H: f32 = 48.0;
const FOOTER_H: f32 = 56.0;

// ----- Model -----

/// no-Clone 不変条件。`Clone` / `PartialEq` / `Hash` / `Default` は実装しない。
struct SampleEditOpsModel {
    samples: Vec<Vec<f32>>, // Planar 1ch、destructive edit で長さ可変
    valid_len: usize,
    generation: u64,

    /// X 軸 view (M7 Phase 22: ViewportState1D に集約)。
    viewport: ViewportState1D,
    vertical_gain: f32,

    selection: Option<(u64, u64)>, // (start, end) 順序保証
    cursor_sample: u64,

    last_action: String,
    last_frame_ms: f32,
}

impl SampleEditOpsModel {
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
            last_action: "起動 — Shift+Drag で範囲選択 → [Trim] / [Fade In] / [Fade Out]".to_string(),
            last_frame_ms: 0.0,
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
        self.viewport.zoom_at(factor, anchor_frac, 8.0);
        self.viewport.clamp_to(total);
    }

    fn x_to_sample(&self, x: f32, area: Rect) -> u64 {
        let local_x = (x - area.x).clamp(0.0, area.w);
        self.viewport.px_to_unit(local_x, area.w) as u64
    }

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
        let env = (t * std::f32::consts::PI / seconds).sin().max(0.0);
        let f1 = (t * 220.0 * std::f32::consts::TAU).sin();
        let f2 = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        let f3 = (t * 880.0 * std::f32::consts::TAU).sin() * 0.25;
        let n = (i.wrapping_mul(1664525).wrapping_add(1013904223)) as u32;
        let noise = (n as f32 / u32::MAX as f32 - 0.5) * 0.05;
        plane.push(((f1 + f2 + f3) * env + noise) * 0.85);
    }
    vec![plane]
}

fn waveform_area(screen: PhysicalSize) -> Rect {
    let pad_x = 16.0;
    let w = (screen.width as f32 - pad_x * 2.0).max(100.0);
    let h = (screen.height as f32 - HEADER_H - TOOLBAR_H - FOOTER_H).max(100.0);
    Rect { x: pad_x, y: HEADER_H, w, h }
}

fn toolbar_y(screen: PhysicalSize) -> f32 {
    HEADER_H + (screen.height as f32 - HEADER_H - TOOLBAR_H - FOOTER_H).max(100.0)
}

// ----- App -----

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<SampleEditOpsModel>,
    model: SampleEditOpsModel,
    scene: Scene,
    input: InputAccumulator,

    /// drag 状態: (anchor_x, anchor_view_start (f64 unit), anchor_sample, accum_dx, kind)
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
        window.set_title("daw-ui sample_edit_ops (M6 Phase 21)");
        Self {
            ui: UiHost::with_window(window.clone()),

            window,
            renderer,
            model: SampleEditOpsModel::new(),
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

    /// 戻り値: このフレームで `Edit<M>` が発行された場合 `true` (= 次フレームで再描画必要)。
    /// 理由: edits が出たフレームの scene は描画クロージャ後に apply されるため古い model
    /// 値で積まれている。次フレームで apply 後の値で描き直す必要がある。
    #[allow(clippy::too_many_lines)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let area = waveform_area(screen);
        let bar_y = toolbar_y(screen);

        // 1 frame に 1 回だけ take_input する。`take_input` 内部で `take_frame` が呼ばれ
        // `primary_just_pressed`/`just_released` がリセットされるため、ここで取った
        // snapshot を drag/wheel 処理にも、Ui::frame (button click 検出含む) にも使い回す。
        // (apply_pending_input で別途 take_frame すると button が click を検出できない)
        let input = self.input.take_input();
        let pointer = input.pointer;

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
                let cur_sample = self.model.x_to_sample(px, area);
                let (s, e) = if cur_sample >= anchor_sample {
                    (anchor_sample, cur_sample)
                } else {
                    (cur_sample, anchor_sample)
                };
                self.model.selection = Some((s, e));
            } else {
                self.model.viewport.view_start = ave_start;
                self.model.pan_pixels(dx, area.w);
            }
            if let Some(anchor) = self.drag_anchor.as_mut() {
                anchor.3 = dx.abs();
            }
        }

        // --- drag 終了 (短い click なら pending_click 設定) ---
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

        // --- pending click 消費 (cursor 移動) ---
        if let Some(click_px) = self.pending_click.take() {
            let s = self.model.x_to_sample(click_px, area);
            self.model.cursor_sample = s;
            self.model.selection = None;
            self.model.last_action = format!("cursor → sample {s}");
        }

        let hud = format!(
            "frame {:>5.2}ms │ samples {} │ view [{:>7}..{:>7}) │ cursor {} │ sel {} │ gain {:.2}x",
            self.model.last_frame_ms,
            self.model.valid_len,
            self.model.viewport.view_start,
            self.model.viewport.view_start + self.model.viewport.view_len,
            self.model.cursor_sample,
            self.model
                .selection
                .map_or("-".to_string(), |(s, e)| format!("[{s}..{e}) {} samples", e - s)),
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
                    "daw-ui sample_edit_ops — M6 Phase 21 (trim / linear fade in / linear fade out)",
                    16.0, 16.0, 16.0,
                    Color::rgb(0.92, 0.95, 0.98),
                );
                ui.label_at("hud", &hud, 16.0, 36.0, 12.0, Color::rgb(0.75, 0.78, 0.82));

                // --- waveform ---
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
                };
                let style = WaveformStyle {
                    fg: Color::rgb(0.55, 0.78, 0.95),
                    fg_clipped: Color::rgb(0.95, 0.45, 0.40),
                    fill: None,
                    baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.10)),
                    channel_layout: ChannelLayout::Overlay,
                    render_mode: WaveformRenderMode::Auto,
                    line_width_px: 1.0,
                };
                let _ = ui.waveform("main", area, source, view, style);

                // --- selection overlay + cursor ---
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

                // --- toolbar buttons ---
                let btn_w = 140.0;
                let btn_h = 32.0;
                let pad = 8.0;
                let btn_y = bar_y + 8.0;

                ui.button_at(
                    "trim",
                    "Trim Selection",
                    Rect::new(16.0, btn_y, btn_w, btn_h),
                    || {
                        Edit::mutate(|m: &mut SampleEditOpsModel| {
                            let Some((start, end)) = m.selection else {
                                m.last_action = "Trim: 範囲未選択".to_string();
                                return;
                            };
                            let plane = &mut m.samples[0];
                            let s = (start as usize).min(plane.len());
                            let e = (end as usize).min(plane.len());
                            if s >= e {
                                m.last_action = "Trim: 空範囲".to_string();
                                return;
                            }
                            plane.drain(..s);
                            let new_len = e - s;
                            plane.truncate(new_len);
                            m.valid_len = new_len;
                            m.generation += 1;
                            m.selection = None;
                            m.cursor_sample = 0;
                            m.viewport.view_start = 0.0;
                            m.viewport.view_len = new_len as f64;
                            m.last_action = format!("Trim: {new_len} samples 残った (generation += 1 で LOD 再構築)");
                        })
                    },
                );

                ui.button_at(
                    "fade_in",
                    "Fade In",
                    Rect::new(16.0 + (btn_w + pad), btn_y, btn_w, btn_h),
                    || {
                        Edit::mutate(|m: &mut SampleEditOpsModel| {
                            let Some((start, end)) = m.selection else {
                                m.last_action = "Fade In: 範囲未選択".to_string();
                                return;
                            };
                            let plane = &mut m.samples[0];
                            let s = (start as usize).min(plane.len());
                            let e = (end as usize).min(plane.len());
                            if s >= e {
                                m.last_action = "Fade In: 空範囲".to_string();
                                return;
                            }
                            let len = (e - s) as f32;
                            for (i, sample) in plane[s..e].iter_mut().enumerate() {
                                let t = i as f32 / len;
                                *sample *= t;
                            }
                            m.generation += 1;
                            m.last_action =
                                format!("Fade In: [{s}..{e}) に linear ramp 0→1 適用");
                        })
                    },
                );

                ui.button_at(
                    "fade_out",
                    "Fade Out",
                    Rect::new(16.0 + (btn_w + pad) * 2.0, btn_y, btn_w, btn_h),
                    || {
                        Edit::mutate(|m: &mut SampleEditOpsModel| {
                            let Some((start, end)) = m.selection else {
                                m.last_action = "Fade Out: 範囲未選択".to_string();
                                return;
                            };
                            let plane = &mut m.samples[0];
                            let s = (start as usize).min(plane.len());
                            let e = (end as usize).min(plane.len());
                            if s >= e {
                                m.last_action = "Fade Out: 空範囲".to_string();
                                return;
                            }
                            let len = (e - s) as f32;
                            for (i, sample) in plane[s..e].iter_mut().enumerate() {
                                let t = i as f32 / len;
                                *sample *= 1.0 - t;
                            }
                            m.generation += 1;
                            m.last_action =
                                format!("Fade Out: [{s}..{e}) に linear ramp 1→0 適用");
                        })
                    },
                );

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
                    "split / curve fade / undo は M7 送り (Vec<Clip> 構造変更 / history stack 必要)",
                    16.0, footer_y + 18.0, 12.0,
                    Color::rgb(0.50, 0.55, 0.62),
                );
                ui.label_at(
                    "footer3",
                    &m.last_action,
                    640.0, footer_y + 18.0, 12.0,
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
        // drag / wheel 中は連続再描画。had_edits 時も 1 frame 追加描画して
        // apply 後の model で scene を積み直す (immediate-mode + Edit queue の必然)。
        self.drag_anchor.is_some() || self.pending_zoom_dy.abs() > 0.0
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui sample_edit_ops (M6 Phase 21)")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 600.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
