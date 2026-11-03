//! Automation Editor (オートメーションクリップの編集面) のシナリオ。
//!
//! セルはレーンに描かれないので、ここが点に触る唯一の場所になる。実際のポインタ入力を
//! フレームで流し、Song の点がどう変わるかを見る。

use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::model::{
    AutomationClip, AutomationClipKey, AutomationContent, AutomationCurve, AutomationLane,
    AutomationPoint, AutomationTarget, ClipContent, ClipKey, ContentId, Device, LaunchSettings,
    PluginInstance, SessionAutomationClip, TrackBuiltinParam,
};
use common::plugin_format::PluginFormat;
use common::protocol::{PluginCommand, PluginParamInfo};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{track_with, AppData, AppEvent, AutomationPointKeyRef};
use daw_gui::app_types::EditSurface;
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event_launcher::{LauncherCellKey, LauncherEvent};
use daw_gui::view::automation_editor::{self, EditorLayout};
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::{ElementState, KeyEvent, Modifiers, PhysicalKey, PhysicalSize};
use daw_ui_renderer::{OffscreenRenderer, Rect, Scene};

const AREA: Rect = Rect { x: 0.0, y: 300.0, w: 1000.0, h: 400.0 };
/// 8 拍のクリップを全体表示したときの幅に使う。
const CLIP_LEN: f64 = 8.0;
const CELL_CONTENT: ContentId = 501;
const ARR_CONTENT: ContentId = 502;
const CELL: AutomationClipKey = AutomationClipKey { track: 1, lane: 1, clip: 2 };
const ARR: AutomationClipKey = AutomationClipKey { track: 1, lane: 1, clip: 1 };

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None::<AppDirs>,
        48_000,
    );
    (app, plugin_rx)
}

fn point(id: u32, time_beat: f64, value: f64, curve: AutomationCurve) -> AutomationPoint {
    AutomationPoint { id, time_beat, value, curve }
}

/// track 1 の Volume レーン (plain `0..=2`) に、拍 16 のアレンジのクリップと列 0 のセルを置く。
/// セルの点は `cell_points`。
fn seed(app: &mut AppData, target: AutomationTarget, cell_points: Vec<AutomationPoint>) {
    let next_point_id = cell_points.len() as u32 + 1;
    app.edit_song(move |song| {
        song.tracks.clear();
        let scene_id = song.ensure_scene_at(0);
        song.clip_contents.insert(
            CELL_CONTENT,
            ClipContent::Automation(AutomationContent { points: cell_points, next_point_id }),
        );
        song.clip_contents.insert(
            ARR_CONTENT,
            ClipContent::Automation(AutomationContent {
                points: vec![point(1, 0.0, 1.0, AutomationCurve::Linear)],
                next_point_id: 2,
            }),
        );
        song.tracks.push(track_with(|t| {
            t.id = 1;
            // 束縛先の device が無いプラグインのレーンは編集のたびに掃除される。
            if let AutomationTarget::PluginParam { device_id, .. } = target {
                let mut inst = PluginInstance::new("com.example.synth".into(), PluginFormat::Clap);
                inst.id = device_id;
                t.devices = vec![Device::Plugin(inst)];
            }
            t.automation_lanes = vec![AutomationLane {
                id: 1,
                height_px: 80,
                clips: vec![AutomationClip {
                    id: 1,
                    start_beat: 16.0,
                    length_beats: CLIP_LEN,
                    content_id: ARR_CONTENT,
                    ..AutomationClip::default()
                }],
                session_clips: vec![SessionAutomationClip {
                    scene_id,
                    clip: AutomationClip {
                        id: 2,
                        start_beat: 0.0,
                        length_beats: CLIP_LEN,
                        content_id: CELL_CONTENT,
                        ..AutomationClip::default()
                    },
                    launch: LaunchSettings::default(),
                }],
                next_clip_id: 3,
                ..AutomationLane::new(target, 1.0)
            }];
        }));
    });
    app.cur.view.arrange_snap_enabled = false;
}

fn volume() -> AutomationTarget {
    AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume)
}

/// 空のセルを開いた状態。
fn app_with_cell(points: Vec<AutomationPoint>) -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (mut app, rx) = build_app();
    seed(&mut app, volume(), points);
    app.handle_event(AppEvent::Launcher(LauncherEvent::OpenCellEditor(LauncherCellKey::Lane(CELL))));
    (app, rx)
}

fn points_of(app: &AppData, content: ContentId) -> Vec<AutomationPoint> {
    app.cur.song_doc
        .song()
        .clip_contents
        .get(&content)
        .and_then(ClipContent::automation_points)
        .map(<[_]>::to_vec)
        .unwrap_or_default()
}

fn layout() -> EditorLayout {
    EditorLayout::new(AREA)
}

/// 全体表示 (拍 0..CLIP_LEN) の画面座標。`norm` は値域の高さ (Volume は plain の半分)。
fn at(beat: f64, norm: f32) -> (f32, f32) {
    let plot = layout().plot;
    (plot.x + (beat / CLIP_LEN) as f32 * plot.w, plot.y + (1.0 - norm) * plot.h)
}

