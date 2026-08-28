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

use common::model::{
    AudioContent, AudioEvent, AutomationClip, AutomationContent, AutomationCurve, AutomationLane,
    AutomationPoint, AutomationTarget, Clip, ClipContent, ContentId, MidiContent, Note, Section,
    TrackBuiltinParam,
};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{track_with, AppData};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::widgets::arrangement::arrangement;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Color, Primitive, Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
const ZOOM: f32 = 64.0;
const ROW_H: f32 = 50.0;
/// 帯 (arranger lane) の縦中央 y。
const SEC_Y: f32 = 29.0;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    build_app_with_header(0.0)
}

/// header pane を踏むテスト用の fixture。 `arrange_header_w` 以外は `build_app()` と同一。
///
/// `build_app()` (= `build_app_with_header(0.0)`) では press 側 (`press_header::dispatch` の
/// `f.header_w > 0.0`) と描画側 (`header::draw_rows` の同ゲート) がともに丸ごと skip されるので、
/// header pane を踏むテストは `build_app_with_header(160.0)` (production default、`app.rs`) を使う。
///
/// **既存テストの座標定数 (`ZOOM` / `WIDGET_RECT.w` / `track0_y()` 等) を header 側と共有しないこと。**
/// `lanes.x` が 0 → 160 にずれ、`view.len_beats` も `640/64 = 10.0` に変わる (beat→x は
/// `160.0 + beat * ZOOM`)。`beat_per_px` は `len_beats / lanes.w` なので header_w に依らず `1/64`。
fn build_app_with_header(
    header_w: f32,
) -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
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
    app.ui_prefs.arrange_header_w = header_w;
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

/// 1 フレーム走らせ、widget が発行した `Edit<AppData>` を `app` に自動 apply し、
/// **描かれた `Scene` を返す**。`primitives` は call order = z-order で並ぶ
/// (`ui/crates/renderer/src/scene.rs`)、つまり「何がどこに / どの順で描かれたか」を
/// ウィンドウを出さずにそのまま検証できる。
fn drive_scene(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    host.frame(app, &mut scene, screen, frame(p), |app, ui| {
        let _ = arrangement(app, ui, WIDGET_RECT);
    });
    scene
}

/// 1 フレーム走らせ、widget が発行した `Edit<AppData>` を `app` に自動 apply する。
fn drive(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) {
    let _ = drive_scene(host, app, p);
}

