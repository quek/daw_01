// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #10: Home / End プレイヘッド移動 — AppData 側 headless 回帰。
//!
//! 検証する挙動 (`handler::transport::goto_timeline_home` / `goto_timeline_end`):
//! - Home 1 回目: 先頭 (時間的に最初) のクリップの頭へ。
//! - Home 2 回目 (= 直後): 1.1.1 (beat 0) へ (位置導出でなく flag トグル)。
//! - End: content 終端 (最後のクリップの後ろ) へ。
//! - clip が無ければ Home/End とも beat 0。
//!
//! seek は `seek_playhead_to` に一本化されているので、 ここでは AppEvent →
//! handler → `transport.playhead_beat` の往復だけを見る (audio IPC 送信は
//! receiver を生かして tolerate)。 クリップは earliest > 0 (beat 4 / 12) に置いて
//! Home の 2 段トグルが観測できるようにする。

use std::sync::Arc;

use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

/// audio / plugin の receiver を生かしたまま AppData を組む (seek が投げる
/// `SeekTo` / `LoadSong` 送信を drop 済み receiver で失わせない)。
fn build_app() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None, // plugin db 不要
        event_dispatcher,
        job_dispatcher,
        None,
        None,   // app_dirs None = 永続化なし
        48_000, // test sample rate
    );
    (app, audio_rx, plugin_rx)
}

fn playhead(app: &AppData) -> f64 {
    f64::from(app.transport.playhead_beat.expect("playhead set after seek"))
}

/// 先頭 track にクリップ (beat 4 / 12) を 2 つ置いた app。 earliest = 4。
fn app_with_clips() -> (
    AppData,
    UnboundedReceiver<AudioCommand>,
    UnboundedReceiver<PluginCommand>,
) {
    let (mut app, a, p) = build_app();
    app.edit_song(|song| {
        for t in &mut song.tracks {
            t.clips.clear();
        }
    });
    // CreateClip は track *index*。
    app.handle_event(AppEvent::CreateClip { track: 0, start_beat: 4.0 });
    app.handle_event(AppEvent::CreateClip { track: 0, start_beat: 12.0 });
    (app, a, p)
}

/// content 両端 (earliest_start, content_end)。
fn bounds(app: &AppData) -> (f64, f64) {
    common::timing::content_bounds_beats(app.song_doc.song()).expect("clips exist")
}

#[test]
fn home_toggles_between_first_clip_and_song_start() {
    let (mut app, _a, _p) = app_with_clips();
    let (first, _end) = bounds(&app);
    assert!((first - 4.0).abs() < 1e-9, "先頭クリップは beat 4 (got {first})");

    // 1 回目の Home → 先頭 (最初) のクリップ開始位置。
    app.handle_event(AppEvent::GotoTimelineHome);
    assert!(
        (playhead(&app) - first).abs() < 1e-3,
        "Home 1 回目は先頭クリップ開始 {first} (got {})",
        playhead(&app)
    );

    // 2 回目の Home (直後) → 1.1.1 (beat 0)。
    app.handle_event(AppEvent::GotoTimelineHome);
    assert!(
        playhead(&app).abs() < 1e-6,
        "Home 2 回目は先頭 beat 0 (got {})",
        playhead(&app)
    );

    // もう一度 Home → 再び先頭クリップ位置 (トグルが往復する)。
    app.handle_event(AppEvent::GotoTimelineHome);
    assert!(
        (playhead(&app) - first).abs() < 1e-3,
        "3 回目の Home は再び先頭クリップ位置 (got {})",
        playhead(&app)
    );
}

