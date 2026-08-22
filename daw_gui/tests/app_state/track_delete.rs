// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #43: トラックを選択して Delete で削除できること、 および
//! **クリップを Delete した直後の 2 回目の Delete でトラックが消えないこと**。
//!
//! Delete の対象面は `AppData::edit_surface` (ポインタ面 → last-wins →
//! 非空優先順) が単独で決め、 `AppData::delete_current_surface` がそれを dispatch
//! する。 view (`view/root.rs`) は Delete shortcut を後者へ流すだけなので、
//! この 2 つを headless で押さえれば実機の Delete 挙動が固定できる。
//!
//! 非自明なのは「`selected_track_ids` の非空 = トラックを消したい意図」 では
//! ない点 — クリップ選択は `select_track` で暗黙にトラックを選び、 トラック削除
//! 後は「選択ゼロを避ける」 ために最後の生存トラックが自動選択される。 だから
//! トラック面は **明示的に選んだ (= last-wins タグが立った) ときだけ** Delete の
//! 対象になる。 この不変条件をテストで固定する。

use common::model::{Clip, ClipContent, MidiContent, Section};

use common::protocol::PluginEvent;

use daw_gui::app::{AppData, AppEvent, ClipRef, EditSurface};
use daw_gui::widgets::select_modifier::SelectModifier;

use super::support::{build_app, load_instrument, select_track_single};

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
///
/// **click の前にタグを別面 (クリップ) へ倒してから見る**。 `AddInstrumentTrack` は
/// 追加したトラックを選択して Tracks タグを立てるので、 何もせず assert すると
/// click 前から成立していて何も検証しない (= `apply_select_tracks` からタグ設定を
/// 丸ごと消しても緑のまま通る) 恒真テストになる。
#[test]
fn header_click_makes_tracks_the_delete_surface() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 3);
    let clip_id = add_clip(&mut app, ids[0]);

    // 前提を崩す: クリップを選んでタグを Clips に倒す。
    app.handle_event(AppEvent::SelectClip {
        target: clip_ref(&app, ids[0], clip_id),
        additive: false,
    });
    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Clips),
        "前提: click 前の対象面はクリップ"
    );

    select_track_single(&mut app, 1);

    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Tracks),
        "ヘッダ click 直後はトラック面が Delete の対象"
    );
    assert_eq!(
        app.selection.selected_track_ids,
        vec![ids[1]],
        "click したトラックだけが選択される"
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
    assert!(
        !app.selection.selected_track_ids.is_empty(),
        "クリップを消してもトラック選択は残る (= 危険な前提が生きている)"
    );

    // 2 回目の Delete: クリップ選択は空、 トラック選択だけが残る。 対象面は
    // **None** (クリップ面が空になっただけで別の面へ飛ばない) でなければならない。
    assert_eq!(
        app.edit_surface(false),
        None,
        "クリップを消した直後の Delete は対象なし (トラック面へ飛ばない)"
    );

    // 実際に 2 回目の Delete を撃つ (production の Delete shortcut と同じ関数)。
    app.delete_current_surface(false);

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

/// 削除後の自動再選択は **削除位置に繰り上がった隣接トラック** (Ableton / REAPER 流)。
///
/// ここを `tracks.last()` (曲の最下段) にすると、 タグが Tracks のまま残るので
/// **次の Delete が画面外の最下段トラックを消す**。 `len()` だけを見ると隣接実装でも
/// 末尾実装でも通ってしまうので、 **どの id が残ったか** まで固定する。
#[test]
fn delete_reselects_the_adjacent_track_not_the_last_one() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 4);

    select_track_single(&mut app, 1);
    app.delete_current_surface(false);

    assert_eq!(visible_ids(&app), vec![ids[0], ids[2], ids[3]]);
    assert_eq!(
        app.selection.selected_track_ids,
        vec![ids[2]],
        "削除位置に繰り上がった隣接トラックが選ばれる (末尾 ids[3] ではない)"
    );
}

/// 末尾トラックを消したときだけは「1 つ上」 へ倒れる。
#[test]
fn deleting_the_last_track_reselects_the_one_above() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 3);

    select_track_single(&mut app, 2);
    app.delete_current_surface(false);

    assert_eq!(
        app.selection.selected_track_ids,
        vec![ids[1]],
        "末尾を消したら直前のトラック"
    );
}