/// [`drive`] と同じだが、`arrangement()` を呼ぶ **前に** widget 跨ぎの drag を
/// 始める (r.md #71 プラグインのコピー / 移動: インスペクタのチェーンから掴んだ
/// device を運んでいる最中、という状況を作る)。payload は `UiHost` が持つので、
/// 一度始めれば release まで後続フレームでも生きている。
fn drive_dragging(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    host.frame(app, &mut scene, screen, frame(p), |app, ui| {
        if ui.dragging_kind().is_none() {
            ui.begin_drag(
                daw_gui::app::DEVICE_DRAG_KIND,
                daw_gui::app::DeviceDragPayload { device_ids: vec![1], source_track: 1 },
            );
        }
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


// ============================================================
// r.md #68 / #70: 描画そのものの回帰 (Scene の primitive を直接見る)
//
// `Scene::primitives` は call order = z-order で並ぶので、「中身がどこに描かれたか」
// 「どちらが手前か」 をウィンドウを出さずに検証できる。 どちらの症状も
// build / clippy / 既存 test をすり抜けるクラス (見た目だけが壊れる) なので、
// ここで固定する。
// ============================================================

/// MIDI clip 1 件 + ノート 1 件 (content-local 位置を指定できる版)。
fn add_midi_track_with_note(
    app: &mut AppData,
    track_id: u32,
    clip_id: u32,
    start: f64,
    len: f64,
    note_start: f64,
    note_len: f64,
) {
    app.edit_song(|song| {
        song.tracks.clear();
        let cid: ContentId = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Midi(MidiContent {
                notes: vec![Note {
                    pitch: 60,
                    start_beat: note_start,
                    duration_beats: note_len,
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

/// clip 行に描かれた **MIDI ノートの矩形** を (x, w) で拾う。
///
/// `draw_clip_midi_inner` のノートは「枠なし・角丸なし」 の小さな rect。 同じ行の他の
/// rect とは形で切り分く: 行背景 / lane 区切りは全幅、 clip クローム (元 clip と
/// ゴースト) は行高いっぱい、 選択リングは枠付き。 ノートの高さは pitch レンジから
/// 決まるので行高よりずっと小さい。
fn note_rects(scene: &Scene, row_center_y: f32) -> Vec<(f32, f32)> {
    let band_top = row_center_y - ROW_H * 0.5;
    let band_bottom = row_center_y + ROW_H * 0.5;
    let mut out: Vec<(f32, f32)> = scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Rect(c) => Some(c),
            _ => None,
        })
        .filter(|c| {
            c.border_width == 0.0
                && c.radius == [0.0; 4]
                && c.rect.h < ROW_H * 0.5
                && c.rect.w < WIDGET_RECT.w * 0.5
                && c.rect.y >= band_top
                && c.rect.y + c.rect.h <= band_bottom
        })
        .map(|c| (c.rect.x, c.rect.w))
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).expect("finite rect"));
    out
}

fn assert_close(got: (f32, f32), want: (f32, f32), what: &str) {
    assert!(
        (got.0 - want.0).abs() < 1.5 && (got.1 - want.1).abs() < 1.5,
        "{what}: got (x={}, w={}), want (x={}, w={})",
        got.0,
        got.1,
        want.0,
        want.1
    );
}

/// r.md #68 本体: **右端を引っ張ってもノートは伸びない**。
///
/// 修正前はドラッグゴーストが「drag 後の矩形 × drag 前のクリップ長」 でノートを描いて
/// いたため、 4 拍 → 8 拍に伸ばすとノート間隔もノート長もちょうど 2 倍になった
/// (= `stretch_remap` と pixel 一致する time-stretch の絵)。
#[test]
fn right_edge_trim_does_not_stretch_midi_notes() {
    let (mut app, _a, _p) = build_app();
    // clip [2,6) = x∈[128,384]、ノートは content 拍 1..2 (= 曲の 3..4 拍 = x∈[192,256])。
    add_midi_track_with_note(&mut app, 1, 10, 2.0, 4.0, 1.0, 1.0);
    let y = track0_y();
    let mut host = UiHost::no_redraw();

    let before = note_rects(&drive_scene(&mut host, &mut app, PointerFrame::default()), y);
    assert_eq!(before.len(), 1, "ドラッグ前はノート 1 本: got {before:?}");
    assert_close(before[0], (192.0, 64.0), "ドラッグ前");

    // 右端 (x=384) を掴んで +4 拍 (256px) 伸ばす → clip [2,10)。
    drive(&mut host, &mut app, press(384.0, y, no_mods()));
    let during = note_rects(&drive_scene(&mut host, &mut app, hold(640.0, y, no_mods())), y);
    assert_eq!(during.len(), 2, "元 clip とゴーストで 2 本: got {during:?}");
    for (i, r) in during.iter().enumerate() {
        assert_close(*r, before[0], &format!("ドラッグ中 {i} 本目 (トリムでは動かない)"));
    }

    drive(&mut host, &mut app, release(640.0, y, no_mods()));
    let after = note_rects(&drive_scene(&mut host, &mut app, PointerFrame::default()), y);
    assert_eq!(after.len(), 1, "確定後はノート 1 本: got {after:?}");
    assert_close(after[0], before[0], "確定後 (プレビューと一致)");
}

/// r.md #68: **Shift + 端ドラッグ (= 本物の time-stretch) では伸びる**。
/// しかもプレビューと確定結果が一致する (修正前は「ゴーストは伸びるが commit は
/// トリム」「オーディオはその逆」 という preview ≠ commit が両方向に起きていた)。
#[test]
fn shift_right_edge_stretch_preview_matches_commit() {
    let (mut app, _a, _p) = build_app();
    add_midi_track_with_note(&mut app, 1, 10, 2.0, 4.0, 1.0, 1.0);
    let y = track0_y();
    let shift = modifiers(false, true, false);
    let mut host = UiHost::no_redraw();

    let before = note_rects(&drive_scene(&mut host, &mut app, PointerFrame::default()), y);
    assert_close(before[0], (192.0, 64.0), "ドラッグ前");

    // Shift + 右端 drag で 4 拍 → 8 拍 (factor 2、pivot = 左端 = 拍 2)。
    // ノート (曲の 3..4 拍) は 4..6 拍へ → x∈[256,384]、幅は 2 倍。
    drive(&mut host, &mut app, press(384.0, y, shift));
    let during = note_rects(&drive_scene(&mut host, &mut app, hold(640.0, y, shift)), y);
    assert_eq!(during.len(), 2, "元 clip とゴーストで 2 本: got {during:?}");

    drive(&mut host, &mut app, release(640.0, y, shift));
    let after = note_rects(&drive_scene(&mut host, &mut app, PointerFrame::default()), y);
    assert_eq!(after.len(), 1, "確定後はノート 1 本: got {after:?}");
    assert_close(after[0], (256.0, 128.0), "確定後 (伸縮している)");
    // ゴーストのどちらか一方が確定結果と一致する (= 見えていたとおりに確定した)。
    assert!(
        during.iter().any(|r| (r.0 - after[0].0).abs() < 1.5 && (r.1 - after[0].1).abs() < 1.5),
        "ゴーストに確定結果と同じ絵が含まれる: during={during:?} after={after:?}"
    );
}

/// r.md #68: 左端ドラッグでは **中身が絶対時間に留まり、掴んだ端が中身の上を滑る**。
/// 修正前はゴーストの原点が「窓の左端」 だったので波形 / ノートが掴んだ端に付いてきて
/// いた (確定時は `content_offset_beats += δ` で留まるので preview ≠ commit)。
#[test]
fn left_edge_trim_keeps_content_in_place() {
    let (mut app, _a, _p) = build_app();
    // clip [2,6)、ノートは content 拍 2..3 (= 曲の 4..5 拍 = x∈[256,320])。
    add_midi_track_with_note(&mut app, 1, 10, 2.0, 4.0, 2.0, 1.0);
    let y = track0_y();
    let mut host = UiHost::no_redraw();

    let before = note_rects(&drive_scene(&mut host, &mut app, PointerFrame::default()), y);
    assert_close(before[0], (256.0, 64.0), "ドラッグ前");

    // 左端 (x=128) を掴んで +1 拍 (64px) 右へ → clip [3,6) / content_offset 1。
    drive(&mut host, &mut app, press(128.0, y, no_mods()));
    let during = note_rects(&drive_scene(&mut host, &mut app, hold(192.0, y, no_mods())), y);
    assert_eq!(during.len(), 2, "元 clip とゴーストで 2 本: got {during:?}");
    for (i, r) in during.iter().enumerate() {
        assert_close(*r, before[0], &format!("ドラッグ中 {i} 本目 (端に付いてこない)"));
    }

    drive(&mut host, &mut app, release(192.0, y, no_mods()));
    let after = note_rects(&drive_scene(&mut host, &mut app, PointerFrame::default()), y);
    assert_close(after[0], before[0], "確定後 (プレビューと一致)");
}

/// r.md #70: **ドラッグ中の Arranger 帯は最前面**。
///
/// 帯は不透明 fill で、`sections` は `start_beat` 昇順。 修正前は drag 対象を base
/// ループの中で並び順のまま描いていたので、 右へ動かすと後から描かれる隣の帯に
/// 塗り + 帯名ごと食われていた。
#[test]
fn dragged_section_band_is_drawn_in_front() {
    let (mut app, _a, _p) = build_app();
    let red = [1.0, 0.0, 0.0];
    let green = [0.0, 1.0, 0.0];
    app.edit_song(|song| {
        song.sections.clear();
        for (id, start, color) in [(1_u32, 0.0_f64, red), (2, 4.0, green)] {
            song.sections.push(Section {
                id,
                name: "S".to_string(),
                color,
                start_beat: start,
                len_beats: 4.0,
            });
        }
    });
    let fill_index = |scene: &Scene, rgb: [f32; 3]| -> Option<usize> {
        let want = Color::rgb(rgb[0], rgb[1], rgb[2]);
        scene.primitives.iter().position(|p| match p {
            Primitive::Rect(c) => {
                c.fill.r == want.r && c.fill.g == want.g && c.fill.b == want.b && c.fill.a == want.a
            }
            _ => false,
        })
    };
    let mut host = UiHost::no_redraw();
    // 帯 1 = [0,4) (x∈[0,256]) の中央を掴んで +3 拍 (192px) 右へ → [3,7) で帯 2 に重なる。
    drive(&mut host, &mut app, press(128.0, SEC_Y, no_mods()));
    let scene = drive_scene(&mut host, &mut app, hold(320.0, SEC_Y, no_mods()));
    let dragged = fill_index(&scene, red).expect("掴んでいる帯 (赤) が描かれている");
    let other = fill_index(&scene, green).expect("動かしていない帯 (緑) が描かれている");
    assert!(
        dragged > other,
        "掴んでいる帯が後に (= 手前に) 描かれる: dragged={dragged} other={other}"
    );
}

/// r.md #68: **長い動画クリップを横スクロールして先頭が画面外に出ても、
/// サムネイルは見え続ける** (= ユーザー報告そのもの)。
///
/// 旧実装は 1 枚を clip 矩形に aspect-fit していたので、クリップの中央が
/// 画面外にあると 1 枚も見えなかった。 REAPER の "Center/tile image" と同じく
/// content 原点を位相に敷き詰めるので、可視域は常にタイルで埋まる。
#[test]
fn video_clip_thumbnails_tile_across_the_visible_range() {
    use std::num::NonZeroU32;
    use std::path::PathBuf;

    use common::model::{VideoContent, VideoEvent, VideoSource, VideoSourcePath};
    use daw_ui_renderer::TextureHandle;

    let (mut app, _a, _p) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.media.video_sources.insert(
            1,
            VideoSource {
                path: VideoSourcePath::Absolute(PathBuf::from("dummy.mp4")),
                width: 1920,
                height: 1080,
                framerate: 30.0,
                duration_micros: 60_000_000,
                codec: "h264".to_string(),
                audio_source_id: None,
            },
        );
        let cid: ContentId = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Video(VideoContent {
                events: vec![VideoEvent {
                    source_id: 1,
                    event_start_in_clip_beats: 0.0,
                    event_length_beats: 64.0,
                    ..VideoEvent::default()
                }],
            }),
        );
        song.tracks.push(track_with(|t| {
            t.id = 1;
            t.clips = vec![Clip {
                id: 10,
                content_id: cid,
                start_beat: 0.0,
                length_beats: 64.0,
                ..Clip::default()
            }];
        }));
    });
    app.ui_ephemeral
        .video_texture_cache
        .insert(1, TextureHandle::from_raw(NonZeroU32::new(9).unwrap()));

    let mut host = UiHost::no_redraw();
    let tiles_now = |host: &mut UiHost<AppData>, app: &mut AppData| -> Vec<Rect> {
        drive_scene(host, app, PointerFrame::default())
            .primitives
            .iter()
            .filter_map(|p| match p {
                Primitive::Texture(q) => Some(q.rect),
                _ => None,
            })
            .collect()
    };
    // クリップ先頭が画面内 → 途中まで横スクロール、の 2 状態で確認する。
    // 2 回目はカリング範囲が変わるので、cached 層が scroll で再構築されないと
    // タイルが古い可視域のまま残る (= viewport_key に start_beat が入っている前提の回帰)。
    for (scroll, what) in [(0.0_f32, "先頭が画面内"), (20.0, "先頭が画面外")] {
        app.ui_prefs.arrange_scroll_beat = scroll;
        let tiles = tiles_now(&mut host, &mut app);
        assert!(tiles.len() >= 2, "{what}: 可視域がタイルで埋まる: got {} 枚", tiles.len());
        // 全タイル同寸 (= クリップ長でも位置でも大きさが変わらない)。
        let w0 = tiles[0].w;
        for t in &tiles {
            assert!((t.w - w0).abs() < 1e-3, "{what}: タイルは全部同寸: {t:?} vs w={w0}");
        }
        // 可視域 (lanes 全幅) を覆う。
        let left = tiles.iter().fold(f32::MAX, |m, t| m.min(t.x));
        let right = tiles.iter().fold(f32::MIN, |m, t| m.max(t.x + t.w));
        assert!(left <= 0.0, "{what}: 左端まで届く: got {left}");
        assert!(right >= WIDGET_RECT.w, "{what}: 右端まで届く: got {right}");
    }
}

/// r.md #71 (ユーザー報告そのもの): 「Break を 2 まで D&D しても元に戻ってしまいます。
/// その先の C まで D&D すると Break と 2 が入れかわります。」
///
/// 帯を **右隣の帯の位置** まで引っ張ったら、その場で入れかわること (1 つ先まで
/// 引っ張らないと届かない = 1 セクションぶんのズレが無いこと)。 併せて
/// **ドラッグ中に見えていた位置と確定後の位置が一致する** (overlay == commit) ことも見る。
#[test]
fn section_drag_onto_next_band_swaps_and_lands_where_previewed() {
    let (mut app, _a, _p) = build_app();
    let colors = [[1.0_f32, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    app.edit_song(|song| {
        song.sections.clear();
        // Break[0,4) = 赤 / 2[4,8) = 緑 / C[8,12) = 青。1 拍 = 64px。
        for (i, (id, start)) in [(1_u32, 0.0_f64), (2, 4.0), (3, 8.0)].into_iter().enumerate() {
            song.sections.push(Section {
                id,
                name: format!("S{id}"),
                color: colors[i],
                start_beat: start,
                len_beats: 4.0,
            });
        }
    });
    let band_rect = |scene: &Scene, rgb: [f32; 3]| -> Option<Rect> {
        let want = Color::rgb(rgb[0], rgb[1], rgb[2]);
        scene.primitives.iter().find_map(|p| match p {
            Primitive::Rect(c)
                if c.fill.r == want.r
                    && c.fill.g == want.g
                    && c.fill.b == want.b
                    && c.fill.a == want.a =>
            {
                Some(c.rect)
            }
            _ => None,
        })
    };
    let starts = |app: &AppData| -> Vec<(u32, f64)> {
        app.song_doc.song().sections.iter().map(|s| (s.id, s.start_beat)).collect()
    };

    let mut host = UiHost::no_redraw();
    // Break の中央 (x=128) を掴んで、2 の中央 (x=384) まで運ぶ = +4 拍。
    drive(&mut host, &mut app, press(128.0, SEC_Y, no_mods()));
    let scene = drive_scene(&mut host, &mut app, hold(384.0, SEC_Y, no_mods()));
    let previewed = band_rect(&scene, colors[0]).expect("ドラッグ中の帯 (赤) が描かれている");
    assert!(
        (previewed.x - 256.0).abs() < 1.0,
        "ゴーストは 2 の位置 (拍 4 = x256) に見えている: got {}",
        previewed.x
    );

    drive(&mut host, &mut app, release(384.0, SEC_Y, no_mods()));
    assert_eq!(
        starts(&app),
        vec![(2, 0.0), (1, 4.0), (3, 8.0)],
        "Break と 2 が入れかわる (元に戻らない / 1 つ先へ飛ばない)"
    );

    // overlay == commit: 確定後に描かれる赤帯が、ドラッグ中に見えていた位置と同じ。
    let scene = drive_scene(&mut host, &mut app, PointerFrame::default());
    let committed = band_rect(&scene, colors[0]).expect("確定後の帯 (赤)");
    assert!(
        (committed.x - previewed.x).abs() < 1.0 && (committed.w - previewed.w).abs() < 1.0,
        "見えていた位置に落ちる: preview={previewed:?} committed={committed:?}"
    );
}

/// r.md #71: 帯に **食い込む** 位置まで引っ張ったときも、ゴーストは「実際に着地する位置」 を
/// 見せる (= overlay == commit)。 解決を commit 側だけに入れると、見えていた位置と違う所に
/// 落ちるという別のバグになる。
#[test]
fn section_drag_preview_shows_the_resolved_landing_position() {
    let (mut app, _a, _p) = build_app();
    let red = [1.0_f32, 0.0, 0.0];
    app.edit_song(|song| {
        song.sections.clear();
        for (id, start, color) in [
            (1_u32, 0.0_f64, red),
            (2, 4.0, [0.0, 1.0, 0.0]),
            (3, 8.0, [0.0, 0.0, 1.0]),
        ] {
            song.sections.push(Section {
                id,
                name: format!("S{id}"),
                color,
                start_beat: start,
                len_beats: 4.0,
            });
        }
    });
    let red_x = |scene: &Scene| -> Option<f32> {
        let want = Color::rgb(red[0], red[1], red[2]);
        scene.primitives.iter().find_map(|p| match p {
            Primitive::Rect(c)
                if c.fill.r == want.r && c.fill.g == want.g && c.fill.b == want.b =>
            {
                Some(c.rect.x)
            }
            _ => None,
        })
    };

    let mut host = UiHost::no_redraw();
    // Break の中央を +5 拍 (320px) 引っ張る。素の落とし先 (拍 5) は C の位置に食い込むので、
    // 近い方の境界 (拍 4) へ解決される。
    drive(&mut host, &mut app, press(128.0, SEC_Y, no_mods()));
    let scene = drive_scene(&mut host, &mut app, hold(448.0, SEC_Y, no_mods()));
    let previewed = red_x(&scene).expect("ドラッグ中の帯 (赤)");
    assert!(
        (previewed - 256.0).abs() < 1.0,
        "ゴーストは解決後の位置 (拍 4 = x256) に出る (拍 5 = x320 ではない): got {previewed}"
    );

    drive(&mut host, &mut app, release(448.0, SEC_Y, no_mods()));
    let landed = app.song_doc.song().sections.iter().find(|s| s.id == 1).map(|s| s.start_beat);
    assert_eq!(landed, Some(4.0), "見えていた拍 4 に落ちる");
    let scene = drive_scene(&mut host, &mut app, PointerFrame::default());
    let committed = red_x(&scene).expect("確定後の帯 (赤)");
    assert!(
        (committed - previewed).abs() < 1.0,
        "overlay == commit: preview={previewed} committed={committed}"
    );
}

// ============================================================
// r.md #77 §9-B: 分割で崩れうる 3 種を機械で止める
//
// 1. 押した場所ごとに何が起きるか (header pane 系は `build_app_with_header(160.0)`)
// 2. 優先順位の排他 (`!splitter` の 9 ゲート + point / curve_handle / session の各読み点)
// 3. 描画順 (heavy → header の z 順)
//
// **自明な算術を写経するテストは書かない。** 下の 3 種はいずれも「押した場所 → 起きること」
// 「積まれた順序」 という観測可能な振る舞いを assert していて、本番の式を写していない。
// ============================================================

/// production default の header 幅。`build_app_with_header(HEADER_W)` と対で使う。
const HEADER_W: f32 = 160.0;

/// `add_expanded_automation_lane` が足す lane の高さ (px)。
const LANE_H: u16 = 60;

/// master row (visible index 0) の縦中央 y。
fn master_row_y() -> f32 {
    38.0 + ROW_H * 0.5
}

/// track 0 行の下端 (= lane がある場合は lane の上端)。`track0_y()` の行の底。
fn track0_bottom() -> f32 {
    38.0 + ROW_H * 2.0
}

/// automation lane body の下端 (= lane 下端 splitter の位置)。
fn lane_bottom() -> f32 {
    track0_bottom() + f32::from(LANE_H)
}

/// track 0 に automation lane (`lane_id`、高さ `LANE_H`) を 1 本足して展開する。
/// clip `[0, 8)` に point 3 つ (2 つ目は Bezier = curve handle の対象)。
fn add_expanded_automation_lane(app: &mut AppData, track_id: u32, lane_id: u32) {
    app.edit_song(|song| {
        song.clip_contents.insert(
            AUTOMATION_CONTENT_ID,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: 0.2,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint {
                        id: 2,
                        time_beat: 2.0,
                        value: 0.8,
                        curve: AutomationCurve::Bezier { tension: 0.5 },
                    },
                    AutomationPoint {
                        id: 3,
                        time_beat: 6.0,
                        value: 0.4,
                        curve: AutomationCurve::Linear,
                    },
                ],
                next_point_id: 4,
            }),
        );
        if let Some(t) = song.tracks.iter_mut().find(|t| t.id == track_id) {
            t.automation_lanes.push(AutomationLane {
                id: lane_id,
                target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                default_value: 0.5,
                enabled: true,
                visible: true,
                height_px: LANE_H,
                clips: vec![AutomationClip {
                    id: 1,
                    name: String::new(),
                    start_beat: 0.0,
                    length_beats: 8.0,
                    content_id: AUTOMATION_CONTENT_ID,
                    content_offset_beats: 0.0,
                }],
                next_clip_id: 2,
            });
        }
    });
    app.ui_prefs.expanded_automation_tracks.insert(track_id);
}

const AUTOMATION_CONTENT_ID: ContentId = 901;

/// track 0 の automation lane を 1 本持つ状態の `app`。
fn app_with_lane(
    header_w: f32,
) -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (mut app, a, p) = build_app_with_header(header_w);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    add_expanded_automation_lane(&mut app, 1, 1);
    (app, a, p)
}

/// 1 フレーム走らせて `ArrangementResponse` を返す (`arrange_fit_layout.rs` と同じ捕捉手口)。
/// point rect の実座標は response から引く — lane 内の y は `value_norm` 依存なので
/// 座標を当て推量で書くと分岐に届かない。
fn drive_response(
    host: &mut UiHost<AppData>,
    app: &mut AppData,
    p: PointerFrame,
) -> daw_gui::widgets::arrangement::ArrangementResponse {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let mut captured = None;
    host.frame(app, &mut scene, screen, frame(p), |app, ui| {
        captured = Some(arrangement(app, ui, WIDGET_RECT));
    });
    captured.expect("arrangement() は毎フレーム response を返す")
}

/// `point_idx` 番目の automation point の中心 (screen 座標)。
fn point_center(app: &mut AppData, point_idx: u32) -> (f32, f32) {
    let mut host = UiHost::no_redraw();
    let r = drive_response(&mut host, app, PointerFrame::default());
    let (_, rect) = r
        .automation_point_rects
        .iter()
        .find(|(k, _)| k.point_idx == point_idx)
        .copied()
        .expect("automation_point_rects に対象 point が居る");
    (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5)
}

/// 描かれた glyph を **文字で** 引いてその中心を返す。
/// lane header の ★ / 👁 / ✕ は右寄せ配置で、座標を当て推量で書くと分岐に届かない
/// (実際 1 度外した)。 production が実際に置いた位置を読む。
fn glyph_center(scene: &Scene, text: &str) -> Option<(f32, f32)> {
    scene.primitives.iter().find_map(|p| match p {
        Primitive::Glyph(g) if &*g.text == text => {
            Some((g.left + g.font_size * 0.5, g.top + g.font_size * 0.5))
        }
        _ => None,
    })
}

/// header pane 内に描かれた volume band (`arr_tvol_track` の細い帯) の中心。
/// 行内の他の rect とは **高さ** で切り分ける (band は数 px、行背景 / ボタンは桁違いに高い)。
fn volume_band_center(scene: &Scene, row_top: f32) -> Option<(f32, f32)> {
    scene.primitives.iter().find_map(|p| match p {
        Primitive::Rect(c)
            if c.rect.h < 8.0
                && c.rect.x + c.rect.w <= HEADER_W + 0.5
                && c.rect.y > row_top
                && c.rect.y < row_top + ROW_H =>
        {
            Some((c.rect.x + c.rect.w * 0.5, c.rect.y + c.rect.h * 0.5))
        }
        _ => None,
    })
}

fn track_volume_of(app: &AppData, id: u32) -> f32 {
    app.song_doc.song().tracks.iter().find(|t| t.id == id).expect("track が居る").volume
}

fn lane_enabled(app: &AppData, track_id: u32, lane_id: u32) -> Option<bool> {
    lane_field(app, track_id, lane_id, |l| l.enabled)
}

fn lane_visible(app: &AppData, track_id: u32, lane_id: u32) -> Option<bool> {
    lane_field(app, track_id, lane_id, |l| l.visible)
}

fn lane_height(app: &AppData, track_id: u32, lane_id: u32) -> Option<u16> {
    lane_field(app, track_id, lane_id, |l| l.height_px)
}

fn lane_clip_start(app: &AppData, track_id: u32, lane_id: u32) -> Option<f64> {
    lane_field(app, track_id, lane_id, |l| l.clips.first().map(|c| c.start_beat)).flatten()
}

fn lane_exists(app: &AppData, track_id: u32, lane_id: u32) -> bool {
    lane_field(app, track_id, lane_id, |_| ()).is_some()
}

fn lane_field<T>(
    app: &AppData,
    track_id: u32,
    lane_id: u32,
    f: impl Fn(&AutomationLane) -> T,
) -> Option<T> {
    app.song_doc
        .song()
        .tracks
        .iter()
        .find(|t| t.id == track_id)
        .and_then(|t| t.automation_lanes.iter().find(|l| l.id == lane_id))
        .map(f)
}

/// automation curve に残っている point の id 列。
fn point_ids(app: &AppData) -> Vec<u32> {
    app.song_doc
        .song()
        .clip_contents
        .get(&AUTOMATION_CONTENT_ID)
        .and_then(common::model::ClipContent::automation_points)
        .map(|pts| pts.iter().map(|p| p.id).collect())
        .unwrap_or_default()
}

/// `id` の point の clip-local 拍。
fn point_time(app: &AppData, id: u32) -> Option<f64> {
    app.song_doc
        .song()
        .clip_contents
        .get(&AUTOMATION_CONTENT_ID)
        .and_then(common::model::ClipContent::automation_points)
        .and_then(|pts| pts.iter().find(|p| p.id == id))
        .map(|p| p.time_beat)
}

// ------------------------------------------------------------
// 1. 押した場所ごとに何が起きるか (header pane 系 = header_w 160 側)
// ------------------------------------------------------------

/// header の volume band を掴んで右へ引く → その track の音量が上がる。
/// `header_w = 0` の既存 fixture では press 側のゲートで丸ごと skip される領域。
#[test]
fn header_volume_band_drag_changes_track_volume() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    let before = track_volume_of(&app, 1);
    let mut host = UiHost::no_redraw();
    // band の実位置は production が描いた帯から引く (行内で高さが桁違いに小さい rect)。
    let scene = drive_scene(&mut host, &mut app, PointerFrame::default());
    let (bx, by) = volume_band_center(&scene, track0_y() - ROW_H * 0.5)
        .expect("track 0 の volume band が描かれている");
    drive(&mut host, &mut app, press(bx, by, no_mods()));
    drive(&mut host, &mut app, hold(HEADER_W - 10.0, by, no_mods()));
    drive(&mut host, &mut app, release(HEADER_W - 10.0, by, no_mods()));
    let after = track_volume_of(&app, 1);
    assert!(after > before, "右へ引いたら音量が上がる: before={before} after={after}");
}

