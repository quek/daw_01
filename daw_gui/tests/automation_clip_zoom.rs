//! arrangement の「選択素材へのズーム / ループ」 (`Z` / `R`) が
//! automation clip を第一級の選択面として扱うことの回帰テスト。
//!
//! automation clip は通常 clip (`selected_clips`) と直交した独立選択集合
//! (`selected_automation_clips`) なので、 以前は「選択 clip 群の span」を
//! `selected_clips` だけから算出していた `Z` (zoom-to-selection) /
//! `R` (loop-to-selection) が、 automation clip しか選択していないとき
//! span を解決できず no-op になっていた (= 「automation clip で z が効かない」)。
//!
//! 検証する状態機械 (`AppData`):
//! - `ZoomArrangeToSelectedClip` 1 回目 = 横ズーム (`arrange_zoom_x` /
//!   `arrange_scroll_beat`)、 2 回目 = 縦ズーム (`arrange_track_row_h` /
//!   `arrange_track_top`)。
//! - `LoopSelectedClipToggle` = 選択素材の bounding range を loop に設定。

use std::sync::Arc;

use common::model::{
    AutomationClip, AutomationClipKey, AutomationLane, AutomationLaneKey, AutomationTarget, Clip,
    ClipKey, MASTER_TRACK_ID, TrackBuiltinParam,
};
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let event_dispatcher_dyn: Arc<dyn BackgroundDispatcher> = event_dispatcher.clone();
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher_dyn,
        job_dispatcher,
        None,
        // app_dirs: None = 永続化なし (実 recent*.json を汚染しない)。
        None,
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, plugin_rx)
}

/// `track_idx` の track に automation lane (`lane_id`) を 1 本足し、 その中に
/// `[start, start+len)` の automation clip (`clip_id`) を置く。
fn add_track_automation_clip(
    app: &mut AppData,
    track_idx: usize,
    lane_id: u32,
    clip_id: u32,
    start: f64,
    len: f64,
) {
    app.edit_song(|song| {
        song.tracks[track_idx].automation_lanes.push(AutomationLane {
            id: lane_id,
            target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
            default_value: 0.0,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: vec![AutomationClip {
                id: clip_id,
                name: String::new(),
                start_beat: start,
                length_beats: len,
                content_id: 0,
                content_offset_beats: 0.0,
            }],
            next_clip_id: clip_id + 1,
        });
    });
}

#[test]
fn z_横ズームは選択された_automation_clip_を対象にする() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    add_track_automation_clip(&mut app, 0, 1, 1, 8.0, 4.0);
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);
    // 通常 clip は未選択、 automation clip のみ選択 (= 以前の no-op ケース)。
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: track_id,
        lane: 1,
        clip: 1,
    }];

    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });

    // span = 4、 pad = max(4*0.04, 0.5) = 0.5。
    // scroll = (8 - 0.5).max(0) = 7.5、 zoom_x = 800 / (4 + 0.5*2) = 160。
    assert_eq!(app.ui_prefs.arrange_scroll_beat, 7.5);
    assert_eq!(app.ui_prefs.arrange_zoom_x, 160.0);
}

#[test]
fn z_縦ズームは_automation_clip_の_track_を対象にする() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    add_track_automation_clip(&mut app, 0, 1, 1, 8.0, 4.0);
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: track_id,
        lane: 1,
        clip: 1,
    }];

    // 1 回目 = 横、 2 回目 = 縦。
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });

    // 可視 track は track[0] のみ → 行 1 (行 0 = master)。 rows = 1、
    // row_h = (600/1).clamp(16, 2000) = 600、 track_top = 1 * 600 = 600。
    assert_eq!(app.ui_prefs.arrange_track_row_h, 600.0);
    assert_eq!(app.ui_prefs.arrange_track_top, 600.0);
}

#[test]
fn z_縦ズームは_master_行の_automation_clip_を行_0_として扱う() {
    let (mut app, _rx) = build_app();
    // master (song-level) lane に automation clip を置く。
    app.edit_song(|song| {
        song.song_lanes.push(AutomationLane {
            id: 1,
            target: AutomationTarget::SongTempo,
            default_value: 120.0,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: vec![AutomationClip {
                id: 1,
                name: String::new(),
                start_beat: 8.0,
                length_beats: 4.0,
                content_id: 0,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
        });
    });
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: MASTER_TRACK_ID,
        lane: 1,
        clip: 1,
    }];

    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true }); // 横
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true }); // 縦

    // master = 行 0 → rows = 1、 row_h = 600、 track_top = 0 (master を上端に)。
    assert_eq!(app.ui_prefs.arrange_track_row_h, 600.0);
    assert_eq!(app.ui_prefs.arrange_track_top, 0.0);
}

