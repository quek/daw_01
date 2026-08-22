// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #63: 全体表示 (`X` キー / Fit ボタン) が **Arranger 帯のぶん下へはみ出さない**
//! ことの回帰テスト。
//!
//! 症状は「フィットしたのに最下段トラックの下が切れている」 で、原因は widget が
//! `last_arrange_*` に書き込む lanes 高さを rect 分割とは別に `area.h - RULER_H` で
//! 再導出しており、 ruler の下に確保される Arranger (section) 帯を引いていなかったこと。
//! 帯の高さは widget 内部の定数なので、 テスト側で `area.h - 20 - 18` と書くと **同じ式を
//! 写経するだけ**になり検出力が無い。 そこで widget を headless に 1 フレーム走らせ、
//! 「最下段の行の下端 == lanes 領域の下端」 という **見た目そのもの** を assert する
//! (`arr_widget.rs` と同じ `UiHost::frame` ドライブ)。

use std::sync::Arc;

use common::model::{AutomationLane, AutomationTarget, Clip, TrackBuiltinParam};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{track_with, AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};
use daw_gui::widgets::arrangement::{arrangement, ArrangementResponse};
use daw_ui_core::{FrameInput, UiHost};
use daw_ui_platform::PhysicalSize;
use daw_ui_renderer::{Rect, Scene};

/// 1080p 既定レイアウトに近い arrangement 本体の矩形。
const WIDGET_RECT: Rect = Rect { x: 0.0, y: 0.0, w: 1200.0, h: 600.0 };

fn build_app() -> (AppData, UnboundedReceiver<AudioCommand>, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
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

/// 1 フレーム走らせ、widget が発行した `Edit<AppData>` を `app` に自動 apply する。
fn drive(host: &mut UiHost<AppData>, app: &mut AppData) -> ArrangementResponse {
    let mut scene = Scene::new();
    let screen =
        PhysicalSize { width: WIDGET_RECT.w as u32, height: WIDGET_RECT.h as u32 };
    let mut captured = None;
    host.frame(app, &mut scene, screen, FrameInput::default(), |app, ui| {
        captured = Some(arrangement(app, ui, WIDGET_RECT));
    });
    captured.expect("arrangement() は毎フレーム response を返す")
}

/// track を `n` 本に増やし、各 track に 8 拍のクリップを 1 つ置く
/// (フィットの横方向が退避 fallback に落ちないように)。
fn fill_tracks(app: &mut AppData, n: usize) -> Vec<u32> {
    app.edit_song(|song| {
        while song.tracks.len() < n {
            let id = song.tracks.iter().map(|t| t.id).max().unwrap_or(0) + 1;
            song.tracks.push(track_with(|t| t.id = id));
        }
        for t in &mut song.tracks {
            t.clips.push(Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 8.0,
                ..Default::default()
            });
        }
    });
    app.song_doc.song().tracks.iter().map(|t| t.id).collect()
}

/// フィット後の最下段の行の下端と、lanes 領域の下端。
fn content_bottom_and_lanes_bottom(resp: &ArrangementResponse, track_top: f32) -> (f32, f32) {
    let last = resp.rows.last().expect("master 行が必ず居るので行は 1 つ以上");
    let content_bottom = resp.lanes_rect.y - track_top + last.content_top + last.height;
    (content_bottom, resp.lanes_rect.y + resp.lanes_rect.h)
}

