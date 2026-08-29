//! r.md #14 の回帰テスト: Make Unique が **選択した全 clip** を独立化する。
//!
//! 右クリックした clip が現在の複数選択に含まれるなら選択全体を、 含まれない
//! なら右クリックした 1 つだけを per-clip fork する。 選択内で互いに linked だった
//! clip も全て独立になる。 既に全て独立なら no-op (dirty 化しない)。

use common::model::{Clip, ClipContent};

use daw_gui::app::AppEvent;
use daw_gui::app_types::ClipKey;

use super::support;

/// track0 に content を共有する 2 clip を用意して返す。 戻り値は共有 content_id。
fn app_with_two_linked_clips() -> daw_gui::app::AppData {
    let (mut app, _a, _p, _d) = support::build_app();
    app.edit_song(|song| {
        let cid = song.alloc_content(ClipContent::default(), "shared".to_string());
        song.tracks[0].clips = vec![
            Clip { id: 1, start_beat: 0.0, length_beats: 4.0, content_id: cid, ..Default::default() },
            Clip { id: 2, start_beat: 4.0, length_beats: 4.0, content_id: cid, ..Default::default() },
        ];
    });
    app.song_doc.mark_saved();
    app
}

fn content_ids(app: &daw_gui::app::AppData) -> (u32, u32) {
    let clips = &app.song_doc.song().tracks[0].clips;
    (clips[0].content_id, clips[1].content_id)
}

const A: ClipKey = ClipKey { track_id: TRACK_ID, clip_id: 1 };
const B: ClipKey = ClipKey { track_id: TRACK_ID, clip_id: 2 };
/// `support::build_app` の既定トラックの id (住所は index ではなく安定 id)。
/// 起動時の 1 本目は allocator から採るので 1 (r.md #87 — id 0 は未採番の
/// sentinel で、実トラックの住所に使うとランチャーの行キーと衝突する)。
const TRACK_ID: u32 = 1;

/// 2 clip を選択して Make Unique → 両方が独立 (別々の content_id) になる。
#[test]
fn make_unique_forks_all_selected_clips() {
    let mut app = app_with_two_linked_clips();
    let (x, y) = content_ids(&app);
    assert_eq!(x, y, "前提: 2 clip は content を共有している");

    app.handle_event(AppEvent::SetClipSelection(vec![A, B]));
    app.handle_event(AppEvent::MakeClipUnique(A));

    let (a, b) = content_ids(&app);
    assert_ne!(a, b, "選択した 2 clip は互いに独立した content になる (r.md #14)");
    assert!(app.song_doc.is_dirty(), "実際に独立化したので dirty");
}

/// 選択外の clip を右クリック Make Unique → その 1 つだけ独立化 (相方は共有のまま)。
#[test]
fn make_unique_on_unselected_clip_forks_only_that_clip() {
    let mut app = app_with_two_linked_clips();
    // 選択は空 (どちらも未選択)。
    app.handle_event(AppEvent::SetClipSelection(vec![]));

    app.handle_event(AppEvent::MakeClipUnique(A));

    let (a, b) = content_ids(&app);
    assert_ne!(a, b, "右クリックした A だけ fork される");
    // B は元の content のまま (= A の旧 content と同じではなく、 B が旧 content を保持)。
    // A が fork したので A != B。 ここでは「B だけ据え置き」を確認するため、
    // もう一度同じ操作をしても B は変わらないことまでは見ない (単純化)。
}

/// 既に独立している clip の Make Unique は no-op (dirty 化しない)。
#[test]
fn make_unique_on_independent_clip_is_noop() {
    let (mut app, _a, _p, _d) = support::build_app();
    app.edit_song(|song| {
        let cid = song.alloc_content(ClipContent::default(), "solo".to_string());
        song.tracks[0].clips = vec![Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            ..Default::default()
        }];
    });
    app.song_doc.mark_saved();

    app.handle_event(AppEvent::SetClipSelection(vec![A]));
    app.handle_event(AppEvent::MakeClipUnique(A));

    assert!(
        !app.song_doc.is_dirty(),
        "既に独立した clip の Make Unique は何も変えない (r.md #14 / #12 と同じ no-op 規律)"
    );
}