#[test]
fn z_は対象面ごとに片方の選択だけを_framing_する() {
    // 報告バグの回帰: 通常 clip と automation clip は直交して同時選択できるので、
    // 「MIDI clip を選んだのに残存 automation 選択へズームしてしまう」 を防ぐ。
    // union ではなく root の edit_surface arbiter が選んだ片面だけを対象にする。
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    // 通常 MIDI clip を beat 0..4 に。
    app.edit_song(|song| {
        song.tracks[0].clips.push(Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 4.0,
            ..Default::default()
        });
    });
    app.selection.selected_clips = vec![ClipKey { track_id, clip_id: 1 }];
    // automation clip を beat 8..12 に。 両方が同時選択された状態。
    add_track_automation_clip(&mut app, 0, 1, 1, 8.0, 4.0);
    app.selection.selected_automation_clips = vec![AutomationClipKey { track: track_id, lane: 1, clip: 1 }];
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);

    // 対象面 = clip → MIDI clip (0..4) だけを framing (automation を巻き込まない)。
    // span 4, pad 0.5 → scroll = (0-0.5).max(0) = 0、 zoom = 800/(4+1) = 160。
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: false });
    assert_eq!(app.ui_prefs.arrange_scroll_beat, 0.0);
    assert_eq!(app.ui_prefs.arrange_zoom_x, 160.0);

    // 対象面 = automation → automation clip (8..12) だけを framing。 選択集合は
    // 同じだが対象面が変わるので段階を仕切り直し、 新 clip へ横ズームし直す。
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    assert_eq!(app.ui_prefs.arrange_scroll_beat, 7.5);
    assert_eq!(app.ui_prefs.arrange_zoom_x, 160.0);
}

#[test]
fn z_縦ズームは選択_automation_clip_のレーンを画面いっぱいに拡大する() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    add_track_automation_clip(&mut app, 0, 1, 1, 8.0, 4.0);
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: track_id,
        lane: 1,
        clip: 1,
    }];
    // view が毎フレーム算出する「primary レーンの実 content-Y 上端」を模す
    // (実機では widget の automation_lane_rects 由来)。
    let lane_key = AutomationLaneKey {
        track: track_id,
        lane: 1,
    };
    app.ui_ephemeral.arrange_primary_lane_content_top = Some((lane_key, 250.0));

    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true }); // 横
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true }); // 縦 = レーン拡大

    // レーン高 override = viewport 高 (600)、 scroll はレーン上端 (250) へ。
    assert_eq!(app.ui_prefs.automation_lane_row_overrides.get(&lane_key).copied(), Some(600));
    assert_eq!(app.ui_prefs.arrange_track_top, 250.0);
}

/// 報告バグの回帰: レーン拡大 (lane-fill) の後、
/// - 別対象へ Z し直すと一時 override は破棄される (fresh 横ズームの起点)。
/// - X で全体フィットすると、 automation レーンも track と同じ fit 行高へ scale される
///   (= 「track だけ縮んで automation レーンだけ高いまま」 を解消)。 拡大の 600 が
///   残ってはいけない。
#[test]
fn lane_拡大は_fresh_zoom_で破棄_fit_で行高に_scale_される() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    // lane 1 (clip 1 @ 8..12) と通常 MIDI clip (@ 0..4)。 lane を展開状態にする
    // (= 実機で automation clip を選べる前提)。
    app.edit_song(|song| {
        song.tracks[0].automation_lanes.push(AutomationLane {
            id: 1,
            target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
            default_value: 0.0,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: vec![AutomationClip { id: 1, name: String::new(), start_beat: 8.0, length_beats: 4.0, content_id: 0, content_offset_beats: 0.0 }],
            next_clip_id: 2,
        });
    });
    app.edit_song(|song| song.tracks[0].clips.push(Clip { id: 1, start_beat: 0.0, length_beats: 4.0, ..Default::default() }));
    app.ui_prefs.expanded_automation_tracks.insert(track_id);
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);
    let lane_key = AutomationLaneKey { track: track_id, lane: 1 };
    app.ui_ephemeral.arrange_primary_lane_content_top = Some((lane_key, 250.0));

    // automation clip を選んで Z×2 → レーン拡大 (override = viewport 高 600)。
    app.selection.selected_automation_clips = vec![AutomationClipKey { track: track_id, lane: 1, clip: 1 }];
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    assert_eq!(app.ui_prefs.automation_lane_row_overrides.get(&lane_key).copied(), Some(600));

    // 別対象 (MIDI clip) を選んで fresh な Z → 横ズーム開始で override 破棄。
    app.selection.selected_clips = vec![ClipKey { track_id, clip_id: 1 }];
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: false });
    assert!(
        app.ui_prefs.automation_lane_row_overrides.is_empty(),
        "fresh zoom should drop the lane-fill override, got {:?}",
        app.ui_prefs.automation_lane_row_overrides
    );

    // 再度レーン拡大してから全体フィット → レーンは拡大 600 ではなく fit 行高へ縮む。
    // row_count = master(1) + visible track(1) + visible lane(1) = 3、
    // row_h = (600/3).clamp(16,96) = 96 → lane override も 96 (track と同じ高さ)。
    app.selection.selected_clips.clear();
    app.selection.selected_automation_clips = vec![AutomationClipKey { track: track_id, lane: 1, clip: 1 }];
    app.ui_ephemeral.arrange_primary_lane_content_top = Some((lane_key, 250.0));
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    assert_eq!(app.ui_prefs.automation_lane_row_overrides.get(&lane_key).copied(), Some(600));
    app.handle_event(AppEvent::FitArrangeToContent);
    assert_eq!(
        app.ui_prefs.automation_lane_row_overrides.get(&lane_key).copied(),
        Some(96),
        "fit should scale the lane to the fitted row height (not leave it at 600)",
    );
    assert_eq!(app.ui_prefs.arrange_track_row_h, 96.0);
}