fn no_mods() -> Modifiers {
    Modifiers::empty()
}

fn alt() -> Modifiers {
    Modifiers { alt: true, ..Modifiers::empty() }
}

fn press(p: (f32, f32), m: Modifiers) -> PointerFrame {
    PointerFrame {
        pos: Some(p),
        primary_just_pressed: true,
        primary_pressed: true,
        modifiers: m,
        ..PointerFrame::default()
    }
}

fn hold(p: (f32, f32), m: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some(p), primary_pressed: true, modifiers: m, ..PointerFrame::default() }
}

fn release(p: (f32, f32), m: Modifiers) -> PointerFrame {
    PointerFrame { pos: Some(p), primary_just_released: true, modifiers: m, ..PointerFrame::default() }
}

fn drive(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 1000, height: 700 };
    let input = FrameInput { pointer: p, ..FrameInput::default() };
    let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
        automation_editor::draw(app, ui, AREA);
    });
    for e in edits {
        e.apply(app);
    }
    scene
}

fn run(app: &mut AppData, frames: &[PointerFrame]) {
    let mut host = UiHost::no_redraw();
    for p in frames {
        drive(&mut host, app, *p);
    }
}

fn double_click(p: (f32, f32), m: Modifiers) -> [PointerFrame; 4] {
    [press(p, m), release(p, m), press(p, m), release(p, m)]
}

fn drag(from: (f32, f32), to: (f32, f32), m: Modifiers) -> [PointerFrame; 3] {
    [press(from, m), hold(to, m), release(to, m)]
}

fn two_points() -> Vec<AutomationPoint> {
    vec![
        point(1, 1.0, 0.5, AutomationCurve::Linear),
        point(2, 5.0, 1.5, AutomationCurve::Linear),
    ]
}

// ---- 開閉 ----------------------------------------------------------------

#[test]
fn セルを開くと_automation_editor_が開く() {
    let (mut app, _rx) = build_app();
    seed(&mut app, volume(), Vec::new());
    app.cur.view.bottom_panel = None;
    assert_eq!(app.automation_editor_target(), None);

    app.handle_event(AppEvent::Launcher(LauncherEvent::OpenCellEditor(LauncherCellKey::Lane(CELL))));

    assert_eq!(app.automation_editor_target(), Some(CELL));
    assert_eq!(app.cur.view.bottom_panel, Some(1), "閉じていてもタブ 1 で開く");
    assert_eq!(
        app.cur.selection.selected_launcher_cells,
        vec![LauncherCellKey::Lane(CELL)],
        "セルも選択される (他のセルと同じ)"
    );
}

/// 同じタブを取り合うので、片方を開くともう片方は閉じる。
#[test]
fn audio_editor_とは排他() {
    let (mut app, _rx) = app_with_cell(Vec::new());
    app.cur.peph.audio_editor_clip = Some(ClipKey { track_id: 1, clip_id: 99 });
    app.open_automation_editor(ARR);
    assert_eq!(app.cur.peph.audio_editor_clip, None);
    assert_eq!(app.automation_editor_target(), Some(ARR));

    app.show_piano_roll();
    assert_eq!(app.automation_editor_target(), None, "MIDI を開く経路では閉じる");
    assert_eq!(app.cur.view.bottom_panel, Some(1));
}

/// クリップが消えたら、どの経路でも閉じたことになる (貼り替えはしない)。
#[test]
fn セルを消すと閉じ選択も残らない() {
    let (mut app, _rx) = app_with_cell(two_points());
    run(&mut app, &[press(at(1.0, 0.25), no_mods()), release(at(1.0, 0.25), no_mods())]);
    assert_eq!(app.cur.selection.selected_automation_points.len(), 1, "前提: 点を選んだ");

    app.delete_launcher_cells(&[LauncherCellKey::Lane(CELL)]);

    assert_eq!(app.automation_editor_target(), None);
    assert_ne!(app.edit_surface(false), Some(EditSurface::AutomationPoints), "実在しない点は宛先にならない");
    let before = app.cur.song_doc.undo_depth();
    app.delete_current_surface(false);
    assert_eq!(app.cur.song_doc.undo_depth(), before, "空振りの undo を積まない");
}

/// タブを Mixer へ切り替えると、選んでいたセルの点は見えない。Delete が消してはいけない。
#[test]
fn タブを隠すとセルの点はdeleteの宛先から外れる() {
    let (mut app, _rx) = app_with_cell(two_points());
    run(&mut app, &[press(at(1.0, 0.25), no_mods()), release(at(1.0, 0.25), no_mods())]);
    assert_eq!(app.edit_surface(false), Some(EditSurface::AutomationPoints), "前提: 見えている間は宛先");

    app.handle_event(AppEvent::SelectBottomPanel(0));
    app.delete_current_surface(false);
    assert_eq!(points_of(&app, CELL_CONTENT).len(), 2, "見えない点は消えない");

    // 戻せば同じ選択のまま消せる。
    app.handle_event(AppEvent::SelectBottomPanel(1));
    app.delete_current_surface(false);
    assert_eq!(points_of(&app, CELL_CONTENT).len(), 1);
}

