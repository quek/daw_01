//! r.md #157 (`docs/plan_chord_track.md` §3.1): コードトラックの帯の interaction (窓を出さない)。
//!
//! 駆動は `arr_widget.rs` と同じ (`UiHost::frame` に press → hold → release を流し、widget が
//! 発行した `Edit<AppData>` が適用された後の Song / 選択を見る)。
//!
//! 幾何: ruler=20px / Arranger 帯=18px / **コードの帯 y∈[38,58)** / lanes は y≥58。
//! `arrange_header_w=0` / `arrange_zoom_x=64` (1 拍=64px) / snap 無効で beat→x = beat*64。
//! ランチャー帯は畳み、widget の原点を掴み代ぶん左へ出して lanes の左端を 0 に保つ (`arr_widget.rs`)。

#![allow(clippy::field_reassign_with_default)]

use std::sync::Arc;

use common::model::{Chord, ChordEvent, ChordKind};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::AppData;
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::widgets::arrangement::arrangement;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{Modifiers, PhysicalSize};
use daw_ui_renderer::{Rect, Scene};

const LAUNCHER_GRAB_W: f32 = 12.0;
const WIDGET_RECT: Rect = Rect { x: -LAUNCHER_GRAB_W, y: 0.0, w: 800.0 + LAUNCHER_GRAB_W, h: 600.0 };
const ZOOM: f32 = 64.0;
/// 帯の上半分 / 下半分の y。
const UPPER_Y: f32 = 42.0;
const LOWER_Y: f32 = 54.0;

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
    app.cur.view.arrange_track_row_h = 50.0;
    app.cur.view.arrange_track_top = 0.0;
    app.cur.view.arrange_snap_enabled = false;
    app.cur.view.launcher_layout = common::model::LauncherLayout::ArrangerOnly;
    (app, audio_rx, plugin_rx)
}

fn m(ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers { ctrl, shift, alt, ..Modifiers::empty() }
}

fn none() -> Modifiers {
    m(false, false, false)
}

fn press(x: f32, y: f32, mods: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: mods,
        ..PointerFrame::default()
    }
}

fn hold(x: f32, y: f32, mods: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_pressed: true, modifiers: mods, ..PointerFrame::default() }
}

fn release(x: f32, y: f32, mods: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_just_released: true, modifiers: mods, ..PointerFrame::default() }
}

fn drive(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let mut input = FrameInput::default();
    input.pointer = p;
    host.frame(app, &mut scene, screen, input, |app, ui| {
        let _ = arrangement(app, ui, WIDGET_RECT);
    });
}

fn drag(app: &mut AppData, from: (f32, f32), to: (f32, f32), mods: Modifiers) {
    let mut host = UiHost::no_redraw();
    drive(&mut host, app, press(from.0, from.1, mods));
    drive(&mut host, app, hold((from.0 + to.0) * 0.5, to.1, mods));
    drive(&mut host, app, hold(to.0, to.1, mods));
    drive(&mut host, app, release(to.0, to.1, mods));
}

fn place(app: &mut AppData, start: f64, len: f64, root: u8) -> u32 {
    let mut id = 0;
    app.edit_song(|song| {
        id = song
            .place_chord(ChordEvent {
                id: 0,
                start_beat: start,
                len_beats: len,
                chord: Chord::new(root, ChordKind::Major),
                color: None,
            })
            .unwrap();
    });
    id
}

/// `(start, len, root)` の列 (位置順)。
fn layout(app: &AppData) -> Vec<(f64, f64, u8)> {
    app.cur.song_doc.song().chords.iter().map(|c| (c.start_beat, c.len_beats, c.chord.root)).collect()
}

/// 空きのダブルクリックで 1 小節 (4/4 = 4 拍) の箱ができ、選ばれてポップアップが開く。
#[test]
fn double_click_on_empty_creates_a_bar_and_opens_the_popup() {
    let (mut app, _a, _p) = build_app();
    let mut host = UiHost::no_redraw();
    let x = 2.0 * ZOOM;
    for p in [press(x, UPPER_Y, none()), release(x, UPPER_Y, none()), press(x, UPPER_Y, none()), release(x, UPPER_Y, none())] {
        drive(&mut host, &mut app, p);
    }
    assert_eq!(layout(&app), vec![(2.0, 4.0, 0)], "拍 2 から 1 小節の箱 (キーが無いので C)");
    let id = app.cur.song_doc.song().chords[0].id;
    assert_eq!(app.cur.selection.selected_chord_ids, vec![id]);
    assert_eq!(app.cur.peph.chord_band.popup.as_ref().map(|p| p.chord_id), Some(id));
}

