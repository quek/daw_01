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
use daw_gui::widgets::arrangement::{ArrangementRow, ArrangementRowKey};

/// r.md #63: 「widget が 1 フレーム描いた結果」 を模す。 実機では `arrangement()` が
/// 分割した lanes Rect と、 縦に積んだ行 (master 行 + 可視 track 行 + 展開 lane 行) を
/// `ui_ephemeral` に書き込み、 `X` / `Z` は **それだけ** を見て収める。
/// `rows` は `(行, 高さ)` の描画順で、 `content_top` は prefix sum で組む。
fn set_arrange_layout(
    app: &mut AppData,
    lanes_w: f32,
    lanes_h: f32,
    rows: &[(ArrangementRowKey, f32)],
) {
    app.ui_ephemeral.last_arrange_lanes_size = (lanes_w, lanes_h);
    let mut y = 0.0;
    app.ui_ephemeral.last_arrange_rows = rows
        .iter()
        .map(|&(key, height)| {
            let row = ArrangementRow { key, content_top: y, height };
            y += height;
            row
        })
        .collect();
}

/// 既定レイアウト (master 行 + track 1 本、 automation lane は畳んだ状態) の 800x600。
fn set_default_layout(app: &mut AppData, track_id: u32) {
    set_arrange_layout(
        app,
        800.0,
        600.0,
        &[
            (ArrangementRowKey::Track(MASTER_TRACK_ID), 300.0),
            (ArrangementRowKey::Track(track_id), 300.0),
        ],
    );
}

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
            clips: vec![AutomationClip {
                id: clip_id,
                name: String::new(),
                start_beat: start,
                length_beats: len,
                content_id: 0,
                content_offset_beats: 0.0,
            }],
            next_clip_id: clip_id + 1,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.0,
            )
        });
    });
}

#[test]
fn z_横ズームは選択された_automation_clip_を対象にする() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    add_track_automation_clip(&mut app, 0, 1, 1, 8.0, 4.0);
    set_default_layout(&mut app, track_id);
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
    // lane は畳んだまま (= 行として積まれていない) ので、 縦ズームはレーン拡大ではなく
    // 「その automation clip が乗る track を収める」 経路に落ちる。
    set_default_layout(&mut app, track_id);
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: track_id,
        lane: 1,
        clip: 1,
    }];

    // 1 回目 = 横、 2 回目 = 縦。
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });

    // 収める行は track[0] の 1 行だけ → row_h = 600。 上に master 行 1 つ (新しい行高
    // 600) があるので scroll = 600。
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
            clips: vec![AutomationClip {
                id: 1,
                name: String::new(),
                start_beat: 8.0,
                length_beats: 4.0,
                content_id: 0,
                content_offset_beats: 0.0,
            }],
            next_clip_id: 2,
            ..AutomationLane::new(AutomationTarget::SongTempo, 120.0)
        });
    });
    let track_id = app.song_doc.song().tracks[0].id;
    // master の lane も畳んだまま (= 行として積まれていない)。
    set_default_layout(&mut app, track_id);
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: MASTER_TRACK_ID,
        lane: 1,
        clip: 1,
    }];

    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true }); // 横
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true }); // 縦

    // master = 先頭行 → 収める行は 1 つ、 row_h = 600、 track_top = 0 (master を上端に)。
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
    set_default_layout(&mut app, track_id);

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
    app.selection.selected_automation_clips = vec![AutomationClipKey {
        track: track_id,
        lane: 1,
        clip: 1,
    }];
    let lane_key = AutomationLaneKey {
        track: track_id,
        lane: 1,
    };
    // lane を展開した状態のレイアウト: master 行 100 + track 行 150 の下に lane 行
    // (= レーンの content-Y 上端は 250)。
    set_arrange_layout(
        &mut app,
        800.0,
        600.0,
        &[
            (ArrangementRowKey::Track(MASTER_TRACK_ID), 100.0),
            (ArrangementRowKey::Track(track_id), 150.0),
            (ArrangementRowKey::Lane(lane_key), 60.0),
        ],
    );

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
            clips: vec![AutomationClip { id: 1, name: String::new(), start_beat: 8.0, length_beats: 4.0, content_id: 0, content_offset_beats: 0.0 }],
            next_clip_id: 2,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.0,
            )
        });
    });
    app.edit_song(|song| song.tracks[0].clips.push(Clip { id: 1, start_beat: 0.0, length_beats: 4.0, ..Default::default() }));
    app.ui_prefs.expanded_automation_tracks.insert(track_id);
    let lane_key = AutomationLaneKey { track: track_id, lane: 1 };
    // widget が積む行: master 行 (100) + track 行 (150) + 展開 lane 行 (60) = 3 行。
    let rows = [
        (ArrangementRowKey::Track(MASTER_TRACK_ID), 100.0),
        (ArrangementRowKey::Track(track_id), 150.0),
        (ArrangementRowKey::Lane(lane_key), 60.0),
    ];
    set_arrange_layout(&mut app, 800.0, 600.0, &rows);

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
    // r.md #63: 行は widget が積んだ 3 行 (master + track + lane)、 行高に上限は無いので
    // 600/3 = 200 が均等高 (旧実装の clamp(16, 96) は「全体表示なのに下半分が空く」
    // 原因だったので撤去)。
    app.selection.selected_clips.clear();
    app.selection.selected_automation_clips = vec![AutomationClipKey { track: track_id, lane: 1, clip: 1 }];
    set_arrange_layout(&mut app, 800.0, 600.0, &rows);
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: true });
    assert_eq!(app.ui_prefs.automation_lane_row_overrides.get(&lane_key).copied(), Some(600));
    app.handle_event(AppEvent::FitArrangeToContent);
    assert_eq!(
        app.ui_prefs.automation_lane_row_overrides.get(&lane_key).copied(),
        Some(200),
        "fit should scale the lane to the fitted row height (not leave it at 600)",
    );
    assert_eq!(app.ui_prefs.arrange_track_row_h, 200.0);
}