/// 消えたクリップの key を持ち続けない。undo で戻ったときに、Piano Roll を見ているタブが
/// 突然エディタへ変わってはいけない。
#[test]
fn クリップが消えたら閉じundoで戻っても開き直さない() {
    let (mut app, _rx) = app_with_cell(two_points());
    app.delete_launcher_cells(&[LauncherCellKey::Lane(CELL)]);
    let mut host = UiHost::no_redraw();
    drive_root_with(&mut host, &mut app, Vec::new());
    assert_eq!(app.cur.peph.automation_editor_clip, None, "描画のフレームで閉じる");

    app.handle_event(AppEvent::Undo);
    assert!(
        app.cur.song_doc.song().automation_lane_by_key(1, 1).is_some_and(|l| l.clip_by_id(CELL.clip).is_some()),
        "前提: セルが戻った"
    );
    assert_eq!(app.automation_editor_target(), None);
}

/// 閉じたあとの Delete が、もう見えないセルの点を消さない。
#[test]
fn 閉じるとセルの点は_delete_の宛先から外れる() {
    let (mut app, _rx) = app_with_cell(two_points());
    run(&mut app, &[press(at(1.0, 0.25), no_mods()), release(at(1.0, 0.25), no_mods())]);

    app.close_automation_editor();
    app.delete_current_surface(false);

    assert_eq!(points_of(&app, CELL_CONTENT).len(), 2, "見えない点は消えない");
    assert_eq!(app.automation_editor_target(), None);
}

/// アレンジのクリップの点はレーンに見えているので、閉じても選択は残る。
#[test]
fn 閉じてもアレンジのクリップの点の選択は残る() {
    let (mut app, _rx) = build_app();
    seed(&mut app, volume(), Vec::new());
    app.open_automation_editor(ARR);
    run(&mut app, &[press(at(0.0, 0.5), no_mods()), release(at(0.0, 0.5), no_mods())]);
    assert_eq!(app.cur.selection.selected_automation_points.len(), 1, "前提: 点を選んだ");

    app.close_automation_editor();

    assert_eq!(app.cur.selection.selected_automation_points.len(), 1);
}

// ---- 点の編集 (ダブルクリック / ドラッグ / Alt) ---------------------------

/// この機能の本題: 空のセルに、点を足せる。
#[test]
fn 空のセルにダブルクリックで点を足せる() {
    let (mut app, _rx) = app_with_cell(Vec::new());
    let before = app.cur.song_doc.undo_depth();

    run(&mut app, &double_click(at(2.0, 0.5), no_mods()));

    let pts = points_of(&app, CELL_CONTENT);
    assert_eq!(pts.len(), 1, "点が 1 つ増える: {pts:?}");
    assert!((pts[0].time_beat - 2.0).abs() < 1e-3, "拍 2: {:?}", pts[0]);
    assert!((pts[0].value - 1.0).abs() < 1e-3, "高さ半分 = Volume の 1.0: {:?}", pts[0]);
    assert_eq!(app.cur.song_doc.undo_depth() - before, 1, "1 回の操作 = 1 undo step");
}

/// スナップはレーンと同じ設定を使う。Alt で切れる。
#[test]
fn 点の追加はスナップしaltで切れる() {
    for (m, want) in [(no_mods(), 2.0), (alt(), 2.3)] {
        let (mut app, _rx) = app_with_cell(Vec::new());
        app.cur.view.arrange_snap_enabled = true;
        app.cur.view.arrange_snap_choice = 1; // 1/4 = 1 拍
        run(&mut app, &double_click(at(2.3, 0.5), m));
        let pts = points_of(&app, CELL_CONTENT);
        assert_eq!(pts.len(), 1, "alt={}: {pts:?}", m.alt);
        assert!((pts[0].time_beat - want).abs() < 1e-3, "alt={}: {:?}", m.alt, pts[0]);
    }
}

/// 窓の左端がずれたクリップでも、画面の拍 = 窓ローカルで content へ正しく戻る。
#[test]
fn 窓がずれたクリップも正しい位置に足す() {
    let (mut app, _rx) = build_app();
    seed(&mut app, volume(), Vec::new());
    app.edit_song(|song| {
        let lane = song.automation_lane_by_key_mut(1, 1).expect("lane");
        lane.session_clips[0].clip.content_offset_beats = 4.0;
    });
    app.open_automation_editor(CELL);

    run(&mut app, &double_click(at(1.0, 0.5), no_mods()));

    let pts = points_of(&app, CELL_CONTENT);
    assert!((pts[0].time_beat - 5.0).abs() < 1e-3, "窓の 1 拍 = content の 5 拍: {pts:?}");
}

#[test]
fn ドラッグで点を動かし離すまで確定しない() {
    let (mut app, _rx) = app_with_cell(two_points());
    let before = app.cur.song_doc.undo_depth();
    let from = at(1.0, 0.25);
    let to = at(2.0, 0.5);
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(from, no_mods()));
    drive(&mut host, &mut app, hold(to, no_mods()));
    assert_eq!(points_of(&app, CELL_CONTENT)[0].time_beat, 1.0, "ドラッグ中は model を変えない");

    drive(&mut host, &mut app, release(to, no_mods()));

    let p = points_of(&app, CELL_CONTENT)[0].clone();
    assert!((p.time_beat - 2.0).abs() < 1e-3, "{p:?}");
    assert!((p.value - 1.0).abs() < 1e-3, "{p:?}");
    assert_eq!(app.cur.song_doc.undo_depth() - before, 1);
}