/// レビュー指摘の回帰: Home の 2 段トグルは **再生中でも** 成立する。 位置導出
/// 実装だと playhead が毎フレーム進むため 2 度目が先頭へ戻れなかった。 flag 方式は
/// playback poll (playhead_beat 直書き = seek_playhead_to を通らない) の影響を
/// 受けないので、 playhead が動いても 2 度目は必ず beat 0 へ。
#[test]
fn home_toggle_survives_playhead_movement_during_playback() {
    let (mut app, _a, _p) = app_with_clips();
    let (first, _end) = bounds(&app);

    // 1 回目 Home → 先頭クリップ位置。
    app.handle_event(AppEvent::GotoTimelineHome);
    assert!((playhead(&app) - first).abs() < 1e-3);

    // 再生 poll が playhead を先へ進めたのを模す (engine tick と同じく
    // playhead_beat を直接書き、 seek_playhead_to は通らない → flag に触れない)。
    app.transport.playhead_beat = Some((first + 4.0) as f32);

    // 2 回目 Home は playhead が動いていても先頭 (beat 0) へ戻る。
    app.handle_event(AppEvent::GotoTimelineHome);
    assert!(
        playhead(&app).abs() < 1e-6,
        "再生中 playhead 移動後も 2 度目 Home は beat 0 (got {})",
        playhead(&app)
    );
}

/// 明示 seek (ここでは End) を挟むと Home トグルはリセットされ、 次の Home は
/// また先頭クリップ位置から始まる (先頭直行にならない)。
#[test]
fn explicit_seek_resets_home_toggle() {
    let (mut app, _a, _p) = app_with_clips();
    let (first, _end) = bounds(&app);

    app.handle_event(AppEvent::GotoTimelineHome); // → first (flag true)
    app.handle_event(AppEvent::GotoTimelineEnd); // 明示 seek → flag リセット
    app.handle_event(AppEvent::GotoTimelineHome); // → また first (0 直行でない)
    assert!(
        (playhead(&app) - first).abs() < 1e-3,
        "End を挟んだ後の Home は先頭クリップ位置 (got {})",
        playhead(&app)
    );
}

#[test]
fn end_moves_to_content_end() {
    let (mut app, _a, _p) = app_with_clips();
    let (_first, end) = bounds(&app);
    assert!(end > 12.0, "content 終端は最後のクリップ start(12) + length (got {end})");

    app.handle_event(AppEvent::GotoTimelineEnd);
    assert!(
        (playhead(&app) - end).abs() < 1e-3,
        "End は content 終端 {end} (got {})",
        playhead(&app)
    );
}

/// r.md #10 追補 (user 要望): Home/End はプレイヘッド移動に合わせてアレンジを
/// 横スクロールし移動先を可視化する — Home は先頭を左端寄せ、 End は終端を右端寄せ。
#[test]
fn home_end_scroll_arrange_to_reveal_target() {
    let (mut app, _a, _p) = app_with_clips(); // clips at 4, 12
    // 可視 4 拍 (200px / 50px-per-beat) にして端寄せを観測可能にする。
    app.ui_ephemeral.last_arrange_lanes_size = (200.0, 400.0);
    app.ui_prefs.arrange_zoom_x = 50.0;
    let (first, end) = bounds(&app);
    assert!(end - first > 4.0, "content が可視幅(4拍)より広い前提");

    // Home → 先頭 (first) が左端付近 (1 拍余白)。
    app.handle_event(AppEvent::GotoTimelineHome);
    assert!(
        (f64::from(app.ui_prefs.arrange_scroll_beat) - (first - 1.0)).abs() < 1e-3,
        "Home は先頭を左端付近へ (scroll={})",
        app.ui_prefs.arrange_scroll_beat
    );

    // End → 終端 (end) が右端付近 (scroll = end - visible(4) + 1)。
    app.handle_event(AppEvent::GotoTimelineEnd);
    let expected = (end - 4.0 + 1.0).max(0.0);
    assert!(
        (f64::from(app.ui_prefs.arrange_scroll_beat) - expected).abs() < 1e-3,
        "End は終端を右端付近へ (scroll={}, expected={expected})",
        app.ui_prefs.arrange_scroll_beat
    );
}

#[test]
fn home_and_end_go_to_start_when_no_clips() {
    let (mut app, _a, _p) = build_app();
    app.edit_song(|song| {
        for t in &mut song.tracks {
            t.clips.clear();
        }
    });
    // clip 無しでは Home/End とも beat 0。
    app.handle_event(AppEvent::GotoTimelineEnd);
    assert!(playhead(&app).abs() < 1e-6, "clip 無し End は beat 0 (got {})", playhead(&app));
    app.handle_event(AppEvent::GotoTimelineHome);
    assert!(playhead(&app).abs() < 1e-6, "clip 無し Home は beat 0 (got {})", playhead(&app));
}