/// track 行の catch-all click → そのトラックが選択される。
#[test]
fn header_row_click_selects_track() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    app.selection.selected_track_ids.clear();
    let mut host = UiHost::no_redraw();
    // 名前帯 / M·S·R / volume band / lane disclosure を避けた行上部の空き。
    let y = track0_y() - ROW_H * 0.4;
    let x = HEADER_W - 8.0;
    drive(&mut host, &mut app, press(x, y, no_mods()));
    drive(&mut host, &mut app, release(x, y, no_mods()));
    assert_eq!(app.selection.selected_track_ids, vec![1], "行 click でそのトラックが選択される");
}

/// r.md #71 (プラグインのコピー / 移動): **外部 drag を落とした frame の release は
/// 「ヘッダの click」 として扱わない**。
///
/// 扱うと Ctrl+drop が `SelectModifier::Toggle` として解決され、 落とし先トラックの
/// 選択が勝手に反転する / last-wins タグが `Tracks` に倒れて次の Delete がトラックを
/// 消しに行く (= 「落とし先を表示し続ける」 という要件と真っ向から衝突する)。
///
/// このガードは元 `run.rs` にあり、 r.md #77 の 9 ファイル分割で
/// `header::commit_clicks` へ移設された。 **移設で落ちても build / clippy は通る**
/// 種類の壊れ方なので、 ここで機械的に止める。
#[test]
fn header_release_during_external_drag_does_not_select_track() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    app.selection.selected_track_ids.clear();
    app.selection.last_edit_select = None;
    let mut host = UiHost::no_redraw();
    // 押した場所はインスペクタ側 (= この widget の外) なので press は起こさない。
    // 掴んだままヘッダの上へ来て、そこで離す。
    let y = track0_y() - ROW_H * 0.4;
    let x = HEADER_W - 8.0;
    drive_dragging(&mut host, &mut app, hold(x, y, no_mods()));
    drive_dragging(&mut host, &mut app, release(x, y, no_mods()));
    assert!(
        app.selection.selected_track_ids.is_empty(),
        "運搬の drop frame ではトラック選択を走らせない: {:?}",
        app.selection.selected_track_ids
    );
    assert_eq!(
        app.selection.last_edit_select, None,
        "last-wins タグも Tracks に倒さない (次の Delete がトラックを消さない)"
    );

    // 対照: drag していなければ同じ release で普通に選択される (ガードが
    // 「常に選択を殺す」 方向へ壊れていないこと)。
    drive(&mut host, &mut app, release(x, y, no_mods()));
    assert_eq!(
        app.selection.selected_track_ids,
        vec![1],
        "drag していない release は従来どおりトラックを選択する"
    );
}

