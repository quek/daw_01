//! S4c: piano_roll widget を `AppData` 直結・`Edit<AppData>` 直発行に移設した後の
//! interaction 回帰テスト (旧 in-file `TestModel` + `PianoRollEditRequest` 記録方式を置換)。
//!
//! ドライブ手順: `AppData` に Song を組んで対象 MIDI クリップを選択し、`UiHost::<AppData>::frame`
//! で press → (hold) → release / key のフレーム列を `piano_roll(app, ui, rect)` に流す。
//! `UiHost::frame` は widget が発行した `Edit<AppData>` を `app` に自動 apply するので、
//! **適用後の app 状態**を assert する。
//!
//! 幾何 (widget 固定レイアウト、view state はデフォルト zoom_x=64 / zoom_y=14 / top_pitch=84):
//! toolbar=24 / ruler=20 / vel lane=60 / kbd=56。body y=24。grid = {x:56, y:44, w:744, h:496}。
//! 1 拍 = 64px (`x = 56 + beat*64`)、1 semitone = 14px (`y_top = 44 + (84 - pitch)*14`)。snap 無効。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use common::model::{Clip, ClipContent, ContentId, MidiContent, Note};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{track_with, AppData};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::widgets::piano_roll::piano_roll;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
const GRID_X: f32 = 56.0;
const GRID_Y: f32 = 44.0;
/// ruler 帯の縦中央 y。
const RULER_Y: f32 = 34.0;
/// velocity lane 内の y (上寄り = 高 velocity)。
const VEL_Y_HIGH: f32 = 545.0;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher;
    let mut app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    // 決定的な pixel→beat 変換のため snap を無効化 (zoom 等は per-clip default = 64/14/84/0)。
    app.cur.view.pianoroll_snap_enabled = false;
    (app, audio_rx, plugin_rx)
}

// ---- 座標 helper ----

/// song-absolute 拍 → grid x。
fn beat_x(beat: f64) -> f32 {
    GRID_X + (beat as f32) * 64.0
}
/// pitch → note rect の縦中央 y。row_h=14、pitch 行は `[44+(84-p)*14, +14)`。
fn pitch_y(pitch: u8) -> f32 {
    GRID_Y + (84.0 - f32::from(pitch)) * 14.0 + 7.0
}

// ---- 入力 helper ----

fn modifiers(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers { ctrl, shift, alt, ..Modifiers::empty() }
}
fn no_mods() -> Modifiers {
    modifiers(false, false, false)
}

fn press(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}
fn hold(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_pressed: true, modifiers: m, ..PointerFrame::default() }
}
fn release(x: f32, y: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_released: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

/// wheel (scroll) フレーム。`(sx, sy)` = scroll_delta。
fn wheel(x: f32, y: f32, sx: f32, sy: f32, m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        scroll_delta: (sx, sy),
        modifiers: m,
        ..PointerFrame::default()
    }
}

fn pointer_input(p: PointerFrame) -> FrameInput {
    let mut input = FrameInput::default();
    input.pointer = p;
    input
}

/// 1 フレーム走らせ、widget が発行した `Edit<AppData>` を `app` に自動 apply する。
/// 描いた scene を返す (描画の検査用)。
fn drive(host: &mut UiHost<AppData>, app: &mut AppData, input: FrameInput) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    // 本番の runner と同じく、毎フレームのパレットを host に渡す (heavy 内の描画が読む)。
    host.set_palette(app.theme.core.clone());
    host.frame(app, &mut scene, screen, input, |app, ui| {
        let _ = piano_roll(app, ui, WIDGET_RECT);
    });
    scene
}
fn drive_pointer(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) {
    drive(host, app, pointer_input(p));
}

fn key(pk: PhysicalKey) -> KeyEvent {
    KeyEvent { state: ElementState::Pressed, text: None, physical_key: pk , repeat: false}
}

// ---- Song setup ----

