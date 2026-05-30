//! examples/piano_roll — M9 Phase 41e: library widget `Ui::piano_roll` の実用例。
//!
//! 100k notes を `crates/ui/src/widgets/piano_roll.rs` の library widget で描画する。
//! example は HUD / view state pan + wheel zoom / window 起動 / Edit factory dispatch のみ
//! を担い、描画 + drag state machine + hit-test + Shift+drag + shortcut は widget に閉じ込める。
//!
//! 操作:
//! - 無修飾 drag = pan (空白 or note なし上で press → drag)
//! - 無修飾 wheel = X zoom (cur_mouse 位置 anchor)
//! - Ctrl+wheel = Y zoom (pitch 範囲を変える)
//! - note 中央 drag = move (release で Undoable)
//! - note 左右端 drag = resize (release で Undoable)
//! - note click (drag<16px) = selection 1 個
//! - 空白 click = selection clear
//! - Shift+drag = rect multi-select (加算: 既存選択 ∪ rect 内)
//! - Insert = pointer 位置に新規 note 追加 (next_note_id を bump)
//! - Delete = selected を一括削除
//! - Ctrl+Z / Ctrl+Shift+Z = undo / redo (M8 history stack)

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use daw_ui_core::{
    Edit, InputAccumulator, MoveDelta, Note, NoteId, PianoRollEditRequest, PianoRollResponse,
    PianoRollStyle, PianoRollView, ResizeDelta, SnapConfig, UiHost, VelocityUpdate, hash_inputs,
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

    /// 拍は f64 (M9 Phase 44c 後の Note schema と一致)。
    view_start_beat: f64,
    view_len_beats: f64,
    /// pitch は MIDI 0..127 なので f32 で十分。
    pitch_top: f32,
    pitch_visible: f32,

    /// (M14 Phase 69 / daw_01 #041) ruler 上 click / drag で動かす playhead 位置 (song-global beat)。
    playhead_beat: f64,
    /// (M14 Phase 69 / daw_01 #041) ruler 上 Shift+drag で edit する loop range (`(start, end)`)。
    /// `None` で loop band 非表示 (起動時のデフォルト)。 Shift+drag NewRange で新規作成、
    /// 既存 range の Start/End/Middle handle drag で edit、 Alt 押下で snap 一時無効化。
    loop_range: Option<(f64, f64)>,
    /// (M14 Phase 70 / daw_01 #042) scale 状態。 起動時 `None`、 Tab で Highlight → Fold → None を
    /// 順送り、 数字キー (1=C 〜 9=G#) で root pitch class を切替 (Shift+数字で半音上)。 Bitwig
    /// と同じく Major scale 固定の demo (= `in_scale_mask = 0b0000_1010_1011_0101`)。
    scale: Option<daw_ui_core::PianoRollScale>,
    /// (M14 Phase 70b / daw_01 #042 follow-up) drag preview snap toggle。 S key で flip。
    /// `scale = Some(Highlight)` + `true` で y-drag 中 preview が in-scale 行に jump (= Bitwig 流)。
    snap_pitch_during_drag: bool,

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
            playhead_beat: 2.0,
            loop_range: None,
            scale: None,
            snap_pitch_during_drag: false,
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
            // M9 Phase 45c: velocity lane の描画 + M14 Phase 64: lane 内 drag で velocity 編集。
            // 60.0 px で表示 (drag→ release で SetVelocity 発行 / multi-select で同値更新)。
            velocity_lane_h: 60.0,
            playhead_beat: Some(self.playhead_beat),
            // M13 Phase 55: ruler 領域 + bpm + time_sig (デモのため ruler_h: 20.0 で
            // 上端に小節番号テキスト表示)。
            ruler_h: 20.0,
            bpm: 120.0,
            time_sig: (4, 4),
            // M9 Phase 45f (#010 [Replied]): デフォルト Adaptive snap で grid 吸着の動作確認。
            snap: SnapConfig::DEFAULT,
            // M14 Phase 69 / daw_01 #041: ruler 上 Shift+drag で edit するための loop range。
            // demo では `None` で起動 (Shift+drag で新規 NewRange を作成可能)、 user 操作で更新される。
            loop_range: self.loop_range,
            // M14 Phase 70 / daw_01 #042: scale 機能。 例では起動時 None、 Tab / Shift+Tab で
            // Highlight ↔ Fold ↔ None を遷移、 1〜9 で root pitch class を変えられる (下記 build_ui)。
            scale: self.scale,
            // M14 Phase 70b / daw_01 #042 follow-up: Highlight + Snap on Draw 相当の drag preview snap。
            // S key で flip (下記 build_ui)。 demo 用、 daw_01 では `app.snap_on_draw` を流す想定。
            snap_pitch_during_drag: self.snap_pitch_during_drag,
        }
    }
}

