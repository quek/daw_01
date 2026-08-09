//! r.md #43: トラックを選択して Delete で削除できること、 および
//! **クリップを Delete した直後の 2 回目の Delete でトラックが消えないこと**。
//!
//! Delete の対象面は `AppData::edit_surface` (ポインタ面 → last-wins →
//! 非空優先順) が単独で決める。 view (`view/root.rs::delete_for_surface`) は
//! その結果を `AppEvent` に流すだけなので、 arbiter とイベント側を headless で
//! 押さえれば実機の Delete 挙動が固定できる。
//!
//! 非自明なのは「`selected_track_ids` の非空 = トラックを消したい意図」 では
//! ない点 — クリップ選択は `select_track` で暗黙にトラックを選び、 トラック削除
//! 後は「選択ゼロを避ける」 ために最後の生存トラックが自動選択される。 だから
//! トラック面は **明示的に選んだ (= last-wins タグが立った) ときだけ** Delete の
//! 対象になる。 この不変条件をテストで固定する。

use common::model::{Clip, ClipContent, MidiContent, Section};

use daw_gui::app::{AppData, AppEvent, ClipRef, EditSurface};
use daw_gui::widgets::select_modifier::SelectModifier;

use super::support::{build_app, select_track_single};

/// 可視順 = 現在の `song.tracks` 順 (テスト song は group 折り畳み無し)。
fn visible_ids(app: &AppData) -> Vec<u32> {
    app.song_doc.song().tracks.iter().map(|t| t.id).collect()
}

/// `n` 本になるまでトラックを足して id 列を返す (初期 song は 1 本)。
fn ensure_tracks(app: &mut AppData, n: usize) -> Vec<u32> {
    while app.song_doc.song().tracks.len() < n {
        app.handle_event(AppEvent::AddInstrumentTrack);
    }
    visible_ids(app)
}

/// 安定 id (track_id, clip_id) → いまの index ベース `ClipRef`。
fn clip_ref(app: &AppData, track_id: u32, clip_id: u32) -> ClipRef {
    app.clip_ref_of(common::model::ClipKey { track_id, clip_id })
        .expect("clip exists")
}

/// `track_id` に空の MIDI clip を 1 つ足して `clip_id` を返す。
fn add_clip(app: &mut AppData, track_id: u32) -> u32 {
    app.edit_song(|song| {
        let cid = song.alloc_content_id();
        song.clip_contents
            .insert(cid, ClipContent::Midi(MidiContent::default()));
        let track = song
            .tracks
            .iter_mut()
            .find(|t| t.id == track_id)
            .expect("track exists");
        let clip_id = track.clips.iter().map(|c| c.id + 1).max().unwrap_or(1);
        let mut clip = Clip::default();
        clip.id = clip_id;
        clip.start_beat = 0.0;
        clip.length_beats = 4.0;
        clip.content_id = cid;
        track.clips.push(clip);
        clip_id
    })
    .expect("edit_song")
}

// ---------------------------------------------------------------------------
// 本丸: ヘッダ選択 → Delete でトラックが消える
// ---------------------------------------------------------------------------

/// トラックヘッダを click するとトラック面が Delete の対象になる。
#[test]
fn header_click_makes_tracks_the_delete_surface() {
    let (mut app, _a, _p, _d) = build_app();
    ensure_tracks(&mut app, 3);

    select_track_single(&mut app, 1);

    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Tracks),
        "ヘッダ click 直後はトラック面が Delete の対象"
    );
}

