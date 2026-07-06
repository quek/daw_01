//! S4b: arrangement widget を `AppData` 直結・`Edit<AppData>` 直発行に移設した後の
//! interaction 回帰テスト (旧 `arr_*.rs` の mirror + EditRequest 記録方式を置換)。
//!
//! ドライブ手順: `AppData` に Song を組み、`UiHost::<AppData>::frame` で press → (hold) →
//! release のフレーム列を `arrangement(app, ui, rect)` に流す。`UiHost::frame` は widget が
//! 発行した `Edit<AppData>` を `app` に自動 apply するので、**適用後の app 状態**を assert する。
//!
//! 幾何 (widget 固定レイアウト): ruler=20px / arranger 帯=18px (y∈[20,38)) / lanes は y≥38。
//! `arrange_header_w=0` / `arrange_zoom_x=64` (1 拍=64px) / `arrange_scroll_beat=0` / snap 無効で
//! beat→x = beat*64。 master row が lanes 先頭に 1 行入るので track 0 は `38 + row_h` から。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use common::model::{Clip, ClipContent, ContentId, MidiContent, Note, Section};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{track_with, AppData};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::widgets::arrangement::arrangement;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
const ZOOM: f32 = 64.0;
const ROW_H: f32 = 50.0;
/// 帯 (arranger lane) の縦中央 y。
const SEC_Y: f32 = 29.0;

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
    // 決定的な pixel→beat 変換のため view 由来の ui_prefs を固定。
    app.ui_prefs.arrange_header_w = 0.0;
    app.ui_prefs.arrange_zoom_x = ZOOM;
    app.ui_prefs.arrange_scroll_beat = 0.0;
    app.ui_prefs.arrange_track_row_h = ROW_H;
    app.ui_prefs.arrange_track_top = 0.0;
    app.ui_prefs.arrange_snap_enabled = false;
    (app, audio_rx, plugin_rx)
}

fn modifiers(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers { ctrl, shift, alt, ..Modifiers::empty() }
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

fn frame(p: PointerFrame) -> FrameInput {
    let mut input = FrameInput::default();
    input.pointer = p;
    input
}

/// 1 フレーム走らせ、widget が発行した `Edit<AppData>` を `app` に自動 apply する。
fn drive(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    host.frame(app, &mut scene, screen, frame(p), |app, ui| {
        let _ = arrangement(app, ui, WIDGET_RECT);
    });
}

fn no_mods() -> Modifiers {
    modifiers(false, false, false)
}

// ============================================================
// section (arranger 帯) drag / click
// ============================================================

fn add_section(app: &mut AppData, id: u32, start: f64, len: f64) {
    app.edit_song(|song| {
        song.sections.push(Section {
            id,
            name: "A".to_string(),
            color: [0.3, 0.4, 0.5],
            start_beat: start,
            len_beats: len,
        });
    });
}

fn section_start(app: &AppData, id: u32) -> Option<f64> {
    app.song_doc.song().sections.iter().find(|s| s.id == id).map(|s| s.start_beat)
}

/// 帯中央 drag → 破壊的 move。dest が帯の外 (左方向) なら observable に移動する
/// (`Song::move_section` の ripple 仕様: dest が自帯範囲内なら no-op なので、外へ動かす)。
#[test]
fn section_center_drag_moves_section() {
    let (mut app, _a, _p) = build_app();
    add_section(&mut app, 7, 8.0, 2.0); // [8,10): x∈[512,640], center 576 (beat 9)
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(576.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, hold(400.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, release(192.0, SEC_Y, no_mods())); // beat 3 → delta -6 拍
    // anchor_start 8 + (-6) = 2、dest(2) <= a(8) なので section は 2.0 へ落ちる。
    assert!(
        section_start(&app, 7).is_some_and(|s| (s - 2.0).abs() < 1e-3),
        "section 7 は 8.0 → 2.0 へ移動: got {:?}",
        section_start(&app, 7)
    );
}

/// 帯右端 drag → resize (start 固定・len 変化、`apply_resize_section` は直接 set)。
#[test]
fn section_right_edge_drag_resizes() {
    let (mut app, _a, _p) = build_app();
    add_section(&mut app, 7, 2.0, 2.0); // [2,4): 右端 x=256
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(256.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, hold(288.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, release(320.0, SEC_Y, no_mods())); // +64px = +1 拍
    let s = app.song_doc.song().sections.iter().find(|s| s.id == 7).cloned();
    assert!(
        s.as_ref().is_some_and(|s| (s.start_beat - 2.0).abs() < 1e-3 && (s.len_beats - 3.0).abs() < 1e-3),
        "start 2.0 固定・len 2.0→3.0: got {s:?}"
    );
}

/// 空きレーンの範囲 drag → 新規 section 作成。
#[test]
fn section_empty_range_drag_creates_section() {
    let (mut app, _a, _p) = build_app();
    let before = app.song_doc.song().sections.len();
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(64.0, SEC_Y, no_mods())); // beat 1.0
    drive(&mut host, &mut app, hold(200.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, release(320.0, SEC_Y, no_mods())); // beat 5.0 → len 4.0
    let secs = &app.song_doc.song().sections;
    assert_eq!(secs.len(), before + 1, "1 件作成される");
    let s = secs.last().unwrap();
    assert!((s.start_beat - 1.0).abs() < 1e-3, "start 1.0: got {}", s.start_beat);
    assert!((s.len_beats - 4.0).abs() < 1e-3, "len 4.0: got {}", s.len_beats);
}

/// 帯中央の短 click → playhead ジャンプ (section.start) + 選択。
#[test]
fn section_short_click_jumps_and_selects() {
    let (mut app, _a, _p) = build_app();
    add_section(&mut app, 7, 2.0, 4.0);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(256.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, release(258.0, SEC_Y, no_mods())); // 2px < 4px = click
    assert!(
        app.transport.playhead_beat.is_some_and(|b| (b - 2.0).abs() < 1e-3),
        "playhead が section.start=2.0 へ: got {:?}",
        app.transport.playhead_beat
    );
    assert!(
        app.selection.selected_section_ids.contains(&7),
        "section 7 が選択される: got {:?}",
        app.selection.selected_section_ids
    );
}

// ============================================================
// clip drag / select (track 0 は master row の下 = y∈[38+ROW_H, 38+2*ROW_H))
// ============================================================

/// track 0 上の clip の縦中央 y。master row 1 行 (ROW_H) を挟む。
fn track0_y() -> f32 {
    38.0 + ROW_H + ROW_H * 0.5
}

fn add_midi_track_with_clip(app: &mut AppData, track_id: u32, clip_id: u32, start: f64, len: f64) {
    app.edit_song(|song| {
        // AppData::new の既定 "Track 1" を除去し、追加 track を row 0 (master 直下) に固定する。
        song.tracks.clear();
        let cid: ContentId = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    pitch: 60,
                    start_beat: 0.0,
                    duration_beats: 1.0,
                    velocity: 100,
                    ..Note::default()
                }],
                ..MidiContent::default()
            }),
        );
        song.tracks.push(track_with(|t| {
            t.id = track_id;
            t.clips = vec![Clip {
                id: clip_id,
                content_id: cid,
                start_beat: start,
                length_beats: len,
                ..Clip::default()
            }];
        }));
    });
}

