//! アレンジの Ctrl+A 段階拡大 (`AppData::select_all_arrangement`、grill-me 2026-09-06)。
//!
//! 段の判定 (「今の範囲が前段と一致するか」)・外接の計算・閉じた lane / master の
//! song lane を含めること、をコマンド層で検証する。 マウス位置 → 対象トラックの
//! 配線は `view/root.rs` で、実機確認が担当。

use std::sync::Arc;

use common::model::{
    AutomationClip, AutomationLane, AutomationLaneKey, AutomationTarget, Clip, LaneRef,
    TimeSelection, TrackBuiltinParam, MASTER_TRACK_ID,
};
use common::protocol::{AudioCommand, PluginCommand};
use tokio::sync::mpsc;

use daw_gui::app::{track_with, AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

fn build_app() -> AppData {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel::<AudioCommand>();
    let (plugin_tx, _plugin_rx) = mpsc::unbounded_channel::<PluginCommand>();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        None,
        48_000,
    )
}

fn auto_lane(id: u32, clip: (f64, f64)) -> AutomationLane {
    AutomationLane {
        id,
        clips: vec![AutomationClip {
            id: 1,
            name: String::new(),
            start_beat: clip.0,
            length_beats: clip.1,
            content_id: 0,
            content_offset_beats: 0.0,
            color: None,
        }],
        next_clip_id: 2,
        ..AutomationLane::new(AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume), 0.0)
    }
}

/// track 1: clip [4,12) + lane 1 (閉じたまま) に automation clip [16,20)
/// track 2: clip [0,8)
/// track 3: 空
/// master: song lane 1 に automation clip [24,28)
fn setup(app: &mut AppData) {
    app.edit_song(|song| {
        song.tracks.clear();
        song.tracks.push(track_with(|t| {
            t.id = 1;
            t.clips = vec![Clip { id: 10, start_beat: 4.0, length_beats: 8.0, ..Clip::default() }];
            t.automation_lanes = vec![auto_lane(1, (16.0, 4.0))];
        }));
        song.tracks.push(track_with(|t| {
            t.id = 2;
            t.clips = vec![Clip { id: 20, start_beat: 0.0, length_beats: 8.0, ..Clip::default() }];
        }));
        song.tracks.push(track_with(|t| t.id = 3));
        song.song_lanes.push(AutomationLane {
            ..auto_lane(1, (24.0, 4.0))
        });
        if let Some(l) = song.song_lanes.last_mut() {
            l.target = AutomationTarget::SongTempo;
        }
    });
}

fn press(app: &mut AppData, track: Option<u32>) {
    app.handle_event(AppEvent::SelectAllArrangement { track });
}

fn lane(track: u32, lane: u32) -> LaneRef {
    LaneRef::Automation(AutomationLaneKey { track, lane })
}

fn track1_only() -> TimeSelection {
    TimeSelection::new(4.0, 20.0, vec![LaneRef::Track(1), lane(1, 1)]).unwrap()
}

fn everything() -> TimeSelection {
    TimeSelection::new(
        0.0,
        28.0,
        vec![LaneRef::Track(1), lane(1, 1), LaneRef::Track(2), LaneRef::Track(3), lane(MASTER_TRACK_ID, 1)],
    )
    .unwrap()
}

#[test]
fn first_press_covers_the_track_and_its_closed_lanes_second_press_everything() {
    let mut app = build_app();
    setup(&mut app);

    press(&mut app, Some(1));
    assert_eq!(app.cur.selection.time, Some(track1_only()), "1 回目: トラック行 + 閉じた lane、外接 4..20");

    press(&mut app, Some(1));
    assert_eq!(app.cur.selection.time, Some(everything()), "2 回目: 全トラック + 全 lane + master lane、外接 0..28");

    press(&mut app, Some(1));
    assert_eq!(app.cur.selection.time, Some(everything()), "3 回目: 冪等");
}

#[test]
fn changing_the_target_track_restarts_from_tier_one() {
    let mut app = build_app();
    setup(&mut app);
    press(&mut app, Some(1));
    press(&mut app, Some(2));
    assert_eq!(
        app.cur.selection.time,
        Some(TimeSelection::new(0.0, 8.0, vec![LaneRef::Track(2)]).unwrap()),
        "別トラックで押し直すと、そのトラックの 1 段目"
    );
}

#[test]
fn empty_track_or_no_target_goes_straight_to_everything() {
    let mut app = build_app();
    setup(&mut app);
    press(&mut app, Some(3));
    assert_eq!(app.cur.selection.time, Some(everything()), "クリップの無いトラックは 1 段目を飛ばす");

    app.handle_event(AppEvent::ClearSelection);
    press(&mut app, None);
    assert_eq!(app.cur.selection.time, Some(everything()), "対象トラック無し = 直接 全トラック");
}

#[test]
fn master_row_tier_one_is_its_song_lanes() {
    let mut app = build_app();
    setup(&mut app);
    press(&mut app, Some(MASTER_TRACK_ID));
    assert_eq!(
        app.cur.selection.time,
        Some(TimeSelection::new(24.0, 28.0, vec![lane(MASTER_TRACK_ID, 1)]).unwrap()),
        "master は song lane 行だけで 1 段目"
    );
}