/// 既定 track を消し、`track_id` の MIDI track + `clip_id` の clip (start 0, len 16, 与えた notes) を
/// 1 本だけ置いて選択する。clip_start=0 なので widget song-absolute 拍 == model clip-local 拍。
fn setup_clip(app: &mut AppData, track_id: u32, clip_id: u32, notes: Vec<Note>) {
    app.edit_song(|song| {
        song.tracks.clear();
        let cid: ContentId = song.alloc_content_id();
        song.clip_contents
            .insert(cid, ClipContent::Midi(MidiContent { notes, ..MidiContent::default() }));
        song.tracks.push(track_with(|t| {
            t.id = track_id;
            t.clips = vec![Clip {
                id: clip_id,
                content_id: cid,
                start_beat: 0.0,
                length_beats: 16.0,
                ..Clip::default()
            }];
        }));
    });
    let key = common::model::ClipKey { track_id, clip_id };
    // per-clip view を **先に** 既定値で登録してから選択する。 未登録のクリップを
    // 初めて開くと `set_clip_selection` が auto-fit してズームが変わり、
    // px → 拍の換算が既定 (ZOOM) からずれる。
    app.cur.view
        .piano_roll_views
        .insert(key, common::model::PianoRollViewState::default());
    // 選択の SSoT は時間範囲 1 本 — クリップ選択はその特殊形として張り直す。
    app.handle_event(daw_gui::app::AppEvent::SetClipSelection(vec![key]));
}

fn mk_note(pitch: u8, start: f64, len: f64, vel: u8) -> Note {
    Note { pitch, start_beat: start, duration_beats: len, velocity: vel, ..Note::default() }
}

fn clip_notes(app: &AppData, track_id: u32, clip_id: u32) -> Vec<Note> {
    let song = app.cur.song_doc.song();
    let t = song.tracks.iter().find(|t| t.id == track_id).expect("track");
    let c = t.clips.iter().find(|c| c.id == clip_id).expect("clip");
    song.clip_notes(c).to_vec()
}

// ============================================================
// note create (Insert shortcut / 空白ダブルクリック)
// ============================================================

#[test]
fn insert_shortcut_adds_note_at_cursor() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![]);
    let mut host = UiHost::<AppData>::no_redraw();
    host.shortcut_map_mut().bind("add_note", "Insert");
    let mut input = pointer_input(PointerFrame {
        pos: Some((beat_x(2.0), pitch_y(60))),
        ..PointerFrame::default()
    });
    input.keyboard = vec![key(PhysicalKey::Insert)];
    drive(&mut host, &mut app, input);
    let notes = clip_notes(&app, 1, 10);
    assert_eq!(notes.len(), 1, "Insert で 1 note 追加: got {}", notes.len());
    assert_eq!(notes[0].pitch, 60, "pitch=60 の行");
    assert!((notes[0].start_beat - 2.0).abs() < 1e-3, "start=2.0: got {}", notes[0].start_beat);
}

#[test]
fn double_click_empty_creates_note() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![]);
    let mut host = UiHost::<AppData>::no_redraw();
    let x = beat_x(3.0);
    let y = pitch_y(64);
    // Frame1: 1 度目 click (release) → UiHost が last_click 記録。
    drive_pointer(&mut host, &mut app, release(x, y, no_mods()));
    assert_eq!(clip_notes(&app, 1, 10).len(), 0, "1 度目 click では作成しない");
    // Frame2: 2 度目 press (double-click 検出) → 作成 session 開始。
    drive_pointer(&mut host, &mut app, press(x, y, no_mods()));
    // Frame3: 放す (ドラッグ無し) → 既定長で AddNote。
    drive_pointer(&mut host, &mut app, release(x, y, no_mods()));
    let notes = clip_notes(&app, 1, 10);
    assert_eq!(notes.len(), 1, "ダブルクリックで 1 note 作成: got {}", notes.len());
    assert_eq!(notes[0].pitch, 64, "pitch=64 の行");
    assert!((notes[0].start_beat - 3.0).abs() < 1e-3, "start=3.0: got {}", notes[0].start_beat);
}

// ============================================================
// note move / resize / delete
// ============================================================

#[test]
fn note_center_drag_moves_note() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]); // x∈[184,248], center 216
    let mut host = UiHost::<AppData>::no_redraw();
    let y = pitch_y(60);
    drive_pointer(&mut host, &mut app, press(216.0, y, no_mods()));
    drive_pointer(&mut host, &mut app, hold(280.0, y, no_mods()));
    drive_pointer(&mut host, &mut app, release(280.0, y, no_mods())); // +64px = +1 拍
    let notes = clip_notes(&app, 1, 10);
    assert!((notes[0].start_beat - 3.0).abs() < 1e-3, "2.0 → 3.0 へ移動: got {}", notes[0].start_beat);
    assert_eq!(notes[0].pitch, 60, "pitch 不変 (水平移動)");
}