/// トラック削除後も対象面はトラックのままなので Delete 連打で消し続けられるが、
/// **消えるのは常に手元 (削除位置) の行**。 タグを降ろす操作 (クリップ選択) を
/// 挟めば止まる — 上の退行ガード参照。
#[test]
fn repeated_delete_keeps_removing_from_the_deleted_position() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 4);

    select_track_single(&mut app, 1);
    for _ in 0..2 {
        assert_eq!(app.edit_surface(false), Some(EditSurface::Tracks));
        app.delete_current_surface(false);
    }

    assert_eq!(
        visible_ids(&app),
        vec![ids[0], ids[3]],
        "index 1 の位置から 2 本連続で消える (末尾へ飛ばない)"
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

// ---------------------------------------------------------------------------
// plugin 有り (deferred) 経路 — 実プロジェクトの本番経路
// ---------------------------------------------------------------------------

/// plugin が居る song では削除が `RequestAllStates` の round-trip 越しになる。
/// **複数トラックでも enqueue は 1 件**で、 応答後にまとめて消え undo も 1 ステップ。
/// id ごとに enqueue する実装に退行すると round-trip が別 event に分かれ、
/// undo が N ステップに割れる (Ctrl+Z 1 回で 1 本しか戻らない)。
#[test]
fn deferred_delete_removes_all_selected_in_one_roundtrip_and_one_undo_step() {
    let (mut app, _a, _p, _d) = build_app();
    load_instrument(&mut app); // track 0 に synth → song_has_plugin() == true
    let ids = ensure_tracks(&mut app, 4);
    let visible = visible_ids(&app);

    app.apply_select_tracks(ids[1], SelectModifier::Single, &visible);
    app.apply_select_tracks(ids[2], SelectModifier::RangeFromAnchor, &visible);
    assert_eq!(app.edit_surface(false), Some(EditSurface::Tracks));

    let before = visible_ids(&app);
    let depth_before = app.song_doc.undo_depth();
    app.delete_current_surface(false);

    // まだ消えない (state round-trip 待ち)。 enqueue は 1 件だけ。
    assert_eq!(visible_ids(&app), before, "応答前は song 未変更");
    assert_eq!(
        app.ipc.pending_state_queue.len(),
        1,
        "複数トラックでも deferred は 1 件にまとまる (undo 分裂を防ぐ)"
    );

    // plugin host からの応答で実行。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates {
        entries: Vec::new(),
    }));

    assert_eq!(
        visible_ids(&app),
        vec![ids[0], ids[3]],
        "応答後に選択した 2 本がまとめて消える"
    );
    assert_eq!(
        app.song_doc.undo_depth(),
        depth_before + 1,
        "deferred でも undo は 1 ステップ"
    );
    assert!(app.song_doc.undo(), "undo できる");
    assert_eq!(visible_ids(&app), before, "Undo 1 回で 2 本とも戻る");
}

/// plugin 入りトラックを削除すると、 削除 **前** に取り込んだ plugin state が
/// undo snapshot に入る (= ノブを回した状態で消して Undo すると値が戻る)。
#[test]
fn deferred_delete_captures_plugin_state_before_removing() {
    let (mut app, _a, _p, _d) = build_app();
    load_instrument(&mut app);
    let target_id = app.song_doc.song().tracks[0].id;
    let device_id = daw_gui::app::device_id_at(app.song_doc.song(), target_id, 0)
        .expect("instrument device");
    ensure_tracks(&mut app, 2);

    let visible = visible_ids(&app);
    app.apply_select_tracks(target_id, SelectModifier::Single, &visible);
    app.delete_current_surface(false);

    // round-trip の応答に「今の state」 を載せる。 削除前に Song へ書き戻されるので
    // Undo で復元した track の device state に現れる。
    app.handle_event(AppEvent::Plugin(PluginEvent::AllPluginStates {
        entries: vec![common::protocol::SlotState {
            device_id,
            data: Some(b"knob-turned".to_vec()),
            ara_archive: None,
            error: None,
        }],
    }));
    assert!(
        !visible_ids(&app).contains(&target_id),
        "応答後に対象トラックが消える"
    );

    assert!(app.song_doc.undo(), "undo できる");
    let restored = app
        .song_doc
        .song()
        .track_by_id(target_id)
        .expect("undo でトラックが戻る");
    assert_eq!(
        restored.devices[0].state.as_deref(),
        Some(b"knob-turned".as_slice()),
        "削除直前の plugin state が undo で復元される"
    );
}

// ---------------------------------------------------------------------------
// master 行 / rename / hover — Delete が沈黙していた経路
// ---------------------------------------------------------------------------

/// master 行を click しても Tracks タグは立たない。 立てると `edit_surface` が
/// Tracks を返し、 実在 0 件で空振りしたうえ **他の面の Delete まで殺す**。
#[test]
fn master_row_selection_does_not_capture_the_delete_surface() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 2);
    let clip_id = add_clip(&mut app, ids[0]);
    app.handle_event(AppEvent::SelectClip {
        target: clip_ref(&app, ids[0], clip_id),
        additive: false,
    });

    // master 行のヘッダ click 相当 (合成 id なので song.tracks には居ない)。
    let mut visible = visible_ids(&app);
    visible.push(common::model::MASTER_TRACK_ID);
    app.apply_select_tracks(
        common::model::MASTER_TRACK_ID,
        SelectModifier::Single,
        &visible,
    );

    assert_eq!(
        app.selection.selected_track_ids,
        vec![common::model::MASTER_TRACK_ID],
        "選択表示のために集合には入る (マスターのインスペクタ対象)"
    );
    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Clips),
        "が、 対象面は奪わない (クリップ面のまま)"
    );

    app.delete_current_surface(false);
    assert!(
        app.selection.selected_clips.is_empty(),
        "Delete はクリップに効く (master 行 click で殺されない)"
    );
    assert_eq!(visible_ids(&app), ids, "トラックは消えない");
}

