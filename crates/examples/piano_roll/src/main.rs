//! examples/piano_roll — M5 Phase 14 動作確認サンプル。
//!
//! Phase 13 で導入した `Ui::heavy` + `HeavyCtx::cached(viewport_key, ...)` を
//! 100,000 ノートのピアノロールで実用検証する (heavy() 実用第 1 弾)。
//!
//! 確認項目:
//! - 100k notes の visible filtering (二分探索) が走り、画面に矩形が出る
//! - cached(viewport_key) によって viewport 停止フレームでは draw_fn がスキップされ、
//!   描画コマンドが前フレームから再利用される (HUD `cache HIT` で目視)
//! - drag (XY 同時 pan) / wheel zoom / click hit-test が機能する
//! - 停止フレームで HUD `frame_ms` が 8ms 以下 (= 120fps 予算)
//!
//! 操作:
//! - 左ドラッグ: 主領域 (鍵盤右側) で XY 同時 pan
//! - マウスホイール: cur_mouse 位置を anchor に zoom
//! - 短い click (drag 累積 < 16px): note 選択 → 選択 overlay 表示

use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    Edit, InputAccumulator, UiHost, hash_inputs,
};
use daw_ui_platform::{
    AppEvent, AppHost, PhysicalSize, ScrollDelta, WindowBackend, winit_backend,
};
use daw_ui_renderer::{Color, Rect, RectCommand, Renderer, Scene};
use winit::window::WindowAttributes;

// ----- レイアウト定数 -----

const KEYBOARD_W: f32 = 60.0;
const HEADER_H: f32 = 56.0;
const FOOTER_H: f32 = 56.0;

// 1 拍の表示幅 (px) と pitch row 高さは area.w / view_len_beats、area.h / pitch_visible で動的計算する。

// ----- Note データ -----

/// 内部値型 (Model 型ではないので Copy/Clone OK)。
#[derive(Clone, Copy, Debug)]
struct Note {
    start_beat: f32,
    len_beats: f32,
    pitch: u8,    // MIDI 0..127 (生成側で 36..96 に絞る)
    velocity: u8, // 0..127 (色濃度に使う)
}

/// 100k 個を決定論的 LCG で生成。`start_beat` 昇順にソート。
fn generate_notes(count: usize) -> Vec<Note> {
    // 線形合同法 (LCG) + splitmix64 finalizer。LCG 単体の下位 bit は周期が短く
    // (`% 60` のような小さな modulo で「4 半音間隔」のような周期パターンが出る)、
    // splitmix64 finalizer で全 bit を mix してから modulo を取ることで均一分散を得る。
    let mut state: u64 = 0x12345678_9ABCDEF0;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };

    let total_beats: f32 = 1024.0;        // 64 小節 (4/4)
    let pitch_lo: u8 = 36;                // C2
    let pitch_hi: u8 = 96;                // C7
    let mut notes: Vec<Note> = Vec::with_capacity(count);
    for _ in 0..count {
        let r1 = next();
        let r2 = next();
        let r3 = next();
        let r4 = next();
        let start_beat = (r1 as f32 / u64::MAX as f32) * total_beats;
        // 長さは 0.125〜2.0 拍 (8 分音符〜2 分音符)
        let len_beats = 0.125 + (r2 as f32 / u64::MAX as f32) * 1.875;
        let pitch = pitch_lo + ((r3 % u64::from(pitch_hi - pitch_lo)) as u8);
        let velocity = 32 + ((r4 % 96) as u8);
        notes.push(Note { start_beat, len_beats, pitch, velocity });
    }
    notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal));
    notes
}

// ----- Model -----

/// no-Clone 不変条件: `Clone` / `PartialEq` / `Hash` / `Default` は実装しない。
struct PianoRollModel {
    notes: Vec<Note>,
    /// 将来の編集 API 追加時に bump する hook。現状 0 固定。
    notes_generation: u64,

    view_start_beat: f32,
    view_len_beats: f32,   // zoom 倍率の逆数

    pitch_top: f32,        // 表示 top の MIDI ピッチ (浮動小数で smooth scroll)
    pitch_visible: f32,    // 表示する pitch 範囲 (例 36 = 3 オクターブ)