#[test]
fn note_right_edge_drag_resizes() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]); // 右端 x=248
    let mut host = UiHost::<AppData>::no_redraw();
    let y = pitch_y(60);
    drive_pointer(&mut host, &mut app, press(247.0, y, no_mods()));
    drive_pointer(&mut host, &mut app, hold(311.0, y, no_mods()));
    drive_pointer(&mut host, &mut app, release(311.0, y, no_mods())); // +64px = +1 拍
    let notes = clip_notes(&app, 1, 10);
    assert!(
        (notes[0].duration_beats - 2.0).abs() < 1e-3,
        "len 1.0 → 2.0: got {}",
        notes[0].duration_beats
    );
}

#[test]
fn delete_shortcut_removes_selected_note() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]);
    // note index 0 (clip_slot 0) = packed id 0 を選択。
    app.handle_event(daw_gui::app::AppEvent::SetNoteSelection(vec![AppData::pack_note_id(0, 0)]));
    let mut host = UiHost::<AppData>::no_redraw();
    host.shortcut_map_mut().bind("delete", "Delete");
    let mut input = pointer_input(PointerFrame {
        pos: Some((216.0, pitch_y(60))),
        ..PointerFrame::default()
    });
    input.keyboard = vec![key(PhysicalKey::Delete)];
    drive(&mut host, &mut app, input);
    assert_eq!(clip_notes(&app, 1, 10).len(), 0, "選択 note が削除される");
}

// ============================================================
// selection (click / marquee replace / union / xor)
// ============================================================

#[test]
fn note_short_click_selects() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]);
    let mut host = UiHost::<AppData>::no_redraw();
    let y = pitch_y(60);
    drive_pointer(&mut host, &mut app, press(216.0, y, no_mods()));
    drive_pointer(&mut host, &mut app, release(217.0, y, no_mods())); // <4px → click
    assert_eq!(
        app.selected_note_ids(),
        vec![AppData::pack_note_id(0, 0)],
        "note id 0 が選択される: got {:?}",
        app.selected_note_ids()
    );
}

#[test]
fn empty_marquee_plain_is_replace() {
    let (mut app, _a, _p) = build_app();
    // 既存選択を置換 (marquee が note を囲む)。note は x[184,248] y(pitch60)。
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]);
    app.handle_event(daw_gui::app::AppEvent::SetNoteSelection(vec![999])); // 存在しない id (置換で消える)
    let mut host = UiHost::<AppData>::no_redraw();
    // 空白 (150,100) から note を囲む矩形へ。
    drive_pointer(&mut host, &mut app, press(150.0, 100.0, no_mods()));
    drive_pointer(&mut host, &mut app, hold(300.0, 400.0, no_mods()));
    drive_pointer(&mut host, &mut app, release(300.0, 400.0, no_mods()));
    assert_eq!(
        app.selected_note_ids(),
        vec![AppData::pack_note_id(0, 0)],
        "REPLACE で note 0 のみ: got {:?}",
        app.selected_note_ids()
    );
}

#[test]
fn 範囲ドラッグは修飾キーに関係なく引き直す() {
    // 選択の SSoT は範囲 1 本なので、ドラッグは常に「引き直し」。 足すのは
    // Ctrl・Shift+**クリック** (`docs/plan_range_selection.md` §3.1)。
    // アレンジャーの範囲ドラッグと同じ規約 (面で操作感が割れない)。
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100), mk_note(67, 6.0, 1.0, 100)]);
    app.handle_event(daw_gui::app::AppEvent::SetNoteSelection(vec![AppData::pack_note_id(0, 1)]));
    let mut host = UiHost::<AppData>::no_redraw();
    for m in [modifiers(false, true, false), modifiers(true, false, false)] {
        // note 0 (x[184,248]) だけを囲む (note1 は x[440,504] で範囲外)。
        drive_pointer(&mut host, &mut app, press(150.0, 100.0, m));
        drive_pointer(&mut host, &mut app, hold(300.0, 400.0, m));
        drive_pointer(&mut host, &mut app, release(300.0, 400.0, m));
        assert_eq!(
            app.selected_note_ids(),
            vec![AppData::pack_note_id(0, 0)],
            "囲んだ note だけになる (UNION / XOR はしない)"
        );
    }
}