/// 選択中の点を掴むと選択全部が同じだけ動く。
#[test]
fn 選択中の点を掴むと全部動く() {
    let (mut app, _rx) = app_with_cell(two_points());
    app.select_all_automation_editor_points();

    run(&mut app, &drag(at(1.0, 0.25), at(2.0, 0.25), no_mods()));

    let times: Vec<f64> = points_of(&app, CELL_CONTENT).iter().map(|p| p.time_beat).collect();
    assert!((times[0] - 2.0).abs() < 1e-3 && (times[1] - 6.0).abs() < 1e-3, "{times:?}");
}

#[test]
fn クリックで選択しctrlで足しdeleteで消す() {
    let (mut app, _rx) = app_with_cell(two_points());
    let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };
    let key = |point_idx| AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id: 2, point_idx };

    run(&mut app, &[press(at(1.0, 0.25), no_mods()), release(at(1.0, 0.25), no_mods())]);
    assert_eq!(app.cur.selection.selected_automation_points, vec![key(0)]);
    run(&mut app, &[press(at(5.0, 0.75), ctrl), release(at(5.0, 0.75), ctrl)]);
    assert_eq!(app.cur.selection.selected_automation_points, vec![key(0), key(1)]);

    app.delete_current_surface(false);

    assert!(points_of(&app, CELL_CONTENT).is_empty());
    assert_eq!(app.automation_editor_target(), Some(CELL), "セル自体は残る");
}

#[test]
fn 空きのドラッグで囲んだ点を選ぶ() {
    let (mut app, _rx) = app_with_cell(two_points());
    run(&mut app, &drag(at(0.5, 0.05), at(1.5, 0.45), no_mods()));
    assert_eq!(
        app.cur.selection.selected_automation_points,
        vec![AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id: 2, point_idx: 0 }]
    );
    // 空きの短いクリックで解除。
    run(&mut app, &[press(at(7.0, 0.9), no_mods()), release(at(7.0, 0.9), no_mods())]);
    assert!(app.cur.selection.selected_automation_points.is_empty());
}

#[test]
fn altクリックで点を消す() {
    let (mut app, _rx) = app_with_cell(two_points());
    run(&mut app, &[press(at(1.0, 0.25), alt()), release(at(1.0, 0.25), alt())]);
    let pts = points_of(&app, CELL_CONTENT);
    assert_eq!(pts.len(), 1);
    assert_eq!(pts[0].id, 2, "押した点だけが消える");
}

#[test]
fn altで線をドラッグすると曲がる() {
    let (mut app, _rx) = app_with_cell(two_points());
    // 拍 1 (norm .25) → 拍 5 (norm .75) の直線の中点。
    let mid = at(3.0, 0.5);

    run(&mut app, &[press(mid, alt()), release(mid, alt())]);
    assert_eq!(
        points_of(&app, CELL_CONTENT)[1].curve,
        AutomationCurve::Linear,
        "動かしていなければ形は変わらない"
    );

    run(&mut app, &drag(mid, (mid.0, mid.1 - 40.0), alt()));
    let AutomationCurve::Exponential { bend } = points_of(&app, CELL_CONTENT)[1].curve else {
        panic!("曲線になる: {:?}", points_of(&app, CELL_CONTENT)[1]);
    };
    assert!(bend < 0.0, "上り区間を上へ引くと bend は負: {bend}");

    // Alt+ダブルクリックで直線へ戻す (点は増やさない)。曲がった線の中点は固定点ではないので、
    // 実際の値から画面位置を出す。
    let pts = points_of(&app, CELL_CONTENT);
    let v = common::automation::apply_curve(pts[0].value, pts[1].value, 0.5, pts[1].curve);
    run(&mut app, &double_click(at(3.0, (v / 2.0) as f32), alt()));
    let pts = points_of(&app, CELL_CONTENT);
    assert_eq!(pts.len(), 2, "点は増えない: {pts:?}");
    assert_eq!(pts[1].curve, AutomationCurve::Linear);
}

#[test]
fn 点のダブルクリックで数値入力が始まる() {
    let (mut app, _rx) = app_with_cell(two_points());
    run(&mut app, &double_click(at(1.0, 0.25), no_mods()));
    assert_eq!(
        app.cur.peph.editing_automation_point,
        Some(AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id: 2, point_idx: 0 })
    );
    assert_eq!(points_of(&app, CELL_CONTENT).len(), 2, "点は増えない");
}

