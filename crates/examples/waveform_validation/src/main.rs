//! examples/waveform_validation — M2 波形 UI 早期検証サンプル (アレンジメントビュー想定)。
//!
//! 確認項目:
//! - line strip パイプラインで PeakLines が描ける (1px 縦線群)
//! - 1 分ステレオ (sample_rate 48kHz, 5.76M サンプル) で LOD が機能する
//! - **多数 (16 トラック × 8 クリップ = 128 widgets) を同時表示して 60fps**
//! - `generation` 一致時の `Ui::waveform()` 呼び出しコストが小さい (HUD でフレーム時間を観察)
//!
//! 操作:
//! - 左ドラッグ: 横スクロール (全クリップ同期)
//! - マウスホイール: ズーム (カーソル位置を中心、全クリップ同期)
//! - Space: REC シミュレーションのトグル (valid_len を実時間で伸ばし、
//!   インクリメンタル LOD 拡張をストレステスト)

use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    ChannelLayout, InputAccumulator, SampleSlices, UiHost, WaveformRenderMode,
    WaveformSource, WaveformStyle, WaveformView,
};
use daw_ui_platform::{
    AppEvent, AppHost, ElementState, Modifiers, PhysicalKey, PhysicalSize, ScrollDelta,
    WindowBackend, winit_backend,
};
use daw_ui_renderer::{Color, Rect, Renderer, Scene};
use winit::window::WindowAttributes;

const SAMPLE_RATE: u32 = 48_000;
const SECONDS: f32 = 60.0;
const CHANNELS: usize = 2;

/// アレンジメントビュー想定の grid サイズ。`TRACKS × CLIPS_PER_TRACK` 個の波形を
/// 同時表示する。`128` は典型的に重い DAW プロジェクト相当 (16 ch × 8 clips/ch)。
const TRACKS: usize = 16;
const CLIPS_PER_TRACK: usize = 8;
const N_WIDGETS: usize = TRACKS * CLIPS_PER_TRACK;

/// アプリ「GUI Model」。`Clone` は実装しない (no-Clone 不変条件)。
struct WaveformAppModel {
    /// Planar layout: `samples[ch][frame]`。
    samples: Vec<Vec<f32>>,
    /// 有効長 (frame 数)。録音中は時間で増える。
    valid_len: usize,
    /// 内容変更時にインクリメント。REC start 時に bump → ピラミッド完全再構築をトリガ。
    generation: u64,

    /// 表示開始 sample index。
    view_start: u64,
    /// 表示する frame 数。
    view_len: u64,
    /// 縦ゲイン (waveform 振幅倍率、現状 1.0 固定)。
    vertical_gain: f32,
    /// レーン高さ倍率 (Ctrl+wheel で操作)。1.0 で全 TRACKS が area に収まる、
    /// > 1.0 で 1 レーンが大きくなり下のトラックは area からはみ出す (y_offset で pan)。
    y_zoom: f32,
    /// レーン pan の y オフセット (px)。Ctrl+wheel zoom の cur_mouse anchor を維持するため
    /// 自動更新する。0 で一番上の track が area top に揃う。
    y_offset: f32,

    /// REC シミュレーション中かどうか。
    recording: bool,
    /// REC 中の現在位置 (sample index)。
    rec_pos: u64,

    /// `None` で Auto (Phase 15 挙動)。1/2/3 キーで明示上書き、a キーで `None` に戻す
    /// (Phase 16 で追加、`feedback_use_new_abstractions` 適合: sample_editor と同じ仕組み)。
    forced_mode: Option<WaveformRenderMode>,

    /// HUD 用: 最後のフレーム時間 (ms)。
    last_frame_ms: f32,
    last_action: String,
}

impl WaveformAppModel {
    fn new() -> Self {
        let samples = generate_test_samples(SECONDS, SAMPLE_RATE, CHANNELS);
        let total_frames = samples.first().map_or(0, Vec::len);
        Self {
            samples,
            valid_len: total_frames,
            generation: 0,
            view_start: 0,
            view_len: total_frames as u64,
            vertical_gain: 1.0,
            y_zoom: 1.0,
            y_offset: 0.0,
            recording: false,
            rec_pos: 0,
            forced_mode: None,
            last_frame_ms: 0.0,
            last_action: "起動 (Drag = 横スクロール / Wheel = ズーム / Space = REC)"
                .to_string(),
        }
    }

    fn total_frames(&self) -> u64 {
        self.samples.first().map_or(0, |p| p.len() as u64)
    }