#[test]
fn ノートの_ctrl_クリックは範囲を外接まで広げる() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100), mk_note(67, 6.0, 1.0, 100)]);
    app.handle_event(daw_gui::app::AppEvent::SetNoteSelection(vec![AppData::pack_note_id(0, 0)]));
    app.handle_event(daw_gui::app::AppEvent::SelectNote {
        note: AppData::pack_note_id(0, 1),
        additive: true,
    });
    let mut got = app.selected_note_ids();
    got.sort_unstable();
    assert_eq!(
        got,
        vec![AppData::pack_note_id(0, 0), AppData::pack_note_id(0, 1)],
        "2 音とも入る: got {got:?}"
    );
}

// ============================================================
// velocity lane drag
// ============================================================

#[test]
fn velocity_lane_drag_sets_velocity() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 40)]); // bar x = 184
    let mut host = UiHost::<AppData>::no_redraw();
    // vel lane (y 540..600) の bar 上 (x 184) を押して上端付近 (高 velocity) へ。
    drive_pointer(&mut host, &mut app, press(184.0, 590.0, no_mods()));
    drive_pointer(&mut host, &mut app, hold(184.0, VEL_Y_HIGH, no_mods()));
    drive_pointer(&mut host, &mut app, release(184.0, VEL_Y_HIGH, no_mods()));
    let vel = clip_notes(&app, 1, 10)[0].velocity;
    // y=545: t = 1 - (545-540)/60 = 0.9167 → 116。
    assert!(vel > 100, "velocity が上がる (40 → ~116): got {vel}");
}

/// #33 再現: 単一クリップで複数ノートを marquee 選択 → velocity ドラッグ。
/// 選択した全ノートの velocity が変わるべき。「一部しか」再現の観測用。
#[test]
fn repro33_marquee_then_velocity_drag_all_selected() {
    let (mut app, _a, _p) = build_app();
    // pitch 60、beats 2/3/4/5、len 1、初期 vel 40/50/60/70。bar x = 184/248/312/376。
    setup_clip(
        &mut app,
        1,
        10,
        vec![
            mk_note(60, 2.0, 1.0, 40),
            mk_note(60, 3.0, 1.0, 50),
            mk_note(60, 4.0, 1.0, 60),
            mk_note(60, 5.0, 1.0, 70),
        ],
    );
    let mut host = UiHost::<AppData>::no_redraw();
    // marquee: 空白 (100,100) から (460,400) へ。note rect x[184..440] y(pitch60)=[380,394] を全部囲む。
    drive_pointer(&mut host, &mut app, press(100.0, 100.0, no_mods()));
    drive_pointer(&mut host, &mut app, hold(460.0, 400.0, no_mods()));
    drive_pointer(&mut host, &mut app, release(460.0, 400.0, no_mods()));
    let mut sel = app.selected_note_ids();
    sel.sort_unstable();
    eprintln!("selected after marquee = {sel:?}");
    // velocity drag: note1 の bar (x=184) を press → 上端付近 (高 vel) へ。
    drive_pointer(&mut host, &mut app, press(184.0, 590.0, no_mods()));
    drive_pointer(&mut host, &mut app, hold(184.0, VEL_Y_HIGH, no_mods()));
    drive_pointer(&mut host, &mut app, release(184.0, VEL_Y_HIGH, no_mods()));
    let vels: Vec<u8> = clip_notes(&app, 1, 10).iter().map(|n| n.velocity).collect();
    eprintln!("velocities after drag = {vels:?}");
    assert!(
        vels.iter().all(|&v| v > 100),
        "選択した全ノートの velocity が上がるべき: got {vels:?}"
    );
}