/// 100k 個を決定論的 LCG で生成。`start_beat` 昇順にソート。
fn generate_notes(count: usize) -> Vec<Note> {
    // M14 Phase 59 / daw_01 #017: 小 count (≤ 32) は歌詞編集 demo 用に「default view 内に
    // 並ぶ連続 note 列」を生成する。 100k benchmark 用 LCG random 配置は count > 32 のときのみ。
    // default view は 4 拍 × 24 pitch (start_beat=0..4、 pitch_top=72) なので、
    // 0.5 拍刻みで pitch=60 (中央 C) 行に並べると最大 8 note が画面内に collide なく入る。
    if count <= 32 {
        return (0..count)
            .map(|i| Note {
                id: i as NoteId,
                start_beat: i as f64 * 0.5,
                len_beats: 0.45,
                pitch: 60,
                velocity: 96,
                lyric: None,
            })
            .collect();
    }

    let mut state: u64 = 0x12345678_9ABCDEF0;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    };

    let total_beats: f64 = 1024.0;
    let pitch_lo: u8 = 36;
    let pitch_hi: u8 = 96;
    let mut notes: Vec<Note> = Vec::with_capacity(count);
    // M9 Phase 44c: lyric demo として 8 note ごとに 1 つだけ Arc<str> を付ける
    // (毎 note に付けると視覚的にうるさいので、demo として疎に配置)。
    let demo_lyrics: [&str; 5] = ["ら", "る", "れ", "ろ", "り"];
    for i in 0..count {
        let r1 = next();
        let r2 = next();
        let r3 = next();
        let r4 = next();
        let start_beat = (r1 as f64 / u64::MAX as f64) * total_beats;
        let len_beats = 0.125_f64 + (r2 as f64 / u64::MAX as f64) * 1.875_f64;
        let pitch = pitch_lo + ((r3 % u64::from(pitch_hi - pitch_lo)) as u8);
        let velocity = 32 + ((r4 % 96) as u8);
        let lyric = if i % 8 == 0 {
            Some(Arc::from(demo_lyrics[(i / 8) % demo_lyrics.len()]))
        } else {
            None
        };
        notes.push(Note { id: i as NoteId, start_beat, len_beats, pitch, velocity, lyric });
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
            // Note は Copy ではない (Arc<str> lyric を持つ) ので clone() で push。
            for note in snap {
                m.notes.push(note.clone());
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

/// (M14 Phase 83 / daw_01 #054) Ctrl+drag で発火する `Copy(deltas)` を受け、各 source note を
/// deep clone + `new_*` 位置へ配置 (元は据え置き)、複製を新 selection にする Undoable Edit。
/// 複製 id は `base_id` から連番 (Add と同じく `next_note_id` を bump)。daw_01 では
/// `duplicate_notes` に集約する model 操作の demo 相当。
fn make_copy_notes_edit(
    deltas: Vec<MoveDelta>,
    base_id: NoteId,
    prev_selection: Vec<NoteId>,
) -> Edit<PianoRollModel> {
    let label = if deltas.len() == 1 { "copy note" } else { "copy notes" };
    Edit::snapshot_inverse(
        label,
        (deltas, base_id, prev_selection),
        |m: &mut PianoRollModel, snap: &(Vec<MoveDelta>, NoteId, Vec<NoteId>)| {
            let (deltas, base, _prev) = snap;
            let mut new_ids: Vec<NoteId> = Vec::with_capacity(deltas.len());
            for (i, (src_id, _, _, nb, np)) in deltas.iter().enumerate() {
                // Note は Copy ではない (Arc<str> lyric) ので clone() で複製。
                if let Some(src) = m.notes.iter().find(|n| n.id == *src_id) {
                    let mut dup = src.clone();
                    dup.id = *base + i as NoteId;
                    dup.start_beat = *nb;
                    dup.pitch = *np;
                    new_ids.push(dup.id);
                    m.notes.push(dup);
                }
            }
            m.next_note_id = m.next_note_id.max(*base + deltas.len() as NoteId);
            m.selected_note_ids = new_ids;
            m.notes.sort_by(|a, b| {
                a.start_beat.partial_cmp(&b.start_beat).unwrap_or(std::cmp::Ordering::Equal)
            });
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &(Vec<MoveDelta>, NoteId, Vec<NoteId>)| {
            let (deltas, base, prev) = snap;
            let ids: HashSet<NoteId> =
                (0..deltas.len()).map(|i| *base + i as NoteId).collect();
            m.notes.retain(|x| !ids.contains(&x.id));
            // copy の forward は selection を複製に上書きするので、undo は複製前の selection を
            // 復元する (複製 id 除外だけでは元選択が失われる、make_select_notes_edit と同じ対称性)。
            m.selected_note_ids.clone_from(prev);
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

/// (M14 Phase 59 / daw_01 #017) note 群の lyric を一括更新する undoable Edit。
/// snapshot に `(id, prev_lyric, next_lyric)` を持って forward/inverse を切替える。
/// `prev_lyric` は widget が dispatch closure で `m.notes` から事前に capture 済み。
/// (M14 Phase 64 / daw_01 #018) velocity を 1 commit で更新する Edit。
/// `(id, prev_velocity, next_velocity)` を snapshot に持ち、 forward / backward 両方向で適用可能。
/// widget は `Vec<(NoteId, u8)>` (= new only) を渡してくるので、 caller が現フレーム時点の
/// `m.notes` から prev を引いて この helper に渡す (make_set_lyrics_edit と同 pattern)。
fn make_set_velocity_edit(deltas: Vec<(NoteId, u8, u8)>) -> Edit<PianoRollModel> {
    let label = if deltas.len() == 1 { "set velocity" } else { "set velocities" };
    Edit::snapshot_inverse(
        label,
        deltas,
        |m: &mut PianoRollModel, snap: &Vec<(NoteId, u8, u8)>| {
            for (id, _prev, next) in snap {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == *id) {
                    n.velocity = *next;
                }
            }
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &Vec<(NoteId, u8, u8)>| {
            for (id, prev, _next) in snap {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == *id) {
                    n.velocity = *prev;
                }
            }
            m.notes_generation += 1;
        },
    )
}

fn make_set_lyrics_edit(
    deltas: Vec<(NoteId, Option<String>, Option<String>)>,
) -> Edit<PianoRollModel> {
    let label = if deltas.len() == 1 { "set lyric" } else { "set lyrics" };
    Edit::snapshot_inverse(
        label,
        deltas,
        |m: &mut PianoRollModel, snap: &Vec<(NoteId, Option<String>, Option<String>)>| {
            for (id, _prev, next) in snap {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == *id) {
                    n.lyric = next.as_deref().map(Arc::from);
                }
            }
            m.notes_generation += 1;
        },
        |m: &mut PianoRollModel, snap: &Vec<(NoteId, Option<String>, Option<String>)>| {
            for (id, prev, _next) in snap {
                if let Some(n) = m.notes.iter_mut().find(|x| x.id == *id) {
                    n.lyric = prev.as_deref().map(Arc::from);
                }
            }
            m.notes_generation += 1;
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
                m.notes.push(note.clone());
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
    // M9 Phase 60 fix: ruler_h / velocity_lane_h を引いて widget 内部 grid と一致させる。
    // これがズレると `note_hit` の grid.y origin が widget と app で異なり、 同じ note でも
    // app は hit=None / widget は hit=Some となり pan と note drag が同時に走る既存 bug
    // (Phase 55 ruler 追加以降)。 値は `App::view()` の `ruler_h: 20.0` / `velocity_lane_h: 0.0`
    // と一致 (構造上 model から view を渡せば DRY だが、 grid_rect_for_user_input は app
    // 側 layout 関数なので const と一致させる方針)。
    const RULER_H: f32 = 20.0;
    const VELOCITY_LANE_H: f32 = 0.0;
    let r = rect_for_widget(screen);
    Rect {
        x: r.x + KEYBOARD_W,
        y: r.y + RULER_H,
        w: (r.w - KEYBOARD_W).max(1.0),
        h: (r.h - RULER_H - VELOCITY_LANE_H).max(1.0),
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
    /// (anchor_x: f32, anchor_y: f32, anchor_view_start_beat: f64, anchor_pitch_top: f32)
    pan_anchor: Option<(f32, f32, f64, f32)>,
    cur_mouse: Option<(f32, f32)>,
    pending_zoom_dy: f32,

    /// (M14 Phase 59 / daw_01 #017) IME 有効/無効と候補ウィンドウ位置の差分管理。
    /// `UiHost::ime_request()` の `Some` / `None` 切替を監視し、 `set_ime_allowed` を
    /// 状態遷移時のみ呼ぶ (mixer.rs と同パターン)。 IME 候補位置 (cursor 直下) は
    /// `text_input_at` 内 `request_ime` で渡されるので、 ここでは差分のみ反映。
    ime_enabled: bool,

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
        // M14 Phase 59 / daw_01 #017: 歌詞 inline 編集モードの起動 shortcut。
        // PianoRollStyle::default().lyric_edit_shortcut == Some("piano_roll.edit_lyric") と
        // 整合する name を bind する。caller opt-in のため with_default_bindings には含めない。
        ui.shortcut_map_mut().bind("piano_roll.edit_lyric", "L");
        // M14 Phase 70 / daw_01 #042: scale mode / root の demo shortcut。
        // K (= Ableton "Fold to Scale" 同 key) で Highlight → Fold → None を順送り、
        // R で root pitch class を +1 (C → C# → D → ...) cycle。 demo 用、 caller が独自に bind 可能。
        ui.shortcut_map_mut().bind("piano_roll.demo_scale_mode_cycle", "K");
        ui.shortcut_map_mut().bind("piano_roll.demo_scale_root_cycle", "R");
        // M14 Phase 70b / daw_01 #042 follow-up: Highlight + Snap on Draw 相当の demo toggle。
        // S で snap_pitch_during_drag を flip。 Highlight mode で note y-drag 中 preview が
        // in-scale 行へ jump する Bitwig / Cubase 流 UX を確認できる。 daw_01 では
        // app.snap_on_draw を流す想定。
        ui.shortcut_map_mut().bind("piano_roll.demo_snap_drag_toggle", "S");
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
            ime_enabled: false,
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

        // pan は note 上以外で press → drag。Shift 修飾は widget の rect select に譲る。
        if pointer.primary_just_pressed
            && !pointer.modifiers.shift
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
            let beat_per_px: f64 = self.model.view_len_beats / f64::from(grid.w.max(1.0));
            let pitch_per_px = self.model.pitch_visible / grid.h.max(1.0);
            self.model.view_start_beat = (ave_start - f64::from(dx) * beat_per_px).max(0.0);
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
                // factor / anchor_frac は f32、view_len_beats は f64 → 計算は f64 で。
                let new_len: f64 =
                    (self.model.view_len_beats * f64::from(factor)).clamp(0.25, 256.0);
                let anchor_beat =
                    self.model.view_start_beat + f64::from(anchor_frac) * self.model.view_len_beats;
                let new_start = (anchor_beat - f64::from(anchor_frac) * new_len).max(0.0);
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

                // M14 Phase 70 / daw_01 #042: scale demo shortcuts。
                // K で mode cycle (None → Highlight → Fold → None)、 R で root cycle (C → C# → ...)。
                // Major scale 固定 demo。 caller が ScaleChange 経由で実 scale を渡す本番は daw_01 側。
                if ui.take_shortcut("piano_roll.demo_scale_mode_cycle") {
                    ui.push_edit(Edit::mutate(|m: &mut PianoRollModel| {
                        const MAJOR_MASK: u16 = 0b0000_1010_1011_0101;
                        m.scale = match m.scale {
                            None => Some(daw_ui_core::PianoRollScale {
                                root: 0,
                                in_scale_mask: MAJOR_MASK,
                                mode: daw_ui_core::PianoRollScaleMode::Highlight,
                            }),
                            Some(sc) => match sc.mode {
                                daw_ui_core::PianoRollScaleMode::Highlight => {
                                    Some(daw_ui_core::PianoRollScale {
                                        mode: daw_ui_core::PianoRollScaleMode::Fold,
                                        ..sc
                                    })
                                }
                                daw_ui_core::PianoRollScaleMode::Fold => None,
                            },
                        };
                        m.last_action = format!("scale mode → {:?}", m.scale.map(|s| s.mode));
                    }));
                }
                if ui.take_shortcut("piano_roll.demo_scale_root_cycle") {
                    ui.push_edit(Edit::mutate(|m: &mut PianoRollModel| {
                        const MAJOR_MASK: u16 = 0b0000_1010_1011_0101;
                        let cur =
                            m.scale.unwrap_or(daw_ui_core::PianoRollScale {
                                root: 0,
                                in_scale_mask: MAJOR_MASK,
                                mode: daw_ui_core::PianoRollScaleMode::Highlight,
                            });
                        let new_root = (cur.root + 1) % 12;
                        m.scale = Some(daw_ui_core::PianoRollScale { root: new_root, ..cur });
                        m.last_action = format!(
                            "scale root → {} ({:?})",
                            daw_ui_core::pitch_class_name(new_root),
                            cur.mode
                        );
                    }));
                }
                if ui.take_shortcut("piano_roll.demo_snap_drag_toggle") {
                    ui.push_edit(Edit::mutate(|m: &mut PianoRollModel| {
                        m.snap_pitch_during_drag = !m.snap_pitch_during_drag;
                        m.last_action = format!(
                            "snap_pitch_during_drag → {} (Highlight + drag で in-scale snap)",
                            m.snap_pitch_during_drag
                        );
                    }));
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
                // M14 Phase 59 / daw_01 #017: SetLyrics の undo 用 prev capture。
                // widget は新 lyric のみ (Vec<(NoteId, Option<String>)>) を返すので、
                // dispatch closure で「現フレーム時点の各 note の lyric」を snapshot して
                // make_set_lyrics_edit に prev として渡す。
                let lyric_snapshot: Vec<(NoteId, Option<Arc<str>>)> =
                    m.notes.iter().map(|n| (n.id, n.lyric.clone())).collect();
                // M14 Phase 64 / daw_01 #018: SetVelocity も同様に prev snapshot。
                // widget は (NoteId, new_velocity) のみ送ってくるので caller 側で prev を引いて
                // make_set_velocity_edit に (id, prev, next) として渡す = undo で復元可能に。
                let velocity_snapshot: Vec<(NoteId, u8)> =
                    m.notes.iter().map(|n| (n.id, n.velocity)).collect();
                // M14 Phase 83 / daw_01 #054: Copy の undo で複製前の selection を復元するための snapshot。
                let selection_snapshot: Vec<NoteId> = m.selected_note_ids.clone();

                let resp: PianoRollResponse = ui.piano_roll(
                    "main",
                    widget_rect,
                    &m.notes,
                    view,
                    &m.selected_note_ids,
                    &style,
                    move |req| match req {
                        PianoRollEditRequest::Add(mut notes) => {
                            // library widget は id=0 placeholder で渡す → user が next_note_id で上書き
                            for note in &mut notes {
                                note.id = next_id_for_add;
                            }
                            make_add_notes_edit(notes)
                        }
                        PianoRollEditRequest::Delete(notes) => make_delete_notes_edit(notes),
                        PianoRollEditRequest::Move(deltas) => make_move_notes_edit(deltas),
                        PianoRollEditRequest::Copy(deltas) => {
                            // 複製 id は Add と同じ next_note_id を base に連番採番。
                            // prev selection を渡して undo で複製前の選択を復元する。
                            make_copy_notes_edit(
                                deltas,
                                next_id_for_add,
                                selection_snapshot.clone(),
                            )
                        }
                        PianoRollEditRequest::Resize(deltas) => make_resize_notes_edit(deltas),
                        PianoRollEditRequest::Select { prev, next } => {
                            make_select_notes_edit(prev, next)
                        }
                        PianoRollEditRequest::SetLyrics(updates) => {
                            // updates = Vec<(NoteId, Option<String>)>。snapshot から prev を引く。
                            let with_prev: Vec<(NoteId, Option<String>, Option<String>)> = updates
                                .into_iter()
                                .map(|(id, next)| {
                                    let prev = lyric_snapshot
                                        .iter()
                                        .find(|(nid, _)| *nid == id)
                                        .and_then(|(_, l)| l.as_deref().map(String::from));
                                    (id, prev, next)
                                })
                                .collect();
                            make_set_lyrics_edit(with_prev)
                        }
                        PianoRollEditRequest::SetVelocity(updates) => {
                            // updates: Vec<VelocityUpdate> = Vec<(NoteId, u8)>。snapshot から prev velocity を引いて
                            // (id, prev, next) tuple に変換 → make_set_velocity_edit へ (undo 復元用)。
                            let with_prev: Vec<(NoteId, u8, u8)> = updates
                                .into_iter()
                                .map(|(id, next): VelocityUpdate| {
                                    let prev = velocity_snapshot
                                        .iter()
                                        .find(|(nid, _)| *nid == id)
                                        .map_or(0_u8, |(_, v)| *v);
                                    (id, prev, next)
                                })
                                .collect();
                            make_set_velocity_edit(with_prev)
                        }
                        // (M14 Phase 69 / daw_01 #041) ruler 上 click/drag で発火する playhead seek を
                        // model.playhead_beat に反映 (= 次 frame の view() で `playhead_beat: Some(...)` が
                        // 更新される)。 daw_01 では更に `MainToChild::SeekTo` 等の audio engine seek IPC を
                        // 連動させるが、 demo では Model 値の更新のみ (playhead 線が ruler に追従して動く)。
                        PianoRollEditRequest::SetPlayheadBeat(beat) => {
                            Edit::mutate(move |m: &mut PianoRollModel| {
                                m.playhead_beat = beat;
                                m.last_action = format!("playhead seek → {beat:.3} beat");
                            })
                        }
                        // (M14 Phase 69 / daw_01 #041) Shift+ruler drag → loop range edit を model に反映。
                        // demo では Model の loop_range field を更新するだけで、 daw_01 側では
                        // `app.song.loop_start_beat` / `loop_end_beat` 等への反映 + 必要なら audio engine
                        // への loop IPC 連動が caller 責務。
                        PianoRollEditRequest::SetLoopRange { start, end } => {
                            Edit::mutate(move |m: &mut PianoRollModel| {
                                m.loop_range = Some((start, end));
                                m.last_action =
                                    format!("loop range → ({start:.3}, {end:.3}) beat");
                            })
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
                // (M14 Phase 84 / daw_01 #055) 鍵盤レーン press のピッチプレビューを HUD に表示。
                // daw_01 では resp.keyboard_active_pitch を前フレーム値と差分して note-on/off を音源へ
                // 送るが、demo では last_action に pitch 名を出して動作確認する (鍵盤レーンを click)。
                if let Some(p) = resp.keyboard_active_pitch {
                    ui.push_edit(Edit::mutate(move |m: &mut PianoRollModel| {
                        m.last_action = format!(
                            "keyboard preview: pitch {p} ({})",
                            daw_ui_core::pitch_class_name(p % 12)
                        );
                    }));
                }

                // Footer
                let footer_y = (screen.height as f32 - 44.0).max(0.0);
                ui.label_at(
                    "footer1",
                    "Drag = pan / Wheel = X zoom / Ctrl+Wheel = Y zoom / Click = select / Insert = add / Delete / Shift+drag = rect-select / K = scale mode / R = scale root / S = snap drag / Ctrl+Z = undo",
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
        // M14 Phase 59 / daw_01 #017: IME 有効/無効 + 候補ウィンドウ位置を OS に伝える
        // (text_input が focus 中に `request_ime` した cursor area を winit に渡す)。
        // mixer.rs と同パターンで差分管理。
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
                // 位置だけ追従 (IME 候補が cursor 移動に追いつくよう毎フレーム)
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

    fn note(id: NoteId, start: f64, len: f64, pitch: u8) -> Note {
        Note { id, start_beat: start, len_beats: len, pitch, velocity: 96, lyric: None }
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
            (0u32, 0.0_f64, 60u8, 2.0_f64, 72u8),
            (1u32, 1.0_f64, 64u8, 3.0_f64, 70u8),
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
        let deltas: Vec<ResizeDelta> = vec![(0u32, 0.0_f64, 0.5_f64, 0.0_f64, 1.0_f64)];
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
        let deltas: Vec<ResizeDelta> = vec![(0u32, 1.0_f64, 1.0_f64, 0.75_f64, 1.25_f64)];
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
    fn copy_notes_then_undo_round_trip_restores_notes_and_selection() {
        // (M14 Phase 83 / daw_01 #054) 元 note 1 個を選択 → Copy で複製 (新位置 + 複製を選択)
        // → undo で複製削除 + 複製前の selection 復元、を 1 往復で固定。
        let mut model = PianoRollModel::new(vec![note(0, 1.0, 0.5, 60)]);
        model.selected_note_ids = vec![0];
        let deltas: Vec<MoveDelta> = vec![(0u32, 1.0_f64, 60u8, 3.0_f64, 62u8)];
        let edit = make_copy_notes_edit(deltas, 1, vec![0]);
        let Edit::Undoable { forward, inverse, .. } = edit else {
            panic!("expected Undoable");
        };
        forward(&mut model);
        // 元 note 据え置き + 複製 1 個 = 2 個、複製 (id 1) が新 selection。
        assert_eq!(model.notes.len(), 2);
        assert_eq!(model.selected_note_ids, vec![1]);
        let dup = model.notes.iter().find(|n| n.id == 1).unwrap();
        assert!((dup.start_beat - 3.0).abs() < 1e-6);
        assert_eq!(dup.pitch, 62);
        // 元 note (id 0) は据え置きで変化なし。
        let src = model.notes.iter().find(|n| n.id == 0).unwrap();
        assert!((src.start_beat - 1.0).abs() < 1e-6);
        assert_eq!(src.pitch, 60);
        // undo: 複製削除 + 複製前 selection (元 note) を復元。
        inverse(&mut model);
        assert_eq!(model.notes.len(), 1);
        assert_eq!(model.selected_note_ids, vec![0], "undo で複製前の selection 復元");
    }

    #[test]
    fn move_preserves_sort_order() {
        let initial = vec![note(0, 0.0, 0.5, 60), note(1, 1.0, 0.5, 64)];
        let mut model = PianoRollModel::new(initial);
        let deltas: Vec<MoveDelta> =
            vec![(0u32, 0.0_f64, 60u8, 2.0_f64, 60u8), (1u32, 1.0_f64, 64u8, 0.5_f64, 64u8)];
        let edit = make_move_notes_edit(deltas);
        if let Edit::Undoable { forward, .. } = edit {
            forward(&mut model);
        }
        let order: Vec<NoteId> = model.notes.iter().map(|n| n.id).collect();
        assert_eq!(order, vec![1, 0]);
    }
}
