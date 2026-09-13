//! M-3: 同じパラメーターを複数の面が描いてもジェスチャーが途切れない (面つき所有者、
//! `docs/plan_rack_native_devices.md` §7.6 / §15.6 / §18-R)。
//!
//! 旧方式は「前フレームにドラッグしていたか」を共有集合から引いて edge を取っていたので、同じ
//! `(track, target)` を 2 つの面が描くとドラッグしていない面が毎フレーム End を出し、undo が
//! 2 フレームごとに 1 step 積まれ、録音も途切れた。ここでは実ポインタで runner と同じ順
//! (build_root → `scrub_gesture::sweep`) にフレームを回し、次を見る:
//!
//! 1. Mixer 帯の Comp Thr を掴んで動かす間、同じ Thr を Rack の面も毎フレーム描く → 所有者は
//!    ずっと Mixer、離したら閉じ、undo は +1 だけ
//! 2. Mixer のフェーダーが見えている状態でアレンジのヘッダ音量を動かす → 所有者はずっとヘッダ、undo +1
//! 3. Latch で再生中にマスターパネルの Bus Comp Thr を動かす → master の `song_lanes` にレーンと点

use std::sync::Arc;

use common::model::{
    AutomationTarget, BusCompParam, ClipContent, CompParam, MASTER_TRACK_ID, NativeKind, NativeParamId, RecordingMode,
    TrackBuiltinParam,
};
use tokio::sync::mpsc;

use daw_gui::app::{AppData, AppEvent, ParamSurface};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event::StripSection;
use daw_gui::view::native_device::{NativeKnobSpec, ParamOwner, native_knob_with_value};
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

const W: u32 = 1280;
const H: u32 = 1000;
/// `mixer_strips.rs` の strip 幅 (strip 背景の目印)。
const STRIP_WIDTH: f32 = 80.0;
/// 1 フレームあたりのドラッグ量 (px)。
const STEP: f32 = 10.0;

fn build_app() -> AppData {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    std::mem::forget((audio_rx, plugin_rx));
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000)
}

/// 同じフレームに Rack の面で描くつまみ (Rack Par のセルと同じ部品・同じ面)。
#[derive(Clone, Copy)]
struct RackKnob {
    owner_track: u32,
    device_id: u64,
    param: NativeParamId,
}

/// Rack の面のつまみを置く場所 (ポインタが通らない画面の隅)。
const RACK_KNOB_RECT: Rect = Rect { x: 2.0, y: H as f32 - 40.0, w: 24.0, h: 24.0 };
const RACK_VALUE_RECT: Rect = Rect { x: 28.0, y: H as f32 - 36.0, w: 52.0, h: 16.0 };

/// 1 フレーム回す (runner と同じ順: build_root → sweep)。edit は `app` に適用される。
fn run(host: &mut UiHost<AppData>, app: &mut AppData, pointer: PointerFrame, rack: Option<RackKnob>) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    host.frame(app, &mut scene, screen, FrameInput { pointer, ..FrameInput::default() }, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
        if let Some(k) = rack {
            let song = app.cur.song_doc.song();
            let track = song.track_by_id(k.owner_track).expect("track");
            let device = song.native_by_id(k.device_id).expect("device");
            let scope = app.live_param_scope();
            let spec = NativeKnobSpec {
                surface: ParamSurface::Rack,
                owner: ParamOwner::of_track(track),
                device,
                param: k.param,
                rect: RACK_KNOB_RECT,
                surface_bg: app.theme.core.panel,
                dimmed: false,
                external_drag: false,
                scope: &scope,
            };
            native_knob_with_value(app, ui, &spec, RACK_VALUE_RECT);
        }
    });
    daw_gui::view::scrub_gesture::sweep(app);
    scene
}

fn hover(pos: (f32, f32)) -> PointerFrame {
    PointerFrame { pos: Some(pos), ..PointerFrame::default() }
}

fn press(pos: (f32, f32)) -> PointerFrame {
    PointerFrame { pos: Some(pos), primary_just_pressed: true, primary_pressed: true, ..PointerFrame::default() }
}

fn hold(pos: (f32, f32)) -> PointerFrame {
    PointerFrame { pos: Some(pos), primary_pressed: true, ..PointerFrame::default() }
}

fn release(pos: (f32, f32)) -> PointerFrame {
    PointerFrame { pos: Some(pos), primary_just_released: true, ..PointerFrame::default() }
}

fn owner_of(app: &AppData, track: u32, target: &AutomationTarget) -> Option<ParamSurface> {
    app.cur.recording.active_param_gestures.get(&(track, target.clone())).copied()
}

