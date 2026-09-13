//! M-3 / G: 1 回の操作は undo 1 step (`docs/plan_rack_native_devices.md` §7.6 / §15.6 / §18-R)。
//!
//! 実ポインタで runner と同じ順 (build_root → `scrub_gesture::sweep`) にフレームを回し、次を見る:
//!
//! 1. **同じパラメーターを複数の面が描いても途切れない** (面つき所有者): Mixer 帯の Comp Thr を掴んで
//!    動かす間、同じ Thr を Rack の Par も毎フレーム描く → 所有者はずっと Mixer、離したら閉じ、undo +1。
//!    Mixer が見えている状態でアレンジのヘッダ音量を動かす → 所有者はずっとヘッダ、undo +1。
//! 2. **束ねる申告が最初の値より後に積まれても割れない** (daw-ui core の prelude キュー): 値の Edit を
//!    申告 (ParamGestureBegin / ScrubGesture の open) より先に出す widget — press でクリック位置へ飛ぶ
//!    ヘッダ音量、ドラッグが閾値を越えたフレームに最初の値を出す数値欄 (Mixer の Pan / Rack Par /
//!    Limiter Ceiling / インスペクタ)、変調深さのドラッグ、EQ 点のホイール Q — のどれも undo +1。
//! 3. Latch で再生中にマスターパネルの Bus Comp Thr を動かす → master の `song_lanes` にレーンと点。
//! 4. マスターパネルのフェーダー (`ScrubGesture::MasterGain`) も undo +1 で、掴んだままパネルが消えたら閉じる。

use std::sync::Arc;
use std::time::Duration;

use common::model::{
    AudioContent, AudioEvent, AudioSource, AudioSourcePath, AutomationTarget, BusCompParam, Clip, ClipContent,
    ClipKey, CompParam, EqBand, MASTER_TRACK_ID, NativeKind, NativeParamId, NativeParams, RackPanelKey,
    RecordingMode, TrackBuiltinParam,
};
use tokio::sync::mpsc;

use daw_gui::app::{AppData, AppEvent, ModSourceKindTag, ParamSurface};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event::StripSection;
use daw_gui::event_device::DeviceEvent;
use daw_gui::view::native_device::{CurveAxes, CurveBand, EqCurveSource, curve_handles};
use daw_gui::view::root::INSPECTOR_W;
use daw_gui::view::track_inspector::native_panel::layout;
use daw_gui::widgets::select_modifier::SelectModifier;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

const W: u32 = 1280;
const H: u32 = 1000;
/// `mixer_strips.rs` の strip 幅 (strip 背景の目印)。
const STRIP_WIDTH: f32 = 80.0;
/// 1 フレームあたりのドラッグ量 (px)。
const STEP: f32 = 10.0;
/// インスペクタの左右の余白 / chain list の行高 / 行の名前の上端からの距離 (`native_rack_visual.rs` と同じ)。
const RACK_PAD: f32 = 12.0;
const RACK_ROW_H: f32 = 26.0;
const NAME_TOP: f32 = 8.0;

/// 1 トラック目を選んだ app (インスペクタの Rack はこのトラックを出す)。
fn build_app() -> AppData {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    std::mem::forget((audio_rx, plugin_rx));
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
    let visible: Vec<u32> = app.cur.song_doc.song().tracks.iter().map(|t| t.id).collect();
    app.apply_select_tracks(visible[0], SelectModifier::Single, &visible);
    app
}

fn first_track(app: &AppData) -> u32 {
    app.cur.song_doc.song().tracks[0].id
}

fn builtin(app: &AppData, owner: u32, kind: NativeKind) -> u64 {
    app.cur.song_doc.song().builtin_native(owner, kind).expect("組み込み device").id
}

/// 1 フレーム回す (runner と同じ順: build_root → sweep)。edit は `app` に適用される。
fn run(host: &mut UiHost<AppData>, app: &mut AppData, pointer: PointerFrame) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    host.frame(app, &mut scene, screen, FrameInput { pointer, ..FrameInput::default() }, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    daw_gui::view::scrub_gesture::sweep(app);
    scene
}

