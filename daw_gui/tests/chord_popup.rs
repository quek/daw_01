//! r.md #157 Q4: コードのポップアップの操作 (窓を出さない)。arrangement widget と
//! `view::chord_popup::draw` を `arrangement_view::draw` と同じ順に毎フレーム回し、打鍵 / 矢印 / Tab を
//! `FrameInput::keyboard` で流す。shortcut map は本番と同じ (`daw_shortcut_map`) — Tab / 矢印が
//! shortcut 層に食われないことも一緒に見る。
//!
//! 幾何は `chord_band_widget.rs` と同じ (コードの帯 y∈[38,58)、1 拍 = 64px)。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use common::model::{Chord, ChordEvent, ChordKind};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::AppData;
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::widgets::arrangement::arrangement;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{ElementState, KeyEvent, PhysicalKey, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: -12.0, y: 0.0, w: 812.0, h: 600.0 };
const ZOOM: f32 = 64.0;
const LANE_Y: f32 = 46.0;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app =
        AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
    app.cur.view.arrange_header_w = 0.0;
    app.cur.view.arrange_zoom_x = ZOOM;
    app.cur.view.arrange_scroll_beat = 0.0;
    app.cur.view.arrange_snap_enabled = false;
    app.cur.view.launcher_layout = common::model::LauncherLayout::ArrangerOnly;
    (app, audio_rx, plugin_rx)
}

fn host() -> UiHost<AppData> {
    let mut host = UiHost::no_redraw();
    *host.shortcut_map_mut() = daw_gui::view::shortcuts::daw_shortcut_map();
    host
}

fn run(host: &mut UiHost<AppData>, app: &mut AppData, pointer: PointerFrame, keyboard: Vec<KeyEvent>) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 800, height: 600 };
    let input = FrameInput { pointer, keyboard, ..FrameInput::default() };
    host.frame(app, &mut scene, screen, input, |app, ui| {
        let _ = arrangement(app, ui, WIDGET_RECT);
        daw_gui::view::chord_popup::draw(app, ui);
    });
}

fn idle(host: &mut UiHost<AppData>, app: &mut AppData) {
    run(host, app, PointerFrame { pos: Some((700.0, 500.0)), ..PointerFrame::default() }, Vec::new());
}

fn key(k: PhysicalKey) -> KeyEvent {
    KeyEvent { state: ElementState::Pressed, text: None, physical_key: k, repeat: false }
}

fn typed(text: &str) -> Vec<KeyEvent> {
    text.chars()
        .map(|c| KeyEvent { state: ElementState::Pressed, text: Some(c.to_string()), physical_key: PhysicalKey::Other(0), repeat: false })
        .collect()
}

fn click(host: &mut UiHost<AppData>, app: &mut AppData, x: f32, y: f32) {
    let p = PointerFrame { pos: Some((x, y)), primary_just_pressed: true, primary_pressed: true, ..PointerFrame::default() };
    run(host, app, p, Vec::new());
    let r = PointerFrame { pos: Some((x, y)), primary_just_released: true, ..PointerFrame::default() };
    run(host, app, r, Vec::new());
}

fn place(app: &mut AppData, start: f64, len: f64, chord: Chord) -> u32 {
    let mut id = 0;
    app.edit_song(|song| {
        id = song.place_chord(ChordEvent::new(start, len, chord)).unwrap();
    });
    id
}

/// 箱をダブルクリックしてポップアップを開く (開いたフレームで入力欄が focus を取る)。
fn open_on(host: &mut UiHost<AppData>, app: &mut AppData, beat: f32) {
    idle(host, app);
    click(host, app, beat * ZOOM, LANE_Y);
    click(host, app, beat * ZOOM, LANE_Y);
    idle(host, app);
    idle(host, app);
    assert!(app.cur.peph.chord_band.popup.is_some(), "ダブルクリックでポップアップが開く");
}

fn chord_of(app: &AppData, id: u32) -> Option<Chord> {
    app.cur.song_doc.song().chord_by_id(id).map(|c| c.chord)
}

