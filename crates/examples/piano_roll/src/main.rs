//! examples/piano_roll — M9 Phase 41e: library widget `Ui::piano_roll` の実用例。
//!
//! 100k notes を `crates/ui/src/widgets/piano_roll.rs` の library widget で描画する。
//! example は HUD / view state pan + wheel zoom / window 起動 / Edit factory dispatch のみ
//! を担い、描画 + drag state machine + hit-test + Alt+drag + shortcut は widget に閉じ込める。
//!
//! 操作:
//! - 無修飾 drag = pan (空白 or note なし上で press → drag)
//! - 無修飾 wheel = X zoom (cur_mouse 位置 anchor)
//! - Ctrl+wheel = Y zoom (pitch 範囲を変える)
//! - note 中央 drag = move (release で Undoable)
//! - note 左右端 drag = resize (release で Undoable)
//! - note click (drag<16px) = selection 1 個
//! - 空白 click = selection clear
//! - Alt+drag = rect multi-select
//! - Insert = pointer 位置に新規 note 追加 (next_note_id を bump)
//! - Delete = selected を一括削除
//! - Ctrl+Z / Ctrl+Shift+Z = undo / redo (M8 history stack)

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    Edit, InputAccumulator, MoveDelta, Note, NoteId, NotesEditRequest, PianoRollResponse,
    PianoRollStyle, PianoRollView, ResizeDelta, UiHost, hash_inputs,
};
use daw_ui_platform::{
    AppEvent, AppHost, PhysicalSize, ScrollDelta, WindowBackend, winit_backend,
};
use daw_ui_renderer::{Color, Rect, Renderer, Scene};
use winit::window::WindowAttributes;

const KEYBOARD_W: f32 = 60.0;
const HEADER_H: f32 = 56.0;
const FOOTER_H: f32 = 56.0;

// ----- Model -----

/// no-Clone 不変条件: `Clone` / `PartialEq` / `Hash` / `Default` は実装しない。
struct PianoRollModel {
    notes: Vec<Note>,
    notes_generation: u64,
    next_note_id: NoteId,
    selected_note_ids: Vec<NoteId>,

    view_start_beat: f32,
    view_len_beats: f32,
    pitch_top: f32,
    pitch_visible: f32,

    last_frame_ms: f32,
    last_action: String,
}

impl PianoRollModel {
    fn new(notes: Vec<Note>) -> Self {
        let next_note_id = notes.iter().map(|n| n.id).max().map_or(0, |m| m + 1);
        Self {
            notes,
            notes_generation: 0,
            next_note_id,
            selected_note_ids: Vec::new(),
            view_start_beat: 0.0,
            view_len_beats: 4.0,
            pitch_top: 72.0,
            pitch_visible: 24.0,
            last_frame_ms: 0.0,
            last_action: "起動 (Drag = pan / Wheel = zoom / Click = select / Insert / Delete)"
                .to_string(),
        }
    }

    /// PianoRollView (library widget に渡す値型) に変換。
    fn view(&self) -> PianoRollView {
        PianoRollView {
            start_beat: self.view_start_beat,
            len_beats: self.view_len_beats,
            pitch_top: self.pitch_top,
            pitch_visible: self.pitch_visible,
            keyboard_w: KEYBOARD_W,
            notes_generation: self.notes_generation,
        }
    }
}

/// 100k 個を決定論的 LCG で生成。`start_beat` 昇順にソート。
fn generate_notes(count: usize) -> Vec<Note> {
    let mut state: u64 = 0x12345678_9ABCDEF0;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };

    let total_beats: f32 = 1024.0;
    let pitch_lo: u8 = 36;
    let pitch_hi: u8 = 96;
    let mut notes: Vec<Note> = Vec::with_capacity(count);
    for i in 0..count {
        let r1 = next();
        let r2 = next();
        let r3 = next();
        let r4 = next();
        let start_beat = (r1 as f32 / u64::MAX as f32) * total_beats;
        let len_beats = 0.125 + (r2 as f32 / u64::MAX as f32) * 1.875;
        let pitch = pitch_lo + ((r3 % u64::from(pitch_hi - pitch_lo)) as u8);
        let velocity = 32 + ((r4 % 96) as u8);
        notes.push(Note { id: i as NoteId, start_beat, len_beats, pitch, velocity });
    }
    notes.sort_by(|a, b| a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal));
    notes
}

