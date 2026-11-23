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

use common::model::{Chord, ChordEvent, ChordKind, Clip, ClipContent, MidiContent, Note};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, track_with};
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
            .place_chord(ChordEvent::new(start, len, Chord::new(root, ChordKind::Major)))
            .unwrap();
    });
    id
}

/// `(start, len, root)` の列 (位置順)。
fn layout(app: &AppData) -> Vec<(f64, f64, u8)> {
    app.cur.song_doc.song().chords.iter().map(|c| (c.start_beat, c.len_beats, c.chord.root)).collect()
}

/// 空きのダブルクリックで 1 小節 (4/4 = 4 拍) の置く前の箱でポップアップが開く。r.md #159: 曲はまだ変わらない
/// (箱が入るのはコードを選んだ時点。選ぶ / 閉じるは `tests/chord_popup.rs`)。
#[test]
fn double_click_on_empty_opens_the_popup_on_a_bar_draft() {
    let (mut app, _a, _p) = build_app();
    let mut host = UiHost::no_redraw();
    let x = 2.0 * ZOOM;
    let depth = app.cur.song_doc.undo_depth();
    for p in [press(x, UPPER_Y, none()), release(x, UPPER_Y, none()), press(x, UPPER_Y, none()), release(x, UPPER_Y, none())] {
        drive(&mut host, &mut app, p);
    }
    assert!(layout(&app).is_empty(), "選ぶまで曲に箱は無い");
    assert_eq!(app.cur.song_doc.undo_depth(), depth);
    let draft = app.cur.peph.chord_band.popup.as_ref().and_then(|p| p.draft()).expect("置く前の箱で開く");
    assert_eq!((draft.start_beat, draft.len_beats, draft.chord.root), (2.0, 4.0, 0), "拍 2 から 1 小節 (キーが無いので C)");
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

/// 見出しの `▾` → 「表記: ローマ数字」で表記が切り替わり、帯のコード名が度数になる (キーが無い曲でも)。
#[test]
fn settings_menu_switches_the_notation() {
    use common::chord_name::ChordNotation;
    let (mut app, _a, _p) = build_app();
    app.cur.view.arrange_header_w = 160.0;
    place(&mut app, 0.0, 4.0, 9);
    let mut host = UiHost::no_redraw();
    let click = |host: &mut UiHost<AppData>, app: &mut AppData, x: f32, y: f32| {
        drive(host, app, press(x, y, none()));
        drive(host, app, release(x, y, none()));
    };
    drive(&mut host, &mut app, hold(700.0, 500.0, none()));
    // `▾` (x∈[128,146)) を押すとメニューがその下に開く。項目は 24px / 行で 3 行目が「ローマ数字」。
    click(&mut host, &mut app, 137.0, 48.0);
    click(&mut host, &mut app, 150.0, 58.0 + 24.0 * 2.0 + 12.0);
    assert_eq!(app.cur.view.chord_band.notation, ChordNotation::Roman, "メニューの項目で表記が切り替わる");
    let chord = app.cur.song_doc.song().chords[0].chord;
    let key = app.cur.song_doc.song().scale_at(0.0);
    assert_eq!(key, None);
    assert_eq!(common::chord_name::format_chord(&chord, ChordNotation::Roman, key), "VI", "キーが無い曲は C を主音にする");
}

/// master 行 (y∈[58,108)) の下の 1 本目のトラックに、C メジャーの和音を鳴らす 4 拍の MIDI クリップを置く。
fn add_triad_clip(app: &mut AppData, track_id: u32, clip_id: u32, start: f64) {
    app.edit_song(|song| {
        song.tracks.clear();
        let cid = song.alloc_content_id();
        let notes = [60u8, 64, 67]
            .into_iter()
            .map(|pitch| Note { pitch, start_beat: 0.0, duration_beats: 4.0, velocity: 100, ..Note::default() })
            .collect();
        song.clip_contents.insert(cid, ClipContent::Midi(MidiContent { notes, ..MidiContent::default() }));
        song.tracks.push(track_with(|t| {
            t.id = track_id;
            t.clips = vec![Clip { id: clip_id, content_id: cid, start_beat: start, length_beats: 4.0, ..Clip::default() }];
        }));
    });
}

/// 箱をトラックのレーンへ運んで離すと、そのトラックの MIDI クリップになる (帯の箱は動かない)。
#[test]
fn dragging_a_box_onto_a_track_makes_a_clip() {
    let (mut app, _a, _p) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.tracks.push(track_with(|t| t.id = 5));
    });
    place(&mut app, 2.0, 4.0, 9);
    // トラック 5 の行は master 行の下 y∈[108,158)。
    drag(&mut app, (3.0 * ZOOM, UPPER_Y), (3.0 * ZOOM, 130.0), none());
    assert_eq!(layout(&app), vec![(2.0, 4.0, 9)], "帯の箱は動かない");
    let t = app.cur.song_doc.song().track_by_id(5).unwrap();
    assert_eq!(t.clips.len(), 1, "落としたトラックにクリップができる");
    assert!((t.clips[0].start_beat - 2.0).abs() < 1e-9, "クリップは箱の位置: {}", t.clips[0].start_beat);
}