/// press → `STEP` px ずつ 4 回動かす → release → 1 フレーム待つ。各フレームの前に `before_frame`
/// (再生ヘッドを進める等) を呼ぶ。`dir` は 1 フレームの移動量。press 後の各フレームで所有者を返す。
fn drag(
    host: &mut UiHost<AppData>,
    app: &mut AppData,
    at: (f32, f32),
    dir: (f32, f32),
    rack: Option<RackKnob>,
    key: (u32, &AutomationTarget),
    mut before_frame: impl FnMut(&mut AppData),
) -> Vec<Option<ParamSurface>> {
    let mut owners = Vec::new();
    before_frame(app);
    run(host, app, press(at), rack);
    for i in 1..=4 {
        before_frame(app);
        let pos = (at.0 + dir.0 * i as f32, at.1 + dir.1 * i as f32);
        run(host, app, hold(pos), rack);
        owners.push(owner_of(app, key.0, key.1));
    }
    let end = (at.0 + dir.0 * 4.0, at.1 + dir.1 * 4.0);
    before_frame(app);
    run(host, app, release(end), rack);
    run(host, app, hover(end), rack);
    owners
}

/// 描かれた glyph のうち `pred` を満たすもの (左上)。
fn find_glyph(scene: &Scene, pred: impl Fn(&str, f32, f32) -> bool) -> Option<(f32, f32)> {
    scene.iter_glyphs().find(|g| pred(g.text.as_ref(), g.left, g.top)).map(|g| (g.left, g.top))
}

