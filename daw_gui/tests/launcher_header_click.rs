//! r.md #87 クリップランチャー — **シーン見出し行の当たり判定**を実ポインタで検証する。
//!
//! `launcher_wiring.rs` が `AppEvent` を直接叩くのに対し、こちらは
//! `arrangement()` widget にポインタのフレーム列を流して **「押した座標」から**
//! 確かめる。当たり判定 (`press::zone_at`) と描画 (`layout::launch_button_rect`) が
//! ずれると「見えている場所と押せる場所が違う」形で出るが、それは event 直叩きの
//! テストを全部通り抜ける。
//!
//! **見るのは widget の出力 ([`LauncherIntent`]) まで。** widget は `Song` も engine も
//! 触らず、`AppEvent` への翻訳は view 層 (`launcher_bridge`) の担当なので、ここで
//! `Song` の変化を期待するのは層をまたいだ誤った assert になる (intent → 挙動は
//! `launcher_wiring.rs` が別に固定している)。
//!
//! ここで固定する契約は 1 つ:
//!
//! > **押せる場所は列の実体の有無で変わらない。** 実体のある列もプレースホルダ列も
//! > `▶` の上だけが発火ボタンで、本体は発火しない。
//!
//! 以前はプレースホルダ列だけ本体ぜんぶがボタン (= 全行停止) で、しかも
//! `launch_scene` が transport を回すので「空きヘッダを押したら再生が始まる」に
//! なっていた。

use std::sync::Arc;

use common::model::{LauncherLayout, Track};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event_launcher::{LauncherEvent, LauncherRow};
use daw_gui::widgets::arrangement::{arrangement, ArrangementResponse, LauncherIntent};
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 900.0, h: 600.0 };

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    );
    app.ui_prefs.arrange_header_w = 0.0;
    app.ui_prefs.arrange_track_row_h = 50.0;
    app.ui_prefs.arrange_track_top = 0.0;
    // 帯を出す (このファイルは帯側の当たり判定の回帰網)。
    app.ui_prefs.launcher_layout = LauncherLayout::Both;
    app.ui_prefs.launcher_width = 320.0;
    app.ui_prefs.launcher_scene_col_w = 96.0;
    app.ui_prefs.launcher_scroll_scene = 0.0;
    // 2 トラック / 実シーン 1 本 (右側はプレースホルダ列になる) / セル 1 個。
    app.edit_song(|song| {
        song.tracks.clear();
        song.scenes.clear();
        for i in 0..2 {
            song.tracks.push(Track { id: i + 1, next_clip_id: 1, ..Track::default() });
        }
        song.ids.next_track_id = 3;
        song.push_scene();
    });
    app.handle_event(AppEvent::Launcher(LauncherEvent::CreateCell {
        row: LauncherRow::Track(1),
        scene_index: 0,
    }));
    (app, audio_rx, plugin_rx)
}

/// 1 フレーム走らせて widget の response を返す。
fn drive(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) -> ArrangementResponse {
    let mut scene = Scene::new();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let screen = PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let mut out = None;
    let input = FrameInput { pointer: p, ..FrameInput::default() };
    host.frame(app, &mut scene, screen, input, |app, ui| {
        out = Some(arrangement(app, ui, WIDGET_RECT));
    });
    out.expect("arrangement は必ず response を返す")
}

fn press(x: f32, y: f32) -> PointerFrame {
    PointerFrame {
        pos: Some((x, y)),
        primary_just_pressed: true,
        primary_pressed: true,
        ..PointerFrame::default()
    }
}

fn release(x: f32, y: f32) -> PointerFrame {
    PointerFrame { pos: Some((x, y)), primary_just_released: true, ..PointerFrame::default() }
}

/// press → release の 1 クリックを流し、**両フレームで出た intent を全部**返す。
fn click_intents(
    host: &mut UiHost<AppData>,
    app: &mut AppData,
    x: f32,
    y: f32,
) -> Vec<LauncherIntent> {
    let mut out = drive(host, app, press(x, y)).launcher.intents;
    out.extend(drive(host, app, release(x, y)).launcher.intents);
    out
}

