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
use daw_ui_renderer::{Color, Primitive, Rect, Scene};

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