    /// REC のトグル。OFF → ON のとき generation を bump して完全再構築を起こし、
    /// 以降の `valid_len` 拡大はインクリメンタル拡張で処理させる。
    fn toggle_recording(&mut self) {
        if self.recording {
            self.recording = false;
            self.last_action = format!("REC 停止 (rec_pos={})", self.rec_pos);
        } else {
            self.recording = true;
            self.rec_pos = 0;
            self.valid_len = 0;
            self.generation = self.generation.wrapping_add(1);
            self.last_action = "REC 開始 (Space で停止)".to_string();
        }
    }

    /// 経過時間 `dt_secs` 分だけ `rec_pos` / `valid_len` を進める。
    fn tick_recording(&mut self, dt_secs: f32) {
        if !self.recording {
            return;
        }
        let advance = (f64::from(SAMPLE_RATE) * f64::from(dt_secs)) as u64;
        self.rec_pos = (self.rec_pos + advance).min(self.total_frames());
        self.valid_len = self.rec_pos as usize;
        if self.rec_pos >= self.total_frames() {
            self.recording = false;
            self.last_action = "REC 完了 (audio buffer 末尾到達)".to_string();
        }
    }

    /// 表示範囲を `dx_pixels` だけずらす (drag panning)。
    fn pan_pixels(&mut self, dx: f32, widget_w: f32) {
        if widget_w <= 0.0 || self.view_len == 0 {
            return;
        }
        let spp = self.view_len as f64 / f64::from(widget_w);
        let delta_samples = (f64::from(dx) * spp) as i64;
        let total = self.total_frames();
        let max_start = total.saturating_sub(self.view_len);
        // dx > 0 (右ドラッグ) → view_start を減らして表示が右へ動く。
        self.view_start = if delta_samples >= 0 {
            self.view_start.saturating_sub(delta_samples.unsigned_abs())
        } else {
            self.view_start
                .saturating_add(delta_samples.unsigned_abs())
                .min(max_start)
        };
    }

    /// `anchor_frac` (0..1) を画面上の位置として固定したまま `factor` 倍にズームする。
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

fn generate_test_samples(seconds: f32, sample_rate: u32, channels: usize) -> Vec<Vec<f32>> {
    let frames = (seconds * sample_rate as f32) as usize;
    let mut planes: Vec<Vec<f32>> = (0..channels).map(|_| Vec::with_capacity(frames)).collect();
    for i in 0..frames {
        let t = i as f32 / sample_rate as f32;
        // 0.5Hz 周期の envelope (二乗で正値化)
        let env = ((t * 0.5 * std::f32::consts::TAU).sin() * 0.5 + 0.5).powi(2);
        for (ch, plane) in planes.iter_mut().enumerate() {
            // ch ごとに少しピッチ違いの sine + 倍音 + 軽いノイズ
            let f = 220.0 + ch as f32 * 30.0;
            let phase = (t * f * std::f32::consts::TAU).sin();
            let harm = (t * f * 2.0 * std::f32::consts::TAU).sin() * 0.3;
            // 決定論的 LCG ベースの軽量ノイズ ([-0.5, 0.5))
            let n = (i.wrapping_mul(1664525).wrapping_add(1013904223 + ch * 7919)) as u32;
            let noise = (n as f32 / u32::MAX as f32 - 0.5) * 0.1;
            plane.push((phase + harm + noise) * env * 0.85);
        }
    }
    planes
}

/// 波形 grid 全体の領域。
fn waveform_area(screen: PhysicalSize) -> Rect {
    let pad_x = 8.0;
    let header_h = 88.0;
    let footer_h = 56.0;
    let w = (screen.width as f32 - pad_x * 2.0).max(100.0);
    let h = (screen.height as f32 - header_h - footer_h).max(100.0);
    Rect { x: pad_x, y: header_h, w, h }
}

/// `i` 番目のクリップ (0..N_WIDGETS) の rect を返す。grid の (col, row) 配置。
/// `y_zoom` でレーン高さ倍率、`y_offset` で縦 pan (anchor 維持型 zoom 用、px 単位)。
fn clip_rect(area: Rect, i: usize, y_zoom: f32, y_offset: f32) -> Rect {
    let col = i % CLIPS_PER_TRACK;
    let row = i / CLIPS_PER_TRACK;
    let cell_w = area.w / CLIPS_PER_TRACK as f32;
    let cell_h = (area.h / TRACKS as f32) * y_zoom;
    Rect {
        x: area.x + col as f32 * cell_w,
        y: area.y + row as f32 * cell_h - y_offset,
        // 1px のギャップでセル境界が見えるように
        w: (cell_w - 1.0).max(1.0),
        h: (cell_h - 1.0).max(1.0),
    }
}

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<WaveformAppModel>,
    model: WaveformAppModel,
    scene: Scene,
    input: InputAccumulator,