/// #33 本命再現: velocity lane はノートを start_beat の x に集約する (pitch 無視) ため、
/// 同じ拍に複数ノート (ハーモニー / 密集) があると、選択中ノートの bar を掴んだつもりでも
/// hit-test が「その x の最前面 (visible 順で最後) の1ノート」を返し、それが選択外だと
/// 選択中ノートが編集されない。選択を優先すべき。
#[test]
fn repro33_velocity_same_beat_prefers_selection() {
    let (mut app, _a, _p) = build_app();
    // 同じ beat 2 に pitch 60 (local 0) と pitch 67 (local 1)。bar は両方 x=184 に重なる。
    setup_clip(
        &mut app,
        1,
        10,
        vec![mk_note(60, 2.0, 1.0, 40), mk_note(67, 2.0, 1.0, 40)],
    );
    let mut host = UiHost::<AppData>::no_redraw();
    // marquee で pitch 60 だけ選択 (y 360..400 は pitch60 rect[380,394] を含み pitch67[282,296] を含まない)。
    drive_pointer(&mut host, &mut app, press(150.0, 360.0, no_mods()));
    drive_pointer(&mut host, &mut app, hold(300.0, 400.0, no_mods()));
    drive_pointer(&mut host, &mut app, release(300.0, 400.0, no_mods()));
    assert_eq!(
        app.selected_note_ids(),
        vec![AppData::pack_note_id(0, 0)],
        "pitch60 (local 0) だけ選択されるべき: got {:?}",
        app.selected_note_ids()
    );
    // velocity drag: 重なった bar (x=184) を掴んで上げる。選択 (pitch60) が編集されるべき。
    drive_pointer(&mut host, &mut app, press(184.0, 590.0, no_mods()));
    drive_pointer(&mut host, &mut app, hold(184.0, VEL_Y_HIGH, no_mods()));
    drive_pointer(&mut host, &mut app, release(184.0, VEL_Y_HIGH, no_mods()));
    let notes = clip_notes(&app, 1, 10);
    let pitch60 = notes.iter().find(|n| n.pitch == 60).unwrap().velocity;
    let pitch67 = notes.iter().find(|n| n.pitch == 67).unwrap().velocity;
    eprintln!("after drag: pitch60={pitch60} pitch67={pitch67}");
    assert!(pitch60 > 100, "選択中の pitch60 の velocity が上がるべき: got {pitch60}");
    assert_eq!(pitch67, 40, "非選択の pitch67 は変わらないべき: got {pitch67}");
}

// ============================================================
// ruler: playhead seek / loop range
// ============================================================

#[test]
fn ruler_plain_click_seeks_playhead() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![]);
    let mut host = UiHost::<AppData>::no_redraw();
    // ruler y=34、beat 2 = x 184。
    drive_pointer(&mut host, &mut app, press(184.0, RULER_Y, no_mods()));
    drive_pointer(&mut host, &mut app, release(184.0, RULER_Y, no_mods()));
    assert!(
        app.cur.transport.playhead_beat.is_some_and(|b| (b - 2.0).abs() < 1e-3),
        "playhead が beat 2.0 へ: got {:?}",
        app.cur.transport.playhead_beat
    );
}

#[test]
fn ruler_shift_drag_sets_loop_range() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![]);
    let mut host = UiHost::<AppData>::no_redraw();
    let m = modifiers(false, true, false);
    // Shift+ruler drag: beat 1 (x120) → beat 4 (x312)。
    drive_pointer(&mut host, &mut app, press(120.0, RULER_Y, m));
    drive_pointer(&mut host, &mut app, hold(312.0, RULER_Y, m));
    drive_pointer(&mut host, &mut app, release(312.0, RULER_Y, m));
    let region = app.cur.transport.loop_region;
    assert!(
        (region.start_beat - 1.0).abs() < 1e-3 && (region.end_beat - 4.0).abs() < 1e-3,
        "loop 範囲 [1,4]: got [{}, {}]",
        region.start_beat,
        region.end_beat
    );
}

// ============================================================
// edge auto-scroll
// ============================================================

// ============================================================
// wheel: 鍵盤レーン上の縦スクロール (#34)
// ============================================================

/// #34: 鍵盤レーン (kbd, x < 56) 上でも plain wheel で pitch 縦スクロールできる。
#[test]
fn wheel_over_keyboard_scrolls_pitch() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![]);
    let mut host = UiHost::<AppData>::no_redraw();
    let before = app.pianoroll_top_pitch();
    // kbd レーン (x=20 < grid.x=56, y=200 は grid/kbd の y 範囲 [44,540] 内) で plain wheel。
    // sy=24 → delta = round(24/12) = 2 → top_pitch += 2。
    drive_pointer(&mut host, &mut app, wheel(20.0, 200.0, 0.0, 24.0, no_mods()));
    let after = app.pianoroll_top_pitch();
    assert_eq!(
        after,
        before.saturating_add(2).min(127),
        "鍵盤レーン上の wheel で pitch がスクロールすべき: before={before} after={after}"
    );
}