/// master 行の header click も同じ経路でトラック選択に乗る。
#[test]
fn header_master_row_click_selects_master() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    app.selection.selected_track_ids.clear();
    let mut host = UiHost::no_redraw();
    let y = master_row_y();
    let x = HEADER_W - 8.0;
    drive(&mut host, &mut app, press(x, y, no_mods()));
    drive(&mut host, &mut app, release(x, y, no_mods()));
    assert_eq!(
        app.selection.selected_track_ids,
        vec![common::model::MASTER_TRACK_ID],
        "master 行も選択対象"
    );
}

/// track 行右端の lane disclosure (`+`/`-`) click → automation lane の展開が畳まれる。
#[test]
fn header_lane_disclosure_click_collapses_lanes() {
    let (mut app, _a, _p) = app_with_lane(HEADER_W);
    assert!(app.ui_prefs.expanded_automation_tracks.contains(&1), "前提: 展開されている");
    let mut host = UiHost::no_redraw();
    // `layout.lane_disc_rect` は S ボタンの右 = 行の右端寄り。
    let x = HEADER_W - 6.0;
    let y = track0_y() - ROW_H * 0.25;
    drive(&mut host, &mut app, press(x, y, no_mods()));
    drive(&mut host, &mut app, release(x, y, no_mods()));
    assert!(
        !app.ui_prefs.expanded_automation_tracks.contains(&1),
        "lane disclosure の click で畳まれる"
    );
}

/// lane header の ★ (enabled) / 👁 (visible) / ✕ (delete) がそれぞれ効く。
/// icon 列は lane header 行の左から順に並ぶ (`automation_lane_header_layout`)。
#[test]
fn lane_header_icons_toggle_and_delete_the_lane() {
    /// icon を **描かれた glyph の位置** で click する (★ は左寄せ、👁 / ✕ は右寄せ)。
    fn click_icon(glyph: &str) -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>)
    {
        let (mut app, a, p) = app_with_lane(HEADER_W);
        let mut host = UiHost::no_redraw();
        let scene = drive_scene(&mut host, &mut app, PointerFrame::default());
        let (x, y) = glyph_center(&scene, glyph)
            .unwrap_or_else(|| panic!("lane header に {glyph} が描かれている"));
        drive(&mut host, &mut app, press(x, y, no_mods()));
        drive(&mut host, &mut app, release(x, y, no_mods()));
        (app, a, p)
    }

    // ★ (enabled を落とす)
    let (app, _a, _p) = click_icon("★");
    assert_eq!(lane_enabled(&app, 1, 1), Some(false), "★ click で lane.enabled が false になる");

    // 👁 (visible を落とす)
    let (app, _a, _p) = click_icon("👁");
    assert_eq!(lane_visible(&app, 1, 1), Some(false), "👁 click で lane.visible が false になる");

    // ✕ (lane 削除)
    let (app, _a, _p) = click_icon("✕");
    assert!(!lane_exists(&app, 1, 1), "✕ click で lane が消える");
}

/// popup (右クリックメニュー) が開いているフレームの header press は、
/// 同じ座標で普段起きること (トラック選択) を **起こさない**。
///
/// context menu は `capture_input == false` で背景 pointer を mask しないので、
/// menu item の click が背後の行に届いてしまう (r.md #43 の同件)。
#[test]
fn popup_open_header_press_does_not_select_track() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    app.selection.selected_track_ids.clear();
    let mut host = UiHost::no_redraw();
    let y = track0_y() - ROW_H * 0.4;
    let x = HEADER_W - 8.0;
    for p in [press(x, y, no_mods()), release(x, y, no_mods())] {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
        host.frame(&mut app, &mut scene, screen, frame(p), |app, ui| {
            ui.open_popup(
                ("arr_test_popup", 0_u32),
                Rect { x: 400.0, y: 300.0, w: 10.0, h: 10.0 },
                false,
            );
            let _ = arrangement(app, ui, WIDGET_RECT);
        });
    }
    assert!(
        app.selection.selected_track_ids.is_empty(),
        "popup が開いているフレームの header press は選択を動かさない: {:?}",
        app.selection.selected_track_ids
    );
}

// ------------------------------------------------------------
// 2. 優先順位の排他 (`!splitter` の 9 ゲート + claim の各読み点)
// ------------------------------------------------------------

/// **clip の内側にある lane 下端 splitter** を押す → lane resize だけが起き、clip は動かない。
/// (`press_lanes::clip_zone` の `!claim.splitter` ゲート)
#[test]
fn lane_splitter_inside_clip_does_not_start_clip_drag() {
    let (mut app, _a, _p) = app_with_lane(0.0);
    let start_before = clip_start(&app, 1, 1);
    let h_before = lane_height(&app, 1, 1);
    let mut host = UiHost::no_redraw();
    // lane 下端 splitter は body の x 範囲 × `[bottom - handle, bottom)`。x は clip [0,4) の内側。
    let x = 2.0 * ZOOM;
    let y = lane_bottom() - 2.0;
    drive(&mut host, &mut app, press(x, y, no_mods()));
    drive(&mut host, &mut app, hold(x + 3.0 * ZOOM, y + 20.0, no_mods()));
    drive(&mut host, &mut app, release(x + 3.0 * ZOOM, y + 20.0, no_mods()));
    assert_eq!(clip_start(&app, 1, 1), start_before, "clip は動かない");
    let h_after = lane_height(&app, 1, 1);
    assert!(h_after > h_before, "lane だけが伸びる: before={h_before:?} after={h_after:?}");
}

/// **track 行下端の splitter** を押す → その行の高さだけが変わり、clip は動かない。
#[test]
fn row_splitter_inside_clip_does_not_start_clip_drag() {
    let (mut app, _a, _p) = build_app();
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    let start_before = clip_start(&app, 1, 1);
    let mut host = UiHost::no_redraw();
    let x = 2.0 * ZOOM;
    let y = track0_bottom() - 2.0;
    drive(&mut host, &mut app, press(x, y, no_mods()));
    drive(&mut host, &mut app, hold(x + 3.0 * ZOOM, y + 25.0, no_mods()));
    drive(&mut host, &mut app, release(x + 3.0 * ZOOM, y + 25.0, no_mods()));
    assert_eq!(clip_start(&app, 1, 1), start_before, "clip は動かない");
    let row_h = app.ui_prefs.track_row_overrides.get(&1).copied().unwrap_or(0);
    assert!(
        f32::from(row_h) > ROW_H,
        "行の高さだけが伸びる: {:?}",
        app.ui_prefs.track_row_overrides
    );
}

/// **arranger 帯と header 境界が重なる x** を押す → header 幅 resize が起き、section drag は起きない。
/// (`press::arranger` の `!claim.splitter` ゲート)
#[test]
fn header_splitter_in_arranger_band_does_not_start_section_drag() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_section(&mut app, 1, 0.0, 4.0);
    let start_before = section_start(&app, 1);
    let mut host = UiHost::no_redraw();
    // header splitter の hot zone は境界 ±`header_resize_handle_px/2`。
    drive(&mut host, &mut app, press(HEADER_W + 1.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, hold(HEADER_W + 60.0, SEC_Y, no_mods()));
    drive(&mut host, &mut app, release(HEADER_W + 60.0, SEC_Y, no_mods()));
    assert_eq!(section_start(&app, 1), start_before, "section は動かない");
    assert!(
        app.ui_prefs.arrange_header_w > HEADER_W + 1.0,
        "header 幅だけが広がる: {}",
        app.ui_prefs.arrange_header_w
    );
}