/// グループ (子を持つトラック = 自分のクリップを鳴らさない) の行は落とし先にならない: クリップはできず、
/// レーンの上で離したので帯の箱も動かない。
#[test]
fn dropping_a_box_on_a_group_row_does_nothing() {
    let (mut app, _a, _p) = build_app();
    app.edit_song(|song| {
        song.tracks.clear();
        song.tracks.push(track_with(|t| t.id = 5));
        song.tracks.push(track_with(|t| {
            t.id = 6;
            t.parent_group_id = Some(5);
        }));
    });
    place(&mut app, 2.0, 4.0, 9);
    // グループ 5 の行は master 行の下 y∈[108,158)。横にも 1 拍動かして離す。
    drag(&mut app, (3.0 * ZOOM, UPPER_Y), (4.0 * ZOOM, 130.0), none());
    assert_eq!(layout(&app), vec![(2.0, 4.0, 9)], "帯の箱は動かない");
    let song = app.cur.song_doc.song();
    assert!(song.track_by_id(5).unwrap().clips.is_empty(), "グループにはクリップを作らない");
    assert!(song.track_by_id(6).unwrap().clips.is_empty());
}

/// MIDI クリップのヘッダを帯へ運んで離すと、クリップは動かずにコード (C) が帯に置かれる。
#[test]
fn dropping_a_midi_clip_on_the_band_extracts_chords() {
    let (mut app, _a, _p) = build_app();
    add_triad_clip(&mut app, 1, 10, 2.0);
    // クリップのヘッダ帯 (行の上端 +2px) を掴んで帯の上へ。
    let header_y = 108.0 + 2.0;
    drag(&mut app, (3.0 * ZOOM, header_y), (3.0 * ZOOM, UPPER_Y), none());
    let song = app.cur.song_doc.song();
    let clip = &song.track_by_id(1).unwrap().clips[0];
    assert!((clip.start_beat - 2.0).abs() < 1e-9, "クリップは動かない: {}", clip.start_beat);
    let roots: Vec<(u8, ChordKind)> = song.chords.iter().map(|c| (c.chord.root, c.chord.kind)).collect();
    assert_eq!(roots, vec![(0, ChordKind::Major)], "C が抽出される: {:?}", song.chords);
    assert!((song.chords[0].start_beat - 2.0).abs() < 1e-9);
}