/// `Am7` と打って Enter → 箱が Am7 になり、ポップアップは閉じる。読めない文字列の Enter は確定しない。
#[test]
fn typing_a_name_and_enter_sets_the_chord() {
    let (mut app, _a, _p) = build_app();
    let id = place(&mut app, 0.0, 4.0, Chord::new(0, ChordKind::Major));
    let mut h = host();
    open_on(&mut h, &mut app, 1.0);
    run(&mut h, &mut app, PointerFrame::default(), typed("Xq"));
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::Enter)]);
    assert_eq!(chord_of(&app, id), Some(Chord::new(0, ChordKind::Major)), "読めない名前では変えない");
    assert!(app.cur.peph.chord_band.popup.is_some(), "読めない間は開いたまま");
    // 入力欄を作り直して (全選択で) 打ち直す。
    app.cur.peph.chord_band.popup = None;
    open_on(&mut h, &mut app, 1.0);
    run(&mut h, &mut app, PointerFrame::default(), typed("Am7"));
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::Enter)]);
    assert_eq!(chord_of(&app, id), Some(Chord::new(9, ChordKind::Minor7)));
    idle(&mut h, &mut app);
    assert!(app.cur.peph.chord_band.popup.is_none(), "Enter で閉じる");
}

fn popup_box(app: &AppData) -> Option<u32> {
    app.cur.peph.chord_band.popup.as_ref().and_then(|p| p.box_id())
}

/// → で隣の箱へ移り、最後の箱で Tab を押すと同じ長さの置く前の箱が後ろに開き、Enter でそこに置かれる。
#[test]
fn arrows_move_between_boxes_and_tab_appends() {
    let (mut app, _a, _p) = build_app();
    let a = place(&mut app, 0.0, 2.0, Chord::new(0, ChordKind::Major));
    let b = place(&mut app, 2.0, 2.0, Chord::new(7, ChordKind::Major));
    let mut h = host();
    open_on(&mut h, &mut app, 1.0);
    assert_eq!(popup_box(&app), Some(a));
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::ArrowRight)]);
    idle(&mut h, &mut app);
    assert_eq!(popup_box(&app), Some(b), "→ で隣の箱");
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::Tab)]);
    idle(&mut h, &mut app);
    assert_eq!(app.cur.song_doc.song().chords.len(), 2, "r.md #159: Tab の行き先は選ぶまで曲に入らない");
    let draft = app.cur.peph.chord_band.popup.as_ref().and_then(|p| p.draft()).expect("Tab で置く前の箱が開く");
    assert_eq!((draft.start_beat, draft.len_beats), (4.0, 2.0));
    assert_eq!(draft.chord, Chord::new(0, ChordKind::Major), "G の次の候補の先頭 (C) を出す");
    assert_eq!(app.cur.view.launcher_layout, common::model::LauncherLayout::ArrangerOnly, "Tab がランチャー帯の巡回に食われない");
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::Enter)]);
    idle(&mut h, &mut app);
    let song = app.cur.song_doc.song();
    assert_eq!(song.chords.len(), 3, "Enter で出しているコードに決めて置く");
    let added = &song.chords[2];
    assert_eq!((added.start_beat, added.len_beats, added.chord), (4.0, 2.0, Chord::new(0, ChordKind::Major)));
}

/// r.md #159: 空きのダブルクリックで開いた置く前の箱は、コードを選んだ時点で置かれる (Enter = 出しているコード、
/// 打った名前ならその名前)。置くのは 1 undo step。
#[test]
fn choosing_a_chord_places_the_draft() {
    let (mut app, _a, _p) = build_app();
    let mut h = host();
    let depth = app.cur.song_doc.undo_depth();
    open_on(&mut h, &mut app, 2.0);
    assert!(app.cur.song_doc.song().chords.is_empty(), "開いただけでは置かない");
    run(&mut h, &mut app, PointerFrame::default(), typed("Am7"));
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::Enter)]);
    idle(&mut h, &mut app);
    let song = app.cur.song_doc.song();
    assert_eq!(song.chords.len(), 1);
    assert_eq!((song.chords[0].start_beat, song.chords[0].chord), (2.0, Chord::new(9, ChordKind::Minor7)));
    assert_eq!(app.cur.selection.selected_chord_ids, vec![song.chords[0].id]);
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1);
    assert!(app.cur.peph.chord_band.popup.is_none());
}

/// r.md #159: 置く前の箱は、選ばずに閉じる (Esc) / ←→ で離れると何も残さない (箱も undo step も `*` も)。
#[test]
fn leaving_a_draft_without_choosing_leaves_nothing() {
    let (mut app, _a, _p) = build_app();
    let a = place(&mut app, 0.0, 2.0, Chord::new(0, ChordKind::Major));
    app.cur.song_doc.mark_saved();
    let depth = app.cur.song_doc.undo_depth();
    let mut h = host();
    open_on(&mut h, &mut app, 4.0);
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::Escape)]);
    idle(&mut h, &mut app);
    assert!(app.cur.peph.chord_band.popup.is_none(), "Esc で閉じる");
    assert_eq!(app.cur.song_doc.song().chords.len(), 1, "閉じたら置く前の箱は残らない");
    open_on(&mut h, &mut app, 4.0);
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::ArrowLeft)]);
    idle(&mut h, &mut app);
    assert_eq!(popup_box(&app), Some(a), "← で前の箱へ");
    assert_eq!(app.cur.song_doc.song().chords.len(), 1, "離れたら置く前の箱は残らない");
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    assert!(!app.cur.song_doc.is_dirty());
}