/// grid 上の wheel と鍵盤上の wheel が同じ pitch スクロール量になる (領域統一の確認)。
#[test]
fn wheel_over_keyboard_matches_grid() {
    let (mut app_k, _a1, _p1) = build_app();
    setup_clip(&mut app_k, 1, 10, vec![]);
    let mut host_k = UiHost::<AppData>::no_redraw();
    drive_pointer(&mut host_k, &mut app_k, wheel(20.0, 200.0, 0.0, 24.0, no_mods())); // kbd
    let (mut app_g, _a2, _p2) = build_app();
    setup_clip(&mut app_g, 1, 10, vec![]);
    let mut host_g = UiHost::<AppData>::no_redraw();
    drive_pointer(&mut host_g, &mut app_g, wheel(200.0, 200.0, 0.0, 24.0, no_mods())); // grid
    assert_eq!(
        app_k.pianoroll_top_pitch(),
        app_g.pianoroll_top_pitch(),
        "kbd 上と grid 上で同じ pitch スクロール量になるべき"
    );
}

#[test]
fn edge_autoscroll_horizontal_on_note_drag() {
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]);
    let mut host = UiHost::<AppData>::no_redraw();
    let y = pitch_y(60);
    // note 中央 press (Move 開始)。
    drive_pointer(&mut host, &mut app, press(216.0, y, no_mods()));
    // 右端 hot-zone へ移動 (press から十分離れて gate 通過) → 前方スクロール。
    drive_pointer(&mut host, &mut app, hold(796.0, y, no_mods()));
    assert!(
        app.pianoroll_scroll_beat() > 0.0,
        "右端 drag で前方 (拍増) へスクロール: got {}",
        app.pianoroll_scroll_beat()
    );
    drive_pointer(&mut host, &mut app, release(796.0, y, no_mods()));
}

// ============================================================
// r.md #157: コード帯 (ルーラー直下) / 色分け / 鳴る音の枠
// ============================================================

/// コード帯の高さ (`view_build::CHORD_LANE_H`)。コードがあるとき grid はこの分だけ下がる。
const CHORD_LANE_H: f32 = 18.0;

fn place_chord(app: &mut AppData, start: f64, len: f64, chord: common::model::Chord, color: Option<[f32; 3]>) {
    app.edit_song(|s| {
        s.place_chord(common::model::ChordEvent { id: 0, start_beat: start, len_beats: len, chord, color });
    });
}

/// scene の中から `text` を描いた glyph と、それより前に描かれた「glyph の位置を含む最後の不透明な塗り」
/// (= 文字の実際の背景) を返す。
fn glyph_and_backdrop(scene: &Scene, text: &str) -> (daw_ui_renderer::GlyphArea, daw_ui_renderer::Color) {
    use daw_ui_renderer::Primitive;
    let at = scene
        .primitives
        .iter()
        .position(|p| matches!(p, Primitive::Glyph(g) if &*g.text == text))
        .unwrap_or_else(|| panic!("glyph {text:?} が描かれていない"));
    let Primitive::Glyph(g) = &scene.primitives[at] else { unreachable!() };
    let fill = scene.primitives[..at]
        .iter()
        .filter_map(|p| match p {
            Primitive::Rect(r) if r.fill.a > 0.99 && r.rect.contains(g.left + 1.0, g.top + 4.0) => Some(r.fill),
            _ => None,
        })
        .last()
        .expect("glyph の下に塗りがある");
    (g.clone(), fill)
}