// ----- Edit factory (M9 Phase 41a-d で snapshot_inverse 化済) -----

/// 1 個 or 複数の note を一括 add する Undoable Edit。
/// forward 内で `next_note_id` を `id+1` に bump (= Insert で id 重複しないよう自動増加)。
/// inverse では bump しない (id は再利用しないので、undo 後に Insert すると新しい id が振られる)。
fn make_add_notes_edit(notes: Vec<Note>) -> Edit<PianoRollModel> {
    let label = if notes.len() == 1 { "add note" } else { "add notes" };
    Edit::snapshot_inverse(
        label,
        notes,
        |m: &mut PianoRollModel, snap: &Vec<Note>| {
            for note in snap {
                m.notes.push(*note);
                m.next_note_id = m.next_note_id.max(note.id + 1);
            }
            m.notes.sort_by(|a, b| {
                a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal)
            });
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &Vec<Note>| {
            let ids: HashSet<NoteId> = snap.iter().map(|n| n.id).collect();
            m.notes.retain(|x| !ids.contains(&x.id));
            m.selected_note_ids.retain(|sid| !ids.contains(sid));
            m.notes_generation += 1;
        },
    )
}

fn make_move_notes_edit(deltas: Vec<MoveDelta>) -> Edit<PianoRollModel> {
    let label = if deltas.len() == 1 { "move note" } else { "move notes" };
    Edit::snapshot_inverse(
        label,
        deltas,
        |m: &mut PianoRollModel, snap: &Vec<MoveDelta>| {
            for (id, _, _, ns, np) in snap.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.start_beat = ns;
                    n.pitch = np;
                }
            }
            m.notes.sort_by(|a, b| {
                a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal)
            });
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &Vec<MoveDelta>| {
            for (id, ps, pp, _, _) in snap.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.start_beat = ps;
                    n.pitch = pp;
                }
            }
            m.notes.sort_by(|a, b| {
                a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal)
            });
            m.notes_generation += 1;
        },
    )
}

fn make_resize_notes_edit(deltas: Vec<ResizeDelta>) -> Edit<PianoRollModel> {
    let label = if deltas.len() == 1 { "resize note" } else { "resize notes" };
    Edit::snapshot_inverse(
        label,
        deltas,
        |m: &mut PianoRollModel, snap: &Vec<ResizeDelta>| {
            for (id, _, _, ns, nl) in snap.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.start_beat = ns;
                    n.len_beats = nl;
                }
            }
            m.notes.sort_by(|a, b| {
                a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal)
            });
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &Vec<ResizeDelta>| {
            for (id, ps, pl, _, _) in snap.iter().copied() {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == id) {
                    n.start_beat = ps;
                    n.len_beats = pl;
                }
            }
            m.notes.sort_by(|a, b| {
                a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal)
            });
            m.notes_generation += 1;
        },
    )
}

fn make_select_notes_edit(prev: Vec<NoteId>, next: Vec<NoteId>) -> Edit<PianoRollModel> {
    Edit::snapshot_inverse(
        "select notes",
        (prev, next),
        |m: &mut PianoRollModel, snap: &(Vec<NoteId>, Vec<NoteId>)| {
            m.selected_note_ids.clone_from(&snap.1);
        },
        |m: &mut PianoRollModel, snap: &(Vec<NoteId>, Vec<NoteId>)| {
            m.selected_note_ids.clone_from(&snap.0);
        },
    )
}

fn make_delete_notes_edit(notes: Vec<Note>) -> Edit<PianoRollModel> {
    let label = if notes.len() == 1 { "delete note" } else { "delete notes" };
    Edit::snapshot_inverse(
        label,
        notes,
        |m: &mut PianoRollModel, snap: &Vec<Note>| {
            let ids: HashSet<NoteId> = snap.iter().map(|n| n.id).collect();
            m.notes.retain(|x| !ids.contains(&x.id));
            m.selected_note_ids.retain(|sid| !ids.contains(sid));
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &Vec<Note>| {
            for note in snap {
                m.notes.push(*note);
            }
            m.notes.sort_by(|a, b| {
                a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal)
            });
            m.notes_generation += 1;
        },
    )
}