/// r.md #63: `Z` の縦ズームは、 選択 track の **上に展開中の automation lane がある** ときも
/// スクロール位置がズレない (行 top を「一様行高 × 行番号」 で再導出せず、 widget が積んだ行を
/// 新しい行高で数え直す)。 選択 track 自身のレーンは Ardour `Editor::fit_tracks` の
/// `child_heights` と同じく viewport 高さから先に引き、 残りを track 行に配る。
#[test]
fn z_縦ズームは展開レーンを含めて行位置と行高を決める() {
    const TRACK_B: u32 = 900;
    // レイアウト: master / A / A の展開 lane (60px) / B、 lanes 高 600。
    // (選択 track, 期待 row_h, 期待 track_top)
    //   A: 自分のレーン 60 を引いた 540 が track 行高。 上は master 行 1 つ → 540。
    //   B: レーン無し → 600。 上は master(600) + A(600) + lane(60) = 1260。
    let cases = [(0usize, 540.0_f32, 540.0_f32), (1, 600.0, 1260.0)];
    for (sel, want_row_h, want_top) in cases {
        let (mut app, _rx) = build_app();
        let track_a = app.song_doc.song().tracks[0].id;
        app.edit_song(|song| {
            song.tracks[0].clips.push(Clip {
                id: 1,
                start_beat: 0.0,
                length_beats: 4.0,
                ..Default::default()
            });
            song.tracks.push(daw_gui::app::track_with(|t| {
                t.id = TRACK_B;
                t.clips.push(Clip {
                    id: 1,
                    start_beat: 0.0,
                    length_beats: 4.0,
                    ..Default::default()
                });
            }));
        });
        let sel_track = if sel == 0 { track_a } else { TRACK_B };
        app.selection.selected_clips = vec![ClipKey { track_id: sel_track, clip_id: 1 }];
        set_arrange_layout(
            &mut app,
            800.0,
            600.0,
            &[
                (ArrangementRowKey::Track(MASTER_TRACK_ID), 100.0),
                (ArrangementRowKey::Track(track_a), 150.0),
                (
                    ArrangementRowKey::Lane(AutomationLaneKey { track: track_a, lane: 1 }),
                    60.0,
                ),
                (ArrangementRowKey::Track(TRACK_B), 150.0),
            ],
        );

        app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: false }); // 横
        app.handle_event(AppEvent::ZoomArrangeToSelectedClip { automation: false }); // 縦

        assert_eq!(app.ui_prefs.arrange_track_row_h, want_row_h, "sel={sel}");
        assert_eq!(app.ui_prefs.arrange_track_top, want_top, "sel={sel}");
    }
}