/// コード帯はルーラーの直下に出て、箱の名前は塗り (既定色 / 明るいユーザー色 / 暗いユーザー色) の上で
/// ダーク・ライトどちらのテーマでも読める。コードが 1 つも無ければ帯は出ない (grid は動かない)。
#[test]
fn chord_lane_shows_readable_chord_names_under_the_ruler() {
    use common::model::{Chord, ChordKind};
    for theme in ["dark", "light"] {
        for color in [None, Some([0.95, 0.92, 0.55]), Some([0.06, 0.06, 0.10])] {
            let (mut app, _a, _p) = build_app();
            app.handle_event(daw_gui::app::AppEvent::SetTheme(theme.to_string()));
            setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]);
            place_chord(&mut app, 0.0, 4.0, Chord::new(9, ChordKind::Minor), color);
            let mut host = UiHost::<AppData>::no_redraw();
            let scene = drive(&mut host, &mut app, FrameInput::default());
            let (g, fill) = glyph_and_backdrop(&scene, "Am");
            assert!(
                g.top >= GRID_Y && g.top < GRID_Y + CHORD_LANE_H,
                "{theme}/{color:?}: 名前はルーラー直下の帯に出る: top={}",
                g.top
            );
            // 既定色は本文の 4.5:1、ユーザー色 (中間の明度もありうる) は可変背景の標識の 3:1 (`ink_for`)。
            let need = if color.is_none() { 4.5 } else { 3.0 };
            let ratio = daw_ui_core::theme::contrast_ratio(g.color, fill);
            assert!(ratio >= need, "{theme}/{color:?}: 名前と箱のコントラスト {ratio:.2} >= {need}");
        }
    }

    // コードが無ければ帯は出ず、grid は従来位置 (y = 44) のまま: 既存のノートを掴める。
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]);
    let mut host = UiHost::<AppData>::no_redraw();
    drive_pointer(&mut host, &mut app, press(216.0, pitch_y(60), no_mods()));
    drive_pointer(&mut host, &mut app, release(217.0, pitch_y(60), no_mods()));
    assert_eq!(app.selected_note_ids(), vec![AppData::pack_note_id(0, 0)]);

    // コードがあれば grid は帯の分だけ下がり、ノートの当たり判定も一緒に下がる。
    let (mut app, _a, _p) = build_app();
    setup_clip(&mut app, 1, 10, vec![mk_note(60, 2.0, 1.0, 100)]);
    place_chord(&mut app, 0.0, 4.0, Chord::new(0, ChordKind::Major), None);
    let mut host = UiHost::<AppData>::no_redraw();
    let y = pitch_y(60) + CHORD_LANE_H;
    drive_pointer(&mut host, &mut app, press(216.0, y, no_mods()));
    drive_pointer(&mut host, &mut app, release(217.0, y, no_mods()));
    assert_eq!(app.selected_note_ids(), vec![AppData::pack_note_id(0, 0)]);
}

/// コードがあるときの、`pitch` の行の上端 y (grid は帯の分だけ下がる)。
fn chord_row_top(pitch: u8) -> f32 {
    GRID_Y + CHORD_LANE_H + (84.0 - f32::from(pitch)) * 14.0
}

/// 左上が `(x, y)` (±0.5px) の矩形を描いた順に返す。
fn rects_at(scene: &Scene, x: f32, y: f32) -> Vec<daw_ui_renderer::RectCommand> {
    scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            daw_ui_renderer::Primitive::Rect(r) if (r.rect.x - x).abs() < 0.5 && (r.rect.y - y).abs() < 0.5 => {
                Some(r.clone())
            }
            _ => None,
        })
        .collect()
}