/// 複数トラックを選んで Delete すると **全部** 消え、 Undo 1 回で全部戻る
/// (Ableton 準拠 / 仕様決定 1)。
#[test]
fn delete_tracks_removes_all_selected_in_one_undo_step() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 4);
    let visible = visible_ids(&app);

    // ヘッダ click 相当: 1 本目を Single、 3 本目まで Shift で範囲選択。
    app.apply_select_tracks(ids[1], SelectModifier::Single, &visible);
    app.apply_select_tracks(ids[2], SelectModifier::RangeFromAnchor, &visible);
    assert_eq!(
        app.selection.selected_track_ids,
        vec![ids[1], ids[2]],
        "Shift+click でアンカーからの範囲が選択される"
    );

    let depth_before = app.song_doc.undo_depth();
    app.handle_event(AppEvent::DeleteTracks(
        app.selection.selected_track_ids.clone(),
    ));

    assert_eq!(
        visible_ids(&app),
        vec![ids[0], ids[3]],
        "選択した 2 本がまとめて消える"
    );
    assert_eq!(
        app.song_doc.undo_depth(),
        depth_before + 1,
        "N 本消しても undo は 1 ステップ (1 event = 1 gesture の squash)"
    );

    assert!(app.song_doc.undo(), "undo できる");
    assert_eq!(visible_ids(&app), ids, "Undo 1 回で 2 本とも戻る");
}

/// 削除対象に group とその子を同時に含めても二重削除で壊れない。 group は
/// subtree ごと消えるので、 集合内の子 id は 2 周目に no-op になる。
#[test]
fn delete_tracks_handles_group_and_its_child_selected_together() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 3);

    // ids[1] を ids[0] の子にする (= ids[0] が group として振る舞う)。
    app.handle_event(AppEvent::SetTrackParent {
        track_id: ids[1],
        parent_id: Some(ids[0]),
    });
    assert!(app.is_group_track(ids[0]), "親が group になっている");

    app.handle_event(AppEvent::DeleteTracks(vec![ids[0], ids[1]]));

    assert_eq!(
        visible_ids(&app),
        vec![ids[2]],
        "group + 子を同時指定しても subtree が 1 度だけ消える"
    );
}

/// 実在しない id (master row の仮想 id / 既に消えた id) だけを渡しても、
/// dirty 化も死んだ undo step も作らない。
#[test]
fn delete_tracks_ignores_ids_not_in_the_song() {
    let (mut app, _a, _p, _d) = build_app();
    ensure_tracks(&mut app, 2);
    app.song_doc.mark_saved();
    let depth_before = app.song_doc.undo_depth();
    let before = visible_ids(&app);

    app.handle_event(AppEvent::DeleteTracks(vec![
        common::model::MASTER_TRACK_ID,
        9_999,
    ]));

    assert_eq!(visible_ids(&app), before, "トラックは 1 本も消えない");
    assert_eq!(
        app.song_doc.undo_depth(),
        depth_before,
        "死んだ undo step を積まない"
    );
    assert!(!app.song_doc.is_dirty(), "dirty 化もしない");
}

// ---------------------------------------------------------------------------
// 退行ガード: クリップの Delete 連打がトラックを巻き込まない
// ---------------------------------------------------------------------------

/// クリップを選ぶと (暗黙に) そのトラックも選択されるが、 Delete の対象面は
/// クリップのまま。 **ここが #43 で一番壊しやすい所** — トラック面を
/// 「`selected_track_ids` が非空」 で選んでいると、 クリップを消した 2 回目の
/// Delete でトラックが飛ぶ。
#[test]
fn clip_selection_never_targets_the_track_surface() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 2);
    let clip_id = add_clip(&mut app, ids[0]);

    app.handle_event(AppEvent::SelectClip {
        target: clip_ref(&app, ids[0], clip_id),
        additive: false,
    });
    assert!(
        !app.selection.selected_track_ids.is_empty(),
        "クリップ選択は暗黙にトラックも選ぶ (前提の確認)"
    );
    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Clips),
        "それでも Delete の対象はクリップ面"
    );

    // 1 回目の Delete = クリップ削除。
    app.handle_event(AppEvent::DeleteSelectedClip);
    let after_clip_delete = visible_ids(&app);

    // 2 回目の Delete: クリップ選択は空になり、 トラック選択だけが残っている。
    // ここでトラック面が選ばれてはいけない。
    assert_ne!(
        app.edit_surface(false),
        Some(EditSurface::Tracks),
        "クリップを消した直後の Delete がトラック面を向いてはいけない"
    );
    assert_eq!(
        visible_ids(&app),
        after_clip_delete,
        "トラックは 1 本も消えていない"
    );
}