    /// 累積中のホイール量 (on_render で適用)
    pending_zoom_dy: f32,
    /// drag 開始時の (mouse_x, view_start)
    drag_anchor: Option<(f32, u64)>,
    /// 現在のマウス位置 (zoom anchor 用に on_event で追従)
    cur_mouse: Option<(f32, f32)>,
    /// 現在の修飾キー状態 (`AppEvent::ModifiersChanged` で更新)。Ctrl+wheel = Y zoom
    /// (vertical_gain) を分岐するために App 側で track する。
    cur_modifiers: Modifiers,

    /// REC 中の前フレーム時刻 (経過 dt 計算用)
    rec_last_tick: Option<Instant>,

    last_frame_start: Option<Instant>,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        window.set_title("daw-ui waveform validation (M2)");
        Self {
            window,
            renderer,
            ui: UiHost::new(),
            model: WaveformAppModel::new(),
            scene: Scene::new(),
            input: InputAccumulator::new(),
            pending_zoom_dy: 0.0,
            drag_anchor: None,
            cur_mouse: None,
            cur_modifiers: Modifiers::default(),
            rec_last_tick: None,
            last_frame_start: None,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let input = self.input.take_input();
        // pointer 値はドラッグ判定で使うので残すが、他は input にまとめる。
        let pointer = input.pointer;

        // 1. drag panning (grid 全域がドラッグ対象)
        let area = waveform_area(screen);
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && area.contains(px, py)
        {
            self.drag_anchor = Some((px, self.model.view_start));
        }
        if pointer.primary_just_released {
            self.drag_anchor = None;
        }
        if let (Some((anchor_x, anchor_view_start)), Some((px, _))) =
            (self.drag_anchor, pointer.pos)
        {
            // 各クリップは width = area.w / CLIPS_PER_TRACK の中で view_len を表示する。
            // ドラッグの感覚を 1 クリップ幅基準にする (細かい操作も効くように)。
            self.model.view_start = anchor_view_start;
            let cell_w = area.w / CLIPS_PER_TRACK as f32;
            self.model.pan_pixels(px - anchor_x, cell_w);
        }

        // 2. ズーム適用 (Ctrl で X/Y 切替、cur_mouse を anchor に)
        if self.pending_zoom_dy.abs() > 0.0 {
            let factor = (-self.pending_zoom_dy * 0.15).exp();
            if self.cur_modifiers.ctrl {
                // Y zoom: レーン高さ倍率 (y_zoom) を変更。cur_mouse.y の位置にある track が
                // 同じスクリーン y に残るよう y_offset を anchor 維持型で更新する
                // (= focus track が画面で動かない感覚)。
                // Y は「ホイール上でレーン拡大」に揃えるため factor を逆数化 (X は view_len
                // 縮小 = 拡大、Y は cell_h 拡大 = 拡大、で意味が逆になるため)。
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
                // X zoom: view_len_samples を変更、anchor はクリップ内 x の比率。
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

        // 3. UI 構築
        let edits = self.ui.frame(
            &self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // タイトル + view 情報
                ui.label_at(
                    "title",
                    "daw-ui waveform validation — M2 (16 tracks × 8 clips = 128 widgets)",
                    16.0,
                    16.0,
                    18.0,
                    Color::rgb(0.95, 0.95, 0.97),
                );
                let rec_tag = if m.recording {
                    format!(
                        "● REC {:.2}s",
                        m.rec_pos as f32 / SAMPLE_RATE as f32,
                    )
                } else {
                    "PAUSED".to_string()
                };
                let info = format!(
                    "frame {:>5.2} ms │ start {:>7} │ len {:>7} │ spp {:>6.1} │ {} widgets │ valid {:.2}s / {:.2}s │ {}",
                    m.last_frame_ms,
                    m.view_start,
                    m.view_len,
                    m.view_len as f64 / f64::from(area.w),
                    N_WIDGETS,
                    m.valid_len as f32 / SAMPLE_RATE as f32,
                    m.total_frames() as f32 / SAMPLE_RATE as f32,
                    rec_tag,
                );
                let info_color = if m.recording {
                    Color::rgb(0.95, 0.50, 0.45)
                } else {
                    Color::rgb(0.75, 0.78, 0.82)
                };
                ui.label_at("info", &info, 16.0, 44.0, 13.0, info_color);

                // 波形 grid: 各クリップは「同じ source の少しずらした view」を表示。
                // pyramid キャッシュは widget_id 単位で個別 → 128 個のキャッシュが state に乗る。
                let planes: Vec<&[f32]> = m.samples.iter().map(Vec::as_slice).collect();
                let source = WaveformSource {
                    samples: SampleSlices::Planar(&planes),
                    valid_len: m.valid_len,
                    generation: m.generation,
                    sample_rate: SAMPLE_RATE,
                };
                let style = WaveformStyle {
                    fg: Color::rgb(0.55, 0.78, 0.95),
                    fg_clipped: Color::rgb(0.95, 0.45, 0.40),
                    fill: None,
                    baseline: Some(Color::rgba(1.0, 1.0, 1.0, 0.08)),
                    channel_layout: ChannelLayout::Overlay,
                    // M5 Phase 15-16: forced_mode で 1/2/3 キーから明示上書き、None なら Auto。
                    render_mode: m.forced_mode.unwrap_or(WaveformRenderMode::Auto),
                    line_width_px: 1.0,
                };
                // 各クリップが少し違うサンプル位置を表示するシフト量。view 全体に対して
                // 1/CLIPS_PER_TRACK の幅をずらすと、横方向にスライドするような視覚効果。
                let shift_per_clip = m.view_len / CLIPS_PER_TRACK as u64;
                let max_start = m.total_frames().saturating_sub(m.view_len);
                for i in 0..N_WIDGETS {
                    let rect = clip_rect(area, i, m.y_zoom, m.y_offset);
                    let view = WaveformView {
                        start_sample: m
                            .view_start
                            .saturating_add(shift_per_clip * (i as u64))
                            .min(max_start),
                        len_samples: m.view_len,
                        vertical_gain: m.vertical_gain,
                    };
                    let _resp = ui.waveform(("clip", i), rect, source, view, style);
                }

                // フッタ: 操作説明 + 状態
                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "Drag = 横スクロール │ Wheel = X zoom │ Ctrl+Wheel = Y zoom │ Space = REC │ [1] PeakLines [2] SamplePolyline [3] RmsBars [a] Auto",
                    16.0,
                    footer_y,
                    13.0,
                    Color::rgb(0.65, 0.68, 0.72),
                );
                ui.label_at(
                    "footer2",
                    &m.last_action,
                    16.0,
                    footer_y + 18.0,
                    13.0,
                    Color::rgb(0.50, 0.55, 0.62),
                );
            },
        );

        for e in edits {
            e.apply(&mut self.model);
        }
    }
}