/// 色分け: Am の箱の上で、構成音 (A) は左端の帯、どちらでもない音 (C#) は輪郭、スケール内 (B) は
/// 何も足さない。標識はノートの塗り (明るいトラック色 / 暗いトラック色) に対して 3:1 以上で読める。
/// ガイドは箱の時間範囲の構成音の行を塗り、grid の地との差が見える。「Chord」を切ると両方消える。
#[test]
fn chord_fit_marks_and_guide_rows_contrast_on_light_and_dark_clips() {
    use common::model::{Chord, ChordKind};
    let (a, b, c_sharp) = (69_u8, 71_u8, 61_u8);
    for theme in ["dark", "light"] {
        for color in [[0.95, 0.90, 0.40], [0.05, 0.05, 0.12]] {
            let (mut app, _a, _p) = build_app();
            app.handle_event(daw_gui::app::AppEvent::SetTheme(theme.to_string()));
            setup_clip(
                &mut app,
                1,
                10,
                vec![mk_note(a, 0.0, 1.0, 127), mk_note(b, 1.0, 1.0, 127), mk_note(c_sharp, 2.0, 1.0, 127)],
            );
            app.edit_song(|s| s.tracks[0].color = Some(color));
            place_chord(&mut app, 0.0, 4.0, Chord::new(9, ChordKind::Minor), None);
            let mut host = UiHost::<AppData>::no_redraw();
            let scene = drive(&mut host, &mut app, FrameInput::default());
            let tag = format!("{theme}/{color:?}");
            let note_fill = |rs: &[daw_ui_renderer::RectCommand]| {
                rs.iter().find(|r| (r.rect.w - 64.0).abs() < 0.5 && r.fill.a > 0.99).expect("ノートの塗り").fill
            };

            let at_a = rects_at(&scene, beat_x(0.0), chord_row_top(a));
            let bar = at_a.iter().find(|r| r.rect.w <= 3.0 && r.fill.a > 0.99).expect("構成音の帯");
            let ratio = daw_ui_core::theme::contrast_ratio(bar.fill, note_fill(&at_a));
            assert!(ratio >= 3.0, "{tag}: 構成音の帯とノートのコントラスト {ratio:.2}");

            let at_b = rects_at(&scene, beat_x(1.0), chord_row_top(b));
            assert!(
                !at_b.iter().any(|r| r.rect.w <= 3.0 || (r.border_width - 1.5).abs() < 0.01),
                "{tag}: スケール内の音には標識を足さない"
            );

            let at_cs = rects_at(&scene, beat_x(2.0), chord_row_top(c_sharp));
            let ring = at_cs.iter().find(|r| (r.border_width - 1.5).abs() < 0.01).expect("外れた音の輪郭");
            let ratio = daw_ui_core::theme::contrast_ratio(ring.border, note_fill(&at_cs));
            assert!(ratio >= 3.0, "{tag}: 外れた音の輪郭とノートのコントラスト {ratio:.2}");

            // ガイド: 箱 (拍 0..4) の E の行 (ノートの無い構成音の行) が塗られ、地との差が見える。
            let bg = daw_gui::widgets::piano_roll::PianoRollStyle::from_theme(&app.theme).bg;
            let guide = rects_at(&scene, GRID_X, chord_row_top(64))
                .into_iter()
                .find(|r| r.fill.a > 0.0 && r.fill.a < 0.5 && (r.rect.w - 4.0 * 64.0).abs() < 0.5)
                .expect("E の行のガイド");
            let shown = daw_ui_core::color::composite_over(guide.fill, bg);
            let ratio = daw_ui_core::theme::contrast_ratio(shown, bg);
            assert!(ratio >= 1.15, "{tag}: ガイドの行と地の差 {ratio:.3}");

            // 「Chord」を切ると標識もガイドも消える。
            app.cur.view.piano_roll_chord_guide = false;
            let scene = drive(&mut host, &mut app, FrameInput::default());
            assert!(!rects_at(&scene, beat_x(0.0), chord_row_top(a)).iter().any(|r| r.rect.w <= 3.0));
            assert!(!rects_at(&scene, GRID_X, chord_row_top(64)).iter().any(|r| (r.rect.w - 256.0).abs() < 0.5));
        }
    }
}

/// 鳴る音の薄い枠: 追従するトラック (最寄りのコード音) で G を Am の箱の上に書くと、鳴る A の行に枠が出る
/// (書いた G はそのまま)。枠線は grid の地に対して 3:1 以上。追従しないトラックには出ない。
#[test]
fn following_track_overlays_the_sounding_note_frame() {
    use common::model::{Chord, ChordFollow, ChordKind};
    for theme in ["dark", "light"] {
        for follow in [ChordFollow::Narrow, ChordFollow::Off] {
            let (mut app, _a, _p) = build_app();
            app.handle_event(daw_gui::app::AppEvent::SetTheme(theme.to_string()));
            setup_clip(&mut app, 1, 10, vec![mk_note(67, 0.0, 1.0, 100)]);
            app.edit_song(|s| s.tracks[0].follow_chords = follow);
            place_chord(&mut app, 0.0, 4.0, Chord::new(9, ChordKind::Minor), None);
            let mut host = UiHost::<AppData>::no_redraw();
            let scene = drive(&mut host, &mut app, FrameInput::default());
            let frame = rects_at(&scene, beat_x(0.0), chord_row_top(69))
                .into_iter()
                .find(|r| r.fill.a < 0.5 && r.border_width > 0.0);
            if follow.is_off() {
                assert!(frame.is_none(), "{theme}: 追従しないトラックには枠を出さない");
                continue;
            }
            let frame = frame.expect("鳴る A の行に枠");
            let bg = daw_gui::widgets::piano_roll::PianoRollStyle::from_theme(&app.theme).bg;
            let ratio =
                daw_ui_core::theme::contrast_ratio(daw_ui_core::color::composite_over(frame.border, bg), bg);
            assert!(ratio >= 3.0, "{theme}: 枠線と地のコントラスト {ratio:.2}");
            assert_eq!(clip_notes(&app, 1, 10)[0].pitch, 67, "書いた音は変わらない");
        }
    }
}
