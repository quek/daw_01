//! オートメーションレーンの行高が **描画ペインより高くならない** ことの回帰テスト。
//!
//! 「最大は画面いっぱいまで」 はもともと `ArrangementStyle` の doc が謳っていた不変条件
//! だが、drag のときにしか掛かっていなかった。上限はペイン高そのものなので、**drag の
//! あとでペインが縮むと必ず破れる** — 下部パネルを開く / 窓を小さくする / 別プロジェクト
//! を開く、のどれでも起きる。破れると、ペインより高いレーン 1 本がビューポートを占有して
//! **全トラック行が画面外へ押し出される** (トラックが消えたように見える)。
//!
//! ここでは保存済み `AutomationLane.height_px` がペインより大きいプロジェクトを組み、
//! 小さいペインで 1 フレーム描いて「行がペインに収まり、トラック行が残る」ことを見る。

use std::sync::Arc;

use common::model::{AutomationLane, AutomationTarget, TrackBuiltinParam};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::AppData;
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::widgets::arrangement::{arrangement, ArrangementRowKey};
use daw_ui_core::{FrameInput, PointerFrame, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

/// レーンが描かれるペインより十分低い widget 高さ (ruler 20 + Arranger 帯 18 を含む)。
const AREA: Rect = Rect { x: 0.0, y: 0.0, w: 1200.0, h: 400.0 };
/// 保存済みの「巨大な」レーン高 (過去に `Z` 縦ズームで画面いっぱいへ拡げた値)。
const HUGE_LANE_PX: u16 = 796;

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let ev: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let mut app = AppData::new(
        audio_tx, plugin_tx, None, None, ev, job, None, None, 48_000,
    );
    app.ui_prefs.arrange_header_w = 160.0;
    app.ui_prefs.arrange_track_row_h = 32.0;
    app.ui_prefs.arrange_track_top = 0.0;
    app.ui_prefs.launcher_layout = common::model::LauncherLayout::ArrangerOnly;
    // 1 トラック目に「保存済みで巨大な」オートメーションレーンを 1 本生やし、展開する。
    let track_id = app.song_doc.song().tracks[0].id;
    app.edit_song(|song| {
        let lane = AutomationLane {
            id: 1,
            height_px: HUGE_LANE_PX,
            visible: true,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                1.0,
            )
        };
        song.tracks[0].automation_lanes.push(lane);
    });
    app.ui_prefs.expanded_automation_tracks.insert(track_id);
    (app, audio_rx, plugin_rx)
}

/// 保存済みの巨大なレーン高は、**今のペイン高**で頭打ちになる。
#[test]
fn 保存済みの巨大なレーン高はペインに収まる() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();
    let mut scene = Scene::new();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let screen = PhysicalSize { width: AREA.w as u32, height: AREA.h as u32 };
    let input = FrameInput { pointer: PointerFrame::default(), ..FrameInput::default() };
    let mut resp = None;
    host.frame(&mut app, &mut scene, screen, input, |app, ui| {
        resp = Some(arrangement(app, ui, AREA));
    });
    let resp = resp.expect("response");
    let lanes_h = resp.lanes_rect.h;

    let lane_row = resp
        .rows
        .iter()
        .find(|r| matches!(r.key, ArrangementRowKey::Lane(_)))
        .expect("展開したレーン行が並ぶ");
    assert!(
        lane_row.height <= lanes_h + 0.5,
        "レーン行がペインより高い: 行高={} ペイン高={lanes_h} (保存値 {HUGE_LANE_PX})",
        lane_row.height
    );

    // 全トラック行がペインの中に残っている (= 画面外へ押し出されていない)。
    let last_track = resp
        .rows
        .iter()
        .filter(|r| matches!(r.key, ArrangementRowKey::Track(_)))
        .map(|r| r.content_top + r.height)
        .fold(0.0_f32, f32::max);
    assert!(
        last_track <= lanes_h + 0.5,
        "トラック行が画面外へ押し出されている: 下端={last_track} ペイン高={lanes_h}"
    );
}

/// 1 フレーム描いて response を返す (`mirror_layout` が `last_arrange_*` を埋めるので、
/// Fit はこれを 1 度通してからでないと動けない)。
fn frame(host: &mut UiHost<AppData>, app: &mut AppData) -> f32 {
    let mut scene = Scene::new();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let screen = PhysicalSize { width: AREA.w as u32, height: AREA.h as u32 };
    let input = FrameInput { pointer: PointerFrame::default(), ..FrameInput::default() };
    let mut lane_h = 0.0;
    host.frame(app, &mut scene, screen, input, |app, ui| {
        let resp = arrangement(app, ui, AREA);
        lane_h = resp
            .rows
            .iter()
            .find(|r| matches!(r.key, ArrangementRowKey::Lane(_)))
            .map_or(0.0, |r| r.height);
    });
    lane_h
}

/// **Fit を押せばレーンも一緒に縮み、その結果が保存されて開き直しても保たれる。**
///
/// 以前は Fit がレーンへ張る行高が session-only だったので、Fit → 保存 → 開き直しで
/// モデル側の巨大な高さが復活し、レーン 1 本で画面が埋まっていた (トラック行高
/// `arrange_track_row_h` は保存されるので、片側だけ Fit が効いた状態になる)。
#[test]
fn fit_したレーン高は保存されて開き直しても保たれる() {
    let (mut app, _a, _p) = build_app();
    let mut host: UiHost<AppData> = UiHost::no_redraw();

    let before = frame(&mut host, &mut app);
    assert!(before > 300.0, "前提: 保存値のせいでレーンが巨大 ({before})");

    app.handle_event(daw_gui::app::AppEvent::FitArrangeToContent);
    let fitted = frame(&mut host, &mut app);
    assert!(fitted < before * 0.5, "Fit でレーンも縮む: {before} -> {fitted}");

    // 保存 → 別セッションで開き直し。
    let snap = app.snapshot_view_state();
    assert!(
        !snap.automation_lane_row_overrides.is_empty(),
        "Fit が張ったレーン行高が保存に載る"
    );
    let (mut reopened, _a2, _p2) = build_app();
    let mut host2: UiHost<AppData> = UiHost::no_redraw();
    reopened.restore_view_state(Some(snap), common::model::LoopRegion::default());
    let after = frame(&mut host2, &mut reopened);
    assert!(
        (after - fitted).abs() < 1.5,
        "開き直しても Fit 後の行高が保たれる: fit={fitted} reopen={after}"
    );
}
