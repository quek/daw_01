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
use common::protocol::MainToChild;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent, ClipRef};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

const CLIP_A: ClipRef = ClipRef { track: 0, clip: 0 };
const CLIP_B: ClipRef = ClipRef { track: 1, clip: 0 };

fn build_app() -> (AppData, UnboundedReceiver<MainToChild>) {
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
    );
    (app, plugin_rx)
}

/// 2 トラック × 各 1 クリップの app。 `Track` は private field を持ち外部クレートから
/// struct literal を作れないので、 既定 app の 1 本目を clone して 2 本目を用意し、
/// clip は公開 API (`CreateClip` event = `next_clip_id` 採番) で各トラックに 1 つ作る。
fn build_two_clip_app() -> (AppData, UnboundedReceiver<MainToChild>) {
    let (mut app, rx) = build_app();
    app.song.tracks.truncate(1);
    app.song.tracks[0].id = 1;
    app.song.tracks[0].clips.clear();
    let mut t2 = app.song.tracks[0].clone();
    t2.id = 2;
    app.song.tracks.push(t2);
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
    let key_a = app.clip_key_of(CLIP_A).expect("clip A は解決できる");

    // 代表的な表示状態を仕込む。
    app.arrange_zoom_x = 50.0;
    app.arrange_scroll_beat = 7.0;
    app.bottom_panel = 1;
    app.master_row_automation_expanded = true;
    app.expanded_automation_tracks.insert(2);
    let pv = PianoRollViewState { zoom_x: 111.0, zoom_y: 20.0, top_pitch: 70, scroll_beat: 2.0 };
    app.piano_roll_views.insert(key_a, pv);
    // 開いていたクリップ (= 開き直しで復元されるべき選択)。
    app.selected_clip = Some(key_a);
    app.selected_clips = vec![key_a];

    let snap = app.snapshot_view_state();

    // すべて別の値へ壊してから restore。
    app.arrange_zoom_x = 999.0;
    app.arrange_scroll_beat = 0.0;
    app.bottom_panel = 0;
    app.master_row_automation_expanded = false;
    app.expanded_automation_tracks.clear();
    app.piano_roll_views.clear();
    app.selected_clip = None;
    app.selected_clips.clear();

    app.restore_view_state(Some(snap));

    assert_eq!(app.arrange_zoom_x, 50.0);
    assert_eq!(app.arrange_scroll_beat, 7.0);
    assert_eq!(app.bottom_panel, 1);
    assert!(app.master_row_automation_expanded);
    assert!(app.expanded_automation_tracks.contains(&2));
    assert_eq!(
        app.piano_roll_views.get(&key_a).copied(),
        Some(pv),
        "per-clip piano roll view が復元される"
    );
    assert_eq!(
        app.selected_clip,
        Some(key_a),
        "開いていたクリップ選択が復元される (= 開き直しでピアノロールが空にならない)"
    );
    assert_eq!(app.selected_clips, vec![key_a]);
}

/// `restore_view_state(None)` (= 旧ファイル) は per-clip map をクリアしつつ
/// globals は触らない (= 従来の fit-to-content / 既定値挙動にフォールバック)。
#[test]
fn restore_none_clears_per_clip_but_keeps_globals() {
    let (mut app, _rx) = build_two_clip_app();
    let key_a = app.clip_key_of(CLIP_A).expect("clip A は解決できる");
    app.arrange_zoom_x = 42.0;
    app.piano_roll_views.insert(key_a, PianoRollViewState::default());

    app.restore_view_state(None);

    assert!(
        app.piano_roll_views.is_empty(),
        "旧ファイルでも前プロジェクトの per-clip view は漏らさずクリア"
    );
    assert_eq!(app.arrange_zoom_x, 42.0, "globals は現状維持 (従来挙動)");
}
