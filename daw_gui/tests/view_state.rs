//! ズーム / スクロール位置のプロジェクト保存 — AppData 側の回帰テスト。
//!
//! 検証する挙動:
//! - **per-clip 記憶**: ピアノロールの zoom はクリップごとに独立して覚えられ、別クリップを
//!   開いても混ざらず、再選択で前回値が復元される (Ableton Live / Bitwig 流)。
//! - **snapshot ⇄ restore**: `snapshot_view_state` で取った表示状態が `restore_view_state` で
//!   完全に復元される (= save/load 経路の AppData 側往復)。
//! - **restore(None)**: 旧ファイルは per-clip map をクリアしつつ globals は触らない。

use std::sync::Arc;

use common::model::PianoRollViewState;
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, ClipKey};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

const CLIP_A: ClipKey = ClipKey { track_id: 1, clip_id: 1 };
const CLIP_B: ClipKey = ClipKey { track_id: 2, clip_id: 1 };

fn build_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
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
        None, // app_dirs None = 永続化なし
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, plugin_rx)
}

/// 2 トラック × 各 1 クリップの app。 `Track` は private field を持ち外部クレートから
/// struct literal を作れないので、 既定 app の 1 本目を clone して 2 本目を用意し、
/// clip は公開 API (`CreateClip` event = `next_clip_id` 採番) で各トラックに 1 つ作る。
fn build_two_clip_app() -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (mut app, rx) = build_app();
    app.edit_song(|song| song.tracks.truncate(1));
    app.edit_song(|song| song.tracks[0].id = 1);
    app.edit_song(|song| song.tracks[0].clips.clear());
    let mut t2 = app.song_doc.song().tracks[0].clone();
    t2.id = 2;
    app.edit_song(|song| song.tracks.push(t2));
    app.handle_event(AppEvent::CreateClip { track: 0, start_beat: 0.0 });
    app.handle_event(AppEvent::CreateClip { track: 1, start_beat: 0.0 });
    (app, rx)
}

/// ピアノロールの zoom はクリップごとに独立して記憶され、別クリップへ漏れず、
/// 再選択で前回値が復元される。
#[test]
fn piano_roll_zoom_is_remembered_per_clip() {
    let (mut app, _rx) = build_two_clip_app();

    // クリップ A を選択し、横ズームを既定 (64) から 120 に変更。
    app.handle_event(AppEvent::SelectClip { target: CLIP_A, additive: false });
    app.handle_event(AppEvent::SetPianoRollZoomX(120.0));
    assert_eq!(app.pianoroll_zoom_x(), 120.0, "A の zoom が 120 になる");

    // クリップ B を開くと A の zoom は引き継がず、B 自身の既定 (64) になる。
    app.handle_event(AppEvent::SelectClip { target: CLIP_B, additive: false });
    assert_eq!(
        app.pianoroll_zoom_x(),
        PianoRollViewState::default().zoom_x,
        "B は A の zoom を引き継がず既定 64"
    );

    // クリップ A を再選択すると 120 が復元される (= 再 fit で飛ばない)。
    app.handle_event(AppEvent::SelectClip { target: CLIP_A, additive: false });
    assert_eq!(app.pianoroll_zoom_x(), 120.0, "A 再選択で zoom 120 が復元される");
}

/// 表示状態 (globals + per-clip view) が snapshot → restore で完全往復する。
#[test]
fn view_state_snapshot_restore_roundtrips() {
    let (mut app, _rx) = build_two_clip_app();
    let key_a = app.live_clip_key(CLIP_A).expect("clip A は解決できる");

    // 代表的な表示状態を仕込む。
    app.ui_prefs.arrange_zoom_x = 50.0;
    app.ui_prefs.arrange_scroll_beat = 7.0;
    app.ui_prefs.bottom_panel = 1;
    app.ui_prefs.master_row_automation_expanded = true;
    app.ui_prefs.expanded_automation_tracks.insert(2);
    let pv = PianoRollViewState { zoom_x: 111.0, zoom_y: 20.0, top_pitch: 70, scroll_beat: 2.0 };
    app.ui_prefs.piano_roll_views.insert(key_a, pv);
    // 開いていたクリップ (= 開き直しで復元されるべき選択)。
    app.selection.selected_clip = Some(key_a);
    app.selection.selected_clips = vec![key_a];
    // ループ (ON/OFF + 範囲) も表示状態と同じ扱いで往復する。
    app.handle_event(AppEvent::SetLoopRange { start: 3.0, end: 11.0 });
    app.handle_event(AppEvent::ToggleLoop);

    let snap = app.snapshot_view_state();

    // すべて別の値へ壊してから restore。
    app.ui_prefs.arrange_zoom_x = 999.0;
    app.ui_prefs.arrange_scroll_beat = 0.0;
    app.ui_prefs.bottom_panel = 0;
    app.ui_prefs.master_row_automation_expanded = false;
    app.ui_prefs.expanded_automation_tracks.clear();
    app.ui_prefs.piano_roll_views.clear();
    app.selection.selected_clip = None;
    app.selection.selected_clips.clear();
    app.handle_event(AppEvent::SetLoopRange { start: 0.0, end: 0.0 });
    app.handle_event(AppEvent::ToggleLoop);

    let loop_region = snap.loop_region;
    app.restore_view_state(Some(snap), loop_region);

    assert_eq!(app.ui_prefs.arrange_zoom_x, 50.0);
    assert_eq!(app.ui_prefs.arrange_scroll_beat, 7.0);
    assert_eq!(app.ui_prefs.bottom_panel, 1);
    assert!(app.ui_prefs.master_row_automation_expanded);
    assert!(app.ui_prefs.expanded_automation_tracks.contains(&2));
    assert_eq!(
        app.ui_prefs.piano_roll_views.get(&key_a).copied(),
        Some(pv),
        "per-clip piano roll view が復元される"
    );
    assert_eq!(
        app.selection.selected_clip,
        Some(key_a),
        "開いていたクリップ選択が復元される (= 開き直しでピアノロールが空にならない)"
    );
    assert_eq!(app.selection.selected_clips, vec![key_a]);
    assert_eq!(
        app.transport.loop_region,
        common::model::LoopRegion { enabled: true, start_beat: 3.0, end_beat: 11.0 },
        "ループ (ON/OFF + 範囲) も往復する"
    );
}