/// master 行しか選んでいない状態で削除を要求したら、 無言ではなく理由を出す。
#[test]
fn deleting_only_the_master_row_reports_why_nothing_happened() {
    let (mut app, _a, _p, _d) = build_app();
    ensure_tracks(&mut app, 2);
    let before = visible_ids(&app);

    app.handle_event(AppEvent::DeleteTracks(vec![common::model::MASTER_TRACK_ID]));

    assert_eq!(visible_ids(&app), before);
    assert_eq!(
        app.ui_ephemeral.status_message,
        "マスタートラックは削除できません"
    );
}

/// inline リネーム中は Delete がどの面にも効かない。
///
/// 通常は text_input の typing lock が shortcut 層で `delete` を止めるが、 lock は
/// 「前フレームに text_input が描かれたか」 由来なので、 リネーム中にその行を
/// スクロールで画面外へ送ると **lock が外れて Delete がトラック削除に化ける**。
/// view の描画状態に依存しないドメイン側のガードを固定する。
#[test]
fn inline_rename_suppresses_the_delete_surface() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 3);

    select_track_single(&mut app, 1);
    assert_eq!(app.edit_surface(false), Some(EditSurface::Tracks));

    app.handle_event(AppEvent::BeginRenameTrack(ids[1]));
    assert_eq!(
        app.edit_surface(false),
        None,
        "リネーム中は対象面なし (キーはテキストのもの)"
    );

    app.delete_current_surface(false);
    assert_eq!(
        visible_ids(&app),
        ids,
        "リネーム中の Delete でトラックは消えない"
    );

    // 取り消せば元に戻る。
    app.handle_event(AppEvent::CancelRenameTrack);
    assert_eq!(app.edit_surface(false), Some(EditSurface::Tracks));
}

/// 展開済み automation lane にポインタが乗っていても、 その面に選択が無ければ
/// トラック面の Delete が通る (位置依存の無反応を作らない)。
#[test]
fn hovering_an_empty_automation_lane_does_not_block_track_delete() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 3);

    select_track_single(&mut app, 1);
    // lane 本体に hover しているだけ (点も automation clip も未選択)。
    app.ui_ephemeral.arrange_hovered_automation_lane =
        Some(common::model::AutomationLaneKey { track: ids[1], lane: 1 });

    assert_eq!(
        app.edit_surface(false),
        Some(EditSurface::Tracks),
        "空の hover 面に吸われない"
    );
    app.delete_current_surface(false);
    assert_eq!(visible_ids(&app), vec![ids[0], ids[2]], "トラックが消える");
}

// ---------------------------------------------------------------------------
// カーソル (= 選択順の末尾) は常に「今 click したトラック」
// ---------------------------------------------------------------------------

/// 下から上へ Shift+click しても cursor は click したトラック。 `range_ordered` の
/// 表示順 slice をそのまま採ると範囲下端に固着し、 インスペクタ / デバイスチェーン /
/// プラグイン追加先が click したトラックと食い違う。
#[test]
fn shift_click_upwards_puts_the_cursor_on_the_clicked_track() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 4);
    let visible = visible_ids(&app);

    app.apply_select_tracks(ids[3], SelectModifier::Single, &visible);
    app.apply_select_tracks(ids[1], SelectModifier::RangeFromAnchor, &visible);

    assert_eq!(
        app.cursor_track_id(),
        Some(ids[1]),
        "cursor は click したトラック (範囲下端 ids[3] ではない)"
    );
    let mut selected = app.selection.selected_track_ids.clone();
    selected.sort_unstable();
    let mut expected = vec![ids[1], ids[2], ids[3]];
    expected.sort_unstable();
    assert_eq!(selected, expected, "範囲はアンカー..click の閉区間");
}

/// 集合が変わらない遷移でも選択順 (= cursor) は更新される。
#[test]
fn reselecting_the_same_set_still_moves_the_cursor() {
    let (mut app, _a, _p, _d) = build_app();
    let ids = ensure_tracks(&mut app, 3);
    let visible = visible_ids(&app);

    app.apply_select_tracks(ids[1], SelectModifier::Toggle, &visible);
    app.apply_select_tracks(ids[2], SelectModifier::Toggle, &visible);
    app.apply_select_tracks(ids[0], SelectModifier::Toggle, &visible);
    assert_eq!(app.cursor_track_id(), Some(ids[0]));

    // アンカー ids[0] から ids[2] まで Shift+click → 集合は同じだが click は ids[2]。
    app.apply_select_tracks(ids[2], SelectModifier::RangeFromAnchor, &visible);
    assert_eq!(
        app.cursor_track_id(),
        Some(ids[2]),
        "集合一致でも cursor は click したトラックへ動く"
    );
}