/// **header 境界と ruler が交差する角** を押す → header 幅 resize が起き、playhead は動かない。
/// (`press::ruler` の `!claim.splitter` ゲート)
#[test]
fn header_splitter_in_ruler_does_not_seek_playhead() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    let before = app.transport.playhead_beat;
    let mut host = UiHost::no_redraw();
    let ruler_y = 10.0;
    drive(&mut host, &mut app, press(HEADER_W + 1.0, ruler_y, no_mods()));
    drive(&mut host, &mut app, hold(HEADER_W + 60.0, ruler_y, no_mods()));
    drive(&mut host, &mut app, release(HEADER_W + 60.0, ruler_y, no_mods()));
    assert_eq!(app.transport.playhead_beat, before, "playhead は動かない");
    assert!(
        app.ui_prefs.arrange_header_w > HEADER_W + 1.0,
        "header 幅だけが広がる: {}",
        app.ui_prefs.arrange_header_w
    );
}

/// **automation point の上** を押す → point が動き、automation clip drag は起動しない。
/// (`press_lanes::automation_clip` の `!claim.point` ゲート)
#[test]
fn point_press_does_not_start_automation_clip_drag() {
    let (mut app, _a, _p) = app_with_lane(0.0);
    let clip_before = lane_clip_start(&app, 1, 1);
    let (px, py) = point_center(&mut app, 0);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(px, py, no_mods()));
    drive(&mut host, &mut app, hold(px + ZOOM, py, no_mods()));
    drive(&mut host, &mut app, release(px + ZOOM, py, no_mods()));
    assert_eq!(
        lane_clip_start(&app, 1, 1),
        clip_before,
        "automation clip は動かない (point drag が先勝)"
    );
    assert_eq!(point_time(&app, 1), Some(1.0), "掴んだ point だけが 1 拍ぶん動く");
}

/// point を Alt+click すると即時削除が走り、同じ drag で lane resize は起きない。
///
/// r.md #73 で Alt+drag = lane resize の経路そのものを撤去したので、
/// 「resize が起きない」根拠は `actions.any()` ゲートから **機能の不在**へ変わった。
/// 観測する挙動は同じなのでテストはそのまま残す (`alt_drag_in_a_lane_no_longer_resizes_it`
/// が撤去そのものを、こちらが「削除と同フレームでも巻き添えが無い」ことを見る)。
#[test]
fn alt_click_on_point_deletes_without_resizing_the_lane() {
    let (mut app, _a, _p) = app_with_lane(0.0);
    let h_before = lane_height(&app, 1, 1);
    let (px, py) = point_center(&mut app, 1);
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(px, py, alt));
    drive(&mut host, &mut app, hold(px, py + 30.0, alt));
    drive(&mut host, &mut app, release(px, py + 30.0, alt));
    assert_eq!(point_ids(&app), vec![1, 3], "Alt+click した point (id=2) だけが消える");
    assert_eq!(lane_height(&app, 1, 1), h_before, "lane の高さは変わらない");
}

/// **lasso は clip の上では起動しない**。automation clip の上で drag すると
/// clip が動き、point 選択 (lasso の結果) は起きない。
#[test]
fn drag_on_automation_clip_moves_it_instead_of_lassoing() {
    let (mut app, _a, _p) = app_with_lane(0.0);
    app.selection.selected_automation_points.clear();
    let mut host = UiHost::no_redraw();
    // clip [0,8) の内側で、point (拍 0 / 2 / 6) から離れた拍 4 付近。
    let x = 4.0 * ZOOM;
    let y = track0_bottom() + f32::from(LANE_H) * 0.8;
    drive(&mut host, &mut app, press(x, y, no_mods()));
    drive(&mut host, &mut app, hold(x + ZOOM, y, no_mods()));
    drive(&mut host, &mut app, release(x + ZOOM, y, no_mods()));
    assert_eq!(lane_clip_start(&app, 1, 1), Some(1.0), "automation clip が 1 拍ぶん動く");
    assert!(
        app.selection.selected_automation_points.is_empty(),
        "lasso は起動しない: {:?}",
        app.selection.selected_automation_points
    );
}

/// **空き lane zone の drag は lasso**。clip の外 (拍 8 以降) から掴んで point の上まで
/// 引くと、囲まれた point が選択される。
#[test]
fn drag_on_empty_lane_zone_lassos_points() {
    let (mut app, _a, _p) = app_with_lane(0.0);
    app.selection.selected_automation_points.clear();
    let mut host = UiHost::no_redraw();
    let y_top = track0_bottom() + 4.0;
    let y_bottom = lane_bottom() - 6.0;
    drive(&mut host, &mut app, press(9.0 * ZOOM, y_top, no_mods()));
    drive(&mut host, &mut app, hold(0.5 * ZOOM, y_bottom, no_mods()));
    drive(&mut host, &mut app, release(0.5 * ZOOM, y_bottom, no_mods()));
    assert!(
        !app.selection.selected_automation_points.is_empty(),
        "空き zone の drag で lasso が走り point が選択される"
    );
}

// ------------------------------------------------------------
// 3. 描画順 (heavy → header の z 順)
// ------------------------------------------------------------

/// **フェーズ順の入れ替えを機械で止める唯一の手段。**
///
/// `Scene.primitives` は call order = z-order なので、`render::dispatch` →
/// `release::commit_releases` → `header::draw_rows` の順を入れ替えると
/// track header 行が heavy の背景に埋もれる (build / test / clippy をすり抜ける壊れ方)。
///
/// 色パレットに依存しないよう **幾何** で 2 つの marker を引く:
/// - A = heavy が最初に置く lanes 全面の背景 (`draw_lanes_bg` の `push_filled_rect(lanes, bg)`)
/// - B = master 行 header の panel (`header::draw_rows` の `ui.panel(("arr_master_thbg", 0), ..)`)
///
/// heavy が置く header pane 背景は `h = lanes.h` なので、B とは `h` で区別できる
/// (**`h` が同じ rect を marker にしないこと**)。
#[test]
fn heavy_lanes_bg_is_drawn_before_header_rows() {
    let (mut app, _a, _p) = build_app_with_header(HEADER_W);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    let mut host = UiHost::no_redraw();
    let scene = drive_scene(&mut host, &mut app, PointerFrame::default());

    let lanes_h = WIDGET_RECT.h - 20.0 - 18.0; // ruler 20 + arranger 帯 18
    let index_of = |want: Rect| -> Option<usize> {
        scene.primitives.iter().position(|p| match p {
            Primitive::Rect(c) => {
                (c.rect.x - want.x).abs() < 0.5
                    && (c.rect.y - want.y).abs() < 0.5
                    && (c.rect.w - want.w).abs() < 0.5
                    && (c.rect.h - want.h).abs() < 0.5
            }
            _ => false,
        })
    };
    let a = index_of(Rect { x: HEADER_W, y: 38.0, w: WIDGET_RECT.w - HEADER_W, h: lanes_h })
        .expect("heavy が置く lanes 全面の背景");
    let b = index_of(Rect { x: 0.0, y: 38.0, w: HEADER_W, h: ROW_H })
        .expect("master 行 header の panel");
    assert!(
        b > a,
        "header 行は heavy の背景より後 (= 手前) に積まれる: lanes_bg={a} master_header={b}"
    );
}

// ============================================================
// r.md #73: オートメーションカーブの操作系
//
// 「線を直接 Alt+ドラッグして曲げる」「Alt+ダブルクリックで直線に戻す」「選択の共存」
// 「Alt+drag resize 撤去後に死角が無いこと」を widget を実際に駆動して確かめる。
//
// **このファイルの本文に本体 exe の env 名 (`CARGO_BIN_EXE_` + `daw_gui`) を
// 素の 1 語で書かないこと。** Makefile の `DAW_GUI_SAFE_TESTS` はその語の
// **単純な substring grep** で「daw_gui を起動する target」を判定するので、
// コメントに書くだけで `make test-nolaunch` がこのファイルを丸ごと素通りする
// (実際 1 度やった。`scripts/test_guards.py` の launchtargets:in-sync が検出する)。
// ============================================================

/// r.md #73: 区間 1 本だけの automation lane (track 1 / lane 1 / clip 1、clip は `[0, 8)`)。
/// `values` は **plain** (Volume は `0.0..=2.0`、norm = plain / 2)。
/// 曲線編集は「区間の向き」で保存値の符号が変わるので、上り / 下りを引数で作り分ける。
/// `curve` は **入射側** (= 後ろの点、id = 2) に付く。
fn add_bend_lane(app: &mut AppData, values: (f64, f64), curve: AutomationCurve) {
    app.edit_song(|song| {
        song.clip_contents.insert(
            AUTOMATION_CONTENT_ID,
            ClipContent::Automation(AutomationContent {
                points: vec![
                    AutomationPoint {
                        id: 1,
                        time_beat: 0.0,
                        value: values.0,
                        curve: AutomationCurve::Linear,
                    },
                    AutomationPoint { id: 2, time_beat: 8.0, value: values.1, curve },
                ],
                next_point_id: 3,
            }),
        );
        if let Some(t) = song.tracks.iter_mut().find(|t| t.id == 1) {
            t.automation_lanes = vec![AutomationLane {
                id: 1,
                target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                default_value: 1.0,
                enabled: true,
                visible: true,
                height_px: LANE_H,
                clips: vec![AutomationClip {
                    id: 1,
                    name: String::new(),
                    start_beat: 0.0,
                    length_beats: 8.0,
                    content_id: AUTOMATION_CONTENT_ID,
                    content_offset_beats: 0.0,
                }],
                next_clip_id: 2,
            }];
        }
    });
    app.ui_prefs.expanded_automation_tracks.insert(1);
}

fn app_with_bend_lane(
    header_w: f32,
    values: (f64, f64),
    curve: AutomationCurve,
) -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (mut app, a, p) = build_app_with_header(header_w);
    add_midi_track_with_clip(&mut app, 1, 1, 0.0, 4.0);
    add_bend_lane(&mut app, values, curve);
    (app, a, p)
}