/// 掴んだ点の index は position なので、途中で点列が変わったら別の点を動かしてはいけない。
#[test]
fn ドラッグ中に点が消えたら確定しない() {
    let (mut app, _rx) = app_with_cell(two_points());
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(at(1.0, 0.25), no_mods()));
    drive(&mut host, &mut app, hold(at(2.0, 0.25), no_mods()));
    // ドラッグの途中で別経路 (undo 等) で点が消えた。
    app.edit_song(|song| {
        if let Some(ClipContent::Automation(a)) = song.clip_contents.get_mut(&CELL_CONTENT) {
            a.points.remove(0);
        }
    });
    drive(&mut host, &mut app, release(at(2.0, 0.25), no_mods()));
    let pts = points_of(&app, CELL_CONTENT);
    assert_eq!(pts.len(), 1);
    assert_eq!(pts[0].time_beat, 5.0, "残った別の点を動かさない");
}

/// 隣の点を追い越しても、選択は掴んだ点についてくる (sort で index が変わる)。
/// 追いかけないと、次の Delete が隣の点を消す。
#[test]
fn 点が隣を追い越しても選択はついてくる() {
    let (mut app, _rx) = app_with_cell(two_points());
    run(&mut app, &[press(at(1.0, 0.25), no_mods()), release(at(1.0, 0.25), no_mods())]);

    run(&mut app, &drag(at(1.0, 0.25), at(7.0, 0.25), no_mods()));

    let pts = points_of(&app, CELL_CONTENT);
    assert_eq!((pts[0].id, pts[1].id), (2, 1), "前提: 追い越して並びが入れ替わった");
    assert_eq!(
        app.cur.selection.selected_automation_points,
        vec![AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id: 2, point_idx: 1 }]
    );
    app.delete_current_surface(false);
    assert_eq!(points_of(&app, CELL_CONTENT)[0].id, 2, "掴んだ点だけが消える");
}

/// 押したまま Esc = ドラッグのキャンセル。離しても何も起きない。
#[test]
fn ドラッグ中のescで動かさない() {
    let (mut app, _rx) = app_with_cell(two_points());
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, press(at(1.0, 0.25), no_mods()));
    drive(&mut host, &mut app, hold(at(3.0, 0.25), no_mods()));
    let mut scene = Scene::new();
    let input = FrameInput {
        pointer: hold(at(3.0, 0.25), no_mods()),
        keyboard: vec![KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape,
            repeat: false,
        }],
        ..FrameInput::default()
    };
    let screen = PhysicalSize { width: 1000, height: 700 };
    for e in host.frame_to_edits(&app, &mut scene, screen, input, |app, ui| {
        automation_editor::draw(app, ui, AREA);
    }) {
        e.apply(&mut app);
    }
    drive(&mut host, &mut app, release(at(3.0, 0.25), no_mods()));
    assert_eq!(points_of(&app, CELL_CONTENT)[0].time_beat, 1.0);
}

/// 数値入力は本体の別の場所をクリックすると確定して閉じる (レーンの「外クリックで確定」と同じ)。
/// 入力欄の上の press は本体の操作にならない。
#[test]
fn 数値入力は本体クリックで閉じ入力欄の上では何もしない() {
    let (mut app, _rx) = app_with_cell(two_points());
    let key = AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id: 2, point_idx: 0 };
    run(&mut app, &[press(at(5.0, 0.75), no_mods()), release(at(5.0, 0.75), no_mods())]);
    let selected = app.cur.selection.selected_automation_points.clone();
    app.handle_event(AppEvent::BeginEditAutomationPointValue { key });
    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app, PointerFrame::default());

    // 点 0 の真上にある入力欄の中を押す。
    let (px, py) = at(1.0, 0.25);
    let in_field = (px, py - 18.0);
    drive(&mut host, &mut app, press(in_field, no_mods()));
    drive(&mut host, &mut app, release(in_field, no_mods()));
    assert_eq!(app.cur.peph.editing_automation_point, Some(key), "入力欄の中では閉じない");
    assert_eq!(app.cur.selection.selected_automation_points, selected, "選択も変わらない");

    let away = at(7.0, 0.9);
    drive(&mut host, &mut app, press(away, no_mods()));
    drive(&mut host, &mut app, release(away, no_mods()));
    assert_eq!(app.cur.peph.editing_automation_point, None, "外クリックで確定して閉じる");
}

fn secondary_click(p: (f32, f32)) -> [PointerFrame; 2] {
    [
        PointerFrame { pos: Some(p), secondary_just_pressed: true, ..PointerFrame::default() },
        PointerFrame { pos: Some(p), secondary_just_released: true, ..PointerFrame::default() },
    ]
}

fn menu_open(host: &mut UiHost<AppData>, app: &mut AppData) -> bool {
    let mut open = false;
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 1000, height: 700 };
    let input = FrameInput::default();
    let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
        automation_editor::draw(app, ui, AREA);
        open = ui.has_open_popups();
    });
    for e in edits {
        e.apply(app);
    }
    open
}