/// 「コードを抽出…」のダイアログ: 「抽出」を押すとクリップのコードが帯に置かれ、ダイアログが閉じる。
/// 画面 812×600 の中央に 380×236 のパネル (x∈[216,596) / y∈[182,418))。
#[test]
fn the_extract_dialog_places_chords_and_closes() {
    use daw_gui::app::AppEvent;
    use daw_gui::event_chord::ChordToolEvent;
    let (mut app, _a, _p) = build_app();
    add_triad_clip(&mut app, 1, 10, 2.0);
    app.handle_event(AppEvent::ChordTool(ChordToolEvent::OpenExtractDialog { clips: vec![(1, 10)] }));
    let mut host = UiHost::no_redraw();
    let frame = |host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame| {
        let mut scene = Scene::new();
        let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
        let mut input = FrameInput::default();
        input.pointer = p;
        host.frame(app, &mut scene, screen, input, |app, ui| {
            let _ = arrangement(app, ui, WIDGET_RECT);
            daw_gui::view::chord_extract_modal::draw(app, ui, screen);
        });
    };
    frame(&mut host, &mut app, hold(700.0, 500.0, none()));
    frame(&mut host, &mut app, PointerFrame { pos: Some((523.0, 386.0)), ..PointerFrame::default() });
    // 「抽出」: 右下 (x∈[468,578) / y∈[372,400))。
    frame(&mut host, &mut app, press(523.0, 386.0, none()));
    frame(&mut host, &mut app, release(523.0, 386.0, none()));
    frame(&mut host, &mut app, PointerFrame { pos: Some((700.0, 500.0)), ..PointerFrame::default() });
    let song = app.cur.song_doc.song();
    let roots: Vec<(u8, ChordKind)> = song.chords.iter().map(|c| (c.chord.root, c.chord.kind)).collect();
    assert_eq!(roots, vec![(0, ChordKind::Major)], "C が抽出される: {:?}", song.chords);
    assert!(app.cur.chord_tools.extract_dialog.is_none(), "ダイアログが閉じる");
}

/// C メジャーの和音 (4 拍) を 1 つ持つ SMF (format 0、PPQ 480) を書く。
fn write_triad_midi(dir: &std::path::Path) -> std::path::PathBuf {
    use midly::num::{u4, u7, u15, u28};
    use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
    let ev = |delta: u32, key: u8, on: bool| TrackEvent {
        delta: u28::from(delta),
        kind: TrackEventKind::Midi {
            channel: u4::from(0),
            message: if on {
                MidiMessage::NoteOn { key: u7::from(key), vel: u7::from(100) }
            } else {
                MidiMessage::NoteOff { key: u7::from(key), vel: u7::from(0) }
            },
        },
    };
    let mut track = vec![ev(0, 60, true), ev(0, 64, true), ev(0, 67, true), ev(1920, 60, false), ev(0, 64, false), ev(0, 67, false)];
    track.push(TrackEvent { delta: u28::from(0u32), kind: TrackEventKind::Meta(MetaMessage::EndOfTrack) });
    let mut smf = Smf::new(Header::new(Format::SingleTrack, Timing::Metrical(u15::from(480))));
    smf.tracks = vec![track];
    let path = dir.join("triad.mid");
    smf.save(&path).unwrap();
    path
}

/// エクスプローラから MIDI ファイルを帯へ落とすと、落とした拍からコードが置かれる (トラックは増えない)。
#[test]
fn dropping_a_midi_file_on_the_band_places_its_chords() {
    let (mut app, _a, _p) = build_app();
    let dir = tempfile::tempdir().unwrap();
    let path = write_triad_midi(dir.path());
    let tracks_before = app.cur.song_doc.song().tracks.len();
    let mut host = UiHost::no_redraw();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let input = FrameInput {
        pointer: PointerFrame { pos: Some((4.0 * ZOOM, UPPER_Y)), ..PointerFrame::default() },
        file_drop: Some(daw_ui_core::DroppedFiles { paths: vec![path], position: (4.0 * ZOOM, UPPER_Y) }),
        ..FrameInput::default()
    };
    host.frame(&mut app, &mut scene, screen, input, |app, ui| {
        let _ = arrangement(app, ui, WIDGET_RECT);
    });
    let song = app.cur.song_doc.song();
    let got: Vec<(f64, u8, ChordKind)> = song.chords.iter().map(|c| (c.start_beat, c.chord.root, c.chord.kind)).collect();
    assert_eq!(got.first().map(|c| (c.0, c.1, c.2)), Some((4.0, 0, ChordKind::Major)), "拍 4 から C: {got:?}");
    assert_eq!(song.tracks.len(), tracks_before, "アレンジの取り込みへは流れない");
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