impl AppHost for App {
    fn on_event(&mut self, ev: AppEvent) {
        // マウス位置の追従 (zoom anchor 用)
        if let AppEvent::PointerMoved(p) = &ev {
            self.cur_mouse = Some((p.x as f32, p.y as f32));
        }
        if let AppEvent::PointerLeft = &ev {
            self.cur_mouse = None;
        }
        // ホイールは累積して on_render で適用
        if let AppEvent::Scroll(delta) = &ev {
            let dy = match delta {
                ScrollDelta::Lines { y, .. } => *y,
                ScrollDelta::Pixels { y, .. } => *y as f32 / 30.0,
            };
            self.pending_zoom_dy += dy;
        }
        // 修飾キー追従 (Ctrl+wheel = Y zoom 分岐用)
        if let AppEvent::ModifiersChanged(m) = &ev {
            self.cur_modifiers = *m;
        }

        // Space で REC トグル + 1/2/3/a で render_mode 上書き (Phase 16)。
        // 数字 / a は `key.text` 経由で判定 (`PhysicalKey::Other` は winit の KeyCode
        // discriminant で OS 非依存だが値が予測困難)。
        if let AppEvent::Keyboard(key) = &ev
            && key.state == ElementState::Pressed
        {
            if key.physical_key == PhysicalKey::Space {
                self.model.toggle_recording();
                self.rec_last_tick = None;
            } else {
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
            | AppEvent::Keyboard(_) => {
                self.window.request_redraw();
            }
            _ => {}
        }
    }

    fn on_render(&mut self) -> bool {
        let now = Instant::now();
        // REC 中: 経過 dt で valid_len を伸ばす (インクリメンタル LOD 拡張をテスト)
        if self.model.recording {
            if let Some(prev) = self.rec_last_tick {
                let dt = now.duration_since(prev).as_secs_f32();
                self.model.tick_recording(dt);
            }
            self.rec_last_tick = Some(now);
        } else {
            self.rec_last_tick = None;
        }

        self.last_frame_start = Some(now);
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        if let Some(t) = self.last_frame_start.take() {
            self.model.last_frame_ms = t.elapsed().as_secs_f32() * 1000.0;
        }
        // drag / REC 中は連続再描画
        self.drag_anchor.is_some() || self.model.recording
    }
}

fn main() {
    let attrs = WindowAttributes::default()
        .with_title("daw-ui waveform validation (M2)")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 600.0));
    if let Err(e) = winit_backend::run_app(attrs, |window| App::new(Arc::new(window))) {
        eprintln!("event loop error: {e}");
    }
}