/// 見出しの rect を `(実シーンの rect, プレースホルダ列の rect)` で返す。
fn scene_head_rects(host: &mut UiHost<AppData>, app: &mut AppData) -> (Rect, Rect) {
    let resp = drive(host, app, PointerFrame::default());
    let find = |want_real: bool| {
        resp.launcher
            .scene_rects
            .iter()
            .find(|(id, _, _)| (*id != 0) == want_real)
            .map(|(_, _, r)| *r)
    };
    (
        find(true).expect("実シーンの見出しが 1 つある"),
        find(false).expect("実シーンの右にプレースホルダ列が並ぶ"),
    )
}

/// 見出しの ▶ の中心 x。`launch_button_rect` は見出し rect の `x + 4` から 14px。
fn glyph_x(head: Rect) -> f32 {
    head.x + 8.0
}

/// 見出しの本体 (▶ から十分離れた右端寄り) の x。
fn body_x(head: Rect) -> f32 {
    head.x + head.w - 8.0
}

/// **プレースホルダ列の本体を押しても何の意図も出ない。**
///
/// 以前はここが `LaunchScene { scene_id: 0 }` (= 全行停止) で、しかも
/// `launch_scene` が transport を回すので「止めるつもりで再生が始まる」になっていた。
#[test]
fn プレースホルダ列の本体を押しても何も起きない() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let (_, ph) = scene_head_rects(&mut host, &mut app);

    let intents = click_intents(&mut host, &mut app, body_x(ph), ph.y + ph.h * 0.5);

    assert!(intents.is_empty(), "空きヘッダの本体は無反応: {intents:?}");
}

/// **プレースホルダ列の ▶ は効く** (= 全行停止 = `LaunchScene { scene_id: 0 }`)。
/// 本体を無反応にしたついでにボタンまで死んでいないことを固定する。
#[test]
fn プレースホルダ列の記号は全行停止を出す() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let (_, ph) = scene_head_rects(&mut host, &mut app);

    let intents = click_intents(&mut host, &mut app, glyph_x(ph), ph.y + ph.h * 0.5);

    assert!(
        intents
            .iter()
            .any(|i| matches!(i, LauncherIntent::LaunchScene { scene_id: 0, pressed: true })),
        "空き列の ▶ は全行停止 (scene_id 0) を出す: {intents:?}"
    );
}

/// 実体のある列も同じ分割 — 本体は発火せず、**列の選択**になる。
#[test]
fn 実シーンの本体を押すと発火ではなく列の選択になる() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let (real, _) = scene_head_rects(&mut host, &mut app);
    let scene_id = app.song_doc.song().scenes[0].id;

    let intents = click_intents(&mut host, &mut app, body_x(real), real.y + real.h * 0.5);

    assert!(
        !intents.iter().any(|i| matches!(i, LauncherIntent::LaunchScene { .. })),
        "見出しの本体クリックはシーンを撃たない: {intents:?}"
    );
    assert!(
        intents.iter().any(|i| matches!(i, LauncherIntent::SelectScene { scene_id: s, .. } if *s == scene_id)),
        "その代わり列が選択される (インスペクタが列のフォローアクションを出せる): {intents:?}"
    );
}

/// 実体のある列の ▶ はそのシーンを撃つ。
#[test]
fn 実シーンの記号はそのシーンを撃つ() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let (real, _) = scene_head_rects(&mut host, &mut app);
    let scene_id = app.song_doc.song().scenes[0].id;

    let intents = click_intents(&mut host, &mut app, glyph_x(real), real.y + real.h * 0.5);

    assert!(
        intents.iter().any(
            |i| matches!(i, LauncherIntent::LaunchScene { scene_id: s, pressed: true } if *s == scene_id)
        ),
        "▶ はそのシーンを撃つ: {intents:?}"
    );
}