#[test]
fn z_は別_clip_を選び直すと新しい選択へ横ズームし直す() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    // lane 1 に 2 つの automation clip (beat 8..12 と 20..24)。
    app.edit_song(|song| {
        song.tracks[0].automation_lanes.push(AutomationLane {
            id: 1,
            target: AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
            default_value: 0.0,
            enabled: true,
            visible: true,
            height_px: 60,
            clips: vec![
                AutomationClip { id: 1, name: String::new(), start_beat: 8.0, length_beats: 4.0, content_id: 0, content_offset_beats: 0.0 },
                AutomationClip { id: 2, name: String::new(), start_beat: 20.0, length_beats: 4.0, content_id: 0, content_offset_beats: 0.0 },
            ],
            next_clip_id: 3,
        });
    });
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);

    // clip 1 (8..12) を選択して Z → 横ズーム。
    app.selection.selected_automation_clips = vec![AutomationClipKey { track: track_id, lane: 1, clip: 1 }];
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    assert_eq!(app.ui_prefs.arrange_scroll_beat, 7.5);

    // clip 2 (20..24) を選び直して Z → 段階を引き継がず新 clip へ横ズームし直す。
    app.selection.selected_automation_clips = vec![AutomationClipKey { track: track_id, lane: 1, clip: 2 }];
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    // 20..24 の横ズーム: pad 0.5 → scroll 19.5。 縦ズーム (no-op on scroll) のままなら 7.5 で落ちる。
    assert_eq!(app.ui_prefs.arrange_scroll_beat, 19.5);
    assert_eq!(app.ui_prefs.arrange_zoom_x, 160.0);
}

#[test]
fn z_はマウスでズームを変えた後に横ズームし直す() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    add_track_automation_clip(&mut app, 0, 1, 1, 8.0, 4.0);
    app.ui_ephemeral.last_arrange_canvas_size = (800.0, 600.0);
    app.selection.selected_automation_clips = vec![AutomationClipKey { track: track_id, lane: 1, clip: 1 }];

    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true }); // 横ズーム (zoom 160)
    assert_eq!(app.ui_prefs.arrange_zoom_x, 160.0);

    // ユーザーがマウスホイールでズームを変えた状況を模す。
    app.ui_prefs.arrange_zoom_x = 50.0;
    // 同じ選択でも view が手動変更されたので、 Z は縦に進まず横ズームし直す。
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    assert_eq!(app.ui_prefs.arrange_zoom_x, 160.0);
    assert_eq!(app.ui_prefs.arrange_scroll_beat, 7.5);
}

#[test]
fn r_loop_は選択された_automation_clip_を対象にする() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    add_track_automation_clip(&mut app, 0, 1, 1, 8.0, 4.0);
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: track_id,
        lane: 1,
        clip: 1,
    }];

    app.handle_event(AppEvent::LoopSelectedClipToggle { automation: true });

    assert_eq!(app.transport.loop_region.start_beat, 8.0);
    assert_eq!(app.transport.loop_region.end_beat, 12.0);
    assert!(app.transport.loop_region.enabled);
}