/// idle を 2 フレーム (インスペクタの本文高さの lag-by-one を落ち着かせる)。
fn settle(host: &mut UiHost<AppData>, app: &mut AppData) -> Scene {
    let _ = run(host, app, PointerFrame::default());
    run(host, app, PointerFrame::default())
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
/// (再生ヘッドを進める等) を呼ぶ。`dir` は 1 フレームの移動量。`key` があれば press 後の各フレームの
/// 所有者を返す。
fn drag(
    host: &mut UiHost<AppData>,
    app: &mut AppData,
    at: (f32, f32),
    dir: (f32, f32),
    key: Option<(u32, &AutomationTarget)>,
    mut before_frame: impl FnMut(&mut AppData),
) -> Vec<Option<ParamSurface>> {
    let mut owners = Vec::new();
    before_frame(app);
    run(host, app, press(at));
    for i in 1..=4 {
        before_frame(app);
        let pos = (at.0 + dir.0 * i as f32, at.1 + dir.1 * i as f32);
        run(host, app, hold(pos));
        if let Some((track, target)) = key {
            owners.push(owner_of(app, track, target));
        }
    }
    let end = (at.0 + dir.0 * 4.0, at.1 + dir.1 * 4.0);
    before_frame(app);
    run(host, app, release(end));
    run(host, app, hover(end));
    owners
}

/// 描かれた glyph のうち `pred` を満たすもの (左上)。
fn find_glyph(scene: &Scene, pred: impl Fn(&str, f32, f32) -> bool) -> Option<(f32, f32)> {
    scene.iter_glyphs().find(|g| pred(g.text.as_ref(), g.left, g.top)).map(|g| (g.left, g.top))
}

/// 一番左の Mixer strip の左端 x。
fn first_strip_x(scene: &Scene) -> f32 {
    scene
        .iter_rects()
        .filter(|r| (r.rect.w - STRIP_WIDTH).abs() < 0.01 && r.rect.h > 100.0)
        .map(|r| r.rect.x)
        .fold(f32::MAX, f32::min)
}

/// インスペクタ (左カラム) に 1 つだけ描かれた `text` の左上。
fn inspector_glyph(scene: &Scene, text: &str) -> (f32, f32) {
    let v: Vec<(f32, f32)> = scene
        .iter_glyphs()
        .filter(|g| g.left < INSPECTOR_W && g.text.as_ref() == text)
        .map(|g| (g.left, g.top))
        .collect();
    assert_eq!(v.len(), 1, "インスペクタに `{text}` が 1 つ: {v:?}");
    v[0]
}

/// Rack の行の名前の glyph から、その行の Par の矩形 (行の直下、行の中身の幅)。
fn rack_panel_rect(name: (f32, f32), kind: NativeKind) -> Rect {
    Rect {
        x: RACK_PAD,
        y: name.1 - NAME_TOP + RACK_ROW_H,
        w: INSPECTOR_W - RACK_PAD * 2.0,
        h: layout::panel_height(kind),
    }
}

fn native_param(app: &AppData, id: u64, param: NativeParamId) -> f32 {
    app.cur.song_doc.song().native_by_id(id).and_then(|d| d.param(param)).expect("param")
}

/// Mixer 帯の Comp セクションの Thr つまみの中心 (行見出し「Thr Rat」の下、2 個を中央に寄せた左)。
/// ポインタを乗せて行見出しが読み出しに変わることまで確かめて返す。
fn mixer_thr_knob(host: &mut UiHost<AppData>, app: &mut AppData) -> (f32, f32) {
    let scene = settle(host, app);
    let strip_x = first_strip_x(&scene);
    let label = find_glyph(&scene, |t, x, _| t == "Thr Rat" && x >= strip_x && x < strip_x + STRIP_WIDTH)
        .expect("Mixer 帯の Comp セクションが描かれる");
    let knob = (label.0 + 22.0, label.1 + 22.0);
    let scene = run(host, app, hover(knob));
    assert!(
        find_glyph(&scene, |t, x, _| t.starts_with("Thr ") && (x - label.0).abs() < 0.5).is_some(),
        "ポインタが Mixer 帯の Thr つまみに乗っている (行見出しが読み出しに変わる)"
    );
    knob
}

#[test]
fn mixer_帯と_rack_par_に同じ_thr_を描いても_mixer_のドラッグは途切れず_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = first_track(&app);
    let comp = builtin(&app, track, NativeKind::Comp);
    let param = NativeParamId::Comp(CompParam::Threshold);
    let target = AutomationTarget::NativeParam { device_id: comp, param };
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(comp))));
    let knob = mixer_thr_knob(&mut host, &mut app);
    let scene = run(&mut host, &mut app, hover(knob));
    assert!(
        find_glyph(&scene, |t, x, _| t == "Listen" && x < INSPECTOR_W).is_some(),
        "Rack に組み込み Comp の Par が開いている (同じ Thr を毎フレーム描く)"
    );

    let depth = app.cur.song_doc.undo_depth();
    let owners = drag(&mut host, &mut app, knob, (0.0, STEP), Some((track, &target)), |_| {});
    assert!(
        owners.iter().all(|o| *o == Some(ParamSurface::MixerStrip)),
        "press 後のどのフレームでも所有者は Mixer 帯のまま (Rack の Par が閉じない): {owners:?}"
    );
    assert_eq!(owner_of(&app, track, &target), None, "離したら閉じる");
    let thr = native_param(&app, comp, param);
    assert!(thr < -1.0, "つまみを下げたので Thr が下がる: {thr}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

/// ヘッダ音量帯は press のフレームにクリック位置へ飛ぶ値を出す (申告は widget の後)。
#[test]
fn mixer_が見えている状態でアレンジのヘッダ音量を動かしても途切れず_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = first_track(&app);
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume);

    // ヘッダの音量帯 (arrangement::header の `track_volume_band_track` 色の細い帯)。最初のトラック行。
    let scene = settle(&mut host, &mut app);
    assert!(first_strip_x(&scene) < f32::MAX, "Mixer の strip が見えている前提");
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
    let owners = drag(&mut host, &mut app, at, (-STEP, 0.0), Some((track, &target)), |_| {});
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
#[test]
fn mixer_の_pan_数値欄をドラッグしても_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = first_track(&app);
    let target = AutomationTarget::TrackBuiltin(TrackBuiltinParam::Pan);

    let scene = settle(&mut host, &mut app);
    let strip_x = first_strip_x(&scene);
    let readout = find_glyph(&scene, |t, x, _| t == "C" && x >= strip_x && x < strip_x + STRIP_WIDTH)
        .expect("一番左の strip の Pan 数値欄 (C)");
    let at = (readout.0 + 3.0, readout.1 + 6.0);

    let depth = app.cur.song_doc.undo_depth();
    let owners = drag(&mut host, &mut app, at, (STEP, 0.0), Some((track, &target)), |_| {});
    assert!(owners.iter().all(|o| *o == Some(ParamSurface::MixerStrip)), "所有者は Mixer 帯: {owners:?}");
    assert_eq!(owner_of(&app, track, &target), None, "離したら閉じる");
    let pan = app.cur.song_doc.song().tracks[0].pan;
    assert!(pan > 0.01, "右へ引いたので Pan が右へ: {pan}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

#[test]
fn rack_par_の数値欄をドラッグしても_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = first_track(&app);
    let comp = builtin(&app, track, NativeKind::Comp);
    let param = NativeParamId::Comp(CompParam::Threshold);
    let target = AutomationTarget::NativeParam { device_id: comp, param };
    app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(comp))));

    // Comp の Par: [LEV|CMP|LIM] + GR → 見出し → つまみ → 数値欄。Thr は 1 列目。
    let scene = settle(&mut host, &mut app);
    let panel = rack_panel_rect(inspector_glyph(&scene, "Comp"), NativeKind::Comp);
    let value_y = panel.y + layout::PAD + layout::BAR_H + layout::ROW_GAP + layout::HEAD + layout::KNOB + layout::KNOB_GAP;
    let at = (layout::column_x(panel, 0, layout::NUM_W) + layout::NUM_W * 0.5, value_y + layout::NUM_H * 0.5);

    let depth = app.cur.song_doc.undo_depth();
    let owners = drag(&mut host, &mut app, at, (-STEP, 0.0), Some((track, &target)), |_| {});
    assert!(owners.iter().all(|o| *o == Some(ParamSurface::Rack)), "所有者は Rack: {owners:?}");
    assert_eq!(owner_of(&app, track, &target), None, "離したら閉じる");
    let thr = native_param(&app, comp, param);
    assert!(thr < -1.0, "左へ引いたので Thr が下がる: {thr}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

#[test]
fn マスターパネルの_limiter_ceiling_数値欄をドラッグしても_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let target = AutomationTarget::MasterLimiter(common::model::MasterLimiterParam::Ceiling);

    let scene = settle(&mut host, &mut app);
    let field = find_glyph(&scene, |t, x, _| t.starts_with("-1.0") && x > W as f32 * 0.5)
        .expect("マスターパネルの Ceiling 数値欄 (-1.0 dB)");
    let at = (field.0 + 4.0, field.1 + 6.0);

    let depth = app.cur.song_doc.undo_depth();
    let owners = drag(&mut host, &mut app, at, (-STEP, 0.0), Some((MASTER_TRACK_ID, &target)), |_| {});
    assert!(owners.iter().all(|o| *o == Some(ParamSurface::MasterPanel)), "所有者はマスターパネル: {owners:?}");
    let ceiling = app.cur.song_doc.song().master_limiter.ceiling_db;
    assert!(ceiling < -1.5, "左へ引いたので Ceiling が下がる: {ceiling}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

/// マスターパネルのフェーダー (つまみ = 高さ 10・幅 28 の面) の中心。
fn master_fader_thumb(app: &AppData, scene: &Scene) -> (f32, f32) {
    let left = W as f32 - daw_gui::view::master_panel::panel_width(app);
    let r = scene
        .iter_rects()
        .find(|r| r.rect.x >= left && (r.rect.h - 10.0).abs() < 0.01 && (r.rect.w - 28.0).abs() < 0.01)
        .expect("マスターフェーダーのつまみ")
        .rect;
    (r.x + r.w * 0.5, r.y + r.h * 0.5)
}

/// X3: マスターパネルのフェーダーも ScrubGesture の口 (同じフレームの値より先に開く / 描かれなくなったら
/// sweep が閉じる) を通る。1 ドラッグ = undo 1 step で、ドラッグ中にパネルを閉じても bracket が開いたまま
/// 残らない (残ると以降の編集が全部 1 step に束ねられる)。
#[test]
fn マスターパネルのフェーダーをドラッグしても_undo_は_1_step_で_パネルが消えたら閉じる() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let scene = settle(&mut host, &mut app);
    let at = master_fader_thumb(&app, &scene);

    let depth = app.cur.song_doc.undo_depth();
    drag(&mut host, &mut app, at, (0.0, STEP), None, |_| {});
    let gain = app.cur.song_doc.song().master_gain;
    assert!(gain < 1.0, "下へ引いたので master gain が下がる: {gain}");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
    assert!(!app.cur.song_doc.gesture_active(), "離したら閉じる");

    // 掴んだままパネルを閉じる → 描かれなくなったフレームで閉じる。
    let scene = settle(&mut host, &mut app);
    let at = master_fader_thumb(&app, &scene);
    run(&mut host, &mut app, press(at));
    run(&mut host, &mut app, hold((at.0, at.1 + STEP)));
    assert!(app.cur.song_doc.gesture_active(), "前提: ドラッグ中は開いている");
    app.handle_event(AppEvent::ToggleMasterPanel);
    run(&mut host, &mut app, hold((at.0, at.1 + STEP * 2.0)));
    assert!(!app.cur.song_doc.gesture_active(), "フェーダーが描かれなくなったら bracket を閉じる");
}

/// 変調深さのドラッグ: ◉ で待受にしたソースを、つまみの縦ドラッグで割り当てて深さを決める。
/// 深さの最初の値 (AddModRouting + SetModRoutingDepth) は ScrubGesture を開くより先に積まれる。
/// 1 フレーム 3px (閾値 4px 未満) で動かすので、閾値を越える前のフレームも通る (つまみは閾値を
/// 越える前に深さを出さない)。
#[test]
fn 変調深さのドラッグは_undo_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = first_track(&app);
    let comp = builtin(&app, track, NativeKind::Comp);
    let target = AutomationTarget::NativeParam { device_id: comp, param: NativeParamId::Comp(CompParam::Threshold) };
    app.handle_event(AppEvent::AddModSource { kind: ModSourceKindTag::Lfo });
    let source = app.cur.song_doc.song().mod_sources.last().expect("LFO").id;
    app.handle_event(AppEvent::ToggleStripSection(StripSection::Comp));
    let knob = mixer_thr_knob(&mut host, &mut app);
    app.handle_event(AppEvent::SetArmedModSource(Some(source)));

    let depth = app.cur.song_doc.undo_depth();
    drag(&mut host, &mut app, knob, (0.0, 3.0), None, |_| {});
    let routing = app.cur.song_doc.song().tracks[0]
        .mod_routings
        .iter()
        .find(|r| r.source_id == source && r.target == target)
        .expect("ドラッグで Thr に routing ができる");
    assert!(routing.depth.abs() > 0.01, "深さが付く: {}", routing.depth);
    assert_eq!(app.cur.peph.armed_mod_source, None, "離したら待受は解除される");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

/// インスペクタの数値欄 (ScrubGesture の bracket): 選んだ audio クリップの Gain dB。
#[test]
fn インスペクタの数値欄をドラッグしても_undo_は_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = first_track(&app);
    app.edit_song(|song| {
        let src = song.alloc_audio_source_id();
        song.media.audio_sources.insert(
            src,
            AudioSource {
                path: AudioSourcePath::Generated { id: 1 },
                sample_rate: 48_000,
                channels: 2,
                frames: 8 * 24_000,
                original_bpm: None,
                root_key: None,
            },
        );
        let content = ClipContent::Audio(AudioContent {
            events: vec![AudioEvent {
                id: 1,
                source_id: src,
                event_length_beats: 8.0,
                source_end_frames: 8 * 24_000,
                ..AudioEvent::default()
            }],
            next_event_id: 2,
        });
        let cid = song.alloc_content(content, "audio".to_string());
        song.tracks[0].clips = vec![Clip { id: 1, start_beat: 0.0, length_beats: 8.0, content_id: cid, ..Default::default() }];
    });
    app.handle_event(AppEvent::SetClipSelection(vec![ClipKey { track_id: track, clip_id: 1 }]));

    let scene = settle(&mut host, &mut app);
    let label = inspector_glyph(&scene, "Gain dB");
    let field = find_glyph(&scene, |t, x, y| t == "0.0" && x > label.0 + 40.0 && x < INSPECTOR_W && (y - label.1).abs() < 8.0)
        .expect("Gain dB の数値欄 (0.0)");
    let at = (field.0 + 4.0, field.1 + 6.0);
    let gain = |app: &AppData| match app.cur.song_doc.song().clip_contents.values().next() {
        Some(ClipContent::Audio(a)) => a.events[0].gain_db,
        other => panic!("audio content が無い: {other:?}"),
    };

    let depth = app.cur.song_doc.undo_depth();
    drag(&mut host, &mut app, at, (-STEP, 0.0), None, |_| {});
    assert!(gain(&app) < -1.0, "左へ引いたので Gain が下がる: {}", gain(&app));
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "1 ドラッグ = 1 undo step");
}