/// 点の右クリックでメニューが開き、項目で区間の形が変わる。外の左クリックは閉じるだけで、
/// 背面の点や選択には効かない。空きの右クリックでは開かない。
#[test]
fn 点の右クリックメニューで形を変え外クリックは閉じるだけ() {
    let (mut app, _rx) = app_with_cell(two_points());
    let mut host = UiHost::no_redraw();
    for p in secondary_click(at(7.0, 0.9)) {
        drive(&mut host, &mut app, p);
    }
    assert!(!menu_open(&mut host, &mut app), "空きでは開かない");

    let target = at(5.0, 0.75);
    for p in secondary_click(target) {
        drive(&mut host, &mut app, p);
    }
    assert!(menu_open(&mut host, &mut app), "点の上で開く");

    // メニューの外の別の点を左クリック: 閉じるだけで選択しない。
    let other = at(1.0, 0.25);
    drive(&mut host, &mut app, press(other, no_mods()));
    drive(&mut host, &mut app, release(other, no_mods()));
    assert!(!menu_open(&mut host, &mut app), "外クリックで閉じる");
    assert!(app.cur.selection.selected_automation_points.is_empty(), "背面へ抜けない");

    // 開き直して先頭項目 (階段) を選ぶ。メニューの左上はポインタの位置。
    for p in secondary_click(target) {
        drive(&mut host, &mut app, p);
    }
    assert!(menu_open(&mut host, &mut app));
    let item = (target.0 + 20.0, target.1 + 12.0);
    drive(&mut host, &mut app, PointerFrame { pos: Some(item), ..PointerFrame::default() });
    drive(&mut host, &mut app, press(item, no_mods()));
    drive(&mut host, &mut app, release(item, no_mods()));
    assert_eq!(points_of(&app, CELL_CONTENT)[1].curve, AutomationCurve::Hold);
    assert!(!menu_open(&mut host, &mut app));
}

/// メニューを開いたまま持ち主が消えても (タブ切替 / Esc でエディタを閉じる)、見えないメニューが
/// 残ってアレンジの操作を止めない。
#[test]
fn エディタが消えるとメニューも消える() {
    let (mut app, _rx) = app_with_cell(two_points());
    let mut host = UiHost::no_redraw();
    for p in secondary_click(at(1.0, 0.25)) {
        drive(&mut host, &mut app, p);
    }
    assert!(menu_open(&mut host, &mut app), "前提: 開いている");

    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 1000, height: 700 };
    let mut still_open = true;
    host.frame_to_edits(&app, &mut scene, screen, FrameInput::default(), |_, _| {});
    host.frame_to_edits(&app, &mut scene, screen, FrameInput::default(), |_, ui| {
        still_open = ui.has_open_popups();
    });
    assert!(!still_open);
}

/// Esc はまずメニューだけを閉じる。エディタは開いたまま。
#[test]
fn escはメニューを閉じてからエディタを閉じる() {
    let (mut app, _rx) = app_with_cell(two_points());
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    *host.shortcut_map_mut() = daw_gui::view::shortcuts::daw_shortcut_map();
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 1400, height: 900 };
    let mut open_at_end = false;
    for e in host.frame_to_edits(&app, &mut scene, screen, FrameInput::default(), |_, ui| {
        ui.context_menu_at(automation_editor::POINT_MENU_ID, Some((10.0, 10.0)), &["a"], |_, _| {});
    }) {
        e.apply(&mut app);
    }
    let input = FrameInput {
        keyboard: vec![KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape,
            repeat: false,
        }],
        ..FrameInput::default()
    };
    for e in host.frame_to_edits(&app, &mut scene, screen, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
        open_at_end = ui.has_open_popups();
    }) {
        e.apply(&mut app);
    }
    assert!(!open_at_end, "メニューは閉じた");
    assert_eq!(app.automation_editor_target(), Some(CELL), "エディタは開いたまま");
}

/// タブを Mixer にしているときの Esc は見えないエディタを閉じない。
#[test]
fn タブが隠れているときのescはエディタを閉じない() {
    let (mut app, _rx) = app_with_cell(two_points());
    app.handle_event(AppEvent::SelectBottomPanel(0));
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    *host.shortcut_map_mut() = daw_gui::view::shortcuts::daw_shortcut_map();
    press_escape(&mut host, &mut app);
    assert_eq!(app.automation_editor_target(), Some(CELL));
}

// ---- 表示状態 --------------------------------------------------------------

#[test]
fn ホイールで横スクロールとズームができる() {
    let (mut app, _rx) = app_with_cell(two_points());
    let body = layout().body;
    let center = (body.x + body.w * 0.5, body.y + body.h * 0.5);
    let wheel = |sy: f32, m: Modifiers| PointerFrame {
        pos: Some(center),
        scroll_delta: (0.0, sy),
        modifiers: m,
        ..PointerFrame::default()
    };
    let ctrl = Modifiers { ctrl: true, ..Modifiers::empty() };

    run(&mut app, &[wheel(200.0, ctrl)]);
    let (start, len, _) = app.automation_editor_view(CELL).expect("view");
    assert!(len < CLIP_LEN, "Ctrl+wheel でズームイン: {len}");
    assert!((start + len * 0.5 - 4.0).abs() < 1e-6, "ポインタ位置の拍を保つ: start={start} len={len}");

    run(&mut app, &[wheel(-50.0, no_mods())]);
    let (scrolled, len2, _) = app.automation_editor_view(CELL).expect("view");
    assert!(scrolled > start, "素 wheel で横スクロール: {start} → {scrolled}");
    assert_eq!(len2, len, "スクロールでズームは変わらない");

    app.fit_automation_editor();
    assert_eq!(app.automation_editor_view(CELL), Some((0.0, CLIP_LEN, CLIP_LEN)), "全体表示へ戻る");
}