/// 2 つの point dot の中心 (x 昇順)。**レイアウト式をテスト側で複製しない**ため、
/// widget が返す `automation_point_rects` を SSoT にする。
fn point_dots(app: &mut AppData) -> ((f32, f32), (f32, f32)) {
    let mut host = UiHost::no_redraw();
    let r = drive_response(&mut host, app, PointerFrame::default());
    let mut c: Vec<(f32, f32)> = r
        .automation_point_rects
        .iter()
        .map(|(_, rect)| (rect.x + rect.w * 0.5, rect.y + rect.h * 0.5))
        .collect();
    c.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert_eq!(c.len(), 2, "点 2 つを想定: {c:?}");
    (c[0], c[1])
}

/// automation clip の描画域 (縦 padding 適用済)。 これも widget の実 rect を使う。
fn lane_clip_rect(app: &mut AppData) -> Rect {
    let mut host = UiHost::no_redraw();
    let r = drive_response(&mut host, app, PointerFrame::default());
    r.automation_clip_rects.first().expect("automation clip が 1 本描かれている").1
}

/// `Linear` 区間の進捗 `u` にあたる screen 座標 (= 2 dot を結ぶ直線上)。
fn linear_segment_point(app: &mut AppData, u: f32) -> (f32, f32) {
    let (p0, p1) = point_dots(app);
    (p0.0 + (p1.0 - p0.0) * u, p0.1 + (p1.1 - p0.1) * u)
}

fn point_curve(app: &AppData, id: u32) -> Option<AutomationCurve> {
    app.song_doc
        .song()
        .clip_contents
        .get(&AUTOMATION_CONTENT_ID)
        .and_then(common::model::ClipContent::automation_points)
        .and_then(|pts| pts.iter().find(|p| p.id == id))
        .map(|p| p.curve)
}

/// 区間の進捗 `u` における **plain 値** (= 実際に鳴る値)。
/// 数式は production の `apply_curve` をそのまま呼ぶ (テストに式を写さない)。
fn curve_value_at(app: &AppData, u: f64) -> f64 {
    let pts = app
        .song_doc
        .song()
        .clip_contents
        .get(&AUTOMATION_CONTENT_ID)
        .and_then(common::model::ClipContent::automation_points)
        .expect("automation content が居る");
    assert!(pts.len() >= 2, "区間が 1 本以上ある: {pts:?}");
    common::automation::apply_curve(pts[0].value, pts[1].value, u, pts[1].curve)
}

/// Alt+ドラッグで線を **画面上へ**曲げ、鳴る値が直線より上へ動いたことを確かめる。
///
/// **保存される `bend` の符号は assert しない** — 値は progress 基準なので上り区間と
/// 下り区間で逆符号になる (それが #73 の核心)。見えるもの (= 値の上下) だけを見る。
fn assert_alt_drag_bends_upward(values: (f64, f64)) {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, values, AutomationCurve::Linear);
    const U: f64 = 0.25;
    let straight = curve_value_at(&app, U);
    let (gx, gy) = linear_segment_point(&mut app, 0.25);
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(gx, gy, alt));
    drive(&mut host, &mut app, hold(gx, gy - 12.0, alt));
    drive(&mut host, &mut app, release(gx, gy - 12.0, alt));
    assert!(
        matches!(point_curve(&app, 2), Some(AutomationCurve::Exponential { .. })),
        "Linear 区間は「曲線」(Exponential) へ自動変換される: got {:?}",
        point_curve(&app, 2)
    );
    let bent = curve_value_at(&app, U);
    assert!(
        bent > straight + 1e-3,
        "{values:?}: カーソルを上げたので鳴る値も直線 {straight} より上のはず (got {bent})"
    );
}

/// Alt+ドラッグで上り区間を曲げると、鳴る値が直線より上になる。
#[test]
fn alt_drag_bends_a_rising_segment_upward() {
    assert_alt_drag_bends_upward((0.2, 1.8));
}

/// 同じジェスチャを下り区間でやっても、鳴る値は直線より上になる (= 画面上で上がる)。
/// 保存される `bend` の符号は上り区間と逆になるが、見える挙動は同じ。
/// **旧実装はここが逆で、上り区間で線が下へ沈んでいた。**
#[test]
fn alt_drag_bends_a_falling_segment_upward_too() {
    assert_alt_drag_bends_upward((1.8, 0.2));
}

/// Hold 区間を Alt+ドラッグすると「曲線」に自動変換されてから量が付く。
/// Hold の描画は水平線なので、掴む座標は前の点の y を使う。
#[test]
fn alt_drag_converts_hold_segment_to_a_curve() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Hold);
    let (p0, p1) = point_dots(&mut app);
    let gx = p0.0 + (p1.0 - p0.0) * 0.25;
    let gy = p0.1; // Hold は前値で水平
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(gx, gy, alt));
    drive(&mut host, &mut app, hold(gx, gy - 12.0, alt));
    drive(&mut host, &mut app, release(gx, gy - 12.0, alt));
    assert!(
        matches!(point_curve(&app, 2), Some(AutomationCurve::Exponential { .. })),
        "Hold 区間も「曲線」へ自動変換される: got {:?}",
        point_curve(&app, 2)
    );
}

/// Alt+ドラッグは release で 1 件だけ Edit を出す (= undo 1 段)。
#[test]
fn alt_drag_commits_once_on_release() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    let (gx, gy) = linear_segment_point(&mut app, 0.25);
    let before = app.song_doc.undo_depth();
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(gx, gy, alt));
    drive(&mut host, &mut app, hold(gx, gy - 8.0, alt));
    drive(&mut host, &mut app, hold(gx, gy - 12.0, alt));
    drive(&mut host, &mut app, release(gx, gy - 12.0, alt));
    assert_eq!(
        app.song_doc.undo_depth(),
        before + 1,
        "drag 中は commit せず、release で 1 段だけ積む"
    );
}

/// **動かさない Alt+クリック (線の上) は何も変えない。**
///
/// `preview_curve` を毎フレーム無条件に逆算すると、Hold / Linear 区間では
/// `start_curve` が `Exponential { bend: 0.0 }` なので dy = 0 でも
/// `Exponential { bend: 0.0 }` が返り、`anchor_curve` (= Linear) と異なるため
/// release の no-op 判定をすり抜けて **クリックしただけで dirty + undo 1 段**になる。
/// 既に曲がっている区間でも逆算の丸めで同じことが起きる。
#[test]
fn alt_click_on_the_line_without_moving_changes_nothing() {
    for curve in [
        AutomationCurve::Linear,
        AutomationCurve::Hold,
        AutomationCurve::Exponential { bend: 0.5 },
    ] {
        let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), curve);
        let (p0, p1) = point_dots(&mut app);
        // Hold は水平なので前の点の y、それ以外は直線 (Exponential は掴めなくてもよい —
        // session が起動しなければそもそも何も起きないので、どちらでも assert は成立する)。
        let gx = p0.0 + (p1.0 - p0.0) * 0.25;
        let gy = if matches!(curve, AutomationCurve::Hold) {
            p0.1
        } else {
            p0.1 + (p1.1 - p0.1) * 0.25
        };
        let before = app.song_doc.undo_depth();
        let alt = modifiers(false, false, true);
        let mut host = UiHost::no_redraw();
        drive(&mut host, &mut app, press(gx, gy, alt));
        drive(&mut host, &mut app, release(gx, gy, alt));
        assert_eq!(point_curve(&app, 2), Some(curve), "{curve:?}: curve は変わらない");
        assert_eq!(app.song_doc.undo_depth(), before, "{curve:?}: undo も積まれない");
    }
}

/// Alt+ダブルクリック (線の上) で直線に戻る。
/// S 字 (`Bezier`) は u=0.5 を必ず通る (数学的な固定点) ので、
/// 2 dot の中点がそのまま曲線の上になる。
#[test]
fn alt_double_click_on_the_line_resets_to_linear() {
    let (mut app, _a, _p) =
        app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Bezier { tension: 0.5 });
    let (mx, my) = linear_segment_point(&mut app, 0.5);
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    for p in [press(mx, my, alt), release(mx, my, alt), press(mx, my, alt), release(mx, my, alt)] {
        drive(&mut host, &mut app, p);
    }
    assert_eq!(
        point_curve(&app, 2),
        Some(AutomationCurve::Linear),
        "線の上の Alt+ダブルクリックで直線に戻る"
    );
}

/// Alt+ダブルクリック (線から離れた場所) は従来どおりスナップ無しで点を足す。
/// **ハーネスの `build_app` は `arrange_snap_enabled = false` なので、
/// このテストは冒頭で `true` に戻す** — 戻さないと「Alt 無しでもスナップしない」ので
/// 「Alt でスナップが切れた」が空振りする。
#[test]
fn alt_double_click_off_the_line_still_adds_an_unsnapped_point() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    app.ui_prefs.arrange_snap_enabled = true;
    let clip_rect = lane_clip_rect(&mut app);
    // 線は norm 0.1 → 0.9 の直線。 clip 上端近く (norm 0.9 付近) は前半では線から遠い。
    // x はグリッドに乗らない拍を選ぶ。
    let x = 1.3_f32 * ZOOM;
    let y = clip_rect.y + clip_rect.h * 0.1;
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    for p in [press(x, y, alt), release(x, y, alt), press(x, y, alt), release(x, y, alt)] {
        drive(&mut host, &mut app, p);
    }
    let times: Vec<f64> = app
        .song_doc
        .song()
        .clip_contents
        .get(&AUTOMATION_CONTENT_ID)
        .and_then(common::model::ClipContent::automation_points)
        .map(|pts| pts.iter().map(|p| p.time_beat).collect())
        .unwrap_or_default();
    assert_eq!(times.len(), 3, "点が 1 つ増える: {times:?}");
    let added = times.iter().copied().find(|t| *t > 0.0 && *t < 8.0).expect("中間に足された点");
    assert!(
        (added - 1.3).abs() < 1e-4,
        "Alt でスナップが切れて raw 拍 1.3 に着地する: got {added}"
    );
}

/// r.md #73 (E): 点の無修飾クリックでクリップ選択が消えない。
/// 選択集合は面を跨いで共存でき、Delete / Copy / Cut の宛先は last-wins が解決する。
#[test]
fn clicking_a_point_keeps_the_automation_clip_selection() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    app.selection.selected_automation_clips =
        vec![common::model::AutomationClipKey { track: 1, lane: 1, clip: 1 }];
    let (p0, _p1) = point_dots(&mut app);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(p0.0, p0.1, no_mods()));
    drive(&mut host, &mut app, release(p0.0, p0.1, no_mods()));
    assert!(
        !app.selection.selected_automation_clips.is_empty(),
        "点の click でクリップ選択は消えない: {:?}",
        app.selection.selected_automation_clips
    );
    assert!(
        !app.selection.selected_automation_points.is_empty(),
        "点は選択される: {:?}",
        app.selection.selected_automation_points
    );
}

