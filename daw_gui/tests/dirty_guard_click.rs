//! `docs/plan_project_tabs.md` Q9: アプリ終了時の「全タブを順に保存確認」を **実ポインタ**で駆動する
//! 回帰網。
//!
//! 実機で「『保存せず終了』を押すと次のタブの確認が出ないまま止まる」が出た。event 直叩きの
//! テスト (`tests/app_state/project_tabs.rs`) は全部通るので、この層でしか捕まらない —
//! 原因はモーダルのボタンが `close_modal` を呼び、`Ui::modal` の close 検出が同じフレームに
//! `on_close` (= `DirtyGuardCancel`) を積んで、**次のタブ用に再武装した `dirty_guard` を
//! 消していた**こと。ここは `build_root` を合成 pointer で描き、ボタンを実際に click して
//! 次のタブの確認が立っていることまで見る。

use std::sync::Arc;

use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, DirtyGuardAction};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::event_tabs::TabEvent;
use daw_gui::shutdown::QuitRequest;
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::Scene;

const W: u32 = 1400;
const H: u32 = 900;

// `view::dirty_guard_modal` のレイアウト定数のミラー (panel は画面中央)。
const PANEL_W: f32 = 460.0;
const PANEL_H: f32 = 176.0;
const PAD: f32 = 16.0;
const BTN_H: f32 = 28.0;
const BTN_W: f32 = 116.0;
const BTN_GAP: f32 = 8.0;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
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
        None,
        48_000,
    );
    (app, audio_rx, plugin_rx)
}

fn frame(host: &mut UiHost<AppData>, app: &mut AppData, p: PointerFrame) {
    let mut scene = Scene::new();
    let screen = PhysicalSize { width: W, height: H };
    let input = FrameInput { pointer: p, ..FrameInput::default() };
    host.frame(app, &mut scene, screen, input, |app, ui| {
        daw_gui::view::root::build_root(app, ui, screen);
    });
}

fn click(host: &mut UiHost<AppData>, app: &mut AppData, x: f32, y: f32) {
    frame(
        host,
        app,
        PointerFrame {
            pos: Some((x, y)),
            primary_just_pressed: true,
            primary_pressed: true,
            ..PointerFrame::default()
        },
    );
    frame(
        host,
        app,
        PointerFrame {
            pos: Some((x, y)),
            primary_just_released: true,
            ..PointerFrame::default()
        },
    );
    // release で積んだ edit を反映させる idle frame。
    frame(host, app, PointerFrame { pos: Some((x, y)), ..PointerFrame::default() });
}

/// 「保存せず終了」ボタンの中心。
fn discard_button_center() -> (f32, f32) {
    let panel_x = ((W as f32) - PANEL_W) * 0.5;
    let panel_y = ((H as f32) - PANEL_H) * 0.5;
    let btn_y = panel_y + PANEL_H - PAD - BTN_H;
    let cancel_x = panel_x + PANEL_W - PAD - BTN_W;
    let discard_x = cancel_x - BTN_GAP - BTN_W;
    (discard_x + BTN_W * 0.5, btn_y + BTN_H * 0.5)
}

#[test]
fn 保存せず終了を押すと次の未保存タブの確認が続けて出る() {
    let (mut app, _audio_rx, _plugin_rx) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();

    // A / B ともに未保存、C は clean でアクティブ。
    let a = app.pk();
    app.cur.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::Tab(TabEvent::New));
    let b = app.pk();
    app.cur.song_doc.normalize(|_| {});
    app.handle_event(AppEvent::Tab(TabEvent::New));

    app.request_close();
    assert_eq!(app.pk(), a, "表示順の最初の未保存タブを前面に出す");
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Quit(QuitRequest::USER))
    );

    // モーダルを 1 フレーム描いてから、実際にボタンを click する。
    frame(&mut host, &mut app, PointerFrame::default());
    let (x, y) = discard_button_center();
    click(&mut host, &mut app, x, y);

    assert_eq!(app.pk(), b, "次の未保存タブへ進む");
    assert_eq!(
        app.ui_ephemeral.dirty_guard,
        Some(DirtyGuardAction::Quit(QuitRequest::USER)),
        "次のタブの確認が立っている (close_modal 由来の Cancel に消されない)"
    );
    assert!(!app.shutdown.is_shutting_down(), "まだ終了しない");
    assert!(!app.tab(a).unwrap().song_doc.is_dirty(), "答えたタブはもう聞かれない");

    // 2 つ目も「保存せず終了」→ 残りは clean なので終了シーケンスへ入る。
    frame(&mut host, &mut app, PointerFrame::default());
    click(&mut host, &mut app, x, y);

    assert!(app.ui_ephemeral.dirty_guard.is_none(), "確認は残らない");
    assert!(app.shutdown.is_shutting_down(), "全タブ答え終わったら終了する");
}
