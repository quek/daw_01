//! M-2: マスターパネルの組み込みブロック (Bus Comp / Tone EQ / Limiter、
//! `docs/plan_rack_native_devices.md` §10.9 / §15.6) の並びと hover の regression。
//!
//! ルート view を実ポインタで 1 フレームずつ組み、Scene の実描画と `AppData` の状態で見る:
//!
//! 1. Bus Comp と Tone EQ の上下は **master チェーン上の前後** に合わせ、Limiter は常に一番下 (Q17)
//! 2. ブロックの hover は `Q` の宛先になり、パネルを閉じた / 列が狭くてブロックを描かないフレームでは
//!    **必ず消える** (hover の publish が 1 か所、§18-AB)。低い画面 (パネル内スクロール) でも hover が届く

use std::sync::Arc;

use common::model::{ChainRef, MASTER_TRACK_ID, NativeKind};
use tokio::sync::mpsc;

use daw_gui::app::{AppData, AppEvent, InsertAt, RelocateDevices};
use daw_gui::dispatcher::{BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher};
use daw_gui::event_device::DeviceEvent;
use daw_gui::handler::bypass_target::BypassTarget;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

/// 全ブロックが描ける広さ (MASTER セクションの本体が必要高 + ラウドネス数値欄を取れる)。
const TALL: (u32, u32) = (960, 1000);
/// パネル内が縦スクロールになる低さ (セクションの最低高の合計が入らない)。
const SHORT: (u32, u32) = (960, 600);

fn build_app() -> AppData {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    std::mem::forget((audio_rx, plugin_rx));
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(audio_tx, plugin_tx, None, None, event_dispatcher, job_dispatcher, None, None, 48_000);
    assert!(app.ui_prefs.master_panel_open, "既定でマスターパネルが開いている前提");
    app
}

/// 1 フレーム回す (runner と同じ順: build_root → sweep)。edit は `app` に適用される。
fn run(host: &mut UiHost<AppData>, app: &mut AppData, (w, h): (u32, u32), pos: Option<(f32, f32)>) -> Scene {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: w, height: h };
    let input = FrameInput { pointer: PointerFrame { pos, ..PointerFrame::default() }, ..FrameInput::default() };
    host.frame(app, &mut scene, screen, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
    daw_gui::view::scrub_gesture::sweep(app);
    scene
}

fn builtin(app: &AppData, kind: NativeKind) -> u64 {
    app.cur.song_doc.song().builtin_native(MASTER_TRACK_ID, kind).expect("master の組み込み").id
}

/// マスターパネルの左端 x (画面右端に常駐する)。
fn panel_left(app: &AppData, (w, _): (u32, u32)) -> f32 {
    w as f32 - daw_gui::view::master_panel::panel_width(app)
}

/// Bus Comp の針メーター: 目盛り「20」の文字を中に含む、いちばん小さい線束の clip 矩形。
/// (パネル内スクロールのときは clip を持たない線束もスクロール領域の clip を受け取るので、
/// 「含む」だけだとスクロール領域そのものを拾う。)
fn needle_rect(scene: &Scene, left: f32) -> Option<Rect> {
    let ticks: Vec<(f32, f32)> = scene
        .iter_glyphs()
        .filter(|g| g.left >= left && g.text.as_ref() == "20")
        .map(|g| (g.left + g.font_size * 0.5, g.top + g.line_height * 0.5))
        .collect();
    scene
        .iter_lines()
        .filter_map(|b| b.clip_rect)
        .filter(|c| c.x >= left && ticks.iter().any(|&(x, y)| c.contains(x, y)))
        .min_by(|a, b| (a.w * a.h).total_cmp(&(b.w * b.h)))
}

/// Tone EQ のカーブ: パネル内で EQ カーブ色 (`strip_eq_curve`) の線束の clip 矩形。
fn curve_rect(scene: &Scene, app: &AppData, left: f32) -> Option<Rect> {
    let c = app.theme.daw.strip_eq_curve;
    scene
        .iter_lines()
        .filter(|b| b.segments.first().is_some_and(|s| s.color.r == c.r && s.color.g == c.g && s.color.b == c.b))
        .filter_map(|b| b.clip_rect)
        .filter(|r| r.x >= left)
        .min_by(|a, b| a.y.total_cmp(&b.y))
}

/// Limiter 行の見出し「Limiter Ceiling」の中心。
fn ceiling_label(scene: &Scene, left: f32) -> Option<(f32, f32)> {
    scene
        .iter_glyphs()
        .find(|g| g.left >= left && g.text.contains("Ceiling"))
        .map(|g| (g.left + g.font_size, g.top + g.line_height * 0.5))
}

fn center(r: Rect) -> (f32, f32) {
    (r.x + r.w * 0.5, r.y + r.h * 0.5)
}