// ---- コピー / 貼り付け ----------------------------------------------------

#[test]
fn 点をコピーして同じセルへ貼れる() {
    let (mut app, _rx) = app_with_cell(two_points());
    app.select_all_automation_editor_points();
    let (json, n) = app.copy_points_clip().expect("コピーできる");
    assert_eq!(n, 2);
    let env = daw_gui::clipboard::ClipboardEnvelope::from_json(&json).expect("json");
    let daw_gui::clipboard::ClipboardPayload::AutomationPoints(points) = env.payload else {
        panic!("点の payload");
    };
    // 先頭の点が窓の 3 拍に来る。1 拍 → 3 拍 / 5 拍 → 7 拍。
    let pasted = app.paste_points_into_clip(points, CELL, 3.0);
    assert_eq!(pasted, 2);
    let times: Vec<f64> = points_of(&app, CELL_CONTENT).iter().map(|p| p.time_beat).collect();
    assert_eq!(times, vec![1.0, 3.0, 5.0, 7.0]);
    assert_eq!(app.cur.selection.selected_automation_points.len(), 2, "貼った点が選択される");
}

// ---- プラグインのパラメータ (値域が 0..1 でない) ----------------------------

/// 画面の高さは `plugin_param_range` で正規化して描いているので、書くときも同じ写像で戻す。
/// 戻さないと高さ半分に点を置いても plain 0.5 になり、20〜20000 の値域の下端へ張り付く。
#[test]
fn プラグインのパラメータの値域で点を足す() {
    let (mut app, _rx) = build_app();
    let target = AutomationTarget::PluginParam { device_id: 42, param_id: 7, legacy_device_index: None };
    seed(&mut app, target, Vec::new());
    let cutoff = || PluginParamInfo {
        id: 7,
        name: "Cutoff".into(),
        module: String::new(),
        min_value: 20.0,
        max_value: 20_000.0,
        default_value: 1000.0,
        flags: 0,
    };
    app.cur.pipc.plugin_params.insert(42, vec![cutoff()]);
    app.open_automation_editor(CELL);

    run(&mut app, &double_click(at(2.0, 0.5), no_mods()));

    let p = points_of(&app, CELL_CONTENT)[0].clone();
    assert!((p.value - 10_010.0).abs() < 1.0, "高さ半分 = 値域の中点: {p:?}");

    // 時間だけ動かしても値は変わらない。
    run(&mut app, &drag(at(2.0, 0.5), at(3.0, 0.5), no_mods()));
    let p = points_of(&app, CELL_CONTENT)[0].clone();
    assert!((p.time_beat - 3.0).abs() < 1e-3, "{p:?}");
    assert!((p.value - 10_010.0).abs() < 1.0, "{p:?}");

    // 値域がまだ届いていないプラグインでも、時間だけ動かせば値を壊さない (norm は飽和している)。
    app.cur.pipc.plugin_params.remove(&42);
    run(&mut app, &drag(at(3.0, 1.0), at(4.0, 1.0), no_mods()));
    let p = points_of(&app, CELL_CONTENT)[0].clone();
    assert!((p.time_beat - 4.0).abs() < 1e-3, "{p:?}");
    assert!((p.value - 10_010.0).abs() < 1.0, "{p:?}");
    app.cur.pipc.plugin_params.insert(42, vec![cutoff()]);

    // コピー / 貼り付けも値を保つ (0..1 へ潰さない)。
    app.select_all_automation_editor_points();
    let (json, _) = app.copy_points_clip().expect("コピーできる");
    let env = daw_gui::clipboard::ClipboardEnvelope::from_json(&json).expect("json");
    let daw_gui::clipboard::ClipboardPayload::AutomationPoints(points) = env.payload else {
        panic!("点の payload");
    };
    assert_eq!(app.paste_points_into_clip(points, CELL, 5.0), 1);
    let p = points_of(&app, CELL_CONTENT)[1].clone();
    assert!((p.time_beat - 5.0).abs() < 1e-9, "{p:?}");
    assert!((p.value - 10_010.0).abs() < 1.0, "{p:?}");

    // セルごとのコピー / 貼り付けも同じ。
    app.handle_event(AppEvent::Launcher(LauncherEvent::OpenCellEditor(LauncherCellKey::Lane(CELL))));
    let (json, _) = app.copy_launcher_cells_clip().expect("セルのコピー");
    let env = daw_gui::clipboard::ClipboardEnvelope::from_json(&json).expect("json");
    let daw_gui::clipboard::ClipboardPayload::LauncherCells(copied) = env.payload else {
        panic!("セルの payload");
    };
    let pasted = app.paste_launcher_cells(
        copied,
        app.cur.song_doc.song().project_id,
        daw_gui::state::LauncherFocus {
            row: daw_gui::event_launcher::LauncherRow::Lane(CELL.lane_key()),
            scene_index: 1,
        },
        &common::model::MediaManifest::default(),
    );
    assert_eq!(pasted, 1);
    let lane = app.cur.song_doc.song().automation_lane_by_key(1, 1).expect("lane");
    let new_cell = lane.session_clips.iter().find(|c| c.clip.id != CELL.clip).expect("貼ったセル");
    let new_points = points_of(&app, new_cell.clip.content_id);
    assert!((new_points[0].value - 10_010.0).abs() < 1.0, "{new_points:?}");
}