/// トラックヘッダを click した **後で** クリップを選ぶと、 last-wins で対象面が
/// クリップへ戻る (= 立ったままの Tracks タグが Delete を奪わない)。
#[test]
fn selecting_a_clip_after_a_header_click_moves_the_surface_back_to_clips() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 2);
    let clip_id = add_clip(&mut app, ids[0]);

    select_track_single(&mut app, 0);
    assert_eq!(app.edit_surface(false), Some(EditSurface::Tracks));

    app.handle_event(AppEvent::SelectClip {
        target: clip_ref(&app, ids[0], clip_id),
        additive: false,
    });

    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Clips),
        "後からクリップを選べば last-wins でクリップ面が勝つ"
    );
}

/// 逆向き: クリップを選んだ後にヘッダを click すれば、 last-wins でトラック面に
/// 移る (= #43 の「クリップを選んだ後だとトラックが消せない」 を防ぐ)。
#[test]
fn header_click_after_a_clip_selection_moves_the_surface_to_tracks() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 2);
    let clip_id = add_clip(&mut app, ids[0]);

    app.handle_event(AppEvent::SelectClip {
        target: clip_ref(&app, ids[0], clip_id),
        additive: false,
    });
    assert_eq!(app.edit_surface(false), Some(EditSurface::Clips));

    select_track_single(&mut app, 1);

    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Tracks),
        "ヘッダ click は last-wins でトラック面を取り返す"
    );
    app.handle_event(AppEvent::DeleteTracks(
        app.selection.selected_track_ids.clone(),
    ));
    assert_eq!(visible_ids(&app), vec![ids[0]], "選んだトラックが消える");
}

/// トラック削除後に「選択ゼロを避ける」 ため自動再選択された生存トラックは、
/// **タグがトラック面のままなら** 続けて Delete できる (Ableton の連打削除)。
/// ただしタグを降ろす操作 (クリップ選択) を挟めば止まる — 上の退行ガード参照。
#[test]
fn repeated_delete_keeps_removing_tracks_while_the_surface_stays_on_tracks() {
    let (mut app, _a, _p, _d) = build_app();
    ensure_tracks(&mut app, 3);

    select_track_single(&mut app, 0);
    for _ in 0..2 {
        let ids = app.selection.selected_track_ids.clone();
        assert_eq!(app.edit_surface(false), Some(EditSurface::Tracks));
        app.handle_event(AppEvent::DeleteTracks(ids));
    }

    assert_eq!(
        app.song_doc.song().tracks.len(),
        1,
        "Delete 連打で 2 本消える (自動再選択された行が次の対象)"
    );
}

// ---------------------------------------------------------------------------
// セクション面: 強制クリアを撤去してタグ方式へ揃えた分の回帰
// ---------------------------------------------------------------------------

/// セクション帯を選んでも他面の選択は消さず、 Delete はセクション面を向く
/// (旧実装は他面選択を全クリアして曖昧さを避けていた = タグ方式との二重規格)。
#[test]
fn section_selection_wins_without_clearing_other_surfaces() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 1);
    let clip_id = add_clip(&mut app, ids[0]);
    app.handle_event(AppEvent::SelectClip {
        target: clip_ref(&app, ids[0], clip_id),
        additive: false,
    });

    let section_id = app
        .edit_song(|song| {
            let id = song.alloc_section_id();
            song.sections.push(Section {
                id,
                name: "A".into(),
                color: [0.5, 0.5, 0.5],
                start_beat: 0.0,
                len_beats: 4.0,
            });
            id
        })
        .expect("edit_song");
    app.apply_select_section(section_id, SelectModifier::Single);

    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Sections),
        "セクションを選んだら Delete はセクション面"
    );
    assert!(
        !app.selection.selected_clips.is_empty(),
        "クリップ選択は破壊されない (強制クリアの撤去)"
    );
}