/// EQ の Par のカーブ上の点のホイール = Q。ホイールの値はカーブ側で積まれ、つまみのセルの申告
/// (`external_drag`) は同じフレームの後に積まれる。窓 (400ms) の中の 2 notch は 1 step。
#[test]
fn eq_点のホイール_q_は最初の_notch_から_undo_1_step() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let track = first_track(&app);
    let eq = builtin(&app, track, NativeKind::Eq);
    let target = AutomationTarget::NativeParam { device_id: eq, param: NativeParamId::Eq { band: EqBand::Lmf, param: common::model::EqParam::Q } };
    app.handle_event(AppEvent::Device(DeviceEvent::ToggleRackPanel(RackPanelKey::Device(eq))));
    let scene = settle(&mut host, &mut app);
    let panel = rack_panel_rect(inspector_glyph(&scene, "EQ"), NativeKind::Eq);
    let dev = *app.cur.song_doc.song().native_by_id(eq).expect("EQ");
    let src = EqCurveSource::from_params(&dev.params).expect("EQ");
    let pos = curve_handles(&src, &CurveAxes::for_source(&src), layout::curve_rect(panel))
        .into_iter()
        .find(|h| h.band == CurveBand::Eq(EqBand::Lmf))
        .expect("LMF の点")
        .pos;
    let q = |app: &AppData| match app.cur.song_doc.song().native_by_id(eq).map(|d| d.params) {
        Some(NativeParams::Eq(e)) => e.lmf.q,
        _ => panic!("EQ"),
    };
    let q_before = q(&app);

    let depth = app.cur.song_doc.undo_depth();
    let _ = run(&mut host, &mut app, hover(pos)); // ホイールの claim は前フレームの宣言を読む
    for _ in 0..2 {
        let _ = run(&mut host, &mut app, PointerFrame { scroll_delta: (0.0, -40.0), ..hover(pos) });
    }
    assert_eq!(owner_of(&app, track, &target), Some(ParamSurface::Rack), "ホイールの窓の間は Q の gesture が開いている");
    std::thread::sleep(Duration::from_millis(450)); // 窓 (400ms) を閉じる
    let _ = run(&mut host, &mut app, hover(pos));
    let _ = run(&mut host, &mut app, hover(pos));
    assert_eq!(owner_of(&app, track, &target), None, "窓が閉じたら gesture も閉じる");
    assert_ne!(q(&app), q_before, "ホイールで Q が変わる");
    assert_eq!(app.cur.song_doc.undo_depth(), depth + 1, "窓の中のホイールは 1 undo step");
}