    selected_note_index: Option<usize>,

    last_frame_ms: f32,
    last_action: String,
}

impl PianoRollModel {
    fn new(notes: Vec<Note>) -> Self {
        Self {
            notes,
            notes_generation: 0,
            view_start_beat: 0.0,
            view_len_beats: 4.0,     // 1 小節 = 個々の note 矩形が判別可能なズーム
            pitch_top: 72.0,         // C5
            pitch_visible: 24.0,     // 2 オクターブ (1 row = 高めで note が見える)
            selected_note_index: None,
            last_frame_ms: 0.0,
            last_action: "起動 (Drag = pan / Wheel = zoom / Click = select)".to_string(),
        }
    }
}

// ----- レイアウト helper -----

fn grid_rect(screen: PhysicalSize) -> Rect {
    Rect {
        x: KEYBOARD_W,
        y: HEADER_H,
        w: (screen.width as f32 - KEYBOARD_W).max(1.0),
        h: (screen.height as f32 - HEADER_H - FOOTER_H).max(1.0),
    }
}

fn keyboard_rect(screen: PhysicalSize) -> Rect {
    Rect {
        x: 0.0,
        y: HEADER_H,
        w: KEYBOARD_W,
        h: (screen.height as f32 - HEADER_H - FOOTER_H).max(1.0),
    }
}

/// 1 つの note の screen 座標 rect を返す。grid 外に出る部分はクリップ済み座標。
#[allow(clippy::many_single_char_names)]
fn note_to_rect(note: Note, m: &PianoRollModel, grid: Rect) -> Rect {
    let beat_to_px = grid.w / m.view_len_beats;
    let pitch_to_px = grid.h / m.pitch_visible;
    let x = grid.x + (note.start_beat - m.view_start_beat) * beat_to_px;
    let w = (note.len_beats * beat_to_px).max(1.5);
    // pitch_top が画面 top (= grid.y) に対応、pitch が下がると y が増える。
    let y = grid.y + (m.pitch_top - f32::from(note.pitch)) * pitch_to_px;
    let h = (pitch_to_px - 1.0).max(2.0);
    Rect { x, y, w, h }
}

fn pitch_color(velocity: u8) -> Color {
    // velocity → 青系の濃淡 (0.5..0.95)
    let t = f32::from(velocity) / 127.0;
    Color::rgba(0.35 + t * 0.35, 0.55 + t * 0.30, 0.85 + t * 0.10, 1.0)
}

fn note_rect_command(rect: Rect, fill: Color) -> RectCommand {
    RectCommand {
        rect,
        fill,
        border: Color::TRANSPARENT,
        border_width: 0.0,
        radius: [1.5; 4],
        clip_rect: None,
    }
}

// 黒鍵判定 (C# / D# / F# / G# / A#)
fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