/// r.md #65: プラグインエディタ窓のジオメトリはプロジェクト単位で往復し、
/// **既に存在しない device の entry は保存時に捨てる** (orphan を溜めない)。
/// 開き直しで「窓が前回の場所とサイズで出る」ための経路そのもの。
#[test]
fn plugin_editor_geometry_roundtrips_and_drops_orphans() {
    use common::model::{EditorWindowGeometry, PluginInstance};
    use common::plugin_format::PluginFormat;

    let (mut app, _rx) = build_two_clip_app();
    // track 0 に device を 1 つ載せて id を確定させる (id 採番は edit_song 経由)。
    app.edit_song(|song| {
        let mut dev = PluginInstance::new("test.plugin".into(), PluginFormat::Clap);
        dev.id = 42;
        song.tracks[0].devices.push(dev);
    });

    let live = EditorWindowGeometry { x: 300, y: 180, width: 880, height: 162 };
    let orphan = EditorWindowGeometry { x: 0, y: 0, width: 640, height: 480 };
    app.ui_prefs.plugin_editor_windows.insert(42, live);
    // 削除済み device の残骸 (song に居ない id)。
    app.ui_prefs.plugin_editor_windows.insert(9999, orphan);

    let snap = app.snapshot_view_state();
    assert_eq!(
        snap.plugin_editor_windows,
        vec![(42, live)],
        "現存 device の分だけが保存され、削除済み device の残骸は捨てられる"
    );

    // 別プロジェクトを開いた想定で壊してから復元。
    app.ui_prefs.plugin_editor_windows.clear();
    app.ui_prefs.plugin_editor_windows.insert(7, orphan);
    let loop_region = snap.loop_region;
    app.restore_view_state(Some(snap), loop_region);

    assert_eq!(
        app.ui_prefs.plugin_editor_windows.get(&42).copied(),
        Some(live),
        "窓の位置とサイズが復元される"
    );
    assert!(
        !app.ui_prefs.plugin_editor_windows.contains_key(&7),
        "前プロジェクトの窓位置は漏れない"
    );
}

/// `restore_view_state(None)` (= 旧ファイル) は per-clip map をクリアしつつ
/// globals は触らない (= 従来の fit-to-content / 既定値挙動にフォールバック)。
#[test]
fn restore_none_clears_per_clip_but_keeps_globals() {
    let (mut app, _rx) = build_two_clip_app();
    let key_a = app.live_clip_key(CLIP_A).expect("clip A は解決できる");
    app.ui_prefs.arrange_zoom_x = 42.0;
    app.ui_prefs.piano_roll_views.insert(key_a, PianoRollViewState::default());

    app.restore_view_state(None, common::model::LoopRegion::default());

    assert!(
        app.ui_prefs.piano_roll_views.is_empty(),
        "旧ファイルでも前プロジェクトの per-clip view は漏らさずクリア"
    );
    assert_eq!(app.ui_prefs.arrange_zoom_x, 42.0, "globals は現状維持 (従来挙動)");
}

/// アレンジと下部パネルの境界比率がプロジェクトに保存され、開き直しても戻らない。
///
/// 以前は比率が `split_view` widget の一時状態にしか無く、アプリを起動し直すと
/// **必ず既定位置へ戻っていた**。行高やズームを保存していても縦に見える範囲が
/// 毎回変わるので、「保存したのに一部しか映らない」という形で出る。
#[test]
fn arrangement_split_ratio_survives_save_and_reopen() {
    let (mut app, _rx) = build_app();
    app.ui_prefs.arrangement_split_ratio = 0.88;

    let snap = app.snapshot_view_state();
    assert!((snap.arrangement_split_ratio - 0.88).abs() < 1e-6, "保存に載る");

    // 別セッションで開き直した状況 (widget state も ui_prefs も初期値)。
    let (mut fresh, _rx2) = build_app();
    assert_eq!(fresh.ui_prefs.arrangement_split_ratio, 0.0, "既定は未設定");
    fresh.restore_view_state(Some(snap), common::model::LoopRegion::default());
    assert!(
        (fresh.ui_prefs.arrangement_split_ratio - 0.88).abs() < 1e-6,
        "開き直しても境界が既定へ戻らない"
    );
}

/// 旧ファイル (比率を持たない) は `0.0` = 未設定として読める。view 側が
/// 既定比率へ倒すので、ここで `0.05` へ clamp してアレンジを潰さない。
#[test]
fn legacy_file_leaves_split_ratio_unset() {
    let (mut app, _rx) = build_app();
    let v = common::model::ViewState { arrangement_split_ratio: 0.0, ..Default::default() };
    app.restore_view_state(Some(v), common::model::LoopRegion::default());
    assert_eq!(app.ui_prefs.arrangement_split_ratio, 0.0, "未設定のまま (既定は view が決める)");
}