/// 中央のドラッグで移動、Ctrl+ドラッグで複製 (元は残る)。
#[test]
fn center_drag_moves_and_ctrl_drag_duplicates() {
    let (mut app, _a, _p) = build_app();
    place(&mut app, 0.0, 2.0, 0);
    drag(&mut app, (1.0 * ZOOM, UPPER_Y), (4.0 * ZOOM, UPPER_Y), none());
    assert_eq!(layout(&app), vec![(3.0, 2.0, 0)]);
    drag(&mut app, (4.0 * ZOOM, UPPER_Y), (7.0 * ZOOM, UPPER_Y), m(true, false, false));
    assert_eq!(layout(&app), vec![(3.0, 2.0, 0), (6.0, 2.0, 0)]);
}

/// 接している 2 箱の境界: 下半分は両方の端が一緒に動き、上半分は掴んだ箱だけ (隣は上書きで削られる / 隙間が空く)。
#[test]
fn boundary_lower_half_moves_both_edges() {
    let (mut app, _a, _p) = build_app();
    place(&mut app, 0.0, 4.0, 0);
    place(&mut app, 4.0, 4.0, 7);
    // 境界 (x=256) の左の箱の右端を下半分で掴んで +1 拍。
    drag(&mut app, (4.0 * ZOOM - 2.0, LOWER_Y), (5.0 * ZOOM - 2.0, LOWER_Y), none());
    assert_eq!(layout(&app), vec![(0.0, 5.0, 0), (5.0, 3.0, 7)], "境界ごと動く");
    // 上半分で -2 拍: 掴んだ箱だけ縮み、隙間が空く。
    drag(&mut app, (5.0 * ZOOM - 2.0, UPPER_Y), (3.0 * ZOOM - 2.0, UPPER_Y), none());
    assert_eq!(layout(&app), vec![(0.0, 3.0, 0), (5.0, 3.0, 7)], "掴んだ箱だけ");
}

/// 空きのドラッグで範囲にかかる箱を選び、Delete (`delete_current_surface`) でその箱だけ消える。
#[test]
fn range_drag_selects_and_delete_removes_only_the_selection() {
    let (mut app, _a, _p) = build_app();
    place(&mut app, 0.0, 1.0, 0);
    place(&mut app, 2.0, 1.0, 2);
    place(&mut app, 4.0, 1.0, 4);
    // 拍 1.5 → 4.5 (2 個目と 3 個目にかかる)。
    drag(&mut app, (1.5 * ZOOM, UPPER_Y), (4.5 * ZOOM, UPPER_Y), none());
    assert_eq!(app.cur.selection.selected_chord_ids.len(), 2);
    app.delete_current_surface(false);
    assert_eq!(layout(&app), vec![(0.0, 1.0, 0)]);
}

/// 見出し (ヘッダ幅 160): `追従` で曲全体の追従スイッチ (Song) が切り替わり、Arranger 帯の見出しの
/// `コード` で帯が隠れる (lanes が Arranger 帯の直下へ詰まる)。
#[test]
fn header_buttons_toggle_follow_and_visibility() {
    let (mut app, _a, _p) = build_app();
    app.cur.view.arrange_header_w = 160.0;
    let mut host = UiHost::no_redraw();
    let click = |host: &mut UiHost<AppData>, app: &mut AppData, x: f32, y: f32| {
        drive(host, app, press(x, y, none()));
        drive(host, app, release(x, y, none()));
    };
    drive(&mut host, &mut app, hold(700.0, 500.0, none()));
    // 見出しの右端 (x = -12 + 160 - 2 = 146) から `▾` 18 / `追従` 30 / `●` 18 の順に並ぶ。
    let before = app.cur.song_doc.song().chord_follow_enabled;
    click(&mut host, &mut app, 109.0, 48.0);
    assert_eq!(app.cur.song_doc.song().chord_follow_enabled, !before, "`追従` で Song のスイッチが切り替わる");
    // Arranger 帯の見出しの右端の `コード` (幅 38)。
    click(&mut host, &mut app, 125.0, 29.0);
    assert!(!app.cur.view.chord_band.visible, "帯が隠れる");
    drive(&mut host, &mut app, hold(700.0, 500.0, none()));
    assert_eq!(app.cur.peph.chord_band.lane.rect.h, 0.0, "隠した帯は高さ 0");
}

/// Alt+端のドラッグ (複数選択) は選択全体を比例伸縮する (右端を掴めば左端が軸)。
#[test]
fn alt_edge_drag_scales_the_selection() {
    let (mut app, _a, _p) = build_app();
    let a = place(&mut app, 0.0, 2.0, 0);
    let b = place(&mut app, 2.0, 2.0, 5);
    app.cur.selection.selected_chord_ids = vec![a, b];
    // 2 個目の右端 (x=256) を +4 拍 → 範囲 4 拍が 8 拍へ (2 倍)。
    drag(&mut app, (4.0 * ZOOM - 2.0, UPPER_Y), (8.0 * ZOOM - 2.0, UPPER_Y), m(false, false, true));
    assert_eq!(layout(&app), vec![(0.0, 4.0, 0), (4.0, 4.0, 5)]);
}