#[test]
fn bus_comp_と_tone_eq_の上下はチェーン順で_limiter_は常に最下() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let left = panel_left(&app, TALL);

    let scene = run(&mut host, &mut app, TALL, None);
    let needle = needle_rect(&scene, left).expect("Bus Comp の針メーターが描かれる");
    let curve = curve_rect(&scene, &app, left).expect("Tone EQ のカーブが描かれる");
    let ceiling = ceiling_label(&scene, left).expect("Limiter 行が描かれる");
    assert!(needle.y < curve.y, "既定 (Bus Comp → Tone EQ): 針 y={} < カーブ y={}", needle.y, curve.y);
    assert!(ceiling.1 > needle.y.max(curve.y), "Limiter は一番下: Ceiling y={}", ceiling.1);

    // Tone EQ を master チェーンの先頭へ。
    let (bus, tone) = (builtin(&app, NativeKind::BusComp), builtin(&app, NativeKind::ToneEq));
    app.handle_event(AppEvent::Device(DeviceEvent::RelocateDevices(RelocateDevices {
        device_ids: vec![tone],
        dest: ChainRef::Track(MASTER_TRACK_ID),
        dest_index: InsertAt::Index(0),
        copy: false,
    })));
    let chain = &app.cur.song_doc.song().master_fx_chain;
    let pos = |id: u64| chain.iter().position(|d| d.id() == id).expect("master チェーンに居る");
    assert!(pos(tone) < pos(bus), "チェーン上で Tone EQ が Bus Comp より前に移っている");

    let scene = run(&mut host, &mut app, TALL, None);
    let needle = needle_rect(&scene, left).expect("Bus Comp の針メーターが描かれる");
    let curve = curve_rect(&scene, &app, left).expect("Tone EQ のカーブが描かれる");
    let ceiling = ceiling_label(&scene, left).expect("Limiter 行が描かれる");
    assert!(curve.y < needle.y, "Tone EQ を前へ: カーブ y={} < 針 y={}", curve.y, needle.y);
    assert!(ceiling.1 > needle.y.max(curve.y), "並べ替えても Limiter は一番下: Ceiling y={}", ceiling.1);
}

#[test]
fn ブロックの_hover_は_q_の宛先になり_描かないフレームでは必ず消える() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let left = panel_left(&app, TALL);
    let bus = builtin(&app, NativeKind::BusComp);

    let scene = run(&mut host, &mut app, TALL, None);
    let needle = center(needle_rect(&scene, left).expect("針メーター"));
    let limiter = ceiling_label(&scene, left).expect("Limiter 行");

    // Bus Comp の上 → そのブロックが Q の宛先。
    run(&mut host, &mut app, TALL, Some(needle));
    assert_eq!(app.cur.peph.master_panel_hovered, Some(BypassTarget::Device(bus)));
    // Limiter 行の上 → Limiter。
    run(&mut host, &mut app, TALL, Some(limiter));
    assert_eq!(app.cur.peph.master_panel_hovered, Some(BypassTarget::MasterLimiter));

    // パネルを閉じたフレーム (描画は早期 return) → 消える。
    run(&mut host, &mut app, TALL, Some(needle));
    assert_eq!(app.cur.peph.master_panel_hovered, Some(BypassTarget::Device(bus)));
    app.handle_event(AppEvent::ToggleMasterPanel);
    run(&mut host, &mut app, TALL, Some(needle));
    assert_eq!(app.cur.peph.master_panel_hovered, None, "閉じたパネルのブロックが Q の宛先に残る");
    app.handle_event(AppEvent::ToggleMasterPanel);

    // 数値欄の列が READOUT_MIN_W より狭い (ブロックを描かない早期 return) → 消える。
    let narrow = ((daw_gui::view::root::INSPECTOR_W + 160.0) as u32, TALL.1);
    run(&mut host, &mut app, TALL, Some(needle));
    assert_eq!(app.cur.peph.master_panel_hovered, Some(BypassTarget::Device(bus)));
    let scene = run(&mut host, &mut app, narrow, Some(needle));
    assert!(needle_rect(&scene, panel_left(&app, narrow)).is_none(), "狭いパネルではブロックを描かない前提");
    assert_eq!(app.cur.peph.master_panel_hovered, None, "描いていないブロックが Q の宛先に残る");
}

#[test]
fn 低い画面のパネル内スクロールでも_hover_が届き_描かない_limiter_は宛先に残らない() {
    let mut app = build_app();
    let mut host = UiHost::no_redraw();
    let left = panel_left(&app, SHORT);
    let bus = builtin(&app, NativeKind::BusComp);

    let scene = run(&mut host, &mut app, SHORT, None);
    // セクションの最低高で積むと MASTER の本体が低く、優先度の低い Limiter は描かれない。
    assert!(ceiling_label(&scene, left).is_none(), "低い画面では Limiter を描かない前提");
    let needle = center(needle_rect(&scene, left).expect("低い画面でも Bus Comp は描く"));

    // スクロール領域の中から hover を返す経路。
    run(&mut host, &mut app, SHORT, Some(needle));
    assert_eq!(app.cur.peph.master_panel_hovered, Some(BypassTarget::Device(bus)));

    // 前のフレームの Limiter が残っていても、描いていないフレームで消える。
    app.cur.peph.master_panel_hovered = Some(BypassTarget::MasterLimiter);
    run(&mut host, &mut app, SHORT, None);
    assert_eq!(app.cur.peph.master_panel_hovered, None);
}