// ----- レイアウト -----

fn rect_for_widget(screen: PhysicalSize) -> Rect {
    Rect {
        x: 0.0,
        y: HEADER_H,
        w: (screen.width as f32).max(1.0),
        h: (screen.height as f32 - HEADER_H - FOOTER_H).max(1.0),
    }
}

fn grid_rect_for_user_input(screen: PhysicalSize) -> Rect {
    let r = rect_for_widget(screen);
    Rect {
        x: r.x + KEYBOARD_W,
        y: r.y,
        w: (r.w - KEYBOARD_W).max(1.0),
        h: r.h,
    }
}

// ----- App -----

struct App {
    window: Arc<winit_backend::WinitWindow>,
    renderer: Renderer<winit_backend::WinitWindow>,
    ui: UiHost<PianoRollModel>,
    model: PianoRollModel,
    scene: Scene,
    input: InputAccumulator,

    /// pan drag 状態 (note_drag は library widget が握るので、ここは pan 専用)。
    /// (anchor_x, anchor_y, anchor_view_start_beat, anchor_pitch_top)
    pan_anchor: Option<(f32, f32, f32, f32)>,
    cur_mouse: Option<(f32, f32)>,
    pending_zoom_dy: f32,

    /// HUD HIT/MISS 推定用
    last_viewport_hash: Option<u64>,

    last_frame_start: Option<Instant>,
}

impl App {
    fn new(window: Arc<winit_backend::WinitWindow>, n_notes: usize) -> Self {
        let renderer = Renderer::new(window.clone()).expect("Renderer::new");
        window.set_title("daw-ui piano_roll (M9 Phase 41e — library widget)");
        let notes = generate_notes(n_notes);
        let mut ui = UiHost::with_window(window.clone());
        ui.shortcut_map_mut().bind("add_note", "Insert");
        Self {
            ui,

            window,
            renderer,
            model: PianoRollModel::new(notes),
            scene: Scene::new(),
            input: InputAccumulator::new(),
            pan_anchor: None,
            cur_mouse: None,
            pending_zoom_dy: 0.0,
            last_viewport_hash: None,
            last_frame_start: None,
        }
    }