fn clip_start(app: &AppData, track_id: u32, clip_id: u32) -> Option<f64> {
    app.song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == track_id)
        .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
        .map(|c| c.start_beat)
}

/// clip 中央 drag → 位置移動 (start_beat が移動先へ)。
#[test]
fn clip_center_drag_moves_clip() {
    let (mut app, _a, _p) = build_app();
    add_midi_track_with_clip(&mut app, 1, 10, 2.0, 4.0); // x∈[128,384], center 256
    let y = track0_y();
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(256.0, y, no_mods()));
    drive(&mut host, &mut app, hold(320.0, y, no_mods()));
    drive(&mut host, &mut app, release(384.0, y, no_mods())); // +128px = +2 拍
    assert!(
        clip_start(&app, 1, 10).is_some_and(|s| (s - 4.0).abs() < 1e-3),
        "clip は 2.0 → 4.0 へ移動: got {:?}",
        clip_start(&app, 1, 10)
    );
}

/// clip 右端 drag → resize (length が伸びる)。
#[test]
fn clip_right_edge_drag_resizes() {
    let (mut app, _a, _p) = build_app();
    add_midi_track_with_clip(&mut app, 1, 10, 2.0, 4.0); // 右端 x=384
    let y = track0_y();
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(384.0, y, no_mods()));
    drive(&mut host, &mut app, hold(416.0, y, no_mods()));
    drive(&mut host, &mut app, release(448.0, y, no_mods())); // +64px = +1 拍
    let len = app
        .song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == 1)
        .and_then(|t| t.clips.iter().find(|c| c.id == 10))
        .map(|c| c.length_beats);
    assert!(len.is_some_and(|l| (l - 5.0).abs() < 1e-3), "len 4.0 → 5.0: got {len:?}");
}

/// clip 短 click → その clip が選択される。
#[test]
fn clip_short_click_selects() {
    let (mut app, _a, _p) = build_app();
    add_midi_track_with_clip(&mut app, 1, 10, 2.0, 4.0);
    let y = track0_y();
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(256.0, y, no_mods()));
    drive(&mut host, &mut app, release(258.0, y, no_mods()));
    assert!(
        app.selection.selected_clips.iter().any(|k| k.track_id == 1 && k.clip_id == 10),
        "clip (1,10) が選択される: got {:?}",
        app.selection.selected_clips
    );
}

/// r.md #14 回帰: context menu (popup) が開いている間の click は、 背景の
/// arrangement に届いても clip 選択をクリアしない。 修正前は「Make Unique」等の
/// menu item click が空きレーン click と誤判定され、 実行前に複数選択が消えていた。
#[test]
fn popup_open_click_does_not_clear_clip_selection() {
    let (mut app, _a, _p) = build_app();
    add_midi_track_with_clip(&mut app, 1, 10, 2.0, 4.0);
    let y = track0_y();
    let mut host = UiHost::no_redraw();
    // まず clip を選択。
    drive(&mut host, &mut app, press(256.0, y, no_mods()));
    drive(&mut host, &mut app, release(258.0, y, no_mods()));
    assert!(!app.selection.selected_clips.is_empty(), "precondition: clip 選択済み");

    // popup を開いた状態で「空きレーン上の click」相当 (clip の無い x) を送る。
    // ガードが効いていれば選択はクリアされない。
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let empty_x = 700.0; // clip [2,4) (x∈[128,256]) の外
    for p in [press(empty_x, y, no_mods()), release(empty_x, y, no_mods())] {
        let mut scene = Scene::new();
        host.frame(&mut app, &mut scene, screen, frame(p), |app, ui| {
            // context menu が開いている状況を模す (毎フレーム open で open_popups を維持)。
            ui.open_popup("test_ctx_menu", WIDGET_RECT, true);
            let _ = arrangement(app, ui, WIDGET_RECT);
        });
    }
    assert!(
        !app.selection.selected_clips.is_empty(),
        "popup が開いている間の click では clip 選択がクリアされない (r.md #14)"
    );
}

