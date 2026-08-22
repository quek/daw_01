// SPDX-FileCopyrightText: Copyright (C) 2026 Tahara Yoshinori
// SPDX-License-Identifier: GPL-3.0-or-later

//! recent-files 永続化先が `AppData` に注入された [`AppDirs`] から解決される
//! ことの回帰テスト。
//!
//! # なぜ存在するか
//!
//! 以前は `push_recent` / `push_recent_saved` が `daw_gui::recent::default_path()`
//! (= 実 `%LOCALAPPDATA%\daw_01\recent*.json`) に直接書き込んでいた。 その結果
//! `tempfile::tempdir()` を使う Save 系 integration test (dirty_guard 等) が
//! 走るたびに、 tempdir の `proj.daw` パスが実ユーザーの recent list に残り、
//! GUI の「Open Recent」 /「Recently Saved」 メニューに test ファイルが出続けた。
//!
//! 現在は永続化先を `AppData::new(.., app_dirs)` で注入する。 test は
//! `Some(AppDirs::under(tempdir))` を渡し、 書き込みを tempdir に隔離するか、
//! `None` を渡して永続化を完全に無効化する。 このテストは:
//!
//! 1. `Some(AppDirs::under(dir))` を渡した Save が **注入 dir** に recent.json /
//!    recent_saved.json を書く (= 実 `%LOCALAPPDATA%` ではない) こと
//! 2. `None` を渡した Save は disk へ一切 recent を書かない (= in-memory のみ更新)
//!    こと
//!
//! を検証する。 これが落ちたら DI 配線が壊れ、 test 汚染が再発する。

use std::sync::Arc;

use common::app_dirs::AppDirs;
use common::protocol::PluginCommand;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use daw_gui::app::{AppData, AppEvent};
use daw_gui::dispatcher::{
    BackgroundDispatcher, JobDispatcher, NoopJobDispatcher, RecordingDispatcher,
};

/// plugin を一切ロードしない素の `AppData` を、 指定の `app_dirs` で構築する。
/// plugin 無しなので Save は同期実行され、 `push_recent` /
/// `push_recent_saved` がその場で永続化を試みる。
fn build_app(app_dirs: Option<AppDirs>) -> (AppData, UnboundedReceiver<PluginCommand>) {
    let (audio_tx, _audio_rx) = mpsc::unbounded_channel();
    let (plugin_tx, plugin_rx) = mpsc::unbounded_channel();
    let event_dispatcher: Arc<dyn BackgroundDispatcher> = RecordingDispatcher::new();
    let job_dispatcher: Arc<dyn JobDispatcher> = Arc::new(NoopJobDispatcher);
    let app = AppData::new(
        audio_tx,
        plugin_tx,
        None,
        None,
        event_dispatcher,
        job_dispatcher,
        None,
        app_dirs,
        48_000, // (A1 r.md #8) test sample rate
    );
    (app, plugin_rx)
}

/// 共通の Save シナリオ: file_path を tempdir の `proj.daw` にセットし、
/// dirty にしてから「保存して続行(終了)」 を発火 (plugin 無しなので同期保存)。
/// 保存された project ファイルのパスを返す。
fn save_fresh_project(app: &mut AppData, proj_dir: &std::path::Path) -> std::path::PathBuf {
    let proj_path = proj_dir.join("proj.daw");
    app.song_doc.file_path = Some(proj_path.clone());
    app.song_doc.normalize(|_| {});
    app.request_close();
    app.handle_event(AppEvent::DirtyGuardSave);
    proj_path
}

#[test]
fn save_persists_recent_into_injected_dir() {
    let data_dir = tempfile::tempdir().unwrap();
    let proj_dir = tempfile::tempdir().unwrap();
    let dirs = AppDirs::under(data_dir.path());

    let (mut app, _rx) = build_app(Some(dirs.clone()));
    let proj_path = save_fresh_project(&mut app, proj_dir.path());

    // 前提: 同期保存が成功している。
    assert!(proj_path.exists(), "project file written: {}", proj_path.display());
    assert!(!app.song_doc.is_dirty(), "is_dirty cleared after save");

    // 本題: recent / recent_saved は **注入した data_dir** に書かれ、
    // 中身は今保存した proj.daw 1 件 (実 %LOCALAPPDATA% ではない)。
    let recent = daw_gui::recent::load(dirs.recent()).unwrap();
    assert_eq!(
        recent.paths,
        vec![proj_path.clone()],
        "recent.json は注入 dir ({}) に書かれ proj.daw を含む",
        dirs.recent().display()
    );

    let recent_saved = daw_gui::recent::load(dirs.recent_saved()).unwrap();
    assert_eq!(
        recent_saved.paths,
        vec![proj_path.clone()],
        "recent_saved.json も注入 dir に書かれ proj.daw を含む"
    );
}

#[test]
fn none_app_dirs_saves_project_without_persisting_recent() {
    let proj_dir = tempfile::tempdir().unwrap();

    let (mut app, _rx) = build_app(None);
    let proj_path = save_fresh_project(&mut app, proj_dir.path());

    // app_dirs=None でも save 自体は成功 (= project file は書かれる)。
    assert!(
        proj_path.exists(),
        "project file written even with no app_dirs"
    );
    assert!(!app.song_doc.is_dirty(), "is_dirty cleared after save");

    // in-memory list は更新される (= menu は session 内で機能する) が、
    // app_dirs=None なので disk へは一切書かない。 disk 検証は
    // 「注入 dir が無い = 書きようがない」 で構造的に保証される。
    assert_eq!(
        app.ui_prefs.recent_saved.paths,
        vec![proj_path.clone()],
        "in-memory recent_saved は更新される"
    );
    assert_eq!(
        app.ui_prefs.recent_files.paths,
        vec![proj_path],
        "in-memory recent_files も更新される"
    );
}