/// 逆方向も同じ — クリップの無修飾クリックで点選択が消えない。
#[test]
fn clicking_an_automation_clip_keeps_the_point_selection() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    app.selection.selected_automation_points = vec![daw_gui::app_types::AutomationPointKeyRef {
        track_id: 1,
        lane_id: 1,
        clip_id: 1,
        point_idx: 0,
    }];
    let clip_rect = lane_clip_rect(&mut app);
    // 線からも点からも離れた clip 内 (上端寄り、 拍 2 付近 = 線はまだ下にいる)。
    let x = 2.0_f32 * ZOOM;
    let y = clip_rect.y + clip_rect.h * 0.05;
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(x, y, no_mods()));
    drive(&mut host, &mut app, release(x + 1.0, y, no_mods()));
    assert!(
        !app.selection.selected_automation_clips.is_empty(),
        "クリップが選択される: {:?}",
        app.selection.selected_automation_clips
    );
    assert!(
        !app.selection.selected_automation_points.is_empty(),
        "クリップの click で点選択は消えない: {:?}",
        app.selection.selected_automation_points
    );
}

/// r.md #73: レーン本体の Alt+ドラッグはもうレーン高さを変えない
/// (高さは Alt+ホイールと下端スプリッタが担う)。
#[test]
fn alt_drag_in_a_lane_no_longer_resizes_it() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    let before = lane_height(&app, 1, 1);
    let (gx, gy) = linear_segment_point(&mut app, 0.25);
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(gx, gy, alt));
    drive(&mut host, &mut app, hold(gx, gy + 30.0, alt));
    drive(&mut host, &mut app, release(gx, gy + 30.0, alt));
    assert_eq!(lane_height(&app, 1, 1), before, "レーン本体の Alt+drag で高さは変わらない");
}

/// r.md #73 (§3.6): 線から離れた場所の Alt+ドラッグは死角にならず、
/// automation clip が動く。しかも Alt がスナップを無効にしている
/// (= MIDI / audio clip と対称)。
///
/// **ハーネスの `build_app` は `arrange_snap_enabled = false` なので、
/// このテストは冒頭で `true` に戻す。** 戻さないと「Alt 無しでもスナップしない」ので
/// 比較が成立しない (空振りする)。
#[test]
fn alt_drag_off_the_line_moves_the_clip_without_snapping() {
    /// 線から離れた clip 上を 0.3 拍ぶん引いて、着地した clip start を返す。
    fn drag_clip(alt_on: bool) -> f64 {
        let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
        app.ui_prefs.arrange_snap_enabled = true;
        let clip_rect = lane_clip_rect(&mut app);
        // 線は左下から右上へ上がるので、左半分の **上端寄り** は線から遠い。
        let x = 2.0_f32 * ZOOM;
        let y = clip_rect.y + clip_rect.h * 0.05;
        let m = modifiers(false, false, alt_on);
        let dx = ZOOM * 0.3; // グリッドに乗らない移動量
        let mut host = UiHost::no_redraw();
        drive(&mut host, &mut app, press(x, y, m));
        drive(&mut host, &mut app, hold(x + dx * 0.5, y, m));
        drive(&mut host, &mut app, release(x + dx, y, m));
        lane_clip_start(&app, 1, 1).expect("automation clip が居る")
    }
    let snapped = drag_clip(false);
    let raw = drag_clip(true);
    assert!(
        (raw - 0.3).abs() < 1e-4,
        "Alt でスナップが切れて 0.3 拍ぶんそのまま動く: got {raw}"
    );
    assert!(
        (snapped - raw).abs() > 1e-3,
        "Alt 無しはグリッドに吸着して別の位置になる: snapped={snapped} raw={raw}"
    );
}

/// r.md #73 (§3.6): lane header 列の Alt+ドラッグはレーン高さを変えない
/// (撤去後は無反応。高さは Alt+ホイールとスプリッタが担う)。
/// 既定ハーネスは `arrange_header_w = 0.0` なので header 付き fixture で回す。
#[test]
fn alt_drag_in_the_lane_header_column_no_longer_resizes() {
    let (mut app, _a, _p) = app_with_bend_lane(HEADER_W, (0.2, 1.8), AutomationCurve::Linear);
    let before = lane_height(&app, 1, 1);
    // lane header 列 = x < HEADER_W、 y は lane body の中ほど (下端 splitter を避ける)。
    let x = HEADER_W * 0.5;
    let y = track0_bottom() + f32::from(LANE_H) * 0.5;
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(x, y, alt));
    drive(&mut host, &mut app, hold(x, y + 30.0, alt));
    drive(&mut host, &mut app, release(x, y + 30.0, alt));
    assert_eq!(
        lane_height(&app, 1, 1),
        before,
        "レーンヘッダ列の Alt+drag でも高さは変わらない"
    );
}

/// r.md #73 (§3.5): Alt+クリック (点) は点を消すだけで、
/// 同フレームに区間 bend も clip drag も起動しない。
/// 旧実装は「point drag session が立ったか」でしか消費を判定しておらず、
/// Alt+クリックでは session が立たないので後続の press が二重に走っていた。
#[test]
fn alt_click_on_a_point_only_deletes_it() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    let clip_before = lane_clip_start(&app, 1, 1);
    let curve_before = point_curve(&app, 2);
    let (p0, _p1) = point_dots(&mut app);
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(p0.0, p0.1, alt));
    drive(&mut host, &mut app, hold(p0.0, p0.1 - 20.0, alt));
    drive(&mut host, &mut app, release(p0.0, p0.1 - 20.0, alt));
    assert_eq!(point_ids(&app), vec![2], "Alt+click した点 (id=1) だけが消える");
    assert_eq!(lane_clip_start(&app, 1, 1), clip_before, "automation clip は動かない");
    assert_eq!(point_curve(&app, 2), curve_before, "区間 bend も起動しない");
}

/// automation lane の中に描かれた線分の総数 (curve 本体 + hover 強調 + preview)。
/// **色ではなく本数**で見る — 「消えた」は本数が減ること、
/// 「強調が乗った」は本数が増えること、と 1 つの尺度で表せる。
fn lane_line_segment_count(scene: &Scene, lane_rect: Rect) -> usize {
    scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Line(b) => Some(b),
            _ => None,
        })
        .flat_map(|b| b.segments.iter())
        .filter(|s| {
            let inside = |y: f32| y >= lane_rect.y - 1.0 && y <= lane_rect.y + lane_rect.h + 1.0;
            inside(s.a[1]) && inside(s.b[1])
        })
        .count()
}

/// **r.md #73 の回帰**: Alt を押したままカーソルを線の上に置いても、
/// オートメーション曲線が消えない。
///
/// 実機で「Alt hover すると線が消える」症状が出た。 hover 強調は cached の **外** に
/// 描くので、cached 側の base curve は 1 本も減らないはず — 減っていたら
/// 「alt 押下が cached を再構築させて curve を落としている」ということになる。
///
/// 色ではなく **線分の本数** で見る (色は overlay の上塗りで変わりうるが、
/// 「消えた」は本数でしか捕まえられない)。
#[test]
fn alt_hover_on_the_line_does_not_erase_the_automation_curve() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    let lane_rect = {
        let mut host = UiHost::no_redraw();
        let r = drive_response(&mut host, &mut app, PointerFrame::default());
        r.automation_lane_rects.first().expect("lane が 1 本描かれている").1
    };
    let (gx, gy) = linear_segment_point(&mut app, 0.5);

    // (a) 修飾なしで線の上 — これが基準。
    let mut host = UiHost::no_redraw();
    let plain = drive_scene(&mut host, &mut app, hold(gx, gy, no_mods()));
    let base = lane_line_segment_count(&plain, lane_rect);
    assert!(base > 0, "前提: 修飾なしで曲線が描かれている (got {base})");

    // (b) Alt を押したまま同じ場所 — 強調が **上乗せ** されるので、本数は減らない。
    let alt = modifiers(false, false, true);
    let hovered = drive_scene(&mut host, &mut app, hold(gx, gy, alt));
    let with_alt = lane_line_segment_count(&hovered, lane_rect);
    assert!(
        with_alt >= base,
        "Alt hover で曲線が消えている: 修飾なし {base} 本 → Alt {with_alt} 本"
    );
}

// r.md #73: 実機と同じ経路 (`view::root::build_root`) と **実ピクセル** での
// 「Alt hover で曲線が消えない」検証は `daw_gui/tests/automation_hover_visual.rs`。
// widget を直接呼ぶこのファイルでは、widget の外で起きる上書きが見えないため。

/// `x_at` を跨ぐ線分の y を集め、近いもの (±1.5px) を 1 つにまとめた「その x に何本の
/// 線が見えるか」。
///
/// **重ね塗り (太い線で覆う) は本数を増やさない** — 縁取りと本体は同じ y に乗るので
/// 1 つのクラスタに畳まれる。形が食い違ったときだけクラスタが増える = 2 重線。
///
/// 薄い線 (alpha < 0.5) は数えない。 lane には `default_value` の水平ガイド
/// (`automation_default_line_color` = `grid_line` の alpha 0.18) が **全 x に** 引かれて
/// いるので、 除かないと常に 1 本増えて「曲線が何本見えるか」 を測れない。
fn line_positions_at(scene: &Scene, band: Rect, x_at: f32) -> Vec<f32> {
    let mut ys: Vec<f32> = scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Line(b) => Some(b),
            _ => None,
        })
        .flat_map(|b| b.segments.iter())
        .filter_map(|s| {
            if s.color.a < 0.5 {
                return None;
            }
            let (x0, x1) = (s.a[0].min(s.b[0]), s.a[0].max(s.b[0]));
            if x_at < x0 || x_at > x1 {
                return None;
            }
            // 線分上の y を線形補間 (垂直線は始点 y)。
            let y = if (s.b[0] - s.a[0]).abs() < 1e-6 {
                s.a[1]
            } else {
                let t = (x_at - s.a[0]) / (s.b[0] - s.a[0]);
                s.a[1] + (s.b[1] - s.a[1]) * t
            };
            // lane の帯に入っているものだけ (グリッド線 / playhead を除く)。
            (y >= band.y && y <= band.y + band.h).then_some(y)
        })
        .collect();
    ys.sort_by(f32::total_cmp);
    let mut clusters: Vec<f32> = Vec::new();
    for y in ys {
        if clusters.last().is_none_or(|last| (y - last).abs() > 1.5) {
            clusters.push(y);
        }
    }
    clusters
}