/// Instrument Input: 開く前に弾いた和音は拾わず、開いている間に弾き終えた和音 (通し番号が進む) は箱に入る。
#[test]
fn chord_played_on_the_keyboard_while_open_is_set() {
    let (mut app, _a, _p) = build_app();
    let id = place(&mut app, 0.0, 4.0, Chord::new(0, ChordKind::Major));
    app.cur.chord_tools.played = Some((Chord::new(2, ChordKind::Minor), 1));
    let mut h = host();
    open_on(&mut h, &mut app, 1.0);
    assert_eq!(chord_of(&app, id), Some(Chord::new(0, ChordKind::Major)), "開く前に弾いたものは拾わない");
    app.cur.chord_tools.played = Some((Chord::new(7, ChordKind::Dominant7), 2));
    idle(&mut h, &mut app);
    assert_eq!(chord_of(&app, id), Some(Chord::new(7, ChordKind::Dominant7)));
    idle(&mut h, &mut app);
    assert!(app.cur.peph.chord_band.popup.is_some(), "弾いても開いたまま (続けて弾ける)");
}

/// Esc で閉じる (箱は変わらない)。
#[test]
fn escape_closes_the_popup() {
    let (mut app, _a, _p) = build_app();
    let id = place(&mut app, 0.0, 4.0, Chord::new(0, ChordKind::Major));
    let mut h = host();
    open_on(&mut h, &mut app, 1.0);
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::Escape)]);
    idle(&mut h, &mut app);
    assert!(app.cur.peph.chord_band.popup.is_none(), "Esc で閉じる");
    assert_eq!(chord_of(&app, id), Some(Chord::new(0, ChordKind::Major)));
}

/// ↓ で候補を巡回して箱に入れる (先頭から)。
#[test]
fn down_arrow_cycles_the_candidates() {
    let (mut app, _a, _p) = build_app();
    place(&mut app, 0.0, 2.0, Chord::new(7, ChordKind::Major));
    let b = place(&mut app, 2.0, 2.0, Chord::new(2, ChordKind::Minor));
    let mut h = host();
    open_on(&mut h, &mut app, 3.0);
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::ArrowDown)]);
    idle(&mut h, &mut app);
    // G の次 (キー無し → G メジャーを推す) の先頭は 5 度下の C。
    assert_eq!(chord_of(&app, b), Some(Chord::new(0, ChordKind::Major)));
    assert_eq!(app.cur.peph.chord_band.popup.as_ref().and_then(|p| p.candidate), Some(0));
}

/// ポップアップの地 (ボタンでも入力欄でもない所) を押しても、↑↓ はポップアップの候補の巡回のまま (入力欄が
/// keyboard を持ち続ける)。r.md #159: 外れると ↑↓ が選んだ箱の移調 (書き換え) に落ち、置いている最中の箱の下の
/// メロディーが動いた。
#[test]
fn clicking_the_popup_background_keeps_the_arrows_on_the_popup() {
    let (mut app, _a, _p) = build_app();
    place(&mut app, 0.0, 2.0, Chord::new(7, ChordKind::Major));
    let b = place(&mut app, 2.0, 2.0, Chord::new(2, ChordKind::Minor));
    let mut h = host();
    open_on(&mut h, &mut app, 3.0);
    let lane = app.cur.peph.chord_band.lane;
    // ポップアップの左下の隅 (ヒントの行より下の余白)。
    let (x, y) = (lane.x_of(2.0).max(lane.rect.x) + 4.0, lane.rect.y + lane.rect.h + 2.0 + 264.0);
    click(&mut h, &mut app, x, y);
    idle(&mut h, &mut app);
    assert!(app.cur.peph.chord_band.popup.is_some(), "中を押しても閉じない");
    run(&mut h, &mut app, PointerFrame::default(), vec![key(PhysicalKey::ArrowDown)]);
    idle(&mut h, &mut app);
    assert_eq!(chord_of(&app, b), Some(Chord::new(0, ChordKind::Major)), "↓ は候補の巡回のまま");
    assert_eq!(app.cur.peph.chord_band.popup.as_ref().and_then(|p| p.candidate), Some(0));
}