// ----- App -----

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<PianoRollModel>,
    model: PianoRollModel,
    scene: Scene,
    input: InputAccumulator,

    /// drag 状態: (anchor_mouse_x, anchor_mouse_y, anchor_view_start_beat, anchor_pitch_top, accum_dx_abs, accum_dy_abs)
    drag_anchor: Option<(f32, f32, f32, f32, f32, f32)>,
    /// マウス位置 (zoom anchor / hit-test 用)
    cur_mouse: Option<(f32, f32)>,
    /// wheel 累積 (on_render で適用)
    pending_zoom_dy: f32,
    /// drag 短距離 release で立つ click 位置 (on_render の build_ui 内で消費)
    pending_click: Option<(f32, f32)>,

    /// HUD HIT/MISS 推定用に前フレームの viewport_hash を保持
    last_viewport_hash: Option<u64>,
    /// HUD 表示用に前フレームの visible 件数を保持
    last_visible_count: u32,

    last_frame_start: Option<Instant>,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>, n_notes: usize) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        window.set_title("daw-ui piano_roll (M5 Phase 14)");
        let notes = generate_notes(n_notes);
        Self {
            ui: UiHost::with_window(window.clone()),

            window,
            renderer,
            model: PianoRollModel::new(notes),
            scene: Scene::new(),
            input: InputAccumulator::new(),
            drag_anchor: None,
            cur_mouse: None,
            pending_zoom_dy: 0.0,
            pending_click: None,
            last_viewport_hash: None,
            last_visible_count: 0,
            last_frame_start: None,
        }
    }

    /// drag/wheel/click を on_render の頭で Model に反映。
    fn apply_pending_input(&mut self, screen: PhysicalSize) {
        let grid = grid_rect(screen);
        let pointer = self.input.take_frame(); // pointer のみ peek、keyboard/ime は build_ui で取り直す
        // 戻すために、pointer を再構成する代わりに、pointer を使った drag/click 判定をここで終わらせる。
        // (build_ui 中の `ui.frame` には改めて take_input() を渡す)

        // --- drag scroll ---
        if pointer.primary_just_pressed
            && let Some((px, py)) = pointer.pos
            && grid.contains(px, py)
        {
            self.drag_anchor = Some((
                px, py,
                self.model.view_start_beat,
                self.model.pitch_top,
                0.0, 0.0,
            ));
            self.pending_click = None;
        }

        if let (Some((ax, ay, ave_start, ave_pitch_top, _accum_dx, _accum_dy)), Some((px, py))) =
            (self.drag_anchor, pointer.pos)
        {
            let dx = px - ax;
            let dy = py - ay;
            // 表示単位への換算: dx px → beat、dy px → pitch
            let beat_per_px = self.model.view_len_beats / grid.w;
            let pitch_per_px = self.model.pitch_visible / grid.h;
            // pan: マウスを右へ動かしたら view_start_beat を減らして表示が右へ動く
            let new_start = (ave_start - dx * beat_per_px).max(0.0);
            // pan: マウスを下へ動かしたら pitch_top を上げて表示が下へ動く
            let new_pitch_top = (ave_pitch_top + dy * pitch_per_px)
                .min(127.0)
                .max(self.model.pitch_visible - 1.0);
            self.model.view_start_beat = new_start;
            self.model.pitch_top = new_pitch_top;
            // accum_dx/dy 更新 (release 時の click 判定用)
            if let Some(anchor) = self.drag_anchor.as_mut() {
                anchor.4 = dx.abs();
                anchor.5 = dy.abs();
            }
        }

        if pointer.primary_just_released {
            if let Some((_, _, _, _, accum_dx, accum_dy)) = self.drag_anchor.take() {
                if accum_dx + accum_dy < 16.0
                    && let Some(pos) = pointer.pos
                {
                    self.pending_click = Some(pos);
                }
            } else if let Some(pos) = pointer.pos {
                // 押下が grid 外で始まった (drag_anchor なし) ケースでも click を流す
                self.pending_click = Some(pos);
            }
        }

        // --- wheel zoom (無修飾 = X zoom、Ctrl+wheel = Y zoom) ---
        if self.pending_zoom_dy.abs() > 0.0 {
            let factor = (-self.pending_zoom_dy * 0.15).exp();
            if pointer.modifiers.ctrl {
                // Y zoom: pitch_visible を変更、anchor は cur_mouse.y の grid 内比率
                let anchor_frac = if let Some((_, my)) = self.cur_mouse {
                    ((my - grid.y) / grid.h).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                let new_visible = (self.model.pitch_visible * factor).clamp(2.0, 128.0);
                let anchor_pitch =
                    self.model.pitch_top - anchor_frac * self.model.pitch_visible;
                let new_top = anchor_pitch + anchor_frac * new_visible;
                self.model.pitch_top = new_top.clamp(new_visible - 1.0, 127.0);
                self.model.pitch_visible = new_visible;
            } else {
                // X zoom: view_len_beats を変更、anchor は cur_mouse.x の grid 内比率
                let anchor_frac = if let Some((mx, _)) = self.cur_mouse {
                    ((mx - grid.x) / grid.w).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                let new_len = (self.model.view_len_beats * factor).clamp(0.25, 256.0);
                let anchor_beat =
                    self.model.view_start_beat + anchor_frac * self.model.view_len_beats;
                let new_start = (anchor_beat - anchor_frac * new_len).max(0.0);
                self.model.view_start_beat = new_start;
                self.model.view_len_beats = new_len;
            }
            self.pending_zoom_dy = 0.0;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_ui(&mut self) {
        self.scene.clear();
        let screen = self.renderer.size();
        let grid = grid_rect(screen);
        let kbd = keyboard_rect(screen);
        let input = self.input.take_input();

        // viewport_key を build_ui の冒頭で計算 (HUD 用 hash + cached() 用 key 両方で同じ tuple を使う)
        let viewport_key = (
            b"piano_roll_v1" as &[u8],
            self.model.view_start_beat.to_bits(),
            self.model.view_len_beats.to_bits(),
            self.model.pitch_top.to_bits(),
            self.model.pitch_visible.to_bits(),
            grid.w.to_bits(),
            grid.h.to_bits(),
            self.model.notes_generation,
        );
        let viewport_hash = hash_inputs(viewport_key);
        // HUD HIT/MISS 推定: 前フレームと同じ hash → cached が hit したはず (approximation)
        let cache_status = if Some(viewport_hash) == self.last_viewport_hash {
            "HIT "
        } else {
            "MISS"
        };
        self.last_viewport_hash = Some(viewport_hash);

        // visible 件数を build_ui の前に算出 (HUD 表示用、heavy 内/外で再利用)
        let view_end_beat = self.model.view_start_beat + self.model.view_len_beats;
        let view_start_beat = self.model.view_start_beat;
        let start_idx = self.model.notes.partition_point(|n| n.start_beat + n.len_beats < view_start_beat);
        let end_idx = start_idx
            + self.model.notes[start_idx..].partition_point(|n| n.start_beat <= view_end_beat);
        self.last_visible_count = (end_idx - start_idx) as u32;

        let hud_text = format!(
            "frame {:>5.2}ms │ visible {:>5} / {} notes │ cache {} │ view [{:.2}..{:.2}) beats │ pitch top={:.1}",
            self.model.last_frame_ms,
            self.last_visible_count,
            self.model.notes.len(),
            cache_status,
            self.model.view_start_beat,
            view_end_beat,
            self.model.pitch_top,
        );

        // pending_click を消費するため take する
        let click_pos = self.pending_click.take();
        let selected_idx = self.model.selected_note_index;

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // M8 Phase 30: shortcut undo/redo (note 編集自体は M10 で本実装、ここは
                // shortcut layer が動くことを示す demo のみ)。
                if ui.take_shortcut("undo") {
                    ui.request_undo();
                }
                if ui.take_shortcut("redo") {
                    ui.request_redo();
                }

                // === HUD (heavy() の外、毎フレーム値が変わるため) ===
                ui.label_at(
                    "title",
                    "daw-ui piano_roll — heavy() 実用第 1 弾 (M5 Phase 14)",
                    16.0, 16.0, 16.0,
                    Color::rgb(0.92, 0.95, 0.98),
                );
                ui.label_at(
                    "hud",
                    &hud_text,
                    16.0, 36.0, 12.0,
                    if cache_status == "HIT " {
                        Color::rgb(0.55, 0.85, 0.65)
                    } else {
                        Color::rgb(0.95, 0.78, 0.55)
                    },
                );

                // === heavy() ブロック: ピアノロール本体 ===
                ui.heavy("piano_roll", |hctx| {
                    // visible 範囲は build_ui で算出済の (start_idx, end_idx) を再計算する形で
                    // closure に閉じ込める (slice の borrow を heavy 内に局所化)。
                    let s_idx = m.notes.partition_point(|n| n.start_beat + n.len_beats < view_start_beat);
                    let e_idx = s_idx
                        + m.notes[s_idx..].partition_point(|n| n.start_beat <= view_end_beat);
                    let visible: &[Note] = &m.notes[s_idx..e_idx];

                    // --- cached(): viewport_key 一致時に skip される背景レイヤ ---
                    hctx.cached(viewport_key, |hctx| {
                        // (a) 主領域背景
                        hctx.push_rect(RectCommand {
                            rect: grid,
                            fill: Color::rgb(0.12, 0.13, 0.16),
                            border: Color::TRANSPARENT,
                            border_width: 0.0,
                            radius: [0.0; 4],
                            clip_rect: None,
                        });
                        // (b) 黒鍵 row 帯 (薄い網)
                        let pitch_to_px = grid.h / m.pitch_visible;
                        let pitch_top_int = m.pitch_top.floor() as i32;
                        let pitch_visible_int = m.pitch_visible.ceil() as i32;
                        for i in 0..=pitch_visible_int {
                            let pitch = pitch_top_int - i;
                            if !(0..=127).contains(&pitch) {
                                continue;
                            }
                            if is_black_key(pitch as u8) {
                                let y = grid.y + (m.pitch_top - pitch as f32) * pitch_to_px;
                                hctx.push_rect(RectCommand {
                                    rect: Rect { x: grid.x, y, w: grid.w, h: pitch_to_px },
                                    fill: Color::rgba(1.0, 1.0, 1.0, 0.04),
                                    border: Color::TRANSPARENT,
                                    border_width: 0.0,
                                    radius: [0.0; 4],
                                    clip_rect: None,
                                });
                            }
                        }
                        // (c) 拍縦線 (1 拍ごと細線、4 拍ごと太線 = 小節)
                        let beat_to_px = grid.w / m.view_len_beats;
                        let first_beat = view_start_beat.floor() as i32;
                        let last_beat = view_end_beat.ceil() as i32;
                        for b in first_beat..=last_beat {
                            let x = grid.x + (b as f32 - view_start_beat) * beat_to_px;
                            if x < grid.x - 1.0 || x > grid.x + grid.w + 1.0 {
                                continue;
                            }
                            let is_bar = b.rem_euclid(4) == 0;
                            let (line_w, alpha) = if is_bar { (1.5_f32, 0.30_f32) } else { (1.0_f32, 0.12_f32) };
                            hctx.push_rect(RectCommand {
                                rect: Rect { x: x - line_w * 0.5, y: grid.y, w: line_w, h: grid.h },
                                fill: Color::rgba(1.0, 1.0, 1.0, alpha),
                                border: Color::TRANSPARENT,
                                border_width: 0.0,
                                radius: [0.0; 4],
                                clip_rect: None,
                            });
                        }
                        // (d) 鍵盤左 widget (画面左 60px 固定)
                        // 背景
                        hctx.push_rect(RectCommand {
                            rect: kbd,
                            fill: Color::rgb(0.22, 0.23, 0.26),
                            border: Color::TRANSPARENT,
                            border_width: 0.0,
                            radius: [0.0; 4],
                            clip_rect: None,
                        });
                        // 各 pitch を rect で描画
                        for i in 0..=pitch_visible_int {
                            let pitch = pitch_top_int - i;
                            if !(0..=127).contains(&pitch) {
                                continue;
                            }
                            let y = grid.y + (m.pitch_top - pitch as f32) * pitch_to_px;
                            let key_rect = Rect { x: kbd.x, y, w: kbd.w - 1.0, h: pitch_to_px - 1.0 };
                            let fill = if is_black_key(pitch as u8) {
                                Color::rgb(0.10, 0.11, 0.13)
                            } else {
                                Color::rgb(0.92, 0.93, 0.95)
                            };
                            hctx.push_rect(RectCommand {
                                rect: key_rect,
                                fill,
                                border: Color::TRANSPARENT,
                                border_width: 0.0,
                                radius: [0.0; 4],
                                clip_rect: None,
                            });
                            // C のオクターブのみラベル (C2..C7 = 24,36,...,84,96)
                            if (pitch as u8).is_multiple_of(12) && pitch_to_px >= 8.0 {
                                let octave = (pitch / 12) - 1;
                                hctx.label_at(
                                    ("c_label", pitch),
                                    &format!("C{octave}"),
                                    kbd.x + 4.0,
                                    y,
                                    11.0,
                                    Color::rgb(0.30, 0.30, 0.35),
                                );
                            }
                        }
                        // (e) notes 矩形 (visible のみ)。
                        // grid_rect で X/Y 両軸を厳密 clip する (renderer 側に scissor がないため、
                        // CPU 側で rect を切り詰めて push する)。
                        for note in visible {
                            let r = note_to_rect(*note, m, grid);
                            let x_left = r.x.max(grid.x);
                            let x_right = (r.x + r.w).min(grid.x + grid.w);
                            let y_top = r.y.max(grid.y);
                            let y_bot = (r.y + r.h).min(grid.y + grid.h);
                            if x_right <= x_left || y_bot <= y_top {
                                continue;
                            }
                            let clipped = Rect {
                                x: x_left,
                                y: y_top,
                                w: x_right - x_left,
                                h: y_bot - y_top,
                            };
                            hctx.push_rect(note_rect_command(clipped, pitch_color(note.velocity)));
                        }
                    });

                    // --- cached の外: 選択 overlay (毎フレーム実行) ---
                    if let Some(idx) = selected_idx
                        && idx >= s_idx
                        && idx < e_idx
                    {
                        let note = m.notes[idx];
                        let r = note_to_rect(note, m, grid);
                        // note より 2px 外側に張り出した黄色 fill + 白縁取りで強調
                        let pad = 2.0;
                        hctx.push_rect(RectCommand {
                            rect: Rect {
                                x: r.x - pad,
                                y: r.y - pad,
                                w: r.w + pad * 2.0,
                                h: r.h + pad * 2.0,
                            },
                            fill: Color::rgb(1.0, 0.85, 0.30),
                            border: Color::rgb(1.0, 1.0, 1.0),
                            border_width: 2.0,
                            radius: [3.0; 4],
                            clip_rect: None,
                        });
                    }

                    // --- cached の外: ヒットテスト (click 時のみ) ---
                    if let Some((cx, cy)) = click_pos
                        && grid.contains(cx, cy)
                    {
                        let mut hit: Option<usize> = None;
                        for (off, note) in visible.iter().enumerate() {
                            let r = note_to_rect(*note, m, grid);
                            if r.contains(cx, cy) {
                                hit = Some(s_idx + off);
                                // 後勝ち (描画順で前面のものが選ばれる)
                            }
                        }
                        if let Some(idx) = hit {
                            hctx.push_edit(Edit::mutate(move |m: &mut PianoRollModel| {
                                m.selected_note_index = Some(idx);
                                m.last_action = format!("note {idx} 選択");
                            }));
                        } else {
                            hctx.push_edit(Edit::mutate(|m: &mut PianoRollModel| {
                                m.selected_note_index = None;
                                m.last_action = "選択解除".to_string();
                            }));
                        }
                    }
                });

                // === Footer (heavy の外) ===
                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "Drag = pan / Wheel = X zoom / Ctrl+Wheel = Y zoom / Click (drag<16px) = note 選択",
                    16.0, footer_y, 12.0,
                    Color::rgb(0.65, 0.68, 0.72),
                );
                ui.label_at(
                    "footer2",
                    &m.last_action,
                    16.0, footer_y + 18.0, 12.0,
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
        let screen = self.renderer.size();
        // pointer 由来の drag/click/wheel を Model に反映 (build_ui の前)
        self.apply_pending_input(screen);
        self.last_frame_start = Some(now);
        self.build_ui();
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        if let Some(t) = self.last_frame_start.take() {
            self.model.last_frame_ms = t.elapsed().as_secs_f32() * 1000.0;
        }
        // edits / drag / wheel が出ていれば連続描画
        self.drag_anchor.is_some() || self.pending_zoom_dy.abs() > 0.0
    }
}

fn main() {
    // 段階的検証用: 環境変数 PIANO_ROLL_NOTES で切り替え可能。デフォルトは 100k。
    let n_notes: usize = std::env::var("PIANO_ROLL_NOTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let attrs = WindowAttributes::default()
        .with_title("daw-ui piano_roll (M5 Phase 14)")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
    if let Err(e) = winit_backend::run_app(attrs, move |window| App::new(Arc::new(window), n_notes)) {
        eprintln!("event loop error: {e}");
    }
}
