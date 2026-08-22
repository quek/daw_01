// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! r.md #12 / #15 の回帰テスト (clip rename の dirty セマンティクス)。
//!
//! - #12: リネーム前後で名前が同じなら **dirty マークを付けない**
//!   (`commit_rename_clip` が `edit_song` を一切呼ばず早期 return する)。
//! - #15: 空文字は「名前をクリア」として通す (旧挙動は空文字を無視して
//!   元の名前に張り付いていた)。 クリア後は共有名が消え、 再度 dirty 化する。
//!
//! dirty 状態 (`SongDoc::is_dirty`) は JS script レイヤーに getter が無いので
//! ここ (handler レイヤー) で検証する。 名前自体の挙動 (clear / 再リネーム) は
//! `tests/scripts/clip_rename_smoke.js` が inspectSongJson で別途担保する。

use daw_gui::app::AppEvent;
use daw_gui::app_types::ClipRef;

use super::support;

/// track0 に content_name 付き clip を 1 つ用意し、 保存済みベースライン
/// (clean) にして返す。 clip は非 Text (clip_contents エントリ無し = content_name
/// 経路)。 戻り値は index ベースの `ClipRef` (track 0 / clip 0)。
fn app_with_named_clip(name: &str) -> (daw_gui::app::AppData, ClipRef) {
    let (mut app, _a, _p, _d) = support::build_app();
    app.edit_song(|song| {
        let cid = song.alloc_content_id();
        song.set_content_name(cid, name.to_string());
        song.tracks[0].clips = vec![common::model::Clip {
            id: 1,
            start_beat: 0.0,
            length_beats: 4.0,
            content_id: cid,
            ..Default::default()
        }];
    });
    // 保存済みベースライン = clean。 以後の rename が dirty 化するかを観測する。
    app.song_doc.mark_saved();
    assert!(!app.song_doc.is_dirty(), "fixture starts clean");
    (app, ClipRef { track: 0, clip: 0 })
}

fn rename(app: &mut daw_gui::app::AppData, target: ClipRef, new_name: &str) {
    app.handle_event(AppEvent::BeginRenameClip(target));
    app.handle_event(AppEvent::RenameClipChanged(new_name.to_string()));
    app.handle_event(AppEvent::CommitRenameClip);
}

fn clip0_name(app: &daw_gui::app::AppData) -> String {
    let cid = app.song_doc.song().tracks[0].clips[0].content_id;
    app.song_doc.song().content_name(cid).to_string()
}

/// #12: 同じ名前に付け直しても dirty にならない。
#[test]
fn rename_to_same_name_is_not_dirty() {
    let (mut app, target) = app_with_named_clip("Verse A");

    rename(&mut app, target, "Verse A");

    assert!(
        !app.song_doc.is_dirty(),
        "renaming to the identical name must not mark the project dirty (r.md #12)"
    );
    assert_eq!(clip0_name(&app), "Verse A", "name unchanged");
}

/// #12: 未編集のまま確定 (begin で seed された現在名をそのまま commit) しても
/// no-op。 production の「Rename をクリックしたが何もタイプせず確定」経路。
#[test]
fn commit_without_editing_is_not_dirty() {
    let (mut app, target) = app_with_named_clip("Chorus");

    // Changed を送らず (= edit buffer は begin の seed のまま) commit。
    app.handle_event(AppEvent::BeginRenameClip(target));
    app.handle_event(AppEvent::CommitRenameClip);

    assert!(!app.song_doc.is_dirty(), "no-op commit is not dirty (r.md #12)");
    assert_eq!(clip0_name(&app), "Chorus");
}

/// 別名に変えたときは従来どおり dirty 化する (no-op ガードの誤爆防止)。
#[test]
fn rename_to_different_name_is_dirty() {
    let (mut app, target) = app_with_named_clip("Verse A");

    rename(&mut app, target, "Bridge");

    assert!(app.song_doc.is_dirty(), "a real rename dirties");
    assert_eq!(clip0_name(&app), "Bridge");
}

/// #15: 空文字は共有名をクリアする (元の名前に張り付かない) + dirty 化する。
#[test]
fn rename_to_empty_clears_name_and_dirties() {
    let (mut app, target) = app_with_named_clip("Bridge");

    rename(&mut app, target, "");

    assert!(app.song_doc.is_dirty(), "clearing a name is a real change (r.md #15)");
    assert_eq!(clip0_name(&app), "", "empty commit clears the shared name (r.md #15)");
    // 共有名 map からキー自体が消えている (空文字 sentinel を残さない)。
    let cid = app.song_doc.song().tracks[0].clips[0].content_id;
    assert!(
        !app.song_doc.song().clip_content_names.contains_key(&cid),
        "cleared name removes the map entry entirely"
    );
}

/// #15 + #12: 既にクリア済み (名前 "") の clip を空文字で確定しても no-op。
#[test]
fn empty_rename_on_already_empty_is_not_dirty() {
    let (mut app, target) = app_with_named_clip("Bridge");
    rename(&mut app, target, ""); // クリア (dirty)
    app.song_doc.mark_saved(); // 新ベースライン = clean
    assert_eq!(clip0_name(&app), "");

    rename(&mut app, target, ""); // 既に空 → no-op

    assert!(
        !app.song_doc.is_dirty(),
        "empty rename on an already-cleared name is a no-op (r.md #12)"
    );
}