// ---- 実機と同じ経路 (build_root) --------------------------------------------

fn drive_root_with(
    host: &mut UiHost<AppData>,
    app: &mut AppData,
    keyboard: Vec<KeyEvent>,
) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: 1400, height: 900 };
    let input = FrameInput { keyboard, ..FrameInput::default() };
    let edits = host.frame_to_edits(app, &mut scene, screen, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    for e in edits {
        e.apply(app);
    }
    scene
}

/// セルの点はレーンに描かれない。入力欄の面倒を見るアレンジ側に「点が画面外」と判断されて、
/// 開いた数値入力がすぐ消えてはいけない。
#[test]
fn セルの点の数値入力はアレンジ側に取り消されない() {
    let (mut app, _rx) = app_with_cell(two_points());
    let key = AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id: 2, point_idx: 0 };
    app.handle_event(AppEvent::BeginEditAutomationPointValue { key });
    let mut host = UiHost::no_redraw();
    for _ in 0..3 {
        drive_root_with(&mut host, &mut app, Vec::new());
    }
    assert_eq!(app.cur.peph.editing_automation_point, Some(key));
}

fn press_escape(host: &mut UiHost<AppData>, app: &mut AppData) {
    drive_root_with(
        host,
        app,
        vec![KeyEvent {
            state: ElementState::Pressed,
            text: None,
            physical_key: PhysicalKey::Escape,
            repeat: false,
        }],
    );
}

/// Esc で閉じる (Audio Editor と同じ)。タブ 1 は Piano Roll へ戻る。
/// 数値入力中の Esc は入力の取り消しだけで、もう 1 回押すと閉じる。
#[test]
fn escは数値入力を取り消してから閉じる() {
    let (mut app, _rx) = app_with_cell(two_points());
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    *host.shortcut_map_mut() = daw_gui::view::shortcuts::daw_shortcut_map();
    let key = AutomationPointKeyRef { track_id: 1, lane_id: 1, clip_id: 2, point_idx: 0 };
    app.handle_event(AppEvent::BeginEditAutomationPointValue { key });
    drive_root_with(&mut host, &mut app, Vec::new());

    press_escape(&mut host, &mut app);
    assert_eq!(app.cur.peph.editing_automation_point, None, "入力の取り消し");
    assert_eq!(app.automation_editor_target(), Some(CELL), "エディタは開いたまま");

    press_escape(&mut host, &mut app);
    assert_eq!(app.automation_editor_target(), None);
    assert_eq!(app.cur.view.bottom_panel, Some(1));
}

/// タブの中身は `build_root` まで届く: 曲線がオフスクリーンで実際に描かれる
/// (レーン色がレーン面に沈まないことも両テーマで見る)。画像は `target/automation_editor/`。
#[test]
fn 曲線と点が両テーマで見える() {
    let Ok(mut renderer) = OffscreenRenderer::new(1000, 700) else {
        eprintln!("skip pixel check: no GPU adapter/device");
        return;
    };
    for theme_id in ["dark", "light"] {
        let (mut app, _rx) = app_with_cell(two_points());
        app.handle_event(AppEvent::SetTheme(theme_id.to_string()));
        assert_eq!(app.theme.id, theme_id, "テーマが適用されている");
        // 選択中の点が 1 つある絵にする。
        run(&mut app, &[press(at(1.0, 0.25), no_mods()), release(at(1.0, 0.25), no_mods())]);
        let mut host = UiHost::no_redraw();
        let scene = drive(&mut host, &mut app, PointerFrame::default());
        let px = renderer.render_to_rgba(&scene).expect("offscreen render");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/automation_editor");
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = image::save_buffer(dir.join(format!("{theme_id}.png")), &px, 1000, 700, image::ColorType::Rgba8);
        }
        let pixel = |(x, y): (f32, f32)| {
            let i = (y as usize * 1000 + x as usize) * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        // 線の中点と、そこから真下に離れた面の色が違う = 線が見えている。
        let on_line = pixel(at(3.0, 0.5));
        let off_line = pixel(at(3.0, 0.1));
        let diff: i32 = on_line.iter().zip(off_line).map(|(a, b)| (i32::from(*a) - i32::from(b)).abs()).sum();
        assert!(diff > 60, "[{theme_id}] 曲線が面に沈んでいる: {on_line:?} vs {off_line:?}");
    }
}

#[test]
fn アレンジのクリップは曲の拍で再生位置を出し_セルは鳴っていなければ出さない() {
    let (mut app, _rx) = app_with_cell(two_points());
    app.cur.transport.playhead_beat = Some(18.0);
    assert_eq!(app.automation_editor_playhead_beat(ARR), Some(18.0));
    assert_eq!(app.automation_editor_playhead_beat(CELL), None);
}