#[test]
fn latch_で再生中にマスターパネルの_bus_comp_thr_を動かすと_master_にレーンと点が入る() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let bus = builtin(&app, MASTER_TRACK_ID, NativeKind::BusComp);
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
    let scene = settle(&mut host, &mut app);
    let label = find_glyph(&scene, |t, _, _| t == "Thr Ratio Atk").expect("マスターパネルの Bus Comp が描かれる");
    let row = scene
        .iter_lines()
        .filter_map(|b| b.clip_rect)
        .filter(|c| (c.x - label.0).abs() < 0.5 && c.y < label.1)
        .min_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)))
        .expect("針メーターの矩形 (= 列の幅)");
    // 3 個 (20px + 間隔 6px) を列の中央に寄せた 1 個目。見出し (13px) の下。
    let knob = (row.x + (row.w - 72.0) * 0.5 + 10.0, label.1 + 13.0 + 10.0);
    let scene = run(&mut host, &mut app, hover(knob));
    assert!(
        find_glyph(&scene, |t, x, _| t.starts_with("Thr ") && (x - label.0).abs() < 0.5).is_some(),
        "ポインタがマスターパネルの Thr つまみに乗っている"
    );

    let owners = drag(&mut host, &mut app, knob, (0.0, STEP), Some((MASTER_TRACK_ID, &target)), &mut tick);
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