/// **r.md #73 の回帰**: 曲げている最中に線が 2 重に見えない。
///
/// ドラッグ中はまだモデルを書き換えないので、cached レイヤは **元の曲線**を描き続ける。
/// その上に preview を重ねる作りだが、曲げた瞬間に 2 本の形が食い違うので下の元の線が
/// 見える。「線幅 1.5 倍で覆う」設計は**形が一致しているときしか成立していなかった**。
#[test]
fn bend_drag_does_not_leave_the_original_curve_showing() {
    let (mut app, _a, _p) = app_with_bend_lane(0.0, (0.2, 1.8), AutomationCurve::Linear);
    let band = lane_clip_rect(&mut app);
    // 区間の中点を掴む。preview は必ず指の位置を通るので、上へ引いた分だけ
    // 元の線と形が食い違う = 2 重線が最大になる場所。
    let (gx, gy) = linear_segment_point(&mut app, 0.5);
    let alt = modifiers(false, false, true);
    let mut host = UiHost::no_redraw();

    // 前提: 掴む前は 1 本。
    let before = drive_scene(&mut host, &mut app, hold(gx, gy, alt));
    assert_eq!(
        line_positions_at(&before, band, gx).len(),
        1,
        "前提: 掴む前は曲線が 1 本 (got {:?})",
        line_positions_at(&before, band, gx)
    );

    drive(&mut host, &mut app, press(gx, gy, alt));
    // 15px 上へ = 元の線と preview が 15px 離れる。
    let dragging = drive_scene(&mut host, &mut app, hold(gx, gy - 15.0, alt));
    let positions = line_positions_at(&dragging, band, gx);
    assert_eq!(
        positions.len(),
        1,
        "曲げている最中に線が 2 重に見えている (掴んだ x で {} 本): {positions:?}",
        positions.len()
    );

    // release 後はモデルが更新され、また 1 本に戻る。
    drive(&mut host, &mut app, release(gx, gy - 15.0, alt));
    let after = drive_scene(&mut host, &mut app, hold(gx, gy - 15.0, no_mods()));
    assert_eq!(
        line_positions_at(&after, band, gx).len(),
        1,
        "release 後も 1 本 (got {:?})",
        line_positions_at(&after, band, gx)
    );
}

// ============================================================
// r.md #73 の同件: ゲインドラッグ中の dB ハンドル線
// ============================================================

/// audio event を 1 つ持つ clip (= `audio_edit` が Some になる) を track 0 に置く。
/// gain は 0 dB (= ハンドル線が clip 矩形の縦中央に来る = 掴む帯と同じ y)。
fn add_audio_track_with_clip(app: &mut AppData, track_id: u32, clip_id: u32, len: f64) {
    app.edit_song(|song| {
        song.tracks.clear();
        let cid: ContentId = song.alloc_content_id();
        song.clip_contents.insert(
            cid,
            ClipContent::Audio(AudioContent {
                events: vec![AudioEvent {
                    id: 1,
                    event_length_beats: len,
                    ..AudioEvent::default()
                }],
                next_event_id: 2,
            }),
        );
        song.tracks.push(track_with(|t| {
            t.id = track_id;
            t.clips = vec![Clip {
                id: clip_id,
                content_id: cid,
                start_beat: 0.0,
                length_beats: len,
                ..Clip::default()
            }];
        }));
    });
}

/// dB ハンドル線の y を集める。
///
/// **何を数えているかを絞り込むためのフィルタ**が 3 つある (r.md #73 の 2 重線テストで
/// レーンの既定値ガイドを数えてしまった教訓):
/// 1. **水平** (`|a.y - b.y| < 0.5`) … 小節線 / 再生ヘッドは縦線なので落ちる。
/// 2. **x 範囲が clip の margin 内側と一致** … トラック行の区切り線は lanes 全幅なので落ちる。
///    ゴースト線も base 線も同じ `clip_rect_anchor` から出るので、どちらも残る (= 2 重線を
///    見逃さない)。
/// 3. **その行の帯の中** … 他トラックの同形の線が混ざらない。
///
/// ±1.5px 以内は 1 本に畳む (縁取り等の重ね塗りは 1 本と数える。 y が食い違ったときだけ増える)。
fn db_handle_ys(scene: &Scene, band: Rect, span: (f32, f32)) -> Vec<f32> {
    let mut ys: Vec<f32> = scene
        .primitives
        .iter()
        .filter_map(|p| match p {
            Primitive::Line(b) => Some(b),
            _ => None,
        })
        .flat_map(|b| b.segments.iter())
        .filter(|s| (s.a[1] - s.b[1]).abs() < 0.5)
        .filter(|s| {
            (s.a[0].min(s.b[0]) - span.0).abs() < 0.5 && (s.a[0].max(s.b[0]) - span.1).abs() < 0.5
        })
        .map(|s| s.a[1])
        .filter(|y| *y >= band.y && *y <= band.y + band.h)
        .collect();
    ys.sort_by(f32::total_cmp);
    let mut clusters: Vec<f32> = Vec::new();
    for y in ys {
        if clusters.last().is_none_or(|last| (y - last).abs() > 1.5) {
            clusters.push(y);
        }
    }
    clusters
}

/// **r.md #73 の同件**: ゲインドラッグ中に dB ハンドル線が 2 重に見えない。
///
/// オートメーション曲線の 2 重線と同じ root cause — ドラッグ中はモデルを書き換えないので
/// cached レイヤが anchor 値の位置に base 線を描き続け、ゴーストが別の y に preview 線を
/// 描く。 覆う覆わない以前に **y が違うので原理的に重ならない**。
#[test]
fn gain_drag_does_not_leave_the_original_db_handle_showing() {
    let (mut app, _a, _p) = build_app();
    add_audio_track_with_clip(&mut app, 1, 1, 4.0);
    // 幾何 (ファイル冒頭の固定レイアウト): track 0 の行 = y∈[88, 138)、
    // clip 矩形 = 行 top+2 / 高さ row_h-4 なので中央 y = 113 (= `track0_y()`)。
    // clip は 0..4 拍 = x∈[0, 256)、 ハンドル線は margin 24 の内側 = x∈[24, 232]。
    let band = Rect { x: 0.0, y: 38.0 + ROW_H, w: WIDGET_RECT.w, h: ROW_H };
    let span = (24.0, 232.0);
    let (gx, gy) = (128.0, track0_y());

    let mut host = UiHost::no_redraw();
    let before = drive_scene(&mut host, &mut app, hold(gx, gy, no_mods()));
    let before_ys = db_handle_ys(&before, band, span);
    assert_eq!(
        before_ys.len(),
        1,
        "前提: 掴む前は dB ハンドル線が 1 本 (got {before_ys:?})"
    );
    assert!(
        (before_ys[0] - gy).abs() < 0.5,
        "前提: 0 dB の線は clip 矩形の縦中央 {gy} にある (got {before_ys:?}) \
         — ここがずれていたら数えている線が違う"
    );

    drive(&mut host, &mut app, press(gx, gy, no_mods()));
    // 60px 上 = +15 dB (0.25 dB/px) → 線は 14.4px 上がる (24dB で clip 高さの半分)。
    let dragging = drive_scene(&mut host, &mut app, hold(gx, gy - 60.0, no_mods()));
    let during = db_handle_ys(&dragging, band, span);
    assert_eq!(
        during.len(),
        1,
        "ドラッグ中に dB ハンドル線が 2 重に見えている ({} 本): {during:?}",
        during.len()
    );
    assert!(
        during[0] < gy - 10.0,
        "ドラッグ中に残っている 1 本は preview 側 (= 上に上がっている) であること (got {during:?})"
    );

    // 掴んだ位置へ戻す = anchor と同値 (`compute_audio_drag_outcome` が None を返す)。
    // ここで ghost が「変化が無いから描かない」 だと、base を skip している分だけ線が
    // **消える** (= anchor 値をまたぐたびに点滅する)。 anchor 値の 1 本が出続けること。
    let back = drive_scene(&mut host, &mut app, hold(gx, gy, no_mods()));
    let back_ys = db_handle_ys(&back, band, span);
    assert_eq!(
        back_ys.len(),
        1,
        "掴んだ値へ戻したフレームで線が消えている / 増えている (got {back_ys:?})"
    );
    assert!(
        (back_ys[0] - gy).abs() < 0.5,
        "戻したフレームの 1 本は anchor 値の位置 (got {back_ys:?})"
    );

    drive(&mut host, &mut app, hold(gx, gy - 60.0, no_mods()));
    // release フレーム: 描画は commit した Edit の適用より前なので cached はまだ anchor 値。
    // session の clone が take より前 (`sessions.rs`) なのでゴーストが残り、ここも 1 本。
    // ここが崩れると「離した瞬間に線が 1 フレームだけ元の位置へ戻る」。
    let on_release = drive_scene(&mut host, &mut app, release(gx, gy - 60.0, no_mods()));
    let release_ys = db_handle_ys(&on_release, band, span);
    assert_eq!(release_ys.len(), 1, "release フレームも 1 本 (got {release_ys:?})");
    assert!(
        release_ys[0] < gy - 10.0,
        "release フレームの 1 本は preview 側 (= anchor へ戻っていない) (got {release_ys:?})"
    );
    let after = drive_scene(&mut host, &mut app, hold(gx, gy - 60.0, no_mods()));
    let after_ys = db_handle_ys(&after, band, span);
    assert_eq!(after_ys.len(), 1, "release 後も 1 本 (got {after_ys:?})");
    // commit されたことを確かめる (= 掴む帯を実際に掴めていた証拠。 これが無いと
    // 「press が外れていて session が始まらなかったから 1 本」 でも緑になる)。
    let gain = app
        .song_doc
        .song()
        .clip_contents
        .values()
        .find_map(|c| c.audio_events().and_then(<[AudioEvent]>::first))
        .map(|ev| ev.gain_db)
        .expect("audio event が 1 つある");
    assert!(gain > 1.0, "ドラッグが commit されて gain が上がっている (got {gain})");
}