#[test]
fn mixer_帯と_rack_に同じ_thr_を描いても_mixer_のドラッグは途切れず_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    let track = app.cur.song_doc.song().tracks[0].id;
    let comp = app.cur.song_doc.song().builtin_native(track, NativeKind::Comp).expect("組み込み Comp").id;
    let param = NativeParamId::Comp(CompParam::Threshold);
    let target = AutomationTarget::NativeParam { device_id: comp, param };
    let rack = Some(RackKnob { owner_track: track, device_id: comp, param });

    // 一番左の strip の「Thr Rat」行。つまみは行見出し (12px) の下、2 個を行の中央に寄せた左側。
    let scene = run(&mut host, &mut app, PointerFrame::default(), rack);
    let strip_x = scene
        .iter_rects()
        .filter(|r| (r.rect.w - STRIP_WIDTH).abs() < 0.01 && r.rect.h > 100.0)
        .map(|r| r.rect.x)
        .fold(f32::MAX, f32::min);
    let label = find_glyph(&scene, |t, x, _| t == "Thr Rat" && x >= strip_x && x < strip_x + STRIP_WIDTH)
        .expect("Mixer 帯の Comp セクションが描かれる");
    let knob = (label.0 + 22.0, label.1 + 22.0);
    let scene = run(&mut host, &mut app, hover(knob), rack);
    assert!(
        find_glyph(&scene, |t, x, _| t.starts_with("Thr ") && (x - label.0).abs() < 0.5).is_some(),
        "ポインタが Mixer 帯の Thr つまみに乗っている (行見出しが読み出しに変わる)"
    );

    let depth = app.cur.song_doc.undo_depth();
    let owners = drag(&mut host, &mut app, knob, (0.0, STEP), rack, (track, &target), |_| {});
    assert!(
        owners.iter().all(|o| *o == Some(ParamSurface::MixerStrip)),
        "press 後のどのフレームでも所有者は Mixer 帯のまま (Rack の面が閉じない): {owners:?}"
    );
    assert_eq!(owner_of(&app, track, &target), None, "離したら閉じる");
    let thr = app.cur.song_doc.song().native_by_id(comp).and_then(|d| d.param(param)).expect("Thr");
    assert!(thr < -1.0, "つまみを下げたので Thr が下がる: {thr}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

#[test]
fn mixer_が見えている状態でアレンジのヘッダ音量を動かしても途切れず_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = app.cur.song_doc.song().tracks[0].id;
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);

    // ヘッダの音量帯 (arrangement::header の `track_volume_band_track` 色の細い帯)。最初のトラック行。
    let scene = run(&mut host, &mut app, PointerFrame::default(), None);
    assert!(
        scene.iter_rects().any(|r| (r.rect.w - STRIP_WIDTH).abs() < 0.01 && r.rect.h > 100.0),
        "Mixer の strip が見えている前提"
    );
    let band_color = app.theme.core.scrim.with_alpha(0.45);
    let band = scene
        .iter_rects()
        .filter(|r| r.fill == band_color && r.rect.h < 8.0)
        .min_by(|a, b| a.rect.y.total_cmp(&b.rect.y))
        .expect("アレンジのヘッダに音量帯が描かれる")
        .rect;
    let at = (band.x + band.w * 0.5, band.y + band.h * 0.5);

    let before = app.cur.song_doc.song().tracks[0].volume;
    let depth = app.cur.song_doc.undo_depth();
    let owners = drag(&mut host, &mut app, at, (-STEP, 0.0), None, (track, &target), |_| {});
    assert!(
        owners.iter().all(|o| *o == Some(ParamSurface::ArrangementHeader)),
        "press 後のどのフレームでも所有者はヘッダのまま (Mixer フェーダーが閉じない): {owners:?}"
    );
    assert_eq!(owner_of(&app, track, &target), None, "離したフレームの sweep で閉じる");
    let after = app.cur.song_doc.song().tracks[0].volume;
    assert!(after < before, "左へ引いたので音量が下がる: {before} → {after}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

/// 数値欄はドラッグが閾値を越えた **そのフレームに** 最初の値を出す (press では出さない)。
/// その値より後に申告すると、最初の値だけが gesture の外で 1 step 積まれる。
#[test]
fn mixer_の_pan_数値欄をドラッグしても_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = app.cur.song_doc.song().tracks[0].id;
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan);

    let scene = run(&mut host, &mut app, PointerFrame::default(), None);
    let strip_x = scene
        .iter_rects()
        .filter(|r| (r.rect.w - STRIP_WIDTH).abs() < 0.01 && r.rect.h > 100.0)
        .map(|r| r.rect.x)
        .fold(f32::MAX, f32::min);
    let readout = find_glyph(&scene, |t, x, _| t == "C" && x >= strip_x && x < strip_x + STRIP_WIDTH)
        .expect("一番左の strip の Pan 数値欄 (C)");
    let at = (readout.0 + 3.0, readout.1 + 6.0);

    let depth = app.cur.song_doc.undo_depth();
    let owners = drag(&mut host, &mut app, at, (STEP, 0.0), None, (track, &target), |_| {});
    assert!(owners.iter().all(|o| *o == Some(ParamSurface::MixerStrip)), "所有者は Mixer 帯: {owners:?}");
    assert_eq!(owner_of(&app, track, &target), None, "離したら閉じる");
    let pan = app.cur.song_doc.song().tracks[0].pan;
    assert!(pan > 0.01, "右へ引いたので Pan が右へ: {pan}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

#[test]
fn latch_で再生中にマスターパネルの_bus_comp_thr_を動かすと_master_にレーンと点が入る() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let bus = app.cur.song_doc.song().builtin_native(MASTER_TRACK_ID, NativeKind::BusComp).expect("Bus Comp").id;
    let param = NativeParamId::BusComp(BusCompParam::Threshold);
    let target = AutomationTarget::NativeParam { device_id: bus, param };
    app.handle_event(AppEvent::SetRecordingMode(RecordingMode::Latch));
    let mut samples = 48_000_u64;
    let mut tick = |app: &mut AppData| {
        samples += 2_400; // 1 フレーム = 0.1 拍 (120 BPM)。録音の間引き (1/64 拍) より長い。
        let project = app.pk();
        app.handle_event(AppEvent::Tick { project, samples, preroll: 0, playing: true, recording_live: false });
    };
    tick(&mut app);
    assert!(app.cur.transport.is_playing, "再生中");

    // マスターパネルの Bus Comp: 針メーター (目盛り「20」を含む線束) の下の 1 行目「Thr Ratio Atk」。
    let scene = run(&mut host, &mut app, PointerFrame::default(), None);
    let label = find_glyph(&scene, |t, _, _| t == "Thr Ratio Atk").expect("マスターパネルの Bus Comp が描かれる");
    let row = scene
        .iter_lines()
        .filter_map(|b| b.clip_rect)
        .filter(|c| (c.x - label.0).abs() < 0.5 && c.y < label.1)
        .min_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)))
        .expect("針メーターの矩形 (= 列の幅)");
    // 3 個 (20px + 間隔 6px) を列の中央に寄せた 1 個目。見出し (13px) の下。
    let knob = (row.x + (row.w - 72.0) * 0.5 + 10.0, label.1 + 13.0 + 10.0);
    let scene = run(&mut host, &mut app, hover(knob), None);
    assert!(
        find_glyph(&scene, |t, x, _| t.starts_with("Thr ") && (x - label.0).abs() < 0.5).is_some(),
        "ポインタがマスターパネルの Thr つまみに乗っている"
    );

    let owners = drag(&mut host, &mut app, knob, (0.0, STEP), None, (MASTER_TRACK_ID, &target), &mut tick);
    assert!(owners.iter().all(|o| *o == Some(ParamSurface::MasterPanel)), "所有者はマスターパネル: {owners:?}");

    let song = app.cur.song_doc.song();
    let lane = song.song_lanes.iter().find(|l| l.target == target).expect("master の song_lanes に Thr のレーン");
    let points: usize = lane
        .clips
        .iter()
        .filter_map(|c| song.clip_contents.get(&c.content_id).and_then(ClipContent::automation_points))
        .map(<[_]>::len)
        .sum();
    assert!(points > 0, "Latch の録音で点が入る");
    assert!(
        song.tracks.iter().all(|t| t.automation_lanes.iter().all(|l| l.target != target)),
        "master の device のレーンはトラックに置かない"
    );
}