/// r.md #63: 全体表示 (`X` / Fit ボタン) は **全行の高さ合計が lanes 高さに一致する**
/// (= 最下段の行の下端が画面下端にぴったり揃い、 はみ出しも余白も残らない)。
///
/// automation lane の行高は u16 (整数 px) しか持てないので、 素直に「lanes_h / 行数」 を
/// 全行に配ると丸め残差が積み上がってはみ出す。 端数は f32 の track 行高が吸収する。
#[test]
fn fit_は全行の高さ合計を_lanes_高さに一致させる() {
    // (lanes 高さ, track 行数 (master 込み), lane 行数)
    let cases = [
        (595.2_f32, 7_usize, 0_usize), // 1080p 既定に近い端数付き / lane 無し
        (595.2, 5, 2),                 // 端数付き + lane 有り (丸め残差が出るケース)
        (600.0, 1, 2),                 // master 行 1 + lane 2 本
        (600.0, 40, 0),                // 行が多すぎて 16px 下限に張り付く (溢れて可)
    ];
    for (lanes_h, track_rows, lane_rows) in cases {
        let (mut app, _rx) = build_app();
        let track_id = app.song_doc.song().tracks[0].id;
        let mut rows: Vec<(ArrangementRowKey, f32)> =
            vec![(ArrangementRowKey::Track(MASTER_TRACK_ID), 10.0)];
        for i in 1..track_rows {
            rows.push((ArrangementRowKey::Track(track_id + i as u32), 10.0));
        }
        for i in 0..lane_rows {
            rows.push((
                ArrangementRowKey::Lane(AutomationLaneKey { track: track_id, lane: i as u32 }),
                10.0,
            ));
        }
        set_arrange_layout(&mut app, 800.0, lanes_h, &rows);

        app.handle_event(AppEvent::FitArrangeToContent);

        let row_h = app.ui_prefs.arrange_track_row_h;
        let total: f32 = row_h * track_rows as f32
            + app
                .ui_prefs
                .automation_lane_row_overrides
                .values()
                .map(|px| f32::from(*px))
                .sum::<f32>();
        assert_eq!(
            app.ui_prefs.automation_lane_row_overrides.len(),
            lane_rows,
            "展開 lane 全部に fit 行高 override が張られる"
        );
        assert_eq!(app.ui_prefs.arrange_track_top, 0.0, "fit は先頭行を上端に置く");
        if row_h > 16.0 {
            assert!(
                (total - lanes_h).abs() < 1e-3,
                "Σ行高 ({total}) が lanes 高さ ({lanes_h}) に一致しない (rows={track_rows}+{lane_rows})",
            );
        } else {
            // 下限 16px に張り付くケースだけは溢れる (縦スクロールで見る)。
            assert!(total >= lanes_h, "下限張り付き時は viewport を埋めきる ({total} < {lanes_h})");
        }
    }
}

#[test]
fn z_は別_clip_を選び直すと新しい選択へ横ズームし直す() {
    let (mut app, _rx) = build_app();
    let track_id = app.song_doc.song().tracks[0].id;
    // lane 1 に 2 つの automation clip (beat 8..12 と 20..24)。
    app.edit_song(|song| {
        song.tracks[0].automation_lanes.push(AutomationLane {
            id: 1,
            clips: vec![
                AutomationClip { id: 1, name: String::new(), start_beat: 8.0, length_beats: 4.0, content_id: 0, content_offset_beats: 0.0 },
                AutomationClip { id: 2, name: String::new(), start_beat: 20.0, length_beats: 4.0, content_id: 0, content_offset_beats: 0.0 },
            ],
            next_clip_id: 3,
            ..AutomationLane::new(
                AutomationTarget::TrackBuiltin(TrackBuiltinParam::Volume),
                0.0,
            )
        });
    });
    set_default_layout(&mut app, track_id);

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
    set_default_layout(&mut app, track_id);
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