    /// pan / wheel zoom を反映 (library widget は pan を扱わないので user 側で)。
    /// pointer は frame 用に取り出した snapshot を渡す (frame と pan で共有)。
    fn apply_pan_and_zoom(
        &mut self,
        pointer: daw_ui_core::PointerFrame,
        screen: PhysicalSize,
    ) {
        let grid = grid_rect_for_user_input(screen);

        // pan は note 上以外で press → drag。Alt 修飾は widget の rect select に譲る。
        if pointer.primary_just_pressed
            && !pointer.modifiers.alt
            && let Some((px, py)) = pointer.pos
            && grid.contains(px, py)
        {
            // note 上で press したか? note_hit を library 関数で問い合わせ。
            // hit があれば widget が drag を握るので、user は pan しない。
            let hit = daw_ui_core::note_hit(
                &self.model.notes,
                self.model.view(),
                grid,
                px,
                py,
                4.0,
            );
            if hit.is_none() {
                self.pan_anchor = Some((
                    px,
                    py,
                    self.model.view_start_beat,
                    self.model.pitch_top,
                ));
            }
        }

        // pan continue
        if let (Some((ax, ay, ave_start, ave_pitch_top)), Some((px, py))) =
            (self.pan_anchor, pointer.pos)
        {
            let dx = px - ax;
            let dy = py - ay;
            let beat_per_px = self.model.view_len_beats / grid.w.max(1.0);
            let pitch_per_px = self.model.pitch_visible / grid.h.max(1.0);
            self.model.view_start_beat = (ave_start - dx * beat_per_px).max(0.0);
            self.model.pitch_top = (ave_pitch_top + dy * pitch_per_px)
                .min(127.0)
                .max(self.model.pitch_visible - 1.0);
        }

        // pan release
        if pointer.primary_just_released {
            self.pan_anchor = None;
        }

        // wheel zoom (無修飾 = X zoom、Ctrl+wheel = Y zoom)
        if self.pending_zoom_dy.abs() > 0.0 {
            let factor = (-self.pending_zoom_dy * 0.15).exp();
            if pointer.modifiers.ctrl {
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
    fn build_ui(&mut self, input: daw_ui_core::FrameInput) {
        self.scene.clear();
        let screen = self.renderer.size();
        let widget_rect = rect_for_widget(screen);

        // viewport_key (HUD HIT/MISS 推定用)
        let viewport_key = (
            b"piano_roll_v2" as &[u8],
            self.model.view_start_beat.to_bits(),
            self.model.view_len_beats.to_bits(),
            self.model.pitch_top.to_bits(),
            self.model.pitch_visible.to_bits(),
            widget_rect.w.to_bits(),
            widget_rect.h.to_bits(),
            self.model.notes_generation,
        );
        let viewport_hash = hash_inputs(viewport_key);
        let cache_status = if Some(viewport_hash) == self.last_viewport_hash {
            "HIT "
        } else {
            "MISS"
        };
        self.last_viewport_hash = Some(viewport_hash);

        // visible 件数 (HUD 用)
        let view_end_beat = self.model.view_start_beat + self.model.view_len_beats;
        let view_start_beat = self.model.view_start_beat;
        let s_idx = self
            .model
            .notes
            .partition_point(|n| n.start_beat + n.len_beats < view_start_beat);
        let e_idx = s_idx
            + self.model.notes[s_idx..]
                .partition_point(|n| n.start_beat <= view_end_beat);
        let visible_count = (e_idx - s_idx) as u32;
        let total_notes = self.model.notes.len();

        let hud_text = format!(
            "frame {:>5.2}ms │ visible {:>5} / {} notes │ cache {} │ view [{:.2}..{:.2}) beats │ pitch top={:.1} │ sel {}",
            self.model.last_frame_ms,
            visible_count,
            total_notes,
            cache_status,
            self.model.view_start_beat,
            view_end_beat,
            self.model.pitch_top,
            self.model.selected_note_ids.len(),
        );

        self.ui.frame(
            &mut self.model,
            &mut self.scene,
            screen,
            input,
            |m, ui| {
                // shortcut undo / redo
                if ui.take_shortcut("undo") {
                    ui.request_undo();
                }
                if ui.take_shortcut("redo") {
                    ui.request_redo();
                }

                // Header HUD
                ui.label_at(
                    "title",
                    "daw-ui piano_roll — M9 Phase 41e (library widget)",
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

                // ===== Library widget 呼び出し =====
                let view = m.view();
                let style = PianoRollStyle::default();
                // next_note_id を Add Edit closure に capture する (id=0 placeholder の上書き)。
                // make_add_notes_edit の forward 内で `next_note_id` も bump されるため、
                // capture 値は「現フレーム時点の最新値」。redo は forward 再実行で重複 id にならない。
                let next_id_for_add = m.next_note_id;

                let resp: PianoRollResponse = ui.piano_roll(
                    "main",
                    widget_rect,
                    &m.notes,
                    view,
                    &m.selected_note_ids,
                    &style,
                    move |req| match req {
                        NotesEditRequest::Add(mut notes) => {
                            // library widget は id=0 placeholder で渡す → user が next_note_id で上書き
                            for note in &mut notes {
                                note.id = next_id_for_add;
                            }
                            make_add_notes_edit(notes)
                        }
                        NotesEditRequest::Delete(notes) => make_delete_notes_edit(notes),
                        NotesEditRequest::Move(deltas) => make_move_notes_edit(deltas),
                        NotesEditRequest::Resize(deltas) => make_resize_notes_edit(deltas),
                        NotesEditRequest::Select { prev, next } => {
                            make_select_notes_edit(prev, next)
                        }
                    },
                );

                // last_action 更新は Edit::mutate で発行 (frame closure 内 m は &M なので直接書けない)。
                // Response の状態変化を見て次フレームで反映される small status text。
                if resp.selection_changed {
                    ui.push_edit(Edit::mutate(|m: &mut PianoRollModel| {
                        let n = m.selected_note_ids.len();
                        m.last_action = format!("selection: {n} note(s)");
                    }));
                }
                if resp.rect_select_active {
                    ui.push_edit(Edit::mutate(|m: &mut PianoRollModel| {
                        m.last_action = "rect select drag…".to_string();
                    }));
                }

                // Footer
                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "Drag = pan / Wheel = X zoom / Ctrl+Wheel = Y zoom / Click = select / Insert = add / Delete / Alt+drag = rect-select / Ctrl+Z = undo",
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
        // input を 1 度取り出し、pan/wheel logic と library widget で共有 (PointerFrame は Copy)。
        let input = self.input.take_input();
        let pointer = input.pointer;
        // pan / wheel zoom (note drag は library widget が握る)
        self.apply_pan_and_zoom(pointer, screen);
        self.last_frame_start = Some(now);
        self.build_ui(input);
        if let Err(e) = self.renderer.render(&self.scene) {
            eprintln!("render error: {e}");
        }
        if let Some(t) = self.last_frame_start.take() {
            self.model.last_frame_ms = t.elapsed().as_secs_f32() * 1000.0;
        }
        self.pan_anchor.is_some() || self.pending_zoom_dy.abs() > 0.0
    }
}

fn main() {
    let n_notes: usize = std::env::var("PIANO_ROLL_NOTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);
    let attrs = WindowAttributes::default()
        .with_title("daw-ui piano_roll (M9 Phase 41e)")
        .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
    if let Err(e) = winit_backend::run_app(attrs, move |window| App::new(Arc::new(window), n_notes)) {
        eprintln!("event loop error: {e}");
    }
}

// ============================================================
// M9 Phase 41a-d: Edit factory の Undoable round-trip tests
// (PianoRollModel 依存なので example 側に残す)
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use daw_ui_core::Edit;

    fn note(id: NoteId, start: f32, len: f32, pitch: u8) -> Note {
        Note { id, start_beat: start, len_beats: len, pitch, velocity: 96 }
    }

    #[test]
    fn add_single_note_then_undo_round_trip() {
        let mut model = PianoRollModel::new(vec![]);
        let n = note(0, 0.0, 0.5, 60);
        let edit = make_add_notes_edit(vec![n]);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        assert_eq!(model.notes.len(), 1);
        assert_eq!(model.notes[0].id, 0);
        inverse(&mut model);
        assert_eq!(model.notes.len(), 0);
    }

    #[test]
    fn add_multiple_notes_then_undo_round_trip() {
        let mut model = PianoRollModel::new(vec![]);
        let notes_to_add = vec![
            note(10, 1.0, 0.5, 60),
            note(11, 2.0, 0.5, 64),
            note(12, 3.0, 0.5, 67),
        ];
        let edit = make_add_notes_edit(notes_to_add);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        assert_eq!(model.notes.len(), 3);
        let ids: Vec<NoteId> = model.notes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![10, 11, 12]);
        inverse(&mut model);
        assert!(model.notes.is_empty());
    }

    #[test]
    fn delete_notes_then_undo_restores_original_state() {
        let initial = vec![
            note(1, 0.0, 0.5, 60),
            note(2, 1.0, 0.5, 64),
            note(3, 2.0, 0.5, 67),
        ];
        let mut model = PianoRollModel::new(initial);
        let to_delete = vec![note(2, 1.0, 0.5, 64)];
        let edit = make_delete_notes_edit(to_delete);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        assert_eq!(model.notes.len(), 2);
        let remaining_ids: Vec<NoteId> = model.notes.iter().map(|n| n.id).collect();
        assert_eq!(remaining_ids, vec![1, 3]);
        inverse(&mut model);
        let restored_ids: Vec<NoteId> = model.notes.iter().map(|n| n.id).collect();
        assert_eq!(restored_ids, vec![1, 2, 3]);
    }

    #[test]
    fn delete_notes_clears_corresponding_selection() {
        let initial = vec![note(1, 0.0, 0.5, 60), note(2, 1.0, 0.5, 64)];
        let mut model = PianoRollModel::new(initial);
        model.selected_note_ids = vec![1, 2];
        let to_delete = vec![note(1, 0.0, 0.5, 60)];
        let edit = make_delete_notes_edit(to_delete);
        if let Edit::Undoable { forward, .. } = edit {
            forward(&mut model);
        }
        assert_eq!(model.selected_note_ids, vec![2]);
    }

    #[test]
    fn move_notes_then_undo_round_trip() {
        let initial = vec![note(0, 0.0, 0.5, 60), note(1, 1.0, 0.5, 64)];
        let mut model = PianoRollModel::new(initial);
        let deltas: Vec<MoveDelta> = vec![
            (0u32, 0.0_f32, 60u8, 2.0_f32, 72u8),
            (1u32, 1.0_f32, 64u8, 3.0_f32, 70u8),
        ];
        let edit = make_move_notes_edit(deltas);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        let n0 = model.notes.iter().find(|n| n.id == 0).unwrap();
        assert!((n0.start_beat - 2.0).abs() < 1e-6);
        assert_eq!(n0.pitch, 72);
        inverse(&mut model);
        let n0 = model.notes.iter().find(|n| n.id == 0).unwrap();
        assert!((n0.start_beat - 0.0).abs() < 1e-6);
        assert_eq!(n0.pitch, 60);
    }

    #[test]
    fn resize_notes_then_undo_round_trip_right_edge() {
        let initial = vec![note(0, 0.0, 0.5, 60)];
        let mut model = PianoRollModel::new(initial);
        let deltas: Vec<ResizeDelta> = vec![(0u32, 0.0_f32, 0.5_f32, 0.0_f32, 1.0_f32)];
        let edit = make_resize_notes_edit(deltas);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        assert!((model.notes[0].len_beats - 1.0).abs() < 1e-6);
        inverse(&mut model);
        assert!((model.notes[0].len_beats - 0.5).abs() < 1e-6);
    }

    #[test]
    fn resize_notes_then_undo_round_trip_left_edge() {
        let initial = vec![note(0, 1.0, 1.0, 60)];
        let mut model = PianoRollModel::new(initial);
        let deltas: Vec<ResizeDelta> = vec![(0u32, 1.0_f32, 1.0_f32, 0.75_f32, 1.25_f32)];
        let edit = make_resize_notes_edit(deltas);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        let n = &model.notes[0];
        assert!((n.start_beat - 0.75).abs() < 1e-6);
        assert!((n.len_beats - 1.25).abs() < 1e-6);
        inverse(&mut model);
        let n = &model.notes[0];
        assert!((n.start_beat - 1.0).abs() < 1e-6);
        assert!((n.len_beats - 1.0).abs() < 1e-6);
    }

    #[test]
    fn select_notes_then_undo_restores_prev_selection() {
        let mut model = PianoRollModel::new(vec![note(0, 0.0, 0.5, 60), note(1, 1.0, 0.5, 64)]);
        model.selected_note_ids = vec![0];
        let prev: Vec<NoteId> = vec![0u32];
        let next: Vec<NoteId> = vec![0u32, 1u32];
        let edit = make_select_notes_edit(prev, next);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        assert_eq!(model.selected_note_ids, vec![0, 1]);
        inverse(&mut model);
        assert_eq!(model.selected_note_ids, vec![0]);
    }

    #[test]
    fn move_preserves_sort_order() {
        let initial = vec![note(0, 0.0, 0.5, 60), note(1, 1.0, 0.5, 64)];
        let mut model = PianoRollModel::new(initial);
        let deltas: Vec<MoveDelta> =
            vec![(0u32, 0.0_f32, 60u8, 2.0_f32, 60u8), (1u32, 1.0_f32, 64u8, 0.5_f32, 64u8)];
        let edit = make_move_notes_edit(deltas);
        if let Edit::Undoable { forward, .. } = edit {
            forward(&mut model);
        }
        let order: Vec<NoteId> = model.notes.iter().map(|n| n.id).collect();
        assert_eq!(order, vec![1, 0]);
    }
}