/// widget が分割する縦の並びは ruler → Arranger 帯 → lanes で、隙間も重なりも無い。
/// (Arranger 帯が実在し lanes がその下から始まる = 「帯を引き忘れると 18px ずれる」の前提)
#[test]
fn lanes_は_arranger_帯の直下から始まる() {
    let (mut app, _a, _p) = build_app();
    let mut host = UiHost::no_redraw();
    let resp = drive(&mut host, &mut app);

    assert!(resp.arranger_rect.h > 0.0, "Arranger 帯は常に確保される");
    assert!(
        (resp.arranger_rect.y - (resp.ruler_rect.y + resp.ruler_rect.h)).abs() < 1e-3,
        "Arranger 帯は ruler の直下 (ruler={:?} arranger={:?})",
        resp.ruler_rect,
        resp.arranger_rect,
    );
    assert!(
        (resp.lanes_rect.y - (resp.arranger_rect.y + resp.arranger_rect.h)).abs() < 1e-3,
        "lanes は Arranger 帯の直下 (arranger={:?} lanes={:?})",
        resp.arranger_rect,
        resp.lanes_rect,
    );
    assert!(
        (resp.lanes_rect.y + resp.lanes_rect.h - (WIDGET_RECT.y + WIDGET_RECT.h)).abs() < 1e-3,
        "lanes の下端は widget 矩形の下端 (lanes={:?})",
        resp.lanes_rect,
    );
}

/// 全体表示は最下段の行の下端を lanes の下端にぴったり合わせる (はみ出しも余白も無い)。
///
/// 旧実装は lanes 高さを Arranger 帯ぶん過大に見積もっていたため、常に帯の高さぶん
/// (18px) 下へはみ出して最下段が切れていた。
#[test]
fn fit_は最下段の行の下端を_lanes_の下端に合わせる() {
    // track 数 (master 行が別に 1 行足される)。6 本以上で旧実装の 18px はみ出しが出た
    // (それ未満は行高が上限クランプに張り付いて別症状になっていた)。
    for track_count in [1usize, 6, 8, 12] {
        let (mut app, _a, _p) = build_app();
        fill_tracks(&mut app, track_count);
        let mut host = UiHost::no_redraw();
        drive(&mut host, &mut app); // 1 フレーム目でレイアウトを記録

        app.handle_event(AppEvent::FitArrangeToContent);
        let resp = drive(&mut host, &mut app); // フィット後のレイアウト

        assert_eq!(app.ui_prefs.arrange_track_top, 0.0, "fit は先頭行を上端に置く");
        assert_eq!(
            resp.rows.len(),
            track_count + 1,
            "行は master 1 + track {track_count}",
        );
        let (content_bottom, lanes_bottom) =
            content_bottom_and_lanes_bottom(&resp, app.ui_prefs.arrange_track_top);
        assert!(
            (content_bottom - lanes_bottom).abs() < 1e-3,
            "track {track_count} 本: 最下段の下端 {content_bottom} が lanes 下端 {lanes_bottom} と一致しない",
        );
    }
}

/// 展開中の automation lane があっても下端は揃う。
///
/// lane の行高は `u16` (整数 px) しか持てないので、素直に「lanes 高 / 行数」を全行に
/// 配ると丸め残差が積み上がってはみ出す。端数は f32 の track 行高が吸収する。
#[test]
fn fit_は展開_automation_lane_があっても下端を揃える() {
    let (mut app, _a, _p) = build_app();
    let track_ids = fill_tracks(&mut app, 6);
    // 先頭 2 track に可視 automation lane を 1 本ずつ足して展開する。
    app.edit_song(|song| {
        for t in song.tracks.iter_mut().take(2) {
            t.automation_lanes.push(AutomationLane {
                id: 1,
                target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                default_value: 0.0,
                enabled: true,
                visible: true,
                height_px: 60,
                clips: Vec::new(),
                next_clip_id: 1,
            });
        }
    });
    for id in track_ids.iter().take(2) {
        app.ui_prefs.expanded_automation_tracks.insert(*id);
    }

    let mut host = UiHost::no_redraw();
    drive(&mut host, &mut app);
    app.handle_event(AppEvent::FitArrangeToContent);
    let resp = drive(&mut host, &mut app);

    assert_eq!(resp.rows.len(), 6 + 1 + 2, "master 1 + track 6 + 展開 lane 2");
    let (content_bottom, lanes_bottom) =
        content_bottom_and_lanes_bottom(&resp, app.ui_prefs.arrange_track_top);
    assert!(
        (content_bottom - lanes_bottom).abs() < 1e-3,
        "最下段の下端 {content_bottom} が lanes 下端 {lanes_bottom} と一致しない",
    );
}
